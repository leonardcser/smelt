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
    /// Block layout from the last `project()`. Backs `block_of_row`.
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
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct ProjectKey {
    generation: u64,
    width: u16,
    show_thinking: bool,
    /// Forces re-stitch when ephemeral content (active thinking) changes.
    ephemeral_fingerprint: Option<u64>,
}

pub(crate) struct ProjectOutput {
    pub total_rows: u16,
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

    /// Block at absolute row `row`. `None` for gap rows or rows past the projected total.
    pub(crate) fn block_of_row(&self, row: usize) -> Option<BlockId> {
        let row = row as u32;
        let idx = self.layout.partition_point(|e| e.start <= row);
        if idx == 0 {
            return None;
        }
        let entry = self.layout[idx - 1];
        let end = entry.start + entry.rows as u32;
        if row < end {
            Some(entry.id)
        } else {
            None
        }
    }

    fn gc_if_stale(&mut self, gen: u64, width: u16) {
        if gen != self.cache_generation || width != self.cache_width {
            self.cache.clear();
            self.cache_generation = gen;
            self.cache_width = width;
            self.project_key = None;
            self.layout.clear();
            self.cached_rows = None;
        }
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
        ephemeral: Option<&Buffer>,
        scroll_top: u16,
        viewport_rows: u16,
    ) -> ProjectOutput {
        let gen = history.generation();
        let ephemeral_fingerprint = ephemeral.map(fingerprint_ephemeral);
        let key = ProjectKey {
            generation: gen,
            width,
            show_thinking,
            ephemeral_fingerprint,
        };

        if self.project_key == Some(key) {
            let total_rows = buf.line_count() as u16;
            return ProjectOutput {
                total_rows,
                clamped_scroll: clamp_scroll(scroll_top, total_rows, viewport_rows),
            };
        }

        self.gc_if_stale(gen, width);

        let base_key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
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
            });
        }

        if let Some(eph) = ephemeral {
            for r in 0..eph.line_count() {
                let row_idx = texts.len();
                texts.push(eph.get_line(r).unwrap_or("").to_string());
                let h = eph.highlights_at(r);
                let dec = eph.decoration_at(r).clone();
                if !h.is_empty() || dec != LineDecoration::default() {
                    pending.push(PendingRow {
                        row: row_idx,
                        highlights: h,
                        decoration: dec,
                    });
                }
            }
        }

        let total_rows = clamp_u16(texts.len() as u32);
        buf.set_all_lines(texts);
        for p in pending {
            apply_row_highlights(buf, p.row, p.highlights);
            if p.decoration != LineDecoration::default() {
                buf.set_decoration(p.row, p.decoration);
            }
        }

        self.layout = layout;
        self.project_key = Some(key);

        ProjectOutput {
            total_rows,
            clamped_scroll: clamp_scroll(scroll_top, total_rows, viewport_rows),
        }
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
                }
            }
            for r in 0..block_buf.line_count() {
                let line_len = block_buf.get_line(r).unwrap_or("").len();
                pos += line_len;
                let current_soft = block_buf.decoration_at(r).soft_wrapped;
                push_row(&mut metas, pos, current_soft);
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

fn fingerprint_ephemeral(buf: &Buffer) -> u64 {
    seahash::hash(buf.text().as_bytes())
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
    fn copy(&self, buf: &Buffer, range: std::ops::Range<usize>) -> smelt_core::buffer::CopyOutput {
        let text = buf.text();
        let raw = text
            .get(range.start..range.end)
            .map(str::to_string)
            .unwrap_or_default();
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
        let line_chars = line.chars().count();
        let dec = buf.decoration_at(r);
        let is_soft = dec.soft_wrapped;
        if r > sr && !is_soft {
            out.push('\n');
            source_text_emitted = false;
        }

        let is_first = r == sr;
        let is_last = r == er;
        let c_start = if is_first { sc } else { 0 };
        let c_end = if is_last { ec.min(line_chars) } else { line_chars };

        let highlights = buf.highlights_at(r);
        let unselectable_intervals = collect_unselectable(&highlights, line_chars);
        let all_selectable_covered =
            all_selectable_in_range(&unselectable_intervals, line_chars, c_start, c_end);

        if all_selectable_covered && is_soft && source_text_emitted {
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
            let col = row[..col_byte].chars().count();
            return (r, col);
        }
        acc = row_end + 1;
    }
    let last_row = lines.len().saturating_sub(1);
    let last_col = lines.last().map(|r| r.chars().count()).unwrap_or(0);
    (last_row, last_col)
}

fn collect_unselectable(highlights: &[Span], line_chars: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for h in highlights {
        if h.meta.selectable {
            continue;
        }
        let s = (h.col_start as usize).min(line_chars);
        let e = (h.col_end as usize).min(line_chars);
        if e > s {
            out.push((s, e));
        }
    }
    out
}

fn all_selectable_in_range(
    unselectable: &[(usize, usize)],
    line_chars: usize,
    c_start: usize,
    c_end: usize,
) -> bool {
    'outer: for i in 0..line_chars {
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
    for (col, ch) in line.chars().enumerate() {
        if col < c_start || col >= c_end {
            continue;
        }
        let mut selectable = true;
        let mut copy_as_hit: Option<(usize, &str)> = None;
        for (idx, span) in highlights.iter().enumerate() {
            let s = span.col_start as usize;
            let e = span.col_end as usize;
            if col < s || col >= e {
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
    }
}

/// Snap `col` (display cell on `row`) to the nearest selectable cell.
pub(crate) fn snap_col_to_selectable(buf: &Buffer, row: usize, col: usize) -> usize {
    let Some(line) = buf.get_line(row) else {
        return col;
    };
    let line_chars = line.chars().count();
    if line_chars == 0 {
        return col;
    }
    let highlights = buf.highlights_at(row);
    let unselectable = collect_unselectable(&highlights, line_chars);
    let is_selectable =
        |c: usize| c < line_chars && !unselectable.iter().any(|(s, e)| c >= *s && c < *e);
    if is_selectable(col) {
        return col;
    }
    for c in (col + 1)..line_chars {
        if is_selectable(c) {
            return c;
        }
    }
    if col > 0 {
        for c in (0..col.min(line_chars)).rev() {
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
    use smelt_core::content::transcript::Transcript;
    use smelt_core::transcript_model::Block;

    #[test]
    fn project_renders_text_block_into_buffer() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "hello".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(1), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            None,
            0,
            80,
        );

        assert!(buf.line_count() > 0);
        assert_eq!(buf.get_line(buf.line_count() - 1), Some("hello"));
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
    fn copy_coalesces_soft_wrapped_rows_via_source_text() {
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
                soft_wrapped: true,
                ..Default::default()
            },
        );
        assert_eq!(copy_byte_range(&buf, 0, 11), "hello world");
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
}
