use super::display_block::{
    measure_block, render_block_into, CompileJob, DisplayModel, DisplayRowIndexEntry,
    DisplayRowIndexNode, MeasureCtx, RenderCtx, TranscriptRenderEnv,
};
use crate::smelt_edit::Theme;
use crate::smelt_edit::{
    clamp_scroll, row_to_usize, BufCreateOpts, BufId, Buffer, CopyOutput, DisplayRow, DisplayRows,
    DocRange, MaterializedRows, RowBreak, RowIndex,
};
use smelt_buffer::coords::copy_byte_range;
use smelt_core::buffer::{LineDecoration, Span, SpanMeta};
use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey, ViewState};
use std::sync::Arc;

pub(crate) struct TranscriptProjection {
    display_model: DisplayModel,
    display_model_generation: u64,
    layout_width: u16,
    materialized: Option<MaterializedProjection>,
    /// Block layout from the last visible `project()`. Surfaced to Lua via `visible_blocks`.
    visible_layout: Vec<LayoutEntry>,
    /// Absolute row represented by local row 0 in the backing buffer.
    visible_row_base: RowIndex,
    /// Total rows in the logical transcript represented by the visible projection.
    visible_total_rows: RowIndex,
    /// Cached `build_rows` result for full-text consumers (Lua API, vim navigation).
    cached_rows: Option<CachedRows>,
    exact_rows: ExactRowIndex,
    cached_row_indexes: Vec<DisplayRowIndexEntry>,
    display_cache_generation: u64,
    renderer_generation: Option<u64>,
    renderer_cache_key: Option<u64>,
    #[cfg(test)]
    counters: TranscriptProjectionCounters,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TranscriptProjectionCounters {
    pub full_row_builds: usize,
    pub display_blocks: usize,
    pub exact_height_measured_blocks: usize,
    pub range_materialized_blocks: usize,
}

struct CachedRows {
    rows: Arc<Vec<String>>,
    generation: u64,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    width: u16,
    show_thinking: bool,
}

#[derive(Default)]
struct ExactRowIndex {
    nodes: Vec<ExactBlockRow>,
    prefix_rows: Vec<RowIndex>,
    prefix_dirty: bool,
    generation: u64,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    width: u16,
    show_thinking: bool,
}

struct ExactBlockRow {
    id: BlockId,
    key: LayoutKey,
    estimated_height: RowIndex,
    exact_height: Option<RowIndex>,
}

impl ExactBlockRow {
    fn measured_or_estimated_height(&self) -> RowIndex {
        self.exact_height.unwrap_or(self.estimated_height)
    }
}

impl ExactRowIndex {
    fn is_current(
        &self,
        history: &BlockHistory,
        width: u16,
        show_thinking: bool,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> bool {
        self.generation == history.generation()
            && self.renderer_generation == renderer_generation
            && self.renderer_cache_key == renderer_cache_key
            && self.width == width
            && self.show_thinking == show_thinking
            && self.nodes.len() == history.order.len()
    }

    fn rebuild_if_stale(
        &mut self,
        history: &BlockHistory,
        width: u16,
        show_thinking: bool,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        base_key: LayoutKey,
    ) {
        let gen = history.generation();
        if self.generation == gen
            && self.renderer_generation == renderer_generation
            && self.renderer_cache_key == renderer_cache_key
            && self.width == width
            && self.show_thinking == show_thinking
        {
            return;
        }

        let keep_measurements = self.renderer_generation == renderer_generation
            && self.renderer_cache_key == renderer_cache_key
            && self.width == width
            && self.show_thinking == show_thinking;
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
                .map(ExactBlockRow::measured_or_estimated_height)
                .or_else(|| {
                    old_nodes
                        .iter()
                        .find(|node| node.id == id)
                        .map(ExactBlockRow::measured_or_estimated_height)
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
            self.nodes.push(ExactBlockRow {
                id,
                key,
                estimated_height,
                exact_height,
            });
        }
        self.generation = gen;
        self.renderer_generation = renderer_generation;
        self.renderer_cache_key = renderer_cache_key;
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
        self.prefix_dirty = true;
        true
    }

    /// Sync the index when the current history keeps the old order as a prefix.
    /// Returns `false` when a deletion or reorder means the index must be rebuilt.
    fn sync_stable_order_prefix(
        &mut self,
        history: &BlockHistory,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        base_key: LayoutKey,
    ) -> bool {
        let old_len = self.nodes.len();
        if self.renderer_generation != renderer_generation
            || self.renderer_cache_key != renderer_cache_key
        {
            return false;
        }
        if old_len > history.order.len() {
            return false;
        }
        if old_len == history.order.len()
            && self.generation == history.generation()
            && self.renderer_generation == renderer_generation
            && self.renderer_cache_key == renderer_cache_key
            && self.width == base_key.width
            && self.show_thinking == base_key.show_thinking
        {
            return true;
        }
        let mut prev_key_changed = false;
        let mut prefix_dirty = false;
        for index in 0..old_len {
            let id = history.order[index];
            let key = history.resolve_key(id, base_key);
            let node = &mut self.nodes[index];
            if node.id != id {
                return false;
            }
            if node.key != key {
                node.key = key;
                node.exact_height = None;
                prev_key_changed = true;
                prefix_dirty = true;
            } else if prev_key_changed {
                node.exact_height = None;
                prev_key_changed = false;
                prefix_dirty = true;
            }
        }
        if old_len < history.order.len() {
            prefix_dirty = true;
        }
        for index in old_len..history.order.len() {
            let id = history.order[index];
            let key = history.resolve_key(id, base_key);
            self.nodes.push(ExactBlockRow {
                id,
                key,
                estimated_height: 1,
                exact_height: None,
            });
        }
        self.generation = history.generation();
        self.renderer_generation = renderer_generation;
        self.renderer_cache_key = renderer_cache_key;
        self.width = base_key.width;
        self.show_thinking = base_key.show_thinking;
        self.prefix_dirty |= prefix_dirty;
        true
    }

    fn is_exact_for(
        &self,
        history: &BlockHistory,
        width: u16,
        show_thinking: bool,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> bool {
        self.is_current(
            history,
            width,
            show_thinking,
            renderer_generation,
            renderer_cache_key,
        ) && self.nodes.iter().all(|node| node.exact_height.is_some())
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

    fn hydrate_from_cache(
        &mut self,
        history: &BlockHistory,
        entry: &DisplayRowIndexEntry,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        base_key: LayoutKey,
    ) -> bool {
        if entry.renderer_generation != renderer_generation
            || entry.renderer_cache_key != renderer_cache_key
            || entry.width != base_key.width
            || entry.show_thinking != base_key.show_thinking
        {
            return false;
        }
        if entry.nodes.len() != history.order.len() {
            return false;
        }
        let mut nodes = Vec::with_capacity(entry.nodes.len());
        for (index, cached) in entry.nodes.iter().enumerate() {
            let id = history.order[index];
            if cached.id != id {
                return false;
            }
            let key = history.resolve_key(id, base_key);
            if cached.key != key {
                return false;
            }
            nodes.push(ExactBlockRow {
                id,
                key,
                estimated_height: cached.exact_height,
                exact_height: Some(cached.exact_height),
            });
        }
        self.nodes = nodes;
        self.generation = history.generation();
        self.renderer_generation = renderer_generation;
        self.renderer_cache_key = renderer_cache_key;
        self.width = base_key.width;
        self.show_thinking = base_key.show_thinking;
        self.rebuild_prefix_rows();
        true
    }

    fn cache_entry(&self) -> Option<DisplayRowIndexEntry> {
        if self.nodes.is_empty() || self.nodes.iter().any(|node| node.exact_height.is_none()) {
            return None;
        }
        Some(DisplayRowIndexEntry {
            width: self.width,
            show_thinking: self.show_thinking,
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
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
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
    block_range: std::ops::Range<usize>,
}

impl ProjectionPlan {
    pub(crate) fn block_range(&self) -> std::ops::Range<usize> {
        self.block_range.clone()
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
}

struct MaterializedTranscriptRange {
    row_base: RowIndex,
    total_rows: RowIndex,
    texts: Vec<String>,
    pending: Vec<PendingRow>,
    layout: Vec<LayoutEntry>,
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

fn row_index_entry_matches(history: &BlockHistory, entry: &DisplayRowIndexEntry) -> bool {
    if entry.nodes.len() != history.order.len() {
        return false;
    }
    let base_key = base_layout_key(entry.width, entry.show_thinking);
    entry.nodes.iter().enumerate().all(|(index, node)| {
        let id = history.order[index];
        node.id == id && node.key == history.resolve_key(id, base_key)
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
            && existing.show_thinking == entry.show_thinking
            && existing.renderer_generation == entry.renderer_generation
            && existing.renderer_cache_key == entry.renderer_cache_key
    }) {
        *existing = entry;
    } else {
        entries.push(entry);
    }
}

fn render_display_block_to_buffer(
    display_model: &DisplayModel,
    id: BlockId,
    key: LayoutKey,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    theme: &Theme,
) -> Option<(Buffer, usize)> {
    let display_block = display_model.get(id, key, renderer_generation, renderer_cache_key)?;
    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let outcome = render_block_into(
        &mut buf,
        display_block,
        RenderCtx {
            width: key.width,
            show_thinking: key.show_thinking,
            view_state: key.view_state,
            theme,
        },
    );
    Some((buf, outcome.line_count))
}

impl TranscriptProjection {
    pub(crate) fn new() -> Self {
        Self {
            display_model: DisplayModel::new(),
            display_model_generation: u64::MAX,
            layout_width: 0,
            materialized: None,
            visible_layout: Vec::new(),
            visible_row_base: 0,
            visible_total_rows: 0,
            cached_rows: None,
            exact_rows: ExactRowIndex::default(),
            cached_row_indexes: Vec::new(),
            display_cache_generation: 0,
            renderer_generation: None,
            renderer_cache_key: None,
            #[cfg(test)]
            counters: TranscriptProjectionCounters::default(),
        }
    }

    pub(crate) fn hydrate_display_cache(
        &mut self,
        history: &BlockHistory,
        data: crate::content::display_cache::DisplayCacheData,
    ) -> usize {
        let crate::content::display_cache::DisplayCacheData {
            row_indexes,
            display_blocks,
        } = data;
        let hydrated_blocks = self
            .display_model
            .hydrate_from_cache(history, display_blocks);
        self.cached_row_indexes = row_indexes;
        smelt_perf::perf::record_value(
            "transcript:display_model_cache:loaded",
            hydrated_blocks as u64,
        );
        smelt_perf::perf::record_value(
            "transcript:row_index_cache:loaded",
            self.cached_row_indexes.len() as u64,
        );
        self.row_index_cache_entries(history).len()
    }

    pub(crate) fn display_cache_data(
        &self,
        history: &BlockHistory,
    ) -> crate::content::display_cache::DisplayCacheData {
        crate::content::display_cache::DisplayCacheData {
            row_indexes: self.row_index_cache_entries(history),
            display_blocks: self.display_model.cache_entries(
                history,
                self.renderer_generation,
                self.renderer_cache_key,
            ),
        }
    }

    pub(crate) fn display_cache_generation(&self) -> u64 {
        self.display_cache_generation
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
        self.display_model = DisplayModel::new();
        self.display_model_generation = u64::MAX;
        self.cached_row_indexes.clear();
        self.clear_materialized_state();
        self.display_cache_generation = self.display_cache_generation.wrapping_add(1);
        true
    }

    fn row_index_cache_entries(&self, history: &BlockHistory) -> Vec<DisplayRowIndexEntry> {
        let renderer_generation = self.renderer_generation;
        let renderer_cache_key = self.renderer_cache_key;
        let mut entries: Vec<DisplayRowIndexEntry> = self
            .cached_row_indexes
            .iter()
            .filter(|entry| {
                row_index_entry_matches_renderer(entry, renderer_generation, renderer_cache_key)
                    && row_index_entry_matches(history, entry)
            })
            .cloned()
            .collect();
        if let Some(current) = self.exact_rows.cache_entry() {
            if row_index_entry_matches_renderer(&current, renderer_generation, renderer_cache_key)
                && row_index_entry_matches(history, &current)
            {
                upsert_row_index_entry(&mut entries, current);
            }
        }
        entries
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
    pub(crate) fn display_model_len(&self) -> usize {
        self.display_model.len()
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
        let blocks = jobs.into_iter().map(|job| job.compile(env)).collect();
        self.display_model.insert_compiled_blocks(blocks);
        if compiled > 0 {
            self.display_cache_generation = self.display_cache_generation.wrapping_add(1);
        }
        #[cfg(test)]
        {
            self.counters.display_blocks += compiled;
        }
        let _ = compiled;
    }

    fn ensure_block_indices(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        indices: impl IntoIterator<Item = usize>,
    ) {
        let jobs = {
            let nodes = &self.exact_rows.nodes;
            let blocks = indices
                .into_iter()
                .filter_map(|index| nodes.get(index).map(|node| (index, node.id, node.key)));
            self.display_model.collect_compile_jobs(
                history,
                env.renderer_generation,
                env.renderer_cache_key,
                blocks,
            )
        };
        self.finish_compile_jobs(env, jobs);
    }

    fn clear_materialized_state(&mut self) {
        self.materialized = None;
        self.visible_layout.clear();
        self.visible_row_base = 0;
        self.visible_total_rows = 0;
        self.cached_rows = None;
        self.exact_rows = ExactRowIndex::default();
    }

    fn gc_if_stale(&mut self, history: &BlockHistory, width: u16) {
        let gen = history.generation();
        if self.display_model_generation != gen {
            self.display_model.retain_order(&history.order);
            self.display_model_generation = gen;
        }
        if width != self.layout_width {
            // Width changes invalidate row indexes and materialized rows, but
            // display blocks are width-independent and stay reusable.
            self.layout_width = width;
            self.clear_materialized_state();
        }
    }

    /// Clear cached visible/layout state so the next projection rebuilds from scratch.
    /// Display blocks are theme-independent; only rendered buffers/rows carry colors.
    pub(crate) fn invalidate_theme(&mut self) {
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

    fn try_hydrate_row_index(
        &mut self,
        history: &BlockHistory,
        width: u16,
        show_thinking: bool,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        base_key: LayoutKey,
    ) -> bool {
        if self.exact_rows.is_current(
            history,
            width,
            show_thinking,
            renderer_generation,
            renderer_cache_key,
        ) {
            return true;
        }
        if renderer_cache_key.is_none() {
            smelt_perf::perf::record_value("transcript:row_index_cache:miss", 1);
            return false;
        }
        let Some(entry) = self.cached_row_indexes.iter().find(|entry| {
            entry.width == width
                && entry.show_thinking == show_thinking
                && entry.renderer_generation == renderer_generation
                && entry.renderer_cache_key == renderer_cache_key
        }) else {
            smelt_perf::perf::record_value("transcript:row_index_cache:miss", 1);
            return false;
        };
        let hydrated = self.exact_rows.hydrate_from_cache(
            history,
            entry,
            renderer_generation,
            renderer_cache_key,
            base_key,
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
        show_thinking: bool,
    ) {
        let env = TranscriptRenderEnv::new(lua, show_thinking);
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
        let show_thinking = env.show_thinking;
        let renderer_generation = env.renderer_generation;
        let renderer_cache_key = env.renderer_cache_key;
        self.invalidate_renderer_if_changed(renderer_generation, renderer_cache_key);
        self.gc_if_stale(history, width);
        let base_key = base_layout_key(width, show_thinking);
        let hydrated_index = self.try_hydrate_row_index(
            history,
            width,
            show_thinking,
            renderer_generation,
            renderer_cache_key,
            base_key,
        );
        let reused_index = hydrated_index
            || self.exact_rows.sync_stable_order_prefix(
                history,
                renderer_generation,
                renderer_cache_key,
                base_key,
            );
        smelt_perf::perf::record_value(
            "transcript:rebuild_row_index:reused_index",
            u64::from(reused_index),
        );
        if !reused_index {
            let _perf = smelt_perf::perf::begin("transcript:rebuild_row_index:rebuild_index");
            self.exact_rows.rebuild_if_stale(
                history,
                width,
                show_thinking,
                renderer_generation,
                renderer_cache_key,
                base_key,
            );
        }
        if self.exact_rows.is_exact_for(
            history,
            width,
            show_thinking,
            renderer_generation,
            renderer_cache_key,
        ) {
            if reused_index {
                self.exact_rows.refresh_prefix_rows();
            }
            return;
        }

        let missing: Vec<usize> = {
            let _perf = smelt_perf::perf::begin("transcript:rebuild_row_index:collect_missing");
            (0..self.exact_rows.nodes.len())
                .filter(|&i| {
                    self.exact_rows
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
        self.ensure_block_indices(env, history, missing.iter().copied());
        for i in missing {
            self.measure_display_block_height(history, i, renderer_generation, renderer_cache_key);
        }
        self.exact_rows.refresh_prefix_rows();
    }

    fn measure_display_block_height(
        &mut self,
        history: &BlockHistory,
        index: usize,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> bool {
        let Some(node) = self.exact_rows.nodes.get(index) else {
            return false;
        };
        if node.exact_height.is_some() {
            return true;
        }
        let id = node.id;
        let key = node.key;
        let Some(block) = self
            .display_model
            .get(id, key, renderer_generation, renderer_cache_key)
        else {
            return false;
        };
        let rows = measure_block(
            block,
            MeasureCtx {
                width: key.width,
                show_thinking: key.show_thinking,
                view_state: key.view_state,
            },
        ) as RowIndex;
        let gap = history.rendered_block_gap(index, rows as usize) as RowIndex;
        self.set_exact_height(index, gap.saturating_add(rows));
        true
    }

    fn set_exact_height(&mut self, index: usize, rows: RowIndex) {
        let measured = self.exact_rows.set_exact_height(index, rows);
        if measured {
            self.display_cache_generation = self.display_cache_generation.wrapping_add(1);
        }
        #[cfg(test)]
        if measured {
            self.counters.exact_height_measured_blocks += 1;
        }
    }

    fn exact_block_layout(&self, history: &BlockHistory) -> Vec<LayoutEntry> {
        let mut layout = Vec::with_capacity(self.exact_rows.nodes.len());
        let mut running_total: RowIndex = 0;
        for (i, node) in self.exact_rows.nodes.iter().enumerate() {
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
            running_total = running_total.saturating_add(gap);
            layout.push(LayoutEntry {
                id: node.id,
                start: running_total,
                rows: exact_height.saturating_sub(gap),
            });
            running_total = running_total.saturating_add(exact_height.saturating_sub(gap));
        }
        layout
    }

    pub(crate) fn exact_total_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
    ) -> RowIndex {
        self.rebuild_row_index(lua, history, width, show_thinking);
        self.exact_rows.total_rows()
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
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> ProjectionPlan {
        let _perf = smelt_perf::perf::begin("transcript:plan_projection_measured");
        let resize_anchor = self.resize_anchor_for(width, scroll_target);
        let env = TranscriptRenderEnv::new(lua, show_thinking);
        self.rebuild_row_index_with_env(env, history, width);
        let key = ProjectKey {
            generation: history.generation(),
            width,
            show_thinking,
            renderer_generation: env.renderer_generation,
            renderer_cache_key: env.renderer_cache_key,
            mode: scroll_target.mode(viewport_rows),
        };
        self.plan_projection_from_prepared(
            history,
            key,
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
        let index = self.exact_rows.block_index(id)?;
        let exact_height = self.exact_rows.nodes.get(index)?.exact_height?;
        let gap = (history.block_gap(index) as RowIndex).min(exact_height);
        Some(
            self.exact_rows
                .prefix_row(index)
                .saturating_add(gap)
                .saturating_add(offset),
        )
    }

    fn plan_projection_from_prepared(
        &self,
        history: &BlockHistory,
        key: ProjectKey,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
        resize_anchor: Option<(BlockId, RowIndex)>,
    ) -> ProjectionPlan {
        let total_rows = self.exact_rows.total_rows();
        let requested_scroll_top = self
            .scroll_top_for_resize_anchor(history, resize_anchor)
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
        let block_range = self.exact_rows.block_range_for_rows(row_window);
        ProjectionPlan {
            key,
            scroll_target,
            scroll_top,
            viewport_rows,
            block_range,
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
        show_thinking: bool,
        theme: &Theme,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> MaterializedRows {
        let lua = smelt_core::lua::runtime::LuaRuntime::new();
        let plan = self.plan_projection_measured(
            &lua,
            history,
            width,
            show_thinking,
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
        let current_env = TranscriptRenderEnv::new(lua, plan.key.show_thinking);
        if history.generation() != plan.key.generation
            || current_env.renderer_generation != plan.key.renderer_generation
            || current_env.renderer_cache_key != plan.key.renderer_cache_key
        {
            self.rebuild_row_index_with_env(current_env, history, plan.key.width);
            let key = ProjectKey {
                generation: history.generation(),
                width: plan.key.width,
                show_thinking: plan.key.show_thinking,
                renderer_generation: current_env.renderer_generation,
                renderer_cache_key: current_env.renderer_cache_key,
                mode: plan.scroll_target.mode(plan.viewport_rows),
            };
            plan = self.plan_projection_from_prepared(
                history,
                key,
                plan.scroll_target,
                plan.viewport_rows,
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
            || prev.show_thinking != key.show_thinking
            || prev.renderer_generation != key.renderer_generation
            || prev.renderer_cache_key != key.renderer_cache_key
        {
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
        lua: &smelt_core::lua::runtime::LuaRuntime,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        theme: &Theme,
        plan: &ProjectionPlan,
    ) -> MaterializedRows {
        let _perf = smelt_perf::perf::begin("transcript:project_visible_range");
        smelt_perf::perf::record_value(
            "transcript:project_visible_range:blocks",
            plan.block_range.len() as u64,
        );
        let materialized = self.collect_blocks_range(
            TranscriptRenderEnv::with_renderer(
                lua,
                plan.key.show_thinking,
                plan.key.renderer_generation,
                plan.key.renderer_cache_key,
            ),
            history,
            theme,
            plan.block_range(),
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
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        theme: &Theme,
        block_range: std::ops::Range<usize>,
    ) -> MaterializedTranscriptRange {
        let _perf = smelt_perf::perf::begin("transcript:collect_blocks_range");
        let start = block_range.start.min(self.exact_rows.nodes.len());
        let end = block_range.end.min(self.exact_rows.nodes.len());
        smelt_perf::perf::record_value(
            "transcript:collect_blocks_range:blocks",
            end.saturating_sub(start) as u64,
        );
        #[cfg(test)]
        {
            self.counters.range_materialized_blocks += end.saturating_sub(start);
        }
        let row_base = self.exact_rows.prefix_row(start);
        let mut texts = Vec::new();
        let mut pending = Vec::new();
        let mut layout = Vec::with_capacity(end.saturating_sub(start));
        let mut rows = ProjectRows {
            row_base,
            texts: &mut texts,
            pending: &mut pending,
            layout: &mut layout,
        };

        let block_indices = start..end;
        self.ensure_block_indices(env, history, block_indices.clone());
        for block_index in block_indices {
            let id = self.exact_rows.nodes[block_index].id;
            let key = self.exact_rows.nodes[block_index].key;
            self.append_projected_block(history, theme, block_index, id, key, &mut rows);
        }

        self.exact_rows.refresh_prefix_rows();
        MaterializedTranscriptRange {
            row_base,
            total_rows: self.exact_rows.total_rows(),
            texts,
            pending,
            layout,
        }
    }

    fn append_projected_block(
        &mut self,
        history: &BlockHistory,
        theme: &Theme,
        block_index: usize,
        id: BlockId,
        key: LayoutKey,
        rows: &mut ProjectRows<'_>,
    ) {
        let renderer_generation = self.exact_rows.renderer_generation;
        let renderer_cache_key = self.exact_rows.renderer_cache_key;
        let Some((block_buf, block_rows)) = render_display_block_to_buffer(
            &self.display_model,
            id,
            key,
            renderer_generation,
            renderer_cache_key,
            theme,
        ) else {
            return;
        };
        let gap = history.rendered_block_gap(block_index, block_rows);
        self.set_exact_height(
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

    /// Exact full block layout for compatibility APIs. This may measure every
    /// transcript block, but it does not concatenate display rows and does not
    /// re-render blocks when the exact height index is already current.
    pub(crate) fn materialize_block_layout(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
    ) -> Vec<(BlockId, RowIndex, RowIndex)> {
        self.rebuild_row_index(lua, history, width, show_thinking);
        self.exact_block_layout(history)
            .into_iter()
            .map(|e| (e.id, e.start, e.rows))
            .collect()
    }

    /// Full display rows. Cached by `(generation, width, show_thinking)`; repeat
    /// callers get a free `Arc::clone`.
    pub(crate) fn build_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> Arc<Vec<String>> {
        let _perf = smelt_perf::perf::begin("transcript:build_rows");
        let gen = history.generation();
        let env = TranscriptRenderEnv::new(lua, show_thinking);
        let renderer_generation = env.renderer_generation;
        let renderer_cache_key = env.renderer_cache_key;
        self.invalidate_renderer_if_changed(renderer_generation, renderer_cache_key);
        self.gc_if_stale(history, width);
        if let Some(c) = &self.cached_rows {
            if c.generation == gen
                && c.renderer_generation == renderer_generation
                && c.renderer_cache_key == renderer_cache_key
                && c.width == width
                && c.show_thinking == show_thinking
            {
                return Arc::clone(&c.rows);
            }
        }
        #[cfg(test)]
        {
            self.counters.full_row_builds += 1;
        }
        smelt_perf::perf::record_value("transcript:build_rows:blocks", history.order.len() as u64);
        let base_key = base_layout_key(width, show_thinking);
        self.exact_rows.rebuild_if_stale(
            history,
            width,
            show_thinking,
            renderer_generation,
            renderer_cache_key,
            base_key,
        );
        let mut rows: Vec<String> = Vec::new();
        let block_indices = 0..self.exact_rows.nodes.len();
        self.ensure_block_indices(env, history, block_indices.clone());
        for i in block_indices {
            let Some(node) = self.exact_rows.nodes.get(i) else {
                continue;
            };
            let id = node.id;
            let bkey = node.key;
            let Some((block_buf, block_rows)) = render_display_block_to_buffer(
                &self.display_model,
                id,
                bkey,
                renderer_generation,
                renderer_cache_key,
                theme,
            ) else {
                continue;
            };
            let gap = history.rendered_block_gap(i, block_rows);
            self.set_exact_height(i, (gap as usize).saturating_add(block_rows) as RowIndex);
            for _ in 0..gap {
                rows.push(String::new());
            }
            for r in 0..block_rows {
                rows.push(block_buf.get_line(r).unwrap_or("").to_string());
            }
        }
        self.exact_rows.refresh_prefix_rows();
        let rows = Arc::new(rows);
        self.cached_rows = Some(CachedRows {
            rows: Arc::clone(&rows),
            generation: gen,
            renderer_generation,
            renderer_cache_key,
            width,
            show_thinking,
        });
        rows
    }

    pub(crate) fn display_rows_for_range(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
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

        self.rebuild_row_index(lua, history, width, show_thinking);
        let total_rows = self.exact_rows.total_rows();
        if total_rows == 0 || start >= total_rows {
            return DisplayRows::empty();
        }
        let end = end.min(total_rows);
        let block_range = self.exact_rows.block_range_for_rows(start..end);
        if block_range.start >= block_range.end {
            return DisplayRows::empty();
        }

        let materialized = self.collect_blocks_range(
            TranscriptRenderEnv::with_renderer(
                lua,
                show_thinking,
                self.exact_rows.renderer_generation,
                self.exact_rows.renderer_cache_key,
            ),
            history,
            theme,
            block_range,
        );
        let local_start = row_to_usize(start.saturating_sub(materialized.row_base));
        let local_end =
            row_to_usize(end.saturating_sub(materialized.row_base)).min(materialized.texts.len());
        if local_start >= local_end {
            return DisplayRows::empty();
        }
        let mut soft_wrapped = vec![false; materialized.texts.len()];
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
            .enumerate()
            .map(|(offset, (text, selectable_ranges))| {
                let row = DisplayRow::new(text, selectable_ranges);
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
        show_thinking: bool,
        theme: &Theme,
        range: DocRange,
    ) -> CopyOutput {
        let _perf = smelt_perf::perf::begin("transcript:copy_range");
        if (range.start.row, range.start.byte_col) >= (range.end.row, range.end.byte_col) {
            return CopyOutput::default();
        }
        self.rebuild_row_index(lua, history, width, show_thinking);
        let total_rows = self.exact_rows.total_rows();
        if total_rows == 0 || range.start.row >= total_rows {
            return CopyOutput::default();
        }
        let end_row = range.end.row.min(total_rows.saturating_sub(1));
        let block_range = self
            .exact_rows
            .block_range_for_rows(range.start.row..end_row.saturating_add(1));
        if block_range.start >= block_range.end {
            return CopyOutput::default();
        }

        let mut scratch = Buffer::new(BufId(0), BufCreateOpts::default());
        let materialized = self.collect_blocks_range(
            TranscriptRenderEnv::with_renderer(
                lua,
                show_thinking,
                self.exact_rows.renderer_generation,
                self.exact_rows.renderer_cache_key,
            ),
            history,
            theme,
            block_range,
        );
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
        let raw = src[range.start..range.end].to_string();
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
            false,
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

        let rows = projection.build_rows(&test_lua(), &mut transcript.history, 80, false, &theme);

        assert!(rows.iter().any(|line| line == "line 99"));
        assert!(rows.iter().any(|line| line == "line 0"));
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 1);
        assert_eq!(counters.display_blocks, 100);
        assert_eq!(counters.exact_height_measured_blocks, 100);

        projection.reset_counters();
        let cached = projection.build_rows(&test_lua(), &mut transcript.history, 80, false, &theme);
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

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);

        assert_eq!(total, 199);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.display_blocks, 100);
        assert_eq!(counters.exact_height_measured_blocks, 100);

        projection.reset_counters();
        assert_eq!(
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false),
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
        let pending_total =
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);

        parser.append_active_output(&mut transcript.history, "call-1", "first\nsecond\nthird");
        parser.set_active_status(
            &mut transcript.history,
            "call-1",
            ToolStatus::Ok,
            std::time::Instant::now(),
        );
        projection.reset_counters();

        let finished_total =
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);

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

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 10, false);

        assert_eq!(total, 12);
        assert_eq!(projection.display_model_len(), 3);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.display_blocks, 3);
        assert_eq!(counters.exact_height_measured_blocks, 3);
    }

    #[test]
    fn exact_total_rows_keeps_display_blocks_width_independent() {
        let mut projection = TranscriptProjection::new();
        let block_count = 537;
        let mut transcript = Transcript::new();
        for i in 0..block_count {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);

        assert_eq!(total, (block_count as RowIndex).saturating_mul(2) - 1);
        assert_eq!(projection.display_model_len(), block_count);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.display_blocks, block_count);
        assert_eq!(counters.exact_height_measured_blocks, block_count);

        projection.reset_counters();
        let total_narrow =
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 40, false);
        assert!(total_narrow >= total);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(
            counters.display_blocks, 0,
            "display blocks are width-independent and should not be recompiled"
        );
        assert_eq!(
            counters.exact_height_measured_blocks, block_count,
            "width change must remeasure all block heights"
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
        let mut projection = TranscriptProjection::new();
        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);
        assert_eq!(total, 199);
        let cache = projection.display_cache_data(&transcript.history);
        assert_eq!(cache.row_indexes.len(), 1);
        assert_eq!(cache.display_blocks.len(), 100);
        assert!(cache
            .row_indexes
            .iter()
            .all(|entry| entry.renderer_cache_key.is_some()));
        assert!(cache
            .display_blocks
            .iter()
            .all(|entry| entry.key.renderer_cache_key.is_some()));

        let mut hydrated = TranscriptProjection::new();
        hydrated.hydrate_display_cache(&transcript.history, cache);
        hydrated.reset_counters();

        assert_eq!(
            hydrated.exact_total_rows(&test_lua(), &mut transcript.history, 80, false),
            total
        );
        assert_eq!(
            hydrated.counters(),
            TranscriptProjectionCounters::default(),
            "hydrated exact row index should avoid compiling or measuring blocks"
        );
    }

    #[test]
    fn display_blocks_round_trip_without_row_index_recompilation() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let mut projection = TranscriptProjection::new();
        let total = projection.exact_total_rows(&lua, &mut transcript.history, 80, false);
        let mut cache = projection.display_cache_data(&transcript.history);
        assert_eq!(cache.display_blocks.len(), 100);
        cache.row_indexes.clear();

        let mut hydrated = TranscriptProjection::new();
        hydrated.hydrate_display_cache(&transcript.history, cache);
        assert_eq!(hydrated.display_model_len(), 100);
        hydrated.reset_counters();

        assert_eq!(
            hydrated.exact_total_rows(&lua, &mut transcript.history, 80, false),
            total
        );
        let counters = hydrated.counters();
        assert_eq!(
            counters.display_blocks, 0,
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
            projection.exact_total_rows(&lua, &mut transcript.history, 80, false),
            1
        );
        assert_eq!(projection.display_model_len(), 1);

        let cache = projection.display_cache_data(&transcript.history);
        assert!(cache.row_indexes.is_empty());
        assert!(cache.display_blocks.is_empty());
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
        let total = projection.exact_total_rows(&lua, &mut transcript.history, 80, false);
        let mut cache = projection.display_cache_data(&transcript.history);
        assert_eq!(cache.row_indexes.len(), 1);
        assert_eq!(cache.display_blocks.len(), 20);
        for entry in &mut cache.row_indexes {
            entry.renderer_cache_key = entry.renderer_cache_key.map(|key| key.wrapping_add(1));
        }
        for entry in &mut cache.display_blocks {
            entry.key.renderer_cache_key =
                entry.key.renderer_cache_key.map(|key| key.wrapping_add(1));
        }

        let mut hydrated = TranscriptProjection::new();
        hydrated.hydrate_display_cache(&transcript.history, cache);
        hydrated.reset_counters();

        assert_eq!(
            hydrated.exact_total_rows(&lua, &mut transcript.history, 80, false),
            total
        );
        let counters = hydrated.counters();
        assert_eq!(
            counters.display_blocks, 20,
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
        let total = projection.exact_total_rows(&lua, &mut transcript.history, 80, false);
        let mut cache = projection.display_cache_data(&transcript.history);
        assert_eq!(cache.row_indexes.len(), 1);
        assert_eq!(cache.display_blocks.len(), 20);
        for entry in &mut cache.row_indexes {
            entry.renderer_generation = entry.renderer_generation.wrapping_add(1);
        }
        for entry in &mut cache.display_blocks {
            entry.key.renderer_generation = entry.key.renderer_generation.wrapping_add(1);
        }

        let mut hydrated = TranscriptProjection::new();
        hydrated.hydrate_display_cache(&transcript.history, cache);
        hydrated.reset_counters();

        assert_eq!(
            hydrated.exact_total_rows(&lua, &mut transcript.history, 80, false),
            total
        );
        let counters = hydrated.counters();
        assert_eq!(
            counters.display_blocks, 20,
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

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);
        assert_eq!(total, 99);
        let first_counters = projection.counters();
        assert_eq!(first_counters.exact_height_measured_blocks, 50);

        projection.reset_counters();
        for i in 50..100 {
            transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }
        let total_after =
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);
        assert_eq!(total_after, 199);
        let second_counters = projection.counters();
        assert_eq!(
            second_counters.exact_height_measured_blocks, 50,
            "only appended blocks should be measured"
        );
        assert_eq!(
            second_counters.display_blocks, 50,
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
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);
        projection.reset_counters();

        transcript.history.rewrite(
            transcript.history.order[10],
            Block::Text {
                content: "rewritten block with different height".into(),
            },
        );
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);

        let counters = projection.counters();
        assert_eq!(
            counters.exact_height_measured_blocks, 2,
            "same-order rewrite should remeasure the changed block and following gap: {counters:?}"
        );
        assert_eq!(counters.display_blocks, 1);
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
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);
        projection.reset_counters();

        transcript.history.order.remove(10);
        transcript.history.invalidate_display_cache();
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false);

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
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false),
            199
        );

        projection.reset_counters();
        let rows = projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            80,
            false,
            &theme,
            150..153,
        );

        let text: Vec<_> = rows.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(text, vec!["line 75", "", "line 76"]);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.display_blocks, 0);
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
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80, false),
            199
        );

        projection.reset_counters();
        let copied = projection.copy_range(
            &test_lua(),
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
        assert_eq!(counters.display_blocks, 0);
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
            &theme,
            ScrollTarget::visible_row(0),
            20,
        );
        let mut range_projection = TranscriptProjection::new();
        let range = range_projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            18,
            false,
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
            .build_rows(&test_lua(), &mut transcript.history, 80, false, &theme)
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
            .materialize_block_layout(&test_lua(), &mut transcript.history, 80, false)
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
            .materialize_block_layout(&test_lua(), &mut transcript.history, 80, false)
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
            projection.materialize_block_layout(&test_lua(), &mut transcript.history, 80, false);
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
        let full_rows =
            projection.build_rows(&test_lua(), &mut transcript.history, 80, false, &theme);
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
            .materialize_block_layout(&test_lua(), &mut transcript.history, 80, false)
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
            let measured =
                projection.exact_total_rows(&test_lua(), &mut transcript.history, width, true);
            let full_rows =
                projection.build_rows(&test_lua(), &mut transcript.history, width, true, &theme);
            assert_eq!(measured as usize, full_rows.len(), "width {width}");

            let range_rows = projection.display_rows_for_range(
                &test_lua(),
                &mut transcript.history,
                width,
                true,
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

    #[test]
    #[ignore = "manual large-transcript baseline; run with --ignored --nocapture"]
    fn mixed_large_transcript_projection_baseline() {
        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        smelt_perf::alloc::set_enabled(true);

        let mut transcript = Transcript::new();
        let generated_bytes =
            push_large_mixed_transcript_fixture(&mut transcript, 10 * 1024 * 1024);
        let approx_bytes = approx_history_bytes(&transcript.history);

        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(77), Default::default());

        let alloc_start = smelt_perf::alloc::snapshot();

        let first_start = std::time::Instant::now();
        let first = projection.project(
            &mut buf,
            &mut transcript.history,
            100,
            false,
            &theme,
            ScrollTarget::visible_tail(),
            40,
        );
        let first_elapsed = first_start.elapsed();
        let first_alloc = smelt_perf::alloc::delta(alloc_start, smelt_perf::alloc::snapshot());

        let resize_start = std::time::Instant::now();
        let resized = projection.project(
            &mut buf,
            &mut transcript.history,
            72,
            false,
            &theme,
            ScrollTarget::visible_row(first.clamped_scroll),
            40,
        );
        let resize_elapsed = resize_start.elapsed();

        let visible_start = std::time::Instant::now();
        let mid = resized.total_rows / 2;
        let visible = projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            72,
            false,
            &theme,
            mid..mid + 80,
        );
        let visible_elapsed = visible_start.elapsed();

        eprintln!(
            "TRANSCRIPT_LAYOUT_BASELINE input_bytes={approx_bytes} generated_bytes={generated_bytes} blocks={} total_rows={} first_ms={} resize_ms={} visible_ms={} allocs={} bytes_allocated={} visible_rows={}",
            transcript.history.order.len(),
            resized.total_rows,
            first_elapsed.as_millis(),
            resize_elapsed.as_millis(),
            visible_elapsed.as_millis(),
            first_alloc.allocs,
            first_alloc.bytes_allocated,
            visible.rows.len(),
        );
        eprintln!("perf snapshot: {:#?}", smelt_perf::perf::snapshot());

        assert!(approx_bytes >= 10 * 1024 * 1024);
        assert!(first.total_rows > 0);
        assert!(resized.total_rows > 0);
        assert!(!visible.rows.is_empty());

        smelt_perf::alloc::set_enabled(false);
        smelt_perf::perf::set_enabled(false);
        smelt_perf::perf::clear();
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

        let display = projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            40,
            false,
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
        let rows = projection.build_rows(&test_lua(), &mut transcript.history, 80, false, &theme);

        let display = projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            80,
            false,
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
