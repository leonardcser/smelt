//! Shared append-only transcript content.
//!
//! Transcript producers transfer owned UTF-8 chunks into [`TranscriptContent`]. Clones share the
//! same entry, so render leaves and persistence work can retain a handle without copying payload
//! bytes. Appends update byte, character, line, display-cell, hash, and revision metadata in one
//! pass and never inspect an earlier chunk.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::hash::Hasher;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard};

static NEXT_CONTENT_ID: AtomicU64 = AtomicU64::new(1);
const CHECKPOINT_BYTES: usize = 4 * 1024;
const MAX_MARKDOWN_FENCE_SCAN_BYTES: usize = 4 * 1024;
const MAX_WORD_WRAP_LINE_BYTES: usize = 64 * 1024;
const TEXT_LAYOUT_CACHE_WIDTHS: usize = 2;
const TEXT_LAYOUT_TAIL_CACHE_ROWS: usize = 64;
const ARC_HEADER_BYTES: usize = 2 * std::mem::size_of::<usize>();

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ContentId(u64);

impl ContentId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug)]
pub struct SharedContentSlice {
    source: Arc<String>,
    range: Range<usize>,
}

impl SharedContentSlice {
    pub fn from_owned(source: String) -> Self {
        Self::from_shared(Arc::new(source))
    }

    pub fn from_shared(source: Arc<String>) -> Self {
        let len = source.len();
        Self {
            source,
            range: 0..len,
        }
    }

    pub fn new(source: Arc<String>, range: Range<usize>) -> Self {
        let start = smelt_buffer::text::snap(&source, range.start);
        let end = smelt_buffer::text::snap(&source, range.end.max(start));
        Self {
            source,
            range: start..end,
        }
    }

    pub fn as_str(&self) -> &str {
        smelt_buffer::text::slice(&self.source, self.range.clone())
    }

    fn prefix(mut self, len: usize) -> Self {
        let end = smelt_buffer::text::snap(
            &self.source,
            self.range.start.saturating_add(len).min(self.range.end),
        );
        self.range.end = end;
        self
    }
}

impl std::ops::Deref for SharedContentSlice {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

fn vec_bool_allocation_bytes(capacity_bits: usize) -> usize {
    capacity_bits
        .div_ceil(usize::BITS as usize)
        .saturating_mul(std::mem::size_of::<usize>())
}

fn retained_chunk_source_bytes(chunks: &[SharedContentSlice]) -> usize {
    let mut sources = HashSet::with_capacity(chunks.len());
    chunks
        .iter()
        .filter(|chunk| sources.insert(Arc::as_ptr(&chunk.source)))
        .map(|chunk| {
            ARC_HEADER_BYTES
                .saturating_add(std::mem::size_of::<String>())
                .saturating_add(chunk.source.capacity())
        })
        .sum()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentCheckpoint {
    pub byte_offset: usize,
    pub char_offset: usize,
    pub logical_line: usize,
    pub display_cells: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContentAnsiCheckpoint {
    logical_line: usize,
    state: crate::content::ansi::AnsiState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentTextRange {
    pub line: usize,
    pub row_offset: usize,
    pub row_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentTextWindow {
    pub row_count: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentTextSpan {
    pub byte_range: Range<usize>,
    pub style: crate::style::Style,
}

pub struct ContentTextRow<'a> {
    entry: &'a ContentEntry,
    row: &'a RetainedContentTextRow,
    logical_line: usize,
    row_offset: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RetainedContentTextRow {
    text: RetainedContentText,
    spans: Vec<ContentTextSpan>,
    wrapped: bool,
    continuation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RetainedContentText {
    Source(Range<usize>),
    Owned(String),
}

fn cached_text_line_retained_bytes(cached: &CachedContentTextLine) -> usize {
    cached
        .rows
        .capacity()
        .saturating_mul(std::mem::size_of::<RetainedContentTextRow>())
        .saturating_add(
            cached
                .rows
                .iter()
                .map(|row| {
                    let text = match &row.text {
                        RetainedContentText::Source(_) => 0,
                        RetainedContentText::Owned(text) => text.capacity(),
                    };
                    text.saturating_add(
                        row.spans
                            .capacity()
                            .saturating_mul(std::mem::size_of::<ContentTextSpan>()),
                    )
                })
                .sum::<usize>(),
        )
}

impl ContentTextRow<'_> {
    pub fn text(&self) -> Cow<'_, str> {
        match &self.row.text {
            RetainedContentText::Source(range) => self.entry.slice(range.clone()),
            RetainedContentText::Owned(text) => Cow::Borrowed(text),
        }
    }

    pub fn visit_text(&self, mut visit: impl FnMut(&str)) {
        match &self.row.text {
            RetainedContentText::Source(range) => self.entry.visit_slice(range.clone(), visit),
            RetainedContentText::Owned(text) => visit(text),
        }
    }

    pub fn source_range(&self) -> Option<Range<usize>> {
        match &self.row.text {
            RetainedContentText::Source(range) => Some(range.clone()),
            RetainedContentText::Owned(_) => None,
        }
    }

    pub fn spans(&self) -> &[ContentTextSpan] {
        &self.row.spans
    }

    pub fn wrapped(&self) -> bool {
        self.row.wrapped
    }

    pub fn continuation(&self) -> bool {
        self.row.continuation
    }

    pub fn logical_line(&self) -> usize {
        self.logical_line
    }

    pub fn row_offset(&self) -> usize {
        self.row_offset
    }

    fn has_visible_text(&self) -> bool {
        let mut visible = false;
        self.visit_text(|text| visible |= !text.trim().is_empty());
        visible
    }
}

#[derive(Clone)]
pub struct TranscriptContent {
    id: ContentId,
    inner: Arc<RwLock<ContentEntry>>,
}

struct ContentEntry {
    chunks: Vec<SharedContentSlice>,
    chunk_starts: Vec<usize>,
    byte_len: usize,
    char_len: usize,
    display_cells: usize,
    line_starts: Vec<usize>,
    line_ascii_words: Vec<bool>,
    line_ascii_cells: Vec<bool>,
    large_lines_have_ascii_cells: bool,
    line_plain_markdown: Vec<bool>,
    line_blank: Vec<bool>,
    ansi_checkpoints: Vec<ContentAnsiCheckpoint>,
    ansi_scanned_lines: usize,
    ansi_state: crate::content::ansi::AnsiState,
    markdown_stable_ends: Vec<usize>,
    markdown_max_completed_bytes: usize,
    markdown_scanned_lines: usize,
    markdown_fence: Option<(u8, usize)>,
    markdown_block_has_content: bool,
    markdown_pending_end: Option<usize>,
    markdown_container: MarkdownContainer,
    markdown_reference_sensitive: bool,
    markdown_tail_previous_adjacent: Option<MarkdownTailLine>,
    markdown_tail_adjacent: Option<MarkdownTailLine>,
    markdown_tail_last_nonempty: Option<MarkdownTailLine>,
    checkpoints: Vec<ContentCheckpoint>,
    next_checkpoint: usize,
    hasher: seahash::SeaHasher,
    hash: u64,
    revision: u64,
    text_layout_clock: u64,
    text_layouts: Vec<ContentTextLayout>,
    file_layout_clock: u64,
    file_layouts: Vec<ContentFileLayout>,
}

struct ContentTextLayout {
    width: u16,
    ansi: bool,
    line_rows: Vec<usize>,
    prefix_rows: Vec<usize>,
    dirty_from: usize,
    tail_lines: Vec<CachedContentTextLine>,
    last_used: u64,
}

struct CachedContentTextLine {
    line: usize,
    total_rows: usize,
    row_start: usize,
    rows: Vec<RetainedContentTextRow>,
}

struct ContentFileLayout {
    width: u16,
    wrap_ends: Vec<usize>,
    active_line: bool,
    active_row_cells: usize,
    active_row_trailing_cr: bool,
    last_used: u64,
}

pub struct ContentRead<'a> {
    entry: RwLockReadGuard<'a, ContentEntry>,
}

struct StoredContent {
    content: TranscriptContent,
    owners: usize,
}

#[derive(Default)]
pub struct ContentStore {
    entries: HashMap<ContentId, StoredContent>,
}

impl ContentStore {
    pub fn register(&mut self, content: &TranscriptContent) {
        match self.entries.entry(content.id()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                debug_assert!(Arc::ptr_eq(&entry.get().content.inner, &content.inner,));
                let owners = entry
                    .get()
                    .owners
                    .checked_add(1)
                    .expect("transcript content owner count overflow");
                entry.get_mut().owners = owners;
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(StoredContent {
                    content: content.clone(),
                    owners: 1,
                });
            }
        }
    }

    pub fn get(&self, id: ContentId) -> Option<&TranscriptContent> {
        self.entries.get(&id).map(|entry| &entry.content)
    }

    pub fn remove(&mut self, id: ContentId) {
        let Some(entry) = self.entries.get_mut(&id) else {
            return;
        };
        entry.owners = entry.owners.saturating_sub(1);
        if entry.owners == 0 {
            self.entries.remove(&id);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn retain(&mut self, mut keep: impl FnMut(ContentId) -> bool) {
        self.entries.retain(|id, _| keep(*id));
    }

    pub fn retained_bytes(&self) -> usize {
        self.entries
            .capacity()
            .saturating_mul(std::mem::size_of::<(ContentId, StoredContent)>())
            .saturating_add(
                self.entries
                    .values()
                    .map(|entry| entry.content.dynamic_retained_bytes())
                    .sum::<usize>(),
            )
    }
}

impl Default for TranscriptContent {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptContent {
    pub fn new() -> Self {
        Self::from(String::new())
    }

    pub fn id(&self) -> ContentId {
        self.id
    }

    pub fn read(&self) -> ContentRead<'_> {
        ContentRead {
            entry: self
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn revision(&self) -> u64 {
        self.read().revision()
    }

    pub fn content_hash(&self) -> u64 {
        self.read().content_hash()
    }

    pub fn push_owned(&mut self, chunk: String) -> Range<usize> {
        self.append_owned(chunk)
    }

    pub fn append_owned(&self, chunk: String) -> Range<usize> {
        self.append_shared(SharedContentSlice::from_owned(chunk))
    }

    pub fn append_shared(&self, chunk: SharedContentSlice) -> Range<usize> {
        let mut entry = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entry.append(chunk)
    }

    pub fn push_str(&mut self, text: &str) {
        self.append_owned(text.to_owned());
    }

    pub fn push(&mut self, ch: char) {
        self.append_owned(ch.to_string());
    }

    pub fn truncate(&mut self, new_len: usize) {
        let mut entry = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entry.truncate(new_len);
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    pub fn snapshot(&self) -> String {
        self.read().to_string()
    }

    pub fn contains(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let read = self.read();
        if read.entry.chunks.len() <= 1 {
            return read
                .entry
                .chunks
                .first()
                .is_some_and(|chunk| chunk.contains(needle));
        }

        let needle = needle.as_bytes();
        let mut prefixes = vec![0usize; needle.len()];
        for index in 1..needle.len() {
            let mut matched = prefixes[index - 1];
            while matched > 0 && needle[index] != needle[matched] {
                matched = prefixes[matched - 1];
            }
            if needle[index] == needle[matched] {
                matched += 1;
            }
            prefixes[index] = matched;
        }

        let mut matched = 0usize;
        for byte in read.entry.chunks.iter().flat_map(|chunk| chunk.bytes()) {
            while matched > 0 && byte != needle[matched] {
                matched = prefixes[matched - 1];
            }
            if byte == needle[matched] {
                matched += 1;
                if matched == needle.len() {
                    return true;
                }
            }
        }
        false
    }

    pub fn ends_with(&self, suffix: char) -> bool {
        self.read()
            .entry
            .chunks
            .iter()
            .rev()
            .find_map(|chunk| chunk.chars().next_back())
            == Some(suffix)
    }

    pub fn trimmed_end_len(&self) -> usize {
        let read = self.read();
        let mut trimmed = 0usize;
        for chunk in read.entry.chunks.iter().rev() {
            let chunk_trimmed = chunk.trim_end().len();
            trimmed = trimmed.saturating_add(chunk.len().saturating_sub(chunk_trimmed));
            if chunk_trimmed != 0 {
                break;
            }
        }
        read.len().saturating_sub(trimmed)
    }

    pub(crate) fn into_trimmed(self) -> Self {
        let len = self.len();
        let trimmed = self.read().trimmed_range(0..len);
        if trimmed == (0..len) {
            self
        } else {
            self.copy_ranges_joined(std::slice::from_ref(&trimmed), "")
        }
    }

    pub(crate) fn copy_ranges_joined(&self, ranges: &[Range<usize>], separator: &str) -> Self {
        let copy = Self::new();
        let read = self.read();
        for (index, range) in ranges.iter().enumerate() {
            if index != 0 && !separator.is_empty() {
                copy.append_owned(separator.to_string());
            }
            read.entry.visit_slice(range.clone(), |fragment| {
                copy.append_owned(fragment.to_string());
            });
        }
        copy
    }

    pub fn markdown_completed_ranges_after(&self, byte_offset: usize) -> Vec<Range<usize>> {
        self.read().markdown_completed_ranges_after(byte_offset)
    }

    pub fn markdown_suffix_range(&self) -> Range<usize> {
        self.read().markdown_suffix_range()
    }

    pub fn ends_with_markdown_heading(&self) -> bool {
        self.read().entry.ends_with_markdown_heading()
    }

    pub fn text_layout_rows(&self, width: u16, ansi: bool) -> usize {
        let mut entry = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = entry.ensure_text_layout(width, ansi);
        entry.text_layouts[index]
            .prefix_rows
            .last()
            .copied()
            .unwrap_or_default()
    }

    pub fn text_layout_ranges(
        &self,
        width: u16,
        ansi: bool,
        row_range: Range<usize>,
    ) -> Vec<ContentTextRange> {
        let mut entry = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = entry.ensure_text_layout(width, ansi);
        let layout = &entry.text_layouts[index];
        text_layout_ranges(&layout.prefix_rows, row_range)
    }

    pub fn file_layout_rows(&self, width: u16) -> usize {
        let mut entry = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = entry.ensure_file_layout(width);
        entry
            .logical_line_count()
            .saturating_add(entry.file_layouts[index].wrap_ends.len())
    }

    pub fn file_layout_ranges(&self, width: u16, row_range: Range<usize>) -> Vec<ContentTextRange> {
        let mut entry = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = entry.ensure_file_layout(width);
        file_layout_ranges(&entry, &entry.file_layouts[index], row_range)
    }

    pub fn visit_file_layout_line_rows(
        &self,
        width: u16,
        line: usize,
        row_range: Range<usize>,
        mut visit: impl FnMut(ContentTextRow<'_>),
    ) -> usize {
        let mut entry = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = entry.ensure_file_layout(width);
        let layout = &entry.file_layouts[index];
        let Some((source_range, wraps)) = file_layout_line(&entry, layout, line) else {
            return 0;
        };
        let row_count = wraps.len().saturating_add(1);
        let start = row_range.start.min(row_count);
        let end = row_range.end.max(start).min(row_count);
        let wrapped = !wraps.is_empty();
        for row_offset in start..end {
            let continuation = row_offset > 0;
            let range_start = if continuation {
                layout.wrap_ends[wraps.start.saturating_add(row_offset).saturating_sub(1)]
            } else {
                source_range.start
            };
            let range_end = if row_offset < wraps.len() {
                layout.wrap_ends[wraps.start.saturating_add(row_offset)]
            } else {
                source_range.end
            }
            .max(range_start);
            let row = RetainedContentTextRow {
                text: RetainedContentText::Source(range_start..range_end),
                spans: Vec::new(),
                wrapped,
                continuation,
            };
            visit(ContentTextRow {
                entry: &entry,
                row: &row,
                logical_line: line,
                row_offset,
            });
        }
        end.saturating_sub(start)
    }

    pub fn text_layout_range_has_visible_text(
        &self,
        width: u16,
        ansi: bool,
        row_range: Range<usize>,
    ) -> bool {
        let mut visible = false;
        self.visit_text_layout_rows(width, ansi, row_range, |row| {
            visible |= row.has_visible_text();
        });
        visible
    }

    pub fn visit_text_layout_rows(
        &self,
        width: u16,
        ansi: bool,
        row_range: Range<usize>,
        mut visit: impl FnMut(ContentTextRow<'_>),
    ) -> usize {
        let mut entry = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = entry.ensure_text_layout(width, ansi);
        let ranges = text_layout_ranges(&entry.text_layouts[index].prefix_rows, row_range);
        let mut visited = 0usize;
        for range in ranges {
            let end = range.row_offset.saturating_add(range.row_count);
            let rows =
                materialize_text_line_rows(&entry, range.line, width, ansi, range.row_offset..end);
            for (index, row) in rows.iter().enumerate() {
                visit(ContentTextRow {
                    entry: &entry,
                    row,
                    logical_line: range.line,
                    row_offset: range.row_offset.saturating_add(index),
                });
            }
            visited = visited.saturating_add(rows.len());
        }
        visited
    }

    pub fn visit_text_layout_head_rows(
        &self,
        width: u16,
        ansi: bool,
        max_rows: usize,
        visit: impl FnMut(ContentTextRow<'_>),
    ) -> ContentTextWindow {
        self.visit_text_layout_edge_rows(width, ansi, max_rows, false, visit)
    }

    pub fn visit_text_layout_tail_rows(
        &self,
        width: u16,
        ansi: bool,
        max_rows: usize,
        visit: impl FnMut(ContentTextRow<'_>),
    ) -> ContentTextWindow {
        self.visit_text_layout_edge_rows(width, ansi, max_rows, true, visit)
    }

    fn visit_cached_text_layout_tail_rows(
        &self,
        width: u16,
        ansi: bool,
        max_rows: usize,
        mut visit: impl FnMut(ContentTextRow<'_>),
    ) -> ContentTextWindow {
        debug_assert!(max_rows <= TEXT_LAYOUT_TAIL_CACHE_ROWS);
        let mut entry = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let line_count = entry.logical_line_count();
        if max_rows == 0 || line_count == 0 {
            return ContentTextWindow {
                row_count: 0,
                truncated: line_count != 0,
            };
        }

        let width = width.max(1);
        entry.text_layout_clock = entry
            .text_layout_clock
            .checked_add(1)
            .expect("transcript text layout clock overflow");
        let mut layout = entry
            .text_layouts
            .iter()
            .position(|layout| layout.width == width && layout.ansi == ansi)
            .map(|index| entry.text_layouts.swap_remove(index))
            .unwrap_or_else(|| ContentTextLayout {
                width,
                ansi,
                line_rows: Vec::new(),
                prefix_rows: vec![0],
                dirty_from: 0,
                tail_lines: Vec::new(),
                last_used: 0,
            });

        let mut selected = [(0usize, 0usize, 0usize); TEXT_LAYOUT_TAIL_CACHE_ROWS];
        let mut selected_count = 0usize;
        let mut selected_rows = 0usize;
        let mut scanned_lines = 0usize;
        let mut partial_line = false;
        for line in (0..line_count).rev() {
            let remaining = max_rows.saturating_sub(selected_rows);
            let reusable = layout
                .tail_lines
                .iter()
                .position(|cached| cached.line == line)
                .filter(|index| {
                    let cached = &layout.tail_lines[*index];
                    let needed = remaining.min(cached.total_rows);
                    cached.rows.len() >= needed
                        && cached.row_start <= cached.total_rows.saturating_sub(needed)
                });
            let cache_index = reusable.unwrap_or_else(|| {
                let (total_rows, row_start, rows) =
                    materialize_text_line_edge_rows(&entry, line, width, ansi, remaining, true);
                if let Some(index) = layout
                    .tail_lines
                    .iter()
                    .position(|cached| cached.line == line)
                {
                    layout.tail_lines[index] = CachedContentTextLine {
                        line,
                        total_rows,
                        row_start,
                        rows,
                    };
                    index
                } else {
                    layout.tail_lines.push(CachedContentTextLine {
                        line,
                        total_rows,
                        row_start,
                        rows,
                    });
                    layout.tail_lines.len().saturating_sub(1)
                }
            });
            let cached = &layout.tail_lines[cache_index];
            let row_count = remaining.min(cached.total_rows);
            let row_start = cached.total_rows.saturating_sub(row_count);
            selected[selected_count] = (line, row_start, row_count);
            selected_count = selected_count.saturating_add(1);
            selected_rows = selected_rows.saturating_add(row_count);
            scanned_lines = scanned_lines.saturating_add(1);
            if cached.total_rows > remaining {
                partial_line = true;
                break;
            }
            if selected_rows == max_rows {
                break;
            }
        }

        for &(line, row_start, row_count) in selected[..selected_count].iter().rev() {
            let cached = layout
                .tail_lines
                .iter()
                .find(|cached| cached.line == line)
                .expect("selected tail line remains cached");
            let local_start = row_start.saturating_sub(cached.row_start);
            for (index, row) in cached.rows[local_start..]
                .iter()
                .take(row_count)
                .enumerate()
            {
                visit(ContentTextRow {
                    entry: &entry,
                    row,
                    logical_line: line,
                    row_offset: row_start.saturating_add(index),
                });
            }
        }

        layout.tail_lines.sort_unstable_by_key(|cached| cached.line);
        while layout
            .tail_lines
            .iter()
            .map(|cached| cached.rows.len())
            .sum::<usize>()
            > TEXT_LAYOUT_TAIL_CACHE_ROWS
        {
            layout.tail_lines.remove(0);
        }
        layout.last_used = entry.text_layout_clock;
        if entry.text_layouts.len() >= TEXT_LAYOUT_CACHE_WIDTHS {
            let oldest = entry
                .text_layouts
                .iter()
                .enumerate()
                .min_by_key(|(_, candidate)| candidate.last_used)
                .map(|(index, _)| index)
                .unwrap_or_default();
            entry.text_layouts.swap_remove(oldest);
        }
        entry.text_layouts.push(layout);

        ContentTextWindow {
            row_count: selected_rows,
            truncated: partial_line || scanned_lines < line_count,
        }
    }

    fn visit_text_layout_edge_rows(
        &self,
        width: u16,
        ansi: bool,
        max_rows: usize,
        tail: bool,
        mut visit: impl FnMut(ContentTextRow<'_>),
    ) -> ContentTextWindow {
        if tail && max_rows <= TEXT_LAYOUT_TAIL_CACHE_ROWS {
            return self.visit_cached_text_layout_tail_rows(width, ansi, max_rows, visit);
        }

        let entry = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let line_count = entry.logical_line_count();
        if max_rows == 0 || line_count == 0 {
            return ContentTextWindow {
                row_count: 0,
                truncated: line_count != 0,
            };
        }

        let mut selected = Vec::with_capacity(max_rows);
        let mut scanned_lines = 0usize;
        let mut partial_line = false;
        if tail {
            for line in (0..line_count).rev() {
                let remaining = max_rows.saturating_sub(selected.len());
                let (line_rows, start, rows) =
                    materialize_text_line_edge_rows(&entry, line, width, ansi, remaining, true);
                selected.splice(
                    0..0,
                    rows.into_iter()
                        .enumerate()
                        .map(|(index, row)| (line, start.saturating_add(index), row)),
                );
                scanned_lines = scanned_lines.saturating_add(1);
                if line_rows > remaining {
                    partial_line = true;
                    break;
                }
                if selected.len() == max_rows {
                    break;
                }
            }
        } else {
            for line in 0..line_count {
                let remaining = max_rows.saturating_sub(selected.len());
                let (line_rows, _, rows) =
                    materialize_text_line_edge_rows(&entry, line, width, ansi, remaining, false);
                partial_line = line_rows > remaining;
                selected.extend(
                    rows.into_iter()
                        .enumerate()
                        .map(|(row_offset, row)| (line, row_offset, row)),
                );
                scanned_lines = scanned_lines.saturating_add(1);
                if selected.len() == max_rows {
                    break;
                }
            }
        }

        let row_count = selected.len();
        let truncated = partial_line || scanned_lines < line_count;
        for (logical_line, row_offset, row) in &selected {
            visit(ContentTextRow {
                entry: &entry,
                row,
                logical_line: *logical_line,
                row_offset: *row_offset,
            });
        }
        ContentTextWindow {
            row_count,
            truncated,
        }
    }

    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }

    pub fn dynamic_retained_bytes(&self) -> usize {
        let read = self.read();
        ARC_HEADER_BYTES
            .saturating_add(std::mem::size_of::<RwLock<ContentEntry>>())
            .saturating_add(
                read.entry
                    .chunks
                    .capacity()
                    .saturating_mul(std::mem::size_of::<SharedContentSlice>()),
            )
            .saturating_add(retained_chunk_source_bytes(&read.entry.chunks))
            .saturating_add(
                read.entry
                    .chunk_starts
                    .capacity()
                    .saturating_mul(std::mem::size_of::<usize>()),
            )
            .saturating_add(
                read.entry
                    .line_starts
                    .capacity()
                    .saturating_mul(std::mem::size_of::<usize>()),
            )
            .saturating_add(vec_bool_allocation_bytes(
                read.entry.line_ascii_words.capacity(),
            ))
            .saturating_add(vec_bool_allocation_bytes(
                read.entry.line_ascii_cells.capacity(),
            ))
            .saturating_add(vec_bool_allocation_bytes(
                read.entry.line_plain_markdown.capacity(),
            ))
            .saturating_add(vec_bool_allocation_bytes(read.entry.line_blank.capacity()))
            .saturating_add(
                read.entry
                    .ansi_checkpoints
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ContentAnsiCheckpoint>()),
            )
            .saturating_add(
                read.entry
                    .markdown_stable_ends
                    .capacity()
                    .saturating_mul(std::mem::size_of::<usize>()),
            )
            .saturating_add(
                read.entry
                    .checkpoints
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ContentCheckpoint>()),
            )
            .saturating_add(
                read.entry
                    .text_layouts
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ContentTextLayout>()),
            )
            .saturating_add(
                read.entry
                    .text_layouts
                    .iter()
                    .map(|layout| {
                        layout
                            .line_rows
                            .capacity()
                            .saturating_mul(std::mem::size_of::<usize>())
                            .saturating_add(
                                layout
                                    .prefix_rows
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<usize>()),
                            )
                            .saturating_add(
                                layout
                                    .tail_lines
                                    .capacity()
                                    .saturating_mul(std::mem::size_of::<CachedContentTextLine>()),
                            )
                            .saturating_add(
                                layout
                                    .tail_lines
                                    .iter()
                                    .map(cached_text_line_retained_bytes)
                                    .sum::<usize>(),
                            )
                    })
                    .sum::<usize>(),
            )
            .saturating_add(
                read.entry
                    .file_layouts
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ContentFileLayout>()),
            )
            .saturating_add(
                read.entry
                    .file_layouts
                    .iter()
                    .map(|layout| {
                        layout
                            .wrap_ends
                            .capacity()
                            .saturating_mul(std::mem::size_of::<usize>())
                    })
                    .sum::<usize>(),
            )
    }
}

fn file_layout_ranges(
    entry: &ContentEntry,
    layout: &ContentFileLayout,
    row_range: Range<usize>,
) -> Vec<ContentTextRange> {
    let line_count = entry.logical_line_count();
    let total_rows = line_count.saturating_add(layout.wrap_ends.len());
    let start = row_range.start.min(total_rows);
    let end = row_range.end.max(start).min(total_rows);
    if start == end {
        return Vec::new();
    }

    let mut low = 0usize;
    let mut high = line_count;
    while low < high {
        let line = low.saturating_add(high.saturating_sub(low) / 2);
        let source_start = entry.line_starts.get(line).copied().unwrap_or_default();
        let wraps_before = layout.wrap_ends.partition_point(|end| *end <= source_start);
        if line.saturating_add(wraps_before) <= start {
            low = line.saturating_add(1);
        } else {
            high = line;
        }
    }

    let first_line = low.saturating_sub(1).min(line_count.saturating_sub(1));
    let mut ranges = Vec::new();
    for line in first_line..line_count {
        let Some((_, wraps)) = file_layout_line(entry, layout, line) else {
            break;
        };
        let line_start = line.saturating_add(wraps.start);
        if line_start >= end {
            break;
        }
        let line_end = line_start.saturating_add(wraps.len()).saturating_add(1);
        let range_start = start.max(line_start);
        let range_end = end.min(line_end);
        if range_start < range_end {
            ranges.push(ContentTextRange {
                line,
                row_offset: range_start.saturating_sub(line_start),
                row_count: range_end.saturating_sub(range_start),
            });
        }
    }
    ranges
}

fn file_layout_line(
    entry: &ContentEntry,
    layout: &ContentFileLayout,
    line: usize,
) -> Option<(Range<usize>, Range<usize>)> {
    let source_range = entry.line_range(line)?;
    let wrap_start = layout
        .wrap_ends
        .partition_point(|end| *end <= source_range.start);
    let wrap_end = layout
        .wrap_ends
        .partition_point(|end| *end < source_range.end)
        .max(wrap_start);
    Some((source_range, wrap_start..wrap_end))
}

fn text_layout_ranges(prefix_rows: &[usize], row_range: Range<usize>) -> Vec<ContentTextRange> {
    let total_rows = prefix_rows.last().copied().unwrap_or_default();
    let start = row_range.start.min(total_rows);
    let end = row_range.end.max(start).min(total_rows);
    if start == end {
        return Vec::new();
    }

    let line_count = prefix_rows.len().saturating_sub(1);
    let first_line = prefix_rows
        .partition_point(|row| *row <= start)
        .saturating_sub(1)
        .min(line_count);
    let mut ranges = Vec::new();
    for line in first_line..line_count {
        let line_start = prefix_rows[line];
        if line_start >= end {
            break;
        }
        let line_end = prefix_rows[line.saturating_add(1)];
        let range_start = start.max(line_start);
        let range_end = end.min(line_end);
        if range_start < range_end {
            ranges.push(ContentTextRange {
                line,
                row_offset: range_start.saturating_sub(line_start),
                row_count: range_end.saturating_sub(range_start),
            });
        }
    }
    ranges
}

enum MarkdownLineAction {
    FenceOpen((u8, usize)),
    FenceClose(bool),
    Blank,
    Content,
}

#[derive(Clone, Copy)]
struct MarkdownTailLine {
    line: usize,
    in_code: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum MarkdownContainer {
    #[default]
    Plain,
    List,
    IndentedCode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MarkdownLineKind {
    Plain,
    ListMarker,
    Indented,
}

impl MarkdownContainer {
    fn continues_across_blank(self, next: MarkdownLineKind) -> bool {
        match self {
            Self::Plain => false,
            Self::List => matches!(
                next,
                MarkdownLineKind::ListMarker | MarkdownLineKind::Indented
            ),
            Self::IndentedCode => next == MarkdownLineKind::Indented,
        }
    }

    fn include(&mut self, line: MarkdownLineKind) {
        if *self == Self::Plain {
            *self = match line {
                MarkdownLineKind::ListMarker => Self::List,
                MarkdownLineKind::Indented => Self::IndentedCode,
                MarkdownLineKind::Plain => Self::Plain,
            };
        }
    }
}

impl ContentEntry {
    fn empty() -> Self {
        Self {
            chunks: Vec::new(),
            chunk_starts: Vec::new(),
            byte_len: 0,
            char_len: 0,
            display_cells: 0,
            line_starts: vec![0],
            line_ascii_words: vec![true],
            line_ascii_cells: vec![true],
            large_lines_have_ascii_cells: true,
            line_plain_markdown: vec![true],
            line_blank: vec![true],
            ansi_checkpoints: vec![ContentAnsiCheckpoint {
                logical_line: 0,
                state: crate::content::ansi::AnsiState::default(),
            }],
            ansi_scanned_lines: 0,
            ansi_state: crate::content::ansi::AnsiState::default(),
            markdown_stable_ends: Vec::new(),
            markdown_max_completed_bytes: 0,
            markdown_scanned_lines: 0,
            markdown_fence: None,
            markdown_block_has_content: false,
            markdown_pending_end: None,
            markdown_container: MarkdownContainer::default(),
            markdown_reference_sensitive: false,
            markdown_tail_previous_adjacent: None,
            markdown_tail_adjacent: None,
            markdown_tail_last_nonempty: None,
            checkpoints: vec![ContentCheckpoint {
                byte_offset: 0,
                char_offset: 0,
                logical_line: 0,
                display_cells: 0,
            }],
            next_checkpoint: CHECKPOINT_BYTES,
            hasher: seahash::SeaHasher::new(),
            hash: seahash::hash(&[]),
            revision: 0,
            text_layout_clock: 0,
            text_layouts: Vec::new(),
            file_layout_clock: 0,
            file_layouts: Vec::new(),
        }
    }

    fn append(&mut self, chunk: SharedContentSlice) -> Range<usize> {
        let start = self.byte_len;
        let text = chunk.as_str();
        if text.is_empty() {
            return start..start;
        }

        let dirty_line = self.line_starts.len().saturating_sub(1);
        let metadata_perf = smelt_perf::perf::begin("transcript:content:append_metadata");
        let base_chars = self.char_len;
        let base_cells = self.display_cells;
        let mut chars = 0usize;
        let mut cells = 0usize;
        let mut local_start = 0usize;
        for segment in text.split_inclusive('\n') {
            let has_newline = segment.ends_with('\n');
            let body_len = segment.len().saturating_sub(usize::from(has_newline));
            let body = smelt_buffer::text::slice(segment, 0..body_len);
            let bytes = body.as_bytes();
            if let Some(ascii_word) = self.line_ascii_words.last_mut() {
                *ascii_word &= bytes
                    .iter()
                    .all(|byte| byte.is_ascii_graphic() && *byte != b' ');
            }
            if let Some(ascii_cells) = self.line_ascii_cells.last_mut() {
                *ascii_cells &= bytes
                    .iter()
                    .all(|byte| byte.is_ascii_graphic() || *byte == b' ');
            }
            if let Some(plain_markdown) = self.line_plain_markdown.last_mut() {
                *plain_markdown &= bytes.iter().all(|byte| !is_markdown_syntax_byte(*byte));
            }
            if let Some(blank) = self.line_blank.last_mut() {
                *blank &= if body.is_ascii() {
                    bytes.iter().all(u8::is_ascii_whitespace)
                } else {
                    body.chars().all(char::is_whitespace)
                };
            }

            let body_start = start.saturating_add(local_start);
            let body_end = body_start.saturating_add(body.len());
            if body_end.saturating_sub(self.line_starts.last().copied().unwrap_or(body_end))
                > MAX_WORD_WRAP_LINE_BYTES
                && !self.line_ascii_cells.last().copied().unwrap_or(false)
            {
                self.large_lines_have_ascii_cells = false;
            }
            self.index_fragment_metrics(
                body, body_start, base_chars, base_cells, &mut chars, &mut cells,
            );

            if has_newline {
                let absolute_end = body_end.saturating_add(1);
                self.line_starts.push(absolute_end);
                self.line_ascii_words.push(true);
                self.line_ascii_cells.push(true);
                self.line_plain_markdown.push(true);
                self.line_blank.push(true);
                chars = chars.saturating_add(1);
                cells = cells.saturating_add(1);
                self.record_checkpoint(absolute_end, base_chars, base_cells, chars, cells);
            }
            local_start = local_start.saturating_add(segment.len());
        }
        drop(metadata_perf);

        let hash_perf = smelt_perf::perf::begin("transcript:content:append_hash");
        self.hasher.write(text.as_bytes());
        self.hash = self.hasher.finish();
        drop(hash_perf);
        self.byte_len = self.byte_len.saturating_add(text.len());
        self.char_len = self.char_len.saturating_add(chars);
        self.display_cells = self.display_cells.saturating_add(cells);
        let file_layout_perf = smelt_perf::perf::begin("transcript:content:append_file_layouts");
        for layout in &mut self.file_layouts {
            layout.append(start, text);
        }
        drop(file_layout_perf);
        self.chunk_starts.push(start);
        self.chunks.push(chunk);
        let ansi_perf = smelt_perf::perf::begin("transcript:content:append_ansi_index");
        self.index_completed_ansi_lines();
        drop(ansi_perf);
        let markdown_perf = smelt_perf::perf::begin("transcript:content:append_markdown_index");
        self.index_completed_markdown_blocks();
        drop(markdown_perf);
        for layout in &mut self.text_layouts {
            layout.dirty_from = layout.dirty_from.min(dirty_line);
            layout.tail_lines.retain(|cached| cached.line < dirty_line);
        }
        self.revision = self
            .revision
            .checked_add(1)
            .expect("transcript content revision overflow");
        start..self.byte_len
    }

    fn index_fragment_metrics(
        &mut self,
        fragment: &str,
        absolute_start: usize,
        base_chars: usize,
        base_cells: usize,
        chars: &mut usize,
        cells: &mut usize,
    ) {
        let fragment_end = absolute_start.saturating_add(fragment.len());
        let mut consumed = 0usize;
        while self.next_checkpoint <= fragment_end {
            let target = self.next_checkpoint.saturating_sub(absolute_start);
            let snapped = smelt_buffer::text::snap(fragment, target);
            let boundary = if snapped == target {
                target
            } else {
                smelt_buffer::text::next_char_boundary(fragment, snapped)
            };
            let measured = smelt_buffer::text::slice(fragment, consumed..boundary);
            let (new_chars, new_cells) = content_fragment_metrics(measured);
            *chars = (*chars).saturating_add(new_chars);
            *cells = (*cells).saturating_add(new_cells);
            consumed = boundary;
            self.record_checkpoint(
                absolute_start.saturating_add(consumed),
                base_chars,
                base_cells,
                *chars,
                *cells,
            );
        }

        let measured = smelt_buffer::text::slice(fragment, consumed..fragment.len());
        let (new_chars, new_cells) = content_fragment_metrics(measured);
        *chars = (*chars).saturating_add(new_chars);
        *cells = (*cells).saturating_add(new_cells);
    }

    fn record_checkpoint(
        &mut self,
        absolute_end: usize,
        base_chars: usize,
        base_cells: usize,
        chars: usize,
        cells: usize,
    ) {
        if absolute_end < self.next_checkpoint {
            return;
        }
        self.checkpoints.push(ContentCheckpoint {
            byte_offset: absolute_end,
            char_offset: base_chars.saturating_add(chars),
            logical_line: self.line_starts.len().saturating_sub(1),
            display_cells: base_cells.saturating_add(cells),
        });
        while self.next_checkpoint <= absolute_end {
            self.next_checkpoint = self.next_checkpoint.saturating_add(CHECKPOINT_BYTES);
        }
    }

    fn index_completed_ansi_lines(&mut self) {
        let completed_lines = self.line_starts.len().saturating_sub(1);
        while self.ansi_scanned_lines < completed_lines {
            let line = self.ansi_scanned_lines;
            let Some(range) = self.line_range(line) else {
                break;
            };
            let content = self.slice(range);
            let mut state = self.ansi_state;
            crate::content::ansi::advance_ansi_state(&content, &mut state);
            drop(content);
            self.ansi_state = state;
            self.ansi_scanned_lines = self.ansi_scanned_lines.saturating_add(1);
            if self
                .ansi_checkpoints
                .last()
                .is_none_or(|checkpoint| checkpoint.state != self.ansi_state)
            {
                self.ansi_checkpoints.push(ContentAnsiCheckpoint {
                    logical_line: self.ansi_scanned_lines,
                    state: self.ansi_state,
                });
            }
        }
    }

    fn ansi_state_for_line(&self, line: usize) -> crate::content::ansi::AnsiState {
        self.ansi_checkpoints
            .partition_point(|checkpoint| checkpoint.logical_line <= line)
            .checked_sub(1)
            .and_then(|index| self.ansi_checkpoints.get(index))
            .map(|checkpoint| checkpoint.state)
            .unwrap_or_default()
    }

    fn index_completed_markdown_blocks(&mut self) {
        let completed_lines = self.line_starts.len().saturating_sub(1);
        while self.markdown_scanned_lines < completed_lines {
            let line = self.markdown_scanned_lines;
            let Some(range) = self.line_range(line) else {
                break;
            };
            let scan_end = range
                .start
                .saturating_add(MAX_MARKDOWN_FENCE_SCAN_BYTES)
                .min(range.end);
            let source = self.slice(range.start..scan_end);
            let body = source.trim_end();
            let line_kind = markdown_line_kind(body);
            let action = if let Some((marker, len)) = self.markdown_fence {
                let closes = if range.len() <= MAX_MARKDOWN_FENCE_SCAN_BYTES {
                    let indent = body
                        .bytes()
                        .take_while(|byte| *byte == b' ')
                        .take(3)
                        .count();
                    let fence_body = smelt_buffer::text::slice(body, indent..body.len());
                    markdown_fence_marker(body).is_some_and(|candidate| {
                        candidate.0 == marker
                            && candidate.1 >= len
                            && fence_body[candidate.1..].trim().is_empty()
                    })
                } else {
                    false
                };
                MarkdownLineAction::FenceClose(closes)
            } else if let Some(fence) = markdown_fence_marker(body) {
                MarkdownLineAction::FenceOpen(fence)
            } else if self.line_blank.get(line).copied().unwrap_or(false) {
                MarkdownLineAction::Blank
            } else {
                MarkdownLineAction::Content
            };
            let reference_sensitive = matches!(action, MarkdownLineAction::Content)
                && markdown_line_may_use_reference(body);
            let blank = self.line_blank.get(line).copied().unwrap_or(false);
            let in_code = matches!(
                action,
                MarkdownLineAction::FenceOpen(_) | MarkdownLineAction::FenceClose(_)
            );
            drop(source);
            self.markdown_reference_sensitive |= reference_sensitive;
            if blank {
                self.markdown_tail_adjacent = None;
            } else {
                let current = MarkdownTailLine { line, in_code };
                self.markdown_tail_previous_adjacent = self.markdown_tail_adjacent;
                self.markdown_tail_adjacent = Some(current);
                self.markdown_tail_last_nonempty = Some(current);
            }

            if self.markdown_fence.is_none()
                && !self.markdown_reference_sensitive
                && !matches!(action, MarkdownLineAction::Blank)
                && self.markdown_pending_end.is_some()
            {
                let end = self.markdown_pending_end.take().unwrap();
                if !self.markdown_container.continues_across_blank(line_kind) {
                    self.commit_markdown_range(end);
                    self.markdown_block_has_content = false;
                    self.markdown_container = MarkdownContainer::default();
                }
            }

            match action {
                MarkdownLineAction::FenceClose(closes) => {
                    self.markdown_block_has_content = true;
                    if closes {
                        self.markdown_fence = None;
                    }
                }
                MarkdownLineAction::FenceOpen(fence) => {
                    self.markdown_fence = Some(fence);
                    self.markdown_block_has_content = true;
                }
                MarkdownLineAction::Blank if self.markdown_block_has_content => {
                    self.markdown_pending_end = self
                        .line_starts
                        .get(line.saturating_add(1))
                        .copied()
                        .or(Some(self.byte_len));
                }
                MarkdownLineAction::Blank => {}
                MarkdownLineAction::Content => {
                    self.markdown_block_has_content = true;
                    self.markdown_container.include(line_kind);
                }
            }
            self.markdown_scanned_lines = self.markdown_scanned_lines.saturating_add(1);
        }
        self.commit_markdown_pending_from_incomplete_line();
    }

    fn commit_markdown_pending_from_incomplete_line(&mut self) {
        if self.markdown_fence.is_some() || self.markdown_pending_end.is_none() {
            return;
        }
        let Some(range) = self.line_range(self.markdown_scanned_lines) else {
            return;
        };
        let scan_end = range
            .start
            .saturating_add(MAX_MARKDOWN_FENCE_SCAN_BYTES)
            .min(range.end);
        let source = self.slice(range.start..scan_end);
        let body = source.trim_end();
        if body.is_empty()
            || self.markdown_reference_sensitive
            || markdown_line_may_use_reference(body)
        {
            return;
        }
        let line_kind = markdown_line_kind(body);
        let stable = self.markdown_container == MarkdownContainer::Plain
            || markdown_line_kind_is_definitive(body, line_kind);
        let continues = self.markdown_container.continues_across_blank(line_kind);
        drop(source);
        if stable && !continues {
            let end = self.markdown_pending_end.take().unwrap();
            self.commit_markdown_range(end);
            self.markdown_block_has_content = false;
            self.markdown_container = MarkdownContainer::default();
        }
    }

    fn commit_markdown_range(&mut self, end: usize) {
        if self.markdown_stable_ends.last().copied() == Some(end) {
            return;
        }
        let completed_start = self.markdown_stable_ends.last().copied().unwrap_or(0);
        self.markdown_max_completed_bytes = self
            .markdown_max_completed_bytes
            .max(end.saturating_sub(completed_start));
        self.markdown_stable_ends.push(end);
    }

    fn ensure_text_layout(&mut self, width: u16, ansi: bool) -> usize {
        let width = width.max(1);
        self.text_layout_clock = self
            .text_layout_clock
            .checked_add(1)
            .expect("transcript text layout clock overflow");
        let mut layout = self
            .text_layouts
            .iter()
            .position(|layout| layout.width == width && layout.ansi == ansi)
            .map(|index| self.text_layouts.swap_remove(index))
            .unwrap_or_else(|| ContentTextLayout {
                width,
                ansi,
                line_rows: Vec::new(),
                prefix_rows: vec![0],
                dirty_from: 0,
                tail_lines: Vec::new(),
                last_used: 0,
            });

        let logical_lines = self.logical_line_count();
        let dirty_from = layout.dirty_from.min(layout.line_rows.len());
        layout.line_rows.truncate(dirty_from);
        layout.prefix_rows.truncate(dirty_from.saturating_add(1));
        for line in dirty_from..logical_lines {
            let row_count = count_text_line_rows(self, line, width, ansi);
            layout.line_rows.push(row_count);
            let previous = layout.prefix_rows.last().copied().unwrap_or_default();
            layout.prefix_rows.push(previous.saturating_add(row_count));
        }
        layout.dirty_from = logical_lines;
        layout.last_used = self.text_layout_clock;

        if self.text_layouts.len() >= TEXT_LAYOUT_CACHE_WIDTHS {
            let oldest = self
                .text_layouts
                .iter()
                .enumerate()
                .min_by_key(|(_, candidate)| candidate.last_used)
                .map(|(index, _)| index)
                .unwrap_or_default();
            self.text_layouts.swap_remove(oldest);
        }
        self.text_layouts.push(layout);
        self.text_layouts.len().saturating_sub(1)
    }

    fn ensure_file_layout(&mut self, width: u16) -> usize {
        let width = width.max(1);
        self.file_layout_clock = self
            .file_layout_clock
            .checked_add(1)
            .expect("transcript file layout clock overflow");
        if let Some(index) = self
            .file_layouts
            .iter()
            .position(|layout| layout.width == width)
        {
            self.file_layouts[index].last_used = self.file_layout_clock;
            return index;
        }

        let mut layout = ContentFileLayout::new(width);
        for (start, chunk) in self.chunk_starts.iter().copied().zip(&self.chunks) {
            layout.append(start, chunk.as_str());
        }
        layout.last_used = self.file_layout_clock;
        if self.file_layouts.len() >= TEXT_LAYOUT_CACHE_WIDTHS {
            let oldest = self
                .file_layouts
                .iter()
                .enumerate()
                .min_by_key(|(_, candidate)| candidate.last_used)
                .map(|(index, _)| index)
                .unwrap_or_default();
            self.file_layouts.swap_remove(oldest);
        }
        self.file_layouts.push(layout);
        self.file_layouts.len().saturating_sub(1)
    }

    fn logical_line_count(&self) -> usize {
        if self.byte_len == 0 {
            0
        } else if self.line_starts.last().copied() == Some(self.byte_len) {
            self.line_starts.len().saturating_sub(1)
        } else {
            self.line_starts.len()
        }
    }

    fn line_range(&self, line: usize) -> Option<Range<usize>> {
        let start = *self.line_starts.get(line)?;
        let mut end = self
            .line_starts
            .get(line.saturating_add(1))
            .copied()
            .unwrap_or(self.byte_len);
        if end > start && self.byte_at(end - 1) == Some(b'\n') {
            end -= 1;
        }
        if end > start && self.byte_at(end - 1) == Some(b'\r') {
            end -= 1;
        }
        Some(start..end)
    }

    fn line(&self, line: usize) -> Option<Cow<'_, str>> {
        self.line_range(line).map(|range| self.slice(range))
    }

    fn bounded_markdown_line(&self, line: usize) -> Option<(Cow<'_, str>, bool)> {
        let range = self.line_range(line)?;
        let complete = range.len() <= MAX_MARKDOWN_FENCE_SCAN_BYTES;
        let end = range
            .start
            .saturating_add(MAX_MARKDOWN_FENCE_SCAN_BYTES)
            .min(range.end);
        Some((self.slice(range.start..end), complete))
    }

    fn ends_with_markdown_heading(&self) -> bool {
        let mut previous_adjacent = self.markdown_tail_previous_adjacent;
        let mut last_nonempty = self.markdown_tail_last_nonempty;
        if self.markdown_scanned_lines < self.logical_line_count() {
            let line = self.markdown_scanned_lines;
            if !self.line_blank.get(line).copied().unwrap_or(true) {
                let Some((source, _)) = self.bounded_markdown_line(line) else {
                    return false;
                };
                let in_code = self.markdown_fence.is_some()
                    || markdown_fence_marker(source.trim_end()).is_some();
                previous_adjacent = self.markdown_tail_adjacent;
                last_nonempty = Some(MarkdownTailLine { line, in_code });
            }
        }

        let Some(last) = last_nonempty.filter(|line| !line.in_code) else {
            return false;
        };
        let Some((last_source, last_complete)) = self.bounded_markdown_line(last.line) else {
            return false;
        };
        if crate::content::markdown_ir::is_atx_heading(&last_source) {
            return true;
        }
        if !last_complete || !crate::content::markdown_ir::is_setext_underline(&last_source) {
            return false;
        }

        let Some(previous) = previous_adjacent.filter(|line| !line.in_code) else {
            return false;
        };
        let Some((previous_source, _)) = self.bounded_markdown_line(previous.line) else {
            return false;
        };
        !crate::content::markdown_ir::is_atx_heading(&previous_source)
            && !crate::content::markdown_ir::is_thematic_break(&previous_source)
    }

    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        let start = range.start.min(self.byte_len);
        let end = range.end.max(start).min(self.byte_len);
        if start == end {
            return Cow::Borrowed("");
        }

        let Some(first_chunk) = self.chunk_index(start) else {
            return Cow::Borrowed("");
        };
        let first_start = self.chunk_starts[first_chunk];
        let first_end = first_start.saturating_add(self.chunks[first_chunk].len());
        if end <= first_end {
            return Cow::Borrowed(smelt_buffer::text::slice(
                &self.chunks[first_chunk],
                start.saturating_sub(first_start)..end.saturating_sub(first_start),
            ));
        }

        let mut output = String::with_capacity(end.saturating_sub(start));
        for index in first_chunk..self.chunks.len() {
            let chunk = &self.chunks[index];
            let chunk_start = self.chunk_starts[index];
            if chunk_start >= end {
                break;
            }
            let local_start = start.saturating_sub(chunk_start);
            let local_end = end.saturating_sub(chunk_start).min(chunk.len());
            output.push_str(smelt_buffer::text::slice(chunk, local_start..local_end));
        }
        Cow::Owned(output)
    }

    fn visit_slice(&self, range: Range<usize>, mut visit: impl FnMut(&str)) {
        let start = range.start.min(self.byte_len);
        let end = range.end.max(start).min(self.byte_len);
        if start == end {
            return;
        }
        let Some(first_chunk) = self.chunk_index(start) else {
            return;
        };
        for index in first_chunk..self.chunks.len() {
            let chunk = &self.chunks[index];
            let chunk_start = self.chunk_starts[index];
            if chunk_start >= end {
                break;
            }
            let local_start = start.saturating_sub(chunk_start);
            let local_end = end.saturating_sub(chunk_start).min(chunk.len());
            let text = smelt_buffer::text::slice(chunk, local_start..local_end);
            if !text.is_empty() {
                visit(text);
            }
        }
    }

    fn visit_chars(&self, range: Range<usize>, mut visit: impl FnMut(usize, char)) {
        let mut offset = range.start.min(self.byte_len);
        self.visit_slice(range, |fragment| {
            for ch in fragment.chars() {
                visit(offset, ch);
                offset = offset.saturating_add(ch.len_utf8());
            }
        });
    }

    fn source_range_needs_materialization(&self, range: Range<usize>, ansi: bool) -> bool {
        let mut needs_materialization = false;
        self.visit_chars(range, |_, ch| {
            needs_materialization |= ch == '\t' || (ansi && ch.is_control());
        });
        needs_materialization
    }

    fn chunk_index(&self, offset: usize) -> Option<usize> {
        if offset >= self.byte_len {
            return None;
        }
        self.chunk_starts
            .partition_point(|start| *start <= offset)
            .checked_sub(1)
    }

    fn byte_at(&self, offset: usize) -> Option<u8> {
        let index = self.chunk_index(offset)?;
        self.chunks[index]
            .as_bytes()
            .get(offset.saturating_sub(self.chunk_starts[index]))
            .copied()
    }

    fn truncate(&mut self, requested_len: usize) {
        let requested_len = requested_len.min(self.byte_len);
        if requested_len == self.byte_len {
            return;
        }
        let mut keep = requested_len;
        let mut retained = Vec::with_capacity(self.chunks.len());
        for chunk in self.chunks.drain(..) {
            if keep == 0 {
                break;
            }
            if chunk.len() <= keep {
                keep -= chunk.len();
                retained.push(chunk);
                continue;
            }
            let chunk = chunk.prefix(keep);
            if !chunk.is_empty() {
                retained.push(chunk);
            }
            keep = 0;
        }

        let next_revision = self
            .revision
            .checked_add(1)
            .expect("transcript content revision overflow");
        *self = Self::empty();
        for chunk in retained {
            self.append(chunk);
        }
        self.revision = next_revision;
    }
}

fn content_fragment_metrics(fragment: &str) -> (usize, usize) {
    if fragment.is_ascii() {
        return (fragment.len(), fragment.len());
    }
    fragment.chars().fold((0usize, 0usize), |counts, ch| {
        (
            counts.0.saturating_add(1),
            counts.1.saturating_add(if ch.is_ascii() {
                1
            } else {
                smelt_buffer::cell_width::char_width(ch)
            }),
        )
    })
}

fn is_markdown_syntax_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'\\'
            | b'`'
            | b'*'
            | b'_'
            | b'['
            | b']'
            | b'('
            | b')'
            | b'#'
            | b'>'
            | b'-'
            | b'+'
            | b'='
            | b'|'
            | b'~'
            | b'!'
            | b'&'
            | b'<'
    )
}

fn markdown_line_kind(line: &str) -> MarkdownLineKind {
    let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
    let body = smelt_buffer::text::slice(line, spaces.min(3)..line.len());
    if markdown_list_marker(body) {
        MarkdownLineKind::ListMarker
    } else if spaces > 0 || line.starts_with('\t') {
        MarkdownLineKind::Indented
    } else {
        MarkdownLineKind::Plain
    }
}

fn markdown_list_marker(line: &str) -> bool {
    let bytes = line.as_bytes();
    if matches!(bytes.first(), Some(b'-' | b'+' | b'*')) {
        return bytes.get(1).is_some_and(u8::is_ascii_whitespace);
    }
    let digits = bytes
        .iter()
        .take(9)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    digits > 0
        && matches!(bytes.get(digits), Some(b'.' | b')'))
        && bytes
            .get(digits.saturating_add(1))
            .is_some_and(u8::is_ascii_whitespace)
}

fn markdown_line_kind_is_definitive(line: &str, kind: MarkdownLineKind) -> bool {
    if kind != MarkdownLineKind::Plain {
        return true;
    }
    !matches!(
        line.as_bytes().first(),
        Some(b'-' | b'+' | b'*' | b'0'..=b'9')
    )
}

fn markdown_line_may_use_reference(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut start = 0usize;
    while let Some(relative) = bytes[start..].iter().position(|byte| *byte == b'[') {
        let open = start.saturating_add(relative);
        if open > 0 && bytes[open - 1] == b'\\' {
            start = open.saturating_add(1);
            continue;
        }
        let Some(close_relative) = bytes[open.saturating_add(1)..]
            .iter()
            .position(|byte| *byte == b']')
        else {
            return false;
        };
        let close = open.saturating_add(1).saturating_add(close_relative);
        if bytes.get(close.saturating_add(1)) != Some(&b'(') {
            return true;
        }
        start = close.saturating_add(2);
        if start >= bytes.len() {
            break;
        }
    }
    false
}

fn markdown_fence_marker(line: &str) -> Option<(u8, usize)> {
    let indent = line
        .bytes()
        .take_while(|byte| *byte == b' ')
        .take(4)
        .count();
    if indent > 3 {
        return None;
    }
    let line = smelt_buffer::text::slice(line, indent..line.len());
    let marker = line.as_bytes().first().copied()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = line.bytes().take_while(|byte| *byte == marker).count();
    (len >= 3).then_some((marker, len))
}

impl ContentFileLayout {
    fn new(width: u16) -> Self {
        Self {
            width: width.max(1),
            wrap_ends: Vec::new(),
            active_line: false,
            active_row_cells: 0,
            active_row_trailing_cr: false,
            last_used: 0,
        }
    }

    fn append(&mut self, chunk_start: usize, chunk: &str) {
        for (local_offset, ch) in chunk.char_indices() {
            let offset = chunk_start.saturating_add(local_offset);
            if ch == '\n' {
                if !self.active_line {
                    self.begin_line();
                }
                self.active_line = false;
                self.active_row_cells = 0;
                self.active_row_trailing_cr = false;
                continue;
            }

            if !self.active_line {
                self.begin_line();
            }
            if self.active_row_trailing_cr {
                self.active_row_trailing_cr = false;
            }
            if ch == '\r' {
                self.active_row_trailing_cr = true;
                continue;
            }

            let cells = if ch == '\t' {
                4
            } else {
                smelt_buffer::cell_width::char_width(ch)
            };
            if self.active_row_cells.saturating_add(cells) > usize::from(self.width)
                && self.active_row_cells > 0
            {
                self.wrap_ends.push(offset);
                self.active_row_cells = 0;
            }
            self.active_row_cells = self.active_row_cells.saturating_add(cells);
        }
    }

    fn begin_line(&mut self) {
        self.active_line = true;
        self.active_row_cells = 0;
        self.active_row_trailing_cr = false;
    }
}

fn line_uses_hard_cell_wrap(
    entry: &ContentEntry,
    line: usize,
    source_range: &Range<usize>,
) -> bool {
    entry.line_ascii_words.get(line).copied().unwrap_or(false)
        || (source_range.len() > MAX_WORD_WRAP_LINE_BYTES
            && entry.line_ascii_cells.get(line).copied().unwrap_or(false))
}

fn count_text_line_rows(entry: &ContentEntry, line: usize, width: u16, ansi: bool) -> usize {
    let source_range = entry.line_range(line).unwrap_or_default();
    if line_uses_hard_cell_wrap(entry, line, &source_range)
        && (!ansi || entry.ansi_state_for_line(line) == crate::content::ansi::AnsiState::default())
    {
        return source_range
            .len()
            .max(1)
            .div_ceil(usize::from(width.max(1)));
    }
    let content = entry.slice(source_range);
    let expanded = content
        .contains('\t')
        .then(|| content.replace('\t', "    "));
    let content = expanded.as_deref().unwrap_or(&content);
    let mut ansi_state = entry.ansi_state_for_line(line);
    if ansi
        && (ansi_state != crate::content::ansi::AnsiState::default()
            || content
                .chars()
                .any(|ch| ch == '\x1b' || (ch.is_control() && ch != '\t')))
    {
        crate::content::ansi::wrap_ansi_with_state(
            content,
            usize::from(width.max(1)),
            &mut ansi_state,
        )
        .1
        .len()
    } else {
        smelt_buffer::wrap::count_line_ranges(content, usize::from(width.max(1)))
    }
}

fn materialize_text_line_edge_rows(
    entry: &ContentEntry,
    line: usize,
    width: u16,
    ansi: bool,
    max_rows: usize,
    tail: bool,
) -> (usize, usize, Vec<RetainedContentTextRow>) {
    let source_range = entry.line_range(line).unwrap_or_default();
    let ansi_state = entry.ansi_state_for_line(line);
    if line_uses_hard_cell_wrap(entry, line, &source_range)
        && (!ansi || ansi_state == crate::content::ansi::AnsiState::default())
    {
        let line_rows = source_range
            .len()
            .max(1)
            .div_ceil(usize::from(width.max(1)));
        let start = if tail {
            line_rows.saturating_sub(max_rows)
        } else {
            0
        };
        let end = if tail {
            line_rows
        } else {
            max_rows.min(line_rows)
        };
        return (
            line_rows,
            start,
            materialize_hard_cell_rows(source_range, width, start..end),
        );
    }

    let mut rows = materialize_text_line_rows(entry, line, width, ansi, 0..usize::MAX);
    let line_rows = rows.len();
    let start = if tail {
        line_rows.saturating_sub(max_rows)
    } else {
        0
    };
    let end = if tail {
        line_rows
    } else {
        max_rows.min(line_rows)
    };
    rows.truncate(end);
    if start != 0 {
        rows.drain(..start);
    }
    (line_rows, start, rows)
}

fn materialize_text_line_rows(
    entry: &ContentEntry,
    line: usize,
    width: u16,
    ansi: bool,
    row_range: Range<usize>,
) -> Vec<RetainedContentTextRow> {
    let source_range = entry.line_range(line).unwrap_or_default();
    let ansi_state = entry.ansi_state_for_line(line);
    if line_uses_hard_cell_wrap(entry, line, &source_range)
        && (!ansi || ansi_state == crate::content::ansi::AnsiState::default())
    {
        return materialize_hard_cell_rows(source_range, width, row_range);
    }

    let mut rows = Vec::new();
    if entry.source_range_needs_materialization(source_range.clone(), ansi)
        || (ansi && ansi_state != crate::content::ansi::AnsiState::default())
    {
        let content = entry.slice(source_range);
        let expanded = content
            .contains('\t')
            .then(|| content.replace('\t', "    "));
        retain_text_rows(
            &mut rows,
            expanded.as_deref().unwrap_or(&content),
            None,
            width.max(1),
            ansi,
            ansi_state,
        );
    } else {
        retain_source_text_rows(entry, &mut rows, source_range, width.max(1));
    }
    let start = row_range.start.min(rows.len());
    let end = row_range.end.max(start).min(rows.len());
    rows.truncate(end);
    if start != 0 {
        rows.drain(..start);
    }
    rows
}

fn materialize_hard_cell_rows(
    source_range: Range<usize>,
    width: u16,
    row_range: Range<usize>,
) -> Vec<RetainedContentTextRow> {
    let width = usize::from(width.max(1));
    let total_rows = source_range.len().max(1).div_ceil(width);
    let start_row = row_range.start.min(total_rows);
    let end_row = row_range.end.max(start_row).min(total_rows);
    (start_row..end_row)
        .map(|row| {
            let start = source_range
                .start
                .saturating_add(row.saturating_mul(width))
                .min(source_range.end);
            let end = start.saturating_add(width).min(source_range.end);
            RetainedContentTextRow {
                text: RetainedContentText::Source(start..end),
                spans: Vec::new(),
                wrapped: total_rows > 1,
                continuation: row != 0,
            }
        })
        .collect()
}

struct SourceWrapState {
    chunk_start: usize,
    chunk_end: usize,
    column: usize,
    word_start: usize,
    has_non_space: bool,
}

fn retain_source_text_rows(
    entry: &ContentEntry,
    rows: &mut Vec<RetainedContentTextRow>,
    source_range: Range<usize>,
    width: u16,
) -> usize {
    let first_row = rows.len();
    let mut state = SourceWrapState {
        chunk_start: source_range.start,
        chunk_end: source_range.start,
        column: 0,
        word_start: source_range.start,
        has_non_space: false,
    };
    entry.visit_chars(source_range.clone(), |offset, ch| {
        if ch == ' ' {
            retain_source_word(
                entry,
                rows,
                first_row,
                &mut state,
                offset,
                true,
                usize::from(width),
            );
        }
    });
    retain_source_word(
        entry,
        rows,
        first_row,
        &mut state,
        source_range.end,
        false,
        usize::from(width),
    );
    push_source_row(rows, first_row, state.chunk_start..state.chunk_end);

    let row_count = rows.len().saturating_sub(first_row);
    if row_count > 1 {
        for row in &mut rows[first_row..] {
            row.wrapped = true;
        }
    }
    row_count
}

fn retain_source_word(
    entry: &ContentEntry,
    rows: &mut Vec<RetainedContentTextRow>,
    first_row: usize,
    state: &mut SourceWrapState,
    word_end: usize,
    trailing_space: bool,
    width: usize,
) {
    let mut word_width = 0usize;
    entry.visit_chars(state.word_start..word_end, |_, ch| {
        word_width = word_width.saturating_add(smelt_buffer::cell_width::char_width(ch));
    });
    let total_width = word_width.saturating_add(usize::from(trailing_space));
    if state.column.saturating_add(total_width) > width
        && state.column > 0
        && (word_width <= width || state.has_non_space)
    {
        push_source_row(rows, first_row, state.chunk_start..state.chunk_end);
        state.chunk_start = state.word_start;
        state.column = 0;
        state.has_non_space = false;
    }

    if word_width > width {
        entry.visit_chars(state.word_start..word_end, |offset, ch| {
            let char_end = offset.saturating_add(ch.len_utf8());
            let char_width = smelt_buffer::cell_width::char_width(ch);
            if state.column.saturating_add(char_width) > width && state.column > 0 {
                push_source_row(rows, first_row, state.chunk_start..state.chunk_end);
                state.chunk_start = offset;
                state.column = 0;
                state.has_non_space = false;
            }
            state.chunk_end = char_end;
            state.column = state.column.saturating_add(char_width);
            state.has_non_space = true;
        });
    } else {
        state.chunk_end = word_end;
        state.column = state.column.saturating_add(word_width);
        state.has_non_space |= state.word_start < word_end;
    }

    if trailing_space {
        if state.column.saturating_add(1) > width && state.column > 0 {
            push_source_row(rows, first_row, state.chunk_start..state.chunk_end);
            state.chunk_start = word_end.saturating_add(1);
            state.chunk_end = state.chunk_start;
            state.column = 0;
            state.has_non_space = false;
        } else {
            state.chunk_end = word_end.saturating_add(1);
            state.column = state.column.saturating_add(1);
        }
        state.word_start = word_end.saturating_add(1);
    }
}

fn push_source_row(
    rows: &mut Vec<RetainedContentTextRow>,
    first_row: usize,
    source_range: Range<usize>,
) {
    rows.push(RetainedContentTextRow {
        text: RetainedContentText::Source(source_range),
        spans: Vec::new(),
        wrapped: false,
        continuation: rows.len() > first_row,
    });
}

fn retain_text_rows(
    rows: &mut Vec<RetainedContentTextRow>,
    content: &str,
    source_start: Option<usize>,
    width: u16,
    ansi: bool,
    mut ansi_state: crate::content::ansi::AnsiState,
) -> usize {
    let requires_ansi = ansi
        && (ansi_state != crate::content::ansi::AnsiState::default()
            || content
                .chars()
                .any(|ch| ch == '\x1b' || (ch.is_control() && ch != '\t')));
    if requires_ansi {
        let (spans, ranges, boundaries) = crate::content::ansi::wrap_ansi_with_state(
            content,
            usize::from(width),
            &mut ansi_state,
        );
        let wrapped = ranges.len() > 1;
        let row_count = ranges.len();
        let single_span_row = matches!(
            (ranges.as_slice(), spans.as_slice()),
            ([(_, end)], [span]) if *end == span.text.len()
        );
        if single_span_row {
            let span = spans.into_iter().next().expect("single ANSI span");
            let text_len = span.text.len();
            let retained_spans = if text_len == 0 || span.style == crate::style::Style::default() {
                Vec::new()
            } else {
                vec![ContentTextSpan {
                    byte_range: 0..text_len,
                    style: span.style,
                }]
            };
            rows.push(RetainedContentTextRow {
                text: RetainedContentText::Owned(span.text),
                spans: retained_spans,
                wrapped: false,
                continuation: false,
            });
            return 1;
        }
        rows.reserve(row_count);
        rows.extend(
            ranges
                .into_iter()
                .enumerate()
                .map(|(row_index, (start, end))| {
                    let mut text = String::with_capacity(end.saturating_sub(start));
                    let mut retained_spans: Vec<ContentTextSpan> = Vec::new();
                    for (span_index, span) in spans.iter().enumerate() {
                        let span_start = boundaries[span_index];
                        let span_end = boundaries[span_index.saturating_add(1)];
                        let overlap_start = start.max(span_start);
                        let overlap_end = end.min(span_end);
                        if overlap_start >= overlap_end {
                            continue;
                        }
                        let row_span_start = text.len();
                        text.push_str(smelt_buffer::text::slice(
                            &span.text,
                            overlap_start.saturating_sub(span_start)
                                ..overlap_end.saturating_sub(span_start),
                        ));
                        let row_span_end = text.len();
                        if let Some(previous) = retained_spans.last_mut().filter(|previous| {
                            previous.style == span.style
                                && previous.byte_range.end == row_span_start
                        }) {
                            previous.byte_range.end = row_span_end;
                        } else {
                            retained_spans.push(ContentTextSpan {
                                byte_range: row_span_start..row_span_end,
                                style: span.style,
                            });
                        }
                    }
                    RetainedContentTextRow {
                        text: RetainedContentText::Owned(text),
                        spans: retained_spans,
                        wrapped,
                        continuation: row_index != 0,
                    }
                }),
        );
        return row_count;
    }

    let width = usize::from(width);
    let row_count = smelt_buffer::wrap::count_line_ranges(content, width);
    let wrapped = row_count > 1;
    rows.reserve(row_count);
    let mut row_index = 0usize;
    smelt_buffer::wrap::visit_line_ranges(content, width, |start, end| {
        let text = source_start.map_or_else(
            || {
                RetainedContentText::Owned(
                    smelt_buffer::text::slice(content, start..end).to_string(),
                )
            },
            |source_start| {
                RetainedContentText::Source(
                    source_start.saturating_add(start)..source_start.saturating_add(end),
                )
            },
        );
        rows.push(RetainedContentTextRow {
            text,
            spans: Vec::new(),
            wrapped,
            continuation: row_index != 0,
        });
        row_index = row_index.saturating_add(1);
    });
    row_count
}

impl ContentRead<'_> {
    pub fn len(&self) -> usize {
        self.entry.byte_len
    }

    pub fn is_empty(&self) -> bool {
        self.entry.byte_len == 0
    }

    pub fn char_len(&self) -> usize {
        self.entry.char_len
    }

    pub fn display_cells(&self) -> usize {
        self.entry.display_cells
    }

    pub fn revision(&self) -> u64 {
        self.entry.revision
    }

    pub fn content_hash(&self) -> u64 {
        self.entry.hash
    }

    pub fn chunks(&self) -> &[SharedContentSlice] {
        &self.entry.chunks
    }

    pub(crate) fn split_line_count(&self) -> usize {
        self.entry.line_starts.len()
    }

    pub(crate) fn split_line_range(&self, line: usize) -> Option<Range<usize>> {
        let start = *self.entry.line_starts.get(line)?;
        let end = self
            .entry
            .line_starts
            .get(line.saturating_add(1))
            .map_or(self.entry.byte_len, |next| next.saturating_sub(1));
        Some(start..end.max(start))
    }

    pub(crate) fn trimmed_range(&self, range: Range<usize>) -> Range<usize> {
        let start = range.start.min(self.len());
        let range = start..range.end.max(start).min(self.len());
        let mut first = None;
        let mut last = range.start;
        self.entry.visit_chars(range.clone(), |offset, ch| {
            if !ch.is_whitespace() {
                first.get_or_insert(offset);
                last = offset.saturating_add(ch.len_utf8());
            }
        });
        first.map_or(range.end..range.end, |start| start..last)
    }

    pub(crate) fn byte_at(&self, offset: usize) -> Option<u8> {
        self.entry.byte_at(offset)
    }

    pub fn checkpoints(&self) -> &[ContentCheckpoint] {
        &self.entry.checkpoints
    }

    pub fn logical_line_count(&self) -> usize {
        if self.entry.byte_len == 0 {
            0
        } else if self.entry.line_starts.last().copied() == Some(self.entry.byte_len) {
            self.entry.line_starts.len().saturating_sub(1)
        } else {
            self.entry.line_starts.len()
        }
    }

    pub(crate) fn whole_line_prefix_len(&self, byte_end: usize) -> usize {
        let line_count = self.logical_line_count();
        let byte_end = byte_end.min(self.len());
        if byte_end == self.len() {
            return line_count;
        }
        self.entry.line_starts[..line_count]
            .partition_point(|start| *start <= byte_end)
            .saturating_sub(1)
    }

    pub(crate) fn whole_line_suffix_len(&self, byte_start: usize) -> usize {
        let line_count = self.logical_line_count();
        let byte_start = byte_start.min(self.len());
        let first =
            self.entry.line_starts[..line_count].partition_point(|start| *start < byte_start);
        line_count.saturating_sub(first)
    }

    pub fn line_range(&self, line: usize) -> Option<Range<usize>> {
        self.entry.line_range(line)
    }

    pub fn line(&self, line: usize) -> Option<Cow<'_, str>> {
        self.entry.line(line)
    }

    pub fn line_is_ascii_word(&self, line: usize) -> bool {
        self.entry
            .line_ascii_words
            .get(line)
            .copied()
            .unwrap_or(false)
    }

    pub fn line_has_ascii_cells(&self, line: usize) -> bool {
        self.entry
            .line_ascii_cells
            .get(line)
            .copied()
            .unwrap_or(false)
    }

    pub fn large_lines_have_ascii_cells(&self) -> bool {
        self.entry.large_lines_have_ascii_cells
    }

    pub fn line_is_plain_markdown(&self, line: usize) -> bool {
        self.entry
            .line_plain_markdown
            .get(line)
            .copied()
            .unwrap_or(false)
    }

    pub fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        self.entry.slice(range)
    }

    pub fn markdown_range_count(&self) -> usize {
        self.entry.markdown_stable_ends.len().saturating_add(1)
    }

    pub fn markdown_range(&self, index: usize) -> Option<Range<usize>> {
        let start = if index == 0 {
            0
        } else {
            *self.entry.markdown_stable_ends.get(index - 1)?
        };
        let end = self
            .entry
            .markdown_stable_ends
            .get(index)
            .copied()
            .unwrap_or(self.entry.byte_len);
        (index <= self.entry.markdown_stable_ends.len()).then_some(start..end)
    }

    pub fn markdown_completed_ranges_after(&self, byte_offset: usize) -> Vec<Range<usize>> {
        let mut start = byte_offset.min(self.len());
        self.entry
            .markdown_stable_ends
            .iter()
            .copied()
            .filter_map(|end| {
                if end <= start {
                    return None;
                }
                let range = start..end;
                start = end;
                Some(range)
            })
            .collect()
    }

    pub fn markdown_suffix_range(&self) -> Range<usize> {
        self.entry
            .markdown_stable_ends
            .last()
            .copied()
            .unwrap_or_default()..self.len()
    }

    pub fn markdown_has_range_larger_than(&self, bytes: usize) -> bool {
        self.entry.markdown_max_completed_bytes > bytes
            || self.markdown_suffix_range().len() > bytes
    }

    pub fn first_nonempty_line(&self) -> String {
        (0..self.logical_line_count())
            .filter_map(|line| self.line(line))
            .find(|line| !line.trim().is_empty())
            .map(Cow::into_owned)
            .unwrap_or_default()
    }
}

impl std::fmt::Display for ContentRead<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for chunk in &self.entry.chunks {
            std::fmt::Write::write_str(formatter, chunk)?;
        }
        Ok(())
    }
}

impl From<String> for TranscriptContent {
    fn from(content: String) -> Self {
        Self::from(Arc::new(content))
    }
}

impl From<Arc<String>> for TranscriptContent {
    fn from(content: Arc<String>) -> Self {
        let id = ContentId(NEXT_CONTENT_ID.fetch_add(1, Ordering::Relaxed));
        let mut entry = ContentEntry::empty();
        entry.append(SharedContentSlice::from_shared(content));
        Self {
            id,
            inner: Arc::new(RwLock::new(entry)),
        }
    }
}

impl From<&str> for TranscriptContent {
    fn from(content: &str) -> Self {
        Self::from(content.to_owned())
    }
}

impl std::fmt::Debug for TranscriptContent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let read = self.read();
        formatter
            .debug_struct("TranscriptContent")
            .field("id", &self.id)
            .field("bytes", &read.len())
            .field("chunks", &read.chunks().len())
            .field("revision", &read.revision())
            .finish()
    }
}

impl PartialEq for TranscriptContent {
    fn eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.inner, &other.inner) {
            return true;
        }
        let self_ptr = Arc::as_ptr(&self.inner) as usize;
        let other_ptr = Arc::as_ptr(&other.inner) as usize;
        if self_ptr < other_ptr {
            let left = self.read();
            let right = other.read();
            content_reads_equal(&left, &right)
        } else {
            let right = other.read();
            let left = self.read();
            content_reads_equal(&left, &right)
        }
    }
}

fn content_reads_equal(left: &ContentRead<'_>, right: &ContentRead<'_>) -> bool {
    left.len() == right.len()
        && left.content_hash() == right.content_hash()
        && left
            .chunks()
            .iter()
            .flat_map(|chunk| chunk.bytes())
            .eq(right.chunks().iter().flat_map(|chunk| chunk.bytes()))
}

fn content_read_equals_str(content: &ContentRead<'_>, text: &str) -> bool {
    content.len() == text.len()
        && content
            .chunks()
            .iter()
            .flat_map(|chunk| chunk.bytes())
            .eq(text.bytes())
}

impl Eq for TranscriptContent {}

impl PartialEq<String> for TranscriptContent {
    fn eq(&self, other: &String) -> bool {
        content_read_equals_str(&self.read(), other)
    }
}

impl PartialEq<str> for TranscriptContent {
    fn eq(&self, other: &str) -> bool {
        content_read_equals_str(&self.read(), other)
    }
}

impl serde::Serialize for TranscriptContent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.snapshot())
    }
}

impl<'de> serde::Deserialize<'de> for TranscriptContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_appends_retain_chunks_and_increment_metadata() {
        let mut content = TranscriptContent::from("alpha\n".to_string());
        let first_hash = content.content_hash();
        assert_eq!(content.push_owned("βeta".to_string()), 6..11);

        let read = content.read();
        assert_eq!(read.chunks().len(), 2);
        assert_eq!(read.len(), 11);
        assert_eq!(read.char_len(), 10);
        assert_eq!(read.logical_line_count(), 2);
        assert_eq!(read.line(0).as_deref(), Some("alpha"));
        assert_eq!(read.line(1).as_deref(), Some("βeta"));
        assert_ne!(read.content_hash(), first_hash);
        assert_eq!(read.revision(), 2);
    }

    #[test]
    fn trimming_reuses_normalized_content_and_copies_only_the_retained_range() {
        let normalized = TranscriptContent::from("already normalized");
        let normalized_id = normalized.id();
        let normalized = normalized.into_trimmed();
        assert_eq!(normalized.id(), normalized_id);

        let content = TranscriptContent::from(" \n".to_string());
        content.append_owned("\tα".to_string());
        content.append_owned(" beta ".to_string());
        content.append_owned("\n ".to_string());
        let trimmed = content.into_trimmed();
        assert_eq!(trimmed.snapshot(), "α beta");
        assert_eq!(trimmed.read().chunks().len(), 2);

        assert!(TranscriptContent::from(" \n\t ").into_trimmed().is_empty());
    }

    #[test]
    fn content_equality_streams_across_different_chunk_boundaries() {
        let chunked = TranscriptContent::from("α".to_string());
        chunked.append_owned("beta".to_string());
        let contiguous = TranscriptContent::from("αbeta");

        assert_eq!(chunked, contiguous);
        assert!(chunked.eq("αbeta"));
        let mut owned = String::from("α");
        owned.push_str("beta");
        assert!(chunked == owned);
        assert_ne!(chunked, TranscriptContent::from("αbet"));
    }

    #[test]
    fn contains_streams_matches_across_chunk_boundaries() {
        let content = TranscriptContent::from("prefix ab".to_string());
        content.append_owned("abα".to_string());
        content.append_owned("β suffix".to_string());

        assert!(content.contains("abab"));
        assert!(content.contains("αβ"));
        assert!(content.contains(""));
        assert!(!content.contains("abac"));
        assert!(!content.contains("suffix!"));
    }

    #[test]
    fn owned_content_transfers_allocations_and_clones_only_the_handle() {
        let initial = String::from("initial payload");
        let initial_ptr = initial.as_ptr();
        let content = TranscriptContent::from(initial);
        assert_eq!(content.read().chunks()[0].as_ptr(), initial_ptr);

        let cloned = content.clone();
        assert!(Arc::ptr_eq(&content.inner, &cloned.inner));
        assert_eq!(Arc::strong_count(&content.inner), 2);
        assert_eq!(content.retained_bytes(), cloned.retained_bytes());

        let suffix = String::from(" and owned suffix");
        let suffix_ptr = suffix.as_ptr();
        let first_chunk_ptr = content.read().chunks()[0].as_ptr();
        content.append_owned(suffix);
        let read = content.read();
        assert_eq!(read.chunks().len(), 2);
        assert_eq!(read.entry.chunk_starts, [0, 15]);
        assert_eq!(read.chunks()[0].as_ptr(), first_chunk_ptr);
        assert_eq!(read.chunks()[1].as_ptr(), suffix_ptr);
    }

    #[test]
    fn shared_content_constructor_transfers_the_arc_without_copying() {
        let source = Arc::new("shared payload".repeat(1_024));
        let content = TranscriptContent::from(Arc::clone(&source));
        let read = content.read();

        assert!(Arc::ptr_eq(&read.chunks()[0].source, &source));
        assert_eq!(read.chunks()[0].as_ptr(), source.as_ptr());
    }

    #[test]
    fn shared_slices_retain_one_source_allocation() {
        let mut text = String::with_capacity(64);
        text.push_str("aéβz");
        let source = Arc::new(text);
        let source_capacity = source.capacity();
        let content = TranscriptContent::new();

        content.append_shared(SharedContentSlice::new(Arc::clone(&source), 1..3));
        content.append_shared(SharedContentSlice::new(Arc::clone(&source), 3..6));

        let read = content.read();
        assert_eq!(read.to_string(), "éβz");
        assert!(Arc::ptr_eq(
            &read.chunks()[0].source,
            &read.chunks()[1].source
        ));
        assert_eq!(
            retained_chunk_source_bytes(read.chunks()),
            source_capacity + std::mem::size_of::<String>() + ARC_HEADER_BYTES
        );
        drop(read);
        assert!(content.retained_bytes() >= source_capacity);
    }

    #[test]
    fn shared_slice_ranges_snap_to_utf8_boundaries() {
        let source = Arc::new("aéβz".to_string());
        let slice = SharedContentSlice::new(source, 2..4);
        assert_eq!(slice.as_str(), "é");
    }

    #[test]
    fn append_growth_is_linear_in_owned_chunks() {
        let content = TranscriptContent::new();
        let chunk_count = 4_096;
        let chunk_bytes = 32;
        for _ in 0..chunk_count {
            content.append_owned("x".repeat(chunk_bytes));
        }

        let read = content.read();
        assert_eq!(read.len(), chunk_count * chunk_bytes);
        assert_eq!(read.chunks().len(), chunk_count);
        assert_eq!(read.revision(), chunk_count as u64);
        let content_len = read.len();
        let checkpoint_bytes = std::mem::size_of_val(read.checkpoints());
        drop(read);
        assert!(
            content.retained_bytes()
                <= content_len
                    .saturating_add(
                        chunk_count
                            * (std::mem::size_of::<SharedContentSlice>()
                                + std::mem::size_of::<String>()
                                + std::mem::size_of::<usize>()
                                + ARC_HEADER_BYTES),
                    )
                    .saturating_add(checkpoint_bytes)
                    .saturating_add(std::mem::size_of::<TranscriptContent>())
                    .saturating_add(64 * 1024),
            "retained metadata should stay linear in chunk and checkpoint counts"
        );
    }

    #[test]
    fn ranges_and_lines_cross_chunk_boundaries() {
        let mut content = TranscriptContent::from("abc".to_string());
        content.push_owned("def\r".to_string());
        content.push_owned("\nghi\n".to_string());

        let read = content.read();
        assert_eq!(read.entry.chunk_starts, [0, 3, 7]);
        assert_eq!(read.slice(2..8), "cdef\r\n");
        assert!(matches!(read.slice(3..6), Cow::Borrowed("def")));
        assert!(matches!(read.slice(2..8), Cow::Owned(_)));
        assert_eq!(read.logical_line_count(), 2);
        assert_eq!(read.line(0).as_deref(), Some("abcdef"));
        assert_eq!(read.line(1).as_deref(), Some("ghi"));
    }

    #[test]
    fn indexed_ranges_preserve_utf8_across_chunk_boundaries() {
        let content = TranscriptContent::from("aé".to_string());
        content.append_owned("βc\n".to_string());
        content.append_owned("δ".to_string());

        let read = content.read();
        assert_eq!(read.entry.chunk_starts, [0, 3, 7]);
        assert_eq!(read.slice(1..5), "éβ");
        assert_eq!(read.line(0).as_deref(), Some("aéβc"));
        assert!(matches!(read.line(1), Some(Cow::Borrowed("δ"))));
    }

    #[test]
    fn truncate_snaps_utf8_and_rebuilds_indexes() {
        let mut content = TranscriptContent::from("one\n".to_string());
        content.push_owned("éé".to_string());
        let revision = content.revision();
        content.truncate(7);

        let read = content.read();
        assert_eq!(read.to_string(), "one\né");
        assert_eq!(read.entry.chunk_starts, [0, 4]);
        assert_eq!(read.logical_line_count(), 2);
        assert!(read.revision() > revision);
    }

    #[test]
    fn content_store_releases_shared_content_after_last_owner() {
        let content = TranscriptContent::from("shared".to_string());
        let id = content.id();
        let mut store = ContentStore::default();

        store.register(&content);
        store.register(&content);
        store.remove(id);
        assert!(store.get(id).is_some());

        store.remove(id);
        assert!(store.get(id).is_none());
    }

    #[test]
    fn text_layout_index_updates_only_the_changed_suffix() {
        let content = TranscriptContent::from("alpha\nbeta".to_string());
        assert_eq!(content.text_layout_rows(3, false), 4);
        assert_eq!(
            content.text_layout_ranges(3, false, 1..3),
            vec![
                ContentTextRange {
                    line: 0,
                    row_offset: 1,
                    row_count: 1,
                },
                ContentTextRange {
                    line: 1,
                    row_offset: 0,
                    row_count: 1,
                },
            ]
        );

        content.append_owned(" gamma\ndelta".to_string());
        assert_eq!(content.text_layout_rows(3, false), 8);
        assert_eq!(
            content.text_layout_ranges(3, false, 6..8),
            vec![ContentTextRange {
                line: 2,
                row_offset: 0,
                row_count: 2,
            }]
        );
    }

    #[test]
    fn bounded_text_windows_materialize_only_requested_edge_rows() {
        let content = TranscriptContent::from(
            (0..100_000)
                .map(|line| format!("line {line:06}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let retained_before = content.retained_bytes();
        let mut tail = Vec::new();
        let window = content.visit_text_layout_tail_rows(80, true, 3, |row| {
            tail.push((row.text().into_owned(), row.spans().to_vec()));
        });

        assert_eq!(window.row_count, 3);
        assert!(window.truncated);
        assert_eq!(
            tail.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
            ["line 099997", "line 099998", "line 099999"]
        );
        let retained_after = content.retained_bytes();
        assert!(retained_after > retained_before);
        assert!(retained_after <= retained_before.saturating_add(4 * 1024));
        content.visit_text_layout_tail_rows(80, true, 3, |_| {});
        assert_eq!(content.retained_bytes(), retained_after);
    }

    #[test]
    fn tail_row_cache_invalidates_only_appended_logical_suffix() {
        let content = TranscriptContent::from(
            "\u{1b}[31mred one\u{1b}[0m\nstable two\n\u{1b}[32mactive".to_string(),
        );
        let mut rows = Vec::new();
        content.visit_text_layout_tail_rows(80, true, 3, |row| {
            rows.push(row.text().into_owned());
        });
        assert_eq!(rows, ["red one", "stable two", "active"]);
        {
            let entry = content
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let layout = entry
                .text_layouts
                .iter()
                .find(|layout| layout.width == 80 && layout.ansi)
                .expect("tail layout");
            assert_eq!(
                layout
                    .tail_lines
                    .iter()
                    .map(|cached| cached.line)
                    .collect::<Vec<_>>(),
                [0, 1, 2]
            );
        }

        content.append_owned(" extended\u{1b}[0m".to_string());
        {
            let entry = content
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let layout = entry
                .text_layouts
                .iter()
                .find(|layout| layout.width == 80 && layout.ansi)
                .expect("tail layout");
            assert_eq!(
                layout
                    .tail_lines
                    .iter()
                    .map(|cached| cached.line)
                    .collect::<Vec<_>>(),
                [0, 1]
            );
        }

        let mut updated = Vec::new();
        content.visit_text_layout_tail_rows(80, true, 3, |row| {
            updated.push((row.text().into_owned(), row.spans().to_vec()));
        });
        assert_eq!(
            updated
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>(),
            ["red one", "stable two", "active extended"]
        );
        assert!(!updated[2].1.is_empty());
        assert!(updated[2].1.iter().all(|span| span.style.fg.is_some()));
        let entry = content
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let layout = entry
            .text_layouts
            .iter()
            .find(|layout| layout.width == 80 && layout.ansi)
            .expect("tail layout");
        assert_eq!(
            layout
                .tail_lines
                .iter()
                .map(|cached| cached.line)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn bounded_ascii_prose_retains_only_requested_tail_rows() {
        let content = TranscriptContent::from(format!("{}tail", "word ".repeat(20_000)));
        {
            let read = content.read();
            let range = read.line_range(0).expect("large line");
            assert!(read.line_has_ascii_cells(0));
            assert!(read.large_lines_have_ascii_cells());
            assert!(read.line_is_plain_markdown(0));
            assert!(!read.line_is_ascii_word(0));
            assert!(line_uses_hard_cell_wrap(&read.entry, 0, &range));
        }
        let retained_before = content.retained_bytes();
        let mut tail = Vec::new();
        let window = content.visit_text_layout_tail_rows(10, false, 3, |row| {
            tail.push((row.text().into_owned(), row.row_offset()));
        });

        assert_eq!(window.row_count, 3);
        assert!(window.truncated);
        assert_eq!(
            tail,
            [
                ("word word ".to_string(), 9_998),
                ("word word ".to_string(), 9_999),
                ("tail".to_string(), 10_000),
            ]
        );
        let retained_after = content.retained_bytes();
        assert!(retained_after > retained_before);
        assert!(retained_after <= retained_before.saturating_add(4 * 1024));
        content.visit_text_layout_tail_rows(10, false, 3, |_| {});
        assert_eq!(content.retained_bytes(), retained_after);
    }

    #[test]
    fn hard_cell_wrap_excludes_utf8_and_tabs() {
        for suffix in ["界", "\t"] {
            let content = TranscriptContent::from(format!("{}{}", "word ".repeat(14_000), suffix));
            let read = content.read();
            let range = read.line_range(0).expect("large line");
            assert!(range.len() > MAX_WORD_WRAP_LINE_BYTES);
            assert!(!read.line_has_ascii_cells(0));
            assert!(!read.large_lines_have_ascii_cells());
            assert!(!line_uses_hard_cell_wrap(&read.entry, 0, &range));
        }
    }

    #[test]
    fn bounded_text_windows_preserve_utf8_wraps_and_ansi_styles() {
        let content = TranscriptContent::from(
            "ignored\n\u{1b}[31mαβγδεζηθ\u{1b}[0m\n\u{1b}[32m東京 café\u{1b}[0m".to_string(),
        );
        let mut rows = Vec::new();
        let window = content.visit_text_layout_tail_rows(6, true, 3, |row| {
            rows.push((row.text().into_owned(), row.spans().to_vec()));
        });

        assert_eq!(window.row_count, 3);
        assert!(window.truncated);
        assert_eq!(
            rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
            ["ηθ", "東京 ", "café"]
        );
        assert!(rows
            .iter()
            .all(|row| row.1.iter().all(|span| span.style.fg.is_some())));
    }

    #[test]
    fn text_layout_extent_remains_wide_past_terminal_row_limits() {
        let content = TranscriptContent::from("x\n".repeat(70_000));
        assert_eq!(content.text_layout_rows(80, false), 70_000);
        assert!(content.text_layout_rows(1, false) > usize::from(u16::MAX));
    }

    #[test]
    fn text_layout_suffix_reflow_matches_full_rebuild_across_chunk_boundaries() {
        fn rows(
            content: &TranscriptContent,
            width: u16,
            ansi: bool,
        ) -> Vec<(String, Vec<ContentTextSpan>, bool, bool)> {
            let mut rows = Vec::new();
            content.visit_text_layout_rows(width, ansi, 0..usize::MAX, |row| {
                rows.push((
                    row.text().into_owned(),
                    row.spans().to_vec(),
                    row.wrapped(),
                    row.continuation(),
                ));
            });
            rows
        }

        let content = TranscriptContent::from("alpha beta gamma".to_string());
        let first_row_range = {
            let mut range = None;
            content.visit_text_layout_rows(6, false, 0..1, |row| range = row.source_range());
            range
        };
        assert_eq!(content.text_layout_rows(6, true), 3);
        for chunk in [
            " delta",
            "epsilon",
            "\n",
            "\nwide界 text",
            "\tindented",
            "\nlast",
        ] {
            content.append_owned(chunk.to_string());
            let rebuilt = TranscriptContent::from(content.snapshot());
            assert_eq!(rows(&content, 6, false), rows(&rebuilt, 6, false));
            assert_eq!(rows(&content, 6, true), rows(&rebuilt, 6, true));
        }
        let retained_first_row_range = {
            let mut range = None;
            content.visit_text_layout_rows(6, false, 0..1, |row| range = row.source_range());
            range
        };
        assert_eq!(retained_first_row_range, first_row_range);
    }

    #[test]
    fn file_layout_extends_unfinished_lines_without_rebuilding_prior_rows() {
        fn rows(content: &TranscriptContent, width: u16) -> Vec<Vec<String>> {
            let ranges = content.file_layout_ranges(width, 0..usize::MAX);
            ranges
                .into_iter()
                .map(|range| {
                    let mut rows = Vec::new();
                    content.visit_file_layout_line_rows(
                        width,
                        range.line,
                        0..range.row_count,
                        |row| rows.push(row.text().into_owned()),
                    );
                    rows
                })
                .collect()
        }

        let content = TranscriptContent::from("ab".to_string());
        assert_eq!(content.file_layout_rows(3), 1);
        for chunk in ["cdef", "\n", "\n界x", "\tz\r", "\nlast"] {
            content.append_owned(chunk.to_string());
            let rebuilt = TranscriptContent::from(content.snapshot());
            assert_eq!(rows(&content, 3), rows(&rebuilt, 3));
            assert_eq!(content.file_layout_rows(3), rebuilt.file_layout_rows(3));
        }
        assert_eq!(rows(&content, 3)[0], ["abc", "def"]);
        assert!(rows(&content, 3)
            .iter()
            .any(|line| line.as_slice() == [String::new()]));
    }

    #[test]
    fn markdown_completed_ranges_leave_only_mutable_suffix() {
        let content = TranscriptContent::from("first\n\n```rust\n\ncode\n```\n\nlast".to_string());
        let completed = content.markdown_completed_ranges_after(0);
        assert_eq!(completed, [0..7, 7..26]);
        assert_eq!(content.markdown_suffix_range(), 26..30);

        content.append_owned(" paragraph\n\nnext".to_string());
        let completed = content.markdown_completed_ranges_after(26);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0], 26..42);
        assert_eq!(content.markdown_suffix_range(), 42..46);
    }

    #[test]
    fn markdown_ranges_defer_repeated_blanks_until_the_next_block() {
        let content = TranscriptContent::from("first\n\n\nnext\n\nlast".to_string());
        let completed = content.markdown_completed_ranges_after(0);
        let read = content.read();

        assert_eq!(completed.len(), 2);
        assert_eq!(read.slice(completed[0].clone()), "first\n\n\n");
        assert_eq!(read.slice(completed[1].clone()), "next\n\n");
        assert_eq!(read.slice(read.markdown_suffix_range()), "last");
    }

    #[test]
    fn markdown_ranges_keep_loose_list_continuations_together() {
        let content =
            TranscriptContent::from("- first\n\n  continuation\n\n- second\n\noutside".to_string());
        let completed = content.markdown_completed_ranges_after(0);
        let read = content.read();

        assert_eq!(completed.len(), 1);
        assert_eq!(
            read.slice(completed[0].clone()),
            "- first\n\n  continuation\n\n- second\n\n"
        );
        assert_eq!(read.slice(read.markdown_suffix_range()), "outside");
    }

    #[test]
    fn markdown_ranges_keep_reference_definitions_with_their_uses() {
        for source in [
            "[reference][id]\n\n[id]: https://example.com",
            "[id]: https://example.com\n\n[reference][id]",
        ] {
            let content = TranscriptContent::from(source.to_string());
            assert!(content.markdown_completed_ranges_after(0).is_empty());
            assert_eq!(
                content.read().slice(content.markdown_suffix_range()),
                source
            );
        }
    }

    #[test]
    fn markdown_range_index_tracks_pathological_blocks_without_copying_lines() {
        let content = TranscriptContent::from(format!("{}\n\nnext", "word ".repeat(14_000)));
        let read = content.read();

        let completed = read.markdown_completed_ranges_after(0);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0], 0..70_002);
        assert_eq!(read.markdown_suffix_range(), 70_002..70_006);
        assert!(read.markdown_has_range_larger_than(MAX_WORD_WRAP_LINE_BYTES));
        assert!(read.large_lines_have_ascii_cells());
    }

    #[test]
    fn retained_markdown_heading_tail_matches_contiguous_parser() {
        for source in [
            "",
            "Paragraph\n\n# Tail\n",
            "Paragraph\n---\n",
            "Paragraph\n\n---\n",
            "# Not tail\n\nParagraph",
            "> # Quoted heading\n",
            "```markdown\n# Not heading\n```",
            "```markdown\n# Not heading",
            "Paragraph\n\n## Tail without newline",
            "Paragraph\n\n## Tail\n\n",
        ] {
            let content = TranscriptContent::from(source.to_string());
            assert_eq!(
                content.ends_with_markdown_heading(),
                crate::content::markdown_ir::ends_with_heading(source),
                "heading tail differs for {source:?}"
            );
        }

        let content = TranscriptContent::from("Paragraph\n".to_string());
        assert!(!content.ends_with_markdown_heading());
        content.append_owned("---".to_string());
        assert!(content.ends_with_markdown_heading());
        content.append_owned("\n\ntext".to_string());
        assert!(!content.ends_with_markdown_heading());
        content.append_owned("\n# tail".to_string());
        assert!(content.ends_with_markdown_heading());
    }

    #[test]
    fn retained_markdown_heading_tail_bounds_pathological_lines() {
        let long_paragraph = "x".repeat(1024 * 1024);
        let content = TranscriptContent::from(format!("{long_paragraph}\n---"));
        assert!(content.ends_with_markdown_heading());

        let oversized_underline = TranscriptContent::from(format!(
            "paragraph\n{}",
            "-".repeat(MAX_MARKDOWN_FENCE_SCAN_BYTES + 1)
        ));
        assert!(!oversized_underline.ends_with_markdown_heading());
    }

    #[test]
    fn text_layout_indexes_plain_and_ansi_content_separately() {
        let content = TranscriptContent::from("\u{1b}[31mred\u{1b}[0m".to_string());
        assert_eq!(content.text_layout_rows(4, true), 1);
        assert!(content.text_layout_rows(4, false) > 1);
    }

    #[test]
    fn text_layout_retains_bounded_plain_rows_across_appends() {
        let content = TranscriptContent::from("alpha beta".to_string());
        let mut rows = Vec::new();
        assert_eq!(
            content.visit_text_layout_rows(6, false, 1..2, |row| {
                rows.push((
                    row.text().into_owned(),
                    row.source_range(),
                    row.wrapped(),
                    row.continuation(),
                    row.spans().to_vec(),
                ));
            }),
            1
        );
        assert_eq!(rows[0].0, "beta");
        assert_eq!(rows[0].1, Some(6..10));
        assert!(rows[0].2);
        assert!(rows[0].3);
        assert!(rows[0].4.is_empty());

        content.append_owned(" gamma".to_string());
        rows.clear();
        content.visit_text_layout_rows(6, false, 0..usize::MAX, |row| {
            rows.push((
                row.text().into_owned(),
                row.source_range(),
                row.wrapped(),
                row.continuation(),
                row.spans().to_vec(),
            ));
        });
        assert_eq!(
            rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
            ["alpha ", "beta ", "gamma"]
        );
        assert!(rows.iter().all(|row| row.1.is_some()));
    }

    #[test]
    fn ascii_word_tail_window_materializes_only_requested_deep_rows() {
        let content = TranscriptContent::from("x".repeat(1_048_576));
        let total_rows = content.text_layout_rows(64, false);
        let mut rows = Vec::new();
        let window = content.visit_text_layout_tail_rows(64, false, 2, |row| {
            rows.push((row.text().into_owned(), row.source_range()));
        });

        assert_eq!(total_rows, 16_384);
        assert_eq!(window.row_count, 2);
        assert!(window.truncated);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0.len(), 64);
        assert_eq!(rows[0].1, Some(1_048_448..1_048_512));
        assert_eq!(rows[1].1, Some(1_048_512..1_048_576));
    }

    #[test]
    fn plain_ansi_rows_stream_source_fragments_across_chunks() {
        let content = TranscriptContent::from("alpha ".to_string());
        content.append_owned("βeta ".to_string());
        content.append_owned("gamma".to_string());

        let mut rows = Vec::new();
        content.visit_text_layout_rows(20, true, 0..usize::MAX, |row| {
            let mut streamed = String::new();
            row.visit_text(|fragment| streamed.push_str(fragment));
            rows.push((streamed, row.text().into_owned(), row.source_range()));
        });

        assert_eq!(
            rows,
            [(
                "alpha βeta gamma".into(),
                "alpha βeta gamma".into(),
                Some(0..17)
            )]
        );
    }

    #[test]
    fn text_layout_retains_ansi_text_and_styles() {
        let content = TranscriptContent::from("\u{1b}[31mred text\u{1b}[0m".to_string());
        let mut rows = Vec::new();
        content.visit_text_layout_rows(4, true, 0..usize::MAX, |row| {
            rows.push((
                row.text().into_owned(),
                row.source_range(),
                row.wrapped(),
                row.spans().to_vec(),
            ));
        });

        assert_eq!(
            rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
            ["red ", "text"]
        );
        assert!(rows.iter().all(|row| row.1.is_none()));
        assert!(rows.iter().all(|row| row.2));
        assert!(rows.iter().all(|row| !row.3.is_empty()));
        assert!(rows
            .iter()
            .flat_map(|row| &row.3)
            .all(|span| span.style.fg.is_some()));
    }

    #[test]
    fn text_layout_preserves_ansi_state_across_chunked_lines() {
        let content = TranscriptContent::from("\u{1b}[31mred\n".to_string());
        content.append_owned("still red\n\u{1b}[0mplain".to_string());
        let mut rows = Vec::new();
        content.visit_text_layout_rows(80, true, 0..usize::MAX, |row| {
            rows.push((row.text().into_owned(), row.spans().to_vec()));
        });

        assert_eq!(
            rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
            ["red", "still red", "plain"]
        );
        assert!(rows[0].1.iter().all(|span| span.style.fg.is_some()));
        assert!(rows[1].1.iter().all(|span| span.style.fg.is_some()));
        assert!(rows[2].1.iter().all(|span| span.style.fg.is_none()));
    }

    #[test]
    fn ansi_checkpoints_retain_only_state_transitions() {
        let reset_lines = "\u{1b}[32mPASS\u{1b}[0m\n".repeat(1_000);
        let content = TranscriptContent::from(reset_lines);
        assert_eq!(content.read().entry.ansi_checkpoints.len(), 1);

        content.append_owned("\u{1b}[31mred\nstill red\n\u{1b}[0mplain\n".to_string());
        let read = content.read();
        let first_carried_line = 1_001;
        assert_eq!(read.entry.ansi_checkpoints.len(), 3);
        assert_ne!(
            read.entry.ansi_state_for_line(first_carried_line),
            crate::content::ansi::AnsiState::default()
        );
        assert_eq!(
            read.entry.ansi_state_for_line(first_carried_line + 2),
            crate::content::ansi::AnsiState::default()
        );
    }

    #[test]
    fn serde_uses_plain_string_payload() {
        let content = TranscriptContent::from("alpha\nβeta".to_string());
        let encoded = serde_json::to_string(&content).unwrap();
        assert_eq!(encoded, "\"alpha\\nβeta\"");
        let decoded: TranscriptContent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, content);
        assert_ne!(decoded.id(), content.id());
    }
}
