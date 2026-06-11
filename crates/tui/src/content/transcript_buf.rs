use super::block_buffers::BlockBufferCache;
use crate::smelt_edit::Theme;
use crate::smelt_edit::{
    clamp_scroll, row_to_usize, BufCreateOpts, BufId, Buffer, CopyOutput, DocRange,
    MaterializedRows, RowIndex,
};
use smelt_core::buffer::{LineDecoration, Span, SpanMeta};
use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey, ViewState};
use std::sync::Arc;

const HEAD_OVERSCAN_ROWS: RowIndex = 20;
const TAIL_OVERSCAN_ROWS: RowIndex = 20;

pub(crate) struct TranscriptProjection {
    cache: BlockBufferCache,
    cache_generation: u64,
    cache_width: u16,
    materialized: Option<MaterializedProjection>,
    /// Block layout from the last visible `project()`. Surfaced to Lua via `visible_blocks`.
    visible_layout: Vec<LayoutEntry>,
    /// Absolute row represented by local row 0 in the backing buffer.
    visible_row_base: RowIndex,
    /// Total rows in the logical transcript represented by the visible projection.
    visible_total_rows: RowIndex,
    /// Cached `build_rows` result for full-text consumers (Lua API, vim navigation).
    cached_rows: Option<CachedRows>,
    row_index: BlockRowIndex,
    #[cfg(test)]
    counters: TranscriptProjectionCounters,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TranscriptProjectionCounters {
    pub full_row_builds: usize,
    pub rendered_blocks: usize,
    pub exact_height_measured_blocks: usize,
    pub range_materialized_blocks: usize,
}

struct CachedRows {
    rows: Arc<Vec<String>>,
    generation: u64,
    width: u16,
    show_thinking: bool,
}

#[derive(Default)]
struct BlockRowIndex {
    nodes: Vec<BlockRow>,
    prefix_rows: Vec<RowIndex>,
    generation: u64,
    width: u16,
    show_thinking: bool,
}

struct BlockRow {
    id: BlockId,
    key: LayoutKey,
    estimated_height: RowIndex,
    exact_height: Option<RowIndex>,
}

impl BlockRow {
    fn measured_or_estimated_height(&self) -> RowIndex {
        self.exact_height.unwrap_or(self.estimated_height)
    }
}

impl BlockRowIndex {
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

        let keep_measurements = self.width == width && self.show_thinking == show_thinking;
        let old_nodes = if keep_measurements {
            std::mem::take(&mut self.nodes)
        } else {
            Vec::new()
        };
        self.nodes.clear();
        self.nodes.reserve(history.order.len());
        for (index, &id) in history.order.iter().enumerate() {
            let key = history.resolve_key(id, base_key);
            let old_same_index = old_nodes.get(index).filter(|node| node.id == id);
            let estimated_height = old_same_index
                .map(BlockRow::measured_or_estimated_height)
                .or_else(|| {
                    old_nodes
                        .iter()
                        .find(|node| node.id == id)
                        .map(BlockRow::measured_or_estimated_height)
                })
                .unwrap_or(1);
            let same_previous = index == 0
                || old_nodes
                    .get(index.saturating_sub(1))
                    .zip(self.nodes.get(index.saturating_sub(1)))
                    .is_some_and(|(old, new)| old.id == new.id && old.key == new.key);
            let exact_height = old_same_index
                .filter(|node| node.key == key && same_previous)
                .and_then(|node| node.exact_height);
            self.nodes.push(BlockRow {
                id,
                key,
                estimated_height,
                exact_height,
            });
        }
        self.generation = gen;
        self.width = width;
        self.show_thinking = show_thinking;
        self.rebuild_prefix_rows();
    }

    fn set_exact_height(&mut self, index: usize, rows: RowIndex) -> bool {
        let Some(node) = self.nodes.get_mut(index) else {
            return false;
        };
        if node.exact_height == Some(rows) {
            return false;
        }
        node.exact_height = Some(rows);
        true
    }

    fn is_exact_for(&self, history: &BlockHistory, width: u16, show_thinking: bool) -> bool {
        self.generation == history.generation()
            && self.width == width
            && self.show_thinking == show_thinking
            && self.nodes.len() == history.order.len()
            && self.nodes.iter().all(|node| node.exact_height.is_some())
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

    fn start_index_for_row(&self, row: RowIndex) -> usize {
        let idx = self.prefix_rows.partition_point(|prefix| *prefix <= row);
        idx.saturating_sub(1).min(self.nodes.len())
    }

    fn block_index(&self, id: BlockId) -> Option<usize> {
        self.nodes.iter().position(|node| node.id == id)
    }

    fn end_index_for_row_end(&self, row_end: RowIndex) -> usize {
        self.prefix_rows
            .partition_point(|prefix| *prefix < row_end)
            .min(self.nodes.len())
    }

    fn block_range_for_rows(&self, rows: std::ops::Range<RowIndex>) -> std::ops::Range<usize> {
        if rows.start >= rows.end {
            return 0..0;
        }
        let first = self.start_index_for_row(rows.start);
        let end = self.end_index_for_row_end(rows.end).max(first);
        first..end
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
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct ProjectKey {
    generation: u64,
    width: u16,
    show_thinking: bool,
    mode: ProjectionMode,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum ProjectionMode {
    Visible { viewport_rows: u16 },
}

#[derive(Clone, Copy)]
struct MaterializedProjection {
    key: ProjectKey,
    buf_id: BufId,
    changedtick: u64,
}

pub(crate) struct ProjectionPlan {
    key: ProjectKey,
    scroll_target: ScrollTarget,
    scroll_top: RowIndex,
    viewport_rows: u16,
    first: usize,
    block_ids: Vec<BlockId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollAnchor {
    Row(RowIndex),
    Tail,
}

impl ScrollAnchor {
    fn as_scroll_top(self) -> RowIndex {
        match self {
            Self::Row(row) => row,
            Self::Tail => RowIndex::MAX,
        }
    }

    fn row(self) -> Option<RowIndex> {
        match self {
            Self::Row(row) => Some(row),
            Self::Tail => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollTarget {
    /// Materialize only the visible window around the anchor.
    Visible(ScrollAnchor),
}

impl ScrollTarget {
    pub(crate) fn visible_row(row: RowIndex) -> Self {
        Self::Visible(ScrollAnchor::Row(row))
    }

    pub(crate) fn visible_tail() -> Self {
        Self::Visible(ScrollAnchor::Tail)
    }

    fn anchor(self) -> ScrollAnchor {
        match self {
            Self::Visible(anchor) => anchor,
        }
    }

    fn as_scroll_top(self) -> RowIndex {
        self.anchor().as_scroll_top()
    }

    fn mode(self, viewport_rows: u16) -> ProjectionMode {
        match self {
            Self::Visible(_) => ProjectionMode::Visible { viewport_rows },
        }
    }

    fn visible_row_anchor(self) -> Option<RowIndex> {
        match self {
            Self::Visible(anchor) => anchor.row(),
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

struct MaterializedTranscriptRange {
    row_base: RowIndex,
    total_rows: RowIndex,
    texts: Vec<String>,
    pending: Vec<PendingRow>,
    layout: Vec<LayoutEntry>,
}

pub(crate) struct TranscriptRangeRows {
    pub rows: Vec<String>,
    pub soft_breaks: Vec<usize>,
    pub hard_breaks: Vec<usize>,
}

impl TranscriptRangeRows {
    fn empty() -> Self {
        Self {
            rows: Vec::new(),
            soft_breaks: Vec::new(),
            hard_breaks: Vec::new(),
        }
    }
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
            materialized: None,
            visible_layout: Vec::new(),
            visible_row_base: 0,
            visible_total_rows: 0,
            cached_rows: None,
            row_index: BlockRowIndex::default(),
            #[cfg(test)]
            counters: TranscriptProjectionCounters::default(),
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

    #[cfg(test)]
    pub(crate) fn counters(&self) -> TranscriptProjectionCounters {
        self.counters
    }

    #[cfg(test)]
    pub(crate) fn reset_counters(&mut self) {
        self.counters = TranscriptProjectionCounters::default();
    }

    fn ensure_blocks(
        &mut self,
        history: &BlockHistory,
        ids: &[BlockId],
        keys: &[LayoutKey],
        theme: &Theme,
    ) {
        let rendered = self.cache.ensure_many(history, ids, keys, theme);
        #[cfg(test)]
        {
            self.counters.rendered_blocks += rendered;
        }
        let _ = rendered;
    }

    fn clear_materialized_state(&mut self) {
        self.materialized = None;
        self.visible_layout.clear();
        self.visible_row_base = 0;
        self.visible_total_rows = 0;
        self.cached_rows = None;
        self.row_index = BlockRowIndex::default();
    }

    fn gc_if_stale(&mut self, gen: u64, width: u16) {
        if width != self.cache_width {
            // Width change invalidates all layouts (wrapping changes).
            self.cache.clear();
            self.cache_width = width;
            self.clear_materialized_state();
        }
        self.cache_generation = gen;
    }

    /// Clear every cached layout so the next `project()` rebuilds from scratch.
    /// Called when the theme changes - colors that were baked into anonymous
    /// highlight groups need to be re-resolved against the new palette.
    pub(crate) fn invalidate_theme(&mut self) {
        self.cache.clear();
        self.clear_materialized_state();
    }

    fn target_has_projection(&self, key: ProjectKey, buf: &Buffer) -> bool {
        self.materialized.is_some_and(|m| {
            m.key == key && m.buf_id == buf.id() && m.changedtick == buf.changedtick()
        })
    }

    fn last_project_key(&self) -> Option<ProjectKey> {
        self.materialized.map(|m| m.key)
    }

    fn mark_projected_into(&mut self, key: ProjectKey, buf: &Buffer) {
        self.materialized = Some(MaterializedProjection {
            key,
            buf_id: buf.id(),
            changedtick: buf.changedtick(),
        });
    }

    pub(crate) fn measure_all_heights(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) {
        let gen = history.generation();
        self.gc_if_stale(gen, width);
        let base_key = base_layout_key(width, show_thinking);
        self.row_index
            .rebuild_if_stale(history, width, show_thinking, base_key);
        if self.row_index.is_exact_for(history, width, show_thinking) {
            return;
        }

        let mut missing_ids = Vec::new();
        let mut missing_keys = Vec::new();
        for node in self
            .row_index
            .nodes
            .iter()
            .filter(|node| node.exact_height.is_none())
        {
            missing_ids.push(node.id);
            missing_keys.push(node.key);
        }
        self.ensure_blocks(history, &missing_ids, &missing_keys, theme);
        for i in 0..history.order.len() {
            if self
                .row_index
                .nodes
                .get(i)
                .is_some_and(|node| node.exact_height.is_some())
            {
                continue;
            }
            let id = history.order[i];
            let key = history.resolve_key(id, base_key);
            let Some(block_buf) = self.cache.get(id, key) else {
                continue;
            };
            let block_rows = block_buf.line_count();
            let gap = history.rendered_block_gap(i, block_rows) as RowIndex;
            let _measured = self
                .row_index
                .set_exact_height(i, gap.saturating_add(block_rows as RowIndex));
            #[cfg(test)]
            if _measured {
                self.counters.exact_height_measured_blocks += 1;
            }
        }
        self.row_index.refresh_height_index();
    }

    fn exact_block_layout(&self, history: &BlockHistory) -> Vec<LayoutEntry> {
        let mut layout = Vec::with_capacity(self.row_index.nodes.len());
        for (i, node) in self.row_index.nodes.iter().enumerate() {
            debug_assert!(
                node.exact_height.is_some(),
                "exact block layout requested before height measurement"
            );
            let Some(exact_height) = node.exact_height else {
                continue;
            };
            let gap = if exact_height == 0 {
                0
            } else {
                (history.block_gap(i) as RowIndex).min(exact_height)
            };
            layout.push(LayoutEntry {
                id: node.id,
                start: self.row_index.prefix_row(i).saturating_add(gap),
                rows: exact_height.saturating_sub(gap),
            });
        }
        layout
    }

    pub(crate) fn exact_total_rows(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> RowIndex {
        self.measure_all_heights(history, width, show_thinking, theme);
        self.row_index.total_rows()
    }

    fn resize_anchor_for(
        &self,
        width: u16,
        scroll_target: ScrollTarget,
    ) -> Option<(BlockId, RowIndex)> {
        let row = scroll_target.visible_row_anchor()?;
        let width_changed = self
            .last_project_key()
            .map(|prev| prev.width != width)
            .unwrap_or(false);
        width_changed.then(|| self.block_anchor_at(row)).flatten()
    }

    pub(crate) fn plan_projection_measured(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
        theme: &Theme,
    ) -> ProjectionPlan {
        let resize_anchor = self.resize_anchor_for(width, scroll_target);
        self.measure_all_heights(history, width, show_thinking, theme);
        self.plan_projection_from_prepared(
            history,
            width,
            show_thinking,
            scroll_target,
            viewport_rows,
            resize_anchor,
        )
    }

    fn scroll_top_for_resize_anchor(
        &self,
        history: &BlockHistory,
        anchor: Option<(BlockId, RowIndex)>,
    ) -> Option<RowIndex> {
        let (id, offset) = anchor?;
        let index = self.row_index.block_index(id)?;
        let key = self.row_index.nodes.get(index)?.key;
        let block_rows = self.cache.get(id, key)?.line_count();
        let gap = history.rendered_block_gap(index, block_rows) as RowIndex;
        Some(
            self.row_index
                .prefix_row(index)
                .saturating_add(gap)
                .saturating_add(offset),
        )
    }

    fn plan_projection_from_prepared(
        &self,
        history: &BlockHistory,
        width: u16,
        show_thinking: bool,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
        resize_anchor: Option<(BlockId, RowIndex)>,
    ) -> ProjectionPlan {
        let key = ProjectKey {
            generation: history.generation(),
            width,
            show_thinking,
            mode: scroll_target.mode(viewport_rows),
        };
        let total_rows = self.row_index.total_rows();
        let requested_scroll_top = self
            .scroll_top_for_resize_anchor(history, resize_anchor)
            .unwrap_or_else(|| scroll_target.as_scroll_top());
        let scroll_top = clamp_scroll(requested_scroll_top, total_rows, viewport_rows);
        let viewport_end = scroll_top
            .saturating_add(viewport_rows as RowIndex)
            .min(total_rows);
        let row_window = match scroll_target {
            ScrollTarget::Visible(ScrollAnchor::Row(_)) => {
                let start = scroll_top.saturating_sub(HEAD_OVERSCAN_ROWS);
                let end = viewport_end
                    .saturating_add(TAIL_OVERSCAN_ROWS)
                    .min(total_rows);
                start..end
            }
            ScrollTarget::Visible(ScrollAnchor::Tail) => {
                let start = scroll_top.saturating_sub(HEAD_OVERSCAN_ROWS);
                start..total_rows
            }
        };
        let block_range = self.row_index.block_range_for_rows(row_window);
        let first = block_range.start;
        let end = block_range.end;
        let block_ids = self.row_index.nodes[first..end]
            .iter()
            .map(|node| node.id)
            .collect();
        ProjectionPlan {
            key,
            scroll_target,
            scroll_top,
            viewport_rows,
            first,
            block_ids,
        }
    }

    /// Render a bounded virtual window into `buf`.
    #[cfg(test)]
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
    ) -> MaterializedRows {
        let plan = self.plan_projection_measured(
            history,
            width,
            show_thinking,
            scroll_target,
            viewport_rows,
            theme,
        );
        self.project_planned(buf, history, theme, plan)
    }

    pub(crate) fn project_planned(
        &mut self,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        theme: &Theme,
        plan: ProjectionPlan,
    ) -> MaterializedRows {
        let row = plan.scroll_top;
        if let Some(out) = self.reuse_visible_projection_for_row(
            buf,
            plan.key.generation,
            plan.key.width,
            plan.key.show_thinking,
            row,
            plan.viewport_rows,
        ) {
            return out;
        }

        match plan.scroll_target {
            ScrollTarget::Visible(_) => {
                let out = self.project_visible_range(buf, history, theme, &plan);
                debug_assert_materialized_viewport(out, plan.viewport_rows);
                out
            }
        }
    }

    fn reuse_visible_projection_for_row(
        &self,
        buf: &Buffer,
        gen: u64,
        width: u16,
        show_thinking: bool,
        row: RowIndex,
        viewport_rows: u16,
    ) -> Option<MaterializedRows> {
        let prev = self.last_project_key()?;
        if prev.generation != gen || prev.width != width || prev.show_thinking != show_thinking {
            return None;
        }

        if !self.target_has_projection(prev, buf) {
            return None;
        }

        let total_rows = self.visible_total_rows;
        let clamped_scroll = clamp_scroll(row, total_rows, viewport_rows);
        let materialized_end = self
            .visible_row_base
            .saturating_add(buf.line_count() as RowIndex);
        let viewport_end = clamped_scroll.saturating_add(viewport_rows as RowIndex);
        if clamped_scroll >= self.visible_row_base && viewport_end <= materialized_end {
            return Some(MaterializedRows {
                clamped_scroll,
                row_base: self.visible_row_base,
                total_rows,
                materialized_rows: buf.line_count() as RowIndex,
            });
        }
        None
    }

    fn project_visible_range(
        &mut self,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        theme: &Theme,
        plan: &ProjectionPlan,
    ) -> MaterializedRows {
        let block_range = plan.first..plan.first.saturating_add(plan.block_ids.len());
        let materialized = self.collect_blocks_range(history, theme, block_range);
        let row_base = materialized.row_base;
        let total_rows = materialized.total_rows;
        let materialized_rows = materialized.texts.len() as RowIndex;
        buf.set_all_lines(materialized.texts);
        for p in materialized.pending {
            apply_row_highlights(buf, p.row, p.highlights);
            if p.decoration != LineDecoration::default() {
                buf.set_decoration(p.row, p.decoration);
            }
        }
        self.visible_layout = materialized.layout;
        self.visible_row_base = row_base;
        self.visible_total_rows = total_rows;
        self.mark_projected_into(plan.key, buf);
        debug_assert!(total_rows >= row_base);
        debug_assert!(row_base.saturating_add(materialized_rows) <= total_rows);
        let clamped_scroll = clamp_scroll(plan.scroll_top, total_rows, plan.viewport_rows);
        MaterializedRows {
            clamped_scroll,
            row_base,
            total_rows,
            materialized_rows,
        }
    }

    fn collect_blocks_range(
        &mut self,
        history: &BlockHistory,
        theme: &Theme,
        block_range: std::ops::Range<usize>,
    ) -> MaterializedTranscriptRange {
        let start = block_range.start.min(self.row_index.nodes.len());
        let end = block_range.end.min(self.row_index.nodes.len());
        #[cfg(test)]
        {
            self.counters.range_materialized_blocks += end.saturating_sub(start);
        }
        let row_base = self.row_index.prefix_row(start);
        let mut texts = Vec::new();
        let mut pending = Vec::new();
        let mut layout = Vec::with_capacity(end.saturating_sub(start));
        let mut rows = ProjectRows {
            row_base,
            texts: &mut texts,
            pending: &mut pending,
            layout: &mut layout,
        };

        for block_index in start..end {
            let id = self.row_index.nodes[block_index].id;
            let key = self.row_index.nodes[block_index].key;
            self.ensure_blocks(history, &[id], &[key], theme);
            self.append_projected_block(history, block_index, id, key, &mut rows);
        }

        self.row_index.refresh_height_index();
        MaterializedTranscriptRange {
            row_base,
            total_rows: self.row_index.total_rows(),
            texts,
            pending,
            layout,
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
        let _measured = self.row_index.set_exact_height(
            block_index,
            (gap as usize).saturating_add(block_rows) as RowIndex,
        );
        #[cfg(test)]
        if _measured {
            self.counters.exact_height_measured_blocks += 1;
        }
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
        });
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
        self.ensure_blocks(history, &ids, &keys, theme);
    }

    /// Exact full block layout for compatibility APIs. This may measure every
    /// transcript block, but it does not concatenate display rows and does not
    /// re-render blocks when the exact height index is already current.
    pub(crate) fn materialize_block_layout(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> Vec<(BlockId, RowIndex, RowIndex)> {
        self.measure_all_heights(history, width, show_thinking, theme);
        self.exact_block_layout(history)
            .into_iter()
            .map(|e| (e.id, e.start, e.rows))
            .collect()
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
        #[cfg(test)]
        {
            self.counters.full_row_builds += 1;
        }
        self.ensure_all(history, width, show_thinking, theme);
        let base_key = base_layout_key(width, show_thinking);
        self.row_index
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
            let _measured = self
                .row_index
                .set_exact_height(i, (gap as usize).saturating_add(block_rows) as RowIndex);
            #[cfg(test)]
            if _measured {
                self.counters.exact_height_measured_blocks += 1;
            }
            for _ in 0..gap {
                rows.push(String::new());
            }
            for r in 0..block_rows {
                rows.push(block_buf.get_line(r).unwrap_or("").to_string());
            }
        }
        self.row_index.refresh_height_index();
        let rows = Arc::new(rows);
        self.cached_rows = Some(CachedRows {
            rows: Arc::clone(&rows),
            generation: gen,
            width,
            show_thinking,
        });
        rows
    }

    pub(crate) fn rows_for_range(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
        start: RowIndex,
        count: RowIndex,
    ) -> TranscriptRangeRows {
        let end = start.saturating_add(count);
        if count == 0 || end <= start {
            return TranscriptRangeRows::empty();
        }

        self.measure_all_heights(history, width, show_thinking, theme);
        let total_rows = self.row_index.total_rows();
        if total_rows == 0 || start >= total_rows {
            return TranscriptRangeRows::empty();
        }
        let end = end.min(total_rows);
        let block_range = self.row_index.block_range_for_rows(start..end);
        if block_range.start >= block_range.end {
            return TranscriptRangeRows::empty();
        }

        let materialized = self.collect_blocks_range(history, theme, block_range);
        let local_start = row_to_usize(start.saturating_sub(materialized.row_base));
        let local_end =
            row_to_usize(end.saturating_sub(materialized.row_base)).min(materialized.texts.len());
        if local_start >= local_end {
            return TranscriptRangeRows::empty();
        }
        let mut soft_wrapped = vec![false; materialized.texts.len()];
        for p in materialized.pending {
            if let Some(slot) = soft_wrapped.get_mut(p.row) {
                *slot = p.decoration.soft_wrapped;
            }
        }
        let rows = materialized.texts[local_start..local_end].to_vec();
        let soft_wrapped = soft_wrapped[local_start..local_end].to_vec();
        let (soft_breaks, hard_breaks) = breaks_for_materialized_rows(&rows, &soft_wrapped);
        TranscriptRangeRows {
            rows,
            soft_breaks,
            hard_breaks,
        }
    }

    pub(crate) fn copy_range(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
        range: DocRange,
    ) -> CopyOutput {
        if (range.start.row, range.start.byte_col) >= (range.end.row, range.end.byte_col) {
            return CopyOutput::default();
        }
        self.measure_all_heights(history, width, show_thinking, theme);
        let total_rows = self.row_index.total_rows();
        if total_rows == 0 || range.start.row >= total_rows {
            return CopyOutput::default();
        }
        let end_row = range.end.row.min(total_rows.saturating_sub(1));
        let block_range = self
            .row_index
            .block_range_for_rows(range.start.row..end_row.saturating_add(1));
        if block_range.start >= block_range.end {
            return CopyOutput::default();
        }

        let mut scratch = Buffer::new(BufId(0), BufCreateOpts::default());
        let materialized = self.collect_blocks_range(history, theme, block_range);
        let row_base = materialized.row_base;
        scratch.set_all_lines(materialized.texts);
        for p in materialized.pending {
            apply_row_highlights(&mut scratch, p.row, p.highlights);
            if p.decoration != LineDecoration::default() {
                scratch.set_decoration(p.row, p.decoration);
            }
        }

        let start_local = range.start.row.saturating_sub(row_base);
        let end_local = range.end.row.saturating_sub(row_base);
        let start = scratch.byte_at_display_pos(row_to_usize(start_local), range.start.byte_col);
        let end = scratch.byte_at_display_pos(row_to_usize(end_local), range.end.byte_col);
        let clipboard = copy_byte_range(&scratch, start, end);
        let raw = smelt_buffer::text::slice(&scratch.text(), start..end).to_string();
        CopyOutput {
            kill_ring: raw,
            clipboard,
        }
    }

    pub(crate) fn line_breaks(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> (Vec<usize>, Vec<usize>) {
        self.measure_all_heights(history, width, show_thinking, theme);
        if self.row_index.nodes.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let materialized = self.collect_blocks_range(history, theme, 0..self.row_index.nodes.len());
        let mut soft_wrapped = vec![false; materialized.texts.len()];
        for p in materialized.pending {
            if let Some(slot) = soft_wrapped.get_mut(p.row) {
                *slot = p.decoration.soft_wrapped;
            }
        }
        breaks_for_materialized_rows(&materialized.texts, &soft_wrapped)
    }
}

fn breaks_for_materialized_rows(
    rows: &[String],
    soft_wrapped: &[bool],
) -> (Vec<usize>, Vec<usize>) {
    breaks_for_row_lengths(rows.iter().map(|row| row.len()), soft_wrapped)
}

fn breaks_for_row_lengths(
    row_lengths: impl IntoIterator<Item = usize>,
    soft_wrapped: &[bool],
) -> (Vec<usize>, Vec<usize>) {
    let mut soft = Vec::new();
    let mut hard = Vec::new();
    let mut pos = 0usize;
    let mut rows = row_lengths.into_iter().peekable();
    let mut i = 0usize;
    while let Some(row_len) = rows.next() {
        if rows.peek().is_none() {
            break;
        }
        pos += row_len;
        if soft_wrapped.get(i + 1).copied().unwrap_or(false) {
            soft.push(pos);
        } else {
            hard.push(pos);
        }
        pos += 1;
        i += 1;
    }
    (soft, hard)
}

fn debug_assert_materialized_viewport(rows: MaterializedRows, viewport_rows: u16) {
    let viewport_end = rows
        .clamped_scroll
        .saturating_add(viewport_rows as RowIndex)
        .min(rows.total_rows);
    debug_assert!(
        rows.clamped_scroll >= rows.row_base,
        "materialized range starts at {}, after viewport start {}",
        rows.row_base,
        rows.clamped_scroll
    );
    debug_assert!(
        viewport_end <= rows.row_base.saturating_add(rows.materialized_rows),
        "materialized range {}..{} does not cover viewport {}..{}",
        rows.row_base,
        rows.row_base.saturating_add(rows.materialized_rows),
        rows.clamped_scroll,
        viewport_end
    );
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(99), Default::default());
        projection.project(
            &mut buf,
            history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(0),
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(0),
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
    fn user_chrome_uses_background_fill_not_fake_blank_text() {
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "hello".into(),
            image_labels: vec![],
        });

        let rows = project_fresh(&mut transcript.history);

        assert_eq!(rows.first().map(|row| row.line.as_str()), Some(""));
        assert_eq!(rows.last().map(|row| row.line.as_str()), Some(""));
        assert!(rows
            .first()
            .is_some_and(|row| row.decoration.fill_bg.is_some()));
        assert!(rows
            .last()
            .is_some_and(|row| row.decoration.fill_bg.is_some()));
        assert!(rows.first().is_some_and(|row| row.highlights.is_empty()));
        assert!(rows.last().is_some_and(|row| row.highlights.is_empty()));
        assert!(rows.iter().any(|row| row.line == " hello"));
        assert!(!rows.iter().any(|row| row.line.len() >= 80));
    }

    #[test]
    fn visible_projection_matches_fresh_after_markdown_table_growth() {
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(2), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        parser.append_streaming_text(&mut transcript.history, " 1 |");
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        let projected = snapshot(&buf);
        let fresh = project_fresh(&mut transcript.history);
        assert_eq!(projected, fresh);
    }

    #[test]
    fn visible_projection_rerenders_tool_state_changes() {
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(3), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(0),
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
            ScrollTarget::visible_row(0),
            80,
        );

        let projected = snapshot(&buf);
        let fresh = project_fresh(&mut transcript.history);
        assert_ne!(projected, before);
        assert_eq!(projected, fresh);
    }

    #[test]
    fn build_rows_materializes_full_transcript() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&mut transcript.history, 80, false, &theme);

        assert!(rows.iter().any(|line| line == "line 99"));
        assert!(rows.iter().any(|line| line == "line 0"));
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 1);
        assert_eq!(counters.rendered_blocks, 100);
        assert_eq!(counters.exact_height_measured_blocks, 100);

        projection.reset_counters();
        let cached = projection.build_rows(&mut transcript.history, 80, false, &theme);
        assert_eq!(cached.len(), rows.len());
        assert_eq!(
            projection.counters(),
            TranscriptProjectionCounters::default()
        );
    }

    #[test]
    fn exact_total_rows_measures_without_building_full_rows() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let total = projection.exact_total_rows(&mut transcript.history, 80, false, &theme);

        assert_eq!(total, 199);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.rendered_blocks, 100);
        assert_eq!(counters.exact_height_measured_blocks, 100);

        projection.reset_counters();
        assert_eq!(
            projection.exact_total_rows(&mut transcript.history, 80, false, &theme),
            total
        );
        assert_eq!(
            projection.counters(),
            TranscriptProjectionCounters::default(),
            "repeated exact row-count queries should use the exact height index"
        );
    }

    #[test]
    fn range_rows_reuse_exact_height_index_without_full_rows() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        assert_eq!(
            projection.exact_total_rows(&mut transcript.history, 80, false, &theme),
            199
        );

        projection.reset_counters();
        let rows = projection.rows_for_range(&mut transcript.history, 80, false, &theme, 150, 3);

        assert_eq!(rows.rows, vec!["line 75", "", "line 76"]);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.rendered_blocks, 0);
        assert_eq!(counters.exact_height_measured_blocks, 0);
        assert!(
            counters.range_materialized_blocks < transcript.history.order.len(),
            "range rows should materialize only intersecting blocks, got {counters:?}"
        );
    }

    #[test]
    fn copy_range_reuses_exact_height_index_without_full_rows() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        assert_eq!(
            projection.exact_total_rows(&mut transcript.history, 80, false, &theme),
            199
        );

        projection.reset_counters();
        let copied = projection.copy_range(
            &mut transcript.history,
            80,
            false,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: 150,
                    byte_col: 0,
                },
                end: crate::smelt_edit::DocPosition {
                    row: 150,
                    byte_col: "line 75".len(),
                },
            },
        );

        assert_eq!(copied.clipboard, "line 75");
        assert_eq!(copied.kill_ring, "line 75");
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.rendered_blocks, 0);
        assert_eq!(counters.exact_height_measured_blocks, 0);
        assert!(
            counters.range_materialized_blocks < transcript.history.order.len(),
            "copy should materialize only intersecting blocks, got {counters:?}"
        );
    }

    #[test]
    fn visible_tail_projection_materializes_bounded_tail_window() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(12), Default::default());

        let output = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        assert!(output.row_base > 0);
        assert!(output.materialized_rows < output.total_rows);
        assert_eq!(output.materialized_rows, buf.line_count() as RowIndex);
        assert_eq!(output.clamped_scroll, output.total_rows.saturating_sub(5));
        assert!(buf.lines().iter().any(|line| line == "line 99"));
        assert!(!buf.lines().iter().any(|line| line == "line 0"));
        assert!(
            projection.counters().range_materialized_blocks < transcript.history.order.len(),
            "tail projection should materialize a bounded block range"
        );
    }

    #[test]
    fn range_rows_match_full_projection_slice() {
        let mut transcript = Transcript::new();
        for i in 0..20 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut full_projection = TranscriptProjection::new();
        let mut full_buf = Buffer::new(crate::smelt_edit::BufId(21), Default::default());
        full_projection.project(
            &mut full_buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(0),
            10,
        );
        let expected = full_buf.lines()[5..12].to_vec();

        let mut range_projection = TranscriptProjection::new();
        let range =
            range_projection.rows_for_range(&mut transcript.history, 80, false, &theme, 5, 7);

        assert_eq!(range.rows, expected);
    }

    #[test]
    fn range_breaks_match_full_projection_for_wrapped_rows() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "one two three four five six seven eight nine ten".into(),
        });
        transcript.push(Block::Text {
            content: "after".into(),
        });
        let theme = Theme::default();
        let mut full_projection = TranscriptProjection::new();
        let mut full_buf = Buffer::new(crate::smelt_edit::BufId(22), Default::default());
        full_projection.project(
            &mut full_buf,
            &mut transcript.history,
            18,
            false,
            &theme,
            ScrollTarget::visible_row(0),
            20,
        );
        let (soft, hard) = full_projection.line_breaks(&mut transcript.history, 18, false, &theme);
        assert!(!soft.is_empty(), "fixture should produce soft wraps");

        let mut range_projection = TranscriptProjection::new();
        let range = range_projection.rows_for_range(
            &mut transcript.history,
            18,
            false,
            &theme,
            0,
            full_buf.line_count() as RowIndex,
        );

        assert_eq!(range.rows, full_buf.lines().to_vec());
        assert_eq!(range.soft_breaks, soft);
        assert_eq!(range.hard_breaks, hard);
    }

    #[test]
    fn visible_tail_projection_uses_measured_prefix_heights() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            transcript.push(Block::Text {
                content: format!("block {i}\ncontinued {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let full_rows = projection
            .build_rows(&mut transcript.history, 80, false, &theme)
            .len() as RowIndex;
        let mut buf = Buffer::new(crate::smelt_edit::BufId(5), Default::default());

        let output = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        assert!(output.row_base > 0);
        assert!(output.materialized_rows < full_rows);
        assert_eq!(output.materialized_rows, buf.line_count() as RowIndex);
        assert_eq!(output.total_rows, full_rows);
        assert_eq!(output.clamped_scroll, full_rows.saturating_sub(5));
        assert!(buf.lines().iter().any(|line| line == "block 39"));
        assert!(!buf.lines().iter().any(|line| line == "block 0"));
    }

    #[test]
    fn cold_tail_projection_uses_exact_total_rows_before_fast_scroll() {
        let mut transcript = Transcript::new();
        for i in 0..120 {
            let content = (0..5)
                .map(|j| format!("block {i} line {j}"))
                .collect::<Vec<_>>()
                .join("\n");
            transcript.push(Block::Text { content });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(14), Default::default());
        let expected_total = 120 * 5 + 119;

        let tail = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            10,
        );
        assert_eq!(tail.total_rows, expected_total);
        assert_eq!(tail.clamped_scroll, expected_total.saturating_sub(10));

        let target = tail.clamped_scroll.saturating_sub(220);
        assert!(target > 0);
        let scrolled = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(target),
            10,
        );

        assert_eq!(scrolled.clamped_scroll, target);
        assert!(scrolled.row_base > 0);
        assert!(buf.lines().iter().any(|line| line == "block 82 line 0"));
        assert!(!buf.lines().iter().any(|line| line == "block 0 line 0"));
    }

    #[test]
    fn visible_projection_materializes_tall_output_block_when_scrolled_inside_it() {
        let mut transcript = Transcript::new();
        for i in 0..5 {
            transcript.push(Block::Text {
                content: format!("before {i}"),
            });
        }
        transcript.push(Block::Text {
            content: (0..80)
                .map(|i| format!("tool output line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        for i in 0..20 {
            transcript.push(Block::Text {
                content: format!("after {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(15), Default::default());

        let scroll_top = 10 + 50;
        let output = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(scroll_top),
            8,
        );

        assert_eq!(output.clamped_scroll, scroll_top);
        assert!(output.row_base <= scroll_top);
        assert!(output.row_base.saturating_add(output.materialized_rows) >= scroll_top + 8);
        assert!(buf.lines().iter().any(|line| line == "tool output line 50"));
        assert!(!buf.lines().iter().any(|line| line == "after 0"));
    }

    #[test]
    fn visible_projection_covers_viewport_crossing_tall_block_boundary() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: (0..80)
                .map(|i| format!("tool output line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        transcript.push(Block::Text {
            content: (0..10)
                .map(|i| format!("after boundary line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        let after_id = *transcript.history.order.last().expect("after block id");
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(16), Default::default());
        let after_start = projection
            .materialize_block_layout(&mut transcript.history, 80, false, &theme)
            .into_iter()
            .find(|(id, _, _)| *id == after_id)
            .map(|(_, start, _)| start)
            .expect("after block layout");
        let scroll_top = after_start.saturating_sub(2);
        let viewport_rows = 5;

        let output = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(scroll_top),
            viewport_rows,
        );

        assert_eq!(output.clamped_scroll, scroll_top);
        assert!(output.row_base <= scroll_top);
        assert!(
            output.row_base.saturating_add(output.materialized_rows)
                >= scroll_top + viewport_rows as RowIndex
        );
        assert!(buf
            .lines()
            .iter()
            .any(|line| line == "after boundary line 0"));
    }

    #[test]
    fn visible_projection_keeps_pinned_row_stable_while_tail_streams() {
        let mut transcript = Transcript::new();
        for i in 0..30 {
            let content = (0..5)
                .map(|j| format!("block {i} line {j}"))
                .collect::<Vec<_>>()
                .join("\n");
            transcript.push(Block::Text { content });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(13), Default::default());
        let anchor_id = transcript.history.order[10];
        let anchor_row = projection
            .materialize_block_layout(&mut transcript.history, 80, false, &theme)
            .into_iter()
            .find(|(id, _, _)| *id == anchor_id)
            .map(|(_, start, _)| start)
            .expect("anchor block layout");

        let before = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            5,
        );
        assert!(buf.lines().iter().any(|line| line == "block 10 line 0"));

        let tail_id = *transcript.history.order.last().expect("tail block");
        transcript.history.rewrite(
            tail_id,
            Block::Text {
                content: format!("{}\nstreamed tail line", "tail\n".repeat(20)),
            },
        );
        let after = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            5,
        );

        assert_eq!(after.clamped_scroll, before.clamped_scroll);
        assert_eq!(after.row_base, before.row_base);
        assert!(buf.lines().iter().any(|line| line == "block 10 line 0"));
    }

    #[test]
    fn cached_tail_projection_rewrites_mutated_target_buffer() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(8), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        buf.set_all_lines(vec!["other session".into()]);
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        assert!(buf.lines().iter().any(|line| line == "line 39"));
        assert!(!buf.lines().iter().any(|line| line == "other session"));
    }

    #[test]
    fn cached_tail_projection_rewrites_new_target_buffer() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut first_buf = Buffer::new(crate::smelt_edit::BufId(8), Default::default());
        projection.project(
            &mut first_buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        let mut second_buf = Buffer::new(crate::smelt_edit::BufId(9), Default::default());
        second_buf.set_all_lines(vec!["other session".into()]);
        projection.project(
            &mut second_buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        assert!(second_buf.lines().iter().any(|line| line == "line 39"));
        assert!(!second_buf
            .lines()
            .iter()
            .any(|line| line == "other session"));
    }

    #[test]
    fn cached_projection_can_be_reused_after_shared_preview_buffer_switches_sessions() {
        let mut first = Transcript::new();
        for i in 0..40 {
            first.push(Block::Text {
                content: format!("first {i}"),
            });
        }
        let mut second = Transcript::new();
        for i in 0..40 {
            second.push(Block::Text {
                content: format!("second {i}"),
            });
        }
        let theme = Theme::default();
        let mut first_projection = TranscriptProjection::new();
        let mut second_projection = TranscriptProjection::new();
        let mut shared = Buffer::new(crate::smelt_edit::BufId(11), Default::default());

        first_projection.project(
            &mut shared,
            &mut first.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );
        second_projection.project(
            &mut shared,
            &mut second.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        first_projection.project(
            &mut shared,
            &mut first.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        assert!(shared.lines().iter().any(|line| line == "first 39"));
        assert!(!shared
            .lines()
            .iter()
            .any(|line| line.starts_with("second ")));
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(6), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
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
    fn visible_tail_uses_exact_total_after_full_compat_materialization() {
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
        let full_rows = projection.build_rows(&mut transcript.history, 80, false, &theme);
        assert_eq!(full_rows.len() as RowIndex, 439);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(7), Default::default());

        let tail = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );
        assert_eq!(tail.total_rows, 439);
        assert!(tail.row_base > 0);
        assert!(tail.materialized_rows < tail.total_rows);
        assert_eq!(tail.materialized_rows, buf.line_count() as RowIndex);
        assert_eq!(tail.clamped_scroll, tail.total_rows.saturating_sub(5));
        assert!(buf.lines().iter().any(|line| line == "block 39 line 9"));
        assert!(!buf.lines().iter().any(|line| line == "block 0 line 0"));

        let top = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(0),
            5,
        );
        assert_eq!(top.row_base, 0);
        assert!(top.materialized_rows < top.total_rows);
        assert!(buf.lines().iter().any(|line| line == "block 0 line 0"));
        assert!(!buf.lines().iter().any(|line| line == "block 39 line 9"));
    }

    #[test]
    fn visible_projection_preserves_block_anchor_across_width_change() {
        let mut transcript = Transcript::new();
        for i in 0..20 {
            transcript.push(Block::Text {
                content: format!("block {i} {}", "wrapped text ".repeat(20)),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(10), Default::default());

        let anchor_id = transcript.history.order[10];
        let anchor_row = projection
            .materialize_block_layout(&mut transcript.history, 80, false, &theme)
            .into_iter()
            .find(|(id, _, _)| *id == anchor_id)
            .map(|(_, start, _)| start)
            .expect("anchor block layout");

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            5,
        );
        assert!(buf.lines().iter().any(|line| line.contains("block 10")));

        projection.project(
            &mut buf,
            &mut transcript.history,
            24,
            false,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            5,
        );
        assert!(buf.lines().iter().any(|line| line.contains("block 10")));
    }

    #[test]
    fn copy_byte_range_basic_text() {
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
        buf.set_all_lines(vec!["hello".into(), "world".into()]);
        assert_eq!(copy_byte_range(&buf, 0, 5), "hello");
        assert_eq!(copy_byte_range(&buf, 0, 11), "hello\nworld");
        assert_eq!(copy_byte_range(&buf, 6, 11), "world");
    }

    fn project_tool_title(name: &str, summary: protocol::StyledLines) -> Buffer {
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        parser.start_tool(
            &mut transcript.history,
            "call-1".into(),
            name.into(),
            summary,
            std::collections::HashMap::new(),
            std::time::Instant::now(),
        );

        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(7), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            false,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        buf
    }

    fn copy_first_row(buf: &Buffer) -> String {
        let end = buf.get_line(0).unwrap_or("").len();
        copy_byte_range(buf, 0, end)
    }

    #[test]
    fn copy_tool_title_includes_pill_name_and_summary() {
        let buf = project_tool_title("bash", protocol::StyledLines::from_plain("ls -la"));

        assert_eq!(copy_first_row(&buf), "\u{23fa} bash ls -la");
    }

    #[test]
    fn copy_tool_title_includes_pill_and_name_when_summary_is_empty() {
        let buf = project_tool_title("bash", protocol::StyledLines::empty());

        assert_eq!(copy_first_row(&buf), "\u{23fa} bash");
    }

    #[test]
    fn copy_tool_title_excludes_non_selectable_suffix() {
        let summary = protocol::StyledLines(vec![vec![
            protocol::StyledSpan {
                text: "echo hi".into(),
                ..Default::default()
            },
            protocol::StyledSpan {
                text: "(timeout: 2m)".into(),
                selectable: false,
                title_suffix: true,
                ..Default::default()
            },
        ]]);
        let buf = project_tool_title("bash", summary);

        assert!(buf.get_line(0).unwrap_or("").contains("timeout"));
        assert_eq!(copy_first_row(&buf), "\u{23fa} bash echo hi");
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
        buf.set_all_lines(vec!["│ hi".into()]);
        buf.add_highlight_group_with_meta(0, 0, 2, hl_for_test(), unselectable_meta());
        let line_bytes = "│ hi".len();
        assert_eq!(copy_byte_range(&buf, 0, line_bytes), "hi");
    }

    #[test]
    fn copy_applies_copy_as_substitution_once_per_span() {
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
        buf.set_all_lines(vec!["+ add".into()]);
        buf.add_highlight_group_with_meta(0, 0, 2, hl_for_test(), copy_as_meta(""));
        assert_eq!(copy_byte_range(&buf, 0, "+ add".len()), "add");
    }

    #[test]
    fn copy_uses_source_text_when_full_row_selected() {
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(3), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            40,
            false,
            &theme,
            ScrollTarget::visible_row(0),
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
        let rows = projection.build_rows(&mut transcript.history, 80, false, &theme);

        let (soft, hard) = projection.line_breaks(&mut transcript.history, 80, false, &theme);
        assert!(
            soft.is_empty(),
            "unwrapped source lines must be hard breaks"
        );
        assert_eq!(hard, crate::smelt_edit::hard_breaks_for_lines(&rows));
    }

    #[test]
    fn table_full_selection_copies_raw_markdown() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "| a | b |\n| - | - |\n| 1 | 2 |".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(4), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            40,
            false,
            &theme,
            ScrollTarget::visible_row(0),
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(5), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            40,
            false,
            &theme,
            ScrollTarget::visible_row(0),
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
        let mut buf = Buffer::new(crate::smelt_edit::BufId(6), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            24,
            false,
            &theme,
            ScrollTarget::visible_row(0),
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
