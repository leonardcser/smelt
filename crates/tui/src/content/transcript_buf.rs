use super::block_buffers::BlockBufferCache;
use crate::smelt_term::Buffer;
use crate::smelt_term::Theme;
use smelt_core::buffer::{LineDecoration, Span, SpanMeta};
use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey, ViewState};
use std::sync::Arc;

pub(crate) struct TranscriptProjection {
    cache: BlockBufferCache,
    cache_generation: u64,
    cache_width: u16,
    project_key: Option<ProjectKey>,
    /// Block layout from the last `project()`. Surfaced to Lua via `block_layout`.
    layout: Vec<LayoutEntry>,
    /// Cached `build_rows` result for full-text consumers (Lua API, vim navigation).
    cached_rows: Option<CachedRows>,
}

struct CachedRows {
    rows: Arc<Vec<String>>,
    generation: u64,
    width: u16,
    show_thinking: bool,
}

#[derive(Clone, Copy)]
struct LayoutEntry {
    id: BlockId,
    /// First absolute row of the block, after its leading gap.
    start: u32,
    rows: u16,
    key: LayoutKey,
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct ProjectKey {
    generation: u64,
    width: u16,
    show_thinking: bool,
}

pub(crate) struct ProjectOutput {
    pub clamped_scroll: u16,
}

impl TranscriptProjection {
    pub(crate) fn new() -> Self {
        Self {
            cache: BlockBufferCache::new(),
            cache_generation: u64::MAX,
            cache_width: 0,
            project_key: None,
            layout: Vec::new(),
            cached_rows: None,
        }
    }

    /// Snapshot of the laid-out blocks: `(BlockId, first_row, rows)` for each
    /// entry from the most recent `project()`. Used by Lua's
    /// `smelt.transcript.blocks()` to map block indices back to display rows
    /// without duplicating the layout walk.
    pub(crate) fn block_layout(&self) -> impl Iterator<Item = (BlockId, u16, u16)> + '_ {
        self.layout
            .iter()
            .map(|e| (e.id, e.start.min(u16::MAX as u32) as u16, e.rows))
    }

    fn gc_if_stale(&mut self, gen: u64, width: u16) {
        if width != self.cache_width {
            // Width change invalidates all layouts (wrapping changes).
            self.cache.clear();
            self.cache_width = width;
            self.project_key = None;
            self.layout.clear();
            self.cached_rows = None;
        }
        self.cache_generation = gen;
    }

    /// Clear every cached layout so the next `project()` rebuilds from scratch.
    /// Called when the theme changes — colors that were baked into anonymous
    /// highlight groups need to be re-resolved against the new palette.
    pub(crate) fn invalidate_theme(&mut self) {
        self.cache.clear();
        self.project_key = None;
        self.layout.clear();
        self.cached_rows = None;
    }

    /// Render every block (parallel on cache misses) and stitch the unified buffer.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn project(
        &mut self,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
        scroll_top: u16,
        viewport_rows: u16,
    ) -> ProjectOutput {
        let gen = history.generation();
        let key = ProjectKey {
            generation: gen,
            width,
            show_thinking,
        };

        if self.project_key == Some(key) {
            let total_rows = buf.line_count() as u16;
            return ProjectOutput {
                clamped_scroll: clamp_scroll(scroll_top, total_rows, viewport_rows),
            };
        }

        // When width changes, capture a content-stable anchor at the current
        // scroll_top — (BlockId, row_offset_in_block) — before the layout is
        // discarded. After the new layout is built we remap this back to a
        // visual row so resize keeps the same block anchored at the viewport
        // top instead of letting the visual-row counter drift.
        let width_changed = self
            .project_key
            .map(|prev| prev.width != width)
            .unwrap_or(false);
        let resize_anchor = if width_changed {
            self.block_anchor_at(scroll_top as usize)
        } else {
            None
        };

        self.gc_if_stale(gen, width);

        let base_key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
            sidecar_hash: 0,
        };
        let _perf = smelt_perf::perf::begin("project:render");

        let n = history.order.len();
        let mut block_ids: Vec<BlockId> = Vec::with_capacity(n);
        let mut block_keys: Vec<LayoutKey> = Vec::with_capacity(n);
        for i in 0..n {
            let id = history.order[i];
            block_ids.push(id);
            block_keys.push(history.resolve_key(id, base_key));
        }
        self.cache
            .ensure_many(history, &block_ids, &block_keys, theme);

        let mut texts: Vec<String> = Vec::with_capacity(n.saturating_mul(8));
        struct PendingRow {
            row: usize,
            highlights: Vec<Span>,
            decoration: LineDecoration,
        }
        let mut pending: Vec<PendingRow> = Vec::new();
        let mut layout: Vec<LayoutEntry> = Vec::with_capacity(n);

        for i in 0..n {
            let id = block_ids[i];
            let bkey = block_keys[i];
            let Some(block_buf) = self.cache.get(id, bkey) else {
                continue;
            };
            let block_rows = block_buf.line_count();
            if block_rows > 0 {
                let gap = history.block_gap(i);
                for _ in 0..gap {
                    texts.push(String::new());
                }
            }
            let start = texts.len() as u32;
            for r in 0..block_rows {
                let row_idx = texts.len();
                texts.push(block_buf.get_line(r).unwrap_or("").to_string());
                let h = block_buf.highlights_at(r);
                let dec = block_buf.decoration_at(r).clone();
                if !h.is_empty() || dec != LineDecoration::default() {
                    pending.push(PendingRow {
                        row: row_idx,
                        highlights: h,
                        decoration: dec,
                    });
                }
            }
            layout.push(LayoutEntry {
                id,
                start,
                rows: block_rows as u16,
                key: bkey,
            });
        }

        // Streaming fast-path: if only the last block grew, trim the buffer
        // to before the last block and append the new tail instead of
        // rebuilding from scratch. This keeps changedtick stable for earlier
        // rows so Window::render re-uses its WrappedLayout cache.
        let incremental = self.can_incremental(&layout)
            && self.apply_incremental(buf, history, &block_ids, &block_keys, &layout);

        if !incremental {
            buf.set_all_lines(texts);
            for p in pending {
                apply_row_highlights(buf, p.row, p.highlights);
                if p.decoration != LineDecoration::default() {
                    buf.set_decoration(p.row, p.decoration);
                }
            }
        }

        self.layout = layout;
        self.project_key = Some(key);
        let total_rows = clamp_u16(buf.line_count() as u32);

        let restored_scroll = resize_anchor
            .and_then(|(block_id, offset)| {
                self.layout.iter().find(|e| e.id == block_id).map(|entry| {
                    let target = entry.start.saturating_add(offset as u32);
                    clamp_u16(target)
                })
            })
            .unwrap_or(scroll_top);

        ProjectOutput {
            clamped_scroll: clamp_scroll(restored_scroll, total_rows, viewport_rows),
        }
    }

    // ── Incremental streaming helpers ─────────────────────────────────

    /// True when all earlier blocks are unchanged and only the last block's
    /// rendered suffix needs replacement.
    fn can_incremental(&self, new_layout: &[LayoutEntry]) -> bool {
        if self.layout.len() != new_layout.len() || self.layout.is_empty() {
            return false;
        }
        // All blocks except last must be identical (same id, rows, and cache key).
        let all_same_except_last = self
            .layout
            .iter()
            .zip(new_layout.iter())
            .take(self.layout.len().saturating_sub(1))
            .all(|(a, b)| a.id == b.id && a.rows == b.rows && a.key == b.key);
        if !all_same_except_last {
            return false;
        }
        // The last block may have a different key while streaming because its
        // content hash changes. `apply_incremental` replaces the whole last
        // block suffix, so only the stable identity matters here.
        let old_last = self.layout.last().unwrap();
        let new_last = new_layout.last().unwrap();
        old_last.id == new_last.id
    }

    /// Replace the last block's rendered suffix. Returns true on success.
    fn apply_incremental(
        &mut self,
        buf: &mut Buffer,
        history: &BlockHistory,
        block_ids: &[BlockId],
        block_keys: &[LayoutKey],
        _new_layout: &[LayoutEntry],
    ) -> bool {
        let old_last = match self.layout.last() {
            Some(e) => e,
            None => return false,
        };

        let i = block_ids.len().saturating_sub(1);
        let id = block_ids[i];
        let bkey = block_keys[i];
        let block_buf = match self.cache.get(id, bkey) {
            Some(b) => b,
            None => return false,
        };
        let block_rows = block_buf.line_count();

        // Trim buffer to just before the last block's old start.
        // If the last block previously had rows, it was preceded by a gap
        // that must also be removed — otherwise the gap is duplicated when
        // we re-append gap + block rows below.
        let mut keep_rows = old_last.start as usize;
        if old_last.rows > 0 {
            let gap = history.block_gap(i);
            keep_rows = keep_rows.saturating_sub(gap as usize);
        }
        // Replace the entire suffix in one buffer mutation. Besides being
        // easier to reason about, this lets the window update only the changed
        // suffix of its wrap layout.
        let gap = if block_rows > 0 {
            history.block_gap(i) as usize
        } else {
            0
        };
        let mut new_lines: Vec<String> = Vec::with_capacity(gap + block_rows);
        if block_rows > 0 {
            for _ in 0..gap {
                new_lines.push(String::new());
            }
        }
        for r in 0..block_rows {
            new_lines.push(block_buf.get_line(r).unwrap_or("").to_string());
        }

        let old_line_count = buf.line_count();
        let inserted_len = new_lines.len();
        buf.set_lines(keep_rows, old_line_count, new_lines);

        let base_row = keep_rows;
        let end_row = base_row + inserted_len;

        // Apply highlights/decorations for the appended rows.
        for r in 0..block_rows {
            let row = base_row + gap + r;
            if row >= end_row {
                break;
            }
            let h = block_buf.highlights_at(r);
            if !h.is_empty() {
                apply_row_highlights(buf, row, h);
            }
            let dec = block_buf.decoration_at(r);
            if *dec != LineDecoration::default() {
                buf.set_decoration(row, dec.clone());
            }
        }
        true
    }

    /// Map an absolute row to its `(BlockId, row_offset_within_block)`. Gap
    /// rows resolve to the previous block's last row so a scroll position
    /// stranded in a gap still anchors to a stable block boundary. Rows past
    /// the end of all blocks (e.g. `follow_tail`'s `u16::MAX` sentinel) return
    /// `None` so the caller falls back to scroll_top and the natural clamp
    /// pins the viewport to the new bottom.
    fn block_anchor_at(&self, row: usize) -> Option<(BlockId, u16)> {
        let last = self.layout.last()?;
        let last_end = last.start.saturating_add(last.rows as u32);
        let row_u32 = row as u32;
        if row_u32 >= last_end {
            return None;
        }
        let idx = self.layout.partition_point(|e| e.start <= row_u32);
        if idx == 0 {
            return None;
        }
        let entry = self.layout[idx - 1];
        let end = entry.start.saturating_add(entry.rows as u32);
        let offset_u32 = if row_u32 < end {
            row_u32 - entry.start
        } else {
            entry.rows.saturating_sub(1) as u32
        };
        Some((entry.id, offset_u32.min(u16::MAX as u32) as u16))
    }

    /// Render every block into the cache. For full-text consumers that may run
    /// before the next `project()`.
    pub(crate) fn ensure_all(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) {
        let gen = history.generation();
        self.gc_if_stale(gen, width);
        let base_key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
            sidecar_hash: 0,
        };
        let n = history.order.len();
        let mut ids = Vec::with_capacity(n);
        let mut keys = Vec::with_capacity(n);
        for i in 0..n {
            let id = history.order[i];
            ids.push(id);
            keys.push(history.resolve_key(id, base_key));
        }
        self.cache.ensure_many(history, &ids, &keys, theme);
    }

    /// Full display rows. Cached by `(generation, width, show_thinking)`; repeat
    /// callers get a free `Arc::clone`.
    pub(crate) fn build_rows(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> Arc<Vec<String>> {
        let gen = history.generation();
        if let Some(c) = &self.cached_rows {
            if c.generation == gen && c.width == width && c.show_thinking == show_thinking {
                return Arc::clone(&c.rows);
            }
        }
        self.ensure_all(history, width, show_thinking, theme);
        let base_key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
            sidecar_hash: 0,
        };
        let mut rows: Vec<String> = Vec::new();
        for i in 0..history.order.len() {
            let id = history.order[i];
            let bkey = history.resolve_key(id, base_key);
            let Some(block_buf) = self.cache.get(id, bkey) else {
                continue;
            };
            if block_buf.line_count() > 0 {
                let gap = history.block_gap(i);
                for _ in 0..gap {
                    rows.push(String::new());
                }
            }
            for r in 0..block_buf.line_count() {
                rows.push(block_buf.get_line(r).unwrap_or("").to_string());
            }
        }
        let rows = Arc::new(rows);
        self.cached_rows = Some(CachedRows {
            rows: Arc::clone(&rows),
            generation: gen,
            width,
            show_thinking,
        });
        rows
    }

    /// Soft (word-wrap) and hard (`\n`) byte positions in
    /// `build_rows(..).join("\n")`. Soft positions are transparent to
    /// word-select; hard positions bound line-select.
    pub(crate) fn line_breaks(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> (Vec<usize>, Vec<usize>) {
        self.ensure_all(history, width, show_thinking, theme);
        let base_key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
            sidecar_hash: 0,
        };

        // The break ending row r is soft iff r+1 has `decoration.soft_wrapped`.
        struct RowMeta {
            byte_end: usize,
            next_soft: bool,
        }
        let mut metas: Vec<RowMeta> = Vec::new();
        let mut pos = 0usize;

        let push_row = |metas: &mut Vec<RowMeta>, byte_end: usize, current_is_soft: bool| {
            if let Some(prev) = metas.last_mut() {
                prev.next_soft = current_is_soft;
            }
            metas.push(RowMeta {
                byte_end,
                next_soft: false,
            });
        };

        for i in 0..history.order.len() {
            let id = history.order[i];
            let bkey = history.resolve_key(id, base_key);
            let Some(block_buf) = self.cache.get(id, bkey) else {
                continue;
            };
            if block_buf.line_count() > 0 {
                let gap = history.block_gap(i);
                for _ in 0..gap {
                    push_row(&mut metas, pos, false);
                    pos += 1;
                }
            }
            for r in 0..block_buf.line_count() {
                let line_len = block_buf.get_line(r).unwrap_or("").len();
                pos += line_len;
                let current_soft = block_buf.decoration_at(r).soft_wrapped;
                push_row(&mut metas, pos, current_soft);
                pos += 1;
            }
        }

        let mut soft = Vec::new();
        let mut hard = Vec::new();
        let last = metas.len().saturating_sub(1);
        for (i, m) in metas.iter().enumerate() {
            if i == last {
                continue;
            }
            if m.next_soft {
                soft.push(m.byte_end);
            } else {
                hard.push(m.byte_end);
            }
        }
        (soft, hard)
    }
}

fn clamp_u16(v: u32) -> u16 {
    v.min(u16::MAX as u32) as u16
}

fn clamp_scroll(scroll_top: u16, total_rows: u16, viewport_rows: u16) -> u16 {
    scroll_top.min(total_rows.saturating_sub(viewport_rows))
}

fn apply_row_highlights(buf: &mut Buffer, row: usize, highlights: Vec<Span>) {
    for span in highlights {
        let meta: SpanMeta = span.meta;
        buf.add_highlight_group_with_meta(row, span.col_start, span.col_end, span.hl, meta);
    }
}

/// Yank transform for the transcript. `kill_ring` keeps the raw source bytes;
/// `clipboard` walks the buffer's cells so `copy_as` substitutions, soft-wrap
/// merging, and `source_text` row overrides are honored on external paste.
pub(crate) struct TranscriptCopier;

impl smelt_core::buffer::BufferCopy for TranscriptCopier {
    fn copy(
        &self,
        buf: &Buffer,
        src: &str,
        range: std::ops::Range<usize>,
    ) -> smelt_core::buffer::CopyOutput {
        let raw = src[range.start..range.end].to_string();
        let clipboard = copy_byte_range(buf, range.start, range.end);
        smelt_core::buffer::CopyOutput {
            kill_ring: raw,
            clipboard,
        }
    }
}

/// Render the byte range as user-facing text: drops non-selectable cells,
/// applies `copy_as`, prefers `source_text` on fully-covered rows, coalesces
/// soft-wrapped runs.
pub(crate) fn copy_byte_range(buf: &Buffer, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    let lines = buf.lines();
    let (sr, sc) = byte_to_row_col(lines, start);
    let (er, ec) = byte_to_row_col(lines, end);
    let er = er.min(lines.len().saturating_sub(1));

    let mut out = String::new();
    let mut source_text_emitted = false;
    for (r, line) in lines.iter().enumerate().take(er + 1).skip(sr) {
        let line_width = smelt_buffer::text::byte_to_cell(line, line.len());
        let dec = buf.decoration_at(r);
        let is_soft = dec.soft_wrapped;
        let is_copy_cont = dec.copy_continuation;
        if r > sr && !is_soft && !is_copy_cont {
            out.push('\n');
            source_text_emitted = false;
        }

        let is_first = r == sr;
        let is_last = r == er;
        let c_start = if is_first { sc } else { 0 };
        let c_end = if is_last {
            ec.min(line_width)
        } else {
            line_width
        };

        let highlights = buf.highlights_at(r);
        let unselectable_intervals = collect_unselectable(&highlights, line_width);
        let all_selectable_covered =
            all_selectable_in_range(&unselectable_intervals, line_width, c_start, c_end);

        if all_selectable_covered && is_copy_cont && source_text_emitted {
            continue;
        }

        if all_selectable_covered {
            if let Some(src) = dec.source_text.as_deref() {
                out.push_str(src);
                source_text_emitted = true;
                continue;
            }
        }

        emit_row_cells(line, &highlights, c_start, c_end, &mut out);
    }
    out
}

fn byte_to_row_col(lines: &[String], byte: usize) -> (usize, usize) {
    let mut acc = 0usize;
    for (r, row) in lines.iter().enumerate() {
        let row_end = acc + row.len();
        if byte <= row_end {
            let col_byte = byte.saturating_sub(acc).min(row.len());
            let col = smelt_buffer::text::byte_to_cell(row, col_byte);
            return (r, col);
        }
        acc = row_end + 1;
    }
    let last_row = lines.len().saturating_sub(1);
    let last_col = lines
        .last()
        .map(|r| smelt_buffer::text::byte_to_cell(r, r.len()))
        .unwrap_or(0);
    (last_row, last_col)
}

fn collect_unselectable(highlights: &[Span], line_width: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for h in highlights {
        if h.meta.selectable {
            continue;
        }
        let s = (h.col_start as usize).min(line_width);
        let e = (h.col_end as usize).min(line_width);
        if e > s {
            out.push((s, e));
        }
    }
    out
}

fn all_selectable_in_range(
    unselectable: &[(usize, usize)],
    line_width: usize,
    c_start: usize,
    c_end: usize,
) -> bool {
    'outer: for i in 0..line_width {
        for (s, e) in unselectable {
            if i >= *s && i < *e {
                continue 'outer;
            }
        }
        if i < c_start || i >= c_end {
            return false;
        }
    }
    true
}

fn emit_row_cells(line: &str, highlights: &[Span], c_start: usize, c_end: usize, out: &mut String) {
    let mut emitted_copy_as: Vec<usize> = Vec::new();
    let mut col = 0usize;
    for ch in line.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(0)
            .max(1);
        let ch_end = col.saturating_add(w);
        if ch_end <= c_start || col >= c_end {
            col = ch_end;
            continue;
        }
        let mut selectable = true;
        let mut copy_as_hit: Option<(usize, &str)> = None;
        for (idx, span) in highlights.iter().enumerate() {
            let s = span.col_start as usize;
            let e = span.col_end as usize;
            if ch_end <= s || col >= e {
                continue;
            }
            if !span.meta.selectable {
                selectable = false;
                break;
            }
            if let Some(s_str) = span.meta.copy_as.as_deref() {
                copy_as_hit = Some((idx, s_str));
            }
        }
        if !selectable {
            col = ch_end;
            continue;
        }
        if let Some((idx, s)) = copy_as_hit {
            if !emitted_copy_as.contains(&idx) {
                out.push_str(s);
                emitted_copy_as.push(idx);
            }
        } else {
            out.push(ch);
        }
        col = ch_end;
    }
}

/// Snap `col` (display cell on `row`) to the nearest selectable cell.
pub(crate) fn snap_col_to_selectable(buf: &Buffer, row: usize, col: usize) -> usize {
    let Some(line) = buf.get_line(row) else {
        return col;
    };
    let line_width = smelt_buffer::text::byte_to_cell(line, line.len());
    if line_width == 0 {
        return col;
    }
    let highlights = buf.highlights_at(row);
    let unselectable = collect_unselectable(&highlights, line_width);
    let is_selectable =
        |c: usize| c < line_width && !unselectable.iter().any(|(s, e)| c >= *s && c < *e);
    if is_selectable(col) {
        return col;
    }
    for c in (col + 1)..line_width {
        if is_selectable(c) {
            return c;
        }
    }
    if col > 0 {
        for c in (0..col.min(line_width)).rev() {
            if is_selectable(c) {
                return c;
            }
        }
    }
    col
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::content::stream_parser::StreamParser;
    use smelt_core::content::transcript::Transcript;
    use smelt_core::transcript_model::{Block, ToolStatus};

    #[derive(Debug, PartialEq)]
    struct RowSnapshot {
        line: String,
        highlights: Vec<Span>,
        decoration: LineDecoration,
    }

    fn snapshot(buf: &Buffer) -> Vec<RowSnapshot> {
        (0..buf.line_count())
            .map(|row| RowSnapshot {
                line: buf.get_line(row).unwrap_or("").to_string(),
                highlights: buf.highlights_at(row),
                decoration: buf.decoration_at(row).clone(),
            })
            .collect()
    }

    fn project_fresh(history: &mut smelt_core::transcript_model::BlockHistory) -> Vec<RowSnapshot> {
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(99), Default::default());
        projection.project(&mut buf, history, 80, false, &theme, 0, 80);
        snapshot(&buf)
    }

    #[test]
    fn project_renders_text_block_into_buffer() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "hello".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(1), Default::default());

        projection.project(&mut buf, &mut transcript.history, 80, false, &theme, 0, 80);

        assert!(buf.line_count() > 0);
        assert_eq!(buf.get_line(buf.line_count() - 1), Some("hello"));
    }

    #[test]
    fn incremental_projection_matches_full_after_markdown_table_growth() {
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "show a table".into(),
            image_labels: vec![],
        });
        let mut parser = StreamParser::new();
        parser.append_streaming_text(
            &mut transcript.history,
            "| Name | Value |\n| --- | --- |\n| alpha |",
        );

        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(2), Default::default());
        projection.project(&mut buf, &mut transcript.history, 80, false, &theme, 0, 80);

        parser.append_streaming_text(&mut transcript.history, " 1 |");
        projection.project(&mut buf, &mut transcript.history, 80, false, &theme, 0, 80);

        let incremental = snapshot(&buf);
        let full = project_fresh(&mut transcript.history);
        assert_eq!(incremental, full);
    }

    #[test]
    fn incremental_projection_rerenders_tool_state_changes() {
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "run ls".into(),
            image_labels: vec![],
        });
        let mut parser = StreamParser::new();
        parser.start_tool(
            &mut transcript.history,
            "call-1".into(),
            "bash".into(),
            protocol::StyledLines::from_plain("ls"),
            std::collections::HashMap::new(),
            std::time::Instant::now(),
        );

        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(3), Default::default());
        projection.project(&mut buf, &mut transcript.history, 80, false, &theme, 0, 80);
        let before = snapshot(&buf);

        parser.append_active_output(&mut transcript.history, "call-1", "done");
        parser.set_active_status(
            &mut transcript.history,
            "call-1",
            ToolStatus::Ok,
            std::time::Instant::now(),
        );
        projection.project(&mut buf, &mut transcript.history, 80, false, &theme, 0, 80);

        let incremental = snapshot(&buf);
        let full = project_fresh(&mut transcript.history);
        assert_ne!(incremental, before);
        assert_eq!(incremental, full);
    }

    #[test]
    fn copy_byte_range_basic_text() {
        let mut buf = Buffer::new(crate::smelt_term::BufId(1), Default::default());
        buf.set_all_lines(vec!["hello".into(), "world".into()]);
        assert_eq!(copy_byte_range(&buf, 0, 5), "hello");
        assert_eq!(copy_byte_range(&buf, 0, 11), "hello\nworld");
        assert_eq!(copy_byte_range(&buf, 6, 11), "world");
    }

    fn unselectable_meta() -> SpanMeta {
        SpanMeta {
            selectable: false,
            copy_as: None,
        }
    }

    fn copy_as_meta(s: &str) -> SpanMeta {
        SpanMeta {
            selectable: true,
            copy_as: Some(s.to_string()),
        }
    }

    fn hl_for_test() -> smelt_core::theme::HlGroup {
        smelt_core::theme::intern("Normal")
    }

    #[test]
    fn copy_skips_non_selectable_chrome() {
        let mut buf = Buffer::new(crate::smelt_term::BufId(1), Default::default());
        buf.set_all_lines(vec!["│ hi".into()]);
        buf.add_highlight_group_with_meta(0, 0, 2, hl_for_test(), unselectable_meta());
        let line_bytes = "│ hi".len();
        assert_eq!(copy_byte_range(&buf, 0, line_bytes), "hi");
    }

    #[test]
    fn copy_applies_copy_as_substitution_once_per_span() {
        let mut buf = Buffer::new(crate::smelt_term::BufId(1), Default::default());
        buf.set_all_lines(vec!["+ add".into()]);
        buf.add_highlight_group_with_meta(0, 0, 2, hl_for_test(), copy_as_meta(""));
        assert_eq!(copy_byte_range(&buf, 0, "+ add".len()), "add");
    }

    #[test]
    fn copy_uses_source_text_when_full_row_selected() {
        let mut buf = Buffer::new(crate::smelt_term::BufId(1), Default::default());
        buf.set_all_lines(vec!["Title".into()]);
        buf.set_decoration(
            0,
            LineDecoration {
                source_text: Some("# Title".into()),
                ..Default::default()
            },
        );
        assert_eq!(copy_byte_range(&buf, 0, 5), "# Title");
        assert_eq!(copy_byte_range(&buf, 1, 4), "itl");
    }

    #[test]
    fn copy_coalesces_copy_continuation_rows_via_source_text() {
        let mut buf = Buffer::new(crate::smelt_term::BufId(1), Default::default());
        buf.set_all_lines(vec!["hello".into(), "world".into()]);
        buf.set_decoration(
            0,
            LineDecoration {
                source_text: Some("hello world".into()),
                ..Default::default()
            },
        );
        buf.set_decoration(
            1,
            LineDecoration {
                copy_continuation: true,
                ..Default::default()
            },
        );
        assert_eq!(copy_byte_range(&buf, 0, 11), "hello world");
    }

    #[test]
    fn copy_copy_continuation_without_source_text_emits_all_rows() {
        let mut buf = Buffer::new(crate::smelt_term::BufId(1), Default::default());
        buf.set_all_lines(vec!["abc".into(), "def".into()]);
        buf.set_decoration(
            1,
            LineDecoration {
                copy_continuation: true,
                ..Default::default()
            },
        );
        assert_eq!(copy_byte_range(&buf, 0, 7), "abcdef");
    }

    #[test]
    fn copy_soft_wrap_without_source_text_emits_all_rows() {
        let mut buf = Buffer::new(crate::smelt_term::BufId(1), Default::default());
        buf.set_all_lines(vec!["abc".into(), "def".into()]);
        buf.set_decoration(
            1,
            LineDecoration {
                soft_wrapped: true,
                ..Default::default()
            },
        );
        assert_eq!(copy_byte_range(&buf, 0, 7), "abcdef");
    }

    #[test]
    fn table_rows_are_hard_breaks_not_soft() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "before".into(),
        });
        transcript.push(Block::Text {
            content: "| a | b |\n| - | - |\n| 1 | 2 |".into(),
        });
        transcript.push(Block::Text {
            content: "after".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(3), Default::default());
        projection.project(&mut buf, &mut transcript.history, 40, false, &theme, 0, 80);

        let (soft, _hard) = projection.line_breaks(&mut transcript.history, 40, false, &theme);
        // For a transcript with only a table, every row boundary should be a
        // hard break (no soft breaks) so triple-click selects one display row.
        // The table is at rows 2-6; verify no soft break falls inside it.
        let lines = buf.lines();
        let mut table_start = None;
        let mut table_end = None;
        for (i, line) in lines.iter().enumerate() {
            if line.contains('\u{2503}') {
                if table_start.is_none() {
                    table_start = Some(i);
                }
                table_end = Some(i);
            }
        }
        let (t0, t1) = (
            table_start.expect("table start"),
            table_end.expect("table end"),
        );
        let mut acc = 0usize;
        for (i, line) in lines.iter().enumerate() {
            acc += line.len();
            if i + 1 < lines.len() {
                let break_pos = acc;
                if i >= t0 && i < t1 && soft.contains(&break_pos) {
                    panic!(
                        "row {} boundary at {} should be hard, not soft",
                        i + 1,
                        break_pos
                    );
                }
                acc += 1; // '\n'
            }
        }
    }

    #[test]
    fn line_break_offsets_include_join_newlines() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "aa\nbbb\nc".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(7), Default::default());
        projection.project(&mut buf, &mut transcript.history, 80, false, &theme, 0, 80);

        let (soft, hard) = projection.line_breaks(&mut transcript.history, 80, false, &theme);
        assert!(
            soft.is_empty(),
            "unwrapped source lines must be hard breaks"
        );
        assert_eq!(hard, crate::smelt_term::hard_breaks_for_lines(buf.lines()));
    }

    #[test]
    fn table_full_selection_copies_raw_markdown() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "| a | b |\n| - | - |\n| 1 | 2 |".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(4), Default::default());
        projection.project(&mut buf, &mut transcript.history, 40, false, &theme, 0, 80);

        // Select the entire table (all rows).
        let total_bytes = buf.text().len();
        let copied = copy_byte_range(&buf, 0, total_bytes);
        assert_eq!(copied, "| a | b |\n| - | - |\n| 1 | 2 |");
    }

    #[test]
    fn table_single_row_selection_copies_cell_contents() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "| a | b |\n| - | - |\n| 1 | 2 |".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(5), Default::default());
        projection.project(&mut buf, &mut transcript.history, 40, false, &theme, 0, 80);

        let lines = buf.lines();
        // Find the data row (contains '1' and '2').
        let data_row = lines
            .iter()
            .position(|l| l.contains('1') && l.contains('2'))
            .expect("data row");
        let mut acc = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if i == data_row {
                let row_end = acc + line.len();
                let copied = copy_byte_range(&buf, acc, row_end);
                // Should emit selectable cells only, no borders or padding.
                assert_eq!(copied, "12");
                break;
            }
            acc += line.len() + 1;
        }
    }

    #[test]
    fn narrow_stacked_table_does_not_overflow_transcript_width() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "\
| Approach | Worth it? | Risk | Notes |
| --- | --- | --- | --- |
| Revert pre-pruning and add retry loop | Yes fixes cache and matches reference | Low | Post-compaction token recompute |
"
            .into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(6), Default::default());
        projection.project(&mut buf, &mut transcript.history, 24, false, &theme, 0, 80);

        let lines = buf.lines();
        assert!(
            lines.iter().all(|line| !line.contains('┏')),
            "expected stacked fallback, got {lines:?}"
        );
        for line in lines {
            let width = smelt_core::content::builder::display_width(line);
            assert!(
                width <= 24,
                "projected table row overflowed transcript width: width={width}, line={line:?}"
            );
        }
    }
}
