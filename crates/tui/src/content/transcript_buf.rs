use super::block_buffers::BlockBufferCache;
use crate::smelt_term::Theme;
use crate::smelt_term::{Buffer, RowIndex};
use smelt_core::buffer::{LineDecoration, Span, SpanMeta};
use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey, ViewState};
use std::sync::Arc;

const TAIL_OVERSCAN_ROWS: RowIndex = 20;

pub(crate) struct TranscriptProjection {
    cache: BlockBufferCache,
    cache_generation: u64,
    cache_width: u16,
    project_key: Option<ProjectKey>,
    /// Block layout from the last visible `project()`. Surfaced to Lua via `visible_blocks`.
    visible_layout: Vec<LayoutEntry>,
    /// Absolute row represented by local row 0 in the backing buffer.
    visible_row_base: RowIndex,
    /// Total rows in the logical transcript represented by the visible projection.
    visible_total_rows: RowIndex,
    /// Cached `build_rows` result for full-text consumers (Lua API, vim navigation).
    cached_rows: Option<CachedRows>,
    document: TranscriptDocument,
}

struct CachedRows {
    rows: Arc<Vec<String>>,
    generation: u64,
    width: u16,
    show_thinking: bool,
}

#[derive(Default)]
struct TranscriptDocument {
    nodes: Vec<TranscriptNode>,
    prefix_rows: Vec<RowIndex>,
    generation: u64,
    width: u16,
    show_thinking: bool,
}

struct TranscriptNode {
    id: BlockId,
    key: LayoutKey,
    estimated_height: RowIndex,
    exact_height: Option<RowIndex>,
}

impl TranscriptDocument {
    fn rebuild_if_stale(
        &mut self,
        history: &BlockHistory,
        width: u16,
        show_thinking: bool,
        base_key: LayoutKey,
    ) {
        let gen = history.generation();
        if self.generation == gen && self.width == width && self.show_thinking == show_thinking {
            return;
        }

        self.nodes.clear();
        self.nodes.reserve(history.order.len());
        for &id in &history.order {
            self.nodes.push(TranscriptNode {
                id,
                key: history.resolve_key(id, base_key),
                estimated_height: 1,
                exact_height: None,
            });
        }
        self.generation = gen;
        self.width = width;
        self.show_thinking = show_thinking;
        self.rebuild_prefix_rows();
    }

    fn set_exact_height(&mut self, index: usize, rows: RowIndex) {
        let Some(node) = self.nodes.get_mut(index) else {
            return;
        };
        node.exact_height = Some(rows);
    }

    fn refresh_height_index(&mut self) {
        self.rebuild_prefix_rows();
    }

    fn prefix_row(&self, index: usize) -> RowIndex {
        self.prefix_rows.get(index).copied().unwrap_or(0)
    }

    fn total_rows(&self) -> RowIndex {
        self.prefix_rows.last().copied().unwrap_or(0)
    }

    fn tail_start_index(&self, viewport_rows: u16) -> usize {
        let target_rows = (viewport_rows as RowIndex).saturating_add(TAIL_OVERSCAN_ROWS);
        let mut selected_rows: RowIndex = 0;
        let mut first = self.nodes.len();
        for i in (0..self.nodes.len()).rev() {
            let node = &self.nodes[i];
            selected_rows =
                selected_rows.saturating_add(node.exact_height.unwrap_or(node.estimated_height));
            first = i;
            if selected_rows >= target_rows {
                break;
            }
        }
        first
    }

    fn tail_block_ids(&self, viewport_rows: u16) -> impl Iterator<Item = BlockId> + '_ {
        self.nodes[self.tail_start_index(viewport_rows)..]
            .iter()
            .map(|node| node.id)
    }

    fn rebuild_prefix_rows(&mut self) {
        self.prefix_rows.clear();
        self.prefix_rows.reserve(self.nodes.len() + 1);
        self.prefix_rows.push(0);
        let mut total: RowIndex = 0;
        for node in &self.nodes {
            total = total.saturating_add(node.exact_height.unwrap_or(node.estimated_height));
            self.prefix_rows.push(total);
        }
    }
}

#[derive(Clone, Copy)]
struct LayoutEntry {
    id: BlockId,
    /// First absolute row of the block, after its leading gap.
    start: RowIndex,
    rows: RowIndex,
    key: LayoutKey,
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct ProjectKey {
    generation: u64,
    width: u16,
    show_thinking: bool,
    tail_viewport_rows: Option<u16>,
}

pub(crate) struct ProjectOutput {
    pub clamped_scroll: RowIndex,
    pub row_base: RowIndex,
    pub total_rows: RowIndex,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollTarget {
    Row(RowIndex),
    Tail,
}

impl ScrollTarget {
    fn as_scroll_top(self) -> RowIndex {
        match self {
            Self::Row(row) => row,
            Self::Tail => RowIndex::MAX,
        }
    }
}

struct PendingRow {
    row: usize,
    highlights: Vec<Span>,
    decoration: LineDecoration,
}

struct ProjectRows<'a> {
    row_base: RowIndex,
    texts: &'a mut Vec<String>,
    pending: &'a mut Vec<PendingRow>,
    layout: &'a mut Vec<LayoutEntry>,
}

fn base_layout_key(width: u16, show_thinking: bool) -> LayoutKey {
    LayoutKey {
        view_state: ViewState::Expanded,
        width,
        show_thinking,
        content_hash: 0,
        sidecar_hash: 0,
    }
}

impl TranscriptProjection {
    pub(crate) fn new() -> Self {
        Self {
            cache: BlockBufferCache::new(),
            cache_generation: u64::MAX,
            cache_width: 0,
            project_key: None,
            visible_layout: Vec::new(),
            visible_row_base: 0,
            visible_total_rows: 0,
            cached_rows: None,
            document: TranscriptDocument::default(),
        }
    }

    /// Snapshot of the visibly laid-out blocks: `(BlockId, first_row, rows)`.
    /// Used by Lua's `smelt.transcript.visible_blocks()` to map block indices
    /// back to display rows without forcing full transcript materialization.
    pub(crate) fn visible_block_layout(
        &self,
    ) -> impl Iterator<Item = (BlockId, RowIndex, RowIndex)> + '_ {
        self.visible_layout.iter().map(|e| (e.id, e.start, e.rows))
    }

    fn gc_if_stale(&mut self, gen: u64, width: u16) {
        if width != self.cache_width {
            // Width change invalidates all layouts (wrapping changes).
            self.cache.clear();
            self.cache_width = width;
            self.project_key = None;
            self.visible_layout.clear();
            self.visible_row_base = 0;
            self.visible_total_rows = 0;
            self.cached_rows = None;
            self.document = TranscriptDocument::default();
        }
        self.cache_generation = gen;
    }

    /// Clear every cached layout so the next `project()` rebuilds from scratch.
    /// Called when the theme changes — colors that were baked into anonymous
    /// highlight groups need to be re-resolved against the new palette.
    pub(crate) fn invalidate_theme(&mut self) {
        self.cache.clear();
        self.project_key = None;
        self.visible_layout.clear();
        self.visible_row_base = 0;
        self.visible_total_rows = 0;
        self.cached_rows = None;
        self.document = TranscriptDocument::default();
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
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> ProjectOutput {
        let scroll_top = scroll_target.as_scroll_top();
        let gen = history.generation();
        let key = ProjectKey {
            generation: gen,
            width,
            show_thinking,
            tail_viewport_rows: (scroll_target == ScrollTarget::Tail).then_some(viewport_rows),
        };

        if self.project_key == Some(key) {
            let total_rows = match scroll_target {
                ScrollTarget::Row(_) => buf.line_count() as RowIndex,
                ScrollTarget::Tail => self.visible_total_rows,
            };
            return ProjectOutput {
                clamped_scroll: clamp_scroll(scroll_top, total_rows, viewport_rows),
                row_base: self.visible_row_base,
                total_rows,
            };
        }

        if scroll_target == ScrollTarget::Tail {
            return self.project_tail_visible(
                buf,
                history,
                width,
                show_thinking,
                theme,
                viewport_rows,
                key,
            );
        }

        // When width changes, capture a content-stable anchor at the current
        // scroll_top — (BlockId, row_offset_in_block) — before the layout is
        // discarded. The same remap is needed when leaving a tail projection:
        // prefix rows may have been estimated, but the block-local anchor is stable.
        let width_changed = self
            .project_key
            .map(|prev| prev.width != width)
            .unwrap_or(false);
        let leaving_tail_projection = self
            .project_key
            .map(|prev| prev.tail_viewport_rows.is_some())
            .unwrap_or(false);
        let resize_anchor = if width_changed || leaving_tail_projection {
            self.block_anchor_at(scroll_top)
        } else {
            None
        };

        self.gc_if_stale(gen, width);

        let base_key = base_layout_key(width, show_thinking);
        self.document
            .rebuild_if_stale(history, width, show_thinking, base_key);
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
        let mut pending: Vec<PendingRow> = Vec::new();
        let mut layout: Vec<LayoutEntry> = Vec::with_capacity(n);
        let mut rows = ProjectRows {
            row_base: 0,
            texts: &mut texts,
            pending: &mut pending,
            layout: &mut layout,
        };

        for i in 0..n {
            self.append_projected_block(history, i, block_ids[i], block_keys[i], &mut rows);
        }
        self.document.refresh_height_index();

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

        self.visible_layout = layout;
        let total_rows = buf.line_count() as RowIndex;
        self.visible_row_base = 0;
        self.visible_total_rows = total_rows;
        self.project_key = Some(key);

        let restored_scroll = resize_anchor
            .and_then(|(block_id, offset)| {
                self.visible_layout
                    .iter()
                    .find(|e| e.id == block_id)
                    .map(|entry| entry.start.saturating_add(offset))
            })
            .unwrap_or(scroll_top);

        ProjectOutput {
            clamped_scroll: clamp_scroll(restored_scroll, total_rows, viewport_rows),
            row_base: 0,
            total_rows,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn project_tail_visible(
        &mut self,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
        viewport_rows: u16,
        key: ProjectKey,
    ) -> ProjectOutput {
        let gen = history.generation();
        self.gc_if_stale(gen, width);

        let base_key = base_layout_key(width, show_thinking);
        self.document
            .rebuild_if_stale(history, width, show_thinking, base_key);

        let first = self.document.tail_start_index(viewport_rows);
        let ids: Vec<BlockId> = self.document.nodes[first..]
            .iter()
            .map(|node| node.id)
            .collect();
        let keys: Vec<LayoutKey> = self.document.nodes[first..]
            .iter()
            .map(|node| node.key)
            .collect();
        self.cache.ensure_many(history, &ids, &keys, theme);

        let row_base = self.document.prefix_row(first);
        let mut texts: Vec<String> = Vec::new();
        let mut pending = Vec::new();
        let mut layout = Vec::with_capacity(ids.len());
        let mut rows = ProjectRows {
            row_base,
            texts: &mut texts,
            pending: &mut pending,
            layout: &mut layout,
        };

        for (offset, (&id, &bkey)) in ids.iter().zip(keys.iter()).enumerate() {
            self.append_projected_block(history, first + offset, id, bkey, &mut rows);
        }

        self.document.refresh_height_index();
        let total_rows = self.document.total_rows();
        let materialized_rows = texts.len() as RowIndex;
        buf.set_all_lines(texts);
        for p in pending {
            apply_row_highlights(buf, p.row, p.highlights);
            if p.decoration != LineDecoration::default() {
                buf.set_decoration(p.row, p.decoration);
            }
        }
        self.visible_layout = layout;
        self.visible_row_base = row_base;
        self.visible_total_rows = total_rows;
        self.project_key = Some(key);
        debug_assert!(total_rows >= row_base);
        debug_assert!(row_base.saturating_add(materialized_rows) <= total_rows);
        let clamped_scroll = clamp_scroll(RowIndex::MAX, total_rows, viewport_rows);
        debug_assert!(clamped_scroll >= row_base.saturating_sub(1));
        ProjectOutput {
            clamped_scroll,
            row_base,
            total_rows,
        }
    }

    fn append_projected_block(
        &mut self,
        history: &BlockHistory,
        block_index: usize,
        id: BlockId,
        key: LayoutKey,
        rows: &mut ProjectRows<'_>,
    ) {
        let Some(block_buf) = self.cache.get(id, key) else {
            return;
        };
        let block_rows = block_buf.line_count();
        let gap = history.rendered_block_gap(block_index, block_rows);
        self.document.set_exact_height(
            block_index,
            (gap as usize).saturating_add(block_rows) as RowIndex,
        );
        for _ in 0..gap {
            rows.texts.push(String::new());
        }
        let local_start = rows.texts.len() as RowIndex;
        for r in 0..block_rows {
            let row_idx = rows.texts.len();
            rows.texts
                .push(block_buf.get_line(r).unwrap_or("").to_string());
            let h = block_buf.highlights_at(r);
            let dec = block_buf.decoration_at(r).clone();
            if !h.is_empty() || dec != LineDecoration::default() {
                rows.pending.push(PendingRow {
                    row: row_idx,
                    highlights: h,
                    decoration: dec,
                });
            }
        }
        rows.layout.push(LayoutEntry {
            id,
            start: rows.row_base.saturating_add(local_start),
            rows: block_rows as RowIndex,
            key,
        });
    }

    // ── Incremental streaming helpers ─────────────────────────────────

    /// True when all earlier blocks are unchanged and only the last block's
    /// rendered suffix needs replacement.
    fn can_incremental(&self, new_layout: &[LayoutEntry]) -> bool {
        if self.visible_layout.len() != new_layout.len() || self.visible_layout.is_empty() {
            return false;
        }
        // All blocks except last must be identical (same id, rows, and cache key).
        let all_same_except_last = self
            .visible_layout
            .iter()
            .zip(new_layout.iter())
            .take(self.visible_layout.len().saturating_sub(1))
            .all(|(a, b)| a.id == b.id && a.rows == b.rows && a.key == b.key);
        if !all_same_except_last {
            return false;
        }
        // The last block may have a different key while streaming because its
        // content hash changes. `apply_incremental` replaces the whole last
        // block suffix, so only the stable identity matters here.
        let old_last = self.visible_layout.last().unwrap();
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
        let old_last = match self.visible_layout.last() {
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
        let mut keep_rows = old_last.start.min(usize::MAX as RowIndex) as usize;
        if old_last.rows > 0 {
            let gap =
                history.rendered_block_gap(i, old_last.rows.min(usize::MAX as RowIndex) as usize);
            keep_rows = keep_rows.saturating_sub(gap as usize);
        }
        // Replace the entire suffix in one buffer mutation. Besides being
        // easier to reason about, this lets the window update only the changed
        // suffix of its wrap layout.
        let gap = history.rendered_block_gap(i, block_rows) as usize;
        let mut new_lines: Vec<String> = Vec::with_capacity(gap + block_rows);
        for _ in 0..gap {
            new_lines.push(String::new());
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
    /// stranded in a gap still anchors to a stable block boundary. Tail targets
    /// beyond the end of all blocks return `None` so the caller falls back to
    /// scroll_top and the natural clamp pins the viewport to the new bottom.
    fn block_anchor_at(&self, row: RowIndex) -> Option<(BlockId, RowIndex)> {
        let last = self.visible_layout.last()?;
        let last_end = last.start.saturating_add(last.rows);
        if row >= last_end {
            return None;
        }
        let idx = self.visible_layout.partition_point(|e| e.start <= row);
        if idx == 0 {
            return None;
        }
        let entry = self.visible_layout[idx - 1];
        let end = entry.start.saturating_add(entry.rows);
        let offset = if row < end {
            row - entry.start
        } else {
            entry.rows.saturating_sub(1)
        };
        Some((entry.id, offset))
    }

    /// Estimated tail-visible block ids for pre-rendering side effects before layout.
    pub(crate) fn tail_block_ids(
        &mut self,
        history: &BlockHistory,
        width: u16,
        show_thinking: bool,
        viewport_rows: u16,
    ) -> Vec<BlockId> {
        let base_key = base_layout_key(width, show_thinking);
        self.document
            .rebuild_if_stale(history, width, show_thinking, base_key);
        self.document.tail_block_ids(viewport_rows).collect()
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
        let base_key = base_layout_key(width, show_thinking);
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

    /// Exact full block layout for compatibility APIs. This may materialize every
    /// transcript block, but does not rewrite the backing display buffer.
    pub(crate) fn materialize_block_layout(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> Vec<(BlockId, RowIndex, RowIndex)> {
        self.ensure_all(history, width, show_thinking, theme);
        let base_key = base_layout_key(width, show_thinking);
        self.document
            .rebuild_if_stale(history, width, show_thinking, base_key);

        let mut row: RowIndex = 0;
        let mut layout = Vec::with_capacity(history.order.len());
        for i in 0..history.order.len() {
            let id = history.order[i];
            let bkey = history.resolve_key(id, base_key);
            let Some(block_buf) = self.cache.get(id, bkey) else {
                continue;
            };
            let block_rows = block_buf.line_count();
            let gap = history.rendered_block_gap(i, block_rows) as RowIndex;
            self.document
                .set_exact_height(i, gap.saturating_add(block_rows as RowIndex));
            row = row.saturating_add(gap);
            layout.push(LayoutEntry {
                id,
                start: row,
                rows: block_rows as RowIndex,
                key: bkey,
            });
            row = row.saturating_add(block_rows as RowIndex);
        }
        self.document.refresh_height_index();
        layout.iter().map(|e| (e.id, e.start, e.rows)).collect()
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
        let base_key = base_layout_key(width, show_thinking);
        self.document
            .rebuild_if_stale(history, width, show_thinking, base_key);
        let mut rows: Vec<String> = Vec::new();
        for i in 0..history.order.len() {
            let id = history.order[i];
            let bkey = history.resolve_key(id, base_key);
            let Some(block_buf) = self.cache.get(id, bkey) else {
                continue;
            };
            let block_rows = block_buf.line_count();
            let gap = history.rendered_block_gap(i, block_rows);
            self.document
                .set_exact_height(i, (gap as usize).saturating_add(block_rows) as RowIndex);
            for _ in 0..gap {
                rows.push(String::new());
            }
            for r in 0..block_rows {
                rows.push(block_buf.get_line(r).unwrap_or("").to_string());
            }
        }
        self.document.refresh_height_index();
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
        let base_key = base_layout_key(width, show_thinking);
        self.document
            .rebuild_if_stale(history, width, show_thinking, base_key);

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
            let block_rows = block_buf.line_count();
            let gap = history.rendered_block_gap(i, block_rows);
            self.document
                .set_exact_height(i, (gap as usize).saturating_add(block_rows) as RowIndex);
            for _ in 0..gap {
                push_row(&mut metas, pos, false);
                pos += 1;
            }
            for r in 0..block_rows {
                let line_len = block_buf.get_line(r).unwrap_or("").len();
                pos += line_len;
                let current_soft = block_buf.decoration_at(r).soft_wrapped;
                push_row(&mut metas, pos, current_soft);
                pos += 1;
            }
        }
        self.document.refresh_height_index();

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

fn clamp_scroll(scroll_top: RowIndex, total_rows: RowIndex, viewport_rows: u16) -> RowIndex {
    scroll_top.min(total_rows.saturating_sub(viewport_rows as RowIndex))
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
        projection.project(
            &mut buf,
            history,
            80,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );
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

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );

        assert!(buf.line_count() > 0);
        assert_eq!(buf.get_line(buf.line_count() - 1), Some("hello"));
    }

    #[test]
    fn project_inserts_gap_around_mode_blocks() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "before".into(),
        });
        transcript.push(Block::Mode {
            text: "now in apply mode".into(),
            icon: "● ".into(),
            hl_group: "SmeltModeApply".into(),
        });
        transcript.push(Block::Text {
            content: "after".into(),
        });

        let rows = project_fresh(&mut transcript.history);
        let lines: Vec<&str> = rows.iter().map(|row| row.line.as_str()).collect();
        assert_eq!(
            lines,
            vec!["before", "", "● now in apply mode", "", "after"]
        );
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
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );

        parser.append_streaming_text(&mut transcript.history, " 1 |");
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );

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
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );
        let before = snapshot(&buf);

        parser.append_active_output(&mut transcript.history, "call-1", "done");
        parser.set_active_status(
            &mut transcript.history,
            "call-1",
            ToolStatus::Ok,
            std::time::Instant::now(),
        );
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );

        let incremental = snapshot(&buf);
        let full = project_fresh(&mut transcript.history);
        assert_ne!(incremental, before);
        assert_eq!(incremental, full);
    }

    #[test]
    fn tail_projection_renders_bounded_suffix_then_scroll_materializes_full_buffer() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(4), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Tail,
            5,
        );
        let tail_lines: Vec<String> = buf.lines().to_vec();
        assert!(tail_lines.iter().any(|line| line == "line 99"));
        assert!(!tail_lines.iter().any(|line| line == "line 0"));

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Row(0),
            5,
        );
        let full_lines: Vec<String> = buf.lines().to_vec();
        assert!(full_lines.iter().any(|line| line == "line 0"));
        assert!(full_lines.iter().any(|line| line == "line 99"));
    }

    #[test]
    fn tail_projection_uses_measured_prefix_heights() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            transcript.push(Block::Text {
                content: format!("block {i}\ncontinued {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(5), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Row(0),
            5,
        );
        let full_rows = buf.line_count() as RowIndex;

        let output = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Tail,
            5,
        );

        assert!(buf.line_count() as RowIndex <= full_rows);
        assert!(buf.line_count() as RowIndex >= 5);
        assert!(output.row_base > 0);
        assert_eq!(output.total_rows, full_rows);
        assert_eq!(output.clamped_scroll, full_rows.saturating_sub(5));
        assert!(buf.lines().iter().any(|line| line == "block 39"));
        assert!(!buf.lines().iter().any(|line| line == "block 0"));
    }

    #[test]
    fn materialized_block_layout_is_exact_after_tail_projection() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(6), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Tail,
            5,
        );
        let visible_count = projection.visible_block_layout().count();
        assert!(visible_count < transcript.history.order.len());

        let layout =
            projection.materialize_block_layout(&mut transcript.history, 80, false, &theme);
        assert_eq!(layout.len(), transcript.history.order.len());
        assert_eq!(layout.first().map(|(_, start, _)| *start), Some(0));
        assert_eq!(layout.last().map(|(_, _, rows)| *rows), Some(1));
        assert_eq!(projection.visible_block_layout().count(), visible_count);
    }

    #[test]
    fn full_projection_remaps_scroll_when_leaving_estimated_tail() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            let lines = (0..10)
                .map(|j| format!("block {i} line {j}"))
                .collect::<Vec<_>>()
                .join("\n");
            transcript.push(Block::Text { content: lines });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_term::BufId(7), Default::default());

        let tail = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Tail,
            5,
        );
        assert!(tail.total_rows < 439, "prefix rows start as estimates");

        let full = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Row(tail.clamped_scroll.saturating_sub(1)),
            5,
        );

        assert_eq!(full.total_rows, 439);
        assert!(
            full.clamped_scroll > 300,
            "leaving tail should stay anchored near the resumed tail, not jump to the top"
        );
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
        projection.project(
            &mut buf,
            &mut transcript.history,
            40,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );

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
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );

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
        projection.project(
            &mut buf,
            &mut transcript.history,
            40,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );

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
        projection.project(
            &mut buf,
            &mut transcript.history,
            40,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );

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
        projection.project(
            &mut buf,
            &mut transcript.history,
            24,
            false,
            &theme,
            ScrollTarget::Row(0),
            80,
        );

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
