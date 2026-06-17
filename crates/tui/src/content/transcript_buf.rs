use super::display_layout::{
    measure_block, render_block_into, CompileJob, DisplayModel, DisplayRowIndexEntry,
    DisplayRowIndexNode, MeasureCtx, RenderCtx, TranscriptRenderEnv,
};
use crate::content::render_plan::{
    NodeLayoutKey, RenderNode, RenderNodeId, RenderPlan, TranscriptDefaultViewPolicy,
    TranscriptPresentationState,
};
use crate::smelt_edit::Theme;
use crate::smelt_edit::{
    clamp_scroll, row_to_usize, BufCreateOpts, BufId, Buffer, CopyOutput, DisplayRow, DisplayRows,
    DocRange, MaterializedRows, RowBreak, RowIndex,
};
use smelt_buffer::coords::copy_byte_range;
use smelt_core::buffer::{LineDecoration, Span, SpanMeta};
use smelt_core::content::highlight::InlineOptions;
use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey, ViewState};
use std::sync::Arc;

pub(crate) struct TranscriptProjection {
    render_plan: RenderPlan,
    default_view_policy: TranscriptDefaultViewPolicy,
    presentation: TranscriptPresentationState,
    display_layouts: DisplayModel,
    display_layouts_generation: u64,
    active_width: u16,
    visible: VisibleProjectionState,
    measurements: MeasurementIndexStore,
    display_cache_generation: u64,
    renderer_generation: Option<u64>,
    renderer_cache_key: Option<u64>,
    inline_options: InlineOptions,
    #[cfg(test)]
    counters: TranscriptProjectionCounters,
}

#[derive(Default)]
struct VisibleProjectionState {
    materialized: Option<MaterializedProjection>,
    /// Block layout from the last visible `project()`. Surfaced to Lua via `visible_blocks`.
    block_layout: Vec<LayoutEntry>,
    /// Absolute row represented by local row 0 in the backing buffer.
    row_base: RowIndex,
    /// Total rows in the logical transcript represented by the visible projection.
    total_rows: RowIndex,
    /// Rendered-row anchors for the materialized rows, parallel to the backing buffer lines.
    row_anchors: Vec<Option<RenderedRowAnchor>>,
    /// Cached `build_rows` result for full-text consumers (Lua API, vim navigation).
    full_rows: Option<CachedRows>,
}

#[derive(Default)]
struct MeasurementIndexStore {
    active: ExactRowIndex,
    entries: Vec<DisplayRowIndexEntry>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TranscriptProjectionCounters {
    pub full_row_builds: usize,
    pub display_layouts: usize,
    pub exact_height_measured_blocks: usize,
    pub range_materialized_blocks: usize,
}

struct CachedRows {
    rows: Arc<Vec<String>>,
    generation: u64,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
    width: u16,
}

#[derive(Clone, Copy)]
struct RowIndexKey {
    width: u16,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
    base_key: LayoutKey,
}

impl RowIndexKey {
    fn new(
        width: u16,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        presentation_generation: u64,
    ) -> Self {
        Self {
            width,
            renderer_generation,
            renderer_cache_key,
            presentation_generation,
            base_key: base_layout_key(width),
        }
    }
}

#[derive(Default)]
struct ExactRowIndex {
    nodes: Vec<ExactNodeRow>,
    prefix_rows: Vec<RowIndex>,
    prefix_dirty: bool,
    generation: u64,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
    width: u16,
}

struct ExactNodeRow {
    id: RenderNodeId,
    key: NodeLayoutKey,
    estimated_height: RowIndex,
    exact_height: Option<RowIndex>,
}

#[derive(serde::Serialize)]
struct SemanticLayoutNodeKey {
    id: RenderNodeId,
    key: NodeLayoutKey,
    exact_height: Option<RowIndex>,
}

impl ExactNodeRow {
    fn measured_or_estimated_height(&self) -> RowIndex {
        self.exact_height.unwrap_or(self.estimated_height)
    }
}

impl ExactRowIndex {
    fn is_current(&self, plan: &RenderPlan, key: RowIndexKey) -> bool {
        self.generation == plan.fingerprint
            && self.renderer_generation == key.renderer_generation
            && self.renderer_cache_key == key.renderer_cache_key
            && self.presentation_generation == key.presentation_generation
            && self.width == key.width
            && self.nodes.len() == plan.len()
    }

    fn rebuild_if_stale(
        &mut self,
        history: &BlockHistory,
        plan: &RenderPlan,
        policy: &TranscriptDefaultViewPolicy,
        presentation: &TranscriptPresentationState,
        key: RowIndexKey,
    ) {
        let gen = plan.fingerprint;
        if self.generation == gen
            && self.renderer_generation == key.renderer_generation
            && self.renderer_cache_key == key.renderer_cache_key
            && self.presentation_generation == key.presentation_generation
            && self.width == key.width
        {
            return;
        }

        let keep_measurements = self.renderer_generation == key.renderer_generation
            && self.renderer_cache_key == key.renderer_cache_key
            && self.width == key.width;
        let old_nodes = if keep_measurements {
            std::mem::take(&mut self.nodes)
        } else {
            Vec::new()
        };
        self.nodes.clear();
        self.nodes.reserve(plan.len());
        for index in 0..plan.len() {
            let Some(id) = plan.node_id(index) else {
                continue;
            };
            let Some(node_key) = plan.node_key(policy, history, presentation, index, key.base_key)
            else {
                continue;
            };
            let old_same_index = old_nodes.get(index).filter(|node| node.id == id);
            let estimated_height = old_same_index
                .map(ExactNodeRow::measured_or_estimated_height)
                .or_else(|| {
                    old_nodes
                        .iter()
                        .find(|node| node.id == id)
                        .map(ExactNodeRow::measured_or_estimated_height)
                })
                .unwrap_or(1);
            let same_previous = index == 0
                || old_nodes
                    .get(index.saturating_sub(1))
                    .zip(self.nodes.get(index.saturating_sub(1)))
                    .is_some_and(|(old, new)| old.id == new.id && old.key == new.key);
            let exact_height = old_same_index
                .filter(|node| node.key == node_key && same_previous)
                .and_then(|node| node.exact_height);
            self.nodes.push(ExactNodeRow {
                id,
                key: node_key,
                estimated_height,
                exact_height,
            });
        }
        self.generation = gen;
        self.renderer_generation = key.renderer_generation;
        self.renderer_cache_key = key.renderer_cache_key;
        self.presentation_generation = key.presentation_generation;
        self.width = key.width;
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
        self.prefix_dirty = true;
        true
    }

    /// Sync the index when the current history keeps the old order as a prefix.
    /// Returns `false` when a deletion or reorder means the index must be rebuilt.
    fn sync_stable_order_prefix(
        &mut self,
        history: &BlockHistory,
        plan: &RenderPlan,
        policy: &TranscriptDefaultViewPolicy,
        presentation: &TranscriptPresentationState,
        key: RowIndexKey,
    ) -> bool {
        let old_len = self.nodes.len();
        if self.renderer_generation != key.renderer_generation
            || self.renderer_cache_key != key.renderer_cache_key
        {
            return false;
        }
        if old_len > plan.len() {
            return false;
        }
        if old_len == plan.len()
            && self.generation == plan.fingerprint
            && self.renderer_generation == key.renderer_generation
            && self.renderer_cache_key == key.renderer_cache_key
            && self.presentation_generation == key.presentation_generation
            && self.width == key.width
        {
            return true;
        }
        let mut prev_key_changed = false;
        let mut prefix_dirty = false;
        for index in 0..old_len {
            let Some(id) = plan.node_id(index) else {
                return false;
            };
            let Some(node_key) = plan.node_key(policy, history, presentation, index, key.base_key)
            else {
                return false;
            };
            let node = &mut self.nodes[index];
            if node.id != id {
                return false;
            }
            if node.key != node_key {
                node.key = node_key;
                node.exact_height = None;
                prev_key_changed = true;
                prefix_dirty = true;
            } else if prev_key_changed {
                node.exact_height = None;
                prev_key_changed = false;
                prefix_dirty = true;
            }
        }
        if old_len < plan.len() {
            prefix_dirty = true;
        }
        for index in old_len..plan.len() {
            let Some(id) = plan.node_id(index) else {
                return false;
            };
            let Some(node_key) = plan.node_key(policy, history, presentation, index, key.base_key)
            else {
                return false;
            };
            self.nodes.push(ExactNodeRow {
                id,
                key: node_key,
                estimated_height: 1,
                exact_height: None,
            });
        }
        self.generation = plan.fingerprint;
        self.renderer_generation = key.renderer_generation;
        self.renderer_cache_key = key.renderer_cache_key;
        self.presentation_generation = key.presentation_generation;
        self.width = key.width;
        self.prefix_dirty |= prefix_dirty;
        true
    }

    fn is_exact_for(&self, plan: &RenderPlan, key: RowIndexKey) -> bool {
        self.is_current(plan, key) && self.nodes.iter().all(|node| node.exact_height.is_some())
    }

    fn refresh_prefix_rows(&mut self) {
        if self.prefix_dirty {
            self.rebuild_prefix_rows();
        }
    }

    fn prefix_row(&self, index: usize) -> RowIndex {
        self.prefix_rows.get(index).copied().unwrap_or(0)
    }

    fn total_rows(&self) -> RowIndex {
        self.prefix_rows.last().copied().unwrap_or(0)
    }

    fn semantic_layout_hash(&self) -> u64 {
        smelt_core::utils::hash_serializable(&(
            self.generation,
            self.renderer_generation,
            self.renderer_cache_key,
            self.presentation_generation,
            self.width,
            self.nodes
                .iter()
                .map(|node| SemanticLayoutNodeKey {
                    id: node.id,
                    key: node.key,
                    exact_height: node.exact_height,
                })
                .collect::<Vec<_>>(),
        ))
    }

    fn start_index_for_row(&self, row: RowIndex) -> usize {
        let idx = self.prefix_rows.partition_point(|prefix| *prefix <= row);
        idx.saturating_sub(1).min(self.nodes.len())
    }

    fn node_index_at_row(&self, row: RowIndex) -> Option<usize> {
        if self.nodes.is_empty() || row >= self.total_rows() {
            return None;
        }
        Some(
            self.start_index_for_row(row)
                .min(self.nodes.len().saturating_sub(1)),
        )
    }

    fn block_index(&self, id: BlockId) -> Option<usize> {
        self.nodes
            .iter()
            .position(|node| node.id.as_block_id() == Some(id))
    }

    fn end_index_for_row_end(&self, row_end: RowIndex) -> usize {
        self.prefix_rows
            .partition_point(|prefix| *prefix < row_end)
            .min(self.nodes.len())
    }

    fn node_range_for_rows(&self, rows: std::ops::Range<RowIndex>) -> std::ops::Range<usize> {
        if rows.start >= rows.end {
            return 0..0;
        }
        let first = self.start_index_for_row(rows.start);
        let end = self.end_index_for_row_end(rows.end).max(first);
        first..end
    }

    fn hydrate_from_cache(
        &mut self,
        history: &BlockHistory,
        plan: &RenderPlan,
        policy: &TranscriptDefaultViewPolicy,
        presentation: &TranscriptPresentationState,
        entry: &DisplayRowIndexEntry,
        key: RowIndexKey,
    ) -> bool {
        if entry.renderer_generation != key.renderer_generation
            || entry.renderer_cache_key != key.renderer_cache_key
            || entry.width != key.width
        {
            return false;
        }
        if entry.nodes.len() != plan.len() {
            return false;
        }
        let mut nodes = Vec::with_capacity(entry.nodes.len());
        for (index, cached) in entry.nodes.iter().enumerate() {
            let Some(id) = plan.node_id(index) else {
                return false;
            };
            if cached.id != id {
                return false;
            }
            let Some(node_key) = plan.node_key(policy, history, presentation, index, key.base_key)
            else {
                return false;
            };
            if cached.key != node_key {
                return false;
            }
            nodes.push(ExactNodeRow {
                id,
                key: node_key,
                estimated_height: cached.exact_height,
                exact_height: Some(cached.exact_height),
            });
        }
        self.nodes = nodes;
        self.generation = plan.fingerprint;
        self.renderer_generation = key.renderer_generation;
        self.renderer_cache_key = key.renderer_cache_key;
        self.presentation_generation = key.presentation_generation;
        self.width = key.width;
        self.rebuild_prefix_rows();
        true
    }

    fn cache_entry(&self) -> Option<DisplayRowIndexEntry> {
        if self.nodes.is_empty() || self.nodes.iter().any(|node| node.exact_height.is_none()) {
            return None;
        }
        Some(DisplayRowIndexEntry {
            width: self.width,
            renderer_generation: self.renderer_generation,
            renderer_cache_key: self.renderer_cache_key,
            nodes: self
                .nodes
                .iter()
                .map(|node| DisplayRowIndexNode {
                    id: node.id,
                    key: node.key,
                    exact_height: node.exact_height.unwrap_or(node.estimated_height),
                })
                .collect(),
        })
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
        self.prefix_dirty = false;
    }
}

impl MeasurementIndexStore {
    fn clear(&mut self) {
        self.active = ExactRowIndex::default();
        self.entries.clear();
    }

    fn clear_active(&mut self) {
        self.active = ExactRowIndex::default();
    }

    fn hydrate(&mut self, entries: Vec<DisplayRowIndexEntry>) {
        self.entries = entries;
    }

    fn remember_active(&mut self) {
        if let Some(entry) = self.active.cache_entry() {
            upsert_row_index_entry(&mut self.entries, entry);
        }
    }

    fn export_entries(
        &self,
        history: &BlockHistory,
        plan: &RenderPlan,
        policy: &TranscriptDefaultViewPolicy,
        presentation: &TranscriptPresentationState,
        renderer_generation: Option<u64>,
        renderer_cache_key: Option<u64>,
    ) -> Vec<DisplayRowIndexEntry> {
        let mut entries: Vec<DisplayRowIndexEntry> = self
            .entries
            .iter()
            .filter(|entry| {
                row_index_entry_matches_renderer(entry, renderer_generation, renderer_cache_key)
                    && row_index_entry_matches(history, plan, policy, presentation, entry)
            })
            .cloned()
            .collect();
        if let Some(current) = self.active.cache_entry() {
            if row_index_entry_matches_renderer(&current, renderer_generation, renderer_cache_key)
                && row_index_entry_matches(history, plan, policy, presentation, &current)
            {
                upsert_row_index_entry(&mut entries, current);
            }
        }
        entries
    }

    fn find_entry(
        &self,
        width: u16,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> Option<&DisplayRowIndexEntry> {
        self.entries.iter().find(|entry| {
            entry.width == width
                && entry.renderer_generation == renderer_generation
                && entry.renderer_cache_key == renderer_cache_key
        })
    }
}

#[derive(Clone, Copy)]
struct RenderedRowAnchor {
    id: BlockId,
    row_offset: RowIndex,
    display_offset: usize,
}

#[derive(Clone, Copy)]
struct ResizeAnchor {
    id: BlockId,
    row_offset: RowIndex,
    display_offset: Option<usize>,
}

#[derive(Clone, Copy)]
struct LayoutEntry {
    id: BlockId,
    /// First absolute row of the block, after its leading gap.
    start: RowIndex,
    rows: RowIndex,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptSearchLayout {
    pub(crate) generation: u64,
    pub(crate) entries: Vec<TranscriptSearchLayoutEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptSearchLayoutEntry {
    pub(crate) id: RenderNodeId,
    pub(crate) key: NodeLayoutKey,
    pub(crate) block_ids: Vec<BlockId>,
    pub(crate) first_row: RowIndex,
    pub(crate) rows: RowIndex,
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct ProjectKey {
    generation: u64,
    width: u16,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
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
    node_range: std::ops::Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FoldAction {
    Toggle,
    Peek,
    Open,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FoldActivation {
    AnyNodeRow,
    ExplicitTargetOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FoldAtRow {
    pub(crate) row: RowIndex,
    pub(crate) action: FoldAction,
    pub(crate) activation: FoldActivation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptNodeRow {
    pub(crate) id: RenderNodeId,
    pub(crate) index: usize,
    pub(crate) first_row: RowIndex,
    pub(crate) rows: RowIndex,
    pub(crate) row_offset: RowIndex,
    pub(crate) view_state: ViewState,
    pub(crate) explicit_fold_target: bool,
}

impl TranscriptNodeRow {
    pub(crate) fn can_activate(self, activation: FoldActivation) -> bool {
        match activation {
            FoldActivation::AnyNodeRow => true,
            FoldActivation::ExplicitTargetOnly => self.explicit_fold_target,
        }
    }
}

fn is_explicit_fold_target(view_state: ViewState, row_offset: RowIndex, rows: RowIndex) -> bool {
    match view_state {
        ViewState::Collapsed | ViewState::Peek => true,
        ViewState::TrimmedHead { .. } => row_offset.saturating_add(1) == rows,
        ViewState::TrimmedTail { .. } => row_offset == 0,
        ViewState::Expanded => false,
    }
}

fn layout_view_state(
    _id: RenderNodeId,
    _view_state: ViewState,
    _history: &BlockHistory,
) -> ViewState {
    ViewState::Expanded
}

impl ProjectionPlan {
    pub(crate) fn node_range(&self) -> std::ops::Range<usize> {
        self.node_range.clone()
    }
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
    row_anchors: &'a mut Vec<Option<RenderedRowAnchor>>,
}

struct MaterializedTranscriptRange {
    row_base: RowIndex,
    total_rows: RowIndex,
    texts: Vec<String>,
    pending: Vec<PendingRow>,
    layout: Vec<LayoutEntry>,
    row_anchors: Vec<Option<RenderedRowAnchor>>,
}

fn base_layout_key(width: u16) -> LayoutKey {
    LayoutKey {
        view_state: ViewState::Expanded,
        width,
        content_hash: 0,
        sidecar_hash: 0,
    }
}

fn subtract_byte_range(ranges: &mut Vec<std::ops::Range<usize>>, remove: std::ops::Range<usize>) {
    if remove.start >= remove.end {
        return;
    }
    let mut out = Vec::with_capacity(ranges.len() + 1);
    for range in ranges.drain(..) {
        if remove.end <= range.start || remove.start >= range.end {
            out.push(range);
            continue;
        }
        if range.start < remove.start {
            out.push(range.start..remove.start);
        }
        if remove.end < range.end {
            out.push(remove.end..range.end);
        }
    }
    *ranges = out;
}

fn selectable_row_text(buf: &Buffer, row: usize) -> String {
    let Some(line) = buf.get_line(row) else {
        return String::new();
    };
    let mut ranges: Vec<std::ops::Range<usize>> = std::iter::once(0..line.len()).collect();
    for span in buf
        .highlights_at(row)
        .into_iter()
        .filter(|span| !span.meta.selectable)
    {
        let start = smelt_buffer::text::cell_to_byte(line, span.col_start as usize);
        let end = smelt_buffer::text::cell_to_byte(line, span.col_end as usize);
        subtract_byte_range(&mut ranges, start..end);
    }
    ranges
        .into_iter()
        .map(|range| smelt_buffer::text::slice(line, range))
        .collect()
}

fn rendered_row_anchors(buf: &Buffer, id: BlockId, rows: usize) -> Vec<RenderedRowAnchor> {
    let mut anchors = Vec::with_capacity(rows);
    let mut display_offset = 0usize;
    for row in 0..rows {
        anchors.push(RenderedRowAnchor {
            id,
            row_offset: row as RowIndex,
            display_offset,
        });
        display_offset = display_offset.saturating_add(selectable_row_text(buf, row).len());
    }
    anchors
}

fn row_offset_for_display_offset(
    buf: &Buffer,
    rows: usize,
    display_offset: usize,
    fallback: RowIndex,
) -> RowIndex {
    if rows == 0 {
        return 0;
    }
    let offsets: Vec<usize> = (0..rows)
        .scan(0usize, |offset, row| {
            let current = *offset;
            *offset = offset.saturating_add(selectable_row_text(buf, row).len());
            Some(current)
        })
        .collect();
    if offsets.iter().all(|offset| *offset == 0) {
        return fallback.min(rows.saturating_sub(1) as RowIndex);
    }
    let mut row = offsets.partition_point(|offset| *offset <= display_offset);
    row = row.saturating_sub(1).min(rows.saturating_sub(1));
    while row > 0 && offsets[row - 1] == offsets[row] {
        row -= 1;
    }
    row as RowIndex
}

fn row_index_entry_matches(
    history: &BlockHistory,
    plan: &RenderPlan,
    policy: &TranscriptDefaultViewPolicy,
    presentation: &TranscriptPresentationState,
    entry: &DisplayRowIndexEntry,
) -> bool {
    if entry.nodes.len() != plan.len() {
        return false;
    }
    let base_key = base_layout_key(entry.width);
    entry.nodes.iter().enumerate().all(|(index, node)| {
        plan.node_id(index) == Some(node.id)
            && plan.node_key(policy, history, presentation, index, base_key) == Some(node.key)
    })
}

fn row_index_entry_matches_renderer(
    entry: &DisplayRowIndexEntry,
    generation: Option<u64>,
    cache_key: Option<u64>,
) -> bool {
    if entry.renderer_cache_key.is_none() {
        return false;
    }
    match generation {
        Some(generation) => {
            cache_key.is_some()
                && entry.renderer_generation == generation
                && entry.renderer_cache_key == cache_key
        }
        None => true,
    }
}

fn upsert_row_index_entry(entries: &mut Vec<DisplayRowIndexEntry>, entry: DisplayRowIndexEntry) {
    if let Some(existing) = entries.iter_mut().find(|existing| {
        existing.width == entry.width
            && existing.renderer_generation == entry.renderer_generation
            && existing.renderer_cache_key == entry.renderer_cache_key
    }) {
        *existing = entry;
    } else {
        entries.push(entry);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_cached_layout_to_buffer(
    display_model: &DisplayModel,
    id: RenderNodeId,
    key: NodeLayoutKey,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    theme: &Theme,
    history: &BlockHistory,
    inline_options: &InlineOptions,
) -> Option<(Buffer, usize)> {
    let layout = display_model.get(id, key, renderer_generation, renderer_cache_key)?;
    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let outcome = render_block_into(
        &mut buf,
        layout,
        RenderCtx {
            width: key.width,
            view_state: layout_view_state(id, key.view_state, history),
            theme,
            history: Some(history),
            inline_options: inline_options.clone(),
        },
    );
    Some((buf, outcome.line_count))
}

impl TranscriptProjection {
    pub(crate) fn new() -> Self {
        Self {
            render_plan: RenderPlan::empty(),
            default_view_policy: TranscriptDefaultViewPolicy::default(),
            presentation: TranscriptPresentationState::default(),
            display_layouts: DisplayModel::new(),
            display_layouts_generation: u64::MAX,
            active_width: 0,
            visible: VisibleProjectionState::default(),
            measurements: MeasurementIndexStore::default(),
            display_cache_generation: 0,
            renderer_generation: None,
            renderer_cache_key: None,
            inline_options: InlineOptions::default(),
            #[cfg(test)]
            counters: TranscriptProjectionCounters::default(),
        }
    }

    fn refresh_render_plan(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &BlockHistory,
    ) {
        let policy = TranscriptDefaultViewPolicy::from_lua(lua);
        let policy_changed = policy != self.default_view_policy;
        if policy_changed {
            self.default_view_policy = policy;
            self.display_layouts = DisplayModel::new();
            self.display_layouts_generation = u64::MAX;
            self.measurements.clear();
            self.clear_visible_state();
            self.visible.full_rows = None;
            self.display_cache_generation = self.display_cache_generation.wrapping_add(1);
        }
        let group_generation = lua.transcript_group_generation();
        let group_cache_key = lua.transcript_group_cache_key();
        if policy_changed
            || self.render_plan.history_generation != history.generation()
            || self.render_plan.group_generation != group_generation
            || self.render_plan.group_cache_key != group_cache_key
        {
            let groups: Vec<_> = lua
                .transcript_group_specs()
                .into_iter()
                .filter(|spec| self.default_view_policy.group_enabled(&spec.name))
                .collect();
            self.render_plan = RenderPlan::for_history_with_groups(
                history,
                &groups,
                group_generation,
                group_cache_key,
            );
            self.presentation.prune(self.render_plan.ids());
        }
    }

    pub(crate) fn inline_options(&self) -> &InlineOptions {
        &self.inline_options
    }

    pub(crate) fn set_inline_options(&mut self, options: InlineOptions) {
        if self.inline_options == options {
            return;
        }
        self.inline_options = options;
        self.display_layouts = DisplayModel::new();
        self.display_layouts_generation = u64::MAX;
        self.measurements.clear();
        self.clear_visible_state();
        self.visible.full_rows = None;
        self.display_cache_generation = self.display_cache_generation.wrapping_add(1);
    }

    pub(crate) fn hydrate_display_cache(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &BlockHistory,
        data: crate::content::display_cache::DisplayCacheData,
    ) -> usize {
        self.refresh_render_plan(lua, history);
        let crate::content::display_cache::DisplayCacheData {
            row_indexes,
            display_layouts,
        } = data;
        let hydrated_layouts = self.display_layouts.hydrate_from_cache(
            history,
            &self.render_plan,
            &self.default_view_policy,
            &self.presentation,
            display_layouts,
        );
        self.measurements.hydrate(row_indexes);
        smelt_perf::perf::record_value(
            "transcript:display_model_cache:loaded",
            hydrated_layouts as u64,
        );
        smelt_perf::perf::record_value(
            "transcript:row_index_cache:loaded",
            self.measurements.entries.len() as u64,
        );
        self.row_index_cache_entries(history).len()
    }

    pub(crate) fn display_cache_data(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &BlockHistory,
    ) -> crate::content::display_cache::DisplayCacheData {
        self.refresh_render_plan(lua, history);
        if !self.persisted_display_cache_enabled() {
            return crate::content::display_cache::DisplayCacheData::default();
        }
        crate::content::display_cache::DisplayCacheData {
            row_indexes: self.row_index_cache_entries(history),
            display_layouts: self.display_layouts.cache_entries(
                history,
                &self.render_plan,
                &self.default_view_policy,
                &self.presentation,
                self.renderer_generation,
                self.renderer_cache_key,
            ),
        }
    }

    pub(crate) fn display_cache_generation(&self) -> u64 {
        self.display_cache_generation
    }

    fn persisted_display_cache_enabled(&self) -> bool {
        self.render_plan.group_generation == 0 || self.render_plan.group_cache_key.is_some()
    }

    pub(crate) fn invalidate_renderer_if_changed(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) -> bool {
        if self.renderer_generation == Some(generation) && self.renderer_cache_key == cache_key {
            return false;
        }
        let initialized = self.renderer_generation.is_some();
        self.renderer_generation = Some(generation);
        self.renderer_cache_key = cache_key;
        if !initialized {
            return false;
        }
        self.display_layouts = DisplayModel::new();
        self.display_layouts_generation = u64::MAX;
        self.measurements.clear();
        self.clear_visible_state();
        self.visible.full_rows = None;
        self.display_cache_generation = self.display_cache_generation.wrapping_add(1);
        true
    }

    fn row_index_cache_entries(&mut self, history: &BlockHistory) -> Vec<DisplayRowIndexEntry> {
        if !self.persisted_display_cache_enabled() {
            return Vec::new();
        }
        self.measurements.export_entries(
            history,
            &self.render_plan,
            &self.default_view_policy,
            &self.presentation,
            self.renderer_generation,
            self.renderer_cache_key,
        )
    }

    /// Snapshot of the visibly laid-out blocks: `(BlockId, first_row, rows)`.
    /// Used by Lua's `smelt.transcript.visible_blocks()` to map block indices
    /// back to display rows without forcing full transcript materialization.
    pub(crate) fn visible_block_layout(
        &self,
    ) -> impl Iterator<Item = (BlockId, RowIndex, RowIndex)> + '_ {
        self.visible
            .block_layout
            .iter()
            .map(|e| (e.id, e.start, e.rows))
    }

    #[cfg(test)]
    pub(crate) fn display_layouts_len(&self) -> usize {
        self.display_layouts.len()
    }

    #[cfg(test)]
    pub(crate) fn counters(&self) -> TranscriptProjectionCounters {
        self.counters
    }

    #[cfg(test)]
    pub(crate) fn reset_counters(&mut self) {
        self.counters = TranscriptProjectionCounters::default();
    }

    fn finish_compile_jobs(&mut self, env: TranscriptRenderEnv<'_>, jobs: Vec<CompileJob>) {
        let compiled = jobs.len();
        let blocks = jobs
            .into_iter()
            .map(|job| job.compile(env.clone()))
            .collect();
        self.display_layouts.insert_compiled_blocks(blocks);
        if compiled > 0 {
            self.display_cache_generation = self.display_cache_generation.wrapping_add(1);
        }
        #[cfg(test)]
        {
            self.counters.display_layouts += compiled;
        }
        let _ = compiled;
    }

    fn ensure_node_indices(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        indices: impl IntoIterator<Item = usize>,
    ) {
        let jobs = {
            let row_nodes = &self.measurements.active.nodes;
            let nodes = indices
                .into_iter()
                .filter_map(|index| {
                    row_nodes.get(index).and_then(|row| {
                        self.render_plan
                            .node(index)
                            .cloned()
                            .map(|node| (index, node, row.key))
                    })
                })
                .collect::<Vec<_>>();
            self.display_layouts.collect_compile_jobs(
                history,
                &self.default_view_policy,
                env.renderer_generation,
                env.renderer_cache_key,
                nodes,
            )
        };
        self.finish_compile_jobs(env, jobs);
    }

    fn clear_visible_state(&mut self) {
        self.visible.materialized = None;
        self.visible.block_layout.clear();
        self.visible.row_base = 0;
        self.visible.row_anchors.clear();
        self.visible.total_rows = 0;
    }

    fn clear_width_dependent_state(&mut self) {
        self.measurements.remember_active();
        self.measurements.clear_active();
        self.clear_visible_state();
        self.visible.full_rows = None;
    }

    fn gc_if_stale(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &BlockHistory,
        width: u16,
    ) {
        self.refresh_render_plan(lua, history);
        let fingerprint = self.render_plan.fingerprint;
        if self.display_layouts_generation != fingerprint {
            self.display_layouts.retain_nodes(self.render_plan.ids());
            self.display_layouts_generation = fingerprint;
        }
        if width != self.active_width {
            // Width changes invalidate row indexes and materialized rows, but
            // display layouts are width-independent and stay reusable.
            self.active_width = width;
            self.clear_width_dependent_state();
        }
    }

    /// Clear rendered visible state so the next projection repaints with the current theme.
    /// Display layouts and exact measurements are theme-independent.
    pub(crate) fn invalidate_theme(&mut self) {
        self.clear_visible_state();
    }

    fn target_has_projection(&self, key: ProjectKey, buf: &Buffer) -> bool {
        self.visible.materialized.is_some_and(|m| {
            m.key == key && m.buf_id == buf.id() && m.changedtick == buf.changedtick()
        })
    }

    fn last_project_key(&self) -> Option<ProjectKey> {
        self.visible.materialized.map(|m| m.key)
    }

    fn mark_projected_into(&mut self, key: ProjectKey, buf: &Buffer) {
        self.visible.materialized = Some(MaterializedProjection {
            key,
            buf_id: buf.id(),
            changedtick: buf.changedtick(),
        });
    }

    fn try_hydrate_row_index(
        &mut self,
        history: &BlockHistory,
        plan: &RenderPlan,
        key: RowIndexKey,
    ) -> bool {
        if self.measurements.active.is_current(plan, key) {
            return true;
        }
        let Some(entry) = self
            .measurements
            .find_entry(key.width, key.renderer_generation, key.renderer_cache_key)
            .cloned()
        else {
            smelt_perf::perf::record_value("transcript:row_index_cache:miss", 1);
            return false;
        };
        let hydrated = self.measurements.active.hydrate_from_cache(
            history,
            plan,
            &self.default_view_policy,
            &self.presentation,
            &entry,
            key,
        );
        smelt_perf::perf::record_value(
            if hydrated {
                "transcript:row_index_cache:hydrated"
            } else {
                "transcript:row_index_cache:hydrate_reject"
            },
            1,
        );
        hydrated
    }

    pub(crate) fn rebuild_row_index(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
    ) {
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.rebuild_row_index_with_env(env, history, width);
    }

    fn rebuild_row_index_with_env(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &mut BlockHistory,
        width: u16,
    ) {
        let _perf = smelt_perf::perf::begin("transcript:rebuild_row_index");
        smelt_perf::perf::record_value(
            "transcript:rebuild_row_index:blocks",
            history.order.len() as u64,
        );
        smelt_perf::perf::record_value(
            "transcript:rebuild_row_index:generation",
            history.generation(),
        );
        let renderer_generation = env.renderer_generation;
        let renderer_cache_key = env.renderer_cache_key;
        self.invalidate_renderer_if_changed(renderer_generation, renderer_cache_key);
        self.gc_if_stale(env.lua, history, width);
        let plan = self.render_plan.clone();
        let row_key = RowIndexKey::new(
            width,
            renderer_generation,
            renderer_cache_key,
            self.presentation.generation(),
        );
        let hydrated_index = self.try_hydrate_row_index(history, &plan, row_key);
        let reused_index = hydrated_index
            || self.measurements.active.sync_stable_order_prefix(
                history,
                &plan,
                &self.default_view_policy,
                &self.presentation,
                row_key,
            );
        smelt_perf::perf::record_value(
            "transcript:rebuild_row_index:reused_index",
            u64::from(reused_index),
        );
        if !reused_index {
            let _perf = smelt_perf::perf::begin("transcript:rebuild_row_index:rebuild_index");
            self.measurements.active.rebuild_if_stale(
                history,
                &plan,
                &self.default_view_policy,
                &self.presentation,
                row_key,
            );
        }
        if self.measurements.active.is_exact_for(&plan, row_key) {
            if reused_index {
                self.measurements.active.refresh_prefix_rows();
            }
            self.measurements.remember_active();
            return;
        }

        let missing: Vec<usize> = {
            let _perf = smelt_perf::perf::begin("transcript:rebuild_row_index:collect_missing");
            (0..self.measurements.active.nodes.len())
                .filter(|&i| {
                    self.measurements
                        .active
                        .nodes
                        .get(i)
                        .is_some_and(|node| node.exact_height.is_none())
                })
                .collect()
        };
        smelt_perf::perf::record_value(
            "transcript:rebuild_row_index:missing",
            missing.len() as u64,
        );
        if let (Some(first), Some(last)) = (missing.first(), missing.last()) {
            smelt_perf::perf::record_value(
                "transcript:rebuild_row_index:missing:first_index",
                *first as u64,
            );
            smelt_perf::perf::record_value(
                "transcript:rebuild_row_index:missing:last_index",
                *last as u64,
            );
        }
        self.ensure_node_indices(env, history, missing.iter().copied());
        for i in missing {
            self.measure_cached_layout_height(history, i, renderer_generation, renderer_cache_key);
        }
        self.measurements.active.refresh_prefix_rows();
        self.measurements.remember_active();
    }

    fn measure_cached_layout_height(
        &mut self,
        history: &BlockHistory,
        index: usize,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> bool {
        let Some(node) = self.measurements.active.nodes.get(index) else {
            return false;
        };
        if node.exact_height.is_some() {
            return true;
        }
        let id = node.id;
        let key = node.key;
        let Some(block) =
            self.display_layouts
                .get(id, key, renderer_generation, renderer_cache_key)
        else {
            return false;
        };
        let rows = measure_block(
            block,
            MeasureCtx {
                width: key.width,
                view_state: layout_view_state(id, key.view_state, history),
                inline_options: self.inline_options.clone(),
            },
        ) as RowIndex;
        let gap = self
            .render_plan
            .rendered_node_gap(history, index, rows as usize) as RowIndex;
        self.set_exact_height(index, gap.saturating_add(rows));
        true
    }

    fn set_exact_height(&mut self, index: usize, rows: RowIndex) {
        let measured = self.measurements.active.set_exact_height(index, rows);
        if measured {
            self.display_cache_generation = self.display_cache_generation.wrapping_add(1);
        }
        #[cfg(test)]
        if measured {
            self.counters.exact_height_measured_blocks += 1;
        }
    }

    fn exact_block_layout(&self, history: &BlockHistory) -> Vec<LayoutEntry> {
        let mut layout = Vec::with_capacity(self.measurements.active.nodes.len());
        let mut running_total: RowIndex = 0;
        for (i, measured_node) in self.measurements.active.nodes.iter().enumerate() {
            debug_assert!(
                measured_node.exact_height.is_some(),
                "exact block layout requested before height measurement"
            );
            let Some(exact_height) = measured_node.exact_height else {
                continue;
            };
            let gap = (self
                .render_plan
                .rendered_node_gap(history, i, exact_height as usize)
                as RowIndex)
                .min(exact_height);
            running_total = running_total.saturating_add(gap);
            let start = running_total;
            let rows = exact_height.saturating_sub(gap);
            match self.render_plan.node(i) {
                Some(RenderNode::Block { id, .. }) => layout.push(LayoutEntry {
                    id: *id,
                    start,
                    rows,
                }),
                Some(RenderNode::Group { child_ids, .. }) => {
                    layout.extend(child_ids.iter().copied().map(|id| LayoutEntry {
                        id,
                        start,
                        rows,
                    }));
                }
                None => {}
            }
            running_total = running_total.saturating_add(rows);
        }
        layout
    }

    fn exact_search_layout(
        &self,
        generation: u64,
        history: &BlockHistory,
    ) -> TranscriptSearchLayout {
        let mut entries = Vec::with_capacity(self.measurements.active.nodes.len());
        let mut running_total: RowIndex = 0;
        for (i, measured_node) in self.measurements.active.nodes.iter().enumerate() {
            debug_assert!(
                measured_node.exact_height.is_some(),
                "search layout requested before height measurement"
            );
            let Some(exact_height) = measured_node.exact_height else {
                continue;
            };
            let gap = (self
                .render_plan
                .rendered_node_gap(history, i, exact_height as usize)
                as RowIndex)
                .min(exact_height);
            running_total = running_total.saturating_add(gap);
            let first_row = running_total;
            let rows = exact_height.saturating_sub(gap);
            let block_ids = match self.render_plan.node(i) {
                Some(RenderNode::Block { id, .. }) => vec![*id],
                Some(RenderNode::Group { child_ids, .. }) => child_ids.clone(),
                None => Vec::new(),
            };
            if !block_ids.is_empty() {
                entries.push(TranscriptSearchLayoutEntry {
                    id: measured_node.id,
                    key: measured_node.key,
                    block_ids,
                    first_row,
                    rows,
                });
            }
            running_total = running_total.saturating_add(rows);
        }
        TranscriptSearchLayout {
            generation,
            entries,
        }
    }

    pub(crate) fn exact_total_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
    ) -> RowIndex {
        self.rebuild_row_index(lua, history, width);
        self.measurements.active.total_rows()
    }

    pub(crate) fn node_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        row: RowIndex,
    ) -> Option<TranscriptNodeRow> {
        self.rebuild_row_index(lua, history, width);
        self.node_at_prepared_row(history, row)
    }

    fn node_at_prepared_row(
        &self,
        history: &BlockHistory,
        row: RowIndex,
    ) -> Option<TranscriptNodeRow> {
        let index = self.measurements.active.node_index_at_row(row)?;
        let node = self.measurements.active.nodes.get(index)?;
        let first_row = self.measurements.active.prefix_row(index);
        let rows = node.exact_height.unwrap_or(node.estimated_height);
        let row_offset = row.saturating_sub(first_row).min(rows.saturating_sub(1));
        let view_state = self.presentation.effective_view_state(
            &self.default_view_policy,
            &self.render_plan,
            history,
            index,
        )?;
        let explicit_fold_target = is_explicit_fold_target(view_state, row_offset, rows);
        Some(TranscriptNodeRow {
            id: node.id,
            index,
            first_row,
            rows,
            row_offset,
            view_state,
            explicit_fold_target,
        })
    }

    pub(crate) fn fold_node_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        request: FoldAtRow,
    ) -> bool {
        let Some(node) = self.node_at_row(lua, history, width, request.row) else {
            return false;
        };
        if !node.can_activate(request.activation) {
            return false;
        }
        self.fold_node(history, node.id, request.action)
    }

    pub(crate) fn fold_node(
        &mut self,
        history: &BlockHistory,
        id: RenderNodeId,
        action: FoldAction,
    ) -> bool {
        let changed = match action {
            FoldAction::Toggle => {
                self.presentation
                    .toggle(&self.default_view_policy, &self.render_plan, history, id)
            }
            FoldAction::Open => self.presentation.set(
                &self.default_view_policy,
                &self.render_plan,
                history,
                id,
                ViewState::Expanded,
            ),
            FoldAction::Peek => self.presentation.set(
                &self.default_view_policy,
                &self.render_plan,
                history,
                id,
                ViewState::Peek,
            ),
            FoldAction::Close => self.presentation.set(
                &self.default_view_policy,
                &self.render_plan,
                history,
                id,
                ViewState::Collapsed,
            ),
        };
        if changed {
            self.clear_visible_state();
            self.visible.full_rows = None;
        }
        changed
    }

    pub(crate) fn fold_all(&mut self, history: &BlockHistory, action: FoldAction) -> bool {
        let view_state = match action {
            FoldAction::Open => ViewState::Expanded,
            FoldAction::Peek => ViewState::Peek,
            FoldAction::Close => ViewState::Collapsed,
            FoldAction::Toggle => return false,
        };
        let changed = self.presentation.set_all(
            &self.default_view_policy,
            &self.render_plan,
            history,
            view_state,
        );
        if changed {
            self.clear_visible_state();
            self.visible.full_rows = None;
        }
        changed
    }

    pub(crate) fn fold_block_kind(
        &mut self,
        history: &BlockHistory,
        kind: &str,
        action: FoldAction,
    ) -> bool {
        let targets: Vec<_> = self
            .render_plan
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| match node {
                RenderNode::Block { id, .. } => history
                    .blocks
                    .get(id)
                    .filter(|block| block.kind() == kind)
                    .map(|_| (index, RenderNodeId::Block(*id))),
                RenderNode::Group { .. } => None,
            })
            .collect();
        if targets.is_empty() {
            return false;
        }
        let view_state = match action {
            FoldAction::Open => ViewState::Expanded,
            FoldAction::Peek => ViewState::Peek,
            FoldAction::Close => ViewState::Collapsed,
            FoldAction::Toggle => {
                let any_folded = targets.iter().any(|(index, _)| {
                    !matches!(
                        self.presentation.effective_view_state(
                            &self.default_view_policy,
                            &self.render_plan,
                            history,
                            *index,
                        ),
                        Some(ViewState::Expanded)
                    )
                });
                if any_folded {
                    ViewState::Expanded
                } else {
                    ViewState::Collapsed
                }
            }
        };
        let mut changed = false;
        for (_, id) in targets {
            changed |= self.presentation.set(
                &self.default_view_policy,
                &self.render_plan,
                history,
                id,
                view_state,
            );
        }
        if changed {
            self.clear_visible_state();
            self.visible.full_rows = None;
        }
        changed
    }

    pub(crate) fn node_metadata_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        row: RowIndex,
    ) -> Option<TranscriptNodeRow> {
        self.node_at_row(lua, history, width, row)
    }

    fn resize_anchor_for(&self, width: u16, scroll_target: ScrollTarget) -> Option<ResizeAnchor> {
        let row = scroll_target.visible_row_anchor()?;
        let width_changed = self
            .last_project_key()
            .map(|prev| prev.width != width)
            .unwrap_or(false);
        if !width_changed {
            return None;
        }
        let rendered = row
            .checked_sub(self.visible.row_base)
            .and_then(|local| self.visible.row_anchors.get(local as usize))
            .and_then(|anchor| *anchor);
        if let Some(anchor) = rendered {
            return Some(ResizeAnchor {
                id: anchor.id,
                row_offset: anchor.row_offset,
                display_offset: Some(anchor.display_offset),
            });
        }
        let (id, row_offset) = self.block_anchor_at(row)?;
        Some(ResizeAnchor {
            id,
            row_offset,
            display_offset: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plan_projection_measured(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> ProjectionPlan {
        let _perf = smelt_perf::perf::begin("transcript:plan_projection_measured");
        let resize_anchor = self.resize_anchor_for(width, scroll_target);
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.rebuild_row_index_with_env(env.clone(), history, width);
        let key = ProjectKey {
            generation: self.render_plan.fingerprint,
            width,
            renderer_generation: env.renderer_generation,
            renderer_cache_key: env.renderer_cache_key,
            presentation_generation: self.presentation.generation(),
            mode: scroll_target.mode(viewport_rows),
        };
        self.plan_projection_from_prepared(
            history,
            key,
            scroll_target,
            viewport_rows,
            theme,
            resize_anchor,
        )
    }

    fn scroll_top_for_resize_anchor(
        &self,
        history: &BlockHistory,
        theme: &Theme,
        anchor: Option<ResizeAnchor>,
    ) -> Option<RowIndex> {
        let anchor = anchor?;
        let index = self.measurements.active.block_index(anchor.id)?;
        let node = self.measurements.active.nodes.get(index)?;
        let exact_height = node.exact_height?;
        let gap = (self
            .render_plan
            .rendered_node_gap(history, index, exact_height as usize)
            as RowIndex)
            .min(exact_height);
        let block_rows = exact_height.saturating_sub(gap);
        let offset = anchor
            .display_offset
            .and_then(|display_offset| {
                let (block_buf, rendered_rows) = render_cached_layout_to_buffer(
                    &self.display_layouts,
                    node.id,
                    node.key,
                    self.measurements.active.renderer_generation,
                    self.measurements.active.renderer_cache_key,
                    theme,
                    history,
                    &self.inline_options,
                )?;
                Some(row_offset_for_display_offset(
                    &block_buf,
                    rendered_rows,
                    display_offset,
                    anchor.row_offset,
                ))
            })
            .unwrap_or(anchor.row_offset);
        Some(
            self.measurements
                .active
                .prefix_row(index)
                .saturating_add(gap)
                .saturating_add(offset.min(block_rows.saturating_sub(1))),
        )
    }

    fn plan_projection_from_prepared(
        &self,
        history: &BlockHistory,
        key: ProjectKey,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
        theme: &Theme,
        resize_anchor: Option<ResizeAnchor>,
    ) -> ProjectionPlan {
        let total_rows = self.measurements.active.total_rows();
        let requested_scroll_top = self
            .scroll_top_for_resize_anchor(history, theme, resize_anchor)
            .unwrap_or_else(|| scroll_target.as_scroll_top());
        let scroll_top = clamp_scroll(requested_scroll_top, total_rows, viewport_rows);
        let visible_rows = viewport_rows.max(1) as RowIndex;
        let viewport_end = scroll_top.saturating_add(visible_rows).min(total_rows);
        // Exact row heights make the visible window precise; keep half a viewport
        // preloaded so nearby scrolls can reuse the materialized buffer.
        let preload_rows = visible_rows / 2;
        let row_window = match scroll_target {
            ScrollTarget::Visible(ScrollAnchor::Row(_)) => {
                let start = scroll_top.saturating_sub(preload_rows);
                let end = viewport_end.saturating_add(preload_rows).min(total_rows);
                start..end
            }
            ScrollTarget::Visible(ScrollAnchor::Tail) => {
                let start = scroll_top.saturating_sub(preload_rows);
                start..total_rows
            }
        };
        let node_range = self.measurements.active.node_range_for_rows(row_window);
        ProjectionPlan {
            key,
            scroll_target,
            scroll_top,
            viewport_rows,
            node_range,
        }
    }

    /// Render a bounded row window into `buf`.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn project(
        &mut self,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> MaterializedRows {
        let lua = smelt_core::lua::runtime::LuaRuntime::new();
        let plan = self.plan_projection_measured(
            &lua,
            history,
            width,
            theme,
            scroll_target,
            viewport_rows,
        );
        self.project_planned(&lua, buf, history, theme, plan)
    }

    pub(crate) fn project_planned(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        theme: &Theme,
        plan: ProjectionPlan,
    ) -> MaterializedRows {
        let _perf = smelt_perf::perf::begin("transcript:project_planned");
        let mut plan = plan;
        let current_env =
            TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        if self.render_plan.history_generation != history.generation()
            || self.render_plan.group_generation != lua.transcript_group_generation()
            || self.render_plan.group_cache_key != lua.transcript_group_cache_key()
            || self.render_plan.fingerprint != plan.key.generation
            || current_env.renderer_generation != plan.key.renderer_generation
            || current_env.renderer_cache_key != plan.key.renderer_cache_key
            || self.presentation.generation() != plan.key.presentation_generation
        {
            self.rebuild_row_index_with_env(current_env.clone(), history, plan.key.width);
            let key = ProjectKey {
                generation: self.render_plan.fingerprint,
                width: plan.key.width,
                renderer_generation: current_env.renderer_generation,
                renderer_cache_key: current_env.renderer_cache_key,
                presentation_generation: self.presentation.generation(),
                mode: plan.scroll_target.mode(plan.viewport_rows),
            };
            plan = self.plan_projection_from_prepared(
                history,
                key,
                plan.scroll_target,
                plan.viewport_rows,
                theme,
                None,
            );
        }

        let row = plan.scroll_top;
        if let Some(out) =
            self.reuse_visible_projection_for_row(buf, plan.key, row, plan.viewport_rows)
        {
            return out;
        }

        match plan.scroll_target {
            ScrollTarget::Visible(_) => {
                let out = self.project_visible_range(lua, buf, history, theme, &plan);
                debug_assert_materialized_viewport(out, plan.viewport_rows);
                out
            }
        }
    }

    fn reuse_visible_projection_for_row(
        &self,
        buf: &Buffer,
        key: ProjectKey,
        row: RowIndex,
        viewport_rows: u16,
    ) -> Option<MaterializedRows> {
        let prev = self.last_project_key()?;
        if prev.generation != key.generation
            || prev.width != key.width
            || prev.renderer_generation != key.renderer_generation
            || prev.renderer_cache_key != key.renderer_cache_key
            || prev.presentation_generation != key.presentation_generation
        {
            return None;
        }

        if !self.target_has_projection(prev, buf) {
            return None;
        }

        let total_rows = self.visible.total_rows;
        let clamped_scroll = clamp_scroll(row, total_rows, viewport_rows);
        let materialized_end = self
            .visible
            .row_base
            .saturating_add(buf.line_count() as RowIndex);
        let viewport_end = clamped_scroll.saturating_add(viewport_rows as RowIndex);
        if clamped_scroll >= self.visible.row_base && viewport_end <= materialized_end {
            return Some(MaterializedRows {
                clamped_scroll,
                row_base: self.visible.row_base,
                total_rows,
                materialized_rows: buf.line_count() as RowIndex,
            });
        }
        None
    }

    fn project_visible_range(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        theme: &Theme,
        plan: &ProjectionPlan,
    ) -> MaterializedRows {
        let _perf = smelt_perf::perf::begin("transcript:project_visible_range");
        smelt_perf::perf::record_value(
            "transcript:project_visible_range:blocks",
            plan.node_range.len() as u64,
        );
        let materialized = self.collect_nodes_range(
            TranscriptRenderEnv::with_renderer(
                lua,
                plan.key.renderer_generation,
                plan.key.renderer_cache_key,
            ),
            history,
            theme,
            plan.node_range(),
        );
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
        self.visible.block_layout = materialized.layout;
        self.visible.row_anchors = materialized.row_anchors;
        self.visible.row_base = row_base;
        self.visible.total_rows = total_rows;
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

    fn collect_nodes_range(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        theme: &Theme,
        node_range: std::ops::Range<usize>,
    ) -> MaterializedTranscriptRange {
        let _perf = smelt_perf::perf::begin("transcript:collect_nodes_range");
        let start = node_range.start.min(self.measurements.active.nodes.len());
        let end = node_range.end.min(self.measurements.active.nodes.len());
        smelt_perf::perf::record_value(
            "transcript:collect_nodes_range:blocks",
            end.saturating_sub(start) as u64,
        );
        #[cfg(test)]
        {
            self.counters.range_materialized_blocks += end.saturating_sub(start);
        }
        let row_base = self.measurements.active.prefix_row(start);
        let mut texts = Vec::new();
        let mut pending = Vec::new();
        let mut layout = Vec::with_capacity(end.saturating_sub(start));
        let mut row_anchors = Vec::new();
        let mut rows = ProjectRows {
            row_base,
            texts: &mut texts,
            pending: &mut pending,
            layout: &mut layout,
            row_anchors: &mut row_anchors,
        };

        let block_indices = start..end;
        self.ensure_node_indices(env, history, block_indices.clone());
        for block_index in block_indices {
            let id = self.measurements.active.nodes[block_index].id;
            let key = self.measurements.active.nodes[block_index].key;
            self.append_projected_node(history, theme, block_index, id, key, &mut rows);
        }

        self.measurements.active.refresh_prefix_rows();
        MaterializedTranscriptRange {
            row_base,
            total_rows: self.measurements.active.total_rows(),
            texts,
            pending,
            layout,
            row_anchors,
        }
    }

    fn append_projected_node(
        &mut self,
        history: &BlockHistory,
        theme: &Theme,
        block_index: usize,
        id: RenderNodeId,
        key: NodeLayoutKey,
        rows: &mut ProjectRows<'_>,
    ) {
        let renderer_generation = self.measurements.active.renderer_generation;
        let renderer_cache_key = self.measurements.active.renderer_cache_key;
        let Some((node_buf, node_rows)) = render_cached_layout_to_buffer(
            &self.display_layouts,
            id,
            key,
            renderer_generation,
            renderer_cache_key,
            theme,
            history,
            &self.inline_options,
        ) else {
            return;
        };
        let gap = self
            .render_plan
            .rendered_node_gap(history, block_index, node_rows);
        self.set_exact_height(
            block_index,
            (gap as usize).saturating_add(node_rows) as RowIndex,
        );
        for _ in 0..gap {
            rows.texts.push(String::new());
            rows.row_anchors.push(None);
        }

        let block_id = id.as_block_id();
        let block_anchors = block_id.map(|id| rendered_row_anchors(&node_buf, id, node_rows));
        let local_start = rows.texts.len() as RowIndex;
        for r in 0..node_rows {
            let row_idx = rows.texts.len();
            rows.texts
                .push(node_buf.get_line(r).unwrap_or("").to_string());
            let h = node_buf.highlights_at(r);
            let dec = node_buf.decoration_at(r).clone();
            if !h.is_empty() || dec != LineDecoration::default() {
                rows.pending.push(PendingRow {
                    row: row_idx,
                    highlights: h,
                    decoration: dec,
                });
            }
            rows.row_anchors
                .push(block_anchors.as_ref().map(|anchors| anchors[r]));
        }
        if let Some(block_id) = block_id {
            rows.layout.push(LayoutEntry {
                id: block_id,
                start: rows.row_base.saturating_add(local_start),
                rows: node_rows as RowIndex,
            });
        }
    }

    /// Map an absolute row to its `(BlockId, row_offset_within_block)`. Gap
    /// rows resolve to the previous block's last row so a scroll position
    /// stranded in a gap still anchors to a stable block boundary. Tail targets
    /// beyond the end of all blocks return `None` so the caller falls back to
    /// scroll_top and the natural clamp pins the viewport to the new bottom.
    fn block_anchor_at(&self, row: RowIndex) -> Option<(BlockId, RowIndex)> {
        let last = self.visible.block_layout.last()?;
        let last_end = last.start.saturating_add(last.rows);
        if row >= last_end {
            return None;
        }
        let idx = self
            .visible
            .block_layout
            .partition_point(|e| e.start <= row);
        if idx == 0 {
            return None;
        }
        let entry = self.visible.block_layout[idx - 1];
        let end = entry.start.saturating_add(entry.rows);
        let offset = if row < end {
            row - entry.start
        } else {
            entry.rows.saturating_sub(1)
        };
        Some((entry.id, offset))
    }

    /// Exact full block layout for snapshot/navigation APIs. This may measure every
    /// transcript block, but it does not concatenate display rows and does not
    /// re-render blocks when the exact height index is already current.
    pub(crate) fn materialize_block_layout(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
    ) -> Vec<(BlockId, RowIndex, RowIndex)> {
        self.rebuild_row_index(lua, history, width);
        self.exact_block_layout(history)
            .into_iter()
            .map(|e| (e.id, e.start, e.rows))
            .collect()
    }

    pub(crate) fn materialize_search_layout(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
    ) -> TranscriptSearchLayout {
        self.rebuild_row_index(lua, history, width);
        let generation = self.measurements.active.semantic_layout_hash();
        self.exact_search_layout(generation, history)
    }

    /// Render the full transcript history into a regular buffer. This is for
    /// small transcript-shaped surfaces outside the main transcript viewport
    /// (for example a streaming `/btw` dialog) that still want the exact same
    /// block parser, markdown layout, and highlights.
    pub(crate) fn project_all(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
    ) {
        self.rebuild_row_index(lua, history, width);
        let materialized = self.collect_nodes_range(
            TranscriptRenderEnv::with_renderer(
                lua,
                self.measurements.active.renderer_generation,
                self.measurements.active.renderer_cache_key,
            ),
            history,
            theme,
            0..self.measurements.active.nodes.len(),
        );
        buf.set_all_lines(materialized.texts);
        for p in materialized.pending {
            apply_row_highlights(buf, p.row, p.highlights);
        }
    }

    /// Full display rows. Cached by transcript, renderer, width, and presentation;
    /// callers get a free `Arc::clone`.
    pub(crate) fn build_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
    ) -> Arc<Vec<String>> {
        let _perf = smelt_perf::perf::begin("transcript:build_rows");
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        let renderer_generation = env.renderer_generation;
        let renderer_cache_key = env.renderer_cache_key;
        self.invalidate_renderer_if_changed(renderer_generation, renderer_cache_key);
        self.gc_if_stale(env.lua, history, width);
        let gen = self.render_plan.fingerprint;
        if let Some(c) = &self.visible.full_rows {
            if c.generation == gen
                && c.renderer_generation == renderer_generation
                && c.renderer_cache_key == renderer_cache_key
                && c.presentation_generation == self.presentation.generation()
                && c.width == width
            {
                return Arc::clone(&c.rows);
            }
        }
        #[cfg(test)]
        {
            self.counters.full_row_builds += 1;
        }
        smelt_perf::perf::record_value("transcript:build_rows:blocks", history.order.len() as u64);
        let plan = self.render_plan.clone();
        let row_key = RowIndexKey::new(
            width,
            renderer_generation,
            renderer_cache_key,
            self.presentation.generation(),
        );
        let hydrated_index = self.try_hydrate_row_index(history, &plan, row_key);
        let reused_index = hydrated_index
            || self.measurements.active.sync_stable_order_prefix(
                history,
                &plan,
                &self.default_view_policy,
                &self.presentation,
                row_key,
            );
        if !reused_index {
            self.measurements.active.rebuild_if_stale(
                history,
                &plan,
                &self.default_view_policy,
                &self.presentation,
                row_key,
            );
        }
        let mut rows: Vec<String> = Vec::new();
        let block_indices = 0..self.measurements.active.nodes.len();
        self.ensure_node_indices(env, history, block_indices.clone());
        for i in block_indices {
            let Some(node) = self.measurements.active.nodes.get(i) else {
                continue;
            };
            let id = node.id;
            let bkey = node.key;
            let Some((block_buf, block_rows)) = render_cached_layout_to_buffer(
                &self.display_layouts,
                id,
                bkey,
                renderer_generation,
                renderer_cache_key,
                theme,
                history,
                &self.inline_options,
            ) else {
                continue;
            };
            let gap = self.render_plan.rendered_node_gap(history, i, block_rows);
            self.set_exact_height(i, (gap as usize).saturating_add(block_rows) as RowIndex);
            for _ in 0..gap {
                rows.push(String::new());
            }
            for r in 0..block_rows {
                rows.push(block_buf.get_line(r).unwrap_or("").to_string());
            }
        }
        self.measurements.active.refresh_prefix_rows();
        self.measurements.remember_active();
        let rows = Arc::new(rows);
        self.visible.full_rows = Some(CachedRows {
            rows: Arc::clone(&rows),
            generation: gen,
            renderer_generation,
            renderer_cache_key,
            presentation_generation: self.presentation.generation(),
            width,
        });
        rows
    }

    pub(crate) fn display_rows_for_range(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        rows: std::ops::Range<RowIndex>,
    ) -> DisplayRows {
        let _perf = smelt_perf::perf::begin("transcript:display_rows_for_range");
        let start = rows.start;
        let end = rows.end;
        let count = end.saturating_sub(start);
        smelt_perf::perf::record_value("transcript:display_rows_for_range:rows", count);
        if count == 0 || end <= start {
            return DisplayRows::empty();
        }

        self.rebuild_row_index(lua, history, width);
        let total_rows = self.measurements.active.total_rows();
        if total_rows == 0 || start >= total_rows {
            return DisplayRows::empty();
        }
        let end = end.min(total_rows);
        let node_range = self.measurements.active.node_range_for_rows(start..end);
        if node_range.start >= node_range.end {
            return DisplayRows::empty();
        }

        let materialized = self.collect_nodes_range(
            TranscriptRenderEnv::with_renderer(
                lua,
                self.measurements.active.renderer_generation,
                self.measurements.active.renderer_cache_key,
            ),
            history,
            theme,
            node_range,
        );
        let local_start = row_to_usize(start.saturating_sub(materialized.row_base));
        let local_end =
            row_to_usize(end.saturating_sub(materialized.row_base)).min(materialized.texts.len());
        if local_start >= local_end {
            return DisplayRows::empty();
        }
        let mut soft_wrapped = vec![false; materialized.texts.len()];
        let mut actions = vec![Vec::new(); materialized.texts.len()];
        let mut selectable_ranges: Vec<Vec<std::ops::Range<usize>>> = materialized
            .texts
            .iter()
            .map(|row| {
                if row.is_empty() {
                    Vec::new()
                } else {
                    std::iter::once(0..row.len()).collect()
                }
            })
            .collect();
        for p in &materialized.pending {
            if let Some(slot) = soft_wrapped.get_mut(p.row) {
                *slot = p.decoration.soft_wrapped;
            }
            if let Some(slot) = actions.get_mut(p.row) {
                *slot = crate::smelt_edit::display_actions_for_spans(&p.highlights);
            }
            if let (Some(row), Some(slot)) = (
                materialized.texts.get(p.row),
                selectable_ranges.get_mut(p.row),
            ) {
                *slot = crate::smelt_edit::selectable_byte_ranges_for_line(row, &p.highlights);
            }
        }
        let rows = materialized.texts[local_start..local_end]
            .iter()
            .cloned()
            .zip(selectable_ranges[local_start..local_end].iter().cloned())
            .zip(actions[local_start..local_end].iter().cloned())
            .enumerate()
            .map(|(offset, ((text, selectable_ranges), actions))| {
                let row = DisplayRow::new(text, selectable_ranges).with_actions(actions);
                if offset == 0 {
                    row
                } else if soft_wrapped[local_start + offset] {
                    row.with_break_before(RowBreak::Soft)
                } else {
                    row.with_break_before(RowBreak::Hard)
                }
            })
            .collect();
        DisplayRows { rows }
    }

    pub(crate) fn copy_range(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        range: DocRange,
    ) -> CopyOutput {
        let _perf = smelt_perf::perf::begin("transcript:copy_range");
        if (range.start.row, range.start.byte_col) >= (range.end.row, range.end.byte_col) {
            return CopyOutput::default();
        }
        self.rebuild_row_index(lua, history, width);
        let total_rows = self.measurements.active.total_rows();
        if total_rows == 0 || range.start.row >= total_rows {
            return CopyOutput::default();
        }
        let end_row = range.end.row.min(total_rows.saturating_sub(1));
        let node_range = self
            .measurements
            .active
            .node_range_for_rows(range.start.row..end_row.saturating_add(1));
        if node_range.start >= node_range.end {
            return CopyOutput::default();
        }

        let mut scratch = Buffer::new(BufId(0), BufCreateOpts::default());
        let materialized = self.collect_nodes_range(
            TranscriptRenderEnv::with_renderer(
                lua,
                self.measurements.active.renderer_generation,
                self.measurements.active.renderer_cache_key,
            ),
            history,
            theme,
            node_range,
        );
        let row_base = materialized.row_base;
        scratch.set_all_lines(materialized.texts);
        for p in materialized.pending {
            apply_row_highlights(&mut scratch, p.row, p.highlights);
            if p.decoration != LineDecoration::default() {
                scratch.set_decoration(p.row, p.decoration);
            }
        }

        let start_local = row_to_usize(range.start.row.saturating_sub(row_base));
        let end_local = row_to_usize(range.end.row.saturating_sub(row_base));
        let start = scratch.byte_at_display_byte_pos(start_local, range.start.byte_col);
        let end = scratch.byte_at_display_byte_pos(end_local, range.end.byte_col);
        let clipboard = copy_byte_range(&scratch, start, end);
        let raw = smelt_buffer::text::slice(&scratch.text(), start..end).to_string();
        CopyOutput {
            kill_ring: raw,
            clipboard,
        }
    }
}

fn debug_assert_materialized_viewport(rows: MaterializedRows, viewport_rows: u16) {
    if viewport_rows == 0 {
        debug_assert!(
            rows.clamped_scroll <= rows.total_rows,
            "empty viewport scroll {} exceeds total rows {}",
            rows.clamped_scroll,
            rows.total_rows
        );
        return;
    }
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
        let raw = smelt_buffer::text::slice(src, range.start..range.end).to_string();
        let clipboard = copy_byte_range(buf, range.start, range.end);
        smelt_core::buffer::CopyOutput {
            kill_ring: raw,
            clipboard,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::content::stream_parser::StreamParser;
    use smelt_core::content::transcript::Transcript;
    use smelt_core::transcript_model::{Block, BlockHistory, ToolOutput, ToolState, ToolStatus};

    fn test_lua() -> smelt_core::lua::runtime::LuaRuntime {
        smelt_core::lua::runtime::LuaRuntime::new()
    }

    fn register_terminal_tool_group(lua: &smelt_core::lua::runtime::LuaRuntime, min: usize) {
        let chunk = r#"
            local defaults = require("smelt.transcript.defaults")
            smelt.transcript.groups.register({
              name = "terminal-tools",
              selector = { kind = "tool", terminal = true },
              min = __MIN__,
              default_view = "expanded",
              cache_key = "test.terminal-tools:v1",
              render = function(group, ctx)
                return smelt.layout.vbox({
                  smelt.layout.text("group:" .. group.name .. ":" .. tostring(group.child_count) .. ":" .. group.view_state),
                  defaults.render_group_child_list(group, ctx, { field = "call_id" }),
                })
              end,
            })
        "#
        .replace("__MIN__", &min.to_string());
        lua.lua.load(chunk.as_str()).exec().expect("register group");
    }

    fn push_tool(
        parser: &mut StreamParser,
        history: &mut BlockHistory,
        call_id: &str,
        summary: &str,
        status: ToolStatus,
    ) {
        parser.start_tool(
            history,
            call_id.into(),
            "bash".into(),
            protocol::StyledLines::from_plain(summary),
            std::collections::HashMap::new(),
            std::time::Instant::now(),
        );
        parser.set_active_status(history, call_id, status, std::time::Instant::now());
    }

    fn push_named_tool(
        transcript: &mut Transcript,
        call_id: &str,
        name: &str,
        summary: &str,
        status: ToolStatus,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        let is_error = matches!(status, ToolStatus::Err);
        transcript.push_tool_call(
            Block::ToolCall {
                call_id: call_id.into(),
                name: name.into(),
                summary: protocol::StyledLines::from_plain(summary),
                args,
            },
            ToolState {
                status,
                elapsed: Some(std::time::Duration::from_millis(25)),
                output: Some(Box::new(ToolOutput {
                    content: format!("{summary} output"),
                    is_error,
                    metadata: None,
                })),
                user_message: None,
            },
        );
    }

    fn tool_args(entries: &[(&str, &str)]) -> std::collections::HashMap<String, serde_json::Value> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), serde_json::json!(*value)))
            .collect()
    }

    fn install_read_file_renderer(lua: &smelt_core::lua::runtime::LuaRuntime) {
        lua.lua
            .load(
                r#"
                smelt.text = smelt.text or {}
                smelt.text.line_count = smelt.text.line_count or function(content)
                  if content == nil or content == "" then return 0 end
                  local _, n = tostring(content):gsub("\n", "")
                  return n + 1
                end
                require('smelt.transcript')
                require('smelt.tools.read_file')
                assert(smelt.transcript.defaults.__tool_collapsed_details.read_file ~= nil)
                assert(smelt.transcript.defaults.__tool_collapsed_details.read_file({ output = { content = "x" } }) == "1 lines")
                "#,
            )
            .exec()
            .expect("load read_file renderer");
    }

    #[test]
    fn read_file_summary_shows_default_limit_only_when_reached() {
        let lua = test_lua();
        install_read_file_renderer(&lua);
        lua.lua
            .load(
                r#"
                local short = "one\ntwo"
                local full = string.rep("line\n", 1999) .. "line"
                assert(smelt.tools.read_file_summary({ file_path = "a.rs" }, short) == "a.rs")
                assert(smelt.tools.read_file_summary({ file_path = "a.rs" }, full) == "a.rs:1-2000")
                assert(smelt.tools.read_file_summary({ file_path = "a.rs", offset = 120 }, short) == "a.rs:120")
                assert(smelt.tools.read_file_summary({ file_path = "a.rs", offset = 120, limit = 40 }, short) == "a.rs:120-159")
                "#,
            )
            .exec()
            .expect("assert read_file summary ranges");
    }

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
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        assert!(buf.line_count() > 0);
        assert_eq!(buf.get_line(buf.line_count() - 1), Some("hello"));
    }

    #[test]
    fn fold_override_collapses_block_without_mutating_semantic_layout_key() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "one\ntwo\nthree".into(),
        });
        let id = transcript.history.order[0];
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(77), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        assert!(buf.lines().iter().any(|line| line == "three"));

        assert!(projection.fold_node_at_row(
            &lua,
            &mut transcript.history,
            80,
            FoldAtRow {
                row: 0,
                action: FoldAction::Close,
                activation: FoldActivation::AnyNodeRow,
            },
        ));
        let history_key = transcript.history.resolve_key(id, base_layout_key(80));
        assert_eq!(history_key.view_state, ViewState::Expanded);

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        assert_eq!(buf.line_count(), 3);
        assert!(!buf.lines().iter().any(|line| line.contains("more lines")));
        let node = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, 1)
            .expect("collapsed block row");
        assert!(node.explicit_fold_target);
    }

    #[test]
    fn collapsed_group_renderer_owns_its_rendered_rows() {
        let lua = test_lua();
        lua.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "custom-read-group",
                  priority = 10,
                  selector = { kind = "tool", name = "read_file", terminal = true },
                  min = 2,
                  default_view = "collapsed",
                  cache_key = "test.custom-read-group:v1",
                  render = function(group, ctx)
                    return smelt.layout.vbox({
                      smelt.layout.text("custom summary " .. group.view_state),
                      smelt.layout.text("custom detail")
                    })
                  end,
                })
                "#,
            )
            .exec()
            .expect("register custom group");
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "a.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "a.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "b.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "b.rs")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(rows.iter().any(|row| row == "custom summary collapsed"));
        assert!(rows.iter().any(|row| row == "custom detail"));
        assert!(!rows.iter().any(|row| row.contains("more lines")));
    }

    #[test]
    fn thinking_blocks_default_peek_and_can_expand() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::Thinking {
            content: "first thought\nsecond thought\nthird thought\nfourth thought\nfifth thought"
                .into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(78), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        assert!(buf
            .lines()
            .iter()
            .any(|line| line.contains("1 line omitted")));
        assert!(!buf
            .lines()
            .iter()
            .any(|line| line.contains("second thought")));

        assert!(projection.fold_node_at_row(
            &lua,
            &mut transcript.history,
            80,
            FoldAtRow {
                row: 1,
                action: FoldAction::Open,
                activation: FoldActivation::ExplicitTargetOnly,
            },
        ));
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        assert!(buf
            .lines()
            .iter()
            .any(|line| line.contains("second thought")));

        assert!(projection.fold_node_at_row(
            &lua,
            &mut transcript.history,
            80,
            FoldAtRow {
                row: 1,
                action: FoldAction::Toggle,
                activation: FoldActivation::AnyNodeRow,
            },
        ));
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        assert!(buf
            .lines()
            .iter()
            .any(|line| line.contains("1 line omitted")));
        assert!(!buf
            .lines()
            .iter()
            .any(|line| line.contains("second thought")));
    }

    #[test]
    fn fold_kind_toggle_uses_aggregate_state() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::Thinking {
            content: "a\nb".into(),
        });
        transcript.push(Block::Thinking {
            content: "c\nd".into(),
        });
        let mut projection = TranscriptProjection::new();
        projection.rebuild_row_index(&lua, &mut transcript.history, 80);

        assert!(projection.fold_block_kind(&transcript.history, "thinking", FoldAction::Toggle));
        let first = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, 0)
            .expect("first thinking node");
        let second = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, first.rows)
            .expect("second thinking node");
        assert_eq!(first.view_state, ViewState::Expanded);
        assert_eq!(second.view_state, ViewState::Expanded);

        assert!(projection.fold_node(&transcript.history, first.id, FoldAction::Close));
        assert!(projection.fold_block_kind(&transcript.history, "thinking", FoldAction::Toggle));
        let first = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, 0)
            .expect("first thinking node");
        let second = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, first.rows)
            .expect("second thinking node");
        assert_eq!(first.view_state, ViewState::Expanded);
        assert_eq!(second.view_state, ViewState::Expanded);

        assert!(projection.fold_block_kind(&transcript.history, "thinking", FoldAction::Toggle));
        let first = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, 0)
            .expect("first thinking node");
        assert_eq!(first.view_state, ViewState::Collapsed);
    }

    #[test]
    fn explicit_fold_targets_are_limited_to_affordance_rows() {
        assert!(!is_explicit_fold_target(ViewState::Expanded, 0, 3));
        assert!(is_explicit_fold_target(ViewState::Collapsed, 0, 2));
        assert!(is_explicit_fold_target(ViewState::Collapsed, 1, 2));
        assert!(!is_explicit_fold_target(
            ViewState::TrimmedHead { keep: 2 },
            0,
            3
        ));
        assert!(is_explicit_fold_target(
            ViewState::TrimmedHead { keep: 2 },
            2,
            3
        ));
        assert!(is_explicit_fold_target(
            ViewState::TrimmedTail { keep: 2 },
            0,
            3
        ));
        assert!(!is_explicit_fold_target(
            ViewState::TrimmedTail { keep: 2 },
            1,
            3
        ));
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
    fn user_chrome_uses_indented_background_fill_not_fake_full_width_text() {
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "hello".into(),
            image_labels: vec![],
        });

        let rows = project_fresh(&mut transcript.history);

        assert_eq!(rows.first().map(|row| row.line.as_str()), Some(" "));
        assert_eq!(rows.last().map(|row| row.line.as_str()), Some(" "));
        assert!(rows
            .first()
            .is_some_and(|row| row.decoration.fill_bg.is_some()));
        assert!(rows
            .last()
            .is_some_and(|row| row.decoration.fill_bg.is_some()));
        assert!(rows.first().is_some_and(|row| {
            row.highlights
                .iter()
                .any(|span| span.col_start == 0 && span.col_end == 1 && !span.meta.selectable)
        }));
        assert!(rows.last().is_some_and(|row| {
            row.highlights
                .iter()
                .any(|span| span.col_start == 0 && span.col_end == 1 && !span.meta.selectable)
        }));
        assert!(rows.iter().any(|row| row.line == " hello"));
        assert!(!rows.iter().any(|row| row.line.len() >= 80));
    }

    #[test]
    fn planned_projection_rechecks_renderer_identity_before_rendering() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "before".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let plan = projection.plan_projection_measured(
            &lua,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            10,
        );
        lua.lua
            .load(
                r#"
                smelt.transcript.set_renderer(function(block, ctx)
                  local _ = block
                  local _ = ctx
                  return smelt.layout.text("after")
                end, { cache_key = "test.planned_projection.after:v1" })
                "#,
            )
            .exec()
            .expect("set renderer");
        let mut buf = Buffer::new(crate::smelt_edit::BufId(23), Default::default());

        projection.project_planned(&lua, &mut buf, &mut transcript.history, &theme, plan);

        assert!(buf.lines().iter().any(|line| line == "after"));
        assert!(!buf.lines().iter().any(|line| line == "before"));
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
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        parser.append_streaming_text(&mut transcript.history, " 1 |");
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
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
    fn lua_registered_groups_replace_adjacent_terminal_tools() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-1",
            "first",
            ToolStatus::Ok,
        );
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-2",
            "second failed",
            ToolStatus::Err,
        );
        transcript.push(Block::Text {
            content: "after".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let before_group = projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        assert!(before_group.iter().any(|line| line.contains("bash")));
        assert!(!before_group.iter().any(|line| line.starts_with("group:")));

        register_terminal_tool_group(&lua, 2);
        let grouped = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.render_plan.nodes.as_slice(),
            [crate::content::render_plan::RenderNode::Group { child_ids, .. }, crate::content::render_plan::RenderNode::Block { .. }] if child_ids.len() == 2
        ));
        assert!(grouped
            .iter()
            .any(|line| line == "group:terminal-tools:2:expanded"));
        assert!(
            grouped.iter().any(|line| line == "  call-1"),
            "grouped rows: {grouped:?}"
        );
        assert!(grouped.iter().any(|line| line == "  call-2"));
        assert!(grouped.iter().any(|line| line == "after"));
        assert!(!grouped.iter().any(|line| line.contains("bash")));
    }

    #[test]
    fn group_min_threshold_keeps_original_blocks() {
        let lua = test_lua();
        register_terminal_tool_group(&lua, 3);
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-1",
            "first",
            ToolStatus::Ok,
        );
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-2",
            "second",
            ToolStatus::Ok,
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert_eq!(projection.render_plan.nodes.len(), 2);
        assert!(projection
            .render_plan
            .nodes
            .iter()
            .all(|node| matches!(node, crate::content::render_plan::RenderNode::Block { .. })));
        assert!(!rows.iter().any(|line| line.starts_with("group:")));
        assert_eq!(rows.iter().filter(|line| line.contains("bash")).count(), 2);
    }

    #[test]
    fn non_matching_block_breaks_registered_groups() {
        let lua = test_lua();
        register_terminal_tool_group(&lua, 2);
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-1",
            "first",
            ToolStatus::Ok,
        );
        transcript.push(Block::Text {
            content: "between".into(),
        });
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-2",
            "second",
            ToolStatus::Ok,
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert_eq!(projection.render_plan.nodes.len(), 3);
        assert!(projection
            .render_plan
            .nodes
            .iter()
            .all(|node| matches!(node, crate::content::render_plan::RenderNode::Block { .. })));
        assert!(!rows.iter().any(|line| line.starts_with("group:")));
        assert!(rows.iter().any(|line| line == "between"));
    }

    #[test]
    fn group_without_cache_key_skips_persisted_display_cache() {
        let lua = test_lua();
        lua.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "uncached-tools",
                  selector = { kind = "tool", terminal = true },
                  min = 2,
                  render = function(group, ctx)
                    local _ = group
                    local _ = ctx
                    return smelt.layout.text("uncached group")
                  end,
                })
                "#,
            )
            .exec()
            .expect("register uncached group");
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-1",
            "first",
            ToolStatus::Ok,
        );
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-2",
            "second",
            ToolStatus::Ok,
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        let cache = projection.display_cache_data(&lua, &transcript.history);

        assert!(rows.iter().any(|line| line == "uncached group"));
        assert!(cache.row_indexes.is_empty());
        assert!(cache.display_layouts.is_empty());
    }

    #[test]
    fn grouped_node_gap_uses_semantic_child_boundary() {
        let lua = test_lua();
        lua.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "assistant-pair",
                  selector = { kind = "assistant" },
                  min = 2,
                  default_view = "expanded",
                  cache_key = "test.assistant-pair:v1",
                  render = function(group, ctx)
                    local _ = group
                    local _ = ctx
                    return smelt.layout.text("assistant group")
                  end,
                })
                "#,
            )
            .exec()
            .expect("register assistant group");
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        transcript.push(Block::Text {
            content: "## heading".into(),
        });
        transcript.push(Block::CodeLine {
            content: "x".into(),
            lang: "rust".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert_eq!(
            rows.as_ref(),
            &vec!["assistant group".to_string(), "x".to_string()]
        );
    }

    #[test]
    fn terminal_group_keeps_failed_child_in_order() {
        let lua = test_lua();
        register_terminal_tool_group(&lua, 2);
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-1",
            "ok child",
            ToolStatus::Ok,
        );
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-2",
            "failed child",
            ToolStatus::Err,
        );
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-3",
            "later ok",
            ToolStatus::Ok,
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.render_plan.nodes.as_slice(),
            [crate::content::render_plan::RenderNode::Group { child_ids, .. }] if child_ids.len() == 3
        ));
        let child_rows: Vec<_> = rows
            .iter()
            .filter(|line| line.starts_with("  "))
            .map(String::as_str)
            .collect();
        assert_eq!(child_rows, vec!["  call-1", "  call-2", "  call-3"]);
    }

    #[test]
    fn built_in_read_file_group_replaces_adjacent_terminal_calls() {
        let lua = test_lua();
        install_read_file_renderer(&lua);
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "crates/core/src/transcript_model.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "crates/core/src/transcript_model.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "crates/tui/src/content/display_layout.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "crates/tui/src/content/display_layout.rs")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.render_plan.nodes.as_slice(),
            [crate::content::render_plan::RenderNode::Group { name, child_ids, .. }] if name == "read_file_batch" && child_ids.len() == 2
        ));
        assert!(
            rows.iter().any(|line| line == "* read_file ×2"),
            "rows: {rows:?}"
        );
        assert!(rows
            .iter()
            .any(|line| line == "  crates/core/src/transcript_model.rs"));
        assert!(rows
            .iter()
            .any(|line| line == "  crates/tui/src/content/display_layout.rs"));

        let group_id = projection.render_plan.nodes[0].id();
        assert!(projection.fold_node(&transcript.history, group_id, FoldAction::Open));
        let expanded = projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        assert!(
            expanded.iter().any(|line| line.contains("* read_file")),
            "expanded rows: {expanded:?}"
        );
        assert!(
            expanded.iter().any(|line| line.trim() == "1 lines"),
            "expanded rows: {expanded:?}"
        );
        assert!(!expanded.iter().any(|line| line.contains(" output")));
        assert!(projection.fold_node_at_row(
            &lua,
            &mut transcript.history,
            80,
            FoldAtRow {
                row: 2,
                action: FoldAction::Toggle,
                activation: FoldActivation::AnyNodeRow,
            },
        ));
        let collapsed = projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        assert!(collapsed.iter().any(|line| line == "* read_file ×2"));
        assert!(!collapsed.iter().any(|line| line == "  2 lines"));
    }

    #[test]
    fn grouped_children_remain_in_semantic_block_layout() {
        let lua = test_lua();
        install_read_file_renderer(&lua);
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "crates/core/src/transcript_model.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "crates/core/src/transcript_model.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "crates/tui/src/content/display_layout.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "crates/tui/src/content/display_layout.rs")]),
        );
        let first = transcript.history.order[0];
        let second = transcript.history.order[1];
        let mut projection = TranscriptProjection::new();

        let layout = projection.materialize_block_layout(&lua, &mut transcript.history, 80);
        let search_layout = projection.materialize_search_layout(&lua, &mut transcript.history, 80);

        assert!(matches!(
            projection.render_plan.nodes.as_slice(),
            [crate::content::render_plan::RenderNode::Group { name, child_ids, .. }] if name == "read_file_batch" && child_ids.as_slice() == [first, second]
        ));
        assert_eq!(layout.len(), 2);
        assert_eq!(layout[0].0, first);
        assert_eq!(layout[1].0, second);
        assert_eq!(layout[0].1, layout[1].1);
        assert_eq!(layout[0].2, layout[1].2);
        assert!(layout[0].2 > 0);
        assert_eq!(search_layout.entries.len(), 1);
        assert_eq!(search_layout.entries[0].block_ids, vec![first, second]);
        assert_eq!(search_layout.entries[0].first_row, layout[0].1);
        assert_eq!(search_layout.entries[0].rows, layout[0].2);
    }

    #[test]
    fn built_in_grep_and_glob_groups_stay_separate() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "grep-1",
            "grep",
            "RenderNode",
            ToolStatus::Ok,
            tool_args(&[("pattern", "RenderNode")]),
        );
        push_named_tool(
            &mut transcript,
            "grep-2",
            "grep",
            "ViewState",
            ToolStatus::Ok,
            tool_args(&[("pattern", "ViewState")]),
        );
        transcript.push(Block::Text {
            content: "between".into(),
        });
        push_named_tool(
            &mut transcript,
            "glob-1",
            "glob",
            "**/*.rs",
            ToolStatus::Ok,
            tool_args(&[("pattern", "**/*.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "glob-2",
            "glob",
            "runtime/lua/**/*.lua",
            ToolStatus::Ok,
            tool_args(&[("pattern", "runtime/lua/**/*.lua")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.render_plan.nodes.as_slice(),
            [
                crate::content::render_plan::RenderNode::Group { name: first, child_ids: first_children, .. },
                crate::content::render_plan::RenderNode::Block { .. },
                crate::content::render_plan::RenderNode::Group { name: second, child_ids: second_children, .. },
            ] if first == "grep_batch" && first_children.len() == 2 && second == "glob_batch" && second_children.len() == 2
        ));
        assert!(rows.iter().any(|line| line == "* grep ×2"));
        assert!(rows.iter().any(|line| line == "  \"RenderNode\""));
        assert!(rows.iter().any(|line| line == "between"));
        assert!(rows.iter().any(|line| line == "* glob ×2"));
        assert!(rows.iter().any(|line| line == "  **/*.rs"));
    }

    #[test]
    fn built_in_tool_groups_include_pending_calls_without_mixing_tools() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "a.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "a.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "grep-1",
            "grep",
            "needle",
            ToolStatus::Ok,
            tool_args(&[("pattern", "needle")]),
        );
        push_named_tool(
            &mut transcript,
            "read-pending",
            "read_file",
            "b.rs",
            ToolStatus::Pending,
            tool_args(&[("file_path", "b.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "c.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "c.rs")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.render_plan.nodes.as_slice(),
            [
                crate::content::render_plan::RenderNode::Block { .. },
                crate::content::render_plan::RenderNode::Block { .. },
                crate::content::render_plan::RenderNode::Group { name, child_ids, .. },
            ] if name == "read_file_batch" && child_ids.len() == 2
        ));
        assert!(rows.iter().any(|line| line.starts_with("* read_file ×2")));
    }

    #[test]
    fn built_in_tool_group_summary_surfaces_errors_and_denials() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "ok.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "ok.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "err.rs",
            ToolStatus::Err,
            tool_args(&[("file_path", "err.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-3",
            "read_file",
            "denied.rs",
            ToolStatus::Denied,
            tool_args(&[("file_path", "denied.rs")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.render_plan.nodes.as_slice(),
            [crate::content::render_plan::RenderNode::Group { name, child_ids, .. }] if name == "read_file_batch" && child_ids.len() == 3
        ));
        assert!(
            rows.iter()
                .any(|line| line == "* read_file ×3 (1 error, 1 denied)"),
            "rows: {rows:?}"
        );
        assert!(rows.iter().any(|line| line == "  err.rs"));
    }

    #[test]
    fn built_in_background_process_completion_group_uses_typed_event_fields() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::ProcessStatus {
            text: "Background process 1 finished successfully.".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "1",
                Some(0),
            )),
        });
        transcript.push(Block::ProcessStatus {
            text: "Background process 2 exited with code 7.".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "2",
                Some(7),
            )),
        });
        transcript.push(Block::ProcessStatus {
            text: "legacy process note".into(),
            event: None,
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.render_plan.nodes.as_slice(),
            [
                crate::content::render_plan::RenderNode::Group { name, child_ids, .. },
                crate::content::render_plan::RenderNode::Block { .. },
            ] if name == "background_process_completed" && child_ids.len() == 2
        ));
        assert!(
            rows.iter()
                .any(|line| line
                    == "background processes finished: 2, 1 failed: 2 exited with code 7")
        );
        assert!(rows.iter().any(|line| line == "legacy process note"));
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

        let rows = projection.build_rows(&test_lua(), &mut transcript.history, 80, &theme);

        assert!(rows.iter().any(|line| line == "line 99"));
        assert!(rows.iter().any(|line| line == "line 0"));
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 1);
        assert_eq!(counters.display_layouts, 100);
        assert_eq!(counters.exact_height_measured_blocks, 100);

        projection.reset_counters();
        let cached = projection.build_rows(&test_lua(), &mut transcript.history, 80, &theme);
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
        let mut projection = TranscriptProjection::new();

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        assert_eq!(total, 199);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.display_layouts, 100);
        assert_eq!(counters.exact_height_measured_blocks, 100);

        projection.reset_counters();
        assert_eq!(
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80),
            total
        );
        assert_eq!(
            projection.counters(),
            TranscriptProjectionCounters::default(),
            "repeated exact row-count queries should use the exact height index"
        );
    }

    #[test]
    fn tool_state_changes_invalidate_measured_rows() {
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        parser.start_tool(
            &mut transcript.history,
            "call-1".into(),
            "bash".into(),
            protocol::StyledLines::from_plain("echo hi"),
            std::collections::HashMap::new(),
            std::time::Instant::now(),
        );
        let mut projection = TranscriptProjection::new();
        let pending_total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        parser.append_active_output(&mut transcript.history, "call-1", "first\nsecond\nthird");
        parser.set_active_status(
            &mut transcript.history,
            "call-1",
            ToolStatus::Ok,
            std::time::Instant::now(),
        );
        projection.reset_counters();

        let finished_total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        assert!(
            finished_total > pending_total,
            "finished tool output should add measured rows"
        );
        assert!(
            projection.counters().exact_height_measured_blocks > 0,
            "state changes must force row heights to be measured again"
        );
    }

    #[test]
    fn code_line_heights_measure_without_rendering_syntax() {
        let mut transcript = Transcript::new();
        for _ in 0..3 {
            transcript.push(Block::CodeLine {
                content: "x".repeat(40),
                lang: "rust".into(),
            });
        }
        let mut projection = TranscriptProjection::new();

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 10);

        assert_eq!(total, 12);
        assert_eq!(projection.display_layouts_len(), 3);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.display_layouts, 3);
        assert_eq!(counters.exact_height_measured_blocks, 3);
    }

    #[test]
    fn exact_total_rows_keeps_display_layouts_width_independent() {
        let mut projection = TranscriptProjection::new();
        let block_count = 537;
        let mut transcript = Transcript::new();
        for i in 0..block_count {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        assert_eq!(total, (block_count as RowIndex).saturating_mul(2) - 1);
        assert_eq!(projection.display_layouts_len(), block_count);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.display_layouts, block_count);
        assert_eq!(counters.exact_height_measured_blocks, block_count);

        projection.reset_counters();
        let total_narrow = projection.exact_total_rows(&test_lua(), &mut transcript.history, 40);
        assert!(total_narrow >= total);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(
            counters.display_layouts, 0,
            "display layouts are width-independent and should not be recompiled"
        );
        assert_eq!(
            counters.exact_height_measured_blocks, block_count,
            "width change must remeasure all block heights"
        );
    }

    #[test]
    fn theme_invalidation_preserves_exact_measurements() {
        let lua = test_lua();
        let mut projection = TranscriptProjection::new();
        let mut transcript = Transcript::new();
        for i in 0..128 {
            transcript.push(Block::Text {
                content: format!("line {i} {}", "x".repeat(64)),
            });
        }

        let total = projection.exact_total_rows(&lua, &mut transcript.history, 40);
        assert!(total > 0);
        projection.reset_counters();

        projection.invalidate_theme();
        assert_eq!(
            projection.exact_total_rows(&lua, &mut transcript.history, 40),
            total
        );
        assert_eq!(
            projection.counters().exact_height_measured_blocks,
            0,
            "theme changes must not discard exact row measurements"
        );
    }

    #[test]
    fn width_revisit_reuses_cached_exact_measurements() {
        let lua = test_lua();
        let mut projection = TranscriptProjection::new();
        let mut transcript = Transcript::new();
        for i in 0..128 {
            transcript.push(Block::Text {
                content: format!("line {i} {}", "x".repeat(64)),
            });
        }

        let wide_total = projection.exact_total_rows(&lua, &mut transcript.history, 80);
        let narrow_total = projection.exact_total_rows(&lua, &mut transcript.history, 40);
        assert!(narrow_total >= wide_total);
        projection.reset_counters();

        assert_eq!(
            projection.exact_total_rows(&lua, &mut transcript.history, 80),
            wide_total
        );
        assert_eq!(
            projection.counters().exact_height_measured_blocks,
            0,
            "revisiting a measured width should hydrate the exact row index from memory"
        );
    }

    #[test]
    fn exact_row_index_round_trips_through_display_cache() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let lua = test_lua();
        let mut projection = TranscriptProjection::new();
        let total = projection.exact_total_rows(&lua, &mut transcript.history, 80);
        assert_eq!(total, 199);
        let cache = projection.display_cache_data(&lua, &transcript.history);
        assert_eq!(cache.row_indexes.len(), 1);
        assert_eq!(cache.display_layouts.len(), 100);
        assert!(cache
            .row_indexes
            .iter()
            .all(|entry| entry.renderer_cache_key.is_some()));
        assert!(cache
            .display_layouts
            .iter()
            .all(|entry| entry.key.renderer_cache_key.is_some()));

        let mut hydrated = TranscriptProjection::new();
        hydrated.hydrate_display_cache(&lua, &transcript.history, cache);
        hydrated.reset_counters();

        assert_eq!(
            hydrated.exact_total_rows(&test_lua(), &mut transcript.history, 80),
            total
        );
        assert_eq!(
            hydrated.counters(),
            TranscriptProjectionCounters::default(),
            "hydrated exact row index should avoid compiling or measuring blocks"
        );
    }

    #[test]
    fn display_layouts_round_trip_without_row_index_recompilation() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let mut projection = TranscriptProjection::new();
        let total = projection.exact_total_rows(&lua, &mut transcript.history, 80);
        let mut cache = projection.display_cache_data(&lua, &transcript.history);
        assert_eq!(cache.display_layouts.len(), 100);
        cache.row_indexes.clear();

        let mut hydrated = TranscriptProjection::new();
        hydrated.hydrate_display_cache(&lua, &transcript.history, cache);
        assert_eq!(hydrated.display_layouts_len(), 100);
        hydrated.reset_counters();

        assert_eq!(
            hydrated.exact_total_rows(&lua, &mut transcript.history, 80),
            total
        );
        let counters = hydrated.counters();
        assert_eq!(
            counters.display_layouts, 0,
            "hydrated DisplayIR should avoid Lua recompilation"
        );
        assert_eq!(
            counters.exact_height_measured_blocks, 100,
            "without a row index, hydrated DisplayIR still needs exact measurement"
        );
    }

    #[test]
    fn renderer_without_cache_key_skips_persisted_display_cache() {
        let lua = test_lua();
        lua.lua
            .load(
                r#"
                smelt.transcript.set_renderer(function(block, ctx)
                  local _ = ctx
                  return smelt.layout.text(block.content or block.text or "")
                end)
                "#,
            )
            .exec()
            .expect("set renderer");
        assert_eq!(lua.transcript_renderer_cache_key(), None);

        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "hello".into(),
        });
        let mut projection = TranscriptProjection::new();
        assert_eq!(
            projection.exact_total_rows(&lua, &mut transcript.history, 80),
            1
        );
        assert_eq!(projection.display_layouts_len(), 1);

        let cache = projection.display_cache_data(&lua, &transcript.history);
        assert!(cache.row_indexes.is_empty());
        assert!(cache.display_layouts.is_empty());
    }

    #[test]
    fn renderer_cache_key_mismatch_rejects_display_cache_entries() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        for i in 0..20 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let mut projection = TranscriptProjection::new();
        let total = projection.exact_total_rows(&lua, &mut transcript.history, 80);
        let mut cache = projection.display_cache_data(&lua, &transcript.history);
        assert_eq!(cache.row_indexes.len(), 1);
        assert_eq!(cache.display_layouts.len(), 20);
        for entry in &mut cache.row_indexes {
            entry.renderer_cache_key = entry.renderer_cache_key.map(|key| key.wrapping_add(1));
        }
        for entry in &mut cache.display_layouts {
            entry.key.renderer_cache_key =
                entry.key.renderer_cache_key.map(|key| key.wrapping_add(1));
        }

        let mut hydrated = TranscriptProjection::new();
        hydrated.hydrate_display_cache(&lua, &transcript.history, cache);
        hydrated.reset_counters();

        assert_eq!(
            hydrated.exact_total_rows(&lua, &mut transcript.history, 80),
            total
        );
        let counters = hydrated.counters();
        assert_eq!(
            counters.display_layouts, 20,
            "renderer-cache-key mismatch must recompile persisted DisplayIR"
        );
        assert_eq!(
            counters.exact_height_measured_blocks, 20,
            "renderer-cache-key mismatch must reject persisted row indexes"
        );
    }

    #[test]
    fn renderer_generation_mismatch_rejects_display_cache_entries() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        for i in 0..20 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let mut projection = TranscriptProjection::new();
        let total = projection.exact_total_rows(&lua, &mut transcript.history, 80);
        let mut cache = projection.display_cache_data(&lua, &transcript.history);
        assert_eq!(cache.row_indexes.len(), 1);
        assert_eq!(cache.display_layouts.len(), 20);
        for entry in &mut cache.row_indexes {
            entry.renderer_generation = entry.renderer_generation.wrapping_add(1);
        }
        for entry in &mut cache.display_layouts {
            entry.key.renderer_generation = entry.key.renderer_generation.wrapping_add(1);
        }

        let mut hydrated = TranscriptProjection::new();
        hydrated.hydrate_display_cache(&lua, &transcript.history, cache);
        hydrated.reset_counters();

        assert_eq!(
            hydrated.exact_total_rows(&lua, &mut transcript.history, 80),
            total
        );
        let counters = hydrated.counters();
        assert_eq!(
            counters.display_layouts, 20,
            "renderer-generation mismatch must recompile persisted DisplayIR"
        );
        assert_eq!(
            counters.exact_height_measured_blocks, 20,
            "renderer-generation mismatch must reject persisted row indexes"
        );
    }

    #[test]
    fn incremental_row_index_only_measures_appended_blocks() {
        let mut projection = TranscriptProjection::new();
        let mut transcript = Transcript::new();
        for i in 0..50 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);
        assert_eq!(total, 99);
        let first_counters = projection.counters();
        assert_eq!(first_counters.exact_height_measured_blocks, 50);

        projection.reset_counters();
        for i in 50..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let total_after = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);
        assert_eq!(total_after, 199);
        let second_counters = projection.counters();
        assert_eq!(
            second_counters.exact_height_measured_blocks, 50,
            "only appended blocks should be measured"
        );
        assert_eq!(
            second_counters.display_layouts, 50,
            "only appended blocks should be compiled"
        );
    }

    #[test]
    fn incremental_row_index_remeasures_rewritten_block_and_successor() {
        let mut projection = TranscriptProjection::new();
        let mut transcript = Transcript::new();
        for i in 0..50 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);
        projection.reset_counters();

        transcript.history.rewrite(
            transcript.history.order[10],
            Block::Text {
                content: "rewritten block with different height".into(),
            },
        );
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        let counters = projection.counters();
        assert_eq!(
            counters.exact_height_measured_blocks, 2,
            "same-order rewrite should remeasure the changed block and following gap: {counters:?}"
        );
        assert_eq!(counters.display_layouts, 1);
    }

    #[test]
    fn incremental_row_index_rebuilds_when_order_prefix_changes() {
        let mut projection = TranscriptProjection::new();
        let mut transcript = Transcript::new();
        for i in 0..50 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);
        projection.reset_counters();

        transcript.history.order.remove(10);
        transcript.history.invalidate_display_cache();
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        let counters = projection.counters();
        assert!(
            counters.exact_height_measured_blocks >= 39,
            "order-prefix change should force a rebuild from the changed point: {counters:?}"
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
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80),
            199
        );

        projection.reset_counters();
        let rows = projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            80,
            &theme,
            150..153,
        );

        let text: Vec<_> = rows.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(text, vec!["line 75", "", "line 76"]);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.display_layouts, 0);
        assert_eq!(counters.exact_height_measured_blocks, 0);
        assert!(
            counters.range_materialized_blocks < transcript.history.order.len(),
            "range rows should materialize only intersecting blocks, got {counters:?}"
        );
    }

    #[test]
    fn copy_range_treats_doc_columns_as_bytes_for_multibyte_text() {
        let mut transcript = Transcript::new();
        let content = "alpha ’ beta gamma";
        transcript.push(Block::Text {
            content: content.into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let copied = projection.copy_range(
            &test_lua(),
            &mut transcript.history,
            80,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: 0,
                    byte_col: "alpha ’ ".len(),
                },
                end: crate::smelt_edit::DocPosition {
                    row: 0,
                    byte_col: "alpha ’ beta".len(),
                },
            },
        );

        assert_eq!(copied.clipboard, "beta");
        assert_eq!(copied.kill_ring, "beta");
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
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80),
            199
        );

        projection.reset_counters();
        let copied = projection.copy_range(
            &test_lua(),
            &mut transcript.history,
            80,
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
        assert_eq!(counters.display_layouts, 0);
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
    fn visible_projection_preload_scales_with_viewport() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(18), Default::default());
        let viewport_rows = 10;
        let max_materialized = (viewport_rows as RowIndex).saturating_mul(2);

        let top = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            viewport_rows,
        );
        assert!(
            top.materialized_rows <= max_materialized,
            "top projection should preload relative to viewport height: {top:?}"
        );
        assert!(buf.lines().iter().any(|line| line == "line 0"));

        let tail = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            viewport_rows,
        );
        assert!(
            tail.materialized_rows <= max_materialized,
            "tail projection should preload relative to viewport height: {tail:?}"
        );
        assert_eq!(
            tail.clamped_scroll,
            tail.total_rows.saturating_sub(viewport_rows as RowIndex)
        );
        assert!(buf.lines().iter().any(|line| line == "line 99"));
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
            &theme,
            ScrollTarget::visible_row(0),
            10,
        );
        let expected = full_buf.lines()[5..12].to_vec();

        let mut range_projection = TranscriptProjection::new();
        let range = range_projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            80,
            &theme,
            5..12,
        );

        let text: Vec<_> = range.rows.iter().map(|row| row.text.clone()).collect();
        assert_eq!(text, expected);
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
            &theme,
            ScrollTarget::visible_row(0),
            20,
        );
        let mut range_projection = TranscriptProjection::new();
        let range = range_projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            18,
            &theme,
            0..full_buf.line_count() as RowIndex,
        );
        assert!(
            !range.soft_breaks().is_empty(),
            "fixture should produce soft wraps"
        );

        let text: Vec<_> = range.rows.iter().map(|row| row.text.clone()).collect();
        assert_eq!(text, full_buf.lines().to_vec());
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
            .build_rows(&test_lua(), &mut transcript.history, 80, &theme)
            .len() as RowIndex;
        let mut buf = Buffer::new(crate::smelt_edit::BufId(5), Default::default());

        let output = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
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
            .materialize_block_layout(&test_lua(), &mut transcript.history, 80)
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
            .materialize_block_layout(&test_lua(), &mut transcript.history, 80)
            .into_iter()
            .find(|(id, _, _)| *id == anchor_id)
            .map(|(_, start, _)| start)
            .expect("anchor block layout");

        let before = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
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
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        buf.set_all_lines(vec!["other session".into()]);
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
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
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );
        second_projection.project(
            &mut shared,
            &mut second.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        first_projection.project(
            &mut shared,
            &mut first.history,
            80,
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
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );
        let visible_count = projection.visible_block_layout().count();
        assert!(visible_count < transcript.history.order.len());

        let layout = projection.materialize_block_layout(&test_lua(), &mut transcript.history, 80);
        assert_eq!(layout.len(), transcript.history.order.len());
        assert_eq!(layout.first().map(|(_, start, _)| *start), Some(0));
        assert_eq!(layout.last().map(|(_, _, rows)| *rows), Some(1));
        assert_eq!(projection.visible_block_layout().count(), visible_count);
    }

    #[test]
    fn visible_tail_uses_exact_total_after_full_row_materialization() {
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
        let full_rows = projection.build_rows(&test_lua(), &mut transcript.history, 80, &theme);
        assert_eq!(full_rows.len() as RowIndex, 439);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(7), Default::default());

        let tail = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
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
    fn visible_projection_preserves_rendered_row_anchor_across_width_change() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: format!(
                "{}\nANCHOR stay at viewport top\nafter",
                "before wrapping content ".repeat(6)
            ),
        });
        for i in 0..20 {
            transcript.push(Block::Text {
                content: format!("tail {i}"),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(24), Default::default());
        let old_width = 30;
        let new_width = 14;
        let viewport_rows = 5;
        let old_rows =
            projection.build_rows(&test_lua(), &mut transcript.history, old_width, &theme);
        let anchor_row = old_rows
            .iter()
            .position(|line| line.contains("ANCHOR"))
            .expect("anchor line") as RowIndex;

        let before = projection.project(
            &mut buf,
            &mut transcript.history,
            old_width,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            viewport_rows,
        );
        let before_local = row_to_usize(before.clamped_scroll.saturating_sub(before.row_base));
        assert!(buf
            .get_line(before_local)
            .is_some_and(|line| line.contains("ANCHOR")));

        let after = projection.project(
            &mut buf,
            &mut transcript.history,
            new_width,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            viewport_rows,
        );
        let after_local = row_to_usize(after.clamped_scroll.saturating_sub(after.row_base));

        assert!(
            buf.get_line(after_local)
                .is_some_and(|line| line.contains("ANCHOR")),
            "width shrink should keep the same rendered content at viewport top; got {:?}",
            buf.get_line(after_local)
        );

        let widened = projection.project(
            &mut buf,
            &mut transcript.history,
            old_width,
            &theme,
            ScrollTarget::visible_row(after.clamped_scroll),
            viewport_rows,
        );
        let widened_local = row_to_usize(widened.clamped_scroll.saturating_sub(widened.row_base));

        assert!(
            buf.get_line(widened_local)
                .is_some_and(|line| line.contains("ANCHOR")),
            "width expansion should keep the same rendered content at viewport top; got {:?}",
            buf.get_line(widened_local)
        );
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
            .materialize_block_layout(&test_lua(), &mut transcript.history, 80)
            .into_iter()
            .find(|(id, _, _)| *id == anchor_id)
            .map(|(_, start, _)| start)
            .expect("anchor block layout");

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            5,
        );
        assert!(buf.lines().iter().any(|line| line.contains("block 10")));

        projection.project(
            &mut buf,
            &mut transcript.history,
            24,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            5,
        );
        assert!(buf.lines().iter().any(|line| line.contains("block 10")));
    }

    fn next_u64(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    fn randomish_text(seed: &mut u64, words: usize) -> String {
        const WORDS: &[&str] = &[
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
            "lambda", "mu", "nu", "xi", "omicron", "pi",
        ];
        (0..words)
            .map(|_| WORDS[(next_u64(seed) as usize) % WORDS.len()])
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn randomish_block(i: usize, seed: &mut u64) -> Block {
        match (next_u64(seed) % 7) as usize {
            0 => Block::User {
                text: randomish_text(seed, 12),
                image_labels: vec![],
            },
            1 => Block::Text {
                content: format!(
                    "# Heading {i}\n\n{}\n\n- {}\n- `code {i}` and **bold**",
                    randomish_text(seed, 18),
                    randomish_text(seed, 8)
                ),
            },
            2 => Block::Thinking {
                content: randomish_text(seed, 20),
            },
            3 => Block::CodeLine {
                content: format!("let value_{i} = {};", next_u64(seed) % 10_000),
                lang: "rust".into(),
            },
            4 => Block::Exec {
                command: format!("echo {i}"),
                output: format!("{}\n{}", randomish_text(seed, 8), randomish_text(seed, 10)),
            },
            5 => Block::Compacted {
                summary: randomish_text(seed, 24),
            },
            _ => Block::ProcessStatus {
                text: format!("process status {i}: {}", randomish_text(seed, 6)),
                event: None,
            },
        }
    }

    #[test]
    fn current_projection_range_matches_full_rows_for_randomish_blocks_and_widths() {
        for width in [18, 31, 80] {
            let mut seed = 0x5eed_u64 + width as u64;
            let mut transcript = Transcript::new();
            for i in 0..96 {
                transcript.push(randomish_block(i, &mut seed));
            }
            transcript.push(Block::Mode {
                text: "now in apply mode".into(),
                icon: "● ".into(),
                hl_group: "SmeltModeApply".into(),
            });

            let theme = Theme::default();
            let mut projection = TranscriptProjection::new();
            let measured = projection.exact_total_rows(&test_lua(), &mut transcript.history, width);
            let full_rows =
                projection.build_rows(&test_lua(), &mut transcript.history, width, &theme);
            assert_eq!(measured as usize, full_rows.len(), "width {width}");

            let range_rows = projection.display_rows_for_range(
                &test_lua(),
                &mut transcript.history,
                width,
                &theme,
                0..measured,
            );
            let range_text: Vec<_> = range_rows
                .rows
                .iter()
                .map(|row| row.text.as_str())
                .collect();
            let full_text: Vec<_> = full_rows.iter().map(String::as_str).collect();
            assert_eq!(range_text, full_text, "width {width}");
        }
    }

    fn push_large_mixed_transcript_fixture(
        transcript: &mut Transcript,
        target_bytes: usize,
    ) -> usize {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let user = format!(
                "Investigate transcript layout batch {i}. {}",
                "preserve exact copy navigation scrollbar resize preview ".repeat(8)
            );
            approx_bytes += user.len();
            transcript.push(Block::User {
                text: user,
                image_labels: vec![],
            });

            let markdown = large_mixed_markdown_payload(i);
            approx_bytes += markdown.len();
            transcript.push(Block::Text { content: markdown });

            if i.is_multiple_of(5) {
                let reasoning = format!(
                    "{}\n{}",
                    "consider cached width-independent measurement ".repeat(20),
                    "validate row-count and copy-source equivalence ".repeat(20)
                );
                approx_bytes += reasoning.len();
                transcript.push(Block::Thinking { content: reasoning });
            }

            if i.is_multiple_of(7) {
                let command = format!("python scripts/analyze_layout.py --batch {i}");
                let output = (0..24)
                    .map(|j| {
                        format!(
                            "result {i}.{j}: {}",
                            "tool output wraps and truncates ".repeat(10)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                approx_bytes += command.len() + output.len();
                transcript.push(Block::Exec { command, output });
            }

            if i.is_multiple_of(11) {
                let call_id = format!("mixed-fixture-call-{i}");
                let output = (0..80)
                    .map(|j| {
                        format!(
                            "tool line {i}.{j}: {}",
                            "visible materialization ".repeat(12)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                approx_bytes += output.len();
                transcript.push_tool_call(
                    Block::ToolCall {
                        call_id: call_id.clone(),
                        name: "edit_file".into(),
                        summary: protocol::StyledLines::from_plain(format!("edit mixed_{i}.rs")),
                        args: std::collections::HashMap::new(),
                    },
                    ToolState {
                        status: ToolStatus::Ok,
                        elapsed: Some(std::time::Duration::from_millis(1_250)),
                        output: Some(Box::new(ToolOutput {
                            content: output,
                            is_error: false,
                            metadata: None,
                        })),
                        user_message: None,
                    },
                );
            }

            i += 1;
        }
        approx_bytes
    }

    fn large_mixed_markdown_payload(i: usize) -> String {
        format!(
            "# Batch {i}\n\n{}\n\n| file | rows | notes |\n| --- | ---: | --- |\n| transcript_{i}.rs | {} | {} |\n| render_{i}.rs | {} | {} |\n\n```rust\nfn batch_{i}() {{\n    let rows = {};\n    println!(\"{{rows}}\");\n}}\n```\n\n- {}\n- {}",
            "markdown paragraphs with inline `code`, **bold spans**, links, and wrap pressure ".repeat(18),
            i * 3 + 1,
            "table cells wrap under preview width ".repeat(8),
            i * 3 + 2,
            "copy source must remain markdown exact ".repeat(8),
            i * 17,
            "resize should preserve visible block anchors ".repeat(10),
            "first render should not materialize irrelevant rows ".repeat(10),
        )
    }

    fn approx_history_bytes(history: &BlockHistory) -> usize {
        let mut bytes = 0usize;
        for id in &history.order {
            if let Some(block) = history.blocks.get(id) {
                bytes += block.raw_text().map_or(0, |text| text.len());
                if let Block::ToolCall { call_id, .. } = block {
                    if let Some(state) = history.tool_state(call_id) {
                        bytes += state
                            .output
                            .as_ref()
                            .map_or(0, |output| output.content.len());
                    }
                }
            }
        }
        bytes
    }

    #[allow(clippy::too_many_arguments)]
    fn project_with_lua(
        projection: &mut TranscriptProjection,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> MaterializedRows {
        let plan = projection.plan_projection_measured(
            lua,
            history,
            width,
            theme,
            scroll_target,
            viewport_rows,
        );
        projection.project_planned(lua, buf, history, theme, plan)
    }

    fn push_markdown_heavy_transcript_fixture(
        transcript: &mut Transcript,
        target_bytes: usize,
    ) -> usize {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let content = format!(
                "# Markdown-heavy document {i}\n\n{}\n\n## Table\n\n| column | value | notes |\n| --- | ---: | --- |\n| alpha | {} | {} |\n| beta | {} | {} |\n\n## Code\n\n```rust\nfn markdown_heavy_{i}() -> usize {{\n    let mut total = 0;\n    for n in 0..{} {{ total += n; }}\n    total\n}}\n```\n\n{}\n\n{}",
                "Paragraph with links, `inline code`, emphasis, and wrap pressure. ".repeat(42),
                i * 11 + 1,
                "table cells intentionally contain enough content to wrap at narrow widths ".repeat(18),
                i * 11 + 2,
                "copy/search/source exactness must survive markdown measurement ".repeat(18),
                i % 127 + 32,
                "- bullet item with long content ".repeat(44),
                "> quoted reasoning line ".repeat(36),
            );
            approx_bytes += content.len();
            transcript.push(Block::Text { content });
            i += 1;
        }
        approx_bytes
    }

    fn push_tool_output_heavy_transcript_fixture(
        transcript: &mut Transcript,
        target_bytes: usize,
    ) -> usize {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let call_id = format!("tool-output-heavy-{i}");
            let command = format!("cargo test package_{i} -- --nocapture");
            let output = (0..160)
                .map(|j| {
                    format!(
                        "tool output line {i}.{j}: {}",
                        "ansi-free terminal output wraps exactly and keeps tail caps honest "
                            .repeat(8)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            approx_bytes += command.len() + output.len();
            transcript.push_tool_call(
                Block::ToolCall {
                    call_id: call_id.clone(),
                    name: "bash".into(),
                    summary: protocol::StyledLines::from_plain(command),
                    args: std::collections::HashMap::new(),
                },
                ToolState {
                    status: ToolStatus::Ok,
                    elapsed: Some(std::time::Duration::from_millis(2_000 + i as u64)),
                    output: Some(Box::new(ToolOutput {
                        content: output,
                        is_error: false,
                        metadata: None,
                    })),
                    user_message: None,
                },
            );
            i += 1;
        }
        approx_bytes
    }

    fn push_many_tiny_blocks_transcript_fixture(
        transcript: &mut Transcript,
        target_bytes: usize,
    ) -> usize {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let text = format!("tiny block {i} alpha beta gamma");
            approx_bytes += text.len();
            match i % 6 {
                0 => transcript.push(Block::User {
                    text,
                    image_labels: vec![],
                }),
                1 => transcript.push(Block::Text { content: text }),
                2 => transcript.push(Block::CodeLine {
                    content: format!("let tiny_{i} = {i};"),
                    lang: "rust".into(),
                }),
                3 => transcript.push(Block::Thinking { content: text }),
                4 => transcript.push(Block::Compacted { summary: text }),
                _ => transcript.push(Block::ProcessStatus { text, event: None }),
            }
            i += 1;
        }
        approx_bytes
    }

    fn push_few_huge_blocks_transcript_fixture(
        transcript: &mut Transcript,
        target_bytes: usize,
    ) -> usize {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let content = format!(
                "# Huge block {i}\n\n{}\n\n```text\n{}\n```\n\n{}",
                "large paragraph with wrapping pressure and markdown spans `code` **bold** ".repeat(900),
                "preformatted output still contributes exact rows without visible materialization\n"
                    .repeat(420),
                "closing paragraph ".repeat(700),
            );
            approx_bytes += content.len();
            transcript.push(Block::Text { content });
            i += 1;
        }
        approx_bytes
    }

    #[derive(Clone, Copy)]
    struct TranscriptBenchWorkload {
        name: &'static str,
        target_bytes: usize,
        build: fn(&mut Transcript, usize) -> usize,
    }

    #[derive(Clone, Copy, Debug)]
    struct TranscriptBenchSample {
        input_bytes: usize,
        generated_bytes: usize,
        blocks: usize,
        total_rows: RowIndex,
        cache_row_indexes: usize,
        cache_display_layouts: usize,
        first_ms: f64,
        resize_ms: f64,
        theme_ms: f64,
        scroll12_ms: f64,
        visible_ms: f64,
        hydrated_full_ms: f64,
        hydrated_ir_only_ms: f64,
        no_cache_ms: f64,
        allocs: u64,
        bytes_allocated: u64,
        visible_rows: usize,
        scroll_materialized_rows: u64,
        first_counters: TranscriptProjectionCounters,
        resize_counters: TranscriptProjectionCounters,
        theme_counters: TranscriptProjectionCounters,
        scroll_counters: TranscriptProjectionCounters,
        visible_counters: TranscriptProjectionCounters,
        hydrated_full_loaded_rows: usize,
        hydrated_full_counters: TranscriptProjectionCounters,
        hydrated_ir_loaded_rows: usize,
        hydrated_ir_counters: TranscriptProjectionCounters,
        no_cache_counters: TranscriptProjectionCounters,
    }

    #[derive(Clone, Copy, Debug)]
    struct MetricStats {
        mean: f64,
        stddev: f64,
        min: f64,
        max: f64,
    }

    impl MetricStats {
        fn from(values: &[f64]) -> Self {
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            let variance = if values.len() > 1 {
                values
                    .iter()
                    .map(|value| {
                        let delta = value - mean;
                        delta * delta
                    })
                    .sum::<f64>()
                    / (values.len() - 1) as f64
            } else {
                0.0
            };
            let min = values
                .iter()
                .copied()
                .fold(f64::INFINITY, |acc, value| acc.min(value));
            let max = values
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, |acc, value| acc.max(value));
            Self {
                mean,
                stddev: variance.sqrt(),
                min,
                max,
            }
        }

        fn display(self) -> String {
            format!("{:.1}±{:.1}", self.mean, self.stddev)
        }
    }

    fn elapsed_ms(elapsed: std::time::Duration) -> f64 {
        elapsed.as_secs_f64() * 1_000.0
    }

    fn transcript_bench_runs() -> usize {
        std::env::var("SMELT_TRANSCRIPT_BENCH_RUNS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|runs| *runs > 0)
            .unwrap_or(3)
    }

    fn transcript_bench_workloads() -> Vec<TranscriptBenchWorkload> {
        let all = vec![
            TranscriptBenchWorkload {
                name: "mixed_10mib",
                target_bytes: 10 * 1024 * 1024,
                build: push_large_mixed_transcript_fixture,
            },
            TranscriptBenchWorkload {
                name: "mixed_50mib",
                target_bytes: 50 * 1024 * 1024,
                build: push_large_mixed_transcript_fixture,
            },
            TranscriptBenchWorkload {
                name: "markdown_4mib",
                target_bytes: 4 * 1024 * 1024,
                build: push_markdown_heavy_transcript_fixture,
            },
            TranscriptBenchWorkload {
                name: "tool_output_4mib",
                target_bytes: 4 * 1024 * 1024,
                build: push_tool_output_heavy_transcript_fixture,
            },
            TranscriptBenchWorkload {
                name: "tiny_blocks_1mib",
                target_bytes: 1024 * 1024,
                build: push_many_tiny_blocks_transcript_fixture,
            },
            TranscriptBenchWorkload {
                name: "huge_blocks_4mib",
                target_bytes: 4 * 1024 * 1024,
                build: push_few_huge_blocks_transcript_fixture,
            },
        ];
        let Some(filter) = std::env::var("SMELT_TRANSCRIPT_BENCH_WORKLOADS")
            .ok()
            .filter(|filter| !filter.trim().is_empty())
        else {
            return all;
        };
        let names: std::collections::HashSet<_> = filter
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        all.into_iter()
            .filter(|workload| names.contains(workload.name))
            .collect()
    }

    fn push_session_resume_fixture(
        session: &mut smelt_core::session::Session,
        target_bytes: usize,
    ) {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let user = format!(
                "resume benchmark prompt {i}: {}",
                "please inspect the previous output and continue exactly ".repeat(8)
            );
            approx_bytes += user.len();
            session
                .history
                .push(protocol::HistoryItem::user(protocol::Content::text(user)));

            let assistant = format!(
                "# Resume benchmark response {i}\n\n{}\n\n```rust\nfn resume_bench_{i}() -> usize {{ {} }}\n```\n\n{}",
                "This paragraph has enough markdown and wrapping pressure to exercise transcript rebuild and layout measurement. ".repeat(18),
                "1 + ".repeat(32) + "0",
                "- exact rows must survive cache hydration\n".repeat(24),
            );
            approx_bytes += assistant.len();
            session.history.push(protocol::HistoryItem::assistant(
                protocol::AssistantStep::terminal(
                    Some(protocol::Content::text(assistant)),
                    None,
                    Vec::new(),
                ),
            ));
            i += 1;
        }
    }

    fn run_true_resume_bench_sample(target_bytes: usize) {
        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        let lua = crate::lua::LuaRuntime::new();
        let theme = Theme::default();
        let mut session = smelt_core::session::Session::new(0, std::env::current_dir().unwrap());
        session.id = format!("transcript-resume-bench-{}", smelt_core::session::now_ms());
        push_session_resume_fixture(&mut session, target_bytes);
        let history_items = session.history.len();

        let build_start = std::time::Instant::now();
        let mut transcript = crate::app::history::build_transcript_from_session(&lua, &session);
        let build_ms = elapsed_ms(build_start.elapsed());
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(91), Default::default());
        let first_start = std::time::Instant::now();
        let first = project_with_lua(
            &mut projection,
            &lua,
            &mut buf,
            &mut transcript.history,
            100,
            &theme,
            ScrollTarget::visible_tail(),
            40,
        );
        let first_ms = elapsed_ms(first_start.elapsed());
        let cache = projection.display_cache_data(&lua, &transcript.history);
        smelt_core::session::save_with_blobs(&session, &std::collections::HashMap::new());
        crate::content::display_cache::write_for_session(&session, &cache);

        let load_start = std::time::Instant::now();
        let loaded = smelt_core::session::load(&session.id).expect("load benchmark session");
        let load_ms = elapsed_ms(load_start.elapsed());
        let cache_start = std::time::Instant::now();
        let loaded_cache = crate::content::display_cache::read_for_session(&loaded);
        let cache_read_ms = elapsed_ms(cache_start.elapsed());
        let rebuild_start = std::time::Instant::now();
        let mut resumed = crate::app::history::build_transcript_from_session(&lua, &loaded);
        let rebuild_ms = elapsed_ms(rebuild_start.elapsed());
        let mut resumed_projection = TranscriptProjection::new();
        let hydrated_rows =
            resumed_projection.hydrate_display_cache(&lua, &resumed.history, loaded_cache);
        let mut resumed_buf = Buffer::new(crate::smelt_edit::BufId(92), Default::default());
        let render_start = std::time::Instant::now();
        let resumed_rows = project_with_lua(
            &mut resumed_projection,
            &lua,
            &mut resumed_buf,
            &mut resumed.history,
            100,
            &theme,
            ScrollTarget::visible_tail(),
            40,
        );
        let render_ms = elapsed_ms(render_start.elapsed());
        assert_eq!(resumed_rows.total_rows, first.total_rows);
        assert_eq!(hydrated_rows, 1);
        smelt_core::session::delete(&session.id);
        smelt_perf::perf::set_enabled(false);
        eprintln!(
            "TRANSCRIPT_TRUE_RESUME_SAMPLE target_bytes={} history_items={} rows={} build_ms={:.3} first_ms={:.3} load_ms={:.3} cache_read_ms={:.3} rebuild_ms={:.3} hydrated_rows={} render_ms={:.3}",
            target_bytes,
            history_items,
            first.total_rows,
            build_ms,
            first_ms,
            load_ms,
            cache_read_ms,
            rebuild_ms,
            hydrated_rows,
            render_ms,
        );
    }
    fn run_transcript_bench_sample(workload: TranscriptBenchWorkload) -> TranscriptBenchSample {
        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        smelt_perf::alloc::set_enabled(true);

        let lua = test_lua();
        let mut transcript = Transcript::new();
        let generated_bytes = (workload.build)(&mut transcript, workload.target_bytes);
        let input_bytes = approx_history_bytes(&transcript.history);
        let blocks = transcript.history.order.len();
        let theme = Theme::default();

        let mut cold = TranscriptProjection::new();
        let mut cold_buf = Buffer::new(crate::smelt_edit::BufId(77), Default::default());
        let alloc_start = smelt_perf::alloc::snapshot();
        let first_start = std::time::Instant::now();
        let first = project_with_lua(
            &mut cold,
            &lua,
            &mut cold_buf,
            &mut transcript.history,
            100,
            &theme,
            ScrollTarget::visible_tail(),
            40,
        );
        let first_ms = elapsed_ms(first_start.elapsed());
        let first_alloc = smelt_perf::alloc::delta(alloc_start, smelt_perf::alloc::snapshot());
        let first_counters = cold.counters();
        let cache = cold.display_cache_data(&lua, &transcript.history);

        cold.reset_counters();
        let resize_start = std::time::Instant::now();
        let resized = project_with_lua(
            &mut cold,
            &lua,
            &mut cold_buf,
            &mut transcript.history,
            72,
            &theme,
            ScrollTarget::visible_row(first.clamped_scroll),
            40,
        );
        let resize_ms = elapsed_ms(resize_start.elapsed());
        let resize_counters = cold.counters();

        cold.invalidate_theme();
        cold.reset_counters();
        let theme_start = std::time::Instant::now();
        let themed = project_with_lua(
            &mut cold,
            &lua,
            &mut cold_buf,
            &mut transcript.history,
            72,
            &theme,
            ScrollTarget::visible_row(resized.clamped_scroll),
            40,
        );
        let theme_ms = elapsed_ms(theme_start.elapsed());
        let theme_counters = cold.counters();

        cold.reset_counters();
        let scroll_start = std::time::Instant::now();
        let mut scroll_materialized_rows = 0u64;
        let max_scroll = resized.total_rows.saturating_sub(1);
        let step = (resized.total_rows / 12).max(1);
        for i in 0..12u64 {
            let row = i.saturating_mul(step).min(max_scroll);
            let rows = project_with_lua(
                &mut cold,
                &lua,
                &mut cold_buf,
                &mut transcript.history,
                72,
                &theme,
                ScrollTarget::visible_row(row),
                40,
            );
            scroll_materialized_rows =
                scroll_materialized_rows.saturating_add(rows.materialized_rows);
        }
        let scroll12_ms = elapsed_ms(scroll_start.elapsed());
        let scroll_counters = cold.counters();

        cold.reset_counters();
        let visible_start = std::time::Instant::now();
        let mid = resized.total_rows / 2;
        let visible =
            cold.display_rows_for_range(&lua, &mut transcript.history, 72, &theme, mid..mid + 80);
        let visible_ms = elapsed_ms(visible_start.elapsed());
        let visible_counters = cold.counters();

        let mut hydrated_full = TranscriptProjection::new();
        let hydrated_full_loaded_rows =
            hydrated_full.hydrate_display_cache(&lua, &transcript.history, cache.clone());
        hydrated_full.reset_counters();
        let mut hydrated_full_buf = Buffer::new(crate::smelt_edit::BufId(78), Default::default());
        let hydrated_full_start = std::time::Instant::now();
        let hydrated_full_projection = project_with_lua(
            &mut hydrated_full,
            &lua,
            &mut hydrated_full_buf,
            &mut transcript.history,
            100,
            &theme,
            ScrollTarget::visible_tail(),
            40,
        );
        let hydrated_full_ms = elapsed_ms(hydrated_full_start.elapsed());
        let hydrated_full_counters = hydrated_full.counters();

        let mut ir_only_cache = cache.clone();
        ir_only_cache.row_indexes.clear();
        let mut hydrated_ir = TranscriptProjection::new();
        let hydrated_ir_loaded_rows =
            hydrated_ir.hydrate_display_cache(&lua, &transcript.history, ir_only_cache);
        hydrated_ir.reset_counters();
        let mut hydrated_ir_buf = Buffer::new(crate::smelt_edit::BufId(79), Default::default());
        let hydrated_ir_start = std::time::Instant::now();
        let hydrated_ir_projection = project_with_lua(
            &mut hydrated_ir,
            &lua,
            &mut hydrated_ir_buf,
            &mut transcript.history,
            100,
            &theme,
            ScrollTarget::visible_tail(),
            40,
        );
        let hydrated_ir_only_ms = elapsed_ms(hydrated_ir_start.elapsed());
        let hydrated_ir_counters = hydrated_ir.counters();

        let mut no_cache = TranscriptProjection::new();
        let mut no_cache_buf = Buffer::new(crate::smelt_edit::BufId(80), Default::default());
        let no_cache_start = std::time::Instant::now();
        let no_cache_projection = project_with_lua(
            &mut no_cache,
            &lua,
            &mut no_cache_buf,
            &mut transcript.history,
            100,
            &theme,
            ScrollTarget::visible_tail(),
            40,
        );
        let no_cache_ms = elapsed_ms(no_cache_start.elapsed());
        let no_cache_counters = no_cache.counters();

        assert!(generated_bytes >= workload.target_bytes);
        assert!(input_bytes > 0);
        assert!(first.total_rows > 0);
        assert!(resized.total_rows > 0);
        assert_eq!(themed.total_rows, resized.total_rows);
        assert!(!visible.rows.is_empty());
        assert_eq!(hydrated_full_projection.total_rows, first.total_rows);
        assert_eq!(hydrated_ir_projection.total_rows, first.total_rows);
        assert_eq!(no_cache_projection.total_rows, first.total_rows);
        assert_eq!(first_counters.display_layouts, blocks);
        assert_eq!(resize_counters.display_layouts, 0);
        assert_eq!(theme_counters.display_layouts, 0);
        assert_eq!(scroll_counters.display_layouts, 0);
        assert_eq!(scroll_counters.exact_height_measured_blocks, 0);
        assert_eq!(visible_counters.display_layouts, 0);
        assert_eq!(visible_counters.exact_height_measured_blocks, 0);
        assert_eq!(hydrated_full_counters.display_layouts, 0);
        assert_eq!(hydrated_full_counters.exact_height_measured_blocks, 0);
        assert_eq!(hydrated_ir_counters.display_layouts, 0);
        assert_eq!(no_cache_counters.display_layouts, blocks);

        smelt_perf::alloc::set_enabled(false);
        smelt_perf::perf::set_enabled(false);
        smelt_perf::perf::clear();

        TranscriptBenchSample {
            input_bytes,
            generated_bytes,
            blocks,
            total_rows: first.total_rows,
            cache_row_indexes: cache.row_indexes.len(),
            cache_display_layouts: cache.display_layouts.len(),
            first_ms,
            resize_ms,
            theme_ms,
            scroll12_ms,
            visible_ms,
            hydrated_full_ms,
            hydrated_ir_only_ms,
            no_cache_ms,
            allocs: first_alloc.allocs,
            bytes_allocated: first_alloc.bytes_allocated,
            visible_rows: visible.rows.len(),
            scroll_materialized_rows,
            first_counters,
            resize_counters,
            theme_counters,
            scroll_counters,
            visible_counters,
            hydrated_full_loaded_rows,
            hydrated_full_counters,
            hydrated_ir_loaded_rows,
            hydrated_ir_counters,
            no_cache_counters,
        }
    }

    fn print_transcript_bench_summary(
        workload: TranscriptBenchWorkload,
        samples: &[TranscriptBenchSample],
    ) {
        let first = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.first_ms)
                .collect::<Vec<_>>(),
        );
        let resize = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.resize_ms)
                .collect::<Vec<_>>(),
        );
        let theme = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.theme_ms)
                .collect::<Vec<_>>(),
        );
        let scroll = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.scroll12_ms)
                .collect::<Vec<_>>(),
        );
        let visible = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.visible_ms)
                .collect::<Vec<_>>(),
        );
        let hydrated_full = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.hydrated_full_ms)
                .collect::<Vec<_>>(),
        );
        let hydrated_ir = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.hydrated_ir_only_ms)
                .collect::<Vec<_>>(),
        );
        let no_cache = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.no_cache_ms)
                .collect::<Vec<_>>(),
        );
        let sample = samples[0];
        eprintln!(
            "| {:<18} | {:>8.2} | {:>6} | {:>8} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} |",
            workload.name,
            sample.input_bytes as f64 / (1024.0 * 1024.0),
            sample.blocks,
            sample.total_rows,
            first.display(),
            resize.display(),
            theme.display(),
            scroll.display(),
            visible.display(),
            hydrated_full.display(),
            hydrated_ir.display(),
            no_cache.display(),
        );
        eprintln!(
            "TRANSCRIPT_LAYOUT_BENCH_SUMMARY workload={} runs={} input_bytes={} generated_bytes={} blocks={} rows={} cache_row_indexes={} cache_display_layouts={} first_mean_ms={:.3} first_stddev_ms={:.3} resize_mean_ms={:.3} resize_stddev_ms={:.3} theme_mean_ms={:.3} theme_stddev_ms={:.3} scroll12_mean_ms={:.3} scroll12_stddev_ms={:.3} visible_mean_ms={:.3} visible_stddev_ms={:.3} hydrated_full_mean_ms={:.3} hydrated_full_stddev_ms={:.3} hydrated_ir_only_mean_ms={:.3} hydrated_ir_only_stddev_ms={:.3} no_cache_mean_ms={:.3} no_cache_stddev_ms={:.3} allocs={} bytes_allocated={} visible_rows={} scroll_materialized_rows={} first_min_ms={:.3} first_max_ms={:.3}",
            workload.name,
            samples.len(),
            sample.input_bytes,
            sample.generated_bytes,
            sample.blocks,
            sample.total_rows,
            sample.cache_row_indexes,
            sample.cache_display_layouts,
            first.mean,
            first.stddev,
            resize.mean,
            resize.stddev,
            theme.mean,
            theme.stddev,
            scroll.mean,
            scroll.stddev,
            visible.mean,
            visible.stddev,
            hydrated_full.mean,
            hydrated_full.stddev,
            hydrated_ir.mean,
            hydrated_ir.stddev,
            no_cache.mean,
            no_cache.stddev,
            sample.allocs,
            sample.bytes_allocated,
            sample.visible_rows,
            sample.scroll_materialized_rows,
            first.min,
            first.max,
        );
        eprintln!(
            "TRANSCRIPT_LAYOUT_BENCH_COUNTERS workload={} first={:?} resize={:?} theme={:?} scroll12={:?} visible={:?} hydrated_full_loaded_rows={} hydrated_full={:?} hydrated_ir_loaded_rows={} hydrated_ir={:?} no_cache={:?}",
            workload.name,
            sample.first_counters,
            sample.resize_counters,
            sample.theme_counters,
            sample.scroll_counters,
            sample.visible_counters,
            sample.hydrated_full_loaded_rows,
            sample.hydrated_full_counters,
            sample.hydrated_ir_loaded_rows,
            sample.hydrated_ir_counters,
            sample.no_cache_counters,
        );
    }

    #[test]
    #[ignore = "manual transcript layout benchmark suite; prefer `cargo xtask bench-transcript-layout`"]
    fn transcript_layout_benchmark_suite() {
        let runs = transcript_bench_runs();
        let workloads = transcript_bench_workloads();
        assert!(!workloads.is_empty(), "no benchmark workloads selected");
        eprintln!(
            "TRANSCRIPT_LAYOUT_BENCH runs={runs} workloads={}",
            workloads.len()
        );
        eprintln!(
            "| workload           |      MiB | blocks |     rows |     first ms |    resize ms |     theme ms |   scroll12 ms |   visible ms |  fullcache ms |    ironly ms |   nocache ms |"
        );
        eprintln!(
            "|--------------------|----------|--------|----------|--------------|--------------|--------------|--------------|--------------|--------------|--------------|--------------|"
        );
        for workload in workloads {
            let _warmup = run_transcript_bench_sample(workload);
            let mut samples = Vec::with_capacity(runs);
            for run in 0..runs {
                let sample = run_transcript_bench_sample(workload);
                eprintln!(
                    "TRANSCRIPT_LAYOUT_BENCH_SAMPLE workload={} run={} input_bytes={} generated_bytes={} blocks={} rows={} first_ms={:.3} resize_ms={:.3} theme_ms={:.3} scroll12_ms={:.3} visible_ms={:.3} hydrated_full_ms={:.3} hydrated_ir_only_ms={:.3} no_cache_ms={:.3} allocs={} bytes_allocated={}",
                    workload.name,
                    run + 1,
                    sample.input_bytes,
                    sample.generated_bytes,
                    sample.blocks,
                    sample.total_rows,
                    sample.first_ms,
                    sample.resize_ms,
                    sample.theme_ms,
                    sample.scroll12_ms,
                    sample.visible_ms,
                    sample.hydrated_full_ms,
                    sample.hydrated_ir_only_ms,
                    sample.no_cache_ms,
                    sample.allocs,
                    sample.bytes_allocated,
                );
                samples.push(sample);
            }
            print_transcript_bench_summary(workload, &samples);
        }
    }

    #[test]
    #[ignore = "manual true session resume benchmark; run with --ignored --nocapture"]
    fn transcript_true_resume_benchmark_suite() {
        let target_bytes = std::env::var("SMELT_TRANSCRIPT_RESUME_BENCH_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(10 * 1024 * 1024);
        run_true_resume_bench_sample(target_bytes);
    }

    #[test]
    #[ignore = "manual large-transcript baseline; run with --ignored --nocapture"]
    fn mixed_large_transcript_projection_baseline() {
        let workload = TranscriptBenchWorkload {
            name: "mixed_10mib",
            target_bytes: 10 * 1024 * 1024,
            build: push_large_mixed_transcript_fixture,
        };
        let sample = run_transcript_bench_sample(workload);
        eprintln!(
            "TRANSCRIPT_LAYOUT_BASELINE input_bytes={} generated_bytes={} blocks={} total_rows={} cache_row_indexes={} cache_display_layouts={} first_ms={:.3} resize_ms={:.3} theme_ms={:.3} scroll12_ms={:.3} visible_ms={:.3} hydrated_full_ms={:.3} hydrated_ir_only_ms={:.3} no_cache_ms={:.3} allocs={} bytes_allocated={} visible_rows={} scroll_materialized_rows={}",
            sample.input_bytes,
            sample.generated_bytes,
            sample.blocks,
            sample.total_rows,
            sample.cache_row_indexes,
            sample.cache_display_layouts,
            sample.first_ms,
            sample.resize_ms,
            sample.theme_ms,
            sample.scroll12_ms,
            sample.visible_ms,
            sample.hydrated_full_ms,
            sample.hydrated_ir_only_ms,
            sample.no_cache_ms,
            sample.allocs,
            sample.bytes_allocated,
            sample.visible_rows,
            sample.scroll_materialized_rows,
        );
        eprintln!(
            "TRANSCRIPT_LAYOUT_COUNTERS first={:?} resize={:?} theme={:?} scroll12={:?} visible={:?} hydrated_full_loaded_rows={} hydrated_full={:?} hydrated_ir_loaded_rows={} hydrated_ir={:?} no_cache={:?}",
            sample.first_counters,
            sample.resize_counters,
            sample.theme_counters,
            sample.scroll_counters,
            sample.visible_counters,
            sample.hydrated_full_loaded_rows,
            sample.hydrated_full_counters,
            sample.hydrated_ir_loaded_rows,
            sample.hydrated_ir_counters,
            sample.no_cache_counters,
        );
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

        assert_eq!(copy_first_row(&buf), "* bash ls -la");
    }

    #[test]
    fn copy_tool_title_includes_pill_and_name_when_summary_is_empty() {
        let buf = project_tool_title("bash", protocol::StyledLines::empty());

        assert_eq!(copy_first_row(&buf), "* bash");
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
        assert_eq!(copy_first_row(&buf), "* bash echo hi");
    }

    #[test]
    fn expanded_group_compact_child_detail_is_selectable() {
        let lua = test_lua();
        install_read_file_renderer(&lua);
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "a.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "a.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "b.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "b.rs")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        let group_id = projection.render_plan.nodes[0].id();
        assert!(projection.fold_node(&transcript.history, group_id, FoldAction::Open));
        let expanded = projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        assert!(
            expanded.iter().any(|line| line.trim() == "1 lines"),
            "expanded rows: {expanded:?}"
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(8), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        assert!(
            buf.lines().iter().any(|line| line.trim() == "1 lines"),
            "rows: {:?}",
            buf.lines()
        );
        let copied = copy_byte_range(&buf, 0, buf.text().len());
        assert!(copied.contains("* read_file a.rs"), "copied: {copied:?}");
        assert!(copied.contains("1 lines"), "copied: {copied:?}");
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
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        let display = projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            40,
            &theme,
            0..buf.line_count() as RowIndex,
        );
        let soft = display.soft_breaks();
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
        let rows = projection.build_rows(&test_lua(), &mut transcript.history, 80, &theme);

        let display = projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            80,
            &theme,
            0..rows.len() as RowIndex,
        );
        assert!(
            display.soft_breaks().is_empty(),
            "unwrapped source lines must be hard breaks"
        );
        assert_eq!(
            display.hard_breaks(),
            crate::smelt_edit::hard_breaks_for_lines(&rows)
        );
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
