//! Transcript block history, streaming state, projection, and cursor glyph cache.

use crate::app::transcript_scroll_trace::{
    TranscriptInteractionTraceEvent, TranscriptProjectionTargetTrace, TranscriptRecordTraceRange,
    TranscriptScrollIntent, TranscriptScrollTrace, TranscriptScrollTraceFrame,
    TranscriptScrollTraceFrameStart, TranscriptScrollTraceRenderInput, TranscriptTraceAnchor,
    TranscriptVisibleContentAnchor,
};
use crate::app::TuiApp;
use crate::content::prompt_parser::{
    build_prompt_display_lines, prompt_display_uses_cursor_padding,
};
use crate::content::transcript_buf::TranscriptRowAnchor;
use crate::smelt_edit::{
    add_signed_row, Buffer, DisplayDocument, DisplayRow, DisplayRows, DisplaySnapshot, DocPosition,
    DocRange, DocumentCommand, RowIndex, TextRange, Theme, VerticalScroll,
};
use smelt_buffer::wrap_layout::WrappedLayout;
use smelt_core::content::file_icons::FileIconOptions;
use smelt_core::content::highlight::InlineOptions;

use smelt_core::content::transcript::Transcript;
use smelt_core::lua::runtime::LuaRuntime;
use smelt_core::transcript_model::{
    Block, BlockHistory, BlockId, LayoutKey, Status, StoredBlockWithId, ToolOutputRef, ToolStatus,
    TranscriptBlockRecordWithId,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DISPLAY_ONLY_TRANSCRIPT_OVERSCAN_VIEWPORTS: u16 = 3;
const DISPLAY_ONLY_TRANSCRIPT_MIN_TARGET_ROWS: u16 = 80;
const TRANSCRIPT_ACTIVE_RECORD_WINDOW_MAX_MULTIPLIER: usize = 3;
const TRANSCRIPT_RECORD_WINDOW_MIN_RECORDS: usize = 512;
const TRANSCRIPT_RECORD_PAGE_SIZE: usize = 128;
const TRANSCRIPT_RECORD_CACHE_GUARD_PAGES: usize = 2;
const TRANSCRIPT_RECORD_PREFIX_STRIDE: usize = smelt_store::TRANSCRIPT_EXTENT_CHUNK_RECORDS;
const TRANSCRIPT_LOCAL_PAGE_GUARD_VIEWPORTS: RowIndex = 8;
const TRANSCRIPT_IDLE_COMPACTION_BLOCKS: usize = 64;
const TRANSCRIPT_IDLE_COMPACTION_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_HYDRATED_BLOCK_BUDGET: usize = 32 * 1024 * 1024;
const DEFAULT_RECORD_WINDOW_BUDGET: usize = 16 * 1024 * 1024;
const DEFAULT_RENDERED_PAYLOAD_BUDGET: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptMemoryBudget {
    pub(crate) hydrated_blocks: usize,
    pub(crate) record_windows: usize,
    pub(crate) rendered_rows: usize,
}

impl Default for TranscriptMemoryBudget {
    fn default() -> Self {
        Self {
            hydrated_blocks: DEFAULT_HYDRATED_BLOCK_BUDGET,
            record_windows: DEFAULT_RECORD_WINDOW_BUDGET,
            rendered_rows: DEFAULT_RENDERED_PAYLOAD_BUDGET,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TranscriptMemorySnapshot {
    pub(crate) live_blocks: usize,
    pub(crate) stored_blocks: usize,
    pub(crate) hydrated_blocks: usize,
    pub(crate) hydrated_budget_bytes: usize,
    pub(crate) record_budget_bytes: usize,
    pub(crate) rendered_budget_bytes: usize,
    pub(crate) live_block_bytes: usize,
    pub(crate) live_tool_state_bytes: usize,
    pub(crate) hydrated_block_bytes: usize,
    pub(crate) hydrated_tool_state_bytes: usize,
    pub(crate) compact_record_bytes: usize,
    pub(crate) record_window_bytes: usize,
    pub(crate) tool_state_metadata_bytes: usize,
    pub(crate) block_metadata_bytes: usize,
    pub(crate) layout_bytes: usize,
    pub(crate) source_view_bytes: usize,
    pub(crate) height_index_bytes: usize,
    pub(crate) height_index_cache_bytes: usize,
    pub(crate) visible_rows_bytes: usize,
    pub(crate) full_rows_bytes: usize,
    pub(crate) pinned_hydrated_bytes: usize,
    pub(crate) pinned_rendered_bytes: usize,
    pub(crate) hydrated_oversize_debt_bytes: usize,
    pub(crate) record_oversize_debt_bytes: usize,
    pub(crate) rendered_oversize_debt_bytes: usize,
    pub(crate) hydration_reads: u64,
    pub(crate) hydration_ranges: u64,
    pub(crate) hydration_bytes: u64,
    pub(crate) hydration_duration_us: u64,
    pub(crate) evicted_entries: u64,
    pub(crate) evicted_bytes: u64,
    pub(crate) dematerialized_entries: u64,
    pub(crate) dematerialized_bytes: u64,
}

#[derive(Default)]
struct TranscriptHydrationState {
    lru: VecDeque<BlockId>,
    lru_ids: HashSet<BlockId>,
    viewport_pins: HashSet<BlockId>,
    operation_pins: HashMap<BlockId, usize>,
    hydration_reads: u64,
    hydration_ranges: u64,
    hydration_bytes: u64,
    hydration_duration_us: u64,
    evicted_entries: u64,
    evicted_bytes: u64,
    dematerialized_entries: u64,
    dematerialized_bytes: u64,
}

impl TranscriptHydrationState {
    fn is_pinned(&self, id: BlockId) -> bool {
        self.viewport_pins.contains(&id) || self.operation_pins.contains_key(&id)
    }

    fn touch_many(&mut self, ids: &[BlockId]) {
        let mut moved = ids.iter().copied().collect::<HashSet<_>>();
        if moved.is_empty() {
            return;
        }
        self.lru.retain(|candidate| !moved.contains(candidate));
        for id in ids {
            if moved.remove(id) {
                self.lru_ids.insert(*id);
                self.lru.push_back(*id);
            }
        }
    }

    fn pin_operation(&mut self, ids: &[BlockId]) {
        for id in ids {
            *self.operation_pins.entry(*id).or_default() += 1;
        }
    }

    fn unpin_operation(&mut self, ids: &[BlockId]) {
        for id in ids {
            let Some(count) = self.operation_pins.get_mut(id) else {
                continue;
            };
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.operation_pins.remove(id);
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingTranscriptCompaction {
    record_len: usize,
    next_order_index: usize,
    next_record_index: usize,
}

pub(crate) struct LoadedRecordWindow {
    pub(crate) start: smelt_store::TranscriptRecordOffset,
    pub(crate) total_count: usize,
    pub(crate) hydration: smelt_store::TranscriptRecordHydration,
    pub(crate) records: Vec<StoredBlockWithId>,
}

impl LoadedRecordWindow {
    pub(crate) fn end(&self) -> smelt_store::TranscriptRecordOffset {
        smelt_store::TranscriptRecordOffset::new(
            self.start.get().saturating_add(self.records.len()),
        )
    }

    fn from_slice(slice: smelt_store::TranscriptRecordSlice) -> Option<Self> {
        if slice.is_empty() {
            return None;
        }
        let start = slice.start;
        Some(Self {
            start,
            total_count: slice.total_count,
            hydration: slice.hydration,
            records: compact_record_rows(start, slice.into_records())?,
        })
    }
}

pub(crate) struct LoadedTranscript {
    pub(crate) transcript: Transcript,
    pub(crate) record_window: Option<LoadedRecordWindow>,
    pub(crate) session_dir: Option<PathBuf>,
}

impl LoadedTranscript {
    pub(crate) fn full(transcript: Transcript) -> Self {
        Self {
            transcript,
            record_window: None,
            session_dir: None,
        }
    }

    pub(crate) fn tail_from_sqlite_dir(
        session_dir: PathBuf,
        width: u16,
        viewport_rows: u16,
    ) -> Option<Self> {
        let store = {
            let _perf = smelt_perf::perf::begin("transcript:resume_tail:open_store");
            SqliteTranscriptStore::open_read_only(&session_dir).ok()?
        };
        let target_rows = record_tail_target_rows(viewport_rows);
        let slice = {
            let _perf = smelt_perf::perf::begin("transcript:resume_tail:read_tail_slice");
            store
                .read_tail_record_slice_for_rows(width, target_rows)
                .ok()?
        };
        smelt_perf::perf::record_value("transcript:sqlite:record_total", slice.total_count as u64);
        smelt_perf::perf::record_value("transcript:sqlite:record_loaded", slice.len() as u64);
        let _perf = smelt_perf::perf::begin("transcript:resume_tail:build_loaded");
        Self::from_record_slice(slice, session_dir)
    }

    pub(crate) fn from_record_slice(
        slice: smelt_store::TranscriptRecordSlice,
        session_dir: PathBuf,
    ) -> Option<Self> {
        let record_window = LoadedRecordWindow::from_slice(slice)?;
        Some(Self {
            transcript: Transcript::new(),
            record_window: Some(record_window),
            session_dir: Some(session_dir),
        })
    }
}

fn compact_record_rows(
    start: smelt_store::TranscriptRecordOffset,
    rows: Vec<smelt_store::StoredTranscriptBlock>,
) -> Option<Vec<StoredBlockWithId>> {
    if rows.is_empty() {
        return None;
    }
    let _perf = smelt_perf::perf::begin("transcript:record_window:compact_records");
    smelt_perf::perf::record_value("transcript:record_window:decode_rows", rows.len() as u64);
    smelt_core::transcript_model::compact_block_rows(start.get(), rows).ok()
}

struct SqliteTranscriptStore {
    reader: smelt_store::LineageSessionReader,
    #[cfg(test)]
    extent_read_count: std::cell::Cell<usize>,
}

impl SqliteTranscriptStore {
    fn open_read_only(session_dir: impl AsRef<std::path::Path>) -> smelt_store::Result<Self> {
        let session_dir = session_dir.as_ref();
        let (root, session_id) = session_dir
            .parent()
            .zip(session_dir.file_name().and_then(|name| name.to_str()))
            .ok_or_else(|| {
                smelt_store::StoreError::Integrity("invalid lineage session directory".into())
            })?;
        let reader = smelt_store::LineageSessionReader::open_existing(root, session_id)?;
        Ok(Self {
            reader,
            #[cfg(test)]
            extent_read_count: std::cell::Cell::new(0),
        })
    }

    fn read_tail_record_slice_for_rows(
        &self,
        width: u16,
        target_rows: u16,
    ) -> smelt_store::Result<smelt_store::TranscriptRecordSlice> {
        let total = usize::try_from(self.reader.snapshot()?.transcript_len).map_err(|_| {
            smelt_store::StoreError::Integrity(
                "lineage transcript length exceeds platform limits".into(),
            )
        })?;
        self.reader
            .transcript_tail_for_rows_with_total(total, width, target_rows)
    }

    fn read_record_slice(
        &self,
        range: smelt_store::TranscriptRecordRange,
        total_count: usize,
    ) -> smelt_store::Result<smelt_store::TranscriptRecordSlice> {
        self.reader
            .transcript_record_slice_with_total(range, total_count)
    }

    fn record_index_for_block_idx(&self, block_idx: u64) -> smelt_store::Result<Option<usize>> {
        const CHUNK_RECORDS: u64 = 256;

        let transcript_len = self.reader.snapshot()?.transcript_len;
        let mut start = 0;
        while start < transcript_len {
            let end = start.saturating_add(CHUNK_RECORDS).min(transcript_len);
            if let Some(offset) = self
                .reader
                .transcript_object_backed_range(start, end)?
                .iter()
                .position(|record| record.block_idx == block_idx)
            {
                let index = usize::try_from(start).map_err(|_| {
                    smelt_store::StoreError::Integrity(
                        "lineage transcript offset exceeds platform limits".into(),
                    )
                })?;
                return Ok(Some(index.saturating_add(offset)));
            }
            start = end;
        }
        Ok(None)
    }

    fn read_hydration_records(
        &self,
        range: smelt_store::TranscriptRecordRange,
        total_count: usize,
    ) -> Option<Vec<TranscriptBlockRecordWithId>> {
        let slice = self.read_record_slice(range, total_count).ok()?;
        slice
            .into_records()
            .into_iter()
            .map(TranscriptBlockRecordWithId::try_from)
            .collect::<Result<Vec<_>, _>>()
            .ok()
    }

    fn estimated_record_rows(&self, width: u16, range: Range<usize>) -> smelt_store::Result<u64> {
        #[cfg(test)]
        self.extent_read_count
            .set(self.extent_read_count.get().saturating_add(1));
        self.reader
            .transcript_estimated_rows(range.into(), width.max(1))
    }

    fn record_before_kind(
        &self,
        kind: &str,
        before_or_at: usize,
    ) -> smelt_store::Result<Option<(usize, smelt_store::StoredTranscriptBlock)>> {
        const CHUNK: usize = 256;
        let total = usize::try_from(self.reader.snapshot()?.transcript_len).map_err(|_| {
            smelt_store::StoreError::Integrity(
                "lineage transcript length exceeds platform limits".into(),
            )
        })?;
        let mut end = before_or_at.saturating_add(1).min(total);
        while end > 0 {
            let start = end.saturating_sub(CHUNK);
            if let Some((offset, record)) = self
                .reader
                .transcript_object_backed_range(start as u64, end as u64)?
                .into_iter()
                .enumerate()
                .rev()
                .find(|(_, record)| record.kind == kind)
            {
                return Ok(Some((start.saturating_add(offset), record)));
            }
            end = start;
        }
        Ok(None)
    }

    fn record_after_kind(
        &self,
        kind: &str,
        after_or_at: usize,
    ) -> smelt_store::Result<Option<(usize, smelt_store::StoredTranscriptBlock)>> {
        const CHUNK: usize = 256;
        let total = usize::try_from(self.reader.snapshot()?.transcript_len).map_err(|_| {
            smelt_store::StoreError::Integrity(
                "lineage transcript length exceeds platform limits".into(),
            )
        })?;
        let mut start = after_or_at.min(total);
        while start < total {
            let end = start.saturating_add(CHUNK).min(total);
            if let Some((offset, record)) = self
                .reader
                .transcript_object_backed_range(start as u64, end as u64)?
                .into_iter()
                .enumerate()
                .find(|(_, record)| record.kind == kind)
            {
                return Ok(Some((start.saturating_add(offset), record)));
            }
            start = end;
        }
        Ok(None)
    }

    fn extent_chunks(&self) -> smelt_store::Result<Vec<smelt_store::TranscriptExtentChunk>> {
        #[cfg(test)]
        self.extent_read_count
            .set(self.extent_read_count.get().saturating_add(1));
        self.reader.transcript_extent_chunks()
    }

    #[cfg(test)]
    fn extent_read_count(&self) -> usize {
        self.extent_read_count.get()
    }
}

#[derive(Default)]
struct TranscriptStoreCache {
    store: Option<(PathBuf, SqliteTranscriptStore)>,
    #[cfg(test)]
    open_attempt_count: usize,
}

impl TranscriptStoreCache {
    fn cached_store_for_session(&self, session_dir: &PathBuf) -> Option<&SqliteTranscriptStore> {
        self.store
            .as_ref()
            .filter(|(open_dir, _)| open_dir == session_dir)
            .map(|(_, store)| store)
    }

    fn store_for_session(
        &mut self,
        session_dir: Option<&PathBuf>,
    ) -> Option<&SqliteTranscriptStore> {
        let session_dir = session_dir?.clone();
        let needs_open = self
            .store
            .as_ref()
            .is_none_or(|(open_dir, _)| open_dir != &session_dir);
        if needs_open {
            #[cfg(test)]
            {
                self.open_attempt_count = self.open_attempt_count.saturating_add(1);
            }
            let store = SqliteTranscriptStore::open_read_only(&session_dir).ok()?;
            self.store = Some((session_dir, store));
        }
        self.store.as_ref().map(|(_, store)| store)
    }

    fn read_record_slice(
        &mut self,
        session_dir: Option<&PathBuf>,
        range: smelt_store::TranscriptRecordRange,
        total_count: usize,
    ) -> Option<smelt_store::TranscriptRecordSlice> {
        self.store_for_session(session_dir)?
            .read_record_slice(range, total_count)
            .ok()
    }

    fn read_hydration_records(
        &mut self,
        session_dir: Option<&PathBuf>,
        range: smelt_store::TranscriptRecordRange,
        total_count: usize,
    ) -> Option<Vec<TranscriptBlockRecordWithId>> {
        self.store_for_session(session_dir)?
            .read_hydration_records(range, total_count)
    }
}

pub(crate) fn record_tail_target_rows(viewport_rows: u16) -> u16 {
    viewport_rows
        .max(1)
        .saturating_mul(DISPLAY_ONLY_TRANSCRIPT_OVERSCAN_VIEWPORTS.saturating_add(1))
        .max(DISPLAY_ONLY_TRANSCRIPT_MIN_TARGET_ROWS)
}

fn record_window_payload_bytes(records: &[StoredBlockWithId]) -> u64 {
    records
        .iter()
        .map(|record| record.stored.retained_bytes() as u64)
        .sum()
}

fn record_record_window_metrics(window: &LoadedRecordWindow) {
    smelt_perf::perf::record_value("transcript:record_window:start", window.start.get() as u64);
    smelt_perf::perf::record_value("transcript:record_window:end", window.end().get() as u64);
    smelt_perf::perf::record_value("transcript:record_window:total", window.total_count as u64);
    smelt_perf::perf::record_value(
        "transcript:record_window:loaded",
        window.records.len() as u64,
    );
    smelt_perf::perf::record_value(
        "transcript:record_window:payload_bytes",
        record_window_payload_bytes(&window.records),
    );
    smelt_perf::perf::record_value(
        "transcript:record_window:object_backed",
        u64::from(matches!(
            window.hydration,
            smelt_store::TranscriptRecordHydration::ObjectBacked
        )),
    );
}

#[derive(Default)]
struct SparseTranscriptRecords {
    total_count: Option<usize>,
    loaded_ranges: Vec<Range<smelt_store::TranscriptRecordOffset>>,
    records: BTreeMap<smelt_store::TranscriptRecordOffset, StoredBlockWithId>,
    lru: VecDeque<smelt_store::TranscriptRecordOffset>,
}

impl SparseTranscriptRecords {
    fn from_loaded(loaded: Option<&LoadedRecordWindow>) -> Self {
        let mut records = Self::default();
        if let Some(loaded) = loaded {
            records.merge(loaded);
        }
        records
    }

    fn merge(&mut self, loaded: &LoadedRecordWindow) -> bool {
        let start = loaded.start;
        let end = loaded.end();
        if start >= end {
            self.total_count = Some(loaded.total_count);
            return false;
        }
        self.total_count = Some(loaded.total_count);
        self.records
            .retain(|index, _| *index < start || *index >= end);
        self.lru.retain(|index| *index < start || *index >= end);
        for (offset, record) in loaded.records.iter().cloned().enumerate() {
            let index =
                smelt_store::TranscriptRecordOffset::new(start.get().saturating_add(offset));
            self.records.insert(index, record);
            self.lru.push_back(index);
        }
        self.add_loaded_range(start..end);
        true
    }

    fn truncate(&mut self, total_count: usize) -> usize {
        let total_count = self
            .total_count
            .map_or(total_count, |current| current.min(total_count));
        let end = smelt_store::TranscriptRecordOffset::new(total_count);
        self.total_count = Some(total_count);
        self.records.retain(|index, _| *index < end);
        self.lru.retain(|index| *index < end);
        for range in &mut self.loaded_ranges {
            range.end = range.end.min(end);
        }
        self.loaded_ranges.retain(|range| range.start < range.end);
        total_count
    }

    fn invalidate_from(&mut self, start: smelt_store::TranscriptRecordOffset) {
        self.records.retain(|index, _| *index < start);
        self.lru.retain(|index| *index < start);
        self.loaded_ranges = self
            .loaded_ranges
            .drain(..)
            .filter_map(|range| {
                if range.start >= start {
                    None
                } else {
                    Some(range.start..range.end.min(start))
                }
            })
            .collect();
    }

    fn add_loaded_range(&mut self, range: Range<smelt_store::TranscriptRecordOffset>) {
        if range.start >= range.end {
            return;
        }
        self.loaded_ranges.push(range);
        self.loaded_ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<smelt_store::TranscriptRecordOffset>> =
            Vec::with_capacity(self.loaded_ranges.len());
        for range in self.loaded_ranges.drain(..) {
            if let Some(last) = merged.last_mut() {
                if range.start <= last.end {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            merged.push(range);
        }
        self.loaded_ranges = merged;
    }

    fn records_for_range(
        &self,
        range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
    ) -> Vec<StoredBlockWithId> {
        match range {
            Some(range) => self
                .records
                .range(range.clone())
                .map(|(_, record)| record.clone())
                .collect(),
            None => self.records.values().cloned().collect(),
        }
    }

    fn total_count(&self) -> Option<usize> {
        self.total_count
    }

    fn loaded_record_count(&self) -> usize {
        self.records.len()
    }

    fn missing_prefix_count_for_range(
        &self,
        range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
    ) -> usize {
        range
            .map(|range| range.start.get())
            .unwrap_or_default()
            .min(self.total_count.unwrap_or_default())
    }

    fn missing_suffix_count_for_range(
        &self,
        range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
    ) -> usize {
        self.total_count
            .map(|total| {
                total.saturating_sub(
                    range
                        .map(|range| range.end.get())
                        .unwrap_or_else(|| self.records.len()),
                )
            })
            .unwrap_or_default()
    }

    fn range_is_loaded(&self, range: &Range<smelt_store::TranscriptRecordOffset>) -> bool {
        range.start >= range.end
            || self
                .loaded_ranges
                .iter()
                .any(|loaded| loaded.start <= range.start && loaded.end >= range.end)
    }

    fn missing_ranges(
        &self,
        range: &Range<smelt_store::TranscriptRecordOffset>,
    ) -> Vec<Range<smelt_store::TranscriptRecordOffset>> {
        if range.start >= range.end {
            return Vec::new();
        }
        let mut missing = Vec::new();
        let mut cursor = range.start;
        for loaded in &self.loaded_ranges {
            if loaded.end <= cursor {
                continue;
            }
            if loaded.start >= range.end {
                break;
            }
            if loaded.start > cursor {
                missing.push(cursor..loaded.start.min(range.end));
            }
            cursor = cursor.max(loaded.end);
            if cursor >= range.end {
                break;
            }
        }
        if cursor < range.end {
            missing.push(cursor..range.end);
        }
        missing
    }

    fn cache_range_around(
        &self,
        range: &Range<smelt_store::TranscriptRecordOffset>,
    ) -> Range<smelt_store::TranscriptRecordOffset> {
        let Some(total) = self.total_count else {
            return range.clone();
        };
        let guard = TRANSCRIPT_RECORD_PAGE_SIZE.saturating_mul(TRANSCRIPT_RECORD_CACHE_GUARD_PAGES);
        let mut start = range.start.get().saturating_sub(guard);
        let mut end = range.end.get().saturating_add(guard).min(total);
        if end.saturating_sub(start) >= TRANSCRIPT_RECORD_PAGE_SIZE {
            start = start / TRANSCRIPT_RECORD_PAGE_SIZE * TRANSCRIPT_RECORD_PAGE_SIZE;
            end = end
                .saturating_add(TRANSCRIPT_RECORD_PAGE_SIZE - 1)
                .saturating_div(TRANSCRIPT_RECORD_PAGE_SIZE)
                .saturating_mul(TRANSCRIPT_RECORD_PAGE_SIZE)
                .min(total);
        }
        smelt_store::TranscriptRecordOffset::new(start)
            ..smelt_store::TranscriptRecordOffset::new(end)
    }

    fn touch_range(&mut self, range: &Range<smelt_store::TranscriptRecordOffset>) {
        self.lru
            .retain(|index| !(range.start <= *index && *index < range.end));
        self.lru
            .extend(self.records.range(range.clone()).map(|(index, _)| *index));
    }

    fn retained_bytes(&self) -> usize {
        self.records
            .values()
            .map(|record| record.stored.retained_bytes())
            .sum()
    }

    fn enforce_byte_budget(
        &mut self,
        pinned: &Range<smelt_store::TranscriptRecordOffset>,
        budget: usize,
    ) {
        let mut retained = self.retained_bytes();
        let mut attempts = self.lru.len();
        while retained > budget && attempts > 0 {
            attempts -= 1;
            let Some(candidate) = self.lru.pop_front() else {
                break;
            };
            if pinned.start <= candidate && candidate < pinned.end {
                self.lru.push_back(candidate);
                continue;
            }
            if let Some(record) = self.records.remove(&candidate) {
                retained = retained.saturating_sub(record.stored.retained_bytes());
                attempts = self.lru.len();
            }
        }
        self.lru.retain(|index| self.records.contains_key(index));
        self.rebuild_loaded_ranges();
        smelt_perf::perf::record_value("transcript:record_cache:retained_bytes", retained as u64);
        smelt_perf::perf::record_value(
            "transcript:record_cache:oversize_debt_bytes",
            retained.saturating_sub(budget) as u64,
        );
    }

    fn rebuild_loaded_ranges(&mut self) {
        let mut ranges: Vec<Range<smelt_store::TranscriptRecordOffset>> = Vec::new();
        for index in self.records.keys().copied() {
            match ranges.last_mut() {
                Some(range) if range.end.get() == index.get() => {
                    range.end =
                        smelt_store::TranscriptRecordOffset::new(index.get().saturating_add(1));
                }
                _ => ranges.push(
                    index..smelt_store::TranscriptRecordOffset::new(index.get().saturating_add(1)),
                ),
            }
        }
        self.loaded_ranges = ranges;
    }

    fn record(&self, index: smelt_store::TranscriptRecordOffset) -> Option<&StoredBlockWithId> {
        self.records.get(&index)
    }

    fn record_index_for_block_id(
        &self,
        block_id: BlockId,
    ) -> Option<smelt_store::TranscriptRecordOffset> {
        self.records
            .iter()
            .find_map(|(index, record)| (record.block_id == block_id).then_some(*index))
    }

    fn navigation_record(
        &self,
        role: &str,
        anchor: smelt_store::TranscriptRecordOffset,
        direction: TranscriptNavigationDirection,
    ) -> Option<(usize, StoredBlockWithId)> {
        match direction {
            TranscriptNavigationDirection::Previous => {
                self.records
                    .range(..anchor)
                    .rev()
                    .find_map(|(index, record)| {
                        (record.stored.kind.as_str() == role).then(|| (index.get(), record.clone()))
                    })
            }
            TranscriptNavigationDirection::Next => {
                let after_anchor =
                    smelt_store::TranscriptRecordOffset::new(anchor.get().saturating_add(1));
                self.records
                    .range(after_anchor..)
                    .find_map(|(index, record)| {
                        (record.stored.kind.as_str() == role).then(|| (index.get(), record.clone()))
                    })
            }
        }
    }

    #[cfg(test)]
    fn loaded_ranges(&self) -> &[Range<smelt_store::TranscriptRecordOffset>] {
        &self.loaded_ranges
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordRangeState {
    Unavailable,
    Loaded,
    Missing,
}

struct TranscriptRecordExtentModel<'a> {
    records: &'a SparseTranscriptRecords,
    store: Option<&'a SqliteTranscriptStore>,
    width: u16,
    active_range: Option<Range<usize>>,
    total_count: Option<usize>,
    fallback_rows_per_record: RowIndex,
}

fn persisted_record_rows_to_transcript_rows(
    estimated_record_rows: RowIndex,
    record_count: usize,
) -> RowIndex {
    estimated_record_rows.saturating_sub(RowIndex::from(record_count > 0))
}

#[derive(Clone, Debug)]
struct TranscriptExtentPrefixIndex {
    width: u16,
    total_count: usize,
    stride: usize,
    prefix_rows: Vec<RowIndex>,
}

impl TranscriptExtentPrefixIndex {
    fn matches(&self, width: u16, total_count: usize) -> bool {
        self.width == width.max(1)
            && self.total_count == total_count
            && self.stride == TRANSCRIPT_RECORD_PREFIX_STRIDE
    }

    fn total_rows(&self) -> RowIndex {
        self.prefix_rows.last().copied().unwrap_or_default()
    }

    fn chunk_for_row(&self, row: RowIndex) -> Option<(usize, usize, RowIndex)> {
        if self.total_count == 0 || self.prefix_rows.len() < 2 {
            return None;
        }
        let row = row.min(self.total_rows().saturating_sub(1));
        let mut lo = 0usize;
        let mut hi = self.prefix_rows.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.prefix_rows[mid] <= row {
                lo = mid.saturating_add(1);
            } else {
                hi = mid;
            }
        }
        let chunk = lo
            .saturating_sub(1)
            .min(self.prefix_rows.len().saturating_sub(2));
        let start = chunk.saturating_mul(self.stride).min(self.total_count);
        let end = start.saturating_add(self.stride).min(self.total_count);
        Some((start, end, self.prefix_rows[chunk]))
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ExactRecordRowsKey {
    record_index: usize,
    width: u16,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
    node_key: LayoutKey,
}

#[derive(Default)]
struct TranscriptExtentIndex {
    prefix_index: Option<TranscriptExtentPrefixIndex>,
    exact_record_rows: HashMap<ExactRecordRowsKey, RowIndex>,
    latest_exact_record_rows: BTreeMap<(u16, usize), ExactRecordRowsKey>,
}

impl TranscriptExtentIndex {
    fn clear_exact_local_record_rows(&mut self) {
        self.exact_record_rows.clear();
        self.latest_exact_record_rows.clear();
    }

    fn clear_persisted_record_estimates(&mut self) {
        self.prefix_index = None;
    }

    fn exact_observation_count(&self) -> usize {
        self.exact_record_rows.len()
    }

    fn exact_local_rows_for_record(&self, record_index: usize, width: u16) -> Option<RowIndex> {
        let width = width.max(1);
        let key = self.latest_exact_record_rows.get(&(width, record_index))?;
        self.exact_record_rows.get(key).copied()
    }

    fn observe_exact_loaded_record_rows(
        &mut self,
        records: &SparseTranscriptRecords,
        snapshot: crate::content::transcript_buf::TranscriptExactHeightSnapshot,
    ) {
        let loaded_record_indices: HashSet<_> =
            records.records.keys().map(|index| index.get()).collect();
        self.latest_exact_record_rows
            .retain(|(_, record_index), _| loaded_record_indices.contains(record_index));
        self.exact_record_rows
            .retain(|key, _| loaded_record_indices.contains(&key.record_index));
        if snapshot.observations.is_empty() {
            return;
        }
        let record_by_block: HashMap<BlockId, usize> = records
            .records
            .iter()
            .map(|(index, record)| (record.block_id, index.get()))
            .collect();
        for observation in snapshot.observations {
            let Some(record_index) = record_by_block.get(&observation.block_id).copied() else {
                continue;
            };
            let key = ExactRecordRowsKey {
                record_index,
                width: snapshot.width.max(1),
                renderer_generation: snapshot.renderer_generation,
                renderer_cache_key: snapshot.renderer_cache_key,
                presentation_generation: snapshot.presentation_generation,
                node_key: observation.key,
            };
            let latest_key = (key.width, record_index);
            self.exact_record_rows.insert(key.clone(), observation.rows);
            if let Some(previous_key) = self
                .latest_exact_record_rows
                .insert(latest_key, key.clone())
            {
                if previous_key != key {
                    self.exact_record_rows.remove(&previous_key);
                }
            }
        }
    }

    fn local_rows_for_loaded_records(
        &self,
        records: &SparseTranscriptRecords,
        width: u16,
    ) -> RowIndex {
        records
            .records
            .iter()
            .map(|(index, record)| {
                self.exact_local_rows_for_record(index.get(), width)
                    .unwrap_or_else(|| {
                        record
                            .stored
                            .estimated_text_bytes
                            .max(1)
                            .div_ceil(u64::from(width.max(1)))
                            .saturating_add(1)
                    })
            })
            .sum()
    }

    fn loaded_record_count(&self, records: &SparseTranscriptRecords) -> usize {
        records.loaded_record_count()
    }

    fn fallback_average_rows_per_loaded_record(
        &self,
        records: &SparseTranscriptRecords,
        width: u16,
    ) -> RowIndex {
        let loaded = self.loaded_record_count(records) as RowIndex;
        if loaded == 0 {
            return 2;
        }
        self.local_rows_for_loaded_records(records, width)
            .saturating_add(loaded.saturating_sub(1))
            .saturating_div(loaded)
            .max(1)
    }

    fn approximate_rows_for_unloaded_record_range(
        &self,
        store: Option<&SqliteTranscriptStore>,
        width: u16,
        range: Range<usize>,
    ) -> Option<RowIndex> {
        if range.start >= range.end {
            return Some(0);
        }
        store?
            .estimated_record_rows(width.max(1), range)
            .ok()
            .map(|rows| rows as RowIndex)
    }

    fn record_extent_model<'a>(
        &self,
        records: &'a SparseTranscriptRecords,
        active_record_range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
        store: Option<&'a SqliteTranscriptStore>,
        width: u16,
    ) -> TranscriptRecordExtentModel<'a> {
        let width = width.max(1);
        TranscriptRecordExtentModel {
            records,
            store,
            width,
            active_range: active_record_range.map(|range| range.start.get()..range.end.get()),
            total_count: records.total_count(),
            fallback_rows_per_record: self.fallback_average_rows_per_loaded_record(records, width),
        }
    }

    fn estimated_rows_for_missing_record_range(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
        range: Range<usize>,
    ) -> RowIndex {
        let count = range.end.saturating_sub(range.start) as RowIndex;
        self.approximate_rows_for_unloaded_record_range(model.store, model.width, range)
            .unwrap_or_else(|| count.saturating_mul(model.fallback_rows_per_record))
    }

    fn estimated_rows_for_record_range(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
        range: Range<usize>,
    ) -> RowIndex {
        if range.start >= range.end {
            return 0;
        }
        self.estimated_rows_for_missing_record_range(model, range)
    }

    fn estimated_rows_before_record(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
        record_index: usize,
    ) -> RowIndex {
        let Some(total) = model.total_count else {
            return self.estimated_rows_for_record_range(model, 0..record_index);
        };
        let record_index = record_index.min(total);
        let stride = TRANSCRIPT_RECORD_PREFIX_STRIDE;
        let chunk = record_index / stride;
        let chunk_start = chunk.saturating_mul(stride);
        let base_rows = self
            .ensure_prefix_index(model)
            .and_then(|index| index.prefix_rows.get(chunk).copied())
            .unwrap_or_default();
        if chunk_start == record_index {
            return base_rows;
        }
        base_rows
            .saturating_add(self.estimated_rows_for_record_range(model, chunk_start..record_index))
    }

    fn cached_rows_before_record(
        &self,
        width: u16,
        total_count: usize,
        record_index: usize,
    ) -> Option<RowIndex> {
        let record_index = record_index.min(total_count);
        if !record_index.is_multiple_of(TRANSCRIPT_RECORD_PREFIX_STRIDE) {
            return None;
        }
        let index = self.prefix_index.as_ref()?;
        if !index.matches(width, total_count) {
            return None;
        }
        index
            .prefix_rows
            .get(record_index / TRANSCRIPT_RECORD_PREFIX_STRIDE)
            .copied()
    }

    fn cached_total_rows(&self, width: u16, total_count: usize) -> Option<RowIndex> {
        let index = self.prefix_index.as_ref()?;
        if !index.matches(width, total_count) {
            return None;
        }
        Some(persisted_record_rows_to_transcript_rows(
            index.total_rows(),
            total_count,
        ))
    }

    fn estimated_total_record_rows(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
    ) -> Option<RowIndex> {
        let total = model.total_count?;
        let rows = self.ensure_prefix_index(model)?.total_rows();
        Some(persisted_record_rows_to_transcript_rows(rows, total))
    }

    fn ensure_prefix_index(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
    ) -> Option<&TranscriptExtentPrefixIndex> {
        let total = model.total_count?;
        let width = model.width.max(1);
        if self
            .prefix_index
            .as_ref()
            .is_some_and(|index| index.matches(width, total))
        {
            return self.prefix_index.as_ref();
        }

        let stride = TRANSCRIPT_RECORD_PREFIX_STRIDE;
        let persisted_prefix = model.store.and_then(|store| {
            let chunks = store.extent_chunks().ok()?;
            let mut prefix_rows = Vec::with_capacity(chunks.len().saturating_add(1));
            prefix_rows.push(0);
            let mut expected_start = 0usize;
            let mut rows: RowIndex = 0;
            for chunk in chunks {
                if chunk.start.get() != expected_start || chunk.record_count == 0 {
                    return None;
                }
                expected_start = chunk.end().get();
                if expected_start > total {
                    return None;
                }
                rows = rows.saturating_add(chunk.profile.estimated_rows(width));
                prefix_rows.push(rows);
            }
            (expected_start == total).then_some(prefix_rows)
        });
        let prefix_rows = persisted_prefix.unwrap_or_else(|| {
            let mut prefix_rows = Vec::with_capacity(total.saturating_add(stride - 1) / stride + 1);
            prefix_rows.push(0);
            let mut rows: RowIndex = 0;
            let mut start = 0;
            while start < total {
                let end = start.saturating_add(stride).min(total);
                rows = rows.saturating_add(
                    (end.saturating_sub(start) as RowIndex)
                        .saturating_mul(model.fallback_rows_per_record),
                );
                prefix_rows.push(rows);
                start = end;
            }
            prefix_rows
        });
        self.prefix_index = Some(TranscriptExtentPrefixIndex {
            width,
            total_count: total,
            stride,
            prefix_rows,
        });
        self.prefix_index.as_ref()
    }

    fn estimated_record_for_row(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
        row: RowIndex,
    ) -> Option<(usize, RowIndex)> {
        let total = model.total_count?;
        if total == 0 {
            return None;
        }
        let (chunk_start, chunk_end, chunk_base_rows) =
            self.ensure_prefix_index(model)?.chunk_for_row(row)?;
        if chunk_start >= chunk_end {
            return None;
        }

        let target_in_chunk = row.saturating_sub(chunk_base_rows);
        let mut lo = chunk_start;
        let mut hi = chunk_end;
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            let before_mid = self.estimated_rows_for_record_range(model, chunk_start..mid);
            if before_mid <= target_in_chunk {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let before_record = self.estimated_rows_for_record_range(model, chunk_start..lo);
        let record_start = chunk_base_rows.saturating_add(before_record);
        Some((
            lo.min(total.saturating_sub(1)),
            row.saturating_sub(record_start),
        ))
    }

    fn estimated_sparse_prefix_rows(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
    ) -> RowIndex {
        let end = model
            .active_range
            .as_ref()
            .map(|range| range.start)
            .unwrap_or_else(|| model.records.missing_prefix_count_for_range(None));
        self.estimated_rows_before_record(model, end)
    }

    fn estimated_sparse_suffix_rows(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
    ) -> RowIndex {
        let Some(total) = model.total_count else {
            let count = model.records.missing_suffix_count_for_range(None);
            return (count as RowIndex).saturating_mul(model.fallback_rows_per_record);
        };
        let start = model
            .active_range
            .as_ref()
            .map(|range| range.end)
            .unwrap_or(total);
        self.estimated_rows_for_record_range(model, start..total)
    }

    fn extent_total_rows(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
        exact_loaded_rows: RowIndex,
    ) -> RowIndex {
        if let Some(total) = model.total_count {
            let mut total_rows = self.estimated_total_record_rows(model).unwrap_or_default();
            if model
                .active_range
                .as_ref()
                .is_some_and(|range| range.end >= total)
            {
                // Tail windows can include live blocks that are not in persisted row estimates yet.
                total_rows = total_rows.max(
                    self.estimated_sparse_prefix_rows(model)
                        .saturating_add(exact_loaded_rows),
                );
            }
            return total_rows;
        }

        self.estimated_sparse_prefix_rows(model)
            .saturating_add(exact_loaded_rows)
            .saturating_add(self.estimated_sparse_suffix_rows(model))
    }

    fn approximate_sparse_prefix_rows(
        &mut self,
        records: &SparseTranscriptRecords,
        active_record_range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
        store: Option<&SqliteTranscriptStore>,
        width: u16,
    ) -> RowIndex {
        let model = self.record_extent_model(records, active_record_range, store, width);
        self.estimated_sparse_prefix_rows(&model)
    }

    fn scrollbar_total_rows(
        &mut self,
        records: &SparseTranscriptRecords,
        active_record_range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
        store: Option<&SqliteTranscriptStore>,
        width: u16,
        exact_loaded_rows: RowIndex,
    ) -> RowIndex {
        let model = self.record_extent_model(records, active_record_range, store, width);
        self.extent_total_rows(&model, exact_loaded_rows)
    }
}

pub(crate) struct ReasoningSummarySnapshot {
    pub(crate) id: BlockId,
    pub(crate) title: Option<String>,
    pub(crate) summary_titles: Vec<String>,
    pub(crate) content: String,
}

pub(crate) struct TranscriptDocument {
    content: TranscriptContentState,
    records: TranscriptRecordState,
    store_cache: TranscriptStoreCache,
    extent_index: TranscriptExtentIndex,
    viewport: TranscriptViewportRuntime,
    memory_budget: TranscriptMemoryBudget,
    hydration: TranscriptHydrationState,
    pending_compaction: Option<PendingTranscriptCompaction>,
    compaction_order_index: usize,
    compacted_record_len: usize,
}

struct TranscriptContentState {
    transcript: Transcript,
    projection: crate::content::transcript_buf::TranscriptProjection,
    compaction_preview_id: Option<BlockId>,
}

struct TranscriptRecordState {
    sparse: SparseTranscriptRecords,
    active_range: Option<Range<smelt_store::TranscriptRecordOffset>>,
    session_dir: Option<PathBuf>,
}

impl TranscriptRecordState {
    fn from_loaded(loaded: &LoadedTranscript) -> Self {
        Self {
            sparse: SparseTranscriptRecords::from_loaded(loaded.record_window.as_ref()),
            active_range: loaded
                .record_window
                .as_ref()
                .map(|window| window.start..window.end()),
            session_dir: loaded.session_dir.clone(),
        }
    }

    fn active_range(&self) -> Option<&Range<smelt_store::TranscriptRecordOffset>> {
        self.active_range.as_ref()
    }

    fn session_dir(&self) -> Option<&PathBuf> {
        self.session_dir.as_ref()
    }

    fn extent_session_dir(&self) -> Option<&PathBuf> {
        if self.total_count()? == 0 {
            return None;
        }
        self.session_dir()
    }

    fn total_count(&self) -> Option<usize> {
        self.sparse.total_count()
    }

    fn records_for_active_range(&self) -> Vec<StoredBlockWithId> {
        self.sparse.records_for_range(self.active_range())
    }

    fn global_record_index(&self, local_record_index: usize) -> usize {
        self.active_range
            .as_ref()
            .map_or(local_record_index, |range| {
                range.start.get().saturating_add(local_record_index)
            })
    }

    fn truncate(&mut self, total_count: usize) {
        let total_count = self.sparse.truncate(total_count);
        self.active_range = self.active_range.take().and_then(|mut range| {
            range.end = range
                .end
                .min(smelt_store::TranscriptRecordOffset::new(total_count));
            (range.start < range.end).then_some(range)
        });
    }
}

#[derive(Default)]
struct TranscriptViewportRuntime {
    state: TranscriptViewportState,
    trace: Option<TranscriptScrollTrace>,
    #[cfg(test)]
    projection_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptRecordSaveBounds {
    pub(crate) order_start: usize,
    pub(crate) record_start_idx: usize,
    pub(crate) record_end_idx: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SparseProjectionGap {
    scroll_top: RowIndex,
    row_base: RowIndex,
    end: RowIndex,
}

fn minimum_materialized_total_rows(
    row_base: RowIndex,
    clamped_scroll: RowIndex,
    viewport_rows: u16,
) -> RowIndex {
    let viewport_rows = RowIndex::from(viewport_rows.max(1));
    let visible_end = if clamped_scroll == 0 {
        0
    } else {
        clamped_scroll.saturating_add(viewport_rows)
    };
    row_base.max(visible_end)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptAnchorBias {
    Top,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TranscriptContentAnchor {
    record_index: usize,
    block_id: BlockId,
    intra_block_row: RowIndex,
    bias: TranscriptAnchorBias,
    row_anchor: TranscriptRowAnchor,
    fallback_row: RowIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptNodeAnchor {
    record_index: usize,
    block_id: BlockId,
    node_index: usize,
    row_anchor: TranscriptRowAnchor,
}

// Durable origin for semantic transcript navigation. Search/reveal intents set this
// to the target block; normal viewport projection falls back to the top visible
// content anchor; far-seek gaps clear it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptSemanticAnchor {
    pub(crate) record_index: usize,
    pub(crate) block_id: BlockId,
    pub(crate) row_offset: RowIndex,
}

impl From<TranscriptContentAnchor> for TranscriptSemanticAnchor {
    fn from(anchor: TranscriptContentAnchor) -> Self {
        Self {
            record_index: anchor.record_index,
            block_id: anchor.block_id,
            row_offset: anchor.intra_block_row,
        }
    }
}

impl From<TranscriptNodeAnchor> for TranscriptSemanticAnchor {
    fn from(anchor: TranscriptNodeAnchor) -> Self {
        Self {
            record_index: anchor.record_index,
            block_id: anchor.block_id,
            row_offset: anchor.row_anchor.row_offset,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptScrollAnchor {
    Tail,
    Content(TranscriptContentAnchor),
    EstimatedRow(RowIndex),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptViewportMode {
    Tail,
    Anchored,
    FarSeek,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptLocalScroll {
    pub(crate) base_scroll: RowIndex,
    pub(crate) next_scroll: RowIndex,
    pub(crate) rows: isize,
    pub(crate) cursor_row: RowIndex,
    pub(crate) cursor_screen_row: u16,
}

fn signed_row_delta(before: RowIndex, after: RowIndex) -> isize {
    if after >= before {
        after.saturating_sub(before).min(isize::MAX as RowIndex) as isize
    } else {
        -(before.saturating_sub(after).min(isize::MAX as RowIndex) as isize)
    }
}

fn transcript_screen_row_or_edge(row: RowIndex, scroll_top: RowIndex, viewport_rows: u16) -> u16 {
    let rel = row.checked_sub(scroll_top);
    rel.and_then(|rel| (rel < RowIndex::from(viewport_rows)).then_some(rel as u16))
        .unwrap_or_else(|| {
            if row < scroll_top {
                0
            } else {
                viewport_rows.saturating_sub(1)
            }
        })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TranscriptProjectionRestore {
    pub(crate) cursor_screen_row: Option<u16>,
    pub(crate) drag_endpoint_screen_row: Option<u16>,
}

impl TranscriptProjectionRestore {
    pub(crate) fn merge(&mut self, other: Self) {
        if other.cursor_screen_row.is_some() {
            self.cursor_screen_row = other.cursor_screen_row;
        }
        if other.drag_endpoint_screen_row.is_some() {
            self.drag_endpoint_screen_row = other.drag_endpoint_screen_row;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptProjectionHint {
    SearchProjectedRow {
        width: u16,
        anchor: TranscriptSearchAnchor,
        start_byte_col: usize,
        row: RowIndex,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingTranscriptProjection {
    intent: TranscriptScrollIntent,
    restore: TranscriptProjectionRestore,
    local_scroll_top: Option<RowIndex>,
    hint: Option<TranscriptProjectionHint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TranscriptExactViewport {
    tape: crate::content::transcript_buf::ExactRowTapeHandle,
    width: u16,
    row_offset: RowIndex,
    global_total_rows: RowIndex,
    active_record_range: Option<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadedRowOffsetPolicy {
    /// Interpret rows from the currently rendered loaded projection. Falls back
    /// to the sparse prefix estimate when no matching exact viewport is active.
    RenderedViewportOrEstimate,
    /// Interpret rows in semantic sparse space while planning a new projection.
    SparseEstimate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TranscriptResolvedViewportAnchor {
    top: TranscriptScrollAnchor,
    offset_rows: isize,
    scroll_top: RowIndex,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TranscriptViewportState {
    resolved_anchor: Option<TranscriptResolvedViewportAnchor>,
    semantic_anchor: Option<TranscriptSemanticAnchor>,
    mode: TranscriptViewportMode,
    pending_projection: Option<PendingTranscriptProjection>,
    exact_viewport: Option<TranscriptExactViewport>,
    needs_tail_repin: bool,
}

impl Default for TranscriptViewportState {
    fn default() -> Self {
        Self {
            resolved_anchor: None,
            semantic_anchor: None,
            mode: TranscriptViewportMode::Tail,
            pending_projection: None,
            exact_viewport: None,
            needs_tail_repin: false,
        }
    }
}

struct TranscriptViewportIntent {
    intent: TranscriptScrollIntent,
    hint: Option<TranscriptProjectionHint>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TranscriptViewportProjectionInput {
    pub(crate) fallback_scroll_top: RowIndex,
    pub(crate) follow_tail: bool,
    pub(crate) width_changed: bool,
    pub(crate) previous_width: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptSearchAnchor {
    Content(TranscriptNodeAnchor),
    EstimatedRow(RowIndex),
}

impl TranscriptSearchAnchor {
    pub(crate) fn block_id(self) -> Option<BlockId> {
        match self {
            Self::Content(anchor) => Some(anchor.block_id),
            Self::EstimatedRow(_) => None,
        }
    }

    pub(crate) fn position_key(self, byte_col: usize) -> TranscriptSearchPositionKey {
        match self {
            Self::Content(anchor) => TranscriptSearchPositionKey {
                kind: 0,
                major: anchor.record_index as u64,
                node_index: anchor.node_index as u64,
                row_offset: anchor.row_anchor.row_offset,
                byte_col,
            },
            Self::EstimatedRow(row) => TranscriptSearchPositionKey {
                kind: 1,
                major: row,
                node_index: 0,
                row_offset: 0,
                byte_col,
            },
        }
    }

    fn same_position(self, other: Self) -> bool {
        match (self, other) {
            (Self::Content(left), Self::Content(right)) => left == right,
            (Self::EstimatedRow(left), Self::EstimatedRow(right)) => left == right,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TranscriptSearchPositionKey {
    kind: u8,
    major: u64,
    node_index: u64,
    row_offset: RowIndex,
    byte_col: usize,
}

impl TranscriptSearchPositionKey {
    pub(crate) fn min() -> Self {
        Self {
            kind: u8::MIN,
            major: u64::MIN,
            node_index: u64::MIN,
            row_offset: RowIndex::MIN,
            byte_col: usize::MIN,
        }
    }

    pub(crate) fn max() -> Self {
        Self {
            kind: u8::MAX,
            major: u64::MAX,
            node_index: u64::MAX,
            row_offset: RowIndex::MAX,
            byte_col: usize::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptSearchBound {
    key: TranscriptSearchPositionKey,
    inclusive: bool,
}

impl TranscriptSearchBound {
    pub(crate) fn inclusive(key: TranscriptSearchPositionKey) -> Self {
        Self {
            key,
            inclusive: true,
        }
    }

    pub(crate) fn before(key: TranscriptSearchPositionKey) -> Self {
        Self {
            key,
            inclusive: false,
        }
    }

    pub(crate) fn contains_forward(self, key: TranscriptSearchPositionKey) -> bool {
        if self.inclusive {
            key >= self.key
        } else {
            key > self.key
        }
    }

    pub(crate) fn contains_backward(self, key: TranscriptSearchPositionKey) -> bool {
        if self.inclusive {
            key <= self.key
        } else {
            key < self.key
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptSearchMatch {
    pub(crate) range: DocRange,
    pub(crate) anchor: TranscriptSearchAnchor,
}

impl TranscriptSearchMatch {
    pub(crate) fn new(range: DocRange, anchor: TranscriptSearchAnchor) -> Self {
        Self { range, anchor }
    }

    pub(crate) fn start_byte_col(&self) -> usize {
        self.range.start.byte_col
    }

    pub(crate) fn end_byte_col(&self) -> usize {
        self.range.end.byte_col
    }

    pub(crate) fn start_key(&self) -> TranscriptSearchPositionKey {
        self.anchor.position_key(self.start_byte_col())
    }

    pub(crate) fn end_key(&self) -> TranscriptSearchPositionKey {
        self.anchor.position_key(self.end_byte_col())
    }

    pub(crate) fn sort_key(&self) -> (TranscriptSearchPositionKey, usize) {
        (self.start_key(), self.end_byte_col())
    }

    pub(crate) fn same_position(&self, other: &Self) -> bool {
        self.anchor.same_position(other.anchor)
            && self.start_byte_col() == other.start_byte_col()
            && self.end_byte_col() == other.end_byte_col()
    }
}

pub(crate) struct AppliedTranscriptViewport {
    pub(crate) materialized_rows: crate::smelt_edit::MaterializedRows,
    pub(crate) top_anchor: Option<TranscriptTraceAnchor>,
    pub(crate) scrollbar_total_rows: RowIndex,
    pub(crate) exact_visible_range: Range<RowIndex>,
    pub(crate) placeholder_rows_visible: bool,
    pub(crate) scroll_state: VerticalScroll,
    pub(crate) cursor_range: Option<DocRange>,
}

#[derive(Clone, Copy, Debug)]
struct TranscriptIntentBehavior {
    viewport_mode: TranscriptViewportMode,
    allow_sparse_placeholders: bool,
    repin_at_semantic_tail: bool,
}

#[derive(Clone, Copy, Debug)]
struct TranscriptCursorTarget {
    anchor: TranscriptSearchAnchor,
    start_byte_col: usize,
    end_byte_col: usize,
}

enum TranscriptMaterializationPlan {
    ExactRowTape(crate::content::transcript_buf::ExactRowTapeProjection),
    Loaded(crate::content::transcript_buf::ProjectionPlan),
    UnloadedGap(SparseProjectionGap),
}

fn inert_sparse_gap_rows(count: RowIndex) -> Vec<DisplayRow> {
    (0..count)
        .map(|_| {
            DisplayRow::new(String::new(), Vec::new())
                .with_break_before(crate::smelt_edit::RowBreak::Hard)
        })
        .collect()
}

pub(crate) struct TranscriptProjectionPlan {
    materialization: TranscriptMaterializationPlan,
    row_offset: RowIndex,
    total_rows: RowIndex,
    planned_loaded_rows: RowIndex,
    preserve_total_rows: bool,
    requested_scroll: Option<RowIndex>,
    repin_at_semantic_tail: bool,
    cursor_target: Option<TranscriptCursorTarget>,
    semantic_anchor: Option<TranscriptSemanticAnchor>,
    scroll_anchor: TranscriptScrollAnchor,
    width: u16,
    viewport_rows: u16,
    trace_frame: Option<TranscriptScrollTraceFrameStart>,
    trace_started_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptProjectionHydrationError {
    pub(crate) required_blocks: usize,
    pub(crate) missing_blocks: usize,
}

#[derive(Clone, Copy, Debug)]
struct TranscriptSemanticFarSeek {
    scroll_top: RowIndex,
    total_rows: RowIndex,
    row_anchor: TranscriptRowAnchor,
}

#[derive(Clone, Copy, Debug)]
struct TranscriptProjectionOptions {
    allow_sparse_placeholders: bool,
    repin_at_semantic_tail: bool,
    semantic_far_seek: Option<TranscriptSemanticFarSeek>,
}

struct TranscriptScrollTraceFinishContext {
    width: u16,
    row_offset: RowIndex,
    viewport_rows: u16,
    trace_frame: Option<TranscriptScrollTraceFrameStart>,
    trace_started_at: Option<Instant>,
}

impl TranscriptDocument {
    pub(crate) fn new() -> Self {
        Self::from_transcript(Transcript::new())
    }

    pub(crate) fn next_refresh_at(&self) -> Option<std::time::Instant> {
        self.content.projection.next_refresh_at()
    }

    pub(crate) fn from_transcript(transcript: Transcript) -> Self {
        Self::from_loaded_transcript(LoadedTranscript::full(transcript))
    }

    pub(crate) fn from_loaded_transcript(loaded: LoadedTranscript) -> Self {
        if let Some(window) = loaded.record_window.as_ref() {
            record_record_window_metrics(window);
        }
        let records = TranscriptRecordState::from_loaded(&loaded);
        let compacted_record_len = records
            .active_range()
            .map(|range| range.start.get())
            .unwrap_or_default();
        let mut document = Self {
            content: TranscriptContentState {
                transcript: loaded.transcript,
                projection: crate::content::transcript_buf::TranscriptProjection::new(),
                compaction_preview_id: None,
            },
            records,
            store_cache: TranscriptStoreCache::default(),
            extent_index: TranscriptExtentIndex::default(),
            viewport: TranscriptViewportRuntime::default(),
            memory_budget: TranscriptMemoryBudget::default(),
            hydration: TranscriptHydrationState::default(),
            pending_compaction: None,
            compaction_order_index: 0,
            compacted_record_len,
        };
        document
            .content
            .projection
            .set_memory_budget(document.memory_budget.rendered_rows);
        if let Some(active_range) = document.records.active_range().cloned() {
            document
                .records
                .sparse
                .enforce_byte_budget(&active_range, document.memory_budget.record_windows);
            document.install_active_record_projection();
        }
        document
    }

    pub(crate) fn replace_transcript(&mut self, transcript: Transcript) {
        let session_dir = self.records.session_dir.clone();
        self.replace_loaded_transcript(LoadedTranscript::full(transcript));
        if let Some(session_dir) = session_dir {
            self.set_session_dir(session_dir);
        }
    }

    pub(crate) fn replace_loaded_transcript(&mut self, loaded: LoadedTranscript) {
        let inline_options = self.content.projection.inline_options().clone();
        let scroll_trace = self.viewport.trace.take();
        let memory_budget = self.memory_budget;
        *self = Self::from_loaded_transcript(loaded);
        self.viewport.trace = scroll_trace;
        self.set_inline_options(inline_options);
        self.set_memory_budget(memory_budget);
    }

    pub(crate) fn set_memory_budget(&mut self, budget: TranscriptMemoryBudget) {
        self.memory_budget = budget;
        self.content
            .projection
            .set_memory_budget(budget.rendered_rows);
        if let Some(active) = self.records.active_range().cloned() {
            self.records.sparse.touch_range(&active);
            self.records
                .sparse
                .enforce_byte_budget(&active, budget.record_windows);
        }
        self.enforce_hydrated_budget();
    }

    pub(crate) fn set_session_dir(&mut self, session_dir: PathBuf) {
        if self.records.session_dir.as_ref() != Some(&session_dir) {
            self.records.session_dir = Some(session_dir);
            self.store_cache = TranscriptStoreCache::default();
        }
    }

    pub(crate) fn session_dir(&self) -> Option<&std::path::Path> {
        self.records.session_dir.as_deref()
    }

    fn touch_hydrated(&mut self, ids: &[BlockId]) {
        let hydrated = ids
            .iter()
            .copied()
            .filter(|id| self.content.transcript.history.is_hydrated(*id))
            .collect::<Vec<_>>();
        self.hydration.touch_many(&hydrated);
    }

    pub(crate) fn ensure_hydrated_ids(&mut self, ids: &[BlockId]) -> bool {
        let mut requested = ids
            .iter()
            .copied()
            .filter_map(|id| {
                let history = &self.content.transcript.history;
                (!history.is_materialized(id)).then(|| {
                    history
                        .stored_ref(id)
                        .cloned()
                        .map(|stored| (stored.record_index, id, stored))
                })?
            })
            .collect::<Vec<_>>();
        requested.sort_unstable_by_key(|(record_index, _, _)| *record_index);
        requested.dedup_by_key(|(_, id, _)| *id);
        if requested.is_empty() {
            self.touch_hydrated(ids);
            self.enforce_hydrated_budget();
            return ids
                .iter()
                .all(|id| self.content.transcript.history.is_materialized(*id));
        }

        let mut ranges = Vec::<Range<usize>>::new();
        for (record_index, _, _) in &requested {
            match ranges.last_mut() {
                Some(range) if range.end == *record_index => {
                    range.end = range.end.saturating_add(1);
                }
                Some(range) if range.end > *record_index => {}
                _ => ranges.push(*record_index..record_index.saturating_add(1)),
            }
        }
        let requested_by_id = requested
            .into_iter()
            .map(|(_, id, stored)| (id, stored))
            .collect::<HashMap<_, _>>();
        let total_count = self.records.total_count().unwrap_or_else(|| {
            self.compacted_record_len
                .max(self.content.transcript.history.persisted_block_count())
        });
        if total_count == 0 {
            return false;
        }
        let session_dir = self.records.session_dir.clone();
        let hydration_started_at = Instant::now();
        let mut hydrated = 0_u64;
        let mut hydrated_bytes = 0_u64;
        for range in ranges {
            let Some(records) = self.store_cache.read_hydration_records(
                session_dir.as_ref(),
                smelt_store::TranscriptRecordRange::from(range),
                total_count,
            ) else {
                smelt_perf::perf::record_value("transcript:block_cache:hydration_failures", 1);
                continue;
            };
            self.hydration.hydration_ranges = self.hydration.hydration_ranges.saturating_add(1);
            for record in records {
                let Some(stored) = requested_by_id.get(&record.block_id).cloned() else {
                    continue;
                };
                if self.content.transcript.history.install_hydrated_record(
                    record.block_id,
                    stored,
                    record.record,
                ) {
                    hydrated = hydrated.saturating_add(1);
                    hydrated_bytes = hydrated_bytes.saturating_add(
                        self.content
                            .transcript
                            .history
                            .materialized_retained_bytes(record.block_id)
                            as u64,
                    );
                }
            }
        }
        let hydration_duration_us =
            u64::try_from(hydration_started_at.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.hydration.hydration_reads = self.hydration.hydration_reads.saturating_add(hydrated);
        self.hydration.hydration_bytes = self
            .hydration
            .hydration_bytes
            .saturating_add(hydrated_bytes);
        self.hydration.hydration_duration_us = self
            .hydration
            .hydration_duration_us
            .saturating_add(hydration_duration_us);
        smelt_perf::perf::record_value("transcript:block_cache:hydration_reads", hydrated);
        smelt_perf::perf::record_value("transcript:block_cache:hydration_bytes", hydrated_bytes);
        smelt_perf::perf::record_value(
            "transcript:block_cache:hydration_duration_us",
            hydration_duration_us,
        );
        smelt_perf::perf::record_value(
            "transcript:block_cache:hydration_ranges",
            self.hydration.hydration_ranges,
        );
        self.touch_hydrated(ids);
        self.enforce_hydrated_budget();
        ids.iter()
            .all(|id| self.content.transcript.history.is_materialized(*id))
    }

    fn set_viewport_hydration_ids(&mut self, ids: &[BlockId]) -> bool {
        self.hydration.viewport_pins.clear();
        self.hydration.viewport_pins.extend(ids.iter().copied());
        let hydrated = self.ensure_hydrated_ids(ids);
        self.enforce_hydrated_budget();
        hydrated
    }

    fn hydrate_projection_plan(
        &mut self,
        lua: &LuaRuntime,
        theme: &Theme,
        mut ids: Vec<BlockId>,
        mut plan: crate::content::transcript_buf::ProjectionPlan,
    ) -> Result<crate::content::transcript_buf::ProjectionPlan, TranscriptProjectionHydrationError>
    {
        loop {
            let generation_before = self.content.transcript.history.generation();
            let all_ids_hydrated = self.set_viewport_hydration_ids(&ids);
            plan = self.content.projection.remeasure_projection_plan(
                lua,
                &mut self.content.transcript.history,
                theme,
                plan,
            );
            let next_ids = self
                .content
                .projection
                .projection_hydration_ids_for_plan(&plan);
            if next_ids
                .iter()
                .all(|id| self.content.transcript.history.is_materialized(*id))
            {
                if next_ids != ids {
                    let _ = self.set_viewport_hydration_ids(&next_ids);
                }
                return Ok(plan);
            }
            if !all_ids_hydrated
                && self.content.transcript.history.generation() == generation_before
                && next_ids == ids
            {
                let missing_blocks = next_ids
                    .iter()
                    .filter(|id| !self.content.transcript.history.is_materialized(**id))
                    .count();
                return Err(TranscriptProjectionHydrationError {
                    required_blocks: next_ids.len(),
                    missing_blocks,
                });
            }
            ids = next_ids;
        }
    }

    pub(crate) fn pin_operation_blocks(&mut self, ids: &[BlockId]) -> bool {
        if ids.is_empty() {
            return true;
        }
        self.hydration.pin_operation(ids);
        if self.ensure_hydrated_ids(ids) {
            true
        } else {
            self.hydration.unpin_operation(ids);
            self.enforce_hydrated_budget();
            false
        }
    }

    pub(crate) fn unpin_operation_blocks(&mut self, ids: &[BlockId]) {
        if ids.is_empty() {
            return;
        }
        self.hydration.unpin_operation(ids);
        self.enforce_hydrated_budget();
    }

    pub(crate) fn promote_last_reasoning_summary(&mut self) -> Option<ReasoningSummarySnapshot> {
        let id = self.history().last_block_id()?;
        if self.history().block_kind(id) != Some("thinking") {
            return None;
        }
        if !self.pin_operation_blocks(&[id]) {
            return None;
        }
        let summary = match self.history().block(id) {
            Some(Block::Thinking {
                title,
                summary_titles,
                content,
                kind: protocol::ReasoningKind::Summary,
            }) => Some(ReasoningSummarySnapshot {
                id,
                title: title.clone(),
                summary_titles: summary_titles.clone(),
                content: content.clone(),
            }),
            _ => None,
        };
        if summary.is_some() {
            let promoted = self.content.transcript.history.promote_hydrated(id);
            debug_assert!(promoted, "materialized reasoning summary can be promoted");
        }
        self.unpin_operation_blocks(&[id]);
        summary
    }

    pub(crate) fn pin_record_suffix_for_save(
        &mut self,
        bounds: Option<TranscriptRecordSaveBounds>,
    ) -> Result<Vec<BlockId>, String> {
        let Some(bounds) = bounds else {
            return Ok(Vec::new());
        };
        let ids = self
            .content
            .transcript
            .history
            .order
            .iter()
            .skip(bounds.order_start)
            .copied()
            .filter(|id| self.content.transcript.history.stored_ref(*id).is_some())
            .collect::<Vec<_>>();
        if self.pin_operation_blocks(&ids) {
            Ok(ids)
        } else {
            Err("hydrate canonical transcript record suffix".to_string())
        }
    }

    pub(crate) fn apply_persisted_record_suffix(
        &mut self,
        bounds: TranscriptRecordSaveBounds,
        total_count: usize,
    ) {
        self.content
            .transcript
            .history
            .reindex_stored_records_from(bounds.order_start, bounds.record_start_idx);
        if self.records.total_count().is_some() {
            let record_start = smelt_store::TranscriptRecordOffset::new(bounds.record_start_idx);
            self.records.sparse.invalidate_from(record_start);
            self.records.sparse.total_count = Some(total_count);
            if let Some(active) = self.records.active_range.as_mut() {
                let start = active.start.get().min(total_count);
                let end = start
                    .saturating_add(self.content.transcript.history.persisted_block_count())
                    .min(total_count);
                *active = smelt_store::TranscriptRecordOffset::new(start)
                    ..smelt_store::TranscriptRecordOffset::new(end);
            }
            self.extent_index.clear_persisted_record_estimates();
            self.clear_transcript_layout_caches();
        }
    }

    fn enforce_hydrated_budget(&mut self) {
        let history = &self.content.transcript.history;
        for (id, _) in history.hydrated_blocks() {
            if self.hydration.lru_ids.insert(id) {
                self.hydration.lru.push_back(id);
            }
        }

        let mut retained = self.content.transcript.history.hydrated_retained_bytes();
        let mut attempts = self.hydration.lru.len();
        while retained > self.memory_budget.hydrated_blocks && attempts > 0 {
            attempts -= 1;
            let Some(id) = self.hydration.lru.pop_front() else {
                break;
            };
            self.hydration.lru_ids.remove(&id);
            if self.hydration.is_pinned(id) {
                self.hydration.lru_ids.insert(id);
                self.hydration.lru.push_back(id);
                continue;
            }
            let evicted = self.content.transcript.history.evict_hydrated(id);
            if evicted == 0 {
                continue;
            }
            retained = retained.saturating_sub(evicted);
            self.hydration.evicted_entries = self.hydration.evicted_entries.saturating_add(1);
            self.hydration.evicted_bytes =
                self.hydration.evicted_bytes.saturating_add(evicted as u64);
            attempts = self.hydration.lru.len();
        }
        self.hydration
            .lru
            .retain(|id| self.content.transcript.history.is_hydrated(*id));
        self.hydration
            .lru_ids
            .retain(|id| self.content.transcript.history.is_hydrated(*id));

        let pinned_bytes = self
            .content
            .transcript
            .history
            .hydrated_blocks()
            .filter(|(id, _)| self.hydration.is_pinned(*id))
            .map(|(_, bytes)| bytes)
            .sum::<usize>();
        let debt = retained.saturating_sub(self.memory_budget.hydrated_blocks);
        smelt_perf::perf::record_value("transcript:block_cache:retained_bytes", retained as u64);
        smelt_perf::perf::record_value("transcript:block_cache:pinned_bytes", pinned_bytes as u64);
        smelt_perf::perf::record_value("transcript:block_cache:oversize_debt_bytes", debt as u64);
    }

    pub(crate) fn memory_snapshot(&self) -> TranscriptMemorySnapshot {
        let history = &self.content.transcript.history;
        let mut record_window_bytes = 0;
        let mut seen = HashSet::new();
        for record in self.records.sparse.records.values() {
            if seen.insert(Arc::as_ptr(&record.stored) as usize) {
                record_window_bytes += record.stored.retained_bytes();
            }
        }
        let mut compact_record_bytes = 0;
        for id in &history.order {
            let Some(stored) = history.stored_ref(*id) else {
                continue;
            };
            if seen.insert(Arc::as_ptr(stored) as usize) {
                compact_record_bytes += stored.retained_bytes();
            }
        }
        let pinned_hydrated_bytes = history
            .hydrated_blocks()
            .filter(|(id, _)| self.hydration.is_pinned(*id))
            .map(|(_, bytes)| bytes)
            .sum();
        let hydrated_block_bytes = history.hydrated_block_retained_bytes();
        let hydrated_tool_state_bytes = history.hydrated_tool_state_retained_bytes();
        let hydrated_cache_bytes = hydrated_block_bytes.saturating_add(hydrated_tool_state_bytes);
        let render = self.content.projection.memory_snapshot();
        TranscriptMemorySnapshot {
            live_blocks: history.live_block_count(),
            stored_blocks: history.stored_block_count(),
            hydrated_blocks: history.hydrated_block_count(),
            hydrated_budget_bytes: self.memory_budget.hydrated_blocks,
            record_budget_bytes: self.memory_budget.record_windows,
            rendered_budget_bytes: self.memory_budget.rendered_rows,
            live_block_bytes: history.live_block_retained_bytes(),
            live_tool_state_bytes: history.live_tool_state_retained_bytes(),
            hydrated_block_bytes,
            hydrated_tool_state_bytes,
            compact_record_bytes,
            record_window_bytes,
            tool_state_metadata_bytes: history.tool_state_metadata_retained_bytes(),
            block_metadata_bytes: history.block_metadata_retained_bytes(),
            layout_bytes: render.layout_bytes,
            source_view_bytes: render.source_view_bytes,
            height_index_bytes: render.height_index_bytes,
            height_index_cache_bytes: render.height_index_cache_bytes,
            visible_rows_bytes: render.visible_rows_bytes,
            full_rows_bytes: render.full_rows_bytes,
            pinned_hydrated_bytes,
            pinned_rendered_bytes: render
                .pinned_layout_bytes
                .saturating_add(render.height_index_bytes)
                .saturating_add(render.visible_rows_bytes),
            hydrated_oversize_debt_bytes: hydrated_cache_bytes
                .saturating_sub(self.memory_budget.hydrated_blocks),
            record_oversize_debt_bytes: record_window_bytes
                .saturating_sub(self.memory_budget.record_windows),
            rendered_oversize_debt_bytes: render.oversize_debt_bytes,
            hydration_reads: self.hydration.hydration_reads,
            hydration_ranges: self.hydration.hydration_ranges,
            hydration_bytes: self.hydration.hydration_bytes,
            hydration_duration_us: self.hydration.hydration_duration_us,
            evicted_entries: self.hydration.evicted_entries,
            evicted_bytes: self.hydration.evicted_bytes,
            dematerialized_entries: self.hydration.dematerialized_entries,
            dematerialized_bytes: self.hydration.dematerialized_bytes,
        }
    }

    fn record_memory_metrics(&self) {
        let snapshot = self.memory_snapshot();
        for (label, value) in [
            ("transcript:memory:live_blocks", snapshot.live_blocks as u64),
            (
                "transcript:memory:stored_blocks",
                snapshot.stored_blocks as u64,
            ),
            (
                "transcript:memory:hydrated_blocks",
                snapshot.hydrated_blocks as u64,
            ),
            (
                "transcript:memory:live_block_bytes",
                snapshot.live_block_bytes as u64,
            ),
            (
                "transcript:memory:live_tool_state_bytes",
                snapshot.live_tool_state_bytes as u64,
            ),
            (
                "transcript:memory:hydrated_block_bytes",
                snapshot.hydrated_block_bytes as u64,
            ),
            (
                "transcript:memory:hydrated_tool_state_bytes",
                snapshot.hydrated_tool_state_bytes as u64,
            ),
            (
                "transcript:memory:compact_record_bytes",
                snapshot.compact_record_bytes as u64,
            ),
            (
                "transcript:memory:record_window_bytes",
                snapshot.record_window_bytes as u64,
            ),
            (
                "transcript:memory:tool_state_metadata_bytes",
                snapshot.tool_state_metadata_bytes as u64,
            ),
            (
                "transcript:memory:block_metadata_bytes",
                snapshot.block_metadata_bytes as u64,
            ),
            (
                "transcript:memory:layout_bytes",
                snapshot.layout_bytes as u64,
            ),
            (
                "transcript:memory:source_view_bytes",
                snapshot.source_view_bytes as u64,
            ),
            (
                "transcript:memory:height_index_bytes",
                snapshot.height_index_bytes as u64,
            ),
            (
                "transcript:memory:height_index_cache_bytes",
                snapshot.height_index_cache_bytes as u64,
            ),
            (
                "transcript:memory:visible_rows_bytes",
                snapshot.visible_rows_bytes as u64,
            ),
            (
                "transcript:memory:full_rows_bytes",
                snapshot.full_rows_bytes as u64,
            ),
            (
                "transcript:memory:pinned_hydrated_bytes",
                snapshot.pinned_hydrated_bytes as u64,
            ),
            (
                "transcript:memory:pinned_rendered_bytes",
                snapshot.pinned_rendered_bytes as u64,
            ),
            (
                "transcript:memory:hydrated_budget_bytes",
                self.memory_budget.hydrated_blocks as u64,
            ),
            (
                "transcript:memory:record_budget_bytes",
                self.memory_budget.record_windows as u64,
            ),
            (
                "transcript:memory:rendered_budget_bytes",
                self.memory_budget.rendered_rows as u64,
            ),
            (
                "transcript:memory:hydrated_oversize_debt_bytes",
                snapshot.hydrated_oversize_debt_bytes as u64,
            ),
            (
                "transcript:memory:record_oversize_debt_bytes",
                snapshot.record_oversize_debt_bytes as u64,
            ),
            (
                "transcript:memory:rendered_oversize_debt_bytes",
                snapshot.rendered_oversize_debt_bytes as u64,
            ),
            (
                "transcript:memory:hydration_reads",
                snapshot.hydration_reads,
            ),
            (
                "transcript:memory:hydration_ranges",
                snapshot.hydration_ranges,
            ),
            (
                "transcript:memory:hydration_bytes",
                snapshot.hydration_bytes,
            ),
            (
                "transcript:memory:hydration_duration_us",
                snapshot.hydration_duration_us,
            ),
            (
                "transcript:memory:evicted_entries",
                snapshot.evicted_entries,
            ),
            ("transcript:memory:evicted_bytes", snapshot.evicted_bytes),
            (
                "transcript:memory:dematerialized_entries",
                snapshot.dematerialized_entries,
            ),
            (
                "transcript:memory:dematerialized_bytes",
                snapshot.dematerialized_bytes,
            ),
        ] {
            smelt_perf::perf::record_value(label, value);
        }
    }

    pub(crate) fn schedule_durable_compaction(
        &mut self,
        record_len: usize,
        persisted_bounds: Option<TranscriptRecordSaveBounds>,
    ) {
        let mut next_order_index = self.compaction_order_index;
        let mut next_record_index = self.compacted_record_len;
        if let Some(bounds) =
            persisted_bounds.filter(|bounds| bounds.order_start <= next_order_index)
        {
            next_order_index = bounds.order_start;
            next_record_index = bounds.record_start_idx;
        }
        match self.pending_compaction.as_mut() {
            Some(pending) => {
                pending.record_len = record_len;
                if next_order_index < pending.next_order_index
                    || (next_order_index == pending.next_order_index
                        && next_record_index != pending.next_record_index)
                {
                    pending.next_order_index = next_order_index;
                    pending.next_record_index = next_record_index;
                }
            }
            None => {
                self.pending_compaction = Some(PendingTranscriptCompaction {
                    record_len,
                    next_order_index,
                    next_record_index,
                });
            }
        }
    }

    pub(crate) fn drain_compaction_slice(&mut self) -> bool {
        let Some(mut pending) = self.pending_compaction.take() else {
            self.enforce_hydrated_budget();
            return false;
        };
        let mut visited = 0;
        let mut released_bytes = 0;
        let mut progressed = false;
        while visited < TRANSCRIPT_IDLE_COMPACTION_BLOCKS
            && released_bytes < TRANSCRIPT_IDLE_COMPACTION_BYTES
        {
            if pending.next_record_index >= pending.record_len {
                break;
            }
            let history = &self.content.transcript.history;
            if history
                .record_dirty_from()
                .is_some_and(|dirty_from| pending.next_order_index >= dirty_from)
            {
                break;
            }
            let Some(id) = history.order.get(pending.next_order_index).copied() else {
                break;
            };
            if let Some(stored) = self.content.transcript.history.stored_ref(id) {
                pending.next_record_index = stored.record_index.saturating_add(1);
                pending.next_order_index = pending.next_order_index.saturating_add(1);
                visited += 1;
                progressed = true;
                continue;
            }
            let temporarily_pinned = self.hydration.is_pinned(id)
                || history.status(id) == Some(Status::Streaming)
                || history.tool_status(id).is_some_and(|status| {
                    !matches!(
                        status,
                        ToolStatus::Ok | ToolStatus::Err | ToolStatus::Denied
                    )
                });
            if temporarily_pinned {
                break;
            }
            let candidate_bytes = history.materialized_retained_bytes(id);
            if released_bytes > 0
                && released_bytes.saturating_add(candidate_bytes) > TRANSCRIPT_IDLE_COMPACTION_BYTES
            {
                break;
            }
            let stored = history.stored_ref_for_materialized(id, pending.next_record_index);
            let Some(stored) = stored else {
                pending.next_order_index = pending.next_order_index.saturating_add(1);
                visited += 1;
                progressed = true;
                continue;
            };
            let released = self
                .content
                .transcript
                .history
                .dematerialize_live(id, stored);
            if released == 0 {
                break;
            }
            released_bytes = released_bytes.saturating_add(released);
            pending.next_record_index = pending.next_record_index.saturating_add(1);
            pending.next_order_index = pending.next_order_index.saturating_add(1);
            visited += 1;
            progressed = true;
            self.hydration.dematerialized_entries =
                self.hydration.dematerialized_entries.saturating_add(1);
            self.hydration.dematerialized_bytes = self
                .hydration
                .dematerialized_bytes
                .saturating_add(released as u64);
        }
        self.compaction_order_index = pending.next_order_index;
        self.compacted_record_len = pending.next_record_index;
        if pending.next_record_index < pending.record_len {
            self.pending_compaction = Some(pending);
        }
        smelt_perf::perf::record_value(
            "transcript:block_cache:dematerialized_entries",
            self.hydration.dematerialized_entries,
        );
        smelt_perf::perf::record_value(
            "transcript:block_cache:dematerialized_bytes",
            self.hydration.dematerialized_bytes,
        );
        self.enforce_hydrated_budget();
        if self.pending_compaction.is_none() {
            self.record_memory_metrics();
        }
        progressed
    }

    pub(crate) fn load_record_window(
        &mut self,
        range: smelt_store::TranscriptRecordRange,
    ) -> Option<LoadedRecordWindow> {
        let total_count = self.records.total_count()?;
        let slice =
            self.store_cache
                .read_record_slice(self.records.session_dir(), range, total_count)?;
        LoadedRecordWindow::from_slice(slice)
    }

    fn merge_record_cache_window(&mut self, window: &LoadedRecordWindow) -> bool {
        let previous_total = self.records.total_count();
        if !self.records.sparse.merge(window) {
            return false;
        }
        if previous_total != self.records.total_count() {
            self.extent_index.clear_persisted_record_estimates();
        }
        record_record_window_metrics(window);
        true
    }

    #[cfg(test)]
    pub(crate) fn merge_record_window(&mut self, window: LoadedRecordWindow) -> bool {
        let active_range = window.start..window.end();
        if !self.merge_record_cache_window(&window) {
            return false;
        }
        self.records.active_range = Some(active_range);
        self.install_active_record_projection();
        true
    }

    pub(crate) fn scroll_trace_enabled(&self) -> bool {
        self.viewport.trace.is_some()
    }

    #[allow(dead_code)]
    pub(crate) fn set_scroll_trace_enabled(&mut self, enabled: bool) {
        match (enabled, self.viewport.trace.is_some()) {
            (true, false) => self.viewport.trace = Some(TranscriptScrollTrace::default()),
            (false, true) => self.viewport.trace = None,
            _ => {}
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_scroll_trace_timings_enabled(&mut self, enabled: bool) {
        self.viewport.trace = Some(TranscriptScrollTrace::with_timings(enabled));
    }

    pub(crate) fn record_scroll_trace_event(
        &mut self,
        kind: impl Into<String>,
        data: serde_json::Value,
    ) {
        if let Some(trace) = self.viewport.trace.as_mut() {
            trace.record_interaction(kind, data);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn take_scroll_trace_interaction_events(
        &mut self,
    ) -> Vec<TranscriptInteractionTraceEvent> {
        self.viewport
            .trace
            .as_mut()
            .map(TranscriptScrollTrace::take_interaction_events)
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub(crate) fn scroll_trace_interaction_events(&self) -> &[TranscriptInteractionTraceEvent] {
        self.viewport
            .trace
            .as_ref()
            .map(TranscriptScrollTrace::interaction_events)
            .unwrap_or(&[])
    }

    #[allow(dead_code)]
    pub(crate) fn scroll_trace_frames(&self) -> &[TranscriptScrollTraceFrame] {
        self.viewport
            .trace
            .as_ref()
            .map(TranscriptScrollTrace::frames)
            .unwrap_or(&[])
    }

    #[allow(dead_code)]
    pub(crate) fn take_scroll_trace_frames(&mut self) -> Vec<TranscriptScrollTraceFrame> {
        self.viewport
            .trace
            .as_mut()
            .map(TranscriptScrollTrace::take_frames)
            .unwrap_or_default()
    }

    pub(crate) fn last_traced_resolved_scroll_top(&self) -> Option<RowIndex> {
        self.viewport
            .trace
            .as_ref()
            .and_then(TranscriptScrollTrace::last_resolved_scroll_top)
    }

    pub(crate) fn scroll_trace_has_pending_input(&self) -> bool {
        self.viewport
            .trace
            .as_ref()
            .is_some_and(TranscriptScrollTrace::has_pending_input)
    }

    pub(crate) fn set_next_scroll_trace_input(&mut self, input: TranscriptScrollTraceRenderInput) {
        if let Some(trace) = self.viewport.trace.as_mut() {
            trace.set_pending_input(input);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_pending_scroll_intent(&mut self, intent: TranscriptScrollIntent) {
        self.set_pending_projection_with_hint(
            intent,
            TranscriptProjectionRestore::default(),
            None,
            None,
        );
    }

    pub(crate) fn set_pending_projection_with_hint(
        &mut self,
        intent: TranscriptScrollIntent,
        restore: TranscriptProjectionRestore,
        local_scroll_top: Option<RowIndex>,
        hint: Option<TranscriptProjectionHint>,
    ) {
        let previous = self.viewport.state.pending_projection.take();
        let mut next_restore = restore;
        let (intent, local_scroll_top, hint) = match (previous, intent) {
            (
                Some(PendingTranscriptProjection {
                    intent: TranscriptScrollIntent::UserDelta { rows: pending },
                    restore: mut pending_restore,
                    local_scroll_top: pending_local_scroll_top,
                    ..
                }),
                TranscriptScrollIntent::UserDelta { rows },
            ) => {
                pending_restore.merge(next_restore);
                next_restore = pending_restore;
                (
                    TranscriptScrollIntent::UserDelta {
                        rows: pending.saturating_add(rows),
                    },
                    local_scroll_top.or(pending_local_scroll_top),
                    None,
                )
            }
            (_, intent @ TranscriptScrollIntent::UserDelta { .. }) => {
                (intent, local_scroll_top, None)
            }
            (_, intent) => (intent, None, hint),
        };
        match intent {
            TranscriptScrollIntent::Tail => {
                self.viewport.state.needs_tail_repin = false;
            }
            TranscriptScrollIntent::UserDelta { rows } if rows < 0 => {
                self.viewport.state.needs_tail_repin = true;
            }
            TranscriptScrollIntent::PageDelta { pages } if pages < 0 => {
                self.viewport.state.needs_tail_repin = true;
            }
            TranscriptScrollIntent::SearchJump { .. }
            | TranscriptScrollIntent::RevealBlock { .. }
            | TranscriptScrollIntent::ExactContentAnchor(_)
            | TranscriptScrollIntent::ScrollbarFraction { .. }
            | TranscriptScrollIntent::ApproximateRowSeek(_)
                if !intent.is_explicit_tail_intent() =>
            {
                self.viewport.state.needs_tail_repin = true;
            }
            _ => {}
        }
        self.viewport.state.mode = Self::intent_behavior(&intent).viewport_mode;
        self.viewport.state.pending_projection = Some(PendingTranscriptProjection {
            intent,
            restore: next_restore,
            local_scroll_top,
            hint,
        });
    }

    pub(crate) fn local_command_scroll_top(&self, fallback: RowIndex) -> RowIndex {
        self.viewport
            .state
            .pending_projection
            .as_ref()
            .and_then(|pending| pending.local_scroll_top)
            .unwrap_or(fallback)
    }

    pub(crate) fn local_scroll_for_document_command(
        &self,
        command: DocumentCommand,
        viewport_rows: u16,
        fallback_scroll_top: RowIndex,
        cursor_row: RowIndex,
    ) -> Option<TranscriptLocalScroll> {
        let viewport_rows = viewport_rows.max(1);
        let base_scroll = self.local_command_scroll_top(fallback_scroll_top);
        let (cursor_row, next_scroll, cursor_screen_row) = match command {
            DocumentCommand::MoveRows(delta) => {
                let cursor_row = add_signed_row(cursor_row, delta);
                let next_scroll =
                    crate::smelt_edit::scroll_to_show(base_scroll, cursor_row, viewport_rows);
                (
                    cursor_row,
                    next_scroll,
                    transcript_screen_row_or_edge(cursor_row, next_scroll, viewport_rows),
                )
            }
            DocumentCommand::PageRows(pages) => {
                let cursor_row =
                    add_signed_row(cursor_row, (viewport_rows as isize).saturating_mul(pages));
                let next_scroll =
                    crate::smelt_edit::scroll_to_show(base_scroll, cursor_row, viewport_rows);
                (
                    cursor_row,
                    next_scroll,
                    transcript_screen_row_or_edge(cursor_row, next_scroll, viewport_rows),
                )
            }
            DocumentCommand::HalfPageRows(pages) => {
                let rows = (viewport_rows as isize / 2).max(1).saturating_mul(pages);
                let cursor_row = add_signed_row(cursor_row, rows);
                let next_scroll =
                    crate::smelt_edit::scroll_to_show(base_scroll, cursor_row, viewport_rows);
                (
                    cursor_row,
                    next_scroll,
                    transcript_screen_row_or_edge(cursor_row, next_scroll, viewport_rows),
                )
            }
            DocumentCommand::ScrollRows(rows) if rows != 0 => (
                cursor_row,
                add_signed_row(base_scroll, rows),
                transcript_screen_row_or_edge(cursor_row, base_scroll, viewport_rows),
            ),
            _ => return None,
        };
        Some(TranscriptLocalScroll {
            base_scroll,
            next_scroll,
            rows: signed_row_delta(base_scroll, next_scroll),
            cursor_row,
            cursor_screen_row,
        })
    }

    pub(crate) fn has_pending_local_scroll_top(&self) -> bool {
        self.viewport
            .state
            .pending_projection
            .as_ref()
            .and_then(|pending| pending.local_scroll_top)
            .is_some()
    }

    fn exact_viewport_state(
        &self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
    ) -> Option<(
        TranscriptExactViewport,
        crate::content::transcript_buf::ExactRowTapeState,
    )> {
        let exact = self.viewport.state.exact_viewport?;
        let state = self.content.projection.exact_row_tape_state(
            lua,
            &self.content.transcript.history,
            exact.tape,
            width,
            viewport_rows,
        )?;
        Some((exact, state))
    }

    pub(crate) fn prime_local_scroll_base(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        scroll_top: RowIndex,
    ) {
        let exact_viewport =
            self.exact_viewport_state(lua, width, viewport_rows)
                .filter(|(exact, state)| {
                    state.rows.clamped_scroll.saturating_add(exact.row_offset) == scroll_top
                });
        let Some((exact, _)) = exact_viewport else {
            self.viewport.state.exact_viewport = None;
            return;
        };
        self.capture_viewport_anchor_with_offset(
            lua,
            width,
            scroll_top,
            viewport_rows,
            exact.row_offset,
            TranscriptScrollAnchor::EstimatedRow(scroll_top),
        );
    }

    pub(crate) fn needs_tail_repin(&self) -> bool {
        self.viewport.state.needs_tail_repin
    }

    pub(crate) fn clear_pending_local_scroll_top(&mut self) {
        if let Some(pending) = self.viewport.state.pending_projection.as_mut() {
            pending.local_scroll_top = None;
        }
    }

    pub(crate) fn take_pending_projection_restore(&mut self) -> TranscriptProjectionRestore {
        self.viewport
            .state
            .pending_projection
            .as_mut()
            .map(|pending| std::mem::take(&mut pending.restore))
            .unwrap_or_default()
    }

    fn intent_behavior(intent: &TranscriptScrollIntent) -> TranscriptIntentBehavior {
        let repin_at_semantic_tail = intent.may_repin_when_semantic_tail_reached();
        match intent {
            TranscriptScrollIntent::Tail => TranscriptIntentBehavior {
                viewport_mode: TranscriptViewportMode::Tail,
                allow_sparse_placeholders: false,
                repin_at_semantic_tail,
            },
            TranscriptScrollIntent::UserDelta { .. } | TranscriptScrollIntent::PageDelta { .. } => {
                TranscriptIntentBehavior {
                    viewport_mode: TranscriptViewportMode::Anchored,
                    allow_sparse_placeholders: false,
                    repin_at_semantic_tail,
                }
            }
            TranscriptScrollIntent::ScrollbarFraction { .. } => TranscriptIntentBehavior {
                viewport_mode: TranscriptViewportMode::FarSeek,
                allow_sparse_placeholders: true,
                repin_at_semantic_tail,
            },
            TranscriptScrollIntent::ApproximateRowSeek(_) => TranscriptIntentBehavior {
                viewport_mode: TranscriptViewportMode::FarSeek,
                allow_sparse_placeholders: true,
                repin_at_semantic_tail,
            },
            TranscriptScrollIntent::PreserveViewport
            | TranscriptScrollIntent::ExactContentAnchor(_)
            | TranscriptScrollIntent::SearchJump { .. }
            | TranscriptScrollIntent::RevealBlock { .. }
            | TranscriptScrollIntent::ResizeReflow { .. } => TranscriptIntentBehavior {
                viewport_mode: TranscriptViewportMode::Anchored,
                allow_sparse_placeholders: false,
                repin_at_semantic_tail,
            },
        }
    }

    fn semantic_anchor_for_intent(
        intent: &TranscriptScrollIntent,
    ) -> Option<TranscriptSemanticAnchor> {
        match intent {
            TranscriptScrollIntent::RevealBlock {
                record_index,
                block_id,
                row_offset,
                ..
            } => Some(TranscriptSemanticAnchor {
                record_index: *record_index,
                block_id: *block_id,
                row_offset: *row_offset,
            }),
            TranscriptScrollIntent::SearchJump {
                anchor: TranscriptSearchAnchor::Content(anchor),
                ..
            } => Some((*anchor).into()),
            _ => None,
        }
    }

    fn trace_record_range(
        range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
    ) -> Option<TranscriptRecordTraceRange> {
        range.map(TranscriptRecordTraceRange::from_store_range)
    }

    fn trace_anchor(anchor: TranscriptScrollAnchor) -> TranscriptTraceAnchor {
        match anchor {
            TranscriptScrollAnchor::Tail => TranscriptTraceAnchor::Tail,
            TranscriptScrollAnchor::Content(anchor) => TranscriptTraceAnchor::Content {
                virtual_row: anchor.fallback_row,
                record_index: anchor.record_index,
                block_id: anchor.block_id,
                node_id: anchor.row_anchor.id,
                row_offset: anchor.intra_block_row,
            },
            TranscriptScrollAnchor::EstimatedRow(row) => TranscriptTraceAnchor::EstimatedRow(row),
        }
    }

    pub(crate) fn trace_anchor_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
    ) -> TranscriptTraceAnchor {
        let anchor = self
            .cached_local_row_offset(width)
            .and_then(|row_offset| {
                self.content_anchor_at_row_with_offset(
                    lua,
                    width,
                    row,
                    row_offset,
                    TranscriptAnchorBias::Top,
                )
            })
            .map(TranscriptScrollAnchor::Content)
            .unwrap_or(TranscriptScrollAnchor::EstimatedRow(row));
        Self::trace_anchor(anchor)
    }

    fn trace_projection_target(
        target: crate::content::transcript_buf::ScrollTarget,
    ) -> TranscriptProjectionTargetTrace {
        match target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => TranscriptProjectionTargetTrace::Tail,
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ExactRow(row),
            ) => TranscriptProjectionTargetTrace::ExactRow(row),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(row),
            ) => TranscriptProjectionTargetTrace::ReflowStableRow(row),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::StableRowDelta { row, delta, .. },
            ) => TranscriptProjectionTargetTrace::StableRowDelta { row, delta },
        }
    }

    fn start_scroll_trace_frame(
        &mut self,
        width: u16,
        projection_target: crate::content::transcript_buf::ScrollTarget,
        exact_extent: Option<(RowIndex, RowIndex)>,
    ) -> Option<(TranscriptScrollTraceFrameStart, Option<Instant>)> {
        let projection_target = Self::trace_projection_target(projection_target);
        let (input, record_timings) = {
            let trace = self.viewport.trace.as_mut()?;
            (
                trace.take_pending_input_or_default(projection_target),
                trace.record_timings(),
            )
        };
        let viewport_anchor_before = self
            .viewport
            .state
            .resolved_anchor
            .map(|anchor| Self::trace_anchor(anchor.top));
        let active_record_range_before = Self::trace_record_range(self.records.active_range());
        let (prefix_estimate_before, suffix_estimate_before) = exact_extent
            .map(|(row_offset, total_rows)| (row_offset, total_rows.saturating_sub(row_offset)))
            .or_else(|| {
                self.viewport
                    .state
                    .exact_viewport
                    .filter(|exact| exact.width == width)
                    .map(|exact| {
                        (
                            exact.row_offset,
                            exact.global_total_rows.saturating_sub(exact.row_offset),
                        )
                    })
            })
            .or_else(|| {
                let row_offset = self.cached_local_row_offset(width)?;
                let total_rows = self.cached_total_rows(width)?;
                Some((row_offset, total_rows.saturating_sub(row_offset)))
            })
            .unwrap_or_default();
        let exact_observation_count = self.extent_index.exact_observation_count();
        let started_at = record_timings.then(Instant::now);
        Some((
            TranscriptScrollTraceFrameStart {
                input,
                viewport_anchor_before,
                projection_target,
                active_record_range_before,
                prefix_estimate_before,
                suffix_estimate_before,
                exact_observation_count,
            },
            started_at,
        ))
    }

    fn finish_scroll_trace_frame(
        &mut self,
        lua: &LuaRuntime,
        rows: crate::smelt_edit::MaterializedRows,
        ctx: &mut TranscriptScrollTraceFinishContext,
        placeholder_rows_visible: bool,
    ) {
        let Some(trace_frame) = ctx.trace_frame.take() else {
            return;
        };
        let viewport_rows = RowIndex::from(ctx.viewport_rows.max(1));
        let first_visible_content_anchor = self.trace_visible_content_anchor_at_row_with_offset(
            lua,
            ctx.width,
            rows.clamped_scroll,
            ctx.row_offset,
        );
        let last_visible_row = rows
            .clamped_scroll
            .saturating_add(viewport_rows.saturating_sub(1))
            .min(rows.total_rows.saturating_sub(1));
        let last_visible_content_anchor = self.trace_visible_content_anchor_at_row_with_offset(
            lua,
            ctx.width,
            last_visible_row,
            ctx.row_offset,
        );
        let active_record_range_after = Self::trace_record_range(self.records.active_range());
        let viewport_anchor_after = self
            .viewport
            .state
            .resolved_anchor
            .map(|anchor| Self::trace_anchor(anchor.top));
        let visible_record_or_block_ids = self.visible_block_ids_for_virtual_range(
            rows.clamped_scroll..rows.clamped_scroll.saturating_add(viewport_rows),
            ctx.row_offset,
        );
        let render_or_projection_ms = ctx.trace_started_at.take().map(|started| {
            let millis = started.elapsed().as_millis();
            millis.min(u128::from(u64::MAX)) as u64
        });
        let frame = trace_frame.finish(
            rows.clamped_scroll,
            viewport_anchor_after,
            active_record_range_after,
            rows.materialized_range(),
            placeholder_rows_visible,
            first_visible_content_anchor,
            last_visible_content_anchor,
            visible_record_or_block_ids,
            render_or_projection_ms,
        );
        if let Some(trace) = self.viewport.trace.as_mut() {
            trace.push(frame);
        }
    }

    fn trace_visible_content_anchor_at_row_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
        row_offset: RowIndex,
    ) -> Option<TranscriptVisibleContentAnchor> {
        self.row_anchor_at_row_with_offset(lua, width, row, row_offset)
            .map(|anchor| TranscriptVisibleContentAnchor {
                virtual_row: row,
                node_id: anchor.id,
                row_offset: anchor.row_offset,
            })
    }

    fn visible_block_ids_for_virtual_range(
        &self,
        range: Range<RowIndex>,
        row_offset: RowIndex,
    ) -> Vec<BlockId> {
        self.visible_block_layout()
            .filter_map(|(id, first_row, rows)| {
                let first_row = first_row.saturating_add(row_offset);
                let end = first_row.saturating_add(rows);
                (first_row < range.end && end > range.start).then_some(id)
            })
            .collect()
    }

    fn install_active_record_projection(&mut self) {
        let records = self.records.records_for_active_range();
        smelt_perf::perf::record_value(
            "transcript:record_window:active_records",
            records.len() as u64,
        );
        if records.is_empty() {
            return;
        }
        self.content.transcript.history.install_stored_projection(
            records
                .into_iter()
                .map(|record| (record.block_id, record.stored)),
        );
        self.content.projection.invalidate_source_sequence();
    }

    fn approximate_average_record_rows(&self, width: u16) -> RowIndex {
        self.extent_index
            .fallback_average_rows_per_loaded_record(&self.records.sparse, width)
    }

    fn active_record_range_key(&self) -> Option<(usize, usize)> {
        self.records
            .active_range()
            .map(|range| (range.start.get(), range.end.get()))
    }

    fn current_rendered_loaded_row_offset(&self, width: u16) -> Option<RowIndex> {
        let active_record_range = self.active_record_range_key();
        self.viewport
            .state
            .exact_viewport
            .filter(|exact| {
                exact.width == width && exact.active_record_range == active_record_range
            })
            .map(|exact| exact.row_offset)
    }

    fn loaded_row_offset(&mut self, width: u16, policy: LoadedRowOffsetPolicy) -> RowIndex {
        match policy {
            LoadedRowOffsetPolicy::RenderedViewportOrEstimate => self
                .current_rendered_loaded_row_offset(width)
                .unwrap_or_else(|| self.approximate_sparse_prefix_row_offset(width)),
            LoadedRowOffsetPolicy::SparseEstimate => {
                self.approximate_sparse_prefix_row_offset(width)
            }
        }
    }

    fn cached_sparse_prefix_row_offset(&self, width: u16) -> Option<RowIndex> {
        self.extent_index.cached_rows_before_record(
            width,
            self.records.total_count()?,
            self.records.active_range()?.start.get(),
        )
    }

    fn cached_local_row_offset(&self, width: u16) -> Option<RowIndex> {
        if self.records.total_count().is_none() {
            return Some(0);
        }
        self.cached_sparse_prefix_row_offset(width)
            .or_else(|| self.current_rendered_loaded_row_offset(width))
    }

    fn cached_total_rows(&self, width: u16) -> Option<RowIndex> {
        let total_count = self.records.total_count()?;
        self.extent_index.cached_total_rows(width, total_count)
    }

    fn approximate_sparse_prefix_row_offset(&mut self, width: u16) -> RowIndex {
        let store = self
            .store_cache
            .store_for_session(self.records.extent_session_dir());
        self.extent_index.approximate_sparse_prefix_rows(
            &self.records.sparse,
            self.records.active_range(),
            store,
            width,
        )
    }

    fn observe_exact_loaded_record_rows(&mut self) {
        let snapshot = self.content.projection.exact_height_snapshot();
        self.extent_index
            .observe_exact_loaded_record_rows(&self.records.sparse, snapshot);
    }

    fn scrollbar_total_rows(&mut self, width: u16, exact_loaded_rows: RowIndex) -> RowIndex {
        let store = self
            .store_cache
            .store_for_session(self.records.extent_session_dir());
        self.extent_index.scrollbar_total_rows(
            &self.records.sparse,
            self.records.active_range(),
            store,
            width,
            exact_loaded_rows,
        )
    }

    fn exact_loaded_row_for_virtual_content_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
    ) -> Option<RowIndex> {
        let row_offset =
            self.loaded_row_offset(width, LoadedRowOffsetPolicy::RenderedViewportOrEstimate);
        self.exact_loaded_row_for_virtual_content_row_with_offset(lua, width, row, row_offset)
    }

    fn exact_loaded_row_for_virtual_content_row_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
        row_offset: RowIndex,
    ) -> Option<RowIndex> {
        let loaded_rows = self.content.projection.estimated_total_rows(
            lua,
            &mut self.content.transcript.history,
            width,
        );
        let loaded_end = row_offset.saturating_add(loaded_rows);
        (row_offset <= row && row < loaded_end).then_some(row.saturating_sub(row_offset))
    }

    fn offset_node_row(
        &mut self,
        width: u16,
        mut node: crate::content::transcript_buf::TranscriptNodeRow,
    ) -> crate::content::transcript_buf::TranscriptNodeRow {
        let offset =
            self.loaded_row_offset(width, LoadedRowOffsetPolicy::RenderedViewportOrEstimate);
        node.first_row = node.first_row.saturating_add(offset);
        node
    }

    fn clear_transcript_layout_caches(&mut self) {
        self.extent_index.clear_exact_local_record_rows();
    }

    #[cfg(test)]
    pub(crate) fn extent_store_read_count_for_harness(&self) -> usize {
        self.store_cache
            .store
            .as_ref()
            .map_or(0, |(_, store)| store.extent_read_count())
    }

    #[cfg(test)]
    pub(crate) fn store_open_attempt_count_for_harness(&self) -> usize {
        self.store_cache.open_attempt_count
    }

    #[cfg(test)]
    pub(crate) fn projection_count_for_harness(&self) -> usize {
        self.viewport.projection_count
    }

    pub(crate) fn set_inline_options(&mut self, options: InlineOptions) {
        self.content.projection.set_inline_options(options);
        self.clear_transcript_layout_caches();
    }

    pub(crate) fn invalidate_theme(&mut self) {
        self.content.projection.invalidate_theme();
    }

    pub(crate) fn build_rows(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
    ) -> Arc<Vec<String>> {
        let ids = self.content.transcript.history.order.clone();
        if !self.pin_operation_blocks(&ids) {
            return Arc::new(Vec::new());
        }
        let rows = self.content.projection.build_rows(
            lua,
            &mut self.content.transcript.history,
            width,
            theme,
        );
        self.unpin_operation_blocks(&ids);
        rows
    }

    pub(crate) fn approximate_scrollbar_total_rows(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> crate::smelt_edit::RowIndex {
        let loaded_rows = self.content.projection.estimated_total_rows(
            lua,
            &mut self.content.transcript.history,
            width,
        );
        self.scrollbar_total_rows(width, loaded_rows)
    }

    fn materialize_exact_loaded_block_layout_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row_offset: RowIndex,
    ) -> Vec<(BlockId, RowIndex, RowIndex)> {
        let ids = self.content.transcript.history.order.clone();
        if !self.pin_operation_blocks(&ids) {
            return Vec::new();
        }
        let layout = self
            .content
            .projection
            .materialize_block_layout(lua, &mut self.content.transcript.history, width)
            .into_iter()
            .map(|(id, first_row, rows)| (id, first_row.saturating_add(row_offset), rows))
            .collect();
        self.unpin_operation_blocks(&ids);
        layout
    }

    pub(crate) fn materialize_exact_loaded_block_layout(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> Vec<(BlockId, RowIndex, RowIndex)> {
        let row_offset = self.loaded_row_offset(width, LoadedRowOffsetPolicy::SparseEstimate);
        self.materialize_exact_loaded_block_layout_with_offset(lua, width, row_offset)
    }

    fn block_snapshot_for_block(
        &self,
        block_id: BlockId,
        first_row: crate::smelt_edit::RowIndex,
        rows: crate::smelt_edit::RowIndex,
    ) -> Option<TranscriptBlockSnapshot> {
        let history = self.history();
        let record_index = self
            .record_index_for_block_id(block_id)
            .or_else(|| history.order.iter().position(|id| *id == block_id))?;
        Some(TranscriptBlockSnapshot {
            record_index,
            block_id,
            role: history.block_kind(block_id)?,
            first_row,
            rows,
            first_line: history.first_line(block_id).unwrap_or_default(),
        })
    }

    fn block_snapshots_from_layout(
        &self,
        layout: impl Iterator<
            Item = (
                BlockId,
                crate::smelt_edit::RowIndex,
                crate::smelt_edit::RowIndex,
            ),
        >,
    ) -> Vec<TranscriptBlockSnapshot> {
        layout
            .filter_map(|(block_id, first_row, rows)| {
                self.block_snapshot_for_block(block_id, first_row, rows)
            })
            .collect()
    }

    pub(crate) fn visible_block_snapshots(&self) -> Vec<TranscriptBlockSnapshot> {
        self.block_snapshots_from_layout(self.visible_block_layout())
    }

    pub(crate) fn materialize_block_snapshots(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> Vec<TranscriptBlockSnapshot> {
        let layout = self.materialize_exact_loaded_block_layout(lua, width);
        self.block_snapshots_from_layout(layout.into_iter())
    }

    fn navigation_block_from_record(
        index: usize,
        record: &StoredBlockWithId,
    ) -> Option<TranscriptNavigationBlock> {
        let first_line = record.stored.first_line().to_string();
        if first_line.is_empty() {
            return None;
        }
        Some(TranscriptNavigationBlock {
            record_index: index,
            block_id: record.block_id,
            role: record.stored.kind.as_str(),
            first_line,
        })
    }

    fn record_index_for_block_id(&self, block_id: BlockId) -> Option<usize> {
        if let Some(index) = self.records.sparse.record_index_for_block_id(block_id) {
            return Some(index.get());
        }
        let history = self.history();
        if let Some(stored) = history.stored_ref(block_id) {
            return Some(stored.record_index);
        }
        if self.records.total_count().is_none() {
            return history.order.iter().position(|id| *id == block_id);
        }
        if !history.is_tool_draft(block_id)
            && history.block_kind(block_id) != Some("compaction_preview")
        {
            if let Some(order_index) = history.order.iter().position(|id| *id == block_id) {
                let local_record_index = history.record_index_for_order_index(order_index);
                return Some(self.records.global_record_index(local_record_index));
            }
        }
        self.stored_record_index_for_block_idx(block_id.get())
    }

    pub(crate) fn record_matches_block_id(&self, record_index: usize, block_id: BlockId) -> bool {
        if self.records.total_count().is_none() {
            return self.history().order.get(record_index).copied() == Some(block_id);
        }
        if let Some(record) = self
            .records
            .sparse
            .record(smelt_store::TranscriptRecordOffset::new(record_index))
        {
            return record.block_id == block_id;
        }
        self.stored_record_index_for_block_idx(block_id.get()) == Some(record_index)
    }

    fn stored_record_index_for_block_idx(&self, block_idx: u64) -> Option<usize> {
        let session_dir = self.records.session_dir()?;
        SqliteTranscriptStore::open_read_only(session_dir)
            .ok()?
            .record_index_for_block_idx(block_idx)
            .ok()
            .flatten()
    }

    pub(crate) fn activate_record_window_for_block_idx(
        &mut self,
        width: u16,
        block_idx: u64,
        viewport_rows: u16,
    ) -> bool {
        if self.records.total_count().is_none() {
            return false;
        }
        let record_index = self.record_index_for_block_id(BlockId::new(block_idx));
        let Some(record_index) = record_index else {
            return false;
        };
        let Some(range) =
            self.record_window_range_around_center(width, record_index, viewport_rows, true)
        else {
            return false;
        };
        self.activate_record_window_range(range)
    }

    fn content_anchor_at_row_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
        row_offset: RowIndex,
        bias: TranscriptAnchorBias,
    ) -> Option<TranscriptContentAnchor> {
        let row_anchor = self.row_anchor_at_row_with_offset(lua, width, row, row_offset)?;
        self.content_anchor_for_row_anchor(row_anchor, row, bias)
    }

    fn content_anchor_for_row_anchor(
        &self,
        row_anchor: TranscriptRowAnchor,
        fallback_row: RowIndex,
        bias: TranscriptAnchorBias,
    ) -> Option<TranscriptContentAnchor> {
        let block_id = row_anchor.id.as_block_id()?;
        let record_index = self.record_index_for_block_id(block_id)?;
        Some(TranscriptContentAnchor {
            record_index,
            block_id,
            intra_block_row: row_anchor.row_offset,
            bias,
            row_anchor,
            fallback_row,
        })
    }

    fn node_anchor_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
    ) -> Option<TranscriptNodeAnchor> {
        let node = self.node_metadata_at_row(lua, width, row)?;
        let block_id = self
            .content
            .projection
            .representative_block_id_for_node_index(node.index)?;
        let record_index = self.record_index_for_block_id(block_id)?;
        Some(TranscriptNodeAnchor {
            record_index,
            block_id,
            node_index: node.index,
            row_anchor: TranscriptRowAnchor {
                id: node.id,
                row_offset: node.row_offset,
            },
        })
    }

    pub(crate) fn search_anchor_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
    ) -> TranscriptSearchAnchor {
        self.node_anchor_at_row(lua, width, row)
            .map(TranscriptSearchAnchor::Content)
            .unwrap_or(TranscriptSearchAnchor::EstimatedRow(row))
    }

    fn semantic_row_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        record_index: usize,
    ) -> Option<RowIndex> {
        let active_changed = if self.records.total_count().is_some() {
            let range =
                self.record_window_range_around_center(width, record_index, viewport_rows, true)?;
            self.activate_record_window_range(range)
        } else {
            false
        };
        let active_record_range = self
            .records
            .active_range()
            .map(|range| (range.start.get(), range.end.get()));
        Some(
            if !active_changed {
                self.exact_viewport_state(lua, width, viewport_rows)
                    .filter(|(exact, _)| exact.active_record_range == active_record_range)
                    .map(|(exact, _)| exact.row_offset)
            } else {
                None
            }
            .unwrap_or_else(|| {
                self.loaded_row_offset(width, LoadedRowOffsetPolicy::SparseEstimate)
            }),
        )
    }

    fn row_for_content_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        anchor: TranscriptContentAnchor,
    ) -> Option<RowIndex> {
        let row_offset =
            self.semantic_row_offset(lua, width, viewport_rows, anchor.record_index)?;
        self.row_for_content_anchor_with_offset(lua, width, anchor, row_offset)
    }

    fn row_for_content_anchor_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: TranscriptContentAnchor,
        row_offset: RowIndex,
    ) -> Option<RowIndex> {
        if let Some(row) =
            self.row_for_anchor_with_offset(lua, width, anchor.row_anchor, row_offset)
        {
            return Some(row);
        }
        let block_id = self.current_block_id_for_content_anchor(anchor);
        let row_anchor = self.content.projection.row_anchor_for_block(
            lua,
            &mut self.content.transcript.history,
            width,
            block_id,
            anchor.intra_block_row,
        )?;
        self.row_for_anchor_with_offset(lua, width, row_anchor, row_offset)
    }

    fn row_for_preserved_content_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        anchor: TranscriptContentAnchor,
    ) -> Option<RowIndex> {
        if self.records.total_count().is_none() {
            return self.row_for_content_anchor(lua, width, viewport_rows, anchor);
        }
        if self.records.active_range().is_some_and(|range| {
            range.start.get() <= anchor.record_index && anchor.record_index < range.end.get()
        }) {
            if let Some(row) = self.row_for_content_anchor(lua, width, viewport_rows, anchor) {
                return Some(row);
            }
        }
        let _ = self.activate_semantic_far_seek_record_window(
            width,
            anchor.record_index,
            viewport_rows,
        );
        let row_offset = self.loaded_row_offset(width, LoadedRowOffsetPolicy::SparseEstimate);
        self.row_for_content_anchor_with_offset(lua, width, anchor, row_offset)
    }

    fn row_for_node_anchor_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: TranscriptNodeAnchor,
        row_offset: RowIndex,
    ) -> Option<RowIndex> {
        if let Some(row) =
            self.row_for_anchor_with_offset(lua, width, anchor.row_anchor, row_offset)
        {
            return Some(row);
        }
        let row_anchor = self.content.projection.row_anchor_for_block(
            lua,
            &mut self.content.transcript.history,
            width,
            anchor.block_id,
            anchor.row_anchor.row_offset,
        )?;
        self.row_for_anchor_with_offset(lua, width, row_anchor, row_offset)
    }

    fn row_for_node_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        anchor: TranscriptNodeAnchor,
    ) -> Option<RowIndex> {
        let row_offset =
            self.semantic_row_offset(lua, width, viewport_rows, anchor.record_index)?;
        self.row_for_node_anchor_with_offset(lua, width, anchor, row_offset)
    }

    fn content_anchor_at_or_after_row_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        top_row: RowIndex,
        viewport_rows: u16,
        row_offset: RowIndex,
    ) -> Option<(TranscriptContentAnchor, isize)> {
        let viewport_rows = RowIndex::from(viewport_rows.max(1));
        for offset in 0..viewport_rows {
            let row = top_row.saturating_add(offset);
            if let Some(anchor) = self.content_anchor_at_row_with_offset(
                lua,
                width,
                row,
                row_offset,
                TranscriptAnchorBias::Top,
            ) {
                return Some((anchor, -(offset as isize)));
            }
        }

        let visible_end = top_row.saturating_add(viewport_rows);
        let (block_id, first_row, rows) = self
            .materialize_exact_loaded_block_layout_with_offset(lua, width, row_offset)
            .into_iter()
            .find(|(_, first_row, rows)| {
                let end = first_row.saturating_add(*rows);
                *first_row < visible_end && end > top_row
            })?;
        let record_index = self.record_index_for_block_id(block_id)?;
        let intra_block_row = top_row
            .saturating_sub(first_row)
            .min(rows.saturating_sub(1));
        let anchor_row = first_row.saturating_add(intra_block_row);
        let offset = if anchor_row >= top_row {
            -(anchor_row
                .saturating_sub(top_row)
                .min(isize::MAX as RowIndex) as isize)
        } else {
            top_row
                .saturating_sub(anchor_row)
                .min(isize::MAX as RowIndex) as isize
        };
        Some((
            TranscriptContentAnchor {
                record_index,
                block_id,
                intra_block_row,
                bias: TranscriptAnchorBias::Top,
                row_anchor: TranscriptRowAnchor {
                    id: crate::content::render_plan::RenderNodeId::Block(block_id),
                    row_offset: intra_block_row,
                },
                fallback_row: anchor_row,
            },
            offset,
        ))
    }

    pub(crate) fn current_navigation_anchor(&self) -> Option<TranscriptSemanticAnchor> {
        if let Some(anchor) = self.viewport.state.semantic_anchor {
            return Some(anchor);
        }

        if let Some(TranscriptResolvedViewportAnchor {
            top: TranscriptScrollAnchor::Content(anchor),
            ..
        }) = self.viewport.state.resolved_anchor
        {
            return Some(TranscriptSemanticAnchor {
                record_index: anchor.record_index,
                block_id: anchor.block_id,
                row_offset: anchor.intra_block_row,
            });
        }

        if let Some(active) = self.records.active_range() {
            let record_index = active.start.get();
            if let Some(record) = self
                .records
                .sparse
                .record(smelt_store::TranscriptRecordOffset::new(record_index))
            {
                return Some(TranscriptSemanticAnchor {
                    record_index,
                    block_id: record.block_id,
                    row_offset: 0,
                });
            }
        }

        self.history()
            .order
            .first()
            .copied()
            .map(|block_id| TranscriptSemanticAnchor {
                record_index: 0,
                block_id,
                row_offset: 0,
            })
    }

    fn navigation_record_from_store(
        &self,
        role: &str,
        anchor_index: usize,
        direction: TranscriptNavigationDirection,
    ) -> Option<(usize, StoredBlockWithId)> {
        let session_dir = self.records.session_dir()?;
        let fallback_store;
        let store = if let Some(store) = self.store_cache.cached_store_for_session(session_dir) {
            store
        } else {
            fallback_store = SqliteTranscriptStore::open_read_only(session_dir).ok()?;
            &fallback_store
        };
        let (index, record) = match direction {
            TranscriptNavigationDirection::Previous => {
                let before = anchor_index.checked_sub(1)?;
                store.record_before_kind(role, before).ok().flatten()?
            }
            TranscriptNavigationDirection::Next => store
                .record_after_kind(role, anchor_index.saturating_add(1))
                .ok()
                .flatten()?,
        };
        let estimated_text_bytes = record.estimated_text_bytes;
        let preview = record.preview_text.clone();
        let record = TranscriptBlockRecordWithId::try_from(record).ok()?;
        let (block_id, stored) = smelt_core::transcript_model::StoredBlockRef::from_record(
            index,
            record.block_id,
            &record.record,
            estimated_text_bytes,
            preview,
        );
        Some((index, StoredBlockWithId { block_id, stored }))
    }

    fn choose_navigation_record(
        loaded: Option<(usize, StoredBlockWithId)>,
        stored: Option<(usize, StoredBlockWithId)>,
        direction: TranscriptNavigationDirection,
    ) -> Option<(usize, StoredBlockWithId)> {
        match (loaded, stored) {
            (Some(loaded), Some(stored)) => match direction {
                TranscriptNavigationDirection::Previous => {
                    Some(if loaded.0 >= stored.0 { loaded } else { stored })
                }
                TranscriptNavigationDirection::Next => {
                    Some(if loaded.0 <= stored.0 { loaded } else { stored })
                }
            },
            (Some(record), None) | (None, Some(record)) => Some(record),
            (None, None) => None,
        }
    }

    fn containing_navigation_block(
        &self,
        anchor: TranscriptSemanticAnchor,
        role: &str,
    ) -> Option<TranscriptNavigationBlock> {
        if anchor.row_offset == 0 {
            return None;
        }

        if self.records.total_count().is_some() {
            let record = self
                .records
                .sparse
                .record(smelt_store::TranscriptRecordOffset::new(
                    anchor.record_index,
                ))?;
            if record.block_id != anchor.block_id || record.stored.kind.as_str() != role {
                return None;
            }
            return Self::navigation_block_from_record(anchor.record_index, record);
        }

        let history = self.history();
        if history.order.get(anchor.record_index).copied() != Some(anchor.block_id)
            || transcript_history_role(history, anchor.block_id) != role
        {
            return None;
        }
        let first_line = transcript_raw_first_line(history, anchor.block_id);
        (!first_line.is_empty()).then_some(TranscriptNavigationBlock {
            record_index: anchor.record_index,
            block_id: anchor.block_id,
            role: transcript_history_role(history, anchor.block_id),
            first_line,
        })
    }

    fn navigation_block_from_anchor(
        &self,
        anchor: TranscriptSemanticAnchor,
        role: Option<&str>,
        direction: TranscriptNavigationDirection,
    ) -> Option<TranscriptNavigationBlock> {
        let role = role.unwrap_or("user");
        if direction == TranscriptNavigationDirection::Previous {
            if let Some(block) = self.containing_navigation_block(anchor, role) {
                return Some(block);
            }
        }

        if self.records.total_count().is_some() {
            let record_anchor = smelt_store::TranscriptRecordOffset::new(anchor.record_index);
            let loaded = self
                .records
                .sparse
                .navigation_record(role, record_anchor, direction);
            let stored = self.navigation_record_from_store(role, anchor.record_index, direction);
            let (index, record) = Self::choose_navigation_record(loaded, stored, direction)?;
            return Self::navigation_block_from_record(index, &record);
        }

        let history = self.history();
        let iter: Box<dyn Iterator<Item = (usize, BlockId)> + '_> = match direction {
            TranscriptNavigationDirection::Previous => Box::new(
                history
                    .order
                    .iter()
                    .copied()
                    .enumerate()
                    .take(anchor.record_index)
                    .rev(),
            ),
            TranscriptNavigationDirection::Next => Box::new(
                history
                    .order
                    .iter()
                    .copied()
                    .enumerate()
                    .skip(anchor.record_index.saturating_add(1)),
            ),
        };
        for (index, block_id) in iter {
            if transcript_history_role(history, block_id) != role {
                continue;
            }
            let first_line = transcript_raw_first_line(history, block_id);
            if first_line.is_empty() {
                continue;
            }
            return Some(TranscriptNavigationBlock {
                record_index: index,
                block_id,
                role: transcript_history_role(history, block_id),
                first_line,
            });
        }
        None
    }

    pub(crate) fn previous_navigation_block_from(
        &self,
        anchor: TranscriptSemanticAnchor,
        role: Option<&str>,
    ) -> Option<TranscriptNavigationBlock> {
        self.navigation_block_from_anchor(anchor, role, TranscriptNavigationDirection::Previous)
    }

    pub(crate) fn next_navigation_block_from(
        &self,
        anchor: TranscriptSemanticAnchor,
        role: Option<&str>,
    ) -> Option<TranscriptNavigationBlock> {
        self.navigation_block_from_anchor(anchor, role, TranscriptNavigationDirection::Next)
    }

    #[cfg(test)]
    pub(crate) fn previous_navigation_block(
        &self,
        role: Option<&str>,
    ) -> Option<TranscriptNavigationBlock> {
        self.previous_navigation_block_from(self.current_navigation_anchor()?, role)
    }

    #[cfg(test)]
    pub(crate) fn next_navigation_block(
        &self,
        role: Option<&str>,
    ) -> Option<TranscriptNavigationBlock> {
        self.next_navigation_block_from(self.current_navigation_anchor()?, role)
    }

    fn record_block_target_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        record_index: usize,
        row_offset: RowIndex,
        viewport_rows: u16,
    ) -> Option<(
        BlockId,
        RowIndex,
        crate::content::transcript_buf::TranscriptRowAnchor,
    )> {
        let block_id = if self.records.total_count().is_some() {
            let range =
                self.record_window_range_around_center(width, record_index, viewport_rows, true)?;
            let _ = self.activate_record_window_range(range);
            self.records
                .sparse
                .record(smelt_store::TranscriptRecordOffset::new(record_index))?
                .block_id
        } else {
            self.history().order.get(record_index).copied()?
        };

        let global_row_offset = self.approximate_sparse_prefix_row_offset(width);
        if !self.pin_operation_blocks(&[block_id]) {
            return None;
        }
        let target = self.content.projection.exact_block_row_target(
            lua,
            &mut self.content.transcript.history,
            width,
            block_id,
            row_offset,
        );
        self.unpin_operation_blocks(&[block_id]);
        let (row_anchor, local_target_row) = target?;
        Some((
            block_id,
            global_row_offset.saturating_add(local_target_row),
            row_anchor,
        ))
    }

    pub(crate) fn record_block_reveal_position(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        record_index: usize,
        row_offset: RowIndex,
        viewport_rows: u16,
    ) -> Option<TranscriptBlockRevealPosition> {
        let (block_id, target_row, row_anchor) =
            self.record_block_target_row(lua, width, record_index, row_offset, viewport_rows)?;
        Some(TranscriptBlockRevealPosition {
            block_id,
            target_row,
            row_anchor,
        })
    }

    pub(crate) fn materialize_exact_loaded_search_layout(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> crate::content::transcript_buf::TranscriptSearchLayout {
        let offset =
            self.loaded_row_offset(width, LoadedRowOffsetPolicy::RenderedViewportOrEstimate);
        let mut layout = self.content.projection.materialize_search_layout(
            lua,
            &mut self.content.transcript.history,
            width,
        );
        for entry in &mut layout.entries {
            entry.first_row = entry.first_row.saturating_add(offset);
        }
        layout
    }

    pub(crate) fn materialize_exact_loaded_search_layout_for_blocks(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        block_indices: &[u64],
    ) -> crate::content::transcript_buf::TranscriptSearchLayout {
        let hydration_ids = block_indices
            .iter()
            .copied()
            .map(BlockId::new)
            .collect::<Vec<_>>();
        if !self.pin_operation_blocks(&hydration_ids) {
            return crate::content::transcript_buf::TranscriptSearchLayout {
                generation: 0,
                entries: Vec::new(),
            };
        }
        let offset =
            self.loaded_row_offset(width, LoadedRowOffsetPolicy::RenderedViewportOrEstimate);
        let mut layout = self
            .content
            .projection
            .materialize_search_layout_for_blocks(
                lua,
                &mut self.content.transcript.history,
                width,
                block_indices,
            );
        self.unpin_operation_blocks(&hydration_ids);
        for entry in &mut layout.entries {
            entry.first_row = entry.first_row.saturating_add(offset);
        }
        layout
    }

    pub(crate) fn block_id_at_or_before_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        forward: bool,
    ) -> Option<BlockId> {
        let local_row = self.exact_loaded_row_for_virtual_content_row(lua, width, row)?;
        self.content.projection.block_id_at_or_before_row(
            lua,
            &mut self.content.transcript.history,
            width,
            local_row,
            forward,
        )
    }

    pub(crate) fn visible_block_layout(
        &self,
    ) -> impl Iterator<
        Item = (
            BlockId,
            crate::smelt_edit::RowIndex,
            crate::smelt_edit::RowIndex,
        ),
    > + '_ {
        self.content.projection.visible_block_layout()
    }

    fn record_window_count(&self, width: u16, viewport_rows: u16, total: usize) -> usize {
        let avg_rows = self.approximate_average_record_rows(width).max(1);
        let visible_records =
            (RowIndex::from(viewport_rows.max(1)) / avg_rows).saturating_add(1) as usize;
        let count = visible_records.saturating_mul(4).max(32);
        let count = if total > TRANSCRIPT_RECORD_WINDOW_MIN_RECORDS {
            count.max(TRANSCRIPT_RECORD_WINDOW_MIN_RECORDS)
        } else {
            count
        }
        .min(total);
        if count >= TRANSCRIPT_RECORD_PAGE_SIZE {
            count
                .saturating_add(TRANSCRIPT_RECORD_PAGE_SIZE - 1)
                .saturating_div(TRANSCRIPT_RECORD_PAGE_SIZE)
                .saturating_mul(TRANSCRIPT_RECORD_PAGE_SIZE)
                .min(total)
                .max(1)
        } else {
            count.max(1)
        }
    }

    fn record_window_range_for_center(
        &self,
        width: u16,
        center: usize,
        viewport_rows: u16,
        total: usize,
    ) -> Range<smelt_store::TranscriptRecordOffset> {
        let count = self.record_window_count(width, viewport_rows, total);
        let centered_start = center
            .saturating_sub(count / 2)
            .min(total.saturating_sub(count));
        let mut start = centered_start;
        if count >= TRANSCRIPT_RECORD_PAGE_SIZE {
            start = start / TRANSCRIPT_RECORD_PAGE_SIZE * TRANSCRIPT_RECORD_PAGE_SIZE;
            if center >= start.saturating_add(count) {
                start = centered_start;
            }
        }
        let end = start.saturating_add(count).min(total);
        smelt_store::TranscriptRecordOffset::new(start)
            ..smelt_store::TranscriptRecordOffset::new(end)
    }

    fn record_window_range_around_center(
        &self,
        width: u16,
        center: usize,
        viewport_rows: u16,
        reuse_active: bool,
    ) -> Option<Range<smelt_store::TranscriptRecordOffset>> {
        let total = self.records.total_count()?;
        if total == 0 {
            return None;
        }
        let center = center.min(total.saturating_sub(1));
        if reuse_active {
            if let Some(active) = self.records.active_range() {
                if active.start.get() <= center && center < active.end.get() {
                    return Some(active.clone());
                }
            }
        }
        Some(self.record_window_range_for_center(width, center, viewport_rows, total))
    }

    fn estimated_record_for_display_row(
        &mut self,
        width: u16,
        row: RowIndex,
    ) -> Option<(usize, RowIndex)> {
        let store = self
            .store_cache
            .store_for_session(self.records.extent_session_dir());
        let model = self.extent_index.record_extent_model(
            &self.records.sparse,
            self.records.active_range(),
            store,
            width,
        );
        self.extent_index.estimated_record_for_row(&model, row)
    }

    fn record_window_range_for_approximate_display_row(
        &mut self,
        width: u16,
        row: RowIndex,
        viewport_rows: u16,
    ) -> Option<Range<smelt_store::TranscriptRecordOffset>> {
        let total = self.records.total_count()?;
        let center = self
            .estimated_record_for_display_row(width, row)
            .map(|(record_index, _)| record_index)
            .unwrap_or_else(|| {
                let avg_rows = self.approximate_average_record_rows(width).max(1);
                ((row / avg_rows) as usize).min(total.saturating_sub(1))
            });
        self.record_window_range_around_center(width, center, viewport_rows, true)
    }

    fn semantic_far_seek_record_window_range(
        &self,
        width: u16,
        record_index: usize,
        viewport_rows: u16,
    ) -> Option<Range<smelt_store::TranscriptRecordOffset>> {
        let total = self.records.total_count()?;
        if total == 0 {
            return None;
        }
        let start = record_index.min(total.saturating_sub(1));
        let count = self.record_window_count(width, viewport_rows, total);
        let end = start.saturating_add(count).min(total);
        Some(
            smelt_store::TranscriptRecordOffset::new(start)
                ..smelt_store::TranscriptRecordOffset::new(end),
        )
    }

    fn activate_semantic_far_seek_record_window(
        &mut self,
        width: u16,
        record_index: usize,
        viewport_rows: u16,
    ) -> bool {
        let Some(projection_range) =
            self.semantic_far_seek_record_window_range(width, record_index, viewport_rows)
        else {
            return false;
        };
        let cache_range = if self
            .records
            .total_count()
            .is_some_and(|total| total > TRANSCRIPT_RECORD_WINDOW_MIN_RECORDS)
        {
            self.records.sparse.cache_range_around(&projection_range)
        } else {
            projection_range.clone()
        };
        self.activate_record_projection_range(projection_range, cache_range)
    }

    fn resolve_semantic_far_seek(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        scroll_top: RowIndex,
        total_rows: RowIndex,
    ) -> Option<TranscriptSemanticFarSeek> {
        let (record_index, in_record_row) =
            self.estimated_record_for_display_row(width, scroll_top)?;
        let _ = self.activate_semantic_far_seek_record_window(width, record_index, viewport_rows);
        let record = self
            .records
            .sparse
            .record(smelt_store::TranscriptRecordOffset::new(record_index))?;
        let block_id = record.block_id;
        let row_anchor = self.content.projection.row_anchor_for_block(
            lua,
            &mut self.content.transcript.history,
            width,
            block_id,
            in_record_row,
        )?;
        Some(TranscriptSemanticFarSeek {
            scroll_top,
            total_rows,
            row_anchor,
        })
    }

    fn tail_record_window_range(
        &self,
        width: u16,
        viewport_rows: u16,
    ) -> Option<Range<smelt_store::TranscriptRecordOffset>> {
        let total = self.records.total_count()?;
        if let Some(active) = self.records.active_range() {
            if active.end.get() == total {
                return Some(active.clone());
            }
        }
        self.record_window_range_around_center(width, total.saturating_sub(1), viewport_rows, false)
    }

    fn activate_record_window_range(
        &mut self,
        projection_range: Range<smelt_store::TranscriptRecordOffset>,
    ) -> bool {
        let mut cache_range = if self
            .records
            .total_count()
            .is_some_and(|total| total > TRANSCRIPT_RECORD_WINDOW_MIN_RECORDS)
        {
            self.records.sparse.cache_range_around(&projection_range)
        } else {
            projection_range.clone()
        };
        if self.records.session_dir().is_none()
            && !self.records.sparse.range_is_loaded(&cache_range)
        {
            cache_range = projection_range.clone();
        }
        self.activate_record_projection_range(projection_range, cache_range)
    }

    fn activate_record_projection_range(
        &mut self,
        projection_range: Range<smelt_store::TranscriptRecordOffset>,
        cache_range: Range<smelt_store::TranscriptRecordOffset>,
    ) -> bool {
        if self.records.active_range() == Some(&projection_range)
            && self.records.sparse.range_is_loaded(&projection_range)
        {
            return false;
        }
        let missing_ranges = self.records.sparse.missing_ranges(&cache_range);
        let mut loaded_any = false;
        for missing in missing_ranges {
            let Some(window) =
                self.load_record_window((missing.start.get()..missing.end.get()).into())
            else {
                return false;
            };
            loaded_any |= self.merge_record_cache_window(&window);
        }
        if !self.records.sparse.range_is_loaded(&projection_range) {
            return false;
        }
        let active_changed = self.records.active_range() != Some(&projection_range);
        if !active_changed && !loaded_any {
            return false;
        }
        self.records.active_range = Some(projection_range.clone());
        self.records.sparse.touch_range(&cache_range);
        self.records
            .sparse
            .enforce_byte_budget(&projection_range, self.memory_budget.record_windows);
        self.install_active_record_projection();
        true
    }

    fn activate_record_window_for_approximate_display_row(
        &mut self,
        width: u16,
        row: RowIndex,
        viewport_rows: u16,
    ) -> bool {
        let Some(range) =
            self.record_window_range_for_approximate_display_row(width, row, viewport_rows)
        else {
            return false;
        };
        self.activate_record_window_range(range)
    }

    fn active_virtual_row_span(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> Option<(RowIndex, RowIndex)> {
        let loaded_rows = self.content.projection.estimated_total_rows(
            lua,
            &mut self.content.transcript.history,
            width,
        );
        if self.records.active_range().is_none() {
            return self
                .records
                .sparse
                .total_count()
                .is_none()
                .then_some((0, loaded_rows));
        }
        let row_offset = self.loaded_row_offset(width, LoadedRowOffsetPolicy::SparseEstimate);
        Some((row_offset, row_offset.saturating_add(loaded_rows)))
    }

    fn record_window_expanded_toward_row(
        &self,
        width: u16,
        viewport_rows: u16,
        row: RowIndex,
        loaded_start: RowIndex,
        loaded_end: RowIndex,
    ) -> Option<Range<smelt_store::TranscriptRecordOffset>> {
        let active = self.records.active_range.clone()?;
        let total = self.records.total_count()?;
        let avg_rows = self.approximate_average_record_rows(width).max(1);
        let missing_rows = if row < loaded_start {
            loaded_start.saturating_sub(row)
        } else {
            row.saturating_sub(loaded_end).saturating_add(1)
        };
        let missing_records = missing_rows
            .saturating_add(avg_rows.saturating_sub(1))
            .saturating_div(avg_rows)
            .max(TRANSCRIPT_RECORD_PAGE_SIZE as RowIndex) as usize;
        let max_records = self
            .record_window_count(width, viewport_rows, total)
            .saturating_mul(TRANSCRIPT_ACTIVE_RECORD_WINDOW_MAX_MULTIPLIER)
            .min(total);
        let (mut start, mut end) = if row < loaded_start {
            (
                active.start.get().saturating_sub(missing_records),
                active.end.get(),
            )
        } else {
            (
                active.start.get(),
                active.end.get().saturating_add(missing_records).min(total),
            )
        };
        if row < loaded_start {
            start = start / TRANSCRIPT_RECORD_PAGE_SIZE * TRANSCRIPT_RECORD_PAGE_SIZE;
        } else {
            end = end
                .saturating_add(TRANSCRIPT_RECORD_PAGE_SIZE - 1)
                .saturating_div(TRANSCRIPT_RECORD_PAGE_SIZE)
                .saturating_mul(TRANSCRIPT_RECORD_PAGE_SIZE)
                .min(total);
        }
        if end.saturating_sub(start) > max_records {
            if row < loaded_start {
                end = start.saturating_add(max_records).min(total);
            } else {
                start = end.saturating_sub(max_records);
            }
        }
        let range = smelt_store::TranscriptRecordOffset::new(start)
            ..smelt_store::TranscriptRecordOffset::new(end);
        (self.records.active_range() != Some(&range)).then_some(range)
    }

    fn activate_record_window_covering_approximate_display_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
        viewport_rows: u16,
    ) {
        let viewport_row_count = RowIndex::from(viewport_rows.max(1));
        let _ = self.activate_record_window_for_approximate_display_row(width, row, viewport_rows);
        while let Some((loaded_start, loaded_end)) = self.active_virtual_row_span(lua, width) {
            let target_end = row.saturating_add(viewport_row_count);
            if row >= loaded_start && target_end <= loaded_end {
                return;
            }
            let edge = if row < loaded_start { row } else { target_end };
            let Some(range) = self.record_window_expanded_toward_row(
                width,
                viewport_rows,
                edge,
                loaded_start,
                loaded_end,
            ) else {
                return;
            };
            if !self.activate_record_window_range(range) {
                return;
            }
        }
    }

    fn activate_tail_record_window(&mut self, width: u16, viewport_rows: u16) -> bool {
        let Some(range) = self.tail_record_window_range(width, viewport_rows) else {
            return false;
        };
        self.activate_record_window_range(range)
    }

    fn scroll_anchor_for_projection_target(
        &self,
        scroll_target: crate::content::transcript_buf::ScrollTarget,
    ) -> TranscriptScrollAnchor {
        match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => TranscriptScrollAnchor::Tail,
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ExactRow(row)
                | crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(row),
            ) => TranscriptScrollAnchor::EstimatedRow(row),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::StableRowDelta { row, delta, .. },
            ) => TranscriptScrollAnchor::EstimatedRow(add_signed_row(row, delta)),
        }
    }

    fn stable_viewport_anchor(&self, anchor: TranscriptContentAnchor) -> bool {
        self.history().is_stable_scroll_anchor(anchor.block_id)
    }

    fn current_block_id_for_content_anchor(&self, anchor: TranscriptContentAnchor) -> BlockId {
        self.records
            .sparse
            .record(smelt_store::TranscriptRecordOffset::new(
                anchor.record_index,
            ))
            .map(|record| record.block_id)
            .unwrap_or(anchor.block_id)
    }

    fn stable_row_anchor_for_content_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: TranscriptContentAnchor,
    ) -> Option<crate::content::transcript_buf::StableRowAnchor> {
        let block_id = self.current_block_id_for_content_anchor(anchor);
        self.content
            .projection
            .row_anchor_for_block(
                lua,
                &mut self.content.transcript.history,
                width,
                block_id,
                anchor.intra_block_row,
            )
            .map(Into::into)
    }

    fn capture_fallback_content_anchor_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        top_row: RowIndex,
        viewport_rows: u16,
        mut anchor: TranscriptContentAnchor,
        row_offset: RowIndex,
    ) -> Option<(Option<TranscriptScrollAnchor>, isize)> {
        let anchor_row = self.row_for_content_anchor_with_offset(lua, width, anchor, row_offset)?;
        let viewport_rows = RowIndex::from(viewport_rows.max(1));
        let visible_end = top_row.saturating_add(viewport_rows);
        let anchor_window_end = anchor_row.saturating_add(viewport_rows);
        if anchor_row >= visible_end || top_row >= anchor_window_end {
            return None;
        }
        let offset = signed_row_delta(anchor_row, top_row);
        anchor.fallback_row = anchor_row;
        Some((Some(TranscriptScrollAnchor::Content(anchor)), offset))
    }

    fn capture_viewport_anchor_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        top_row: RowIndex,
        viewport_rows: u16,
        row_offset: RowIndex,
        fallback: TranscriptScrollAnchor,
    ) {
        let (top_anchor, top_offset_rows) = match fallback {
            TranscriptScrollAnchor::Tail => (Some(TranscriptScrollAnchor::Tail), 0),
            TranscriptScrollAnchor::Content(anchor) => self
                .capture_fallback_content_anchor_with_offset(
                    lua,
                    width,
                    top_row,
                    viewport_rows,
                    anchor,
                    row_offset,
                )
                .or_else(|| {
                    self.content_anchor_at_or_after_row_with_offset(
                        lua,
                        width,
                        top_row,
                        viewport_rows,
                        row_offset,
                    )
                    .and_then(|(anchor, offset)| {
                        self.stable_viewport_anchor(anchor)
                            .then_some((Some(TranscriptScrollAnchor::Content(anchor)), offset))
                    })
                })
                .unwrap_or((Some(TranscriptScrollAnchor::Content(anchor)), 0)),
            TranscriptScrollAnchor::EstimatedRow(_) => self
                .content_anchor_at_or_after_row_with_offset(
                    lua,
                    width,
                    top_row,
                    viewport_rows,
                    row_offset,
                )
                .and_then(|(anchor, offset)| {
                    self.stable_viewport_anchor(anchor)
                        .then_some((Some(TranscriptScrollAnchor::Content(anchor)), offset))
                })
                .unwrap_or((Some(fallback), 0)),
        };
        self.viewport.state.mode = match top_anchor {
            Some(TranscriptScrollAnchor::Tail) => TranscriptViewportMode::Tail,
            Some(TranscriptScrollAnchor::Content(_)) => TranscriptViewportMode::Anchored,
            Some(TranscriptScrollAnchor::EstimatedRow(_)) | None => TranscriptViewportMode::FarSeek,
        };
        self.viewport.state.resolved_anchor =
            top_anchor.map(|top| TranscriptResolvedViewportAnchor {
                top,
                offset_rows: top_offset_rows,
                scroll_top: top_row,
            });
    }

    fn take_viewport_intent(
        &mut self,
        input: TranscriptViewportProjectionInput,
    ) -> TranscriptViewportIntent {
        if let Some(pending) = self.viewport.state.pending_projection.take() {
            return TranscriptViewportIntent {
                intent: pending.intent,
                hint: pending.hint,
            };
        }
        let intent = if input.follow_tail {
            TranscriptScrollIntent::Tail
        } else if input.width_changed {
            TranscriptScrollIntent::ResizeReflow {
                previous_width: input.previous_width.unwrap_or_default(),
            }
        } else {
            TranscriptScrollIntent::PreserveViewport
        };
        TranscriptViewportIntent { intent, hint: None }
    }

    fn max_scroll_for_total(total_rows: RowIndex, viewport_rows: u16) -> RowIndex {
        total_rows.saturating_sub(RowIndex::from(viewport_rows.max(1)))
    }

    fn clamp_viewport_anchor_offset(offset_rows: isize, viewport_rows: u16) -> isize {
        let max_offset = viewport_rows.saturating_sub(1) as isize;
        offset_rows.clamp(-max_offset, max_offset)
    }

    fn semantic_far_seek_for_intent(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        intent: &TranscriptScrollIntent,
    ) -> Option<TranscriptSemanticFarSeek> {
        let (scroll_top, total_rows) = match intent {
            TranscriptScrollIntent::ScrollbarFraction {
                numerator,
                denominator,
                total_rows,
                viewport_rows: gesture_viewport_rows,
            } => {
                let denominator = (*denominator).max(1);
                if *numerator >= denominator {
                    return None;
                }
                let total_rows = (*total_rows).max(RowIndex::from((*gesture_viewport_rows).max(1)));
                let max_scroll =
                    Self::max_scroll_for_total(total_rows, (*gesture_viewport_rows).max(1));
                let scroll_top = ((*numerator).min(denominator) as u128)
                    .saturating_mul(max_scroll as u128)
                    .checked_div(denominator as u128)
                    .unwrap_or(0)
                    .min(RowIndex::MAX as u128) as RowIndex;
                (scroll_top, total_rows)
            }
            TranscriptScrollIntent::ApproximateRowSeek(row) => {
                let total_rows = self.approximate_scrollbar_total_for_viewport(lua, width);
                (
                    (*row).min(Self::max_scroll_for_total(total_rows, viewport_rows)),
                    total_rows,
                )
            }
            _ => return None,
        };
        self.resolve_semantic_far_seek(lua, width, viewport_rows, scroll_top, total_rows)
    }

    fn semantic_tail_record_is_materialized(&self) -> bool {
        let Some(total) = self.records.total_count() else {
            return true;
        };
        if total == 0 {
            return true;
        }
        let Some(active) = self.records.active_range() else {
            return false;
        };
        if active.end.get() < total {
            return false;
        }
        self.records
            .sparse
            .record(smelt_store::TranscriptRecordOffset::new(
                total.saturating_sub(1),
            ))
            .is_some_and(|record| {
                self.content
                    .transcript
                    .history
                    .is_materialized(record.block_id)
            })
    }

    fn projected_viewport_reached_semantic_tail(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        rows: crate::smelt_edit::MaterializedRows,
        viewport_rows: u16,
        row_offset: RowIndex,
        local_total_rows: RowIndex,
    ) -> bool {
        let visible_rows = RowIndex::from(viewport_rows.max(1));
        if rows.clamped_scroll.saturating_add(visible_rows) < rows.total_rows {
            return false;
        }
        if let Some(total) = self.records.total_count() {
            if !self.semantic_tail_record_is_materialized() {
                return false;
            }
            let Some(active) = self.records.active_range() else {
                return false;
            };
            let local_scroll = rows.clamped_scroll.saturating_sub(row_offset);
            if active.end.get() < total
                || local_scroll.saturating_add(visible_rows) < local_total_rows
            {
                return false;
            }
            return (0..visible_rows).rev().any(|offset| {
                self.content_anchor_at_row_with_offset(
                    lua,
                    width,
                    rows.clamped_scroll.saturating_add(offset),
                    row_offset,
                    TranscriptAnchorBias::Top,
                )
                .is_some_and(|anchor| {
                    anchor.record_index.saturating_add(1) == total
                        && self
                            .content
                            .projection
                            .height_suffix_is_exact(anchor.row_anchor)
                })
            });
        }
        let row_offset = self.loaded_row_offset(width, LoadedRowOffsetPolicy::SparseEstimate);
        let loaded_rows = self.content.projection.estimated_total_rows(
            lua,
            &mut self.content.transcript.history,
            width,
        );
        let tail_row = row_offset.saturating_add(loaded_rows);
        rows.clamped_scroll.saturating_add(visible_rows) >= tail_row
    }

    fn projected_scroll_state(&self, reached_semantic_tail: bool) -> VerticalScroll {
        if self.viewport.state.mode == TranscriptViewportMode::Tail || reached_semantic_tail {
            VerticalScroll::Tail
        } else {
            VerticalScroll::Pinned
        }
    }

    fn row_for_trace_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        anchor: TranscriptTraceAnchor,
    ) -> Option<RowIndex> {
        match anchor {
            TranscriptTraceAnchor::Tail => None,
            TranscriptTraceAnchor::EstimatedRow(row) => Some(row),
            TranscriptTraceAnchor::Content {
                record_index,
                node_id,
                row_offset,
                ..
            } => {
                if self.records.total_count().is_some() {
                    let range = self.record_window_range_around_center(
                        width,
                        record_index,
                        viewport_rows,
                        true,
                    )?;
                    let _ = self.activate_record_window_range(range);
                }
                self.row_for_anchor(
                    lua,
                    width,
                    TranscriptRowAnchor {
                        id: node_id,
                        row_offset,
                    },
                )
            }
        }
    }

    fn row_for_search_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        anchor: TranscriptSearchAnchor,
        start_byte_col: usize,
        hint: Option<TranscriptProjectionHint>,
    ) -> Option<RowIndex> {
        match anchor {
            TranscriptSearchAnchor::Content(node_anchor) => {
                if self.records.total_count().is_some() {
                    let range = self.record_window_range_around_center(
                        width,
                        node_anchor.record_index,
                        viewport_rows,
                        true,
                    )?;
                    let _ = self.activate_record_window_range(range);
                }
                let hinted_row = match hint {
                    Some(TranscriptProjectionHint::SearchProjectedRow {
                        width: hint_width,
                        anchor: hint_anchor,
                        start_byte_col: hint_start_byte_col,
                        row,
                    }) if hint_width.max(1) == width.max(1)
                        && hint_anchor == anchor
                        && hint_start_byte_col == start_byte_col =>
                    {
                        Some(row)
                    }
                    _ => None,
                };
                self.row_for_node_anchor(lua, width, viewport_rows, node_anchor)
                    .or(hinted_row)
            }
            TranscriptSearchAnchor::EstimatedRow(row) => Some(row),
        }
    }

    fn row_for_viewport_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        fallback_scroll_top: RowIndex,
    ) -> Option<RowIndex> {
        let Some(anchor) = self.viewport.state.resolved_anchor else {
            return Some(fallback_scroll_top);
        };
        match anchor.top {
            TranscriptScrollAnchor::Tail => Some(fallback_scroll_top),
            TranscriptScrollAnchor::Content(content) => {
                self.row_for_content_anchor(lua, width, viewport_rows, content)
            }
            TranscriptScrollAnchor::EstimatedRow(row) => Some(row),
        }
        .map(|row| {
            add_signed_row(
                row,
                Self::clamp_viewport_anchor_offset(anchor.offset_rows, viewport_rows),
            )
        })
    }

    fn approximate_scrollbar_total_for_viewport(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> RowIndex {
        let loaded_rows = self.content.projection.estimated_total_rows(
            lua,
            &mut self.content.transcript.history,
            width,
        );
        self.scrollbar_total_rows(width, loaded_rows)
    }

    fn activate_record_window_for_local_delta(
        &mut self,
        width: u16,
        viewport_rows: u16,
        anchor_record: usize,
        rows: isize,
    ) -> bool {
        let Some(active) = self.records.active_range().cloned() else {
            return false;
        };
        let Some(total) = self.records.total_count() else {
            return false;
        };
        if rows == 0 || anchor_record < active.start.get() || anchor_record >= active.end.get() {
            return false;
        }
        let base_window_count = self.record_window_count(width, viewport_rows, total);
        let window_count = base_window_count
            .saturating_mul(TRANSCRIPT_ACTIVE_RECORD_WINDOW_MAX_MULTIPLIER)
            .min(base_window_count.max(TRANSCRIPT_RECORD_WINDOW_MIN_RECORDS))
            .min(total);
        let opposite_context = window_count.saturating_div(4).max(1);
        let start = if rows < 0 {
            anchor_record
                .saturating_add(1)
                .saturating_add(opposite_context)
                .saturating_sub(window_count)
        } else {
            anchor_record.saturating_sub(opposite_context)
        }
        .min(total.saturating_sub(window_count));
        let end = start.saturating_add(window_count).min(total);
        let projection_range = smelt_store::TranscriptRecordOffset::new(start)
            ..smelt_store::TranscriptRecordOffset::new(end);
        let cache_range = if self.records.session_dir().is_some()
            && total > TRANSCRIPT_RECORD_WINDOW_MIN_RECORDS
        {
            self.records.sparse.cache_range_around(&projection_range)
        } else {
            projection_range.clone()
        };
        self.activate_record_projection_range(projection_range, cache_range)
    }

    fn local_delta_needs_record_expansion(
        &self,
        tape: Option<crate::content::transcript_buf::ExactRowTapeState>,
        viewport_rows: u16,
        rows: isize,
        anchor_record: Option<usize>,
    ) -> bool {
        let (Some(active), Some(total)) = (self.records.active_range(), self.records.total_count())
        else {
            return false;
        };
        let active_start = active.start.get();
        let active_end = active.end.get();
        // Unmeasured nodes can compress the estimated row tail. Require the semantic
        // anchor to approach the same record edge before rotating the loaded window.
        let record_guard = active_end
            .saturating_sub(active_start)
            .saturating_div(4)
            .max(1);
        let near_record_edge = anchor_record.is_none_or(|record| {
            (rows < 0 && record.saturating_sub(active_start) <= record_guard)
                || (rows > 0 && active_end.saturating_sub(record) <= record_guard)
        });
        if !near_record_edge {
            return false;
        }
        let Some(tape) = tape else {
            return true;
        };
        let target = add_signed_row(tape.rows.clamped_scroll, rows);
        let viewport_rows = RowIndex::from(viewport_rows.max(1));
        let guard_rows = viewport_rows.saturating_mul(TRANSCRIPT_LOCAL_PAGE_GUARD_VIEWPORTS);
        (rows < 0 && active_start > 0 && target <= guard_rows)
            || (rows > 0
                && active_end < total
                && target
                    .saturating_add(viewport_rows)
                    .saturating_add(guard_rows)
                    >= tape.rows.total_rows)
    }

    fn local_delta_scroll_target(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        fallback_scroll_top: RowIndex,
        rows: isize,
    ) -> crate::content::transcript_buf::ScrollTarget {
        let exact_viewport =
            self.exact_viewport_state(lua, width, viewport_rows)
                .filter(|(exact, tape)| {
                    tape.rows.clamped_scroll.saturating_add(exact.row_offset) == fallback_scroll_top
                });
        let current_content_anchor_matches = matches!(
            self.viewport.state.resolved_anchor,
            Some(TranscriptResolvedViewportAnchor {
                top: TranscriptScrollAnchor::Content(_),
                scroll_top,
                ..
            }) if scroll_top == fallback_scroll_top
        );
        if !current_content_anchor_matches {
            if let Some((exact, _)) = exact_viewport {
                self.capture_viewport_anchor_with_offset(
                    lua,
                    width,
                    fallback_scroll_top,
                    viewport_rows,
                    exact.row_offset,
                    TranscriptScrollAnchor::EstimatedRow(fallback_scroll_top),
                );
            }
        }
        let semantic_viewport_matches = self
            .viewport
            .state
            .resolved_anchor
            .is_some_and(|anchor| anchor.scroll_top == fallback_scroll_top);
        let exact_anchor = exact_viewport.and_then(|(_, tape)| tape.top_anchor);
        let (semantic_anchor, semantic_offset_rows) = match self.viewport.state.resolved_anchor {
            Some(TranscriptResolvedViewportAnchor {
                top: TranscriptScrollAnchor::Content(anchor),
                offset_rows,
                ..
            }) if semantic_viewport_matches => (
                Some(anchor),
                Self::clamp_viewport_anchor_offset(offset_rows, viewport_rows),
            ),
            _ => (None, 0),
        };
        let semantic_anchor_row = semantic_anchor
            .and_then(|anchor| self.row_for_content_anchor(lua, width, viewport_rows, anchor));
        let base = exact_viewport
            .map(|(exact, tape)| tape.rows.clamped_scroll.saturating_add(exact.row_offset))
            .or_else(|| {
                semantic_viewport_matches
                    .then(|| {
                        semantic_anchor_row.map(|row| add_signed_row(row, semantic_offset_rows))
                    })
                    .flatten()
            })
            .unwrap_or(fallback_scroll_top);
        let anchor_record = semantic_anchor
            .map(|anchor| anchor.record_index)
            .or_else(|| {
                exact_anchor
                    .and_then(|anchor| anchor.block_id())
                    .and_then(|block_id| self.record_index_for_block_id(block_id))
            });
        let needs_record_expansion = self.local_delta_needs_record_expansion(
            exact_viewport.map(|(_, tape)| tape),
            viewport_rows,
            rows,
            anchor_record,
        );
        if needs_record_expansion {
            if let Some(anchor_record) = anchor_record {
                self.activate_record_window_for_local_delta(
                    width,
                    viewport_rows,
                    anchor_record,
                    rows,
                );
            }
        }
        let reanchored_semantic_anchor = semantic_anchor.map(|anchor| {
            let block_id = self.current_block_id_for_content_anchor(anchor);
            self.content
                .projection
                .row_anchor_for_block(
                    lua,
                    &mut self.content.transcript.history,
                    width,
                    block_id,
                    anchor.intra_block_row,
                )
                .map(Into::into)
                .unwrap_or_else(|| {
                    crate::content::transcript_buf::StableRowAnchor::rendered_block_row(
                        block_id,
                        anchor.intra_block_row,
                    )
                })
        });
        let content_exact_anchor = exact_anchor.filter(|anchor| anchor.block_id().is_some());
        let present_exact_anchor = content_exact_anchor.and_then(|anchor| {
            self.content
                .projection
                .stable_anchor_is_present(lua, &mut self.content.transcript.history, width, anchor)
                .then_some(anchor)
        });
        let semantic_stable_anchor = reanchored_semantic_anchor
            .or_else(|| semantic_anchor.map(|anchor| anchor.row_anchor.into()));
        let stable_anchor = present_exact_anchor
            .or(semantic_stable_anchor)
            .or(content_exact_anchor)
            .or(exact_anchor);
        let (stable_row, stable_delta) =
            if present_exact_anchor.is_none() && semantic_stable_anchor.is_some() {
                (
                    semantic_anchor_row.unwrap_or(base),
                    rows.saturating_add(semantic_offset_rows),
                )
            } else {
                (base, rows)
            };
        crate::content::transcript_buf::ScrollTarget::visible_stable_anchor_delta(
            stable_row,
            stable_anchor,
            stable_delta,
        )
    }

    fn scroll_target_for_intent(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        fallback_scroll_top: RowIndex,
        intent: &TranscriptScrollIntent,
        hint: Option<TranscriptProjectionHint>,
    ) -> (
        crate::content::transcript_buf::ScrollTarget,
        Option<TranscriptCursorTarget>,
    ) {
        match intent {
            TranscriptScrollIntent::Tail => (
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                None,
            ),
            TranscriptScrollIntent::PreserveViewport => {
                let active_record_range = self.active_record_range_key();
                let exact_anchor = self
                    .exact_viewport_state(lua, width, viewport_rows)
                    .filter(|(exact, tape)| {
                        exact.active_record_range == active_record_range
                            && tape.rows.clamped_scroll.saturating_add(exact.row_offset)
                                == fallback_scroll_top
                    })
                    .and_then(|(_, tape)| tape.top_anchor);
                if let Some(anchor) = exact_anchor {
                    return (
                        crate::content::transcript_buf::ScrollTarget::visible_stable_anchor_delta(
                            fallback_scroll_top,
                            Some(anchor),
                            0,
                        ),
                        None,
                    );
                }
                if let Some(TranscriptResolvedViewportAnchor {
                    top: TranscriptScrollAnchor::Content(anchor),
                    offset_rows,
                    ..
                }) = self.viewport.state.resolved_anchor
                {
                    if self
                        .row_for_preserved_content_anchor(lua, width, viewport_rows, anchor)
                        .is_some()
                    {
                        if let Some(stable_anchor) =
                            self.stable_row_anchor_for_content_anchor(lua, width, anchor)
                        {
                            return (
                                crate::content::transcript_buf::ScrollTarget::visible_stable_anchor_delta(
                                    anchor.fallback_row,
                                    Some(stable_anchor),
                                    Self::clamp_viewport_anchor_offset(offset_rows, viewport_rows),
                                ),
                                None,
                            );
                        }
                    }
                }
                let target = match self.row_for_viewport_anchor(
                    lua,
                    width,
                    viewport_rows,
                    fallback_scroll_top,
                ) {
                    Some(row) => crate::content::transcript_buf::ScrollTarget::visible_row(row),
                    None => crate::content::transcript_buf::ScrollTarget::visible_tail(),
                };
                (target, None)
            }
            TranscriptScrollIntent::ResizeReflow { .. } => {
                let target = match self.viewport.state.resolved_anchor {
                    Some(TranscriptResolvedViewportAnchor {
                        top: TranscriptScrollAnchor::Content(_),
                        scroll_top,
                        ..
                    }) if self.records.total_count().is_none() => {
                        crate::content::transcript_buf::ScrollTarget::visible_reflow_stable_row(
                            scroll_top,
                        )
                    }
                    Some(TranscriptResolvedViewportAnchor {
                        top: TranscriptScrollAnchor::Content(anchor),
                        offset_rows,
                        ..
                    }) if self.records.total_count().is_some() => {
                        match self.row_for_preserved_content_anchor(
                            lua,
                            width,
                            viewport_rows,
                            anchor,
                        ) {
                            Some(row) => match self.stable_row_anchor_for_content_anchor(
                                lua,
                                width,
                                anchor,
                            ) {
                                Some(stable_anchor) => {
                                    crate::content::transcript_buf::ScrollTarget::visible_stable_anchor_delta(
                                        row,
                                        Some(stable_anchor),
                                        Self::clamp_viewport_anchor_offset(offset_rows, viewport_rows),
                                    )
                                }
                                None => crate::content::transcript_buf::ScrollTarget::visible_tail(),
                            },
                            None => crate::content::transcript_buf::ScrollTarget::visible_tail(),
                        }
                    }
                    Some(TranscriptResolvedViewportAnchor {
                        top: TranscriptScrollAnchor::EstimatedRow(row),
                        offset_rows,
                        ..
                    }) if self.records.total_count().is_some() => {
                        crate::content::transcript_buf::ScrollTarget::visible_row(add_signed_row(
                            row,
                            Self::clamp_viewport_anchor_offset(offset_rows, viewport_rows),
                        ))
                    }
                    Some(TranscriptResolvedViewportAnchor {
                        top: TranscriptScrollAnchor::Tail,
                        offset_rows,
                        ..
                    }) if self.records.total_count().is_some() => {
                        crate::content::transcript_buf::ScrollTarget::visible_row(add_signed_row(
                            fallback_scroll_top,
                            Self::clamp_viewport_anchor_offset(offset_rows, viewport_rows),
                        ))
                    }
                    None if self.records.total_count().is_some() => {
                        crate::content::transcript_buf::ScrollTarget::visible_row(
                            fallback_scroll_top,
                        )
                    }
                    _ => crate::content::transcript_buf::ScrollTarget::visible_reflow_stable_row(
                        fallback_scroll_top,
                    ),
                };
                (target, None)
            }
            TranscriptScrollIntent::UserDelta { rows } => (
                self.local_delta_scroll_target(
                    lua,
                    width,
                    viewport_rows,
                    fallback_scroll_top,
                    *rows,
                ),
                None,
            ),
            TranscriptScrollIntent::PageDelta { pages } => {
                let rows = pages.saturating_mul(viewport_rows.max(1) as isize);
                (
                    self.local_delta_scroll_target(
                        lua,
                        width,
                        viewport_rows,
                        fallback_scroll_top,
                        rows,
                    ),
                    None,
                )
            }
            TranscriptScrollIntent::ExactContentAnchor(anchor) => {
                let Some(row) = self.row_for_trace_anchor(lua, width, viewport_rows, *anchor)
                else {
                    return (
                        crate::content::transcript_buf::ScrollTarget::visible_tail(),
                        None,
                    );
                };
                if matches!(*anchor, TranscriptTraceAnchor::EstimatedRow(_)) {
                    self.activate_record_window_covering_approximate_display_row(
                        lua,
                        width,
                        row,
                        viewport_rows,
                    );
                }
                (
                    crate::content::transcript_buf::ScrollTarget::visible_reflow_stable_row(row),
                    None,
                )
            }
            TranscriptScrollIntent::SearchJump {
                anchor,
                target_screen_row,
                match_start_byte_col,
                match_end_byte_col,
            } => {
                let Some(row) = self.row_for_search_anchor(
                    lua,
                    width,
                    viewport_rows,
                    *anchor,
                    *match_start_byte_col,
                    hint,
                ) else {
                    return (
                        crate::content::transcript_buf::ScrollTarget::visible_tail(),
                        None,
                    );
                };
                if matches!(*anchor, TranscriptSearchAnchor::EstimatedRow(_)) {
                    self.activate_record_window_covering_approximate_display_row(
                        lua,
                        width,
                        row,
                        viewport_rows,
                    );
                }
                let target_screen_row =
                    (*target_screen_row).min(viewport_rows.saturating_sub(1) as RowIndex);
                (
                    crate::content::transcript_buf::ScrollTarget::visible_row(
                        row.saturating_sub(target_screen_row),
                    ),
                    Some(TranscriptCursorTarget {
                        anchor: *anchor,
                        start_byte_col: *match_start_byte_col,
                        end_byte_col: *match_end_byte_col,
                    }),
                )
            }
            TranscriptScrollIntent::RevealBlock {
                record_index,
                block_id,
                row_offset,
                screen_padding_top,
            } => {
                let Some(reveal) = self.record_block_reveal_position(
                    lua,
                    width,
                    *record_index,
                    *row_offset,
                    viewport_rows,
                ) else {
                    return (
                        crate::content::transcript_buf::ScrollTarget::visible_tail(),
                        None,
                    );
                };
                if reveal.block_id != *block_id {
                    return (
                        crate::content::transcript_buf::ScrollTarget::visible_tail(),
                        None,
                    );
                }
                let screen_padding_top = (*screen_padding_top).min(isize::MAX as RowIndex) as isize;
                (
                    crate::content::transcript_buf::ScrollTarget::visible_stable_row_delta(
                        reveal.target_row,
                        Some(reveal.row_anchor),
                        -screen_padding_top,
                    ),
                    None,
                )
            }
            TranscriptScrollIntent::ScrollbarFraction {
                numerator,
                denominator,
                total_rows,
                viewport_rows: gesture_viewport_rows,
            } => {
                let denominator = (*denominator).max(1);
                if *numerator >= denominator {
                    return (
                        crate::content::transcript_buf::ScrollTarget::visible_tail(),
                        None,
                    );
                }
                let max_scroll =
                    Self::max_scroll_for_total(*total_rows, (*gesture_viewport_rows).max(1));
                let row = ((*numerator).min(denominator) as u128)
                    .saturating_mul(max_scroll as u128)
                    .checked_div(denominator as u128)
                    .unwrap_or(0)
                    .min(RowIndex::MAX as u128) as RowIndex;
                (
                    crate::content::transcript_buf::ScrollTarget::visible_row(row),
                    None,
                )
            }
            TranscriptScrollIntent::ApproximateRowSeek(row) => (
                crate::content::transcript_buf::ScrollTarget::visible_row(*row),
                None,
            ),
        }
    }

    fn resolve_cursor_target(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row_offset: RowIndex,
        target: TranscriptCursorTarget,
    ) -> Option<DocRange> {
        if target.start_byte_col > target.end_byte_col {
            return None;
        }
        let TranscriptSearchAnchor::Content(anchor) = target.anchor else {
            return None;
        };
        let row = self.row_for_node_anchor_with_offset(lua, width, anchor, row_offset)?;
        Some(DocRange {
            start: DocPosition {
                row,
                byte_col: target.start_byte_col,
            },
            end: DocPosition {
                row,
                byte_col: target.end_byte_col,
            },
        })
    }

    fn plan_exact_local_delta(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        fallback_scroll_top: RowIndex,
        rows: isize,
    ) -> Option<TranscriptProjectionPlan> {
        let (exact, tape) = self.exact_viewport_state(lua, width, viewport_rows)?;
        if tape.rows.clamped_scroll.saturating_add(exact.row_offset) != fallback_scroll_top {
            return None;
        }
        let anchor_record = tape
            .top_anchor
            .and_then(|anchor| anchor.block_id())
            .and_then(|block_id| self.record_index_for_block_id(block_id));
        if self.local_delta_needs_record_expansion(Some(tape), viewport_rows, rows, anchor_record) {
            return None;
        }
        let inner = self.content.projection.plan_exact_row_tape_scroll(
            lua,
            &self.content.transcript.history,
            exact.tape,
            width,
            rows,
            viewport_rows,
        )?;
        let local_rows = inner.rows();
        let target_top_anchor = inner.top_anchor();
        let preserve_total_rows = self.records.total_count().is_some();
        if preserve_total_rows && rows != 0 {
            let target_anchor_record = target_top_anchor
                .and_then(|anchor| anchor.block_id())
                .and_then(|block_id| self.record_index_for_block_id(block_id));
            if target_anchor_record.is_none()
                || self.local_delta_needs_record_expansion(
                    Some(crate::content::transcript_buf::ExactRowTapeState {
                        rows: local_rows,
                        top_anchor: target_top_anchor,
                    }),
                    viewport_rows,
                    rows,
                    target_anchor_record,
                )
            {
                return None;
            }
        }
        let total_rows = if preserve_total_rows {
            exact.global_total_rows
        } else {
            exact
                .global_total_rows
                .saturating_sub(tape.rows.total_rows)
                .saturating_add(local_rows.total_rows)
        };
        let reaches_loaded_tail = rows > 0
            && local_rows
                .clamped_scroll
                .saturating_add(RowIndex::from(viewport_rows.max(1)))
                >= local_rows.total_rows
            && self
                .records
                .active_range()
                .zip(self.records.total_count())
                .is_none_or(|(active, total)| active.end.get() >= total);
        if reaches_loaded_tail && !self.semantic_tail_record_is_materialized() {
            return None;
        }
        let repin_at_semantic_tail = reaches_loaded_tail;
        let row_offset = if preserve_total_rows && repin_at_semantic_tail {
            total_rows.saturating_sub(local_rows.total_rows)
        } else {
            exact.row_offset
        };
        let target = local_rows.clamped_scroll.saturating_add(row_offset);
        let scroll_target = if rows == 0 {
            crate::content::transcript_buf::ScrollTarget::visible_stable_anchor_delta(
                target,
                tape.top_anchor,
                0,
            )
        } else {
            crate::content::transcript_buf::ScrollTarget::visible_row(target)
        };
        let (trace_frame, trace_started_at) = self
            .start_scroll_trace_frame(width, scroll_target, Some((row_offset, total_rows)))
            .map_or((None, None), |(frame, started_at)| {
                (Some(frame), started_at)
            });
        Some(TranscriptProjectionPlan {
            materialization: TranscriptMaterializationPlan::ExactRowTape(inner),
            row_offset,
            total_rows,
            planned_loaded_rows: local_rows.total_rows,
            preserve_total_rows,
            requested_scroll: None,
            repin_at_semantic_tail,
            cursor_target: None,
            semantic_anchor: None,
            scroll_anchor: TranscriptScrollAnchor::EstimatedRow(target),
            width,
            viewport_rows,
            trace_frame,
            trace_started_at,
        })
    }

    pub(crate) fn plan_viewport_projection_measured(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        input: TranscriptViewportProjectionInput,
        viewport_rows: u16,
    ) -> Result<TranscriptProjectionPlan, TranscriptProjectionHydrationError> {
        #[cfg(test)]
        {
            self.viewport.projection_count = self.viewport.projection_count.saturating_add(1);
        }
        let previous_top_anchor = self.viewport.state.resolved_anchor.map(|anchor| anchor.top);
        let active_record_range_before = Self::trace_record_range(self.records.active_range());
        let pending_intent = self.take_viewport_intent(input);
        let intent = pending_intent.intent;
        let preserve_anchor_intent = matches!(
            intent,
            TranscriptScrollIntent::PreserveViewport | TranscriptScrollIntent::ResizeReflow { .. }
        );
        let local_delta_rows = match &intent {
            TranscriptScrollIntent::PreserveViewport => Some(0),
            TranscriptScrollIntent::UserDelta { rows } => Some(*rows),
            TranscriptScrollIntent::PageDelta { pages } => {
                Some(pages.saturating_mul(viewport_rows.max(1) as isize))
            }
            _ => None,
        };
        let preserve_previous_content_anchor = preserve_anchor_intent
            || matches!(
                intent,
                TranscriptScrollIntent::UserDelta { .. } | TranscriptScrollIntent::PageDelta { .. }
            );
        if let Some(rows) = local_delta_rows {
            if let Some(mut plan) = self.plan_exact_local_delta(
                lua,
                width,
                viewport_rows,
                input.fallback_scroll_top,
                rows,
            ) {
                if preserve_previous_content_anchor && self.records.total_count().is_some() {
                    if let Some(anchor @ TranscriptScrollAnchor::Content(_)) = previous_top_anchor {
                        plan.scroll_anchor = anchor;
                    }
                }
                if let Some(trace_frame) = plan.trace_frame.as_mut() {
                    trace_frame.active_record_range_before = active_record_range_before;
                }
                return Ok(plan);
            }
        }
        let behavior = Self::intent_behavior(&intent);
        let semantic_anchor = Self::semantic_anchor_for_intent(&intent);
        let semantic_far_seek =
            self.semantic_far_seek_for_intent(lua, width, viewport_rows, &intent);
        let (scroll_target, cursor_target) = semantic_far_seek.map_or_else(
            || {
                self.scroll_target_for_intent(
                    lua,
                    width,
                    viewport_rows,
                    input.fallback_scroll_top,
                    &intent,
                    pending_intent.hint,
                )
            },
            |far_seek| {
                (
                    crate::content::transcript_buf::ScrollTarget::visible_stable_row_delta(
                        far_seek.scroll_top,
                        Some(far_seek.row_anchor),
                        0,
                    ),
                    None,
                )
            },
        );
        if self.scroll_trace_enabled() && !self.scroll_trace_has_pending_input() {
            self.set_next_scroll_trace_input(TranscriptScrollTraceRenderInput {
                input_event_or_tick: "render_frame".to_string(),
                scroll_intent: intent,
                window_scroll_before: self
                    .last_traced_resolved_scroll_top()
                    .unwrap_or(input.fallback_scroll_top),
                window_scroll_after_input: input.fallback_scroll_top,
            });
        }
        let mut plan = self.plan_projection_measured_with_sparse_placeholders(
            lua,
            width,
            theme,
            scroll_target,
            viewport_rows,
            TranscriptProjectionOptions {
                allow_sparse_placeholders: behavior.allow_sparse_placeholders,
                repin_at_semantic_tail: behavior.repin_at_semantic_tail,
                semantic_far_seek,
            },
        )?;
        if preserve_previous_content_anchor && self.records.total_count().is_some() {
            if let Some(anchor @ TranscriptScrollAnchor::Content(_)) = previous_top_anchor {
                plan.scroll_anchor = anchor;
            }
        }
        plan.cursor_target = cursor_target;
        plan.semantic_anchor = semantic_anchor;
        if let Some(trace_frame) = plan.trace_frame.as_mut() {
            trace_frame.active_record_range_before = active_record_range_before;
        }
        Ok(plan)
    }

    pub(crate) fn plan_projection_measured(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        scroll_target: crate::content::transcript_buf::ScrollTarget,
        viewport_rows: u16,
    ) -> Result<TranscriptProjectionPlan, TranscriptProjectionHydrationError> {
        self.plan_projection_measured_with_sparse_placeholders(
            lua,
            width,
            theme,
            scroll_target,
            viewport_rows,
            TranscriptProjectionOptions {
                allow_sparse_placeholders: true,
                repin_at_semantic_tail: false,
                semantic_far_seek: None,
            },
        )
    }

    fn plan_projection_measured_with_sparse_placeholders(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        scroll_target: crate::content::transcript_buf::ScrollTarget,
        viewport_rows: u16,
        options: TranscriptProjectionOptions,
    ) -> Result<TranscriptProjectionPlan, TranscriptProjectionHydrationError> {
        let (trace_frame, trace_started_at) = self
            .start_scroll_trace_frame(width, scroll_target, None)
            .map_or((None, None), |(frame, started_at)| {
                (Some(frame), started_at)
            });
        let scroll_anchor = self.scroll_anchor_for_projection_target(scroll_target);
        match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ExactRow(row),
            ) => {
                if options.allow_sparse_placeholders {
                    self.activate_record_window_covering_approximate_display_row(
                        lua,
                        width,
                        row,
                        viewport_rows,
                    );
                }
            }
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(_)
                | crate::content::transcript_buf::ScrollAnchor::StableRowDelta { .. },
            ) => {}
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => {
                let _ = self.activate_tail_record_window(width, viewport_rows);
            }
        }
        let stable_local_delta = options.semantic_far_seek.is_none()
            && matches!(
                scroll_target,
                crate::content::transcript_buf::ScrollTarget::Visible(
                    crate::content::transcript_buf::ScrollAnchor::StableRowDelta { .. }
                )
            );
        let stable_total_rows = options
            .semantic_far_seek
            .map(|far_seek| far_seek.total_rows)
            .or_else(|| {
                stable_local_delta
                    .then(|| {
                        self.viewport
                            .state
                            .exact_viewport
                            .filter(|exact| exact.width == width)
                            .map(|exact| exact.global_total_rows)
                    })
                    .flatten()
            });
        let stable_requested_scroll = match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::StableRowDelta { row, delta, .. },
            ) => Some(add_signed_row(row, delta)),
            _ => None,
        };
        let active_record_range = self
            .records
            .active_range()
            .map(|range| (range.start.get(), range.end.get()));
        let stable_row_offset = stable_local_delta
            .then(|| {
                self.viewport
                    .state
                    .exact_viewport
                    .filter(|exact| {
                        exact.width == width && exact.active_record_range == active_record_range
                    })
                    .map(|exact| exact.row_offset)
            })
            .flatten()
            .or_else(|| match scroll_target {
                crate::content::transcript_buf::ScrollTarget::Visible(
                    crate::content::transcript_buf::ScrollAnchor::StableRowDelta {
                        row,
                        anchor: Some(anchor),
                        ..
                    },
                ) => self
                    .content
                    .projection
                    .row_for_stable_anchor(
                        lua,
                        &mut self.content.transcript.history,
                        width,
                        theme,
                        anchor,
                    )
                    .map(|local_row| row.saturating_sub(local_row)),
                _ => None,
            })
            .or_else(|| {
                stable_local_delta
                    .then(|| self.cached_sparse_prefix_row_offset(width))
                    .flatten()
            });
        let mut row_offset = if stable_local_delta {
            stable_row_offset.unwrap_or_default()
        } else {
            self.loaded_row_offset(width, LoadedRowOffsetPolicy::SparseEstimate)
        };
        let viewport_row_count = RowIndex::from(viewport_rows.max(1));
        let active_range_reaches_tail = self
            .records
            .active_range()
            .zip(self.records.sparse.total_count())
            .is_some_and(|(range, total)| range.end.get() >= total);
        let requested_scroll = options
            .semantic_far_seek
            .map(|far_seek| far_seek.scroll_top)
            .or(match scroll_target {
                crate::content::transcript_buf::ScrollTarget::Visible(
                    crate::content::transcript_buf::ScrollAnchor::ExactRow(row),
                ) => Some(row),
                crate::content::transcript_buf::ScrollTarget::Visible(
                    crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(_)
                    | crate::content::transcript_buf::ScrollAnchor::StableRowDelta { .. }
                    | crate::content::transcript_buf::ScrollAnchor::Tail,
                ) => None,
            });
        let local_target = match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ExactRow(row),
            ) => crate::content::transcript_buf::ScrollTarget::visible_row(
                row.saturating_sub(row_offset),
            ),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(row),
            ) => crate::content::transcript_buf::ScrollTarget::visible_reflow_stable_row(
                row.saturating_sub(row_offset),
            ),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::StableRowDelta { row, anchor, delta },
            ) => crate::content::transcript_buf::ScrollTarget::visible_stable_anchor_delta(
                row.saturating_sub(row_offset),
                anchor,
                delta,
            ),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => crate::content::transcript_buf::ScrollTarget::visible_tail(),
        };
        let (hydration_ids, hydration_plan) = self.content.projection.projection_hydration_ids(
            lua,
            &mut self.content.transcript.history,
            width,
            theme,
            local_target,
            viewport_rows,
        );
        let mut inner = self.hydrate_projection_plan(lua, theme, hydration_ids, hydration_plan)?;
        let mut loaded_rows = self.content.projection.estimated_total_rows(
            lua,
            &mut self.content.transcript.history,
            width,
        );
        let preserve_total_rows = self.records.total_count().is_some();
        let mut total_rows =
            stable_total_rows.unwrap_or_else(|| self.scrollbar_total_rows(width, loaded_rows));
        if let Some(far_seek) = options.semantic_far_seek {
            row_offset = far_seek.scroll_top.saturating_sub(inner.scroll_top());
        } else if let Some(requested) = stable_requested_scroll {
            let requested = requested.min(Self::max_scroll_for_total(total_rows, viewport_rows));
            row_offset = requested.saturating_sub(inner.scroll_top());
        }
        let target_reaches_tail = match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => true,
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ExactRow(row)
                | crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(row),
            ) => row >= total_rows.saturating_sub(viewport_row_count),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::StableRowDelta { .. },
            ) => false,
        };
        if target_reaches_tail && active_range_reaches_tail {
            let planned_as_tail = matches!(
                local_target,
                crate::content::transcript_buf::ScrollTarget::Visible(
                    crate::content::transcript_buf::ScrollAnchor::Tail
                )
            );
            if !planned_as_tail && options.repin_at_semantic_tail {
                let (tail_hydration_ids, tail_hydration_plan) =
                    self.content.projection.projection_hydration_ids(
                        lua,
                        &mut self.content.transcript.history,
                        width,
                        theme,
                        crate::content::transcript_buf::ScrollTarget::visible_tail(),
                        viewport_rows,
                    );
                inner = self.hydrate_projection_plan(
                    lua,
                    theme,
                    tail_hydration_ids,
                    tail_hydration_plan,
                )?;
                loaded_rows = self.content.projection.estimated_total_rows(
                    lua,
                    &mut self.content.transcript.history,
                    width,
                );
                total_rows = self.scrollbar_total_rows(width, loaded_rows);
            }
            row_offset = total_rows.saturating_sub(loaded_rows);
        }
        let loaded_start = row_offset;
        let loaded_end = row_offset.saturating_add(loaded_rows).min(total_rows);
        let sparse_gap = if options.allow_sparse_placeholders {
            match scroll_target {
                crate::content::transcript_buf::ScrollTarget::Visible(
                    crate::content::transcript_buf::ScrollAnchor::ExactRow(row)
                    | crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(row),
                ) if row < loaded_start
                    || (row.saturating_add(viewport_row_count) > loaded_end
                        && loaded_end < total_rows) =>
                {
                    let scroll_top = row.min(total_rows.saturating_sub(viewport_row_count));
                    let row_base = scroll_top.saturating_sub(viewport_row_count / 2);
                    Some(SparseProjectionGap {
                        scroll_top,
                        row_base,
                        end: row_base
                            .saturating_add(viewport_row_count.saturating_mul(2))
                            .min(total_rows),
                    })
                }
                crate::content::transcript_buf::ScrollTarget::Visible(
                    crate::content::transcript_buf::ScrollAnchor::StableRowDelta { .. },
                ) => None,
                _ => None,
            }
        } else {
            None
        };
        let materialization = sparse_gap
            .map(TranscriptMaterializationPlan::UnloadedGap)
            .unwrap_or(TranscriptMaterializationPlan::Loaded(inner));
        Ok(TranscriptProjectionPlan {
            materialization,
            row_offset,
            total_rows,
            planned_loaded_rows: loaded_rows,
            preserve_total_rows,
            requested_scroll,
            repin_at_semantic_tail: options.repin_at_semantic_tail,
            cursor_target: None,
            semantic_anchor: None,
            scroll_anchor,
            width,
            viewport_rows,
            trace_frame,
            trace_started_at,
        })
    }

    fn project_unloaded_sparse_gap(
        &mut self,
        buf: &mut Buffer,
        total_rows: RowIndex,
        viewport_rows: u16,
        gap: SparseProjectionGap,
    ) -> crate::smelt_edit::MaterializedRows {
        let viewport_rows = RowIndex::from(viewport_rows.max(1));
        let materialized_rows = gap
            .end
            .saturating_sub(gap.row_base)
            .min(viewport_rows.saturating_mul(2))
            .min(total_rows.saturating_sub(gap.row_base));
        buf.set_all_lines(vec![String::new(); materialized_rows as usize]);
        self.viewport.state.resolved_anchor = Some(TranscriptResolvedViewportAnchor {
            top: TranscriptScrollAnchor::EstimatedRow(gap.scroll_top),
            offset_rows: 0,
            scroll_top: gap.scroll_top,
        });
        self.viewport.state.semantic_anchor = None;
        self.viewport.state.mode = TranscriptViewportMode::FarSeek;
        self.viewport.state.exact_viewport = None;
        self.viewport.state.pending_projection = None;
        crate::smelt_edit::MaterializedRows {
            clamped_scroll: gap.scroll_top,
            row_base: gap.row_base,
            total_rows,
            materialized_rows,
        }
    }

    pub(crate) fn project_hydration_failure(
        &mut self,
        buf: &mut Buffer,
        viewport_rows: u16,
    ) -> AppliedTranscriptViewport {
        buf.set_all_lines(vec![
            "  (transcript unavailable: persisted content could not be hydrated)".to_string(),
        ]);
        self.viewport.state.resolved_anchor = Some(TranscriptResolvedViewportAnchor {
            top: TranscriptScrollAnchor::EstimatedRow(0),
            offset_rows: 0,
            scroll_top: 0,
        });
        self.viewport.state.semantic_anchor = None;
        self.viewport.state.mode = TranscriptViewportMode::Anchored;
        self.viewport.state.exact_viewport = None;
        self.viewport.state.pending_projection = None;
        let rows = crate::smelt_edit::MaterializedRows {
            clamped_scroll: 0,
            row_base: 0,
            total_rows: 1,
            materialized_rows: 1,
        };
        self.applied_viewport(rows, viewport_rows, false, VerticalScroll::Pinned, None)
    }

    fn applied_viewport(
        &self,
        rows: crate::smelt_edit::MaterializedRows,
        viewport_rows: u16,
        placeholder_rows_visible: bool,
        scroll_state: VerticalScroll,
        cursor_range: Option<DocRange>,
    ) -> AppliedTranscriptViewport {
        let viewport_rows = RowIndex::from(viewport_rows.max(1));
        AppliedTranscriptViewport {
            materialized_rows: rows,
            top_anchor: self
                .viewport
                .state
                .resolved_anchor
                .map(|anchor| Self::trace_anchor(anchor.top)),
            scrollbar_total_rows: rows.total_rows,
            exact_visible_range: rows.clamped_scroll
                ..rows
                    .clamped_scroll
                    .saturating_add(viewport_rows)
                    .min(rows.total_rows),
            placeholder_rows_visible,
            scroll_state,
            cursor_range,
        }
    }

    pub(crate) fn project_applied_viewport(
        &mut self,
        lua: &LuaRuntime,
        buf: &mut Buffer,
        theme: &Theme,
        plan: TranscriptProjectionPlan,
    ) -> AppliedTranscriptViewport {
        let TranscriptProjectionPlan {
            materialization,
            row_offset,
            total_rows,
            planned_loaded_rows,
            preserve_total_rows,
            requested_scroll,
            repin_at_semantic_tail,
            cursor_target,
            semantic_anchor,
            scroll_anchor,
            width,
            viewport_rows,
            trace_frame,
            trace_started_at,
        } = plan;
        let mut trace_ctx = TranscriptScrollTraceFinishContext {
            width,
            row_offset,
            viewport_rows,
            trace_frame,
            trace_started_at,
        };
        let mut rows = match materialization {
            TranscriptMaterializationPlan::ExactRowTape(exact) => self
                .content
                .projection
                .apply_exact_row_tape_scroll(buf, exact)
                .expect("exact transcript row tape changed before frame application"),
            TranscriptMaterializationPlan::UnloadedGap(gap) => {
                let rows = self.project_unloaded_sparse_gap(buf, total_rows, viewport_rows, gap);
                let scroll_state = self.projected_scroll_state(false);
                if matches!(scroll_state, VerticalScroll::Tail) {
                    self.viewport.state.needs_tail_repin = false;
                }
                self.finish_scroll_trace_frame(lua, rows, &mut trace_ctx, true);
                return self.applied_viewport(rows, viewport_rows, true, scroll_state, None);
            }
            TranscriptMaterializationPlan::Loaded(inner) => self
                .content
                .projection
                .project_planned(lua, buf, &mut self.content.transcript.history, theme, inner),
        };
        let exact_tape = self
            .content
            .projection
            .exact_row_tape_handle(rows)
            .expect("projected transcript rows must belong to the current exact row tape");
        let local_total_rows = rows.total_rows;
        let total_rows = if preserve_total_rows {
            total_rows
        } else {
            total_rows
                .saturating_sub(planned_loaded_rows)
                .saturating_add(local_total_rows)
        };
        self.observe_exact_loaded_record_rows();
        let cursor_range = cursor_target
            .and_then(|target| self.resolve_cursor_target(lua, width, row_offset, target));
        rows.clamped_scroll = rows.clamped_scroll.saturating_add(row_offset);
        rows.row_base = rows.row_base.saturating_add(row_offset);
        let viewport_rows_count = RowIndex::from(viewport_rows.max(1));
        rows.total_rows = total_rows.max(minimum_materialized_total_rows(
            rows.row_base,
            rows.clamped_scroll,
            viewport_rows,
        ));
        if let Some(requested) = requested_scroll {
            let requested = requested.min(rows.total_rows.saturating_sub(viewport_rows_count));
            if requested >= rows.row_base
                && requested.saturating_add(viewport_rows_count)
                    <= rows.row_base.saturating_add(rows.materialized_rows)
            {
                rows.clamped_scroll = requested;
            }
        }
        self.capture_viewport_anchor_with_offset(
            lua,
            width,
            rows.clamped_scroll,
            viewport_rows,
            row_offset,
            scroll_anchor,
        );
        let captured_semantic_anchor = self
            .content_anchor_at_or_after_row_with_offset(
                lua,
                width,
                rows.clamped_scroll,
                viewport_rows,
                row_offset,
            )
            .map(|(anchor, _)| anchor.into());
        self.viewport.state.semantic_anchor = semantic_anchor.or(captured_semantic_anchor);
        let active_record_range = self
            .records
            .active_range()
            .map(|range| (range.start.get(), range.end.get()));
        self.viewport.state.exact_viewport = Some(TranscriptExactViewport {
            tape: exact_tape,
            width,
            row_offset,
            global_total_rows: rows.total_rows,
            active_record_range,
        });
        self.viewport.state.pending_projection = None;
        let reached_semantic_tail = repin_at_semantic_tail
            && self.projected_viewport_reached_semantic_tail(
                lua,
                width,
                rows,
                viewport_rows,
                row_offset,
                local_total_rows,
            );
        let scroll_state = self.projected_scroll_state(reached_semantic_tail);
        if matches!(scroll_state, VerticalScroll::Tail) {
            self.viewport.state.needs_tail_repin = false;
        }
        self.finish_scroll_trace_frame(lua, rows, &mut trace_ctx, false);
        self.applied_viewport(rows, viewport_rows, false, scroll_state, cursor_range)
    }

    pub(crate) fn project_planned(
        &mut self,
        lua: &LuaRuntime,
        buf: &mut Buffer,
        theme: &Theme,
        plan: TranscriptProjectionPlan,
    ) -> crate::smelt_edit::MaterializedRows {
        self.project_applied_viewport(lua, buf, theme, plan)
            .materialized_rows
    }

    pub(crate) fn exact_or_gap_display_rows_for_range(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> crate::smelt_edit::DisplayRows {
        let row_offset =
            self.loaded_row_offset(width, LoadedRowOffsetPolicy::RenderedViewportOrEstimate);
        let end = start.saturating_add(count);
        if count == 0 || end <= start {
            return DisplayRows::empty();
        }
        let mut rows = Vec::new();
        if start < row_offset {
            let prefix_end = end.min(row_offset);
            rows.extend(inert_sparse_gap_rows(prefix_end.saturating_sub(start)));
            if end <= row_offset {
                return DisplayRows { rows };
            }
        }
        let loaded_rows = self.content.projection.estimated_total_rows(
            lua,
            &mut self.content.transcript.history,
            width,
        );
        let loaded_end = row_offset.saturating_add(loaded_rows);
        if start >= loaded_end {
            return DisplayRows {
                rows: inert_sparse_gap_rows(count),
            };
        }
        let local_start = start.saturating_sub(row_offset);
        let local_end = end.saturating_sub(row_offset).min(loaded_rows);
        let hydration_ids = self.content.projection.row_range_hydration_ids(
            lua,
            &mut self.content.transcript.history,
            width,
            local_start..local_end,
        );
        if !self.pin_operation_blocks(&hydration_ids) {
            return DisplayRows { rows };
        }
        let mut loaded = self.content.projection.display_rows_for_range(
            lua,
            &mut self.content.transcript.history,
            width,
            theme,
            local_start..local_end,
        );
        self.unpin_operation_blocks(&hydration_ids);
        rows.append(&mut loaded.rows);
        if end > loaded_end {
            rows.extend(inert_sparse_gap_rows(end.saturating_sub(loaded_end)));
        }
        DisplayRows { rows }
    }

    pub(crate) fn search_matches_for_row_range(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        start: RowIndex,
        count: RowIndex,
        query: &str,
    ) -> Vec<TranscriptSearchMatch> {
        if query.is_empty() || count == 0 {
            return Vec::new();
        }
        let display = self.exact_or_gap_display_rows_for_range(lua, width, theme, start, count);
        let mut matches = Vec::new();
        for (offset, row) in display.rows.iter().enumerate() {
            let row_index = start.saturating_add(offset as RowIndex);
            for range in crate::smelt_edit::display_row_matches(row, row_index, query) {
                matches.push(TranscriptSearchMatch::new(
                    range,
                    self.search_anchor_at_row(lua, width, row_index),
                ));
            }
        }
        matches
    }

    pub(crate) fn node_metadata_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        let local_row = self.exact_loaded_row_for_virtual_content_row(lua, width, row)?;
        self.content
            .projection
            .node_metadata_at_row(lua, &mut self.content.transcript.history, width, local_row)
            .map(|node| self.offset_node_row(width, node))
    }

    #[cfg(test)]
    pub(crate) fn row_anchor_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<TranscriptRowAnchor> {
        let row_offset =
            self.loaded_row_offset(width, LoadedRowOffsetPolicy::RenderedViewportOrEstimate);
        self.row_anchor_at_row_with_offset(lua, width, row, row_offset)
    }

    fn row_anchor_at_row_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        row_offset: RowIndex,
    ) -> Option<TranscriptRowAnchor> {
        let local_row =
            self.exact_loaded_row_for_virtual_content_row_with_offset(lua, width, row, row_offset)?;
        self.content.projection.row_anchor_at_row(
            lua,
            &mut self.content.transcript.history,
            width,
            local_row,
        )
    }

    pub(crate) fn row_for_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: TranscriptRowAnchor,
    ) -> Option<crate::smelt_edit::RowIndex> {
        let row_offset =
            self.loaded_row_offset(width, LoadedRowOffsetPolicy::RenderedViewportOrEstimate);
        self.row_for_anchor_with_offset(lua, width, anchor, row_offset)
    }

    fn row_for_anchor_with_offset(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: TranscriptRowAnchor,
        row_offset: RowIndex,
    ) -> Option<crate::smelt_edit::RowIndex> {
        self.content
            .projection
            .row_for_anchor(lua, &mut self.content.transcript.history, width, anchor)
            .map(|row| row.saturating_add(row_offset))
    }

    pub(super) fn position_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        position: crate::smelt_edit::DocPosition,
    ) -> TranscriptPositionAnchor {
        TranscriptPositionAnchor {
            anchor: self.node_anchor_at_row(lua, width, position.row),
            position,
        }
    }

    pub(super) fn resolve_position_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: TranscriptPositionAnchor,
    ) -> crate::smelt_edit::DocPosition {
        let row = anchor
            .anchor
            .and_then(|anchor| self.row_for_node_anchor(lua, width, 20, anchor))
            .unwrap_or(anchor.position.row);
        crate::smelt_edit::DocPosition {
            row,
            byte_col: anchor.position.byte_col,
        }
    }

    pub(super) fn search_range_anchor(
        &mut self,
        matched: TranscriptSearchMatch,
    ) -> TranscriptSearchRangeAnchor {
        TranscriptSearchRangeAnchor {
            anchor: matched.anchor,
            start_byte_col: matched.start_byte_col(),
            end_byte_col: matched.end_byte_col(),
            fallback_range: matched.range,
        }
    }

    pub(super) fn resolve_search_range_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: TranscriptSearchRangeAnchor,
    ) -> TranscriptSearchMatch {
        let row = self
            .row_for_search_anchor(lua, width, 20, anchor.anchor, anchor.start_byte_col, None)
            .unwrap_or(anchor.fallback_range.start.row);
        let range = crate::smelt_edit::DocRange {
            start: crate::smelt_edit::DocPosition {
                row,
                byte_col: anchor.start_byte_col,
            },
            end: crate::smelt_edit::DocPosition {
                row,
                byte_col: anchor.end_byte_col,
            },
        };
        TranscriptSearchMatch::new(range, anchor.anchor)
    }

    pub(crate) fn fold_node_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        action: crate::content::transcript_buf::FoldAction,
        activation: crate::content::transcript_buf::FoldActivation,
    ) -> bool {
        let Some(local_row) = self.exact_loaded_row_for_virtual_content_row(lua, width, row) else {
            return false;
        };
        let changed = self.content.projection.fold_node_at_row(
            lua,
            &mut self.content.transcript.history,
            width,
            crate::content::transcript_buf::FoldAtRow {
                row: local_row,
                action,
                activation,
            },
        );
        if changed {
            self.clear_transcript_layout_caches();
        }
        changed
    }

    pub(crate) fn prepare_layout(&mut self, lua: &LuaRuntime, width: u16) {
        self.content
            .projection
            .prepare_layout(lua, &mut self.content.transcript.history, width);
    }

    pub(crate) fn fold_node(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        id: crate::content::render_plan::RenderNodeId,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.prepare_layout(lua, width);
        let changed =
            self.content
                .projection
                .fold_node(&self.content.transcript.history, id, action);
        if changed {
            self.clear_transcript_layout_caches();
        }
        changed
    }

    pub(crate) fn fold_all(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.prepare_layout(lua, width);
        let changed = self
            .content
            .projection
            .fold_all(&self.content.transcript.history, action);
        if changed {
            self.clear_transcript_layout_caches();
        }
        changed
    }

    pub(crate) fn fold_block_kind(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        kind: &str,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.prepare_layout(lua, width);
        let changed =
            self.content
                .projection
                .fold_block_kind(&self.content.transcript.history, kind, action);
        if changed {
            self.clear_transcript_layout_caches();
        }
        changed
    }

    pub(crate) fn copy_exact_loaded_range(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        range: crate::smelt_edit::DocRange,
    ) -> crate::smelt_edit::CopyOutput {
        let row_offset =
            self.loaded_row_offset(width, LoadedRowOffsetPolicy::RenderedViewportOrEstimate);
        if range.end.row < row_offset || (range.end.row == row_offset && range.end.byte_col == 0) {
            return crate::smelt_edit::CopyOutput::default();
        }
        let loaded_rows = self.content.projection.estimated_total_rows(
            lua,
            &mut self.content.transcript.history,
            width,
        );
        let mut local_range = range;
        local_range.start.row = local_range.start.row.saturating_sub(row_offset);
        local_range.end.row = local_range
            .end
            .row
            .saturating_sub(row_offset)
            .min(loaded_rows);
        if local_range.start.row > local_range.end.row
            || (local_range.start.row == local_range.end.row
                && local_range.start.byte_col >= local_range.end.byte_col)
        {
            return crate::smelt_edit::CopyOutput::default();
        }
        let hydration_ids = self.content.projection.row_range_hydration_ids(
            lua,
            &mut self.content.transcript.history,
            width,
            local_range.start.row..local_range.end.row.saturating_add(1),
        );
        if !self.pin_operation_blocks(&hydration_ids) {
            return crate::smelt_edit::CopyOutput::default();
        }
        let copied = self.content.projection.copy_range(
            lua,
            &mut self.content.transcript.history,
            width,
            theme,
            local_range,
        );
        self.unpin_operation_blocks(&hydration_ids);
        copied
    }

    pub(crate) fn record_save_bounds(
        &self,
        dirty_history_from: Option<usize>,
    ) -> Option<TranscriptRecordSaveBounds> {
        let history = self.history();
        let history_order_dirty = dirty_history_from
            .and_then(|idx| history.first_block_index_for_history_origin_at_or_after(idx));
        let order_start = match (history.record_dirty_from(), history_order_dirty) {
            (Some(record), Some(history)) => Some(record.min(history)),
            (dirty, None) | (None, dirty) => dirty,
        }?;
        let local_record_start_idx = history.record_index_for_order_index(order_start);
        let record_base_idx = self
            .records
            .active_range
            .as_ref()
            .map(|range| range.start.get())
            .unwrap_or_default();
        Some(TranscriptRecordSaveBounds {
            order_start,
            record_start_idx: record_base_idx.saturating_add(local_record_start_idx),
            record_end_idx: record_base_idx.saturating_add(history.persisted_block_count()),
        })
    }

    pub(crate) fn history(&self) -> &BlockHistory {
        &self.content.transcript.history
    }

    pub(crate) fn history_mut(&mut self) -> &mut BlockHistory {
        self.clear_transcript_layout_caches();
        &mut self.content.transcript.history
    }

    pub(crate) fn projection_generation(&self) -> u64 {
        self.content.projection.projection_generation()
    }

    pub(crate) fn invalidate_renderer_if_changed(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) -> bool {
        let changed = self
            .content
            .projection
            .invalidate_renderer_if_changed(generation, cache_key);
        if changed {
            self.clear_transcript_layout_caches();
        }
        changed
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.content.transcript.history.is_empty()
            && self
                .records
                .sparse
                .total_count()
                .is_none_or(|total_count| total_count == 0)
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn record_total_count(&self) -> Option<usize> {
        self.records.total_count()
    }

    #[cfg(test)]
    fn loaded_record_count(&self) -> usize {
        self.records.sparse.loaded_record_count()
    }

    #[cfg(test)]
    fn loaded_record_ranges(&self) -> &[Range<smelt_store::TranscriptRecordOffset>] {
        self.records.sparse.loaded_ranges()
    }

    #[cfg(test)]
    fn record_range_state(&self, range: Range<usize>) -> RecordRangeState {
        let Some(total_count) = self.records.total_count() else {
            return RecordRangeState::Unavailable;
        };
        let start = range.start.min(total_count);
        let end = range.end.min(total_count);
        if start >= end {
            return RecordRangeState::Loaded;
        }
        let start = smelt_store::TranscriptRecordOffset::new(start);
        let end = smelt_store::TranscriptRecordOffset::new(end);
        if self
            .records
            .sparse
            .loaded_ranges()
            .iter()
            .any(|loaded| loaded.start <= start && loaded.end >= end)
        {
            RecordRangeState::Loaded
        } else {
            RecordRangeState::Missing
        }
    }

    pub(crate) fn push(&mut self, block: Block) {
        self.content.transcript.push(block);
        self.clear_transcript_layout_caches();
    }

    pub(crate) fn push_with_origin(
        &mut self,
        block: Block,
        origin: smelt_core::transcript_model::BlockOrigin,
    ) {
        self.content.transcript.push_with_origin(block, origin);
        self.clear_transcript_layout_caches();
    }

    pub(crate) fn set_compaction_preview(&mut self, summary: String) -> Option<BlockId> {
        let summary = summary.trim().to_string();

        if let Some(id) = self.content.compaction_preview_id {
            if self.content.transcript.block(id).is_some() {
                self.content
                    .transcript
                    .rewrite_compaction_preview(id, summary);
                self.clear_transcript_layout_caches();
                return Some(id);
            }
            self.content.compaction_preview_id = None;
        }

        let id = self.content.transcript.push_compaction_preview(summary)?;
        self.content.compaction_preview_id = Some(id);
        self.clear_transcript_layout_caches();
        Some(id)
    }

    pub(crate) fn clear_compaction_preview(&mut self) -> Option<BlockId> {
        let id = self.content.compaction_preview_id.take()?;
        self.content.transcript.remove_compaction_preview(id);
        self.clear_transcript_layout_caches();
        Some(id)
    }

    pub(crate) fn compaction_preview_id(&self) -> Option<BlockId> {
        self.content.compaction_preview_id
    }

    pub(crate) fn insert_checkpoint_marker_at(
        &mut self,
        block_index: usize,
        history_index: usize,
        block: Block,
    ) {
        self.content
            .transcript
            .insert_checkpoint_marker_at(block_index, history_index, block);
        self.clear_transcript_layout_caches();
    }

    pub(crate) fn remove_unoriginated_at(&mut self, block_index: usize) -> Option<Block> {
        let removed = self.content.transcript.remove_unoriginated_at(block_index);
        if removed.is_some() {
            self.clear_transcript_layout_caches();
        }
        removed
    }

    pub(crate) fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        let drained = self.content.transcript.drain_finished_blocks();
        if !drained.is_empty() {
            self.clear_transcript_layout_caches();
        }
        drained
    }

    pub(crate) fn last_user_block_index(&self) -> Option<usize> {
        self.content.transcript.last_user_block_index()
    }

    pub(crate) fn user_turns(&self) -> Vec<(usize, String)> {
        self.content.transcript.user_turns()
    }

    pub(crate) fn truncate_to(&mut self, block_idx: usize) {
        let history = &self.content.transcript.history;
        let record_len = if block_idx < history.len() {
            self.records.total_count().map(|_| {
                self.records
                    .global_record_index(history.record_index_for_order_index(block_idx))
            })
        } else {
            None
        };
        self.content.transcript.truncate_to(block_idx);
        if let Some(record_len) = record_len {
            self.records.truncate(record_len);
            self.extent_index.clear_persisted_record_estimates();
        }
    }
}

impl Default for TranscriptDocument {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
const TEST_LINEAGE_SESSION_ID: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[cfg(test)]
fn create_test_lineage_session(root: &std::path::Path) -> std::path::PathBuf {
    let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
    session.id = TEST_LINEAGE_SESSION_ID.into();
    let command = smelt_core::session::initial_store_commit_from_session(&session).unwrap();
    let mut writer = smelt_store::OwnedLineageWriter::open(root, TEST_LINEAGE_SESSION_ID).unwrap();
    writer.commit_session(&command).unwrap();
    writer.release().unwrap();
    root.join(TEST_LINEAGE_SESSION_ID)
}

#[cfg(test)]
fn seed_test_transcript_rows(
    root: &std::path::Path,
    records: Vec<smelt_store::StoredTranscriptBlock>,
) -> std::path::PathBuf {
    let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
    session.id = TEST_LINEAGE_SESSION_ID.into();
    let mut command = smelt_core::session::initial_store_commit_from_session(&session).unwrap();
    command.transcript_records = Some(smelt_store::TranscriptRecordSuffix {
        start: smelt_store::TranscriptRecordIndex::ZERO,
        records,
    });
    let mut writer = smelt_store::OwnedLineageWriter::open(root, TEST_LINEAGE_SESSION_ID).unwrap();
    writer.commit_session(&command).unwrap();
    writer.release().unwrap();
    root.join(TEST_LINEAGE_SESSION_ID)
}

#[cfg(test)]
mod document_tests {
    use super::*;
    use crate::app::transcript_scroll_trace::{
        TranscriptProjectionTargetTrace, TranscriptRecordTraceRange, TranscriptScrollIntent,
        TranscriptScrollTraceRenderInput, TranscriptTraceAnchor,
    };
    use smelt_core::transcript_model::{block_retained_bytes, ToolState, TranscriptBlockRecord};

    fn stored_count(document: &TranscriptDocument) -> usize {
        document
            .history()
            .order
            .iter()
            .filter(|id| document.history().stored_ref(**id).is_some())
            .count()
    }

    fn comparable_record_rows(
        records: Vec<smelt_core::TranscriptBlockRecordWithId>,
    ) -> Vec<(u64, String)> {
        records
            .into_iter()
            .map(|record| {
                (
                    record.block_id.get(),
                    serde_json::to_string(&record.record).expect("serialize record"),
                )
            })
            .collect()
    }

    #[test]
    fn record_window_lru_respects_recent_access_and_active_pins() {
        let records = transcript_records(4);
        let first = LoadedRecordWindow {
            start: smelt_store::TranscriptRecordOffset::new(0),
            total_count: 4,
            hydration: smelt_store::TranscriptRecordHydration::Hydrated,
            records: records_with_ids(&records[0..2], 0),
        };
        let second = LoadedRecordWindow {
            start: smelt_store::TranscriptRecordOffset::new(2),
            total_count: 4,
            hydration: smelt_store::TranscriptRecordHydration::Hydrated,
            records: records_with_ids(&records[2..4], 2),
        };
        let mut cache = SparseTranscriptRecords::default();
        assert!(cache.merge(&first));
        assert!(cache.merge(&second));
        let zero = smelt_store::TranscriptRecordOffset::new(0);
        let two = smelt_store::TranscriptRecordOffset::new(2);
        cache.touch_range(&(zero..smelt_store::TranscriptRecordOffset::new(1)));
        let budget = cache.records[&zero]
            .stored
            .retained_bytes()
            .saturating_add(cache.records[&two].stored.retained_bytes());

        cache.enforce_byte_budget(&(two..smelt_store::TranscriptRecordOffset::new(3)), budget);

        assert_eq!(cache.records.len(), 2);
        assert!(cache.records.contains_key(&zero));
        assert!(cache.records.contains_key(&two));
        assert_eq!(cache.retained_bytes(), budget);
    }

    #[test]
    fn sqlite_hydration_reads_only_requested_record_ranges() {
        let records = transcript_records(4);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..4,
            Some(session_dir.clone()),
        ));
        document.set_memory_budget(TranscriptMemoryBudget {
            hydrated_blocks: usize::MAX,
            ..Default::default()
        });
        let ids = document.history().order.clone();

        assert!(document.ensure_hydrated_ids(&[ids[0], ids[2]]));
        let first = document.memory_snapshot();
        assert_eq!(first.hydration_reads, 2);
        assert_eq!(first.hydration_ranges, 2);
        assert!(
            matches!(document.history().block(ids[0]), Some(Block::Text { content }) if content == "block 0")
        );
        assert!(
            matches!(document.history().block(ids[2]), Some(Block::Text { content }) if content == "block 2")
        );

        assert!(document.ensure_hydrated_ids(&[ids[0], ids[2]]));
        let second = document.memory_snapshot();
        assert_eq!(second.hydration_reads, first.hydration_reads);
        assert_eq!(second.hydration_ranges, first.hydration_ranges);
    }

    #[test]
    fn active_full_document_hydrates_stored_block_without_sparse_extent() {
        let mut source = fixed_transcript(3);
        let records = source.history.block_records();
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        source.history.clear_record_dirty();
        let first_id = source.history.order[0];
        let stored = source
            .history
            .stored_ref_for_materialized(first_id, 0)
            .unwrap();
        let mut document = TranscriptDocument::from_transcript(source);
        document.set_session_dir(session_dir);

        assert_eq!(document.records.total_count(), None);
        assert_eq!(document.compacted_record_len, 0);
        assert!(document.history_mut().dematerialize_live(first_id, stored) > 0);
        assert_eq!(document.history().persisted_block_count(), records.len());
        assert!(document.ensure_hydrated_ids(&[first_id]));
        assert!(
            matches!(document.history().block(first_id), Some(Block::Text { content }) if content == "block 0")
        );
    }

    #[test]
    fn transcript_replacement_preserves_session_store_for_hydration() {
        let mut rebuilt = fixed_transcript(3);
        let records = rebuilt.history.block_records();
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        rebuilt.history.clear_record_dirty();
        let first_id = rebuilt.history.order[0];
        let stored = rebuilt
            .history
            .stored_ref_for_materialized(first_id, 0)
            .unwrap();
        assert!(rebuilt.history.dematerialize_live(first_id, stored) > 0);

        let mut document = TranscriptDocument::new();
        document.set_session_dir(session_dir);
        document.replace_transcript(rebuilt);

        assert!(document.ensure_hydrated_ids(&[first_id]));
        assert!(
            matches!(document.history().block(first_id), Some(Block::Text { content }) if content == "block 0")
        );
    }

    #[test]
    fn hydrated_lru_evicts_by_bytes_and_rehydrates_exactly() {
        let records = transcript_records(3);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..3,
            Some(session_dir),
        ));
        let ids = document.history().order.clone();
        let one_block_budget = block_retained_bytes(&records[0].block);
        document.set_memory_budget(TranscriptMemoryBudget {
            hydrated_blocks: one_block_budget,
            ..Default::default()
        });

        assert!(document.ensure_hydrated_ids(&[ids[0]]));
        assert!(document.history().is_hydrated(ids[0]));
        assert!(document.ensure_hydrated_ids(&[ids[1]]));
        assert!(!document.history().is_materialized(ids[0]));
        assert!(document.history().is_hydrated(ids[1]));
        assert_eq!(document.memory_snapshot().evicted_entries, 1);

        assert!(document.ensure_hydrated_ids(&[ids[0]]));
        assert!(
            matches!(document.history().block(ids[0]), Some(Block::Text { content }) if content == "block 0")
        );
        assert!(!document.history().is_materialized(ids[1]));
        assert_eq!(document.memory_snapshot().hydration_reads, 3);
    }

    #[test]
    fn copy_range_hydrates_exact_content_and_releases_operation_pins() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(1);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..1,
            Some(session_dir.clone()),
        ));
        document.set_memory_budget(TranscriptMemoryBudget {
            hydrated_blocks: 1,
            ..Default::default()
        });
        let id = document.history().order[0];

        let copy = document.copy_exact_loaded_range(
            &lua,
            80,
            &theme,
            DocRange {
                start: DocPosition {
                    row: 0,
                    byte_col: 0,
                },
                end: DocPosition {
                    row: 1,
                    byte_col: 0,
                },
            },
        );

        assert!(copy.kill_ring.contains("block 0"));
        assert!(!document.history().is_materialized(id));
        assert_eq!(document.memory_snapshot().hydration_reads, 1);

        let second = document.copy_exact_loaded_range(
            &lua,
            80,
            &theme,
            DocRange {
                start: DocPosition {
                    row: 0,
                    byte_col: 0,
                },
                end: DocPosition {
                    row: 1,
                    byte_col: 0,
                },
            },
        );
        assert_eq!(second, copy);
        assert_eq!(document.memory_snapshot().hydration_reads, 2);
        assert!(!document.history().is_materialized(id));
    }

    #[test]
    fn exact_reveal_rows_and_scroll_anchors_survive_eviction_and_rehydration() {
        let lua = LuaRuntime::new();
        let records = varied_transcript_records(40);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..records.len(),
            Some(session_dir.clone()),
        ));
        document.set_memory_budget(TranscriptMemoryBudget {
            hydrated_blocks: 1,
            ..Default::default()
        });

        let first = document
            .record_block_reveal_position(&lua, 48, 23, 2, 12)
            .expect("first exact reveal");
        let anchor = document
            .row_anchor_at_row(&lua, 48, first.target_row)
            .expect("target row anchor");
        let first_snapshot = document.memory_snapshot();
        assert!(first_snapshot.hydration_reads > 0);
        assert_eq!(first_snapshot.hydrated_blocks, 0);

        let second = document
            .record_block_reveal_position(&lua, 48, 23, 2, 12)
            .expect("rehydrated exact reveal");
        assert_eq!(second, first);
        assert_eq!(
            document.row_for_anchor(&lua, 48, anchor),
            Some(first.target_row)
        );
        let second_snapshot = document.memory_snapshot();
        assert!(second_snapshot.hydration_reads > first_snapshot.hydration_reads);
        assert_eq!(second_snapshot.hydrated_blocks, 0);
    }

    #[test]
    fn randomized_bounded_cache_matches_non_evicting_transcript_behavior() {
        let records = transcript_records(32);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let loaded =
            || sparse_loaded_transcript(&records, 0..records.len(), Some(session_dir.clone()));
        let mut bounded = TranscriptDocument::from_loaded_transcript(loaded());
        let mut non_evicting = TranscriptDocument::from_loaded_transcript(loaded());
        bounded.set_memory_budget(TranscriptMemoryBudget {
            hydrated_blocks: block_retained_bytes(&records[0].block),
            ..Default::default()
        });
        non_evicting.set_memory_budget(TranscriptMemoryBudget {
            hydrated_blocks: usize::MAX,
            ..Default::default()
        });

        let mut seed = 0x9e37_79b9_u64;
        for step in 0..160_u64 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let index = (seed as usize) % bounded.history().order.len();
            let bounded_id = bounded.history().order[index];
            let non_evicting_id = non_evicting.history().order[index];
            assert_eq!(bounded_id, non_evicting_id);
            assert!(bounded.pin_operation_blocks(&[bounded_id]));
            assert!(non_evicting.pin_operation_blocks(&[non_evicting_id]));
            assert_eq!(
                bounded.history().block(bounded_id),
                non_evicting.history().block(non_evicting_id)
            );
            assert_eq!(
                bounded.history().block_origin(bounded_id),
                non_evicting.history().block_origin(non_evicting_id)
            );
            assert_eq!(
                bounded.history().content_hash(bounded_id),
                non_evicting.history().content_hash(non_evicting_id)
            );

            if step.is_multiple_of(29) {
                let replacement = Block::Text {
                    content: format!("rewritten block {index} at step {step}"),
                };
                bounded
                    .history_mut()
                    .rewrite(bounded_id, replacement.clone());
                non_evicting
                    .history_mut()
                    .rewrite(non_evicting_id, replacement);
            }
            bounded.unpin_operation_blocks(&[bounded_id]);
            non_evicting.unpin_operation_blocks(&[non_evicting_id]);

            assert_eq!(bounded.history().order, non_evicting.history().order);
            assert_eq!(
                bounded.history().generation(),
                non_evicting.history().generation()
            );
            assert_eq!(
                bounded.history().navigation_generation(),
                non_evicting.history().navigation_generation()
            );
            assert_eq!(
                bounded.history().record_dirty_from(),
                non_evicting.history().record_dirty_from()
            );
        }

        let bounded_bounds = bounded.record_save_bounds(None);
        let non_evicting_bounds = non_evicting.record_save_bounds(None);
        let bounded_pins = bounded.pin_record_suffix_for_save(bounded_bounds).unwrap();
        let non_evicting_pins = non_evicting
            .pin_record_suffix_for_save(non_evicting_bounds)
            .unwrap();
        assert_eq!(bounded_bounds, non_evicting_bounds);
        let bounded_records = bounded_bounds.map(|bounds| {
            comparable_record_rows(
                bounded
                    .history()
                    .block_records_with_ids_from(bounds.order_start),
            )
        });
        let non_evicting_records = non_evicting_bounds.map(|bounds| {
            comparable_record_rows(
                non_evicting
                    .history()
                    .block_records_with_ids_from(bounds.order_start),
            )
        });
        assert_eq!(bounded_records, non_evicting_records);
        bounded.unpin_operation_blocks(&bounded_pins);
        non_evicting.unpin_operation_blocks(&non_evicting_pins);

        bounded.history_mut().clear_record_dirty();
        non_evicting.history_mut().clear_record_dirty();
        bounded.truncate_to(17);
        non_evicting.truncate_to(17);
        assert_eq!(bounded.history().order, non_evicting.history().order);
        let bounded_bounds = bounded.record_save_bounds(None);
        let non_evicting_bounds = non_evicting.record_save_bounds(None);
        assert_eq!(bounded_bounds, non_evicting_bounds);
        let bounded_records = bounded_bounds.map(|bounds| {
            comparable_record_rows(
                bounded
                    .history()
                    .block_records_with_ids_from(bounds.order_start),
            )
        });
        let non_evicting_records = non_evicting_bounds.map(|bounds| {
            comparable_record_rows(
                non_evicting
                    .history()
                    .block_records_with_ids_from(bounds.order_start),
            )
        });
        assert_eq!(bounded_records, non_evicting_records);
        assert!(bounded.memory_snapshot().evicted_entries > 0);
    }

    #[test]
    fn pinned_oversize_hydration_records_debt_and_converges_after_unpin() {
        let records = transcript_records(1);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..1,
            Some(session_dir.clone()),
        ));
        document.set_memory_budget(TranscriptMemoryBudget {
            hydrated_blocks: 1,
            ..Default::default()
        });
        let id = document.history().order[0];

        assert!(document.pin_operation_blocks(&[id]));
        let pinned = document.memory_snapshot();
        assert!(pinned.pinned_hydrated_bytes > 1);
        assert_eq!(
            pinned.hydrated_oversize_debt_bytes,
            pinned.pinned_hydrated_bytes - 1
        );
        document.unpin_operation_blocks(&[id]);

        let converged = document.memory_snapshot();
        assert_eq!(converged.hydrated_block_bytes, 0);
        assert_eq!(converged.hydrated_tool_state_bytes, 0);
        assert_eq!(converged.hydrated_oversize_debt_bytes, 0);
        assert!(!document.history().is_materialized(id));
    }

    #[test]
    fn idle_compaction_obeys_block_and_byte_slice_limits() {
        let mut source = Transcript::new();
        for index in 0..(TRANSCRIPT_IDLE_COMPACTION_BLOCKS + 6) {
            source.push(Block::Text {
                content: format!("small durable block {index}"),
            });
        }
        source.history.clear_record_dirty();
        let record_len = source.history.block_records().len();
        let mut document = TranscriptDocument::from_transcript(source);
        document.schedule_durable_compaction(record_len, None);

        assert!(document.drain_compaction_slice());
        assert_eq!(stored_count(&document), TRANSCRIPT_IDLE_COMPACTION_BLOCKS);
        assert!(document.drain_compaction_slice());
        assert_eq!(stored_count(&document), record_len);

        let mut source = Transcript::new();
        for marker in ['a', 'b', 'c'] {
            source.push(Block::Text {
                content: marker
                    .to_string()
                    .repeat(TRANSCRIPT_IDLE_COMPACTION_BYTES / 2 + 1024),
            });
        }
        source.history.clear_record_dirty();
        let record_len = source.history.block_records().len();
        let mut document = TranscriptDocument::from_transcript(source);
        document.schedule_durable_compaction(record_len, None);

        assert!(document.drain_compaction_slice());
        assert_eq!(stored_count(&document), 1);
        assert!(document.memory_snapshot().dematerialized_bytes > 0);
    }

    #[test]
    fn idle_compaction_preserves_pins_pending_tools_and_identity() {
        let mut source = Transcript::new();
        source.push(Block::User {
            text: "durable user".into(),
            image_labels: vec![],
            command: false,
        });
        source.push_tool_call(
            Block::ToolCall {
                call_id: "pending-call".into(),
                name: "bash".into(),
                summary: "running".into(),
                args: HashMap::new(),
            },
            ToolState {
                status: ToolStatus::Pending,
                elapsed: None,
                called_at_ms: None,
                elapsed_active: true,
                output: None,
                user_message: None,
                preview_output: None,
            },
        );
        source.history.clear_record_dirty();
        let mut document = TranscriptDocument::from_transcript(source);
        let ids = document.history().order.clone();
        let generation = document.history().generation();
        let navigation_generation = document.history().navigation_generation();
        let record_generation = document.history().record_dirty_generation();
        document.schedule_durable_compaction(2, None);

        assert!(document.pin_operation_blocks(&[ids[0]]));
        assert!(!document.drain_compaction_slice());
        assert!(document.history().is_live(ids[0]));
        document.unpin_operation_blocks(&[ids[0]]);

        assert!(document.drain_compaction_slice());
        assert!(!document.history().is_materialized(ids[0]));
        assert!(document.history().is_live(ids[1]));
        assert_eq!(document.history().order, ids);
        assert_eq!(document.history().generation(), generation);
        assert_eq!(
            document.history().navigation_generation(),
            navigation_generation
        );
        assert_eq!(
            document.history().record_dirty_generation(),
            record_generation
        );

        assert!(document
            .history_mut()
            .update_tool_state(ids[1], |state| state.status = ToolStatus::Ok));
        let persisted_bounds = document.record_save_bounds(None);
        document.history_mut().clear_record_dirty();
        document.schedule_durable_compaction(2, persisted_bounds);
        assert!(document.drain_compaction_slice());
        assert!(!document.history().is_materialized(ids[1]));
    }

    #[test]
    fn idle_compaction_skips_transient_blocks_and_stops_at_new_dirtiness() {
        let mut source = Transcript::new();
        source.push_compaction_preview("transient".into());
        source.push(Block::Text {
            content: "durable".into(),
        });
        source.history.clear_record_dirty();
        let mut document = TranscriptDocument::from_transcript(source);
        let preview_id = document.history().order[0];
        let durable_id = document.history().order[1];
        document.schedule_durable_compaction(1, None);

        assert!(document.drain_compaction_slice());
        assert!(document.history().is_live(preview_id));
        assert!(!document.history().is_materialized(durable_id));

        let mut source = Transcript::new();
        source.push(Block::Text {
            content: "acknowledged".into(),
        });
        source.history.clear_record_dirty();
        let mut document = TranscriptDocument::from_transcript(source);
        let id = document.history().order[0];
        document.schedule_durable_compaction(1, None);
        document.history_mut().rewrite(
            id,
            Block::Text {
                content: "new dirty content".into(),
            },
        );

        assert!(!document.drain_compaction_slice());
        assert!(document.history().is_live(id));
        assert!(
            matches!(document.history().block(id), Some(Block::Text { content }) if content == "new dirty content")
        );
    }

    #[test]
    fn zero_height_empty_projection_keeps_exact_row_tape() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let mut document = TranscriptDocument::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(400), Default::default());

        for _ in 0..2 {
            let plan = document
                .plan_projection_measured(
                    &lua,
                    80,
                    &theme,
                    crate::content::transcript_buf::ScrollTarget::visible_tail(),
                    0,
                )
                .expect("projection hydration");
            let applied = document.project_applied_viewport(&lua, &mut buf, &theme, plan);
            assert_eq!(applied.materialized_rows.materialized_rows, 0);
        }
    }

    #[test]
    fn sparse_tail_document_reports_virtual_prefix_rows() {
        let lua = LuaRuntime::new();
        let mut source = Transcript::new();
        source.push(Block::Text {
            content: "tail one".into(),
        });
        source.push(Block::Text {
            content: "tail two".into(),
        });
        let records = source.history.block_records();
        let loaded = loaded_transcript_with_window(&records, 8, 10, None);
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let loaded_rows = document.content.projection.estimated_total_rows(
            &lua,
            &mut document.content.transcript.history,
            80,
        );

        let total_rows = document.approximate_scrollbar_total_rows(&lua, 80);

        assert!(total_rows > loaded_rows);
        assert!(document.approximate_sparse_prefix_row_offset(80) > 0);
    }

    #[test]
    fn sparse_tail_projection_offsets_materialized_rows() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let mut source = Transcript::new();
        source.push(Block::Text {
            content: "tail one".into(),
        });
        source.push(Block::Text {
            content: "tail two".into(),
        });
        let records = source.history.block_records();
        let loaded = loaded_transcript_with_window(&records, 8, 10, None);
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let offset = document.approximate_sparse_prefix_row_offset(80);
        let plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_tail(),
            10,
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(401), Default::default());

        let rows =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));

        assert!(rows.total_rows > rows.materialized_rows);
        assert!(rows.row_base >= offset);
        assert!(rows.clamped_scroll >= offset);
        assert!(buf.lines().iter().any(|line| line == "tail two"));
    }

    #[test]
    fn transcript_scroll_trace_records_projection_contract_fields() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let mut source = Transcript::new();
        source.push(Block::Text {
            content: "trace head".into(),
        });
        source.push(Block::Text {
            content: "trace tail".into(),
        });
        let records = source.history.block_records();
        let loaded = loaded_transcript_with_window(&records, 0, 2, None);
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        document.set_scroll_trace_enabled(true);
        document.set_next_scroll_trace_input(TranscriptScrollTraceRenderInput {
            input_event_or_tick: "unit_wheel_tick".to_string(),
            scroll_intent: TranscriptScrollIntent::UserDelta { rows: -3 },
            window_scroll_before: 12,
            window_scroll_after_input: 9,
        });
        let plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_tail(),
            10,
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(409), Default::default());
        let rows =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));

        let frames = document.take_scroll_trace_frames();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame.input_event_or_tick, "unit_wheel_tick");
        assert_eq!(
            frame.scroll_intent,
            TranscriptScrollIntent::UserDelta { rows: -3 }
        );
        assert_eq!(frame.window_scroll_before, 12);
        assert_eq!(frame.window_scroll_after_input, 9);
        assert_eq!(
            frame.projection_target,
            TranscriptProjectionTargetTrace::Tail
        );
        assert_eq!(frame.resolved_scroll_top, rows.clamped_scroll);
        assert_eq!(frame.materialized_range, rows.materialized_range().into());
        assert_eq!(
            frame.active_record_range_after,
            Some(TranscriptRecordTraceRange { start: 0, end: 2 })
        );
        assert!(!frame.placeholder_rows_visible);
        assert!(matches!(
            frame.viewport_anchor_after,
            Some(TranscriptTraceAnchor::Tail)
        ));
        assert!(frame.first_visible_content_anchor.is_some());
        assert!(!frame.visible_record_or_block_ids.is_empty());
        assert_eq!(frame.render_or_projection_ms, None);
    }

    #[test]
    fn unfinished_tool_draft_viewport_anchor_uses_stable_row() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 5;
        let mut source = Transcript::new();
        source.push(Block::ToolDraft {
            stream_id: "plan-stream".into(),
            call_id: Some("plan-call".into()),
            name: "present_plan".into(),
            summary: protocol::StyledLines::from_plain("drafting plan"),
            args: HashMap::new(),
            raw_arguments: "{}".into(),
            finished: false,
        });
        let mut document = TranscriptDocument::from_transcript(source);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(412), Default::default());

        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(0),
            viewport_rows,
        );
        let applied = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );

        assert_eq!(applied.materialized_rows.clamped_scroll, 0);
        assert!(matches!(
            applied.top_anchor,
            Some(TranscriptTraceAnchor::EstimatedRow(0))
        ));
    }

    #[test]
    fn unfinished_tool_draft_user_delta_uses_row_base_after_rewrite() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 5;
        let summary = (0..30)
            .map(|i| format!("draft line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut source = Transcript::new();
        source.push(Block::ToolDraft {
            stream_id: "plan-stream".into(),
            call_id: Some("plan-call".into()),
            name: "present_plan".into(),
            summary: protocol::StyledLines::from_plain(summary),
            args: HashMap::new(),
            raw_arguments: "{}".into(),
            finished: false,
        });
        let mut document = TranscriptDocument::from_transcript(source);
        let draft_id = *document.history().order.last().expect("draft block id");
        let mut buf = Buffer::new(crate::smelt_edit::BufId(414), Default::default());

        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(10),
            viewport_rows,
        );
        let first = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );
        assert_eq!(first.materialized_rows.clamped_scroll, 10);

        let rewritten_summary = (0..5)
            .map(|i| format!("new draft prefix {i}"))
            .chain((0..30).map(|i| format!("draft line {i}")))
            .collect::<Vec<_>>()
            .join("\n");
        document.history_mut().rewrite(
            draft_id,
            Block::ToolDraft {
                stream_id: "plan-stream".into(),
                call_id: Some("plan-call".into()),
                name: "present_plan".into(),
                summary: protocol::StyledLines::from_plain(rewritten_summary),
                args: HashMap::new(),
                raw_arguments: "{}".into(),
                finished: false,
            },
        );
        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: -3 });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: first.materialized_rows.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let applied = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );

        assert_eq!(applied.materialized_rows.clamped_scroll, 7);
    }

    #[test]
    fn transcript_viewport_state_preserves_anchor_without_window_scroll_change() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 5;
        let mut document = TranscriptDocument::from_transcript(fixed_transcript(40));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(411), Default::default());

        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(8),
            viewport_rows,
        );
        let first =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));
        assert_eq!(first.clamped_scroll, 8);

        document.set_scroll_trace_enabled(true);
        document.take_scroll_trace_frames();
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: first.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let applied = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );

        assert_eq!(
            applied.materialized_rows.clamped_scroll,
            first.clamped_scroll
        );
        assert_eq!(applied.exact_visible_range.start, first.clamped_scroll);
        let frames = document.take_scroll_trace_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].scroll_intent,
            TranscriptScrollIntent::PreserveViewport
        );
        assert_eq!(
            frames[0].projection_target,
            TranscriptProjectionTargetTrace::StableRowDelta {
                row: first.clamped_scroll,
                delta: 0,
            }
        );
    }

    #[test]
    fn transcript_viewport_state_coalesces_pending_user_deltas() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 5;
        let mut document = TranscriptDocument::from_transcript(fixed_transcript(40));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(412), Default::default());

        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(20),
            viewport_rows,
        );
        let first =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));
        assert_eq!(first.clamped_scroll, 20);

        document.set_scroll_trace_enabled(true);
        document.take_scroll_trace_frames();
        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: -1 });
        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: -2 });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: first.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let applied = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );

        assert_eq!(applied.materialized_rows.clamped_scroll, 17);
        let frames = document.take_scroll_trace_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].scroll_intent,
            TranscriptScrollIntent::UserDelta { rows: -3 }
        );
        assert_eq!(
            frames[0].projection_target,
            TranscriptProjectionTargetTrace::StableRowDelta { row: 20, delta: -3 }
        );
    }

    #[test]
    fn exact_tape_local_delta_preserves_sparse_global_scrollbar_total() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let records = transcript_records(1_000);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            200..800,
            None,
        ));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(416), Default::default());
        let loaded_start = document.approximate_sparse_prefix_row_offset(width);
        let initial_scroll = loaded_start.saturating_add(300);
        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(initial_scroll),
            viewport_rows,
        );
        let first = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );
        assert!(
            first.materialized_rows.total_rows > 600,
            "test requires a global extent larger than the local record window"
        );

        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: 3 });
        let plan = document
            .plan_viewport_projection_measured(
                &lua,
                width,
                &theme,
                TranscriptViewportProjectionInput {
                    fallback_scroll_top: first.materialized_rows.clamped_scroll,
                    follow_tail: false,
                    width_changed: false,
                    previous_width: None,
                },
                viewport_rows,
            )
            .expect("projection hydration");
        assert!(matches!(
            plan.materialization,
            TranscriptMaterializationPlan::ExactRowTape(_)
        ));
        let local = document.project_applied_viewport(&lua, &mut buf, &theme, plan);

        assert_eq!(
            local.materialized_rows.total_rows, first.materialized_rows.total_rows,
            "exact local movement must preserve the sparse global scrollbar extent"
        );
    }

    #[test]
    fn local_delta_without_adjacent_records_stays_on_exact_loaded_content() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let records = transcript_records(80);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            60..80,
            None,
        ));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(413), Default::default());

        let tail_plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: 0,
                follow_tail: true,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let tail = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            tail_plan.expect("projection hydration"),
        );
        assert!(!tail.placeholder_rows_visible);

        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: -10_000 });
        let local_plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: tail.materialized_rows.clamped_scroll.saturating_sub(10_000),
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let local = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            local_plan.expect("projection hydration"),
        );
        assert!(
            !local.placeholder_rows_visible,
            "local deltas must stay on exact loaded content instead of sparse placeholders"
        );
        assert!(local.top_anchor.is_some());

        let far_total_rows = document.approximate_scrollbar_total_rows(&lua, width);
        document.set_pending_scroll_intent(TranscriptScrollIntent::ScrollbarFraction {
            numerator: 0,
            denominator: 1,
            total_rows: far_total_rows,
            viewport_rows,
        });
        let far_plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: local.materialized_rows.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let far = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            far_plan.expect("projection hydration"),
        );
        assert!(
            far.placeholder_rows_visible,
            "far seek intents may expose inert sparse placeholders"
        );
    }

    #[test]
    fn local_delta_crossing_sparse_boundary_loads_adjacent_window() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let records = transcript_records(100);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            60..80,
            Some(session_dir.clone()),
        ));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(414), Default::default());
        let loaded_start = document.approximate_sparse_prefix_row_offset(width);
        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(loaded_start),
            viewport_rows,
        );
        let first = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );
        assert!(!first.placeholder_rows_visible);
        assert!(active_history_contains(&document, "block 60"));

        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: -4 });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: first.materialized_rows.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let local = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );

        assert!(
            !local.placeholder_rows_visible,
            "local deltas must load adjacent content instead of exposing sparse placeholders"
        );
        let active = document
            .records
            .active_range
            .as_ref()
            .expect("active adjacent record window");
        assert!(active.start.get() < 60);
        assert!(active.end.get() >= 80);
        assert!(
            active.end.get().saturating_sub(active.start.get())
                <= document
                    .record_window_count(width, viewport_rows, records.len())
                    .saturating_mul(TRANSCRIPT_ACTIVE_RECORD_WINDOW_MAX_MULTIPLIER)
                    .saturating_add(TRANSCRIPT_RECORD_PREFIX_STRIDE.saturating_sub(1))
                    .min(records.len())
        );
        assert!(active_history_contains(&document, "block 59"));
        assert!(active_history_contains(&document, "block 60"));
        assert!(matches!(
            document.viewport.state.resolved_anchor,
            Some(TranscriptResolvedViewportAnchor {
                top: TranscriptScrollAnchor::Content(_),
                ..
            })
        ));
        assert!(local.materialized_rows.clamped_scroll < first.materialized_rows.clamped_scroll);
    }

    #[test]
    fn local_delta_crossing_sparse_boundary_loads_next_adjacent_window() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let records = transcript_records(100);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            20..40,
            Some(session_dir.clone()),
        ));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(415), Default::default());
        let (_, loaded_end) = document
            .active_virtual_row_span(&lua, width)
            .expect("active row span");
        let top = loaded_end.saturating_sub(RowIndex::from(viewport_rows));
        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(top),
            viewport_rows,
        );
        let first = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );
        assert!(!first.placeholder_rows_visible);
        assert!(active_history_contains(&document, "block 39"));

        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: 4 });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: first.materialized_rows.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let local = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );

        assert!(
            !local.placeholder_rows_visible,
            "local downward deltas must load adjacent content instead of sparse placeholders"
        );
        let active = document
            .records
            .active_range
            .as_ref()
            .expect("active adjacent record window");
        assert!(active.start.get() <= 20);
        assert!(active.end.get() > 40);
        assert!(
            active.end.get().saturating_sub(active.start.get())
                <= document
                    .record_window_count(width, viewport_rows, records.len())
                    .saturating_mul(TRANSCRIPT_ACTIVE_RECORD_WINDOW_MAX_MULTIPLIER)
                    .saturating_add(TRANSCRIPT_RECORD_PREFIX_STRIDE.saturating_sub(1))
                    .min(records.len()),
            "adjacent window grew beyond its memory bound: active={active:?}"
        );
        assert!(active_history_contains(&document, "block 39"));
        assert!(active_history_contains(&document, "block 40"));
        assert!(matches!(
            document.viewport.state.resolved_anchor,
            Some(TranscriptResolvedViewportAnchor {
                top: TranscriptScrollAnchor::Content(_),
                ..
            })
        ));
        assert!(local.materialized_rows.clamped_scroll > first.materialized_rows.clamped_scroll);
    }

    #[test]
    fn transcript_scroll_trace_records_injected_exact_observation_count() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let records = transcript_records(4);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..4,
            None,
        ));
        document.extent_index.observe_exact_loaded_record_rows(
            &document.records.sparse,
            exact_height_snapshot(BlockId::new(1), width, records[1].content_hash, 17),
        );
        document.set_scroll_trace_enabled(true);
        document.set_next_scroll_trace_input(TranscriptScrollTraceRenderInput {
            input_event_or_tick: "exact_height_refinement_probe".to_string(),
            scroll_intent: TranscriptScrollIntent::PreserveViewport,
            window_scroll_before: 0,
            window_scroll_after_input: 0,
        });
        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(0),
            10,
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(410), Default::default());
        let _rows =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));

        let frames = document.take_scroll_trace_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].exact_observation_count, 1);
    }

    #[test]
    fn sparse_projection_preserves_scroll_in_unloaded_suffix_gap() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            40..48,
            None,
        ));
        let offset = document.approximate_sparse_prefix_row_offset(80);
        let loaded_rows = document.content.projection.estimated_total_rows(
            &lua,
            &mut document.content.transcript.history,
            80,
        );
        let target = offset.saturating_add(loaded_rows).saturating_add(10);
        let plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(target),
            10,
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(402), Default::default());

        let rows =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));

        assert_eq!(rows.clamped_scroll, target);
        assert!(rows.row_base <= target);
        assert!(rows.row_base >= offset.saturating_add(loaded_rows));
        assert!(rows.materialized_rows > 0);
        assert!(buf.lines().iter().all(|line| line.is_empty()));
    }

    #[test]
    fn unloaded_sparse_gaps_do_not_provide_content_or_hit_targets() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            40..48,
            None,
        ));
        let width = 80;
        let offset = document.approximate_sparse_prefix_row_offset(width);
        let gap_row = offset.saturating_sub(1);
        assert!(gap_row > 0);

        let display = document.exact_or_gap_display_rows_for_range(&lua, width, &theme, gap_row, 1);
        assert_eq!(display.rows.len(), 1);
        assert!(display.rows[0].text.is_empty());
        assert!(display.rows[0].actions.is_empty());

        let copy = document.copy_exact_loaded_range(
            &lua,
            width,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: gap_row,
                    byte_col: 0,
                },
                end: crate::smelt_edit::DocPosition {
                    row: gap_row.saturating_add(1),
                    byte_col: 0,
                },
            },
        );
        assert!(copy.is_empty());
        assert!(document
            .node_metadata_at_row(&lua, width, gap_row)
            .is_none());
        assert!(document.row_anchor_at_row(&lua, width, gap_row).is_none());
        assert!(!document.fold_node_at_row(
            &lua,
            width,
            gap_row,
            crate::content::transcript_buf::FoldAction::Open,
            crate::content::transcript_buf::FoldActivation::AnyNodeRow,
        ));

        let action = {
            let mut display_document =
                TranscriptDisplayDocument::new(&mut document, &lua, width, &theme);
            DisplayDocument::action_at(
                &mut display_document,
                crate::smelt_edit::DocPosition {
                    row: gap_row,
                    byte_col: 0,
                },
            )
        };
        assert!(action.is_none());

        let loaded_rows = document.content.projection.estimated_total_rows(
            &lua,
            &mut document.content.transcript.history,
            width,
        );
        let suffix_gap_row = offset.saturating_add(loaded_rows).saturating_add(1);
        let display =
            document.exact_or_gap_display_rows_for_range(&lua, width, &theme, suffix_gap_row, 1);
        assert!(display.rows.iter().all(|row| row.text.is_empty()));
        assert!(display.rows.iter().all(|row| row.actions.is_empty()));
        let copy = document.copy_exact_loaded_range(
            &lua,
            width,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: suffix_gap_row,
                    byte_col: 0,
                },
                end: crate::smelt_edit::DocPosition {
                    row: suffix_gap_row.saturating_add(1),
                    byte_col: 0,
                },
            },
        );
        assert!(copy.is_empty());
        assert!(document
            .node_metadata_at_row(&lua, width, suffix_gap_row)
            .is_none());
        assert!(document
            .row_anchor_at_row(&lua, width, suffix_gap_row)
            .is_none());
        assert!(!document.fold_node_at_row(
            &lua,
            width,
            suffix_gap_row,
            crate::content::transcript_buf::FoldAction::Open,
            crate::content::transcript_buf::FoldActivation::AnyNodeRow,
        ));
        let action = {
            let mut display_document =
                TranscriptDisplayDocument::new(&mut document, &lua, width, &theme);
            DisplayDocument::action_at(
                &mut display_document,
                crate::smelt_edit::DocPosition {
                    row: suffix_gap_row,
                    byte_col: 0,
                },
            )
        };
        assert!(action.is_none());
    }

    #[test]
    fn large_sparse_prefix_uses_persisted_record_estimates() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = Transcript::new();
        for idx in 0..2048 {
            let content = if idx < 2000 {
                format!("block {idx} {}", "wide text ".repeat(40))
            } else {
                format!("block {idx}")
            };
            source.push(Block::Text { content });
        }
        let records = source.history.block_records();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            2000..2040,
            Some(session_dir.clone()),
        ));
        let coarse_rows = 2000 * document.approximate_average_record_rows(20);

        let prefix_rows = document.approximate_sparse_prefix_row_offset(20);

        assert!(prefix_rows > coarse_rows);
        assert_eq!(
            prefix_rows,
            records[..2000]
                .iter()
                .map(|record| {
                    record
                        .block
                        .raw_text()
                        .map(|text| crate::content::estimate_text_rows(&text, 20))
                        .unwrap_or(1)
                        .saturating_add(1)
                })
                .sum::<RowIndex>()
        );
    }

    #[test]
    fn viewport_content_anchor_survives_sparse_prefix_estimate_refinement() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            64..72,
            None,
        ));
        let width = 80;
        let viewport_rows = 10;
        let original_top = document.approximate_sparse_prefix_row_offset(width);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(408), Default::default());
        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(original_top),
            viewport_rows,
        );
        let rows =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));
        assert_eq!(rows.clamped_scroll, original_top);
        assert!(buf.lines().iter().any(|line| line.contains("block 64")));

        let refined_top = original_top.saturating_add(25);
        for prefix_rows in document
            .extent_index
            .prefix_index
            .as_mut()
            .expect("extent prefix index")
            .prefix_rows
            .iter_mut()
            .skip(1)
        {
            *prefix_rows = prefix_rows.saturating_add(25);
        }
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: rows.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let rows =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));

        assert_eq!(
            rows.clamped_scroll, original_top,
            "extent refinement must not move the exact semantic viewport coordinate"
        );
        assert_ne!(rows.clamped_scroll, refined_top);
        assert!(buf.lines().iter().any(|line| line.contains("block 64")));
    }

    #[test]
    fn transcript_action_at_materializes_exact_unrendered_span() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let url = "https://example.test/exact-action";
        let target_index = 120usize;
        let mut source = Transcript::new();
        for index in 0..240 {
            let content = if index == target_index {
                format!("target [link]({url})")
            } else {
                format!("block {index}")
            };
            source.push(Block::Text { content });
        }
        let mut document = TranscriptDocument::from_transcript(source);
        let (_id, target_row, target_rows) = document
            .materialize_exact_loaded_block_layout(&lua, 80)
            .into_iter()
            .find(|(id, _, _)| *id == BlockId::new(target_index as u64))
            .expect("target block row");
        let target_display_rows =
            document.exact_or_gap_display_rows_for_range(&lua, 80, &theme, target_row, target_rows);
        let (row_offset, byte_col) = target_display_rows
            .rows
            .iter()
            .enumerate()
            .find_map(|(offset, row)| {
                row.actions.first().map(|action| {
                    (
                        offset as crate::smelt_edit::RowIndex,
                        crate::smelt_edit::text::cell_to_byte(&row.text, action.cell_start),
                    )
                })
            })
            .expect("target action span");
        let pos = crate::smelt_edit::DocPosition {
            row: target_row.saturating_add(row_offset),
            byte_col,
        };

        document.content.projection.reset_counters();
        let action = {
            let mut display_document =
                TranscriptDisplayDocument::new(&mut document, &lua, 80, &theme);
            DisplayDocument::action_at(&mut display_document, pos)
        };
        let counters = document.content.projection.counters();

        assert_eq!(
            action,
            Some(smelt_core::buffer::SpanAction::OpenUrl(url.to_string()))
        );
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.max_range_materialized_blocks, 1);
        assert!(
            counters.max_range_materialized_rows <= target_rows.saturating_add(1) as usize,
            "action hit testing should render only the target block and its leading gap, counters: {counters:?}"
        );
    }

    fn fixed_transcript(count: usize) -> Transcript {
        let mut source = Transcript::new();
        for idx in 0..count {
            source.push(Block::Text {
                content: format!("block {idx}"),
            });
        }
        source
    }

    fn transcript_records(count: usize) -> Vec<TranscriptBlockRecord> {
        fixed_transcript(count).history.block_records()
    }

    fn varied_transcript(count: usize) -> Transcript {
        let mut source = Transcript::new();
        for idx in 0..count {
            let repeats = match idx % 9 {
                0 => 1,
                1 | 2 => 4,
                3..=5 => 12,
                _ => 28,
            };
            source.push(Block::Text {
                content: format!("block {idx} {}", "wrapped text ".repeat(repeats)),
            });
        }
        source
    }

    fn varied_transcript_records(count: usize) -> Vec<TranscriptBlockRecord> {
        varied_transcript(count).history.block_records()
    }

    fn extent_accuracy_transcript(count: usize) -> Transcript {
        let mut source = Transcript::new();
        for idx in 0..count {
            let marker = format!("extent-{idx:04}");
            match idx % 6 {
                0 => {
                    let lines = if idx % 42 == 0 { 120 } else { 4 + idx % 19 };
                    source.push(Block::Text {
                        content: (0..lines)
                            .map(|line| format!("{marker} short hard line {line}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    });
                }
                1 => source.push(Block::Text {
                    content: format!(
                        "{marker} {}",
                        "one very long markdown paragraph with `inline code`, **bold**, and wide 語 text "
                            .repeat(8 + idx % 23)
                    ),
                }),
                2 => source.push(Block::Text {
                    content: format!(
                        "# {marker}\n\n| item | value |\n| --- | ---: |\n| alpha | {idx} |\n\n- {}\n\n```rust\nlet extent = {idx};\n```",
                        "list content with wrapping pressure ".repeat(4 + idx % 7)
                    ),
                }),
                3 => source.push(Block::Thinking {
                    title: Some(format!("Reasoning {marker}")),
                    summary_titles: vec![format!("Summary {idx}")],
                    content: format!(
                        "{}\n{}",
                        "thinking paragraph with several wrapped words ".repeat(5 + idx % 13),
                        (0..idx % 11)
                            .map(|line| format!("detail {line}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                    kind: protocol::ReasoningKind::default(),
                }),
                4 => source.push(Block::User {
                    text: format!(
                        "{marker} {}",
                        "user request with Unicode 語界 and wrapped context ".repeat(3 + idx % 9)
                    ),
                    image_labels: vec![format!("image-{idx}")],
                    command: false,
                }),
                _ => source.push_tool_call(
                    Block::ToolCall {
                        call_id: format!("extent-call-{idx}"),
                        name: "read_file".into(),
                        summary: protocol::StyledLines::from_plain(format!(
                            "read {marker}.rs"
                        )),
                        args: std::collections::HashMap::from([(
                            "file_path".into(),
                            serde_json::json!(format!("src/{marker}.rs")),
                        )]),
                    },
                    smelt_core::transcript_model::ToolState {
                        status: smelt_core::transcript_model::ToolStatus::Ok,
                        elapsed: Some(std::time::Duration::from_millis(25)),
                        called_at_ms: None,
                        elapsed_active: false,
                        output: Some(Box::new(smelt_core::transcript_model::ToolOutput {
                            content: format!(
                                "{marker} output\n{}",
                                "compact tool detail\n".repeat(2 + idx % 5)
                            ),
                            is_error: false,
                            metadata: None,
                        })),
                        user_message: None,
                        preview_output: None,
                    },
                ),
            }
        }
        source
    }

    fn records_with_ids(
        records: &[TranscriptBlockRecord],
        block_id_start: usize,
    ) -> Vec<StoredBlockWithId> {
        let rows = records
            .iter()
            .enumerate()
            .map(|(offset, record)| {
                smelt_core::transcript_model::transcript_block_row_with_block_idx(
                    block_id_start.saturating_add(offset),
                    block_id_start.saturating_add(offset) as u64,
                    record,
                )
                .expect("record row")
            })
            .collect();
        smelt_core::transcript_model::compact_block_rows(block_id_start, rows)
            .expect("compact record rows")
    }

    fn loaded_transcript_with_window(
        records: &[TranscriptBlockRecord],
        start: usize,
        total_count: usize,
        session_dir: Option<PathBuf>,
    ) -> LoadedTranscript {
        let window_records = records_with_ids(records, start);
        let mut transcript = Transcript::new();
        if session_dir.is_none() {
            for (stored, record) in window_records.iter().zip(records.iter().cloned()) {
                assert!(transcript.history.install_hydrated_record(
                    stored.block_id,
                    Arc::clone(&stored.stored),
                    record,
                ));
            }
        }
        LoadedTranscript {
            transcript,
            record_window: Some(LoadedRecordWindow {
                start: smelt_store::TranscriptRecordOffset::new(start),
                total_count,
                hydration: smelt_store::TranscriptRecordHydration::Hydrated,
                records: window_records,
            }),
            session_dir,
        }
    }

    fn sparse_loaded_transcript(
        records: &[TranscriptBlockRecord],
        range: Range<usize>,
        session_dir: Option<PathBuf>,
    ) -> LoadedTranscript {
        loaded_transcript_with_window(
            &records[range.clone()],
            range.start,
            records.len(),
            session_dir,
        )
    }

    fn exact_height_snapshot(
        block_id: BlockId,
        width: u16,
        content_hash: u64,
        rows: RowIndex,
    ) -> crate::content::transcript_buf::TranscriptExactHeightSnapshot {
        crate::content::transcript_buf::TranscriptExactHeightSnapshot {
            width,
            renderer_generation: 7,
            renderer_cache_key: Some(11),
            presentation_generation: 13,
            observations: vec![
                crate::content::transcript_buf::TranscriptExactHeightObservation {
                    block_id,
                    key: LayoutKey {
                        width,
                        view_state: smelt_core::transcript_model::ViewState::Expanded,
                        content_hash,
                        sidecar_hash: 0,
                    },
                    rows,
                },
            ],
        }
    }

    #[test]
    fn exact_loaded_record_rows_override_text_estimates() {
        let records = transcript_records(3);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..3,
            None,
        ));
        let width = 80;
        let block_id = BlockId::new(1);
        let raw_estimate = document
            .extent_index
            .local_rows_for_loaded_records(&document.records.sparse, width);

        document.extent_index.observe_exact_loaded_record_rows(
            &document.records.sparse,
            exact_height_snapshot(block_id, width, records[1].content_hash, 17),
        );

        let refined = document
            .extent_index
            .local_rows_for_loaded_records(&document.records.sparse, width);
        assert_eq!(raw_estimate, 6);
        assert_eq!(refined, 21);
        assert_eq!(document.approximate_average_record_rows(width), 7);
    }

    #[test]
    fn exact_loaded_record_rows_are_width_and_invalidation_scoped() {
        let records = transcript_records(2);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..2,
            None,
        ));
        let width = 80;
        assert!(!document.invalidate_renderer_if_changed(7, Some(11)));
        let block_id = BlockId::new(0);
        document.extent_index.observe_exact_loaded_record_rows(
            &document.records.sparse,
            exact_height_snapshot(block_id, width, records[0].content_hash, 19),
        );

        assert_eq!(
            document.extent_index.exact_local_rows_for_record(0, width),
            Some(19)
        );
        assert_eq!(
            document
                .extent_index
                .exact_local_rows_for_record(0, width - 1),
            None
        );
        document.extent_index.observe_exact_loaded_record_rows(
            &document.records.sparse,
            exact_height_snapshot(block_id, width - 1, records[0].content_hash, 23),
        );
        assert_eq!(
            document
                .extent_index
                .exact_local_rows_for_record(0, width - 1),
            Some(23)
        );
        assert_eq!(
            document.extent_index.exact_local_rows_for_record(0, width),
            Some(19)
        );

        document.invalidate_theme();
        assert_eq!(
            document.extent_index.exact_local_rows_for_record(0, width),
            Some(19),
            "theme invalidation should keep exact record heights"
        );
        assert_eq!(
            document
                .extent_index
                .exact_local_rows_for_record(0, width - 1),
            Some(23),
            "theme invalidation should keep width-scoped exact record heights"
        );

        assert!(document.invalidate_renderer_if_changed(8, Some(11)));
        assert_eq!(
            document.extent_index.exact_local_rows_for_record(0, width),
            None
        );

        document.extent_index.observe_exact_loaded_record_rows(
            &document.records.sparse,
            exact_height_snapshot(block_id, width, records[0].content_hash, 19),
        );
        document.set_inline_options(InlineOptions::default());
        assert_eq!(
            document.extent_index.exact_local_rows_for_record(0, width),
            None
        );
    }

    fn skewed_transcript_records(count: usize) -> Vec<TranscriptBlockRecord> {
        let mut source = Transcript::new();
        for idx in 0..count {
            let repeats = if idx < count / 2 { 80 } else { 1 };
            source.push(Block::Text {
                content: format!("block {idx} {}", "skewed text ".repeat(repeats)),
            });
        }
        source.history.block_records()
    }

    #[test]
    fn resumed_sparse_prefix_rows_use_persisted_record_estimates() {
        let width = 24;
        let records = skewed_transcript_records(300);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let expected = SqliteTranscriptStore::open_read_only(&session_dir)
            .unwrap()
            .estimated_record_rows(width, 0..220)
            .unwrap() as RowIndex;

        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            220..235,
            Some(session_dir),
        ));

        assert_eq!(
            document.approximate_sparse_prefix_row_offset(width),
            expected
        );
    }

    #[test]
    fn exact_record_rows_survive_active_window_switch_without_refining_prefix_estimate() {
        let width = 80;
        let records = transcript_records(8);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..3,
            Some(session_dir.clone()),
        ));
        document.extent_index.observe_exact_loaded_record_rows(
            &document.records.sparse,
            exact_height_snapshot(BlockId::new(1), width, records[1].content_hash, 17),
        );

        let window = LoadedRecordWindow {
            start: smelt_store::TranscriptRecordOffset::new(5),
            total_count: records.len(),
            hydration: smelt_store::TranscriptRecordHydration::Hydrated,
            records: records_with_ids(&records[5..8], 5),
        };
        assert!(document.merge_record_window(window));
        let persisted_prefix = SqliteTranscriptStore::open_read_only(&session_dir)
            .unwrap()
            .estimated_record_rows(width, 0..5)
            .unwrap() as RowIndex;
        assert_eq!(
            document.extent_index.exact_local_rows_for_record(1, width),
            Some(17)
        );
        assert_eq!(
            document.approximate_sparse_prefix_row_offset(width),
            persisted_prefix
        );
    }

    fn assert_sparse_projection_is_bounded(
        document: &TranscriptDocument,
        rows: &crate::smelt_edit::MaterializedRows,
        width: u16,
        viewport_rows: u16,
        step: usize,
    ) {
        if let (Some(active), Some(total)) = (
            document.records.active_range(),
            document.records.total_count(),
        ) {
            let max_records = document
                .record_window_count(width, viewport_rows, total)
                .saturating_mul(TRANSCRIPT_ACTIVE_RECORD_WINDOW_MAX_MULTIPLIER)
                .saturating_add(TRANSCRIPT_RECORD_PREFIX_STRIDE.saturating_sub(1))
                .min(total);
            let active_len = active.end.get().saturating_sub(active.start.get());
            assert!(
                active_len <= max_records,
                "scroll step {step} active record window grew past bound: len={active_len}, max={max_records}, active={active:?}, rows={rows:?}"
            );
        }

        let max_materialized_rows = RowIndex::from(viewport_rows.max(1)).saturating_mul(2);
        assert!(
            rows.materialized_rows <= max_materialized_rows,
            "scroll step {step} materialized too many rows: max={max_materialized_rows}, rows={rows:?}"
        );
    }

    fn materialized_viewport_lines(
        buf: &Buffer,
        rows: &crate::smelt_edit::MaterializedRows,
        viewport_rows: u16,
    ) -> Vec<String> {
        let start = rows.clamped_scroll.saturating_sub(rows.row_base) as usize;
        (start
            ..start
                .saturating_add(viewport_rows as usize)
                .min(buf.line_count()))
            .map(|row| buf.get_line(row).unwrap_or_default().to_string())
            .collect()
    }

    fn active_history_contains(document: &TranscriptDocument, needle: &str) -> bool {
        document
            .content
            .transcript
            .history
            .block_records()
            .iter()
            .any(|record| record.block.raw_text().is_some_and(|text| text == needle))
    }

    #[test]
    fn resumed_sparse_scroll_up_never_stalls_or_reverses() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 24;
        let viewport_rows = 12;
        let records = varied_transcript_records(400);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(session_dir.clone(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(403), Default::default());
        let mut rows = {
            let plan = document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                viewport_rows,
            );
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"))
        };
        assert_sparse_projection_is_bounded(&document, &rows, width, viewport_rows, 0);

        for step in 0..600 {
            if rows.clamped_scroll == 0 {
                break;
            }
            let previous = rows.clamped_scroll;
            let requested = previous.saturating_sub(3);
            let plan = document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_row(requested),
                viewport_rows,
            );
            rows = document.project_planned(
                &lua,
                &mut buf,
                &theme,
                plan.expect("projection hydration"),
            );
            assert_sparse_projection_is_bounded(&document, &rows, width, viewport_rows, step + 1);

            assert!(
                rows.clamped_scroll < previous,
                "scroll step {step} stalled or reversed: previous={previous}, requested={requested}, resolved={}, row_base={}, materialized={}, total={}, active={:?}",
                rows.clamped_scroll,
                rows.row_base,
                rows.materialized_rows,
                rows.total_rows,
                document.records.active_range,
            );
            assert!(
                rows.row_base <= rows.clamped_scroll,
                "scroll step {step} resolved before materialized base: {:?}",
                rows
            );
            assert!(
                rows.clamped_scroll
                    .saturating_add(RowIndex::from(viewport_rows))
                    <= rows.row_base.saturating_add(rows.materialized_rows),
                "scroll step {step} viewport is not materialized: {:?}",
                rows
            );
        }
    }

    #[test]
    fn resumed_sparse_skewed_scroll_up_materializes_requested_viewport() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 24;
        let viewport_rows = 12;
        let records = skewed_transcript_records(300);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(session_dir.clone(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(404), Default::default());
        let mut rows = {
            let plan = document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                viewport_rows,
            );
            document
                .project_applied_viewport(
                    &lua,
                    &mut buf,
                    &theme,
                    plan.expect("projection hydration"),
                )
                .materialized_rows
        };
        assert_sparse_projection_is_bounded(&document, &rows, width, viewport_rows, 0);

        for step in 0..1000 {
            if rows.clamped_scroll == 0 {
                break;
            }
            let previous = rows.clamped_scroll;
            let before = materialized_viewport_lines(&buf, &rows, viewport_rows);
            document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: -3 });
            let plan = document.plan_viewport_projection_measured(
                &lua,
                width,
                &theme,
                TranscriptViewportProjectionInput {
                    fallback_scroll_top: previous,
                    follow_tail: false,
                    width_changed: false,
                    previous_width: None,
                },
                viewport_rows,
            );
            let applied = document.project_applied_viewport(
                &lua,
                &mut buf,
                &theme,
                plan.expect("projection hydration"),
            );
            rows = applied.materialized_rows;
            assert_sparse_projection_is_bounded(&document, &rows, width, viewport_rows, step + 1);

            let after = materialized_viewport_lines(&buf, &rows, viewport_rows);
            assert_eq!(
                after.len(),
                before.len(),
                "scroll step {step} changed visible row count: previous={previous}, rows={rows:?}, active={:?}",
                document.records.active_range,
            );
            assert_eq!(
                &after[3..],
                &before[..before.len().saturating_sub(3)],
                "scroll step {step} did not move by three exact tape rows: previous={previous}, rows={rows:?}, active={:?}",
                document.records.active_range,
            );
            assert!(
                !applied.placeholder_rows_visible
                    && buf.lines().iter().any(|line| !line.is_empty()),
                "scroll step {step} materialized only sparse placeholders: rows={rows:?}, active={:?}",
                document.records.active_range,
            );
        }
    }

    struct ProjectionHarness<'a> {
        lua: &'a LuaRuntime,
        theme: &'a Theme,
        width: u16,
        viewport_rows: u16,
    }

    fn project_frame_into_window(
        document: &mut TranscriptDocument,
        win: &mut crate::smelt_edit::Window,
        buf: &mut Buffer,
        ctx: ProjectionHarness<'_>,
        follow_tail: bool,
    ) -> crate::smelt_edit::MaterializedRows {
        let cursor_screen_row = win.cursor_screen_row(ctx.viewport_rows);
        let scroll_target = if follow_tail {
            crate::content::transcript_buf::ScrollTarget::visible_tail()
        } else {
            crate::content::transcript_buf::ScrollTarget::visible_row(win.scroll_top())
        };
        let plan = document.plan_projection_measured(
            ctx.lua,
            ctx.width,
            ctx.theme,
            scroll_target,
            ctx.viewport_rows,
        );
        let rows =
            document.project_planned(ctx.lua, buf, ctx.theme, plan.expect("projection hydration"));
        win.apply_materialized_rows(rows);
        win.set_resolved_scroll(rows.clamped_scroll);
        win.ensure_layout(buf, ctx.width);
        win.refresh_document_view_position_from_buffer(buf);
        if let Some(screen_row) = cursor_screen_row {
            win.restore_cursor_screen_row(buf, screen_row);
        }
        win.sync_row_render_state(buf, ctx.viewport_rows, std::time::Instant::now());
        rows
    }

    fn project_local_delta_into_window(
        document: &mut TranscriptDocument,
        win: &mut crate::smelt_edit::Window,
        buf: &mut Buffer,
        ctx: ProjectionHarness<'_>,
        fallback_scroll_top: RowIndex,
        delta: isize,
    ) -> crate::smelt_edit::MaterializedRows {
        let cursor_screen_row = win.cursor_screen_row(ctx.viewport_rows);
        let drag_endpoint_screen_row = win
            .document_view_state()
            .drag_endpoint
            .and_then(|endpoint| endpoint.row.checked_sub(win.scroll_top()))
            .filter(|row| *row < RowIndex::from(ctx.viewport_rows))
            .map(|row| row as u16);
        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: delta });
        let plan = document.plan_viewport_projection_measured(
            ctx.lua,
            ctx.width,
            ctx.theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            ctx.viewport_rows,
        );
        let rows = document
            .project_applied_viewport(ctx.lua, buf, ctx.theme, plan.expect("projection hydration"))
            .materialized_rows;
        win.apply_materialized_rows(rows);
        win.set_resolved_scroll(rows.clamped_scroll);
        win.ensure_layout(buf, ctx.width);
        win.refresh_document_view_position_from_buffer(buf);
        if let Some(screen_row) = cursor_screen_row {
            win.restore_cursor_screen_row(buf, screen_row);
        }
        if let Some(screen_row) = drag_endpoint_screen_row {
            win.restore_document_view_screen_rows(
                buf,
                crate::smelt_edit::DocumentViewScreenRowRestore {
                    cursor: None,
                    cursor_selection:
                        crate::smelt_edit::CursorScreenRowSelection::SkipActiveSelection,
                    drag_endpoint: Some(screen_row),
                },
            );
        }
        win.sync_row_render_state(buf, ctx.viewport_rows, std::time::Instant::now());
        rows
    }

    fn transcript_window() -> crate::smelt_edit::Window {
        crate::smelt_edit::Window::new(
            crate::smelt_edit::WinId(1),
            crate::smelt_edit::BufId(1),
            crate::smelt_edit::SplitConfig {
                region: "transcript".into(),
                gutters: smelt_term::layout::Gutters::default(),
            },
        )
    }

    #[test]
    fn resumed_sparse_window_wheel_scroll_up_keeps_moving() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 24;
        let viewport_rows = 12;
        let records = varied_transcript_records(400);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(session_dir.clone(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut win = transcript_window();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
        let ctx = || ProjectionHarness {
            lua: &lua,
            theme: &theme,
            width,
            viewport_rows,
        };
        project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), true);

        for step in 0..200 {
            if win.scroll_top() == 0 {
                break;
            }
            let previous = win.scroll_top();
            win.pan_by_lines(&buf, -3, viewport_rows);
            let requested = win.scroll_top();
            let rows = project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
            assert!(
                win.scroll_top() < previous,
                "wheel step {step} stalled or reversed: previous={previous}, requested={requested}, resolved={}, rows={rows:?}, active={:?}",
                win.scroll_top(),
                document.records.active_range,
            );
            assert!(
                win.scroll_top() <= requested,
                "wheel step {step} projection reversed past the requested scroll: requested={requested}, resolved={}",
                win.scroll_top()
            );
            assert!(
                requested.saturating_sub(win.scroll_top()) <= RowIndex::from(viewport_rows),
                "wheel step {step} projection jumped too far above the requested scroll: requested={requested}, resolved={}",
                win.scroll_top()
            );
        }
    }

    #[test]
    fn resumed_sparse_window_wheel_scroll_down_is_monotonic_and_does_not_snap_back() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 24;
        let viewport_rows = 12;
        let records = skewed_transcript_records(300);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(session_dir.clone(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut win = transcript_window();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(2), Default::default());
        let ctx = || ProjectionHarness {
            lua: &lua,
            theme: &theme,
            width,
            viewport_rows,
        };
        project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), true);
        win.pin_scroll(0);
        project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);

        let mut steps = 0;
        for step in 0..300 {
            let previous = win.scroll_top();
            win.pan_by_lines(&buf, 3, viewport_rows);
            let requested = win.scroll_top();
            if requested == previous {
                break;
            }
            let rows = project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
            assert!(
                win.scroll_top() > previous,
                "wheel step {step} reversed or stalled: previous={previous}, requested={requested}, resolved={}, rows={rows:?}, active={:?}",
                win.scroll_top(),
                document.records.active_range,
            );
            assert_eq!(
                win.scroll_top(),
                requested,
                "wheel step {step} projection did not honor the requested scroll"
            );
            steps += 1;
        }
        assert!(steps > 0, "scroll-down scenario did not move");

        let settled = win.scroll_top();
        let rows = project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
        assert_eq!(
            win.scroll_top(),
            settled,
            "idle projection snapped after scroll-down settled: rows={rows:?}, active={:?}",
            document.records.active_range,
        );
    }

    #[test]
    fn resumed_sparse_drag_select_autoscroll_up_keeps_moving() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 24;
        let viewport_rows = 12;
        let records = varied_transcript_records(400);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(session_dir.clone(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut win = transcript_window();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
        let ctx = || ProjectionHarness {
            lua: &lua,
            theme: &theme,
            width,
            viewport_rows,
        };
        let mut rows = project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), true);
        let top = win.scroll_top();
        let mut state = win.document_view_state();
        state.cursor = crate::smelt_edit::DocPosition {
            row: top,
            byte_col: 0,
        };
        state.drag_endpoint = Some(state.cursor);
        state.selection_anchor = Some(crate::smelt_edit::DocPosition {
            row: top.saturating_add(RowIndex::from(viewport_rows.saturating_sub(1))),
            byte_col: 0,
        });
        state.preferred_cell_col = Some(0);
        win.set_document_view_state(state);

        for step in 0..200 {
            if win.scroll_top() == 0 {
                break;
            }
            let previous = win.scroll_top();
            let before = materialized_viewport_lines(&buf, &rows, viewport_rows);
            assert!(
                win.drag_autoscroll_step(&buf, viewport_rows, -1),
                "drag step {step} did not move before projection"
            );
            let requested = win.scroll_top();
            rows = project_local_delta_into_window(
                &mut document,
                &mut win,
                &mut buf,
                ctx(),
                previous,
                -1,
            );
            let state = win.document_view_state();
            let after = materialized_viewport_lines(&buf, &rows, viewport_rows);
            assert_eq!(
                after.len(),
                before.len(),
                "drag step {step} changed visible row count: previous={previous}, requested={requested}, rows={rows:?}, active={:?}",
                document.records.active_range,
            );
            assert_eq!(
                &after[1..],
                &before[..before.len().saturating_sub(1)],
                "drag step {step} did not move by one exact tape row: previous={previous}, requested={requested}, rows={rows:?}, active={:?}",
                document.records.active_range,
            );
            assert_eq!(
                state.drag_endpoint.map(|pos| pos.row),
                Some(win.scroll_top()),
                "drag step {step} endpoint did not stay parked at the top edge"
            );
        }
    }

    #[test]
    fn resumed_sparse_scrollbar_total_matches_full_transcript_truth_for_fixed_rows() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 32;
        let viewport_rows = 12;
        let source = fixed_transcript(240);
        let records = source.history.block_records();
        let mut full_document = TranscriptDocument::from_transcript(source);
        let mut full_buf = Buffer::new(crate::smelt_edit::BufId(2), Default::default());
        let full_rows = {
            let plan = full_document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                viewport_rows,
            );
            full_document.project_planned(
                &lua,
                &mut full_buf,
                &theme,
                plan.expect("projection hydration"),
            )
        };

        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(session_dir.clone(), width, viewport_rows)
                .expect("tail transcript");
        let mut sparse_document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut sparse_buf = Buffer::new(crate::smelt_edit::BufId(3), Default::default());
        let sparse_rows = {
            let plan = sparse_document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                viewport_rows,
            );
            sparse_document.project_planned(
                &lua,
                &mut sparse_buf,
                &theme,
                plan.expect("projection hydration"),
            )
        };

        assert_eq!(sparse_rows.total_rows, full_rows.total_rows);
        let bar = crate::smelt_edit::ScrollbarState::new(
            width.saturating_sub(1),
            sparse_rows.total_rows,
            viewport_rows,
        )
        .expect("scrollbar");
        assert_eq!(bar.total_rows, full_rows.total_rows);
        assert_eq!(bar.viewport_rows, viewport_rows);
    }

    #[test]
    fn resumed_sparse_scrollbar_extent_tracks_heterogeneous_rendered_truth() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let viewport_rows = 18;
        let source = extent_accuracy_transcript(600);
        let records = source.history.block_records();
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let store = SqliteTranscriptStore::open_read_only(&session_dir).unwrap();

        for width in [24, 47, 80, 137] {
            let mut full_document =
                TranscriptDocument::from_transcript(extent_accuracy_transcript(records.len()));
            let exact_total = full_document.build_rows(&lua, width, &theme).len() as RowIndex;
            let exact_layout = full_document.materialize_exact_loaded_block_layout(&lua, width);

            let loaded =
                LoadedTranscript::tail_from_sqlite_dir(session_dir.clone(), width, viewport_rows)
                    .expect("tail transcript");
            let mut sparse_document = TranscriptDocument::from_loaded_transcript(loaded);
            let estimated_total = sparse_document.approximate_scrollbar_total_rows(&lua, width);
            let total_error = estimated_total.abs_diff(exact_total);
            assert!(
                total_error.saturating_mul(100) <= exact_total.saturating_mul(10),
                "width {width} extent error exceeded 10%: exact={exact_total}, estimated={estimated_total}, error={total_error}"
            );
            let exact_bar = crate::smelt_edit::ScrollbarState::new(
                width.saturating_sub(1),
                exact_total,
                viewport_rows,
            )
            .expect("exact scrollbar");
            let estimated_bar = crate::smelt_edit::ScrollbarState::new(
                width.saturating_sub(1),
                estimated_total,
                viewport_rows,
            )
            .expect("estimated scrollbar");

            for record_index in [records.len() / 4, records.len() / 2, records.len() * 3 / 4] {
                let mut prefix_document =
                    TranscriptDocument::from_transcript(extent_accuracy_transcript(record_index));
                let exact_prefix =
                    prefix_document.build_rows(&lua, width, &theme).len() as RowIndex;
                let estimated_prefix =
                    store.estimated_record_rows(width, 0..record_index).unwrap() as RowIndex;
                let exact_fraction = exact_prefix.saturating_mul(10_000) / exact_total.max(1);
                let estimated_fraction =
                    estimated_prefix.saturating_mul(10_000) / estimated_total.max(1);
                let fraction_error = exact_fraction.abs_diff(estimated_fraction);
                assert!(
                    fraction_error <= 500,
                    "width {width} prefix fraction error exceeded 5 points at record {record_index}: exact={exact_fraction}, estimated={estimated_fraction}, exact_prefix={exact_prefix}, estimated_prefix={estimated_prefix}"
                );
                let exact_thumb = exact_bar.metrics(exact_prefix).thumb_top;
                let estimated_thumb = estimated_bar.metrics(estimated_prefix).thumb_top;
                assert!(
                    exact_thumb.abs_diff(estimated_thumb) <= 1,
                    "width {width} thumb position differed by more than one cell at record {record_index}: exact={exact_thumb}, estimated={estimated_thumb}"
                );
            }

            for thumb_top in [4, 8, 13] {
                let exact_row = exact_bar.metrics(0).scroll_from_thumb_top(thumb_top);
                let estimated_row = estimated_bar.metrics(0).scroll_from_thumb_top(thumb_top);
                let exact_block_id = exact_layout
                    .iter()
                    .rev()
                    .find(|(_, first_row, _)| *first_row <= exact_row)
                    .map(|(block_id, _, _)| *block_id)
                    .expect("exact block at scrollbar position");
                let exact_record = full_document
                    .record_index_for_block_id(exact_block_id)
                    .expect("exact source record");
                let estimated_record = sparse_document
                    .estimated_record_for_display_row(width, estimated_row)
                    .expect("estimated source record")
                    .0;
                assert!(
                    exact_record.abs_diff(estimated_record) <= records.len() / 20,
                    "width {width} thumb {thumb_top} selected a source position more than 5% away: exact_record={exact_record}, estimated_record={estimated_record}"
                );
            }
        }
    }

    #[test]
    fn resumed_sparse_scrollbar_click_and_drag_requests_do_not_snap_back() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 32;
        let viewport_rows = 12;
        let source = fixed_transcript(240);
        let records = source.history.block_records();
        let mut full_document = TranscriptDocument::from_transcript(source);
        let mut full_buf = Buffer::new(crate::smelt_edit::BufId(2), Default::default());
        let full_rows = {
            let plan = full_document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                viewport_rows,
            );
            full_document.project_planned(
                &lua,
                &mut full_buf,
                &theme,
                plan.expect("projection hydration"),
            )
        };

        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(session_dir.clone(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut win = transcript_window();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
        let ctx = || ProjectionHarness {
            lua: &lua,
            theme: &theme,
            width,
            viewport_rows,
        };
        let sparse_rows = project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), true);
        assert_eq!(sparse_rows.total_rows, full_rows.total_rows);

        let full_bar = crate::smelt_edit::ScrollbarState::new(
            width.saturating_sub(1),
            full_rows.total_rows,
            viewport_rows,
        )
        .expect("full scrollbar");
        let sparse_bar = crate::smelt_edit::ScrollbarState::new(
            width.saturating_sub(1),
            sparse_rows.total_rows,
            viewport_rows,
        )
        .expect("sparse scrollbar");

        let sparse_metrics = sparse_bar.metrics(0);
        let full_metrics = full_bar.metrics(0);
        let mut previous = None;
        for rel_row in 0..viewport_rows {
            let request =
                sparse_metrics.scroll_from_thumb_top(sparse_metrics.thumb_top_for_click(rel_row));
            let full_request =
                full_metrics.scroll_from_thumb_top(full_metrics.thumb_top_for_click(rel_row));
            assert_eq!(
                request, full_request,
                "click row {rel_row} mapped differently"
            );

            win.scroll_to_preserving_cursor_screen_row(request, &buf, viewport_rows);
            let requested = win.scroll_top();
            project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
            assert_eq!(
                win.scroll_top(),
                requested,
                "click row {rel_row} snapped after projection"
            );
            if let Some(previous) = previous {
                assert!(
                    win.scroll_top() >= previous,
                    "click row {rel_row} moved backward: previous={previous}, current={}",
                    win.scroll_top()
                );
            }
            previous = Some(win.scroll_top());
        }

        for thumb_top in 0..=sparse_metrics.max_thumb_top {
            let request = sparse_metrics.scroll_from_thumb_top(thumb_top);
            assert_eq!(
                request,
                full_metrics.scroll_from_thumb_top(thumb_top),
                "thumb row {thumb_top} mapped differently"
            );
            win.scroll_to_preserving_cursor_screen_row(request, &buf, viewport_rows);
            let requested = win.scroll_top();
            project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
            assert_eq!(
                win.scroll_top(),
                requested,
                "thumb row {thumb_top} snapped after projection"
            );
            let resolved_thumb = sparse_bar.metrics(win.scroll_top()).thumb_top;
            assert!(
                resolved_thumb.abs_diff(thumb_top) <= 1,
                "thumb row {thumb_top} resolved to {resolved_thumb} for scroll {}",
                win.scroll_top()
            );
        }
    }

    #[test]
    fn sparse_previous_navigation_block_preserves_active_record_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = Transcript::new();
        for idx in 0..100 {
            if idx == 0 || idx == 50 || idx == 80 {
                source.push(Block::User {
                    text: format!("user {idx}"),
                    image_labels: Vec::new(),
                    command: false,
                });
            } else {
                source.push(Block::Text {
                    content: format!("assistant {idx}"),
                });
            }
        }
        let records = source.history.block_records();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            Some(session_dir.clone()),
        ));
        assert!(document.records.sparse.merge(&LoadedRecordWindow {
            start: smelt_store::TranscriptRecordOffset::new(0),
            total_count: records.len(),
            hydration: smelt_store::TranscriptRecordHydration::Hydrated,
            records: records_with_ids(&records[0..1], 0),
        }));
        let active_before = document.records.active_range.clone();
        assert!(!active_history_contains(&document, "user 0"));
        assert!(!active_history_contains(&document, "user 50"));

        let previous = document
            .previous_navigation_block(Some("user"))
            .expect("previous user block outside the active window");

        assert_eq!(previous.record_index, 50);
        assert_eq!(previous.block_id, BlockId::new(50));
        assert_eq!(previous.role, "user");
        assert_eq!(previous.first_line, "user 50");

        let next = document
            .next_navigation_block(Some("user"))
            .expect("next user block in the active window");

        assert_eq!(next.record_index, 80);
        assert_eq!(next.block_id, BlockId::new(80));
        assert_eq!(next.role, "user");
        assert_eq!(next.first_line, "user 80");
        assert_eq!(document.records.active_range, active_before);
        assert!(!active_history_contains(&document, "user 0"));
        assert!(!active_history_contains(&document, "user 50"));
    }

    #[test]
    fn reveal_block_intent_places_target_at_requested_screen_row() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let record_index = 100;
        let top_padding = 2;
        let dir = tempfile::tempdir().unwrap();
        let mut source = Transcript::new();
        for idx in 0..200 {
            if idx == record_index {
                source.push(Block::User {
                    text: format!("user {idx}"),
                    image_labels: Vec::new(),
                    command: false,
                });
            } else {
                source.push(Block::Text {
                    content: format!("assistant {idx}"),
                });
            }
        }
        let records = source.history.block_records();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            160..200,
            Some(session_dir.clone()),
        ));
        let block_id = BlockId::new(record_index as u64);

        document.set_pending_scroll_intent(TranscriptScrollIntent::RevealBlock {
            record_index,
            block_id,
            row_offset: 0,
            screen_padding_top: top_padding,
        });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: 0,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(911), Default::default());
        let rows =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));
        let first_scroll = rows.clamped_scroll;
        let reveal = document
            .record_block_reveal_position(&lua, width, record_index, 0, viewport_rows)
            .expect("revealed block position");
        assert_eq!(reveal.block_id, block_id);
        assert_eq!(
            reveal.target_row.saturating_sub(rows.clamped_scroll),
            top_padding
        );
        assert!(active_history_contains(&document, "user 100"));

        let active_start = document
            .records
            .active_range
            .as_ref()
            .expect("active reveal window")
            .start
            .get();
        assert!(active_start >= TRANSCRIPT_RECORD_PREFIX_STRIDE);
        for prefix_rows in document
            .extent_index
            .prefix_index
            .as_mut()
            .expect("extent prefix index")
            .prefix_rows
            .iter_mut()
            .skip(1)
        {
            *prefix_rows = prefix_rows.saturating_add(123);
        }
        document.set_pending_scroll_intent(TranscriptScrollIntent::RevealBlock {
            record_index,
            block_id,
            row_offset: 0,
            screen_padding_top: top_padding,
        });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: rows.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let rows =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));
        assert_ne!(rows.clamped_scroll, first_scroll);
        let reveal = document
            .record_block_reveal_position(&lua, width, record_index, 0, viewport_rows)
            .expect("revealed block position after prefix refinement");
        assert_eq!(
            reveal.target_row.saturating_sub(rows.clamped_scroll),
            top_padding
        );
    }

    #[test]
    fn viewport_anchor_preserves_record_block_across_window_replacement() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let record_index = 50;
        let dir = tempfile::tempdir().unwrap();
        let mut source = Transcript::new();
        for idx in 0..100 {
            if idx == record_index {
                source.push(Block::User {
                    text: format!("user {idx}"),
                    image_labels: Vec::new(),
                    command: false,
                });
            } else {
                source.push(Block::Text {
                    content: format!("assistant {idx}"),
                });
            }
        }
        let records = source.history.block_records();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            80..100,
            Some(session_dir.clone()),
        ));
        let block_id = BlockId::new(record_index as u64);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(912), Default::default());

        document.set_pending_scroll_intent(TranscriptScrollIntent::RevealBlock {
            record_index,
            block_id,
            row_offset: 0,
            screen_padding_top: 0,
        });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: 0,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let rows =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));
        let anchor = match document.viewport.state.resolved_anchor {
            Some(TranscriptResolvedViewportAnchor {
                top: TranscriptScrollAnchor::Content(anchor),
                ..
            }) => anchor,
            other => panic!("expected content viewport anchor, got {other:?}"),
        };
        assert_eq!(anchor.record_index, record_index);
        assert_eq!(anchor.block_id, block_id);
        assert_eq!(anchor.intra_block_row, anchor.row_anchor.row_offset);
        assert_eq!(anchor.bias, TranscriptAnchorBias::Top);
        assert_eq!(anchor.fallback_row, rows.clamped_scroll);
        let position_anchor = document.position_anchor(
            &lua,
            width,
            crate::smelt_edit::DocPosition {
                row: rows.clamped_scroll,
                byte_col: 0,
            },
        );
        let position_node_anchor = position_anchor
            .anchor
            .expect("position preservation should store a node anchor");
        assert_eq!(position_node_anchor.record_index, record_index);
        assert_eq!(position_node_anchor.block_id, block_id);

        assert!(document.activate_record_window_range(
            smelt_store::TranscriptRecordOffset::new(80)
                ..smelt_store::TranscriptRecordOffset::new(100)
        ));
        assert!(!active_history_contains(&document, "user 50"));

        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: rows.clamped_scroll.saturating_add(999),
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let _rows =
            document.project_planned(&lua, &mut buf, &theme, plan.expect("projection hydration"));
        assert!(active_history_contains(&document, "user 50"));
        let anchor_after = match document.viewport.state.resolved_anchor {
            Some(TranscriptResolvedViewportAnchor {
                top: TranscriptScrollAnchor::Content(anchor),
                ..
            }) => anchor,
            other => panic!("expected preserved content viewport anchor, got {other:?}"),
        };
        assert_eq!(anchor_after.record_index, record_index);
        assert_eq!(anchor_after.block_id, block_id);
        assert_eq!(anchor_after.intra_block_row, anchor.intra_block_row);
    }

    #[test]
    fn node_anchors_follow_child_block_when_group_identity_changes() {
        let lua = LuaRuntime::new();
        let width = 80;
        let mut source = Transcript::new();
        for (index, name) in ["read_file", "grep"].into_iter().enumerate() {
            source.push_tool_call(
                Block::ToolCall {
                    call_id: format!("group-call-{index}"),
                    name: name.into(),
                    summary: format!("{name} complete").into(),
                    args: HashMap::new(),
                },
                ToolState {
                    status: ToolStatus::Ok,
                    elapsed: Some(Duration::from_millis(1)),
                    called_at_ms: None,
                    elapsed_active: false,
                    output: Some(Box::new(smelt_core::transcript_model::ToolOutput {
                        content: format!("{name} output"),
                        is_error: false,
                        metadata: None,
                    })),
                    user_message: None,
                    preview_output: None,
                },
            );
        }
        let first_child = source.history.order[0];
        let mut document = TranscriptDocument::from_transcript(source);
        let original = document
            .node_metadata_at_row(&lua, width, 0)
            .expect("built-in explore group");
        assert!(matches!(
            original.id,
            crate::content::render_plan::RenderNodeId::Group(_)
        ));
        let position = DocPosition {
            row: original.first_row + original.rows - 1,
            byte_col: 0,
        };
        let anchor = document.position_anchor(&lua, width, position);
        assert_eq!(
            anchor.anchor.expect("stable group anchor").block_id,
            first_child
        );
        let search_anchor = document.search_anchor_at_row(&lua, width, position.row);
        assert!(matches!(
            search_anchor,
            TranscriptSearchAnchor::Content(anchor) if anchor.block_id == first_child
        ));

        lua.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "replacement_explore",
                  cache_key = "replacement:v1",
                  priority = 1000,
                  min = 2,
                  default_view = "collapsed",
                  selector = {
                    kind = "tool",
                    names = { "read_file", "grep" },
                    terminal = true,
                  },
                })
                smelt.transcript.extend_renderer("test.replacement_explore", function(next, node, ctx)
                  if node.kind ~= "group" or node.name ~= "replacement_explore" then
                    return next(node, ctx)
                  end
                  return smelt.layout.vbox({
                    smelt.layout.text("replacement"),
                    smelt.layout.text("details"),
                  })
                end, { cache_key = "test.replacement_explore:v1" })
                "#,
            )
            .exec()
            .expect("register replacement transcript group");

        let resolved = document.resolve_position_anchor(&lua, width, anchor);
        let replacement = document
            .node_metadata_at_row(&lua, width, resolved.row)
            .expect("replacement group at resolved cursor");
        assert!(matches!(
            replacement.id,
            crate::content::render_plan::RenderNodeId::Group(_)
        ));
        assert_ne!(replacement.id, original.id);
        assert!(resolved.row >= replacement.first_row);
        assert!(resolved.row < replacement.first_row + replacement.rows);

        let resolved_search = document
            .row_for_search_anchor(&lua, width, 20, search_anchor, 0, None)
            .expect("search anchor follows replacement group");
        let search_replacement = document
            .node_metadata_at_row(&lua, width, resolved_search)
            .expect("replacement group at resolved search row");
        assert_eq!(search_replacement.id, replacement.id);
    }

    #[test]
    fn sparse_active_window_keeps_loaded_ranges_separate() {
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            None,
        ));

        assert!(document.merge_record_window(LoadedRecordWindow {
            start: smelt_store::TranscriptRecordOffset::new(40),
            total_count: records.len(),
            hydration: smelt_store::TranscriptRecordHydration::Hydrated,
            records: records_with_ids(&records[40..48], 40),
        }));

        assert_eq!(document.records.sparse.loaded_ranges().len(), 2);
        assert_eq!(
            document.records.active_range,
            Some(
                smelt_store::TranscriptRecordOffset::new(40)
                    ..smelt_store::TranscriptRecordOffset::new(48)
            )
        );
        assert_eq!(document.content.transcript.history.order.len(), 8);
        let first = document.content.transcript.history.order[0];
        assert_eq!(
            document.content.transcript.history.first_line(first),
            Some("block 40".to_string())
        );
        assert!(!document.content.transcript.history.is_materialized(first));
        assert!(!active_history_contains(&document, "block 90"));
    }

    #[test]
    fn sparse_tail_projection_keeps_the_loaded_resume_window_active() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let dir = tempfile::tempdir().unwrap();
        let records = transcript_records(900);
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            860..900,
            Some(session_dir.clone()),
        ));
        let active_before = document.records.active_range.clone();
        let loaded_before = document.loaded_record_count();

        for _ in 0..3 {
            document
                .plan_projection_measured(
                    &lua,
                    80,
                    &theme,
                    crate::content::transcript_buf::ScrollTarget::visible_tail(),
                    10,
                )
                .expect("tail projection");
        }

        assert_eq!(document.records.active_range, active_before);
        assert_eq!(document.loaded_record_count(), loaded_before);
    }

    #[test]
    fn centered_record_window_contains_every_requested_record() {
        let records = transcript_records(700);
        let document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..512,
            None,
        ));

        for center in 0..records.len() {
            let range = document.record_window_range_for_center(100, center, 32, records.len());
            assert!(
                range.start.get() <= center && center < range.end.get(),
                "record {center} escaped window {:?}",
                range.start.get()..range.end.get()
            );
        }
    }

    #[test]
    fn sparse_row_jump_loads_bounded_record_window() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let dir = tempfile::tempdir().unwrap();
        let records = transcript_records(100);
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            Some(session_dir.clone()),
        ));

        let _plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(80),
            10,
        );
        let active = document.records.active_range.clone().unwrap();

        assert!(active.start.get() <= 40);
        assert!(active.end.get() > 40);
        assert!(active.start.get() > 0);
        assert!(active.end.get() < records.len());
        assert_eq!(document.records.sparse.loaded_ranges().len(), 2);
        assert!(active_history_contains(&document, "block 40"));
        assert!(!active_history_contains(&document, "block 90"));
    }

    #[test]
    fn sparse_nearby_scroll_reuses_active_record_window() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            40..48,
            None,
        ));
        let active_before = document.records.active_range.clone();

        let _plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(82),
            10,
        );

        assert_eq!(document.records.active_range, active_before);
        assert_eq!(document.records.sparse.loaded_ranges().len(), 1);
        assert!(active_history_contains(&document, "block 40"));
    }

    #[test]
    fn approximate_row_seek_reanchors_to_loaded_content() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 10;
        let records = transcript_records(100);
        let dir = tempfile::tempdir().unwrap();
        let session_dir = create_test_lineage_session(dir.path());
        crate::persist::write_transcript_record_suffix(&session_dir, 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            Some(session_dir.clone()),
        ));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(416), Default::default());

        document.set_pending_scroll_intent(TranscriptScrollIntent::ApproximateRowSeek(80));
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: 80,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let applied = document.project_applied_viewport(
            &lua,
            &mut buf,
            &theme,
            plan.expect("projection hydration"),
        );

        assert!(!applied.placeholder_rows_visible);
        assert!(active_history_contains(&document, "block 40"));
        let anchor = match document.viewport.state.resolved_anchor {
            Some(TranscriptResolvedViewportAnchor {
                top: TranscriptScrollAnchor::Content(anchor),
                ..
            }) => anchor,
            other => panic!("far seek should re-anchor to loaded content, got {other:?}"),
        };
        assert_eq!(anchor.record_index, 40);
        assert_eq!(anchor.block_id, BlockId::new(40));
        assert_eq!(
            document.viewport.state.mode,
            TranscriptViewportMode::Anchored
        );
    }

    #[test]
    fn sparse_average_rows_stays_stable_when_switching_loaded_windows() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let mut source = Transcript::new();
        for idx in 0..100 {
            let content = if idx >= 68 {
                format!("block {idx} {}", "long text ".repeat(40))
            } else {
                format!("block {idx}")
            };
            source.push(Block::Text { content });
        }
        let records = source.history.block_records();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            None,
        ));
        assert!(document.merge_record_window(LoadedRecordWindow {
            start: smelt_store::TranscriptRecordOffset::new(40),
            total_count: records.len(),
            hydration: smelt_store::TranscriptRecordHydration::Hydrated,
            records: records_with_ids(&records[40..48], 40),
        }));
        let average_before = document.approximate_average_record_rows(20);

        let _plan = document.plan_projection_measured(
            &lua,
            20,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_tail(),
            10,
        );

        assert_eq!(document.approximate_average_record_rows(20), average_before);
    }

    #[test]
    fn sparse_tail_target_reactivates_loaded_tail_window() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            None,
        ));
        assert!(document.merge_record_window(LoadedRecordWindow {
            start: smelt_store::TranscriptRecordOffset::new(40),
            total_count: records.len(),
            hydration: smelt_store::TranscriptRecordHydration::Hydrated,
            records: records_with_ids(&records[40..48], 40),
        }));

        let _plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_tail(),
            10,
        );

        assert_eq!(
            document.records.active_range,
            Some(
                smelt_store::TranscriptRecordOffset::new(68)
                    ..smelt_store::TranscriptRecordOffset::new(100)
            )
        );
        let active_first_lines = document
            .content
            .transcript
            .history
            .order
            .iter()
            .filter_map(|id| document.content.transcript.history.first_line(*id))
            .collect::<Vec<_>>();
        assert!(active_first_lines.iter().any(|line| line == "block 90"));
        assert!(!active_first_lines.iter().any(|line| line == "block 40"));
    }
}

pub(crate) struct TranscriptDisplayDocument<'a> {
    document: &'a mut TranscriptDocument,
    lua: &'a LuaRuntime,
    width: u16,
    theme: &'a Theme,
}

impl<'a> TranscriptDisplayDocument<'a> {
    pub(crate) fn new(
        document: &'a mut TranscriptDocument,
        lua: &'a LuaRuntime,
        width: u16,
        theme: &'a Theme,
    ) -> Self {
        Self {
            document,
            lua,
            width,
            theme,
        }
    }
}

impl DisplayDocument for TranscriptDisplayDocument<'_> {
    fn snapshot(&mut self) -> DisplaySnapshot {
        DisplaySnapshot {
            generation: self.document.content.projection.projection_generation(),
            total_rows: self
                .document
                .approximate_scrollbar_total_rows(self.lua, self.width),
        }
    }

    fn materialize(
        &mut self,
        range: std::ops::Range<crate::smelt_edit::RowIndex>,
    ) -> crate::smelt_edit::DisplayRows {
        self.document.exact_or_gap_display_rows_for_range(
            self.lua,
            self.width,
            self.theme,
            range.start,
            range.end.saturating_sub(range.start),
        )
    }

    fn copy_range(&mut self, range: TextRange) -> Option<crate::smelt_edit::CopyOutput> {
        range.rows().map(|range| {
            self.document
                .copy_exact_loaded_range(self.lua, self.width, self.theme, range)
        })
    }
}

pub(crate) struct ResumePreviewCache {
    views: HashMap<String, TranscriptDocument>,
    order: VecDeque<String>,
    limit: usize,
}

impl ResumePreviewCache {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            views: HashMap::new(),
            order: VecDeque::new(),
            limit,
        }
    }

    pub(crate) fn take(&mut self, key: &str) -> Option<TranscriptDocument> {
        self.views.remove(key)
    }

    pub(crate) fn store(&mut self, key: String, view: TranscriptDocument) {
        self.order.retain(|existing| existing != &key);
        self.order.push_back(key.clone());
        self.views.insert(key.clone(), view);

        while self.order.len() > self.limit {
            let Some(old_key) = self.order.pop_front() else {
                break;
            };
            if old_key != key {
                self.views.remove(&old_key);
            }
        }
    }

    pub(crate) fn set_inline_options(&mut self, options: InlineOptions) {
        for view in self.views.values_mut() {
            view.set_inline_options(options.clone());
        }
    }

    pub(crate) fn invalidate_theme(&mut self) {
        for view in self.views.values_mut() {
            view.invalidate_theme();
        }
    }

    pub(crate) fn invalidate_renderer_if_changed(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) {
        for view in self.views.values_mut() {
            view.invalidate_renderer_if_changed(generation, cache_key);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptBlockSnapshot {
    pub(crate) record_index: usize,
    pub(crate) block_id: BlockId,
    pub(crate) role: &'static str,
    pub(crate) first_row: crate::smelt_edit::RowIndex,
    pub(crate) rows: crate::smelt_edit::RowIndex,
    pub(crate) first_line: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranscriptNavigationDirection {
    Previous,
    Next,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptNavigationBlock {
    pub(crate) record_index: usize,
    pub(crate) block_id: BlockId,
    pub(crate) role: &'static str,
    pub(crate) first_line: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptBlockRevealPosition {
    pub(crate) block_id: BlockId,
    pub(crate) target_row: RowIndex,
    pub(crate) row_anchor: crate::content::transcript_buf::TranscriptRowAnchor,
}

fn transcript_raw_first_line(history: &BlockHistory, id: BlockId) -> String {
    history.first_line(id).unwrap_or_default()
}

fn transcript_history_role(history: &BlockHistory, id: BlockId) -> &'static str {
    history.block_kind(id).unwrap_or_default()
}

#[derive(Clone, Copy)]
pub(super) struct TranscriptPositionAnchor {
    anchor: Option<TranscriptNodeAnchor>,
    position: crate::smelt_edit::DocPosition,
}

#[derive(Clone, Copy)]
pub(super) struct TranscriptSearchRangeAnchor {
    anchor: TranscriptSearchAnchor,
    start_byte_col: usize,
    end_byte_col: usize,
    fallback_range: crate::smelt_edit::DocRange,
}

struct TranscriptViewAnchors {
    following_tail: bool,
    pinned_to_tail: bool,
    scroll_top: Option<TranscriptPositionAnchor>,
    cursor: Option<TranscriptPositionAnchor>,
    selection_anchor: Option<TranscriptPositionAnchor>,
    drag_endpoint: Option<TranscriptPositionAnchor>,
    search_current: Option<TranscriptSearchRangeAnchor>,
}

impl TuiApp {
    pub(crate) fn begin_turn(&mut self) {
        self.conversation.begin_turn();
    }

    pub(crate) fn push_block(&mut self, block: Block) {
        let appended = self.try_push_block(block);
        debug_assert!(appended);
    }

    pub(super) fn try_push_block(&mut self, block: Block) -> bool {
        self.conversation.append_block(block)
    }

    pub(crate) fn append_streaming_thinking(&mut self, delta: &str) {
        self.conversation
            .append_streaming_thinking(delta.to_string());
    }

    pub(crate) fn flush_streaming_thinking(&mut self) {
        self.conversation.flush_streaming_thinking();
    }

    pub(crate) fn append_streaming_text(&mut self, delta: &str) {
        self.conversation.append_streaming_text(delta.to_string());
    }

    pub(crate) fn flush_streaming_text(&mut self) {
        self.conversation.flush_streaming_text();
    }

    pub(crate) fn update_compaction_preview(&mut self, summary: String) {
        let follow_tail = self.transcript_win().is_following_tail();
        let existing = self.conversation.transcript_compaction_preview_id();
        let Some(id) = self.conversation.update_compaction_preview(summary) else {
            return;
        };
        if existing.is_none() {
            let width = self.transcript_width() as u16;
            self.conversation.fold_transcript_node(
                &self.lua,
                width,
                crate::content::render_plan::RenderNodeId::Block(id),
                crate::content::transcript_buf::FoldAction::Peek,
            );
        }
        if follow_tail {
            self.transcript_win_mut().follow_tail();
        }
        self.request_transient_render();
    }

    pub(crate) fn clear_compaction_preview(&mut self) {
        self.conversation.clear_compaction_preview();
    }

    pub(crate) fn start_tool_at(
        &mut self,
        invocation_id: protocol::InvocationId,
        call_id: String,
        name: String,
        summary: ::protocol::StyledLines,
        args: HashMap<String, serde_json::Value>,
        called_at_ms: u64,
    ) {
        self.conversation.start_tool(
            smelt_core::content::stream_parser::ToolStart {
                invocation_id,
                call_id,
                name,
                summary,
                args,
                preview_output: None,
                called_at_ms,
            },
            self.core.clock.instant_now(),
        );
    }

    pub(crate) fn start_exec(&mut self, command: String) {
        self.conversation.start_exec(command);
    }

    pub(crate) fn append_exec_output(&mut self, chunk: &str) {
        self.conversation.append_exec_output(chunk.to_string());
    }

    pub(crate) fn finish_exec(&mut self, exit_code: Option<i32>) {
        self.conversation.finish_exec(exit_code);
    }

    pub(crate) fn finalize_exec(&mut self) {
        self.conversation.finalize_exec();
    }

    pub(crate) fn has_active_exec(&self) -> bool {
        self.conversation.has_active_exec()
    }

    pub(crate) fn append_active_output(
        &mut self,
        invocation_id: protocol::InvocationId,
        chunk: &str,
    ) {
        self.conversation
            .append_tool_output(invocation_id, chunk.to_string());
    }

    pub(crate) fn set_active_status(
        &mut self,
        invocation_id: protocol::InvocationId,
        status: ToolStatus,
    ) {
        let now = self.core.clock.instant_now();
        self.conversation
            .set_tool_status(invocation_id, status, now);
    }

    pub(crate) fn set_active_user_message(
        &mut self,
        invocation_id: protocol::InvocationId,
        msg: String,
    ) {
        self.conversation.set_tool_user_message(invocation_id, msg);
    }

    pub(crate) fn finish_tool(
        &mut self,
        invocation_id: protocol::InvocationId,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
    ) {
        let now = self.core.clock.instant_now();
        self.conversation
            .finish_tool(invocation_id, status, output, engine_elapsed, now);
    }

    pub(crate) fn has_transcript_content(&mut self) -> bool {
        !self.conversation.transcript().is_empty()
    }

    /// Explicit loaded transcript materialization for APIs/tests that request the
    /// currently loaded post-render display text. Do not use for normal viewport
    /// rendering.
    pub(crate) fn materialize_loaded_transcript_display_rows_expensive(
        &mut self,
    ) -> Arc<Vec<String>> {
        let _perf = smelt_perf::perf::begin("transcript:materialize_rows_loaded:explicit");
        self.sync_transcript_renderer_generation();
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        self.conversation
            .build_transcript_rows(&self.lua, tw, &theme)
    }

    fn capture_transcript_view_anchors(&mut self, width: u16) -> TranscriptViewAnchors {
        let scroll = self.window_scroll_snapshot(crate::app::TRANSCRIPT_WIN);
        let (following_tail, pinned_to_tail, scroll_top, cursor, selection_anchor, drag_endpoint) = {
            let win = self.transcript_win();
            let state = win.document_view_state();
            let following_tail = scroll.as_ref().is_some_and(|scroll| scroll.follow);
            (
                following_tail,
                scroll.as_ref().is_some_and(|scroll| {
                    !scroll.follow
                        && scroll.viewport > 0
                        && scroll.at_bottom
                        && !win.selection_active()
                }),
                win.scroll_top(),
                state.cursor,
                state.selection_anchor,
                state.drag_endpoint,
            )
        };
        let search_current = self
            .overlays
            .search_session()
            .filter(|session| session.target == self.well_known.transcript)
            .and_then(|session| match &session.backend {
                crate::app::search::SearchBackend::Transcript(transcript) => transcript
                    .current
                    .and_then(|index| transcript.matches.get(index).copied()),
                crate::app::search::SearchBackend::Full { .. } => None,
            })
            .map(|matched| self.conversation.transcript_search_range_anchor(matched));
        TranscriptViewAnchors {
            following_tail,
            pinned_to_tail,
            scroll_top: (!following_tail && !pinned_to_tail).then(|| {
                self.conversation.transcript_position_anchor(
                    &self.lua,
                    width,
                    crate::smelt_edit::DocPosition {
                        row: scroll_top,
                        byte_col: 0,
                    },
                )
            }),
            cursor: Some(
                self.conversation
                    .transcript_position_anchor(&self.lua, width, cursor),
            ),
            selection_anchor: selection_anchor.map(|position| {
                self.conversation
                    .transcript_position_anchor(&self.lua, width, position)
            }),
            drag_endpoint: drag_endpoint.map(|position| {
                self.conversation
                    .transcript_position_anchor(&self.lua, width, position)
            }),
            search_current,
        }
    }

    fn restore_transcript_view_anchors(&mut self, width: u16, anchors: TranscriptViewAnchors) {
        let scroll_top = anchors.scroll_top.map(|anchor| {
            self.conversation
                .resolve_transcript_position_anchor(&self.lua, width, anchor)
                .row
        });
        let cursor = anchors.cursor.map(|anchor| {
            self.conversation
                .resolve_transcript_position_anchor(&self.lua, width, anchor)
        });
        let selection_anchor = anchors.selection_anchor.map(|anchor| {
            self.conversation
                .resolve_transcript_position_anchor(&self.lua, width, anchor)
        });
        let drag_endpoint = anchors.drag_endpoint.map(|anchor| {
            self.conversation
                .resolve_transcript_position_anchor(&self.lua, width, anchor)
        });
        let search_current = anchors.search_current.map(|anchor| {
            self.conversation
                .resolve_transcript_search_range_anchor(&self.lua, width, anchor)
        });

        if let Some(win) = self.ui.win_mut(self.well_known.transcript) {
            if anchors.following_tail || anchors.pinned_to_tail {
                win.follow_tail();
            } else if let Some(row) = scroll_top {
                win.pin_scroll(row);
            }
            let mut state = win.document_view_state();
            if let Some(cursor) = cursor {
                state.cursor = cursor;
            }
            state.selection_anchor = selection_anchor;
            state.drag_endpoint = drag_endpoint;
            win.set_document_view_state(state);
        }

        if let Some(matched) = search_current {
            self.overlays
                .replace_current_transcript_search_match(matched);
        }
    }

    #[cfg(test)]
    pub(crate) fn transcript_total_rows(&mut self) -> crate::smelt_edit::RowIndex {
        self.document_snapshot_for_win(crate::app::TRANSCRIPT_WIN)
            .map(|snapshot| snapshot.total_rows)
            .unwrap_or(0)
    }

    pub(crate) fn transcript_rows_and_breaks_range(
        &mut self,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> crate::smelt_edit::DisplayRows {
        let _perf = smelt_perf::perf::begin("transcript:materialize_rows_range");
        self.materialize_document_rows(crate::app::TRANSCRIPT_WIN, start, count)
            .unwrap_or_default()
    }

    pub(crate) fn transcript_visible_rows(
        &mut self,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> Vec<String> {
        self.transcript_rows_and_breaks_range(start, count)
            .into_text_rows()
    }

    pub(crate) fn transcript_node_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        self.conversation
            .transcript_node_metadata_at_row(&self.lua, width, row)
    }

    pub(crate) fn fold_transcript_node_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
        action: crate::content::transcript_buf::FoldAction,
        activation: crate::content::transcript_buf::FoldActivation,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let anchors = self.capture_transcript_view_anchors(width);
        let changed = self
            .conversation
            .fold_transcript_node_at_row(&self.lua, width, row, action, activation);
        if changed {
            self.restore_transcript_view_anchors(width, anchors);
        }
        changed
    }

    pub(crate) fn fold_transcript_node(
        &mut self,
        id: crate::content::render_plan::RenderNodeId,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let anchors = self.capture_transcript_view_anchors(width);
        let changed = self
            .conversation
            .fold_transcript_node(&self.lua, width, id, action);
        if changed {
            self.restore_transcript_view_anchors(width, anchors);
        }
        changed
    }

    pub(crate) fn fold_all_transcript_nodes(
        &mut self,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let anchors = self.capture_transcript_view_anchors(width);
        let changed = self
            .conversation
            .fold_all_transcript_nodes(&self.lua, width, action);
        if changed {
            self.restore_transcript_view_anchors(width, anchors);
        }
        changed
    }

    pub(crate) fn fold_transcript_block_kind(
        &mut self,
        kind: &str,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let anchors = self.capture_transcript_view_anchors(width);
        let changed = self
            .conversation
            .fold_transcript_block_kind(&self.lua, width, kind, action);
        if changed {
            self.restore_transcript_view_anchors(width, anchors);
        }
        changed
    }

    pub(crate) fn snap_cpos_to_selectable(&mut self, rows: &[String], cpos: usize) -> usize {
        let buf_id = self.transcript_win().buf;
        let Some(buf) = self.ui.buf(buf_id) else {
            return cpos;
        };
        let mut acc = 0usize;
        for (r, row) in rows.iter().enumerate() {
            let row_end = acc + row.len();
            if cpos <= row_end {
                let col_byte = cpos.saturating_sub(acc).min(row.len());
                let col = row[..col_byte].chars().count();
                let snapped = smelt_buffer::coords::snap_col_to_selectable(buf, r, col);
                if snapped == col {
                    return cpos;
                }
                let byte_col: usize = row.chars().take(snapped).map(|c| c.len_utf8()).sum();
                return acc + byte_col;
            }
            acc = row_end + 1;
        }
        cpos
    }

    /// Snapshot of the laid-out transcript blocks. `record_index` is the
    /// stable sparse record index accepted by `transcript.reveal_block`,
    /// while `block_id` is the stable block identity. Returns empty when no
    /// projection has run yet.
    pub(crate) fn visible_transcript_block_snapshots(&self) -> Vec<TranscriptBlockSnapshot> {
        self.conversation.transcript().visible_block_snapshots()
    }

    pub(crate) fn loaded_transcript_block_snapshots(&mut self) -> Vec<TranscriptBlockSnapshot> {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        self.conversation
            .materialize_transcript_block_snapshots(&self.lua, width)
    }

    pub(crate) fn loaded_transcript_block_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<TranscriptBlockSnapshot> {
        self.loaded_transcript_block_snapshots()
            .into_iter()
            .find(|snap| {
                let end = snap.first_row.saturating_add(snap.rows);
                row >= snap.first_row && row < end
            })
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn reveal_transcript_record_block(
        &mut self,
        record_index: usize,
        top_padding: crate::smelt_edit::RowIndex,
        cursor: bool,
    ) -> bool {
        self.reveal_transcript_block(record_index, None, top_padding, cursor)
    }

    pub(crate) fn reveal_transcript_target_at_top(
        &mut self,
        record_index: usize,
        block_id: BlockId,
        top_padding: crate::smelt_edit::RowIndex,
        move_cursor: bool,
    ) -> bool {
        if !self
            .conversation
            .transcript()
            .record_matches_block_id(record_index, block_id)
        {
            return false;
        }
        self.reveal_transcript_block(record_index, Some(block_id), top_padding, move_cursor)
    }

    fn reveal_transcript_block(
        &mut self,
        record_index: usize,
        expected_block_id: Option<BlockId>,
        top_padding: crate::smelt_edit::RowIndex,
        cursor: bool,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let viewport_rows = self
            .transcript_win()
            .viewport
            .map(|viewport| viewport.rect.height)
            .unwrap_or(1)
            .max(1);
        let Some(reveal) = self.conversation.transcript_record_block_reveal_position(
            &self.lua,
            width,
            record_index,
            0,
            viewport_rows,
        ) else {
            return false;
        };
        if expected_block_id.is_some_and(|expected| expected != reveal.block_id) {
            return false;
        }
        let window_scroll_before = self.transcript_scroll_top();
        self.reveal_position(
            crate::app::TRANSCRIPT_WIN,
            crate::smelt_edit::DocPosition {
                row: reveal.target_row,
                byte_col: 0,
            },
            crate::app::reveal::RevealOptions {
                top_padding,
                cursor,
                ..Default::default()
            },
        );
        self.record_transcript_scroll_intent(
            "reveal_block",
            TranscriptScrollIntent::RevealBlock {
                record_index,
                block_id: reveal.block_id,
                row_offset: 0,
                screen_padding_top: top_padding,
            },
            window_scroll_before,
        );
        true
    }

    pub(crate) fn finish_transcript_turn(&mut self) {
        let _perf = smelt_perf::perf::begin("render:finish_turn");
        self.conversation.finalize_tools();
    }

    pub(crate) fn set_agent_blocked_paused(&mut self, paused: bool) {
        let now = self.core.clock.instant_now();
        self.working.set_paused(paused);
        self.conversation.set_active_tools_paused(paused, now);
    }

    pub(crate) fn apply_pending_history_appends_for_request(&mut self) {
        if self.block_read_only_mutation("update read-only session history") {
            return;
        }
        let appends = self.conversation.take_pending_history_appends();
        for append in appends {
            let mode_base = match append.mode() {
                Some(_) => match self.mode_append_base() {
                    Ok(mode) => Some(mode),
                    Err(err) => {
                        smelt_perf::perf::record_value("live_session:mode_scan_error", 1);
                        self.notify_session_error_sticky(format!(
                            "failed to read session mode: {err}"
                        ));
                        self.conversation
                            .replace_or_push_history_append(append)
                            .expect("mode append coalescing does not read context history");
                        continue;
                    }
                },
                None => None,
            };
            let history_append = append.history_append(mode_base);
            let coalescing_note_kind = history_append.coalescing_note_kind();
            let result = self.apply_history_append_to_history(&history_append);
            if let Some(block) = append.transcript_block(&self.lua) {
                self.commit_history_append_block(block, coalescing_note_kind, result);
            }
        }
    }

    pub(crate) fn commit_pending_history_append(&mut self, item: &protocol::HistoryItem) {
        let Some(append) = self.conversation.take_matching_history_append(item) else {
            return;
        };
        if append.coalescing_note_kind() == Some(protocol::HistoryNoteKind::ModeChange) {
            if let Some(mode) = append.mode().and_then(protocol::AgentMode::parse) {
                self.conversation.set_applied_mode(mode);
            }
        }
        let result = if append.coalescing_note_kind().is_some() {
            protocol::HistoryAppendResult::ReplacedLast
        } else {
            protocol::HistoryAppendResult::Pushed
        };
        if let Some(block) = append.transcript_block(&self.lua) {
            self.commit_history_append_block(block, append.coalescing_note_kind(), result);
        }
    }

    pub(crate) fn commit_history_append_block(
        &mut self,
        block: Block,
        coalescing_note_kind: Option<protocol::HistoryNoteKind>,
        result: protocol::HistoryAppendResult,
    ) {
        match result {
            protocol::HistoryAppendResult::Unchanged => {}
            protocol::HistoryAppendResult::RemovedLast => {
                self.remove_last_mode_block();
            }
            protocol::HistoryAppendResult::ReplacedLast => {
                if self.rewrite_last_mode_block(block.clone(), coalescing_note_kind) {
                    return;
                }
                self.push_block(block);
            }
            protocol::HistoryAppendResult::Pushed => self.push_block(block),
        }
    }

    fn rewrite_last_mode_block(
        &mut self,
        block: Block,
        coalescing_note_kind: Option<protocol::HistoryNoteKind>,
    ) -> bool {
        if coalescing_note_kind != Some(protocol::HistoryNoteKind::ModeChange) {
            return false;
        }
        let history = self.conversation.transcript().history();
        let Some(id) = history.order.last().copied() else {
            return false;
        };
        if !matches!(history.block(id), Some(Block::Mode { .. })) {
            return false;
        }
        self.conversation.rewrite_block(id, block)
    }

    fn remove_last_mode_block(&mut self) {
        let history = self.conversation.transcript().history();
        let Some((idx, id)) = history.order.iter().copied().enumerate().next_back() else {
            return;
        };
        if matches!(history.block(id), Some(Block::Mode { .. })) {
            self.truncate_to(idx);
        }
    }

    pub(crate) fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.conversation.drain_finished_transcript_blocks()
    }

    pub(crate) fn invalidate_for_theme(&mut self) {
        self.conversation.invalidate_transcript_theme();
    }

    pub(crate) fn inline_options(&self) -> InlineOptions {
        InlineOptions {
            file_icons: FileIconOptions::new(
                self.core.config.settings.file_icons,
                self.core.config.settings.file_icon_colors,
                self.ui.theme().is_light(),
                Some(self.workspace.cwd_path().to_owned()),
            ),
        }
    }

    pub(crate) fn sync_inline_options(&mut self) {
        let options = self.inline_options();
        self.conversation.set_transcript_inline_options(options);
    }

    pub(crate) fn sync_transcript_renderer_generation(&mut self) {
        let generation = self.lua.transcript_renderer_generation();
        let inline_options = self.inline_options();
        let cache_key = crate::content::display_layout::transcript_renderer_cache_key(
            &self.lua,
            &inline_options,
        );
        self.conversation
            .invalidate_transcript_renderer(generation, cache_key);
    }

    /// Install a complete theme and publish it to the process-wide active slot.
    pub(crate) fn install_theme(&mut self, theme: Theme) {
        *self.ui.theme_mut() = theme;
        smelt_core::theme::set_active(self.ui.theme().clone());
        self.sync_inline_options();
        self.sync_transcript_renderer_generation();
        self.invalidate_for_theme();
    }

    /// Mutate the current theme and republish.
    pub(crate) fn mutate_theme(&mut self, f: impl FnOnce(&mut Theme)) {
        f(self.ui.theme_mut());
        smelt_core::theme::set_active(self.ui.theme().clone());
        self.sync_inline_options();
        self.sync_transcript_renderer_generation();
        self.invalidate_for_theme();
    }

    pub(crate) fn clear_transcript(&mut self) {
        self.conversation.clear_transcript();
    }

    pub(crate) fn last_user_block_index(&self) -> Option<usize> {
        self.conversation.transcript().last_user_block_index()
    }

    pub(crate) fn user_turns(&self) -> Vec<(usize, String)> {
        self.conversation.transcript().user_turns()
    }

    pub(crate) fn truncate_to(&mut self, block_idx: usize) {
        self.conversation.truncate_transcript(block_idx);
        self.conversation.clear_stream_tools();
    }

    /// Advance spinner animation. Returns `true` if the frame changed.
    pub(crate) fn update_spinner(&mut self) -> bool {
        let mut changed = false;
        if let (Some(elapsed), Some(prev_frame)) =
            (self.working.elapsed(), self.working.last_spinner_frame())
        {
            let frame = smelt_core::content::spinner_frame_index(elapsed);
            if frame != prev_frame {
                self.working.set_last_spinner_frame(frame);
                changed = true;
            }
        }
        changed
    }

    /// Per-line selection ranges (line, col_start, col_end) in display-cell units.
    /// No-op when no vim visual or selection anchor is active.
    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn transcript_selection_highlights(
        &mut self,
        scroll_top: crate::smelt_edit::RowIndex,
        row_base: crate::smelt_edit::RowIndex,
        viewport_rows: u16,
    ) -> Vec<(usize, u16, u16)> {
        let win = self.transcript_win();
        if win.has_materialized_rows() {
            let buf_id = win.buf;
            let buf = match self.ui.buf(buf_id) {
                Some(b) => b,
                None => return Vec::new(),
            };
            let now = self.core.clock.instant_now();
            return win
                .row_selection_ranges(buf, viewport_rows, now)
                .into_iter()
                .map(|r| (r.line, r.col_start, r.col_end))
                .collect();
        }
        let vim_visual = win.vim_enabled()
            && matches!(
                win.vim_mode(),
                crate::smelt_edit::VimMode::Visual | crate::smelt_edit::VimMode::VisualLine
            );
        let anchor_set = win.selection_anchor().is_some();
        if !vim_visual && !anchor_set {
            return Vec::new();
        }

        let buf_id = self.transcript_win().buf;
        let buf = match self.ui.buf(buf_id) {
            Some(b) => b,
            None => return Vec::new(),
        };
        let rows = buf.lines();
        if rows.is_empty() {
            return Vec::new();
        }
        let text = buf.text();
        let win = self.transcript_win();
        let endpoint = win.effective_endpoint();
        let active_selection = if win.vim_enabled() {
            match win.vim_mode() {
                crate::smelt_edit::VimMode::Visual | crate::smelt_edit::VimMode::VisualLine => {
                    crate::smelt_edit::vim::visual_range(
                        win.vim_state(),
                        &text,
                        endpoint,
                        win.vim_mode(),
                    )
                }
                _ => win.selection_range_at(endpoint, &text),
            }
        } else {
            win.selection_range_at(endpoint, &text)
        };
        let (s, e) = match active_selection {
            Some(range) => range,
            None => return Vec::new(),
        };
        if s >= e {
            return Vec::new();
        }
        // Route through the shared coord helper so the prompt's per-row
        // selection painting and the transcript's stay one implementation -
        // including the "1-cell fallback span on empty middle rows" rule.
        let first = scroll_top
            .saturating_sub(row_base)
            .min(usize::MAX as crate::smelt_edit::RowIndex) as usize;
        let last = first + viewport_rows as usize;
        smelt_buffer::coords::byte_range_to_row_ranges(buf, s, e)
            .into_iter()
            .filter(|r| r.line >= first && r.line < last)
            .map(|r| (r.line, r.col_start, r.col_end))
            .collect()
    }

    /// Wrap the prompt input against `width` and return the resulting row count.
    /// The Lua layout composer reads this as `state.prompt_input_rows` and
    /// gives the prompt window that many rows in the splits tree.
    pub(crate) fn measure_prompt_input_rows(
        &self,
        edit_buf: &crate::smelt_edit::Buffer,
        width: usize,
        placeholder: Option<&str>,
    ) -> u16 {
        let usable = width.saturating_sub(2).min(u16::MAX as usize) as u16;
        let attachment_store = self.prompt.attachment_store();
        let store = attachment_store.lock().unwrap();
        let lines = build_prompt_display_lines(
            edit_buf.source(),
            &edit_buf.attachment_ids,
            &store,
            placeholder,
        );
        let cursor_padding = prompt_display_uses_cursor_padding(edit_buf.source(), placeholder);
        let layout = if cursor_padding {
            WrappedLayout::from_lines_with_cursor_padding(&lines, usable, true)
        } else {
            WrappedLayout::from_lines(&lines, usable, true)
        };
        layout.visual_count().max(1) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::{seed_test_transcript_rows, TEST_LINEAGE_SESSION_ID};
    use crate::app::search::SearchDirection;
    use crate::app::test_harness::TestApp;
    use crate::content::transcript_buf::{FoldAction, FoldActivation};
    use crate::smelt_edit::{DocPosition, TextRange};

    #[test]
    fn selection_highlights_subtract_materialized_row_base() {
        let mut harness = TestApp::builder().build();
        let buf_id = harness.app.transcript_win().buf;
        {
            let buf = harness.app.ui.buf_mut(buf_id).expect("transcript buffer");
            buf.set_all_lines((30..40).map(|i| format!("line {i}")).collect());
        }

        let (line_idx, start, end, line_count) = {
            let buf = harness.app.ui.buf(buf_id).expect("transcript buffer");
            let line_idx = buf
                .lines()
                .iter()
                .position(|line| line == "line 39")
                .expect("tail line is materialized");
            let offsets = smelt_buffer::text::line_start_offsets(buf.lines());
            let start = offsets[line_idx];
            (line_idx, start, start + "line 39".len(), buf.lines().len())
        };
        {
            let win = harness.app.transcript_win_mut();
            win.set_selection_anchor(Some(start));
            win.set_cpos(end);
        }

        let ranges = harness.app.transcript_selection_highlights(39, 30, 5);
        assert!(
            ranges.iter().any(|(line, _, _)| *line == line_idx),
            "selection range should be expressed in materialized buffer rows"
        );
        assert!(ranges.iter().all(|(line, _, _)| *line < line_count));
    }

    #[test]
    fn fold_preserves_transcript_document_anchors() {
        let mut app = TestApp::builder().build().app;
        app.push_block(smelt_core::Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            kind: protocol::ReasoningKind::Raw,
            content: (0..20)
                .map(|i| format!("folded prefix {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        app.push_block(smelt_core::Block::Text {
            content: "anchor target\nanchor cursor".into(),
        });
        app.push_block(smelt_core::Block::Text {
            content: "after".into(),
        });
        assert!(app.fold_transcript_node_at_row(0, FoldAction::Open, FoldActivation::AnyNodeRow));
        app.render_normal_to(&mut std::io::sink());
        let before = app.loaded_transcript_block_snapshots();
        let target_start = before
            .iter()
            .find(|snap| snap.first_line == "anchor target")
            .map(|snap| snap.first_row)
            .expect("target block before fold");
        app.submit_search(
            app.well_known.transcript,
            SearchDirection::Forward,
            "anchor cursor".into(),
        );
        let before_match = app
            .overlays
            .search_session()
            .and_then(|session| session.current_range())
            .expect("current search match before fold");
        assert!(matches!(
            before_match,
            TextRange::Rows(range) if range.start.row == target_start.saturating_add(1)
        ));
        {
            let win = app.transcript_win_mut();
            win.pin_scroll(target_start);
            let mut state = win.document_view_state();
            state.cursor = DocPosition {
                row: target_start.saturating_add(1),
                byte_col: "anchor ".len(),
            };
            state.selection_anchor = Some(DocPosition {
                row: target_start,
                byte_col: 0,
            });
            state.drag_endpoint = Some(DocPosition {
                row: target_start.saturating_add(1),
                byte_col: "anchor cursor".len(),
            });
            win.set_document_view_state(state);
        }

        assert!(app.fold_transcript_node_at_row(0, FoldAction::Close, FoldActivation::AnyNodeRow));
        let after = app.loaded_transcript_block_snapshots();
        let new_target_start = after
            .iter()
            .find(|snap| snap.first_line == "anchor target")
            .map(|snap| snap.first_row)
            .expect("target block after fold");
        let win = app.transcript_win();
        let state = win.document_view_state();

        assert!(new_target_start < target_start);
        assert_eq!(win.scroll_top(), new_target_start);
        assert_eq!(
            state.cursor,
            DocPosition {
                row: new_target_start.saturating_add(1),
                byte_col: "anchor ".len(),
            }
        );
        assert_eq!(
            state.selection_anchor,
            Some(DocPosition {
                row: new_target_start,
                byte_col: 0,
            })
        );
        assert_eq!(
            state.drag_endpoint,
            Some(DocPosition {
                row: new_target_start.saturating_add(1),
                byte_col: "anchor cursor".len(),
            })
        );
        let after_match = app
            .overlays
            .search_session()
            .and_then(|session| session.current_range())
            .expect("current search match after fold");
        assert!(matches!(
            after_match,
            TextRange::Rows(range) if range.start.row == new_target_start.saturating_add(1)
        ));
    }

    #[test]
    fn fold_preserves_transcript_tail_follow() {
        let mut app = TestApp::builder().build().app;
        for i in 0..40 {
            app.push_block(smelt_core::Block::Text {
                content: format!("tail line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());
        assert!(app.transcript_win().is_following_tail());

        assert!(app.fold_all_transcript_nodes(FoldAction::Close));

        assert!(app.transcript_win().is_following_tail());
    }

    fn test_record_record(idx: u64) -> smelt_store::StoredTranscriptBlock {
        let indexed_text = format!("block {idx}");
        smelt_store::StoredTranscriptBlock {
            block_idx: idx,
            history_idx: None,
            kind: "text".into(),
            tool_call_id: None,
            tool_name: None,
            content_hash: format!("{idx}"),
            estimated_text_bytes: 8,
            preview_text: indexed_text.clone(),
            indexed_text: indexed_text.clone(),
            block_json: serde_json::to_string(&smelt_core::Block::Text {
                content: indexed_text,
            })
            .unwrap(),
            origin_json: Some(
                serde_json::to_string(&smelt_core::BlockOrigin::History(idx as usize)).unwrap(),
            ),
            tool_state_json: None,
        }
    }

    #[test]
    fn transcript_document_loads_record_window_from_store() {
        let dir = tempfile::tempdir().unwrap();
        let records = (0..4).map(test_record_record).collect::<Vec<_>>();
        let session_dir = seed_test_transcript_rows(dir.path(), records);
        let reader =
            smelt_store::LineageSessionReader::open_existing(dir.path(), TEST_LINEAGE_SESSION_ID)
                .unwrap();
        let tail = reader
            .transcript_record_slice_with_total((3..4).into(), 4)
            .unwrap();

        let loaded =
            super::LoadedTranscript::from_record_slice(tail, session_dir).expect("loaded tail");
        let mut document = super::TranscriptDocument::from_loaded_transcript(loaded);

        let window = document
            .load_record_window((1..3).into())
            .expect("loaded middle window");
        assert_eq!(window.start.get(), 1);
        assert_eq!(window.end().get(), 3);
        assert_eq!(window.total_count, 4);
        assert_eq!(window.records.len(), 2);
    }

    #[test]
    fn transcript_document_merges_record_windows_without_discarding_tail() {
        let dir = tempfile::tempdir().unwrap();
        let records = (0..6).map(test_record_record).collect::<Vec<_>>();
        let session_dir = seed_test_transcript_rows(dir.path(), records);
        let reader =
            smelt_store::LineageSessionReader::open_existing(dir.path(), TEST_LINEAGE_SESSION_ID)
                .unwrap();
        let tail = reader
            .transcript_record_slice_with_total((4..6).into(), 6)
            .unwrap();

        let loaded =
            super::LoadedTranscript::from_record_slice(tail, session_dir).expect("loaded tail");
        let mut document = super::TranscriptDocument::from_loaded_transcript(loaded);
        assert_eq!(document.record_total_count(), Some(6));
        assert_eq!(document.loaded_record_count(), 2);
        let ranges = document.loaded_record_ranges();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start.get(), 4);
        assert_eq!(ranges[0].end.get(), 6);
        assert_eq!(
            document.record_range_state(4..6),
            super::RecordRangeState::Loaded
        );
        assert_eq!(
            document.record_range_state(1..3),
            super::RecordRangeState::Missing
        );

        let middle = document
            .load_record_window((1..3).into())
            .expect("loaded middle window");
        assert!(document.merge_record_window(middle));

        assert_eq!(document.loaded_record_count(), 4);
        let ranges = document.loaded_record_ranges();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].start.get(), 1);
        assert_eq!(ranges[0].end.get(), 3);
        assert_eq!(ranges[1].start.get(), 4);
        assert_eq!(ranges[1].end.get(), 6);
        assert_eq!(
            document.record_range_state(1..3),
            super::RecordRangeState::Loaded
        );
        assert_eq!(
            document.record_range_state(3..4),
            super::RecordRangeState::Missing
        );
        assert_eq!(document.history().order.len(), 2);
        assert_eq!(
            document.history().order,
            vec![
                smelt_core::transcript_model::BlockId::new(1),
                smelt_core::transcript_model::BlockId::new(2),
            ]
        );
        let origins = document
            .history()
            .order
            .iter()
            .map(|id| document.history().block_origin(*id))
            .collect::<Vec<_>>();
        assert_eq!(
            origins,
            vec![
                Some(smelt_core::BlockOrigin::History(1)),
                Some(smelt_core::BlockOrigin::History(2)),
            ]
        );
    }

    #[test]
    fn transcript_truncation_updates_sparse_record_extent_immediately() {
        let slice = smelt_store::TranscriptRecordSlice::new(
            smelt_store::TranscriptRecordOffset::new(34),
            100,
            smelt_store::TranscriptRecordHydration::Hydrated,
            (34..37).map(test_record_record).collect(),
        );
        let dir = tempfile::tempdir().unwrap();
        let loaded = super::LoadedTranscript::from_record_slice(slice, dir.path().to_path_buf())
            .expect("loaded sparse window");
        let mut document = super::TranscriptDocument::from_loaded_transcript(loaded);

        document.truncate_to(3);
        assert_eq!(document.record_total_count(), Some(100));

        document.truncate_to(1);
        assert_eq!(document.record_total_count(), Some(35));
        assert_eq!(document.loaded_record_ranges().len(), 1);
        assert_eq!(document.loaded_record_ranges()[0].start.get(), 34);
        assert_eq!(document.loaded_record_ranges()[0].end.get(), 35);
    }

    #[test]
    fn transcript_truncation_does_not_extend_sparse_extent_for_dirty_tail() {
        let slice = smelt_store::TranscriptRecordSlice::new(
            smelt_store::TranscriptRecordOffset::new(98),
            100,
            smelt_store::TranscriptRecordHydration::Hydrated,
            (98..100).map(test_record_record).collect(),
        );
        let dir = tempfile::tempdir().unwrap();
        let loaded = super::LoadedTranscript::from_record_slice(slice, dir.path().to_path_buf())
            .expect("loaded sparse tail");
        let mut document = super::TranscriptDocument::from_loaded_transcript(loaded);
        document.push(smelt_core::Block::Text {
            content: "retained dirty tail".into(),
        });
        document.push(smelt_core::Block::Text {
            content: "truncated dirty tail".into(),
        });

        document.truncate_to(3);

        assert_eq!(document.record_total_count(), Some(100));
        assert_eq!(document.loaded_record_ranges().len(), 1);
        assert_eq!(document.loaded_record_ranges()[0].start.get(), 98);
        assert_eq!(document.loaded_record_ranges()[0].end.get(), 100);
        let bounds = document
            .record_save_bounds(None)
            .expect("dirty tail produces save bounds");
        assert_eq!(bounds.record_start_idx, 100);
        assert_eq!(
            document
                .history()
                .block_records_with_ids_from(bounds.order_start)
                .len(),
            1
        );
    }

    #[test]
    fn record_save_bounds_use_earliest_dirty_source() {
        let mut document = super::TranscriptDocument::new();
        document.push_with_origin(
            smelt_core::Block::Text {
                content: "persisted".into(),
            },
            smelt_core::BlockOrigin::History(0),
        );
        document.history_mut().clear_record_dirty();
        document.push_with_origin(
            smelt_core::Block::Text {
                content: "dirty tail".into(),
            },
            smelt_core::BlockOrigin::History(1),
        );
        assert_eq!(document.history().record_dirty_from(), Some(1));

        let bounds = document
            .record_save_bounds(Some(0))
            .expect("dirty record sources should produce save bounds");
        assert_eq!(bounds.record_start_idx, 0);
        assert_eq!(
            document
                .history()
                .block_records_with_ids_from(bounds.order_start)
                .len(),
            2
        );
    }

    #[test]
    fn record_save_pins_history_dirty_prefix() {
        let mut source = smelt_core::content::transcript::Transcript::new();
        for index in 0..2 {
            source.push_with_origin(
                smelt_core::Block::Text {
                    content: format!("history block {index}"),
                },
                smelt_core::BlockOrigin::History(index),
            );
        }
        let records = source.history.block_records();
        let record_rows = records
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, mut record)| {
                record.origin = None;
                smelt_core::transcript_model::transcript_block_row_with_block_idx(
                    index,
                    index as u64,
                    &record,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let dir = tempfile::tempdir().unwrap();
        let session_dir = seed_test_transcript_rows(dir.path(), record_rows);
        source.history.clear_record_dirty();
        let mut document = super::TranscriptDocument::from_transcript(source);
        document.set_session_dir(session_dir);
        document.schedule_durable_compaction(records.len(), None);
        while document.drain_compaction_slice() {}

        let bounds = document.record_save_bounds(Some(0));
        let pins = document.pin_record_suffix_for_save(bounds).unwrap();
        assert_eq!(pins.len(), records.len());
        let bounds = bounds.expect("history dirtiness should produce save bounds");
        assert_eq!(
            document
                .history()
                .block_records_with_ids_from(bounds.order_start)
                .len(),
            records.len()
        );
        document.unpin_operation_blocks(&pins);
    }

    #[test]
    fn approximate_row_seek_uses_prefix_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut records = (0..300).map(test_record_record).collect::<Vec<_>>();
        records[0].estimated_text_bytes = 1_000;
        let session_dir = seed_test_transcript_rows(dir.path(), records);
        let reader =
            smelt_store::LineageSessionReader::open_existing(dir.path(), TEST_LINEAGE_SESSION_ID)
                .unwrap();
        let tail = reader
            .transcript_record_slice_with_total((298..300).into(), 300)
            .unwrap();

        let loaded =
            super::LoadedTranscript::from_record_slice(tail, session_dir).expect("loaded tail");
        let mut document = super::TranscriptDocument::from_loaded_transcript(loaded);

        let range = document
            .record_window_range_for_approximate_display_row(10, 120, 10)
            .expect("record range");

        assert!(
            range.start.get() <= 10 && 10 < range.end.get(),
            "seek should center near the record whose cumulative estimate contains the row: {range:?}"
        );
        assert!(
            range.end.get() < 60,
            "seek must not use loaded tail average rows per record: {range:?}"
        );
    }

    #[test]
    fn scrollbar_total_ignores_exact_loaded_height_refinements_for_sparse_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let records = (0..16).map(test_record_record).collect::<Vec<_>>();
        let session_dir = seed_test_transcript_rows(dir.path(), records);
        let reader =
            smelt_store::LineageSessionReader::open_existing(dir.path(), TEST_LINEAGE_SESSION_ID)
                .unwrap();
        let tail = reader
            .transcript_record_slice_with_total((12..16).into(), 16)
            .unwrap();

        let loaded =
            super::LoadedTranscript::from_record_slice(tail, session_dir).expect("loaded tail");
        let mut document = super::TranscriptDocument::from_loaded_transcript(loaded);
        let before = document.scrollbar_total_rows(10, 1_000);

        let active_start = document.records.active_range().unwrap().start.get();
        let block_id = document
            .records
            .sparse
            .record(smelt_store::TranscriptRecordOffset::new(active_start))
            .unwrap()
            .block_id;
        let snapshot = crate::content::transcript_buf::TranscriptExactHeightSnapshot {
            width: 10,
            renderer_generation: 0,
            renderer_cache_key: None,
            presentation_generation: 0,
            observations: vec![
                crate::content::transcript_buf::TranscriptExactHeightObservation {
                    block_id,
                    key: smelt_core::transcript_model::LayoutKey {
                        width: 10,
                        view_state: smelt_core::transcript_model::ViewState::Expanded,
                        content_hash: 0,
                        sidecar_hash: 0,
                    },
                    rows: 1_000,
                },
            ],
        };
        document
            .extent_index
            .observe_exact_loaded_record_rows(&document.records.sparse, snapshot);
        let after = document.scrollbar_total_rows(10, 1_000);

        assert_eq!(before, after);
    }
}
