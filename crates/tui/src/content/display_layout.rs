use crate::content::transcript_scene::{
    NodeLayoutKey, RenderNode, RenderNodeId, TranscriptDefaultViewPolicy,
};
use crate::smelt_edit::{Buffer, Theme};
use smelt_core::content::block_layout::{
    BlockLayout, GutterSpec, HboxItem, IrLeaf, LayoutIr, LuaLeaf, MarkdownSpec, SourceViewIr,
    StyleSpec, TextSpec,
};
use smelt_core::content::builder::{LineBuilder, Outcome};
use smelt_core::content::highlight::InlineOptions;
use smelt_core::lua::runtime::LuaRuntime;
use smelt_core::theme::intern;
#[cfg(test)]
use smelt_core::transcript_model::BlockId;
use smelt_core::transcript_model::{Block, BlockHistory, Status, ViewState};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

pub(crate) const DISPLAY_RENDERER_VERSION: u64 = 12;

pub(crate) fn transcript_renderer_cache_key(
    lua: &LuaRuntime,
    inline_options: &InlineOptions,
) -> Option<u64> {
    let mut key = lua.transcript_renderer_cache_key();
    let icon_hash = inline_options
        .file_icons
        .enabled
        .then(|| inline_options.file_icons.cache_hash());
    for hash in [icon_hash, lua.transcript_settings_cache_key()]
        .into_iter()
        .flatten()
    {
        key = Some(match key {
            Some(base) => base ^ hash.rotate_left(17),
            None => hash,
        });
    }
    key
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayCacheKey {
    pub(crate) content_hash: u64,
    pub(crate) sidecar_hash: u64,
    pub(crate) renderer_version: u64,
    pub(crate) renderer_generation: u64,
    pub(crate) renderer_cache_key: Option<u64>,
    pub(crate) render_context_hash: u64,
}

impl DisplayCacheKey {
    pub(crate) fn new(
        content_hash: u64,
        sidecar_hash: u64,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        render_context_hash: u64,
    ) -> Self {
        Self {
            content_hash,
            sidecar_hash,
            renderer_version: DISPLAY_RENDERER_VERSION,
            renderer_generation,
            renderer_cache_key,
            render_context_hash,
        }
    }

    fn from_node_key(
        key: NodeLayoutKey,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> Self {
        Self::new(
            key.content_hash,
            key.sidecar_hash,
            renderer_generation,
            renderer_cache_key,
            smelt_core::utils::hash_serializable(&key.view_state),
        )
    }

    fn same_render_inputs_except_content(self, other: Self) -> bool {
        self.sidecar_hash == other.sidecar_hash
            && self.renderer_version == other.renderer_version
            && self.renderer_generation == other.renderer_generation
            && self.renderer_cache_key == other.renderer_cache_key
            && self.render_context_hash == other.render_context_hash
    }
}

#[derive(Clone)]
pub(crate) struct TranscriptRenderEnv<'a> {
    pub(crate) lua: &'a LuaRuntime,
    pub(crate) renderer_generation: u64,
    pub(crate) renderer_cache_key: Option<u64>,
    pub(crate) now_ms: u64,
    pub(crate) refresh_now: std::time::Instant,
}

impl<'a> TranscriptRenderEnv<'a> {
    #[cfg(test)]
    pub(crate) fn new(lua: &'a LuaRuntime) -> Self {
        Self::with_inline_options(lua, InlineOptions::default())
    }

    pub(crate) fn with_inline_options(lua: &'a LuaRuntime, inline_options: InlineOptions) -> Self {
        Self {
            lua,
            renderer_generation: lua.transcript_renderer_generation(),
            renderer_cache_key: transcript_renderer_cache_key(lua, &inline_options),
            now_ms: lua.transcript_now_ms(),
            refresh_now: lua.transcript_instant_now(),
        }
    }

    pub(crate) fn with_renderer(
        lua: &'a LuaRuntime,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> Self {
        Self {
            lua,
            renderer_generation,
            renderer_cache_key,
            now_ms: lua.transcript_now_ms(),
            refresh_now: lua.transcript_instant_now(),
        }
    }
}

pub(crate) struct CompileJob {
    id: RenderNodeId,
    key: DisplayCacheKey,
    view_state: ViewState,
    source: CompileJobSource,
}

enum CompileJobSource {
    Snapshot {
        value: serde_json::Value,
        cache_source_views: bool,
    },
    Ready(LayoutIr),
}

struct CompiledLayout {
    layout: LayoutIr,
    refresh_after_ms: Option<u64>,
}

impl CompileJob {
    fn compile(
        self,
        env: TranscriptRenderEnv<'_>,
        source_views: &mut SourceViewCache,
    ) -> (
        RenderNodeId,
        DisplayCacheKey,
        LayoutIr,
        Option<std::time::Instant>,
    ) {
        let compiled = match self.source {
            CompileJobSource::Snapshot {
                value,
                cache_source_views,
            } => {
                let mut cache = CompileLayoutCache {
                    source_views,
                    source_views_enabled: cache_source_views,
                    refresh_after_ms: None,
                };
                compile_node_with_lua(env.clone(), &value, self.view_state, &mut cache)
            }
            CompileJobSource::Ready(layout) => CompiledLayout {
                layout,
                refresh_after_ms: None,
            },
        };
        (
            self.id,
            self.key,
            compiled.layout,
            compiled.refresh_after_ms.and_then(|after_ms| {
                env.refresh_now
                    .checked_add(std::time::Duration::from_millis(after_ms))
            }),
        )
    }
}

#[derive(Clone)]
pub(crate) struct MeasureCtx {
    pub width: u16,
    pub view_state: ViewState,
    pub inline_options: InlineOptions,
}

#[derive(Clone)]
pub(crate) struct RenderCtx<'a> {
    pub width: u16,
    pub view_state: ViewState,
    pub theme: &'a Theme,
    pub history: Option<&'a BlockHistory>,
    pub inline_options: InlineOptions,
}

struct CachedLayout {
    key: DisplayCacheKey,
    layout: LayoutIr,
    weight: usize,
    refresh_at: Option<std::time::Instant>,
    content_hash_pending: bool,
}

struct CachedSourceView {
    ir: SourceViewIr,
    weight: usize,
}

#[derive(Default)]
struct SourceViewCache {
    entries: HashMap<u64, CachedSourceView>,
    lru: VecDeque<u64>,
    retained_bytes: usize,
}

impl SourceViewCache {
    fn get(&mut self, key: u64) -> Option<&SourceViewIr> {
        let ir = self.entries.get(&key)?;
        self.lru.retain(|candidate| *candidate != key);
        self.lru.push_back(key);
        Some(&ir.ir)
    }

    fn insert(&mut self, key: u64, ir: SourceViewIr) {
        let weight = ir.retained_bytes();
        if let Some(previous) = self.entries.remove(&key) {
            self.retained_bytes = self.retained_bytes.saturating_sub(previous.weight);
        }
        self.lru.retain(|candidate| *candidate != key);
        self.lru.push_back(key);
        self.retained_bytes = self.retained_bytes.saturating_add(weight);
        self.entries.insert(key, CachedSourceView { ir, weight });
    }

    fn evict_oldest(&mut self) -> bool {
        let Some(key) = self.lru.pop_front() else {
            return false;
        };
        if let Some(entry) = self.entries.remove(&key) {
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.weight);
            return true;
        }
        false
    }
}

struct CompileLayoutCache<'a> {
    source_views: &'a mut SourceViewCache,
    source_views_enabled: bool,
    refresh_after_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DisplayMemorySnapshot {
    pub(crate) layout_bytes: usize,
    pub(crate) source_view_bytes: usize,
    pub(crate) pinned_layout_bytes: usize,
    pub(crate) oversize_debt_bytes: usize,
}

pub(crate) struct LayoutCache {
    blocks: HashMap<RenderNodeId, CachedLayout>,
    source_views: SourceViewCache,
    lru: RefCell<VecDeque<RenderNodeId>>,
    pinned: HashSet<RenderNodeId>,
    retained_bytes: usize,
    budget: usize,
    earliest_refresh_at: Option<std::time::Instant>,
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self {
            blocks: HashMap::new(),
            source_views: SourceViewCache::default(),
            lru: RefCell::new(VecDeque::new()),
            pinned: HashSet::new(),
            retained_bytes: 0,
            budget: 16 * 1024 * 1024,
            earliest_refresh_at: None,
        }
    }
}

impl LayoutCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        self.enforce_budget();
    }

    pub(crate) fn set_pinned_nodes(&mut self, ids: impl IntoIterator<Item = RenderNodeId>) {
        self.pinned.clear();
        self.pinned.extend(ids);
        self.enforce_budget();
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained_bytes
            .saturating_add(self.source_views.retained_bytes)
    }

    pub(crate) fn memory_snapshot(&self) -> DisplayMemorySnapshot {
        let pinned_layout_bytes = self
            .pinned
            .iter()
            .filter_map(|id| self.blocks.get(id).map(|entry| entry.weight))
            .sum();
        DisplayMemorySnapshot {
            layout_bytes: self.retained_bytes,
            source_view_bytes: self.source_views.retained_bytes,
            pinned_layout_bytes,
            oversize_debt_bytes: self.retained_bytes().saturating_sub(self.budget),
        }
    }

    pub(crate) fn apply_streaming_text_append(
        &mut self,
        id: RenderNodeId,
        content: &str,
        ranges: &[std::ops::Range<usize>],
    ) -> bool {
        let Some(entry) = self.blocks.get_mut(&id) else {
            return false;
        };
        let Some(spec) = single_markdown_spec_mut(&mut entry.layout) else {
            return false;
        };
        let mut expected_start = spec.content.len();
        for range in ranges {
            if range.start != expected_start
                || range.end < range.start
                || range.end > content.len()
                || smelt_buffer::text::slice(content, range.clone()).len()
                    != range.end.saturating_sub(range.start)
            {
                return false;
            }
            expected_start = range.end;
        }
        let old_weight = entry.weight;
        for range in ranges {
            spec.content
                .push_str(smelt_buffer::text::slice(content, range.clone()));
        }
        entry.weight = entry.layout.retained_bytes();
        entry.content_hash_pending = true;
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(old_weight)
            .saturating_add(entry.weight);
        true
    }

    fn promote_streaming_content_key(&mut self, id: RenderNodeId, key: DisplayCacheKey) -> bool {
        let Some(entry) = self.blocks.get_mut(&id) else {
            return false;
        };
        if !entry.content_hash_pending || !entry.key.same_render_inputs_except_content(key) {
            return false;
        }
        entry.key = key;
        entry.content_hash_pending = false;
        self.touch(id);
        true
    }

    fn touch(&self, id: RenderNodeId) {
        let mut lru = self.lru.borrow_mut();
        lru.retain(|candidate| *candidate != id);
        lru.push_back(id);
    }

    fn recompute_earliest_refresh_at(&mut self) {
        self.earliest_refresh_at = self
            .blocks
            .values()
            .filter_map(|entry| entry.refresh_at)
            .min();
    }

    fn enforce_budget(&mut self) {
        let mut attempts = self.lru.borrow().len();
        while self.retained_bytes() > self.budget && attempts > 0 {
            attempts -= 1;
            let Some(id) = self.lru.borrow_mut().pop_front() else {
                break;
            };
            if self.pinned.contains(&id) {
                self.lru.borrow_mut().push_back(id);
                continue;
            }
            if let Some(entry) = self.blocks.remove(&id) {
                self.retained_bytes = self.retained_bytes.saturating_sub(entry.weight);
                attempts = self.lru.borrow().len();
            }
        }
        while self.retained_bytes() > self.budget && self.source_views.evict_oldest() {}
        self.recompute_earliest_refresh_at();
        smelt_perf::perf::record_value(
            "transcript:render_cache:retained_bytes",
            self.retained_bytes() as u64,
        );
        smelt_perf::perf::record_value(
            "transcript:render_cache:pinned_bytes",
            self.pinned
                .iter()
                .filter_map(|id| self.blocks.get(id).map(|entry| entry.weight))
                .sum::<usize>() as u64,
        );
        smelt_perf::perf::record_value(
            "transcript:render_cache:oversize_debt_bytes",
            self.retained_bytes().saturating_sub(self.budget) as u64,
        );
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.blocks.len()
    }

    #[cfg(test)]
    pub(crate) fn ensure_many(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        ids: &[BlockId],
        keys: &[NodeLayoutKey],
    ) -> usize {
        let renderer_generation = env.renderer_generation;
        let renderer_cache_key = env.renderer_cache_key;
        let blocks = ids
            .iter()
            .copied()
            .zip(keys.iter().copied())
            .filter_map(|(id, key)| {
                history
                    .order
                    .iter()
                    .position(|candidate| *candidate == id)
                    .map(|index| {
                        (
                            index,
                            RenderNode::Block {
                                id,
                                block_index: index,
                            },
                            key,
                        )
                    })
            });
        let jobs = self.collect_compile_jobs(
            history,
            &TranscriptDefaultViewPolicy::default(),
            renderer_generation,
            renderer_cache_key,
            blocks,
        );
        let compiled = jobs.len();
        self.compile_and_insert(env, jobs);
        compiled
    }

    /// Returns compile jobs for cache misses. The caller can run these jobs on
    /// the current thread or schedule them onto a worker pool, then insert the
    /// results with `insert_compiled_blocks`.
    pub(crate) fn collect_compile_jobs(
        &mut self,
        history: &BlockHistory,
        policy: &TranscriptDefaultViewPolicy,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        nodes: impl IntoIterator<Item = (usize, RenderNode, NodeLayoutKey)>,
    ) -> Vec<CompileJob> {
        let _perf = smelt_perf::perf::begin("transcript:layout_cache:ensure_many");

        let mut jobs = Vec::new();
        let mut requested = 0;
        let mut removed_cached_layout = false;
        for (index, node, key) in nodes {
            requested += 1;
            let id = node.id();
            let display_key =
                DisplayCacheKey::from_node_key(key, renderer_generation, renderer_cache_key);
            if self
                .blocks
                .get(&id)
                .is_some_and(|cached| cached.key == display_key)
            {
                self.touch(id);
                continue;
            }
            if let RenderNode::Block { id: block_id, .. } = &node {
                if history.status(*block_id) == Some(Status::Streaming)
                    && self.promote_streaming_content_key(id, display_key)
                {
                    continue;
                }
            }
            let source = match node {
                RenderNode::Block {
                    id: block_id,
                    block_index,
                } => {
                    let Some(block) = history.block(block_id) else {
                        if let Some(removed) = self.blocks.remove(&id) {
                            self.retained_bytes =
                                self.retained_bytes.saturating_sub(removed.weight);
                            removed_cached_layout = true;
                        }
                        self.lru.borrow_mut().retain(|candidate| *candidate != id);
                        continue;
                    };
                    if history.status(block_id) == Some(Status::Streaming) {
                        if let Some(layout) = live_streaming_layout(block) {
                            CompileJobSource::Ready(layout)
                        } else {
                            let Some(value) = block_snapshot_json(
                                history,
                                block_id,
                                block_index,
                                Some(key.view_state),
                            ) else {
                                continue;
                            };
                            CompileJobSource::Snapshot {
                                value,
                                cache_source_views: cache_source_views_for_block(block),
                            }
                        }
                    } else {
                        let Some(value) = block_snapshot_json(
                            history,
                            block_id,
                            block_index,
                            Some(key.view_state),
                        ) else {
                            continue;
                        };
                        CompileJobSource::Snapshot {
                            value,
                            cache_source_views: cache_source_views_for_block(block),
                        }
                    }
                }
                RenderNode::Group(_) => CompileJobSource::Snapshot {
                    value: group_snapshot_json(history, policy, index, &node, key.view_state),
                    cache_source_views: true,
                },
            };
            jobs.push(CompileJob {
                id,
                key: display_key,
                view_state: key.view_state,
                source,
            });
        }
        if removed_cached_layout {
            self.recompute_earliest_refresh_at();
        }
        smelt_perf::perf::record_value("transcript:layout_cache:requested", requested);
        smelt_perf::perf::record_value("transcript:layout_cache:compiled", jobs.len() as u64);
        jobs
    }

    pub(crate) fn compile_and_insert(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        jobs: Vec<CompileJob>,
    ) {
        let _perf = smelt_perf::perf::begin("transcript:layout_cache:compile_and_insert");
        let mut layouts = Vec::with_capacity(jobs.len());
        {
            let _perf = smelt_perf::perf::begin("transcript:layout_cache:compile_layouts");
            for job in jobs {
                layouts.push(job.compile(env.clone(), &mut self.source_views));
            }
        }
        self.insert_compiled_blocks(layouts);
    }

    pub(crate) fn insert_compiled_blocks(
        &mut self,
        layouts: Vec<(
            RenderNodeId,
            DisplayCacheKey,
            LayoutIr,
            Option<std::time::Instant>,
        )>,
    ) {
        let _perf = smelt_perf::perf::begin("transcript:layout_cache:insert_cache");
        for (id, key, layout, refresh_at) in layouts {
            let weight = layout.retained_bytes();
            if let Some(previous) = self.blocks.remove(&id) {
                self.retained_bytes = self.retained_bytes.saturating_sub(previous.weight);
            }
            self.retained_bytes = self.retained_bytes.saturating_add(weight);
            self.blocks.insert(
                id,
                CachedLayout {
                    key,
                    layout,
                    weight,
                    refresh_at,
                    content_hash_pending: false,
                },
            );
            self.touch(id);
        }
        self.enforce_budget();
    }

    pub(crate) fn retain_nodes(&mut self, ids: impl IntoIterator<Item = RenderNodeId>) {
        let live: HashSet<RenderNodeId> = ids.into_iter().collect();
        self.blocks.retain(|id, entry| {
            if live.contains(id) {
                true
            } else {
                self.retained_bytes = self.retained_bytes.saturating_sub(entry.weight);
                false
            }
        });
        self.lru.borrow_mut().retain(|id| live.contains(id));
        self.pinned.retain(|id| live.contains(id));
        self.enforce_budget();
    }

    pub(crate) fn next_refresh_at(&self) -> Option<std::time::Instant> {
        self.earliest_refresh_at
    }

    pub(crate) fn expire_due_refreshes(&mut self, now: std::time::Instant) -> Vec<RenderNodeId> {
        let due = self
            .blocks
            .iter()
            .filter_map(|(id, entry)| {
                entry
                    .refresh_at
                    .filter(|deadline| *deadline <= now)
                    .map(|_| *id)
            })
            .collect::<Vec<_>>();
        for id in &due {
            if let Some(entry) = self.blocks.remove(id) {
                self.retained_bytes = self.retained_bytes.saturating_sub(entry.weight);
            }
        }
        if !due.is_empty() {
            let due_set = due.iter().copied().collect::<HashSet<_>>();
            self.lru.borrow_mut().retain(|id| !due_set.contains(id));
            self.recompute_earliest_refresh_at();
        }
        due
    }

    pub(crate) fn get(
        &self,
        id: RenderNodeId,
        key: NodeLayoutKey,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> Option<&LayoutIr> {
        let display_key =
            DisplayCacheKey::from_node_key(key, renderer_generation, renderer_cache_key);
        let layout = self
            .blocks
            .get(&id)
            .filter(|cached| cached.key == display_key)
            .map(|cached| &cached.layout)?;
        self.touch(id);
        Some(layout)
    }
}

fn cache_source_views_for_block(block: &Block) -> bool {
    !matches!(
        block,
        Block::ToolDraft {
            finished: false,
            ..
        }
    )
}

pub(crate) fn live_streaming_markdown_content(block: &Block) -> Option<&str> {
    match block {
        Block::Text { content } => Some(content),
        Block::Thinking {
            title: None,
            summary_titles,
            content,
            ..
        } if summary_titles.is_empty() => Some(content),
        _ => None,
    }
}

fn live_streaming_layout(block: &Block) -> Option<LayoutIr> {
    let content = live_streaming_markdown_content(block)?;
    match block {
        Block::Text { .. } => Some(markdown_layout(content.to_owned(), false, false)),
        Block::Thinking { .. } => Some(BlockLayout::Gutter {
            child: Box::new(BlockLayout::Style {
                child: Box::new(markdown_layout(content.to_owned(), true, true)),
                spec: StyleSpec {
                    dim: true,
                    italic: true,
                    ..StyleSpec::default()
                },
            }),
            spec: GutterSpec {
                text: "│ ".into(),
                styled: true,
            },
        }),
        _ => None,
    }
}

fn markdown_layout(content: String, dim: bool, italic: bool) -> LayoutIr {
    BlockLayout::Leaf(IrLeaf::Markdown(MarkdownSpec {
        content,
        dim,
        italic,
        inline: false,
    }))
}

fn markdown_spec_count(layout: &LayoutIr) -> usize {
    match layout {
        BlockLayout::Empty => 0,
        BlockLayout::Leaf(IrLeaf::Markdown(_)) => 1,
        BlockLayout::Leaf(_) => 0,
        BlockLayout::Vbox(items) => items.iter().map(markdown_spec_count).sum(),
        BlockLayout::Hbox(items) => items
            .iter()
            .map(|item| markdown_spec_count(&item.layout))
            .sum(),
        BlockLayout::Gutter { child, .. }
        | BlockLayout::RowPrefix { child, .. }
        | BlockLayout::Panel { child, .. }
        | BlockLayout::Style { child, .. }
        | BlockLayout::Cap { child, .. }
        | BlockLayout::Refresh { child, .. } => markdown_spec_count(child),
    }
}

fn first_markdown_spec_mut(layout: &mut LayoutIr) -> Option<&mut MarkdownSpec> {
    match layout {
        BlockLayout::Leaf(IrLeaf::Markdown(spec)) => Some(spec),
        BlockLayout::Vbox(items) => items.iter_mut().find_map(first_markdown_spec_mut),
        BlockLayout::Hbox(items) => items
            .iter_mut()
            .find_map(|item| first_markdown_spec_mut(&mut item.layout)),
        BlockLayout::Gutter { child, .. }
        | BlockLayout::RowPrefix { child, .. }
        | BlockLayout::Panel { child, .. }
        | BlockLayout::Style { child, .. }
        | BlockLayout::Cap { child, .. }
        | BlockLayout::Refresh { child, .. } => first_markdown_spec_mut(child),
        BlockLayout::Empty | BlockLayout::Leaf(_) => None,
    }
}

fn single_markdown_spec_mut(layout: &mut LayoutIr) -> Option<&mut MarkdownSpec> {
    (markdown_spec_count(layout) == 1)
        .then(|| first_markdown_spec_mut(layout))
        .flatten()
}

#[cfg(test)]
pub(crate) fn compile_block(block: &Block) -> LayoutIr {
    let lua = LuaRuntime::new();
    let mut source_views = SourceViewCache::default();
    let mut cache = CompileLayoutCache {
        source_views: &mut source_views,
        source_views_enabled: true,
        refresh_after_ms: None,
    };
    let snapshot =
        smelt_core::lua::runtime::transcript_block_snapshot_json(BlockId::new(0), 0, block, None)
            .expect("block snapshot");
    compile_node_with_lua(
        TranscriptRenderEnv::new(&lua),
        &snapshot,
        ViewState::Expanded,
        &mut cache,
    )
    .layout
}

fn group_snapshot_json(
    history: &BlockHistory,
    policy: &TranscriptDefaultViewPolicy,
    node_index: usize,
    node: &RenderNode,
    view_state: ViewState,
) -> serde_json::Value {
    let RenderNode::Group(group) = node else {
        return serde_json::Value::Null;
    };
    let children: Vec<_> = group
        .child_range
        .clone()
        .filter_map(|block_index| {
            let id = *history.order.get(block_index)?;
            let child_view_state =
                policy.node_default_view_state(history, &RenderNode::Block { id, block_index });
            block_snapshot_json(history, id, block_index, Some(child_view_state))
        })
        .collect();
    serde_json::json!({
        "kind": "group",
        "id": group.id,
        "index": node_index,
        "group_kind": group.name,
        "name": group.name,
        "bucket": group.bucket,
        "view_state": view_state_label(view_state),
        "children": children,
        "child_ids": group.child_ids,
        "child_count": group.child_ids.len(),
    })
}

fn block_snapshot_json(
    history: &BlockHistory,
    id: smelt_core::transcript_model::BlockId,
    block_index: usize,
    view_state: Option<ViewState>,
) -> Option<serde_json::Value> {
    let block = history.block(id)?;
    let state = match block {
        Block::ToolCall { .. } => history.tool_state(id),
        _ => None,
    };
    let serde_json::Value::Object(mut value) =
        smelt_core::lua::runtime::transcript_block_snapshot_json(id, block_index, block, state)
            .ok()?
    else {
        return None;
    };
    if let Some(view_state) = view_state {
        value.insert(
            "view_state".into(),
            serde_json::json!(view_state_label(view_state)),
        );
    }
    Some(serde_json::Value::Object(value))
}

fn view_state_label(view_state: ViewState) -> &'static str {
    match view_state {
        ViewState::Expanded => "expanded",
        ViewState::Peek => "peek",
        ViewState::Collapsed => "collapsed",
        ViewState::TrimmedHead { .. } => "trimmed_head",
        ViewState::TrimmedTail { .. } => "trimmed_tail",
    }
}

fn compile_node_with_lua(
    env: TranscriptRenderEnv<'_>,
    snapshot: &serde_json::Value,
    view_state: ViewState,
    cache: &mut CompileLayoutCache<'_>,
) -> CompiledLayout {
    let kind = snapshot
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let index = snapshot
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let layout = env
        .lua
        .render_transcript_layout(snapshot, view_state, env.now_ms);
    match compile_layout_ir_with_cache(&layout, cache) {
        Ok(layout) => CompiledLayout {
            layout,
            refresh_after_ms: cache.refresh_after_ms,
        },
        Err(error) => {
            env.lua.record_error(format!(
                "transcript render `{kind}` #{index}: compile layout IR: {error}"
            ));
            CompiledLayout {
                layout: BlockLayout::Leaf(IrLeaf::Text(TextSpec {
                    content: format!("{kind} render error"),
                    hl_group: Some("ErrorMsg".into()),
                    ansi: false,
                })),
                refresh_after_ms: None,
            }
        }
    }
}

pub(crate) fn compile_layout_ir(layout: &BlockLayout) -> Result<LayoutIr, String> {
    let mut source_views = SourceViewCache::default();
    let mut cache = CompileLayoutCache {
        source_views: &mut source_views,
        source_views_enabled: true,
        refresh_after_ms: None,
    };
    compile_layout_ir_with_cache(layout, &mut cache)
}

fn compile_layout_ir_with_cache(
    layout: &BlockLayout,
    cache: &mut CompileLayoutCache<'_>,
) -> Result<LayoutIr, String> {
    match layout {
        BlockLayout::Empty => Ok(BlockLayout::Empty),
        BlockLayout::Leaf(LuaLeaf::Text(spec)) => Ok(BlockLayout::Leaf(IrLeaf::Text(TextSpec {
            content: spec.content.clone(),
            hl_group: spec.hl_group.clone(),
            ansi: spec.ansi,
        }))),
        BlockLayout::Leaf(LuaLeaf::Runs(spec)) => Ok(BlockLayout::Leaf(IrLeaf::Runs(spec.clone()))),
        BlockLayout::Leaf(LuaLeaf::Line(spec)) => Ok(BlockLayout::Leaf(IrLeaf::Line(spec.clone()))),
        BlockLayout::Leaf(LuaLeaf::Markdown(spec)) => {
            Ok(BlockLayout::Leaf(IrLeaf::Markdown(spec.clone())))
        }
        BlockLayout::Leaf(LuaLeaf::Code(spec)) => Ok(BlockLayout::Leaf(IrLeaf::Code(spec.clone()))),
        BlockLayout::Leaf(LuaLeaf::Separator(spec)) => {
            Ok(BlockLayout::Leaf(IrLeaf::Separator(spec.clone())))
        }
        BlockLayout::Leaf(LuaLeaf::Diff(spec)) => cached_source_view(
            cache.source_views,
            cache.source_views_enabled,
            smelt_core::utils::hash_serializable(&("diff", spec)),
            || {
                let ext = spec
                    .lang
                    .as_deref()
                    .map(smelt_core::content::highlight::lang_to_ext);
                let ir = if spec.full_file {
                    smelt_core::content::highlight::build_diff_ir_ext_with_base(
                        &spec.old,
                        &spec.new,
                        &spec.path,
                        &spec.anchor,
                        ext,
                        Some(&spec.old),
                    )
                } else {
                    smelt_core::content::highlight::build_diff_ir_ext_with_source(
                        &spec.old,
                        &spec.new,
                        &spec.path,
                        &spec.anchor,
                        ext,
                        &spec.base,
                    )
                };
                SourceViewIr::Diff(ir)
            },
        ),
        BlockLayout::Leaf(LuaLeaf::FileView(spec)) => cached_source_view(
            cache.source_views,
            cache.source_views_enabled,
            smelt_core::utils::hash_serializable(&("file_view", spec)),
            || {
                let ext = spec
                    .lang
                    .as_deref()
                    .map(smelt_core::content::highlight::lang_to_ext)
                    .or_else(|| {
                        std::path::Path::new(&spec.path)
                            .extension()
                            .and_then(|e| e.to_str())
                    });
                SourceViewIr::Diff(smelt_core::content::highlight::build_file_view_ir(
                    &spec.content,
                    ext,
                ))
            },
        ),
        BlockLayout::Leaf(LuaLeaf::SourceView(ir)) => {
            Ok(BlockLayout::Leaf(IrLeaf::SourceView(ir.clone())))
        }
        BlockLayout::Vbox(items) => items
            .iter()
            .map(|layout| compile_layout_ir_with_cache(layout, cache))
            .collect::<Result<Vec<_>, _>>()
            .map(BlockLayout::Vbox),
        BlockLayout::Hbox(items) => items
            .iter()
            .map(|item| {
                Ok(HboxItem {
                    constraint: item.constraint,
                    layout: compile_layout_ir_with_cache(&item.layout, cache)?,
                    copy_owner: item.copy_owner,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(BlockLayout::Hbox),
        BlockLayout::Gutter { child, spec } => Ok(BlockLayout::Gutter {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::RowPrefix { child, spec } => Ok(BlockLayout::RowPrefix {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::Panel { child, spec } => Ok(BlockLayout::Panel {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::Style { child, spec } => Ok(BlockLayout::Style {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::Cap { child, spec } => Ok(BlockLayout::Cap {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::Refresh { child, spec } => {
            cache.refresh_after_ms = Some(
                cache
                    .refresh_after_ms
                    .map_or(spec.after_ms, |current| current.min(spec.after_ms)),
            );
            compile_layout_ir_with_cache(child, cache)
        }
    }
}

fn cached_source_view(
    source_views: &mut SourceViewCache,
    enabled: bool,
    key: u64,
    build: impl FnOnce() -> SourceViewIr,
) -> Result<LayoutIr, String> {
    if enabled {
        if let Some(ir) = source_views.get(key) {
            return Ok(BlockLayout::Leaf(IrLeaf::SourceView(ir.clone())));
        }
    }

    let ir = build();
    if enabled {
        source_views.insert(key, ir.clone());
    }
    Ok(BlockLayout::Leaf(IrLeaf::SourceView(ir)))
}

pub(crate) fn measure_block(layout: &LayoutIr, ctx: MeasureCtx) -> u64 {
    let _perf = smelt_perf::perf::begin("transcript:measure_block:layout");
    let expanded_rows = crate::content::display_renderers::measure_layout_ir_with_options(
        layout,
        ctx.width,
        &ctx.inline_options,
    ) as u64;
    ctx.view_state.measured_height(expanded_rows)
}

pub(crate) fn render_block_into(
    buf: &mut Buffer,
    layout: &LayoutIr,
    ctx: RenderCtx<'_>,
) -> Outcome {
    let outcome = {
        let mut out = LineBuilder::new(buf, ctx.theme, ctx.width);
        render_expanded_block(
            &mut out,
            layout,
            ctx.width as usize,
            ctx.history,
            &ctx.inline_options,
        );
        out.finish()
    };
    apply_view_state(buf, ctx.theme, ctx.width, ctx.view_state, outcome)
}

pub(crate) fn render_block_range_into(
    buf: &mut Buffer,
    layout: &LayoutIr,
    ctx: RenderCtx<'_>,
    row_start: usize,
    row_count: usize,
) -> Outcome {
    if row_count == 0 {
        return Outcome::default();
    }
    if ctx.view_state != ViewState::Expanded || row_start > u16::MAX as usize {
        let outcome = render_block_into(buf, layout, ctx);
        let start = row_start.min(outcome.line_count);
        let end = start.saturating_add(row_count).min(outcome.line_count);
        buf.set_lines(end, outcome.line_count, vec![]);
        buf.set_lines(0, start, vec![]);
        return Outcome {
            line_count: end.saturating_sub(start),
            ..outcome
        };
    }
    let row_start = row_start.min(u16::MAX as usize) as u16;
    let row_count = row_count.min(u16::MAX as usize) as u16;
    let mut out = LineBuilder::new(buf, ctx.theme, ctx.width);
    render_expanded_block_range(
        &mut out,
        layout,
        ctx.width,
        row_start,
        row_count,
        ctx.history,
        &ctx.inline_options,
    );
    out.finish()
}

fn render_expanded_block(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: usize,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:layout");
    if let Some(history) = history {
        crate::content::display_renderers::render_layout_ir_into_with_history(
            out,
            layout,
            width as u16,
            history,
            inline_options,
        )
    } else {
        crate::content::display_renderers::render_layout_ir_into(out, layout, width as u16)
    }
}

fn render_expanded_block_range(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: u16,
    row_start: u16,
    row_count: u16,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:layout:range");
    if let Some(history) = history {
        crate::content::display_renderers::render_layout_ir_range_into_with_history(
            out,
            layout,
            width,
            row_start,
            row_count,
            history,
            inline_options,
        )
    } else {
        crate::content::display_renderers::render_layout_ir_range_into(
            out,
            layout,
            width,
            row_start,
            row_count,
            inline_options,
        )
    }
}

fn apply_view_state(
    buf: &mut Buffer,
    theme: &Theme,
    width: u16,
    state: ViewState,
    outcome: Outcome,
) -> Outcome {
    let total = outcome.line_count;
    let target_total = state.measured_height(total as u64) as usize;
    let start = buf.line_count().saturating_sub(total);
    match state {
        ViewState::Expanded | ViewState::Peek => outcome,
        ViewState::Collapsed => {
            if state.elides_rows(total as u64) {
                let hidden = total - 1;
                buf.set_lines(start + 1, start + total, vec![]);
                let after_truncate_outcome = Outcome {
                    line_count: 1,
                    ..outcome
                };
                let with_ellipsis = append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines"),
                    after_truncate_outcome,
                );
                Outcome {
                    line_count: target_total,
                    ..with_ellipsis
                }
            } else {
                outcome
            }
        }
        ViewState::TrimmedHead { keep } => {
            let keep = keep as usize;
            if state.elides_rows(total as u64) {
                let hidden = total - keep;
                buf.set_lines(start + keep, start + total, vec![]);
                let after_truncate_outcome = Outcome {
                    line_count: keep,
                    ..outcome
                };
                let with_ellipsis = append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines"),
                    after_truncate_outcome,
                );
                Outcome {
                    line_count: target_total,
                    ..with_ellipsis
                }
            } else {
                outcome
            }
        }
        ViewState::TrimmedTail { keep } => {
            let keep = keep as usize;
            if state.elides_rows(total as u64) {
                let hidden = total - keep;
                buf.set_lines(start, start + (total - keep), vec![]);
                let mut kept_lines: Vec<String> = (0..keep)
                    .map(|i| buf.get_line(start + i).unwrap_or("").to_string())
                    .collect();
                let kept_decorations: Vec<_> = (0..keep)
                    .map(|i| buf.decoration_at(start + i).clone())
                    .collect();
                let kept_highlights: Vec<_> =
                    (0..keep).map(|i| buf.highlights_at(start + i)).collect();
                buf.set_lines(start, start + keep, vec![]);
                append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines above"),
                    Outcome {
                        line_count: 0,
                        ..outcome
                    },
                );
                let cur_len = buf.line_count();
                buf.set_lines(cur_len, cur_len, std::mem::take(&mut kept_lines));
                for (i, hl_list) in kept_highlights.into_iter().enumerate() {
                    let row = cur_len + i;
                    for span in hl_list {
                        buf.add_highlight_group_with_meta(
                            row,
                            span.col_start,
                            span.col_end,
                            span.hl,
                            span.meta,
                        );
                    }
                }
                for (i, dec) in kept_decorations.into_iter().enumerate() {
                    if dec != smelt_core::buffer::LineDecoration::default() {
                        buf.set_decoration(cur_len + i, dec);
                    }
                }
                Outcome {
                    line_count: target_total,
                    ..outcome
                }
            } else {
                outcome
            }
        }
    }
}

fn append_ellipsis(
    buf: &mut Buffer,
    theme: &Theme,
    width: u16,
    text: &str,
    outcome: Outcome,
) -> Outcome {
    let added = {
        let mut col = LineBuilder::new(buf, theme, width);
        col.push_dim();
        col.push_hl(intern("Comment"));
        col.print(text);
        col.pop_style();
        col.pop_style();
        col.newline();
        col.finish()
    };
    Outcome {
        line_count: outcome.line_count + added.line_count,
        was_wrapped: outcome.was_wrapped || added.was_wrapped,
        max_line_width: outcome.max_line_width.max(added.max_line_width),
        layout_width: outcome.layout_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_edit::{BufCreateOpts, BufId};
    use smelt_core::content::block_layout::RunsSpec;
    use smelt_core::content::transcript::Transcript;
    use smelt_core::transcript_model::LayoutKey;

    fn base_key(history: &BlockHistory, id: BlockId) -> NodeLayoutKey {
        history.resolve_key(
            id,
            LayoutKey {
                width: 80,
                view_state: ViewState::Expanded,
                content_hash: 0,
                sidecar_hash: 0,
            },
        )
    }

    fn rendered_rows(block: &LayoutIr, width: u16) -> u64 {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        render_block_into(
            &mut buf,
            block,
            RenderCtx {
                width,
                view_state: ViewState::Expanded,
                theme: &theme,
                history: None,
                inline_options: Default::default(),
            },
        )
        .line_count as u64
    }

    fn measured_rows(block: &LayoutIr, width: u16) -> u64 {
        measure_block(
            block,
            MeasureCtx {
                width,
                view_state: ViewState::Expanded,
                inline_options: Default::default(),
            },
        )
    }

    #[test]
    fn block_snapshot_json_exposes_process_status_event_fields() {
        let mut transcript = Transcript::new();
        transcript.push(Block::ProcessStatus {
            text: "background process 42 exited with code 7".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "42",
                Some(7),
            )),
        });

        let id = transcript.history.order[0];
        let snapshot = block_snapshot_json(&transcript.history, id, 0, None).expect("snapshot");

        assert_eq!(snapshot["kind"], "process_status");
        assert_eq!(snapshot["event"], "background_process_completed");
        assert_eq!(snapshot["event_type"], "background_process_completed");
        assert_eq!(
            snapshot["event_data"]["event"],
            "background_process_completed"
        );
        assert_eq!(snapshot["event_data"]["process_id"], "42");
        assert_eq!(snapshot["process_id"], "42");
        assert_eq!(snapshot["exit_code"], 7);
    }
    #[test]
    fn static_block_measurement_matches_rendered_rows() {
        let blocks = [
            Block::Text {
                content: "# Heading\n\nParagraph with **bold** text that wraps across several rows at narrow widths.\n\n```rust\nfn main() { println!(\"hello\"); }\n```\n\n| col | val |\n| --- | --- |\n| a | table cell that wraps |"
                    .into(),
            },
            Block::User {
                text: "Please inspect @crates/tui/src/content/display_layout.rs and this long line that wraps."
                    .into(),
                image_labels: vec![],
                command: false,
            },
            Block::ProcessStatus {
                text: "running a long process status that wraps on narrow terminals".into(),
                event: None,
            },
            Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "**Plan**\nThink through a long line that wraps in expanded thinking mode.".into(),
            },
            Block::Exec {
                command: "echo a very long shell command that wraps".into(),
                output: "output line that is also long enough to wrap in the transcript".into(),
            },
            Block::Compacted {
                summary: "A compacted **summary** with enough text to wrap.".into(),
            },
            Block::Mode {
                text: "plan".into(),
                icon: "◈ ".into(),
                hl_group: "SmeltAccent".into(),
            },
        ];

        for block in blocks {
            let display = compile_block(&block);
            assert_eq!(
                measured_rows(&display, 36),
                rendered_rows(&display, 36),
                "measurement mismatch for {block:?}"
            );
        }
    }

    #[test]
    fn layout_ir_measurement_matches_rendered_rows() {
        let display = BlockLayout::Vbox(vec![
            BlockLayout::Leaf(IrLeaf::Runs(RunsSpec {
                lines: protocol::StyledLines(vec![vec![protocol::StyledSpan {
                    text: "echo hello && echo world && echo done".into(),
                    syntax: Some("bash".into()),
                    ..Default::default()
                }]]),
                hl_group: Some("SmeltToolPending".into()),
                continuation_indent: 0,
            })),
            BlockLayout::Leaf(IrLeaf::Text(TextSpec {
                content: "output line that wraps at narrow widths".into(),
                hl_group: None,
                ansi: false,
            })),
        ]);

        assert_eq!(measured_rows(&display, 24), rendered_rows(&display, 24));
    }

    #[test]
    fn layout_cache_lru_accounts_replacement_access_and_pins() {
        let mut model = LayoutCache::new();
        model.set_budget(usize::MAX);
        let first = RenderNodeId::Block(BlockId::new(1));
        let second = RenderNodeId::Block(BlockId::new(2));
        let key = DisplayCacheKey::new(1, 0, 0, None, 0);
        let small = compile_block(&Block::Text {
            content: "small layout".into(),
        });
        let weight = small.retained_bytes();

        model.insert_compiled_blocks(vec![
            (first, key, small.clone(), None),
            (second, key, small.clone(), None),
        ]);
        assert_eq!(model.retained_bytes, weight * 2);
        model.touch(first);
        model.set_budget(weight);
        assert!(model.blocks.contains_key(&first));
        assert!(!model.blocks.contains_key(&second));
        assert_eq!(model.retained_bytes, weight);

        let replacement = compile_block(&Block::Text {
            content: "replacement ".repeat(128),
        });
        let replacement_weight = replacement.retained_bytes();
        model.set_budget(usize::MAX);
        model.insert_compiled_blocks(vec![(first, key, replacement, None)]);
        assert_eq!(model.blocks.len(), 1);
        assert_eq!(model.retained_bytes, replacement_weight);

        model.set_pinned_nodes([first]);
        model.set_budget(0);
        let pinned = model.memory_snapshot();
        assert_eq!(pinned.pinned_layout_bytes, replacement_weight);
        assert_eq!(pinned.oversize_debt_bytes, replacement_weight);
        assert!(model.blocks.contains_key(&first));
        model.set_pinned_nodes(std::iter::empty());
        assert!(model.blocks.is_empty());
        assert_eq!(model.retained_bytes(), 0);
    }

    #[test]
    fn layout_cache_caches_width_independent_blocks() {
        let mut transcript = Transcript::new();
        transcript.push(Block::CodeLine {
            content: "fn main() {}".into(),
            lang: "rust".into(),
        });
        let id = transcript.history.order[0];
        let key = base_key(&transcript.history, id);
        let mut narrow = key;
        narrow.width = 40;

        let lua = smelt_core::lua::runtime::LuaRuntime::new();
        let mut model = LayoutCache::new();
        assert_eq!(
            model.ensure_many(
                TranscriptRenderEnv::new(&lua),
                &transcript.history,
                &[id],
                &[key],
            ),
            1
        );
        assert_eq!(
            model.ensure_many(
                TranscriptRenderEnv::new(&lua),
                &transcript.history,
                &[id],
                &[narrow],
            ),
            0
        );
    }

    #[test]
    fn refresh_compilation_is_visually_transparent_and_uses_earliest_delay() {
        let child = BlockLayout::Leaf(LuaLeaf::Runs(RunsSpec {
            lines: protocol::StyledLines(vec![vec![protocol::StyledSpan {
                text: "select me".into(),
                selectable: true,
                ..Default::default()
            }]]),
            hl_group: Some("SmeltAccent".into()),
            continuation_indent: 2,
        }));
        let layout = BlockLayout::Refresh {
            child: Box::new(BlockLayout::Vbox(vec![
                BlockLayout::Refresh {
                    child: Box::new(child),
                    spec: smelt_core::content::block_layout::RefreshSpec { after_ms: 400 },
                },
                BlockLayout::Refresh {
                    child: Box::new(BlockLayout::Empty),
                    spec: smelt_core::content::block_layout::RefreshSpec { after_ms: 75 },
                },
            ])),
            spec: smelt_core::content::block_layout::RefreshSpec { after_ms: 250 },
        };
        let mut source_views = SourceViewCache::default();
        let mut cache = CompileLayoutCache {
            source_views: &mut source_views,
            source_views_enabled: true,
            refresh_after_ms: None,
        };

        let compiled = compile_layout_ir_with_cache(&layout, &mut cache).unwrap();

        assert_eq!(cache.refresh_after_ms, Some(75));
        assert_eq!(measured_rows(&compiled, 10), rendered_rows(&compiled, 10));
        let BlockLayout::Vbox(items) = compiled else {
            panic!("refresh wrappers must be stripped from display IR");
        };
        let BlockLayout::Leaf(IrLeaf::Runs(spec)) = &items[0] else {
            panic!("refresh child layout changed during compilation");
        };
        assert_eq!(spec.lines.0[0][0].text, "select me");
        assert!(spec.lines.0[0][0].selectable);
        assert_eq!(spec.continuation_indent, 2);
    }

    #[test]
    fn refresh_deadlines_are_replaced_removed_retained_and_evicted_with_nodes() {
        let start = std::time::Instant::now();
        let key = DisplayCacheKey::new(1, 0, 0, None, 0);
        let layout = compile_block(&Block::Text {
            content: "cached".into(),
        });
        let first = RenderNodeId::Block(BlockId::new(1));
        let second = RenderNodeId::Block(BlockId::new(2));
        let static_node = RenderNodeId::Block(BlockId::new(3));
        let mut model = LayoutCache::new();
        model.set_budget(usize::MAX);
        model.insert_compiled_blocks(vec![
            (
                first,
                key,
                layout.clone(),
                Some(start + std::time::Duration::from_millis(100)),
            ),
            (
                second,
                key,
                layout.clone(),
                Some(start + std::time::Duration::from_millis(200)),
            ),
            (static_node, key, layout.clone(), None),
        ]);

        assert_eq!(
            model.next_refresh_at(),
            Some(start + std::time::Duration::from_millis(100))
        );
        assert!(model
            .expire_due_refreshes(start + std::time::Duration::from_millis(99))
            .is_empty());
        assert_eq!(
            model.expire_due_refreshes(start + std::time::Duration::from_millis(100)),
            vec![first]
        );
        assert!(model.blocks.contains_key(&second));
        assert!(model.blocks.contains_key(&static_node));

        model.insert_compiled_blocks(vec![(second, key, layout.clone(), None)]);
        assert_eq!(model.next_refresh_at(), None);

        model.insert_compiled_blocks(vec![(
            first,
            key,
            layout.clone(),
            Some(start + std::time::Duration::from_secs(1)),
        )]);
        model.retain_nodes([static_node]);
        assert_eq!(model.next_refresh_at(), None);

        model.insert_compiled_blocks(vec![(
            first,
            key,
            layout,
            Some(start + std::time::Duration::from_secs(1)),
        )]);
        model.set_budget(0);
        assert!(model.blocks.is_empty());
        assert_eq!(model.next_refresh_at(), None);
    }

    #[test]
    fn due_refresh_recompiles_only_its_top_level_node() {
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "dynamic".into(),
            image_labels: Vec::new(),
            command: false,
        });
        transcript.push(Block::User {
            text: "static".into(),
            image_labels: Vec::new(),
            command: false,
        });
        let ids = transcript.history.order.clone();
        let keys = ids
            .iter()
            .map(|id| base_key(&transcript.history, *id))
            .collect::<Vec<_>>();
        let lua = smelt_core::lua::runtime::LuaRuntime::new();
        lua.lua
            .load(
                r#"
                smelt.transcript.set_renderer(function(node)
                  local view = smelt.layout.text(node.text)
                  if node.text == "dynamic" then
                    return smelt.layout.refresh(view, { after_ms = 100 })
                  end
                  return view
                end)
                "#,
            )
            .exec()
            .unwrap();
        let renderer_generation = lua.transcript_renderer_generation();
        let renderer_cache_key = lua.transcript_renderer_cache_key();
        let start = std::time::Instant::now();
        let env = TranscriptRenderEnv {
            lua: &lua,
            renderer_generation,
            renderer_cache_key,
            now_ms: 1_700_000_000_000,
            refresh_now: start,
        };
        let mut model = LayoutCache::new();

        assert_eq!(
            model.ensure_many(env.clone(), &transcript.history, &ids, &keys),
            2
        );
        assert_eq!(
            model.next_refresh_at(),
            Some(start + std::time::Duration::from_millis(100))
        );
        assert_eq!(
            model.expire_due_refreshes(start + std::time::Duration::from_millis(100)),
            vec![RenderNodeId::Block(ids[0])]
        );
        assert_eq!(
            model.ensure_many(
                TranscriptRenderEnv {
                    refresh_now: start + std::time::Duration::from_millis(100),
                    ..env
                },
                &transcript.history,
                &ids,
                &keys,
            ),
            1
        );
        assert_eq!(model.blocks.len(), 2);
    }
}
