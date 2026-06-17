use crate::content::render_plan::{
    NodeLayoutKey, RenderNode, RenderNodeId, TranscriptDefaultViewPolicy,
};
use crate::smelt_edit::{Buffer, Theme};
use smelt_core::content::block_layout::{
    BlockLayout, HboxItem, IrLeaf, LayoutIr, LuaLeaf, SourceViewIr, TextSpec,
};
use smelt_core::content::builder::{LineBuilder, Outcome};
use smelt_core::content::highlight::InlineOptions;
use smelt_core::lua::runtime::LuaRuntime;
use smelt_core::theme::intern;
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, ToolState, ViewState};
use std::collections::{HashMap, HashSet};

pub(crate) const DISPLAY_RENDERER_VERSION: u64 = 10;

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
}

#[derive(Clone)]
pub(crate) struct TranscriptRenderEnv<'a> {
    pub(crate) lua: &'a LuaRuntime,
    pub(crate) renderer_generation: u64,
    pub(crate) renderer_cache_key: Option<u64>,
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
        }
    }
}

pub(crate) enum CompileJob {
    Block {
        id: RenderNodeId,
        block_id: BlockId,
        index: usize,
        key: DisplayCacheKey,
        view_state: ViewState,
        block: Block,
        state: Option<ToolState>,
        cache_source_views: bool,
    },
    Group {
        id: RenderNodeId,
        name: String,
        key: DisplayCacheKey,
        view_state: ViewState,
        snapshot: serde_json::Value,
    },
}

impl CompileJob {
    fn compile(
        self,
        env: TranscriptRenderEnv<'_>,
        source_views: &mut SourceViewCache,
    ) -> (RenderNodeId, DisplayCacheKey, LayoutIr) {
        match self {
            Self::Block {
                id,
                block_id,
                index,
                key,
                view_state,
                block,
                state,
                cache_source_views,
            } => {
                let mut cache = CompileLayoutCache {
                    source_views,
                    source_views_enabled: cache_source_views,
                };
                (
                    id,
                    key,
                    compile_block_with_lua(
                        env,
                        block_id,
                        index,
                        &block,
                        state.as_ref(),
                        view_state,
                        &mut cache,
                    ),
                )
            }
            Self::Group {
                id,
                name,
                key,
                view_state,
                snapshot,
            } => (
                id,
                key,
                compile_group_with_lua(env, &name, &snapshot, view_state),
            ),
        }
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
}

type SourceViewCache = HashMap<u64, SourceViewIr>;

struct CompileLayoutCache<'a> {
    source_views: &'a mut SourceViewCache,
    source_views_enabled: bool,
}

#[derive(Default)]
pub(crate) struct DisplayModel {
    blocks: HashMap<RenderNodeId, CachedLayout>,
    source_views: SourceViewCache,
}

impl DisplayModel {
    pub(crate) fn new() -> Self {
        Self::default()
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
        let _perf = smelt_perf::perf::begin("transcript:display_model:ensure_many");

        let mut jobs = Vec::new();
        let mut requested = 0;
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
                continue;
            }
            match node {
                RenderNode::Block { id: block_id, .. } => {
                    let Some(block) = history.block(block_id).cloned() else {
                        self.blocks.remove(&id);
                        continue;
                    };
                    let state = match &block {
                        Block::ToolCall { call_id, .. } => history.tool_state(call_id).cloned(),
                        _ => None,
                    };
                    let cache_source_views = cache_source_views_for_block(&block);
                    jobs.push(CompileJob::Block {
                        id,
                        block_id,
                        index,
                        key: display_key,
                        view_state: key.view_state,
                        block,
                        state,
                        cache_source_views,
                    });
                }
                RenderNode::Group { ref name, .. } => {
                    let snapshot =
                        group_snapshot_json(history, policy, index, &node, key.view_state);
                    jobs.push(CompileJob::Group {
                        id,
                        name: name.clone(),
                        key: display_key,
                        view_state: key.view_state,
                        snapshot,
                    });
                }
            }
        }
        smelt_perf::perf::record_value("transcript:display_model:requested", requested);
        smelt_perf::perf::record_value("transcript:display_model:compiled", jobs.len() as u64);
        jobs
    }

    pub(crate) fn compile_and_insert(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        jobs: Vec<CompileJob>,
    ) {
        let mut layouts = Vec::with_capacity(jobs.len());
        for job in jobs {
            layouts.push(job.compile(env.clone(), &mut self.source_views));
        }
        self.insert_compiled_blocks(layouts);
    }

    pub(crate) fn insert_compiled_blocks(
        &mut self,
        layouts: Vec<(RenderNodeId, DisplayCacheKey, LayoutIr)>,
    ) {
        for (id, key, layout) in layouts {
            self.blocks.insert(id, CachedLayout { key, layout });
        }
    }

    pub(crate) fn retain_nodes(&mut self, ids: impl IntoIterator<Item = RenderNodeId>) {
        let live: HashSet<RenderNodeId> = ids.into_iter().collect();
        self.blocks.retain(|id, _| live.contains(id));
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
        self.blocks
            .get(&id)
            .filter(|cached| cached.key == display_key)
            .map(|cached| &cached.layout)
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

#[cfg(test)]
pub(crate) fn compile_block(block: &Block) -> LayoutIr {
    let lua = LuaRuntime::new();
    let mut source_views = SourceViewCache::default();
    let mut cache = CompileLayoutCache {
        source_views: &mut source_views,
        source_views_enabled: true,
    };
    compile_block_with_lua(
        TranscriptRenderEnv::new(&lua),
        BlockId::new(0),
        0,
        block,
        None,
        ViewState::Expanded,
        &mut cache,
    )
}

fn compile_group_with_lua(
    env: TranscriptRenderEnv<'_>,
    name: &str,
    snapshot: &serde_json::Value,
    view_state: ViewState,
) -> LayoutIr {
    let layout = env
        .lua
        .render_transcript_group_layout(name, snapshot, view_state);
    match compile_layout_ir(&layout) {
        Ok(layout) => layout,
        Err(e) => {
            env.lua.record_error(format!(
                "transcript group render `{name}`: compile layout IR: {e}"
            ));
            BlockLayout::Leaf(IrLeaf::Text(TextSpec {
                content: format!("{name} group render error"),
                hl_group: Some("ErrorMsg".into()),
                ansi: false,
            }))
        }
    }
}

fn group_snapshot_json(
    history: &BlockHistory,
    policy: &TranscriptDefaultViewPolicy,
    node_index: usize,
    node: &RenderNode,
    view_state: ViewState,
) -> serde_json::Value {
    let RenderNode::Group {
        id,
        name,
        bucket,
        child_range,
        child_ids,
        ..
    } = node
    else {
        return serde_json::Value::Null;
    };
    let children: Vec<_> = child_range
        .clone()
        .filter_map(|block_index| {
            let id = *history.order.get(block_index)?;
            let child_view_state =
                policy.node_default_view_state(history, &RenderNode::Block { id, block_index });
            block_snapshot_json(history, block_index, Some(child_view_state))
        })
        .collect();
    serde_json::json!({
        "kind": "group",
        "id": id,
        "index": node_index,
        "group_kind": name,
        "name": name,
        "bucket": bucket,
        "view_state": view_state_label(view_state),
        "children": children,
        "child_ids": child_ids,
        "child_count": child_ids.len(),
    })
}

fn insert_process_status_event_json_fields(
    value: &mut serde_json::Map<String, serde_json::Value>,
    event: Option<&protocol::ProcessStatusEvent>,
) {
    let Some(event) = event else {
        return;
    };
    value.extend(event.snapshot_json_fields());
}

fn block_snapshot_json(
    history: &BlockHistory,
    block_index: usize,
    view_state: Option<ViewState>,
) -> Option<serde_json::Value> {
    let id = *history.order.get(block_index)?;
    let block = history.block(id)?;
    let mut value = serde_json::Map::new();
    value.insert("id".into(), serde_json::to_value(id).ok()?);
    value.insert("index".into(), serde_json::json!(block_index));
    value.insert("kind".into(), serde_json::json!(block_kind(block)));
    if let Some(view_state) = view_state {
        value.insert(
            "view_state".into(),
            serde_json::json!(view_state_label(view_state)),
        );
    }
    match block {
        Block::User { text, image_labels } => {
            value.insert("text".into(), serde_json::json!(text));
            value.insert("image_labels".into(), serde_json::json!(image_labels));
        }
        Block::Mode {
            text,
            icon,
            hl_group,
        } => {
            value.insert("text".into(), serde_json::json!(text));
            value.insert("icon".into(), serde_json::json!(icon));
            value.insert("hl_group".into(), serde_json::json!(hl_group));
        }
        Block::ProcessStatus { text, event } => {
            value.insert("text".into(), serde_json::json!(text));
            insert_process_status_event_json_fields(&mut value, event.as_ref());
        }
        Block::Thinking { content } | Block::Text { content } => {
            value.insert("content".into(), serde_json::json!(content));
        }
        Block::CodeLine { content, lang } => {
            value.insert("content".into(), serde_json::json!(content));
            value.insert("lang".into(), serde_json::json!(lang));
        }
        Block::ToolDraft {
            stream_id,
            call_id,
            name,
            summary,
            args,
            raw_arguments,
            finished,
        } => {
            value.insert("stream_id".into(), serde_json::json!(stream_id));
            value.insert("call_id".into(), serde_json::json!(call_id));
            value.insert("name".into(), serde_json::json!(name));
            value.insert("args".into(), serde_json::json!(args));
            value.insert("raw_arguments".into(), serde_json::json!(raw_arguments));
            value.insert("summary".into(), serde_json::to_value(summary).ok()?);
            value.insert(
                "summary_text".into(),
                serde_json::json!(summary.as_plain_text()),
            );
            value.insert("status".into(), serde_json::json!("drafting"));
            value.insert("status_hl".into(), serde_json::json!("SmeltToolPending"));
            value.insert("draft".into(), serde_json::json!(true));
            value.insert("draft_finished".into(), serde_json::json!(finished));
        }
        Block::ToolCall {
            call_id,
            name,
            summary,
            args,
        } => {
            value.insert("call_id".into(), serde_json::json!(call_id));
            value.insert("name".into(), serde_json::json!(name));
            value.insert("args".into(), serde_json::json!(args));
            value.insert("summary".into(), serde_json::to_value(summary).ok()?);
            value.insert(
                "summary_text".into(),
                serde_json::json!(summary.as_plain_text()),
            );
            let status = state_status(history, call_id);
            value.insert("status".into(), serde_json::json!(status.label()));
            value.insert("status_hl".into(), serde_json::json!(status.hl_group()));
            if let Some(state) = history.tool_state(call_id) {
                value.insert("output".into(), serde_json::to_value(&state.output).ok()?);
                value.insert("user_message".into(), serde_json::json!(state.user_message));
            }
        }
        Block::Exec { command, output } => {
            value.insert("command".into(), serde_json::json!(command));
            value.insert("output".into(), serde_json::json!(output));
        }
        Block::Compacted { summary } | Block::CompactionPreview { summary } => {
            value.insert("summary".into(), serde_json::json!(summary));
        }
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

fn state_status(history: &BlockHistory, call_id: &str) -> smelt_core::ToolStatus {
    history
        .tool_state(call_id)
        .map(|state| state.status)
        .unwrap_or(smelt_core::ToolStatus::Pending)
}

fn compile_block_with_lua(
    env: TranscriptRenderEnv<'_>,
    id: BlockId,
    index: usize,
    block: &Block,
    state: Option<&ToolState>,
    view_state: ViewState,
    cache: &mut CompileLayoutCache<'_>,
) -> LayoutIr {
    let kind = block_kind(block);
    let layout = env
        .lua
        .render_transcript_layout(id, index, block, state, view_state);
    match compile_layout_ir_with_cache(&layout, cache) {
        Ok(layout) => layout,
        Err(e) => {
            env.lua.record_error(format!(
                "transcript render `{kind}` #{index}: compile layout IR: {e}"
            ));
            BlockLayout::Leaf(IrLeaf::Text(TextSpec {
                content: format!("{kind} render error"),
                hl_group: Some("ErrorMsg".into()),
                ansi: false,
            }))
        }
    }
}

fn block_kind(block: &Block) -> &'static str {
    block.kind()
}

pub(crate) fn compile_layout_ir(layout: &BlockLayout) -> Result<LayoutIr, String> {
    let mut source_views = SourceViewCache::default();
    let mut cache = CompileLayoutCache {
        source_views: &mut source_views,
        source_views_enabled: true,
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
        BlockLayout::Leaf(LuaLeaf::Elapsed(spec)) => {
            Ok(BlockLayout::Leaf(IrLeaf::Elapsed(spec.clone())))
        }
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
                    smelt_core::content::highlight::build_diff_ir_ext(
                        &spec.old,
                        &spec.new,
                        &spec.path,
                        &spec.anchor,
                        ext,
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
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(BlockLayout::Hbox),
        BlockLayout::Gutter { child, spec } => Ok(BlockLayout::Gutter {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::RowPrefix { child, spec } => Ok(BlockLayout::RowPrefix {
            child: Box::new(compile_layout_ir(child)?),
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
    }
}

fn cached_source_view(
    source_views: &mut SourceViewCache,
    enabled: bool,
    key: u64,
    build: impl FnOnce() -> SourceViewIr,
) -> Result<LayoutIr, String> {
    if enabled {
        if let Some(ir) = source_views.get(&key) {
            return Ok(BlockLayout::Leaf(IrLeaf::SourceView(ir.clone())));
        }
    }

    let ir = build();
    if enabled {
        if source_views.len() >= 128 {
            source_views.clear();
        }
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
            text: "Background process 42 exited with code 7.".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "42",
                Some(7),
            )),
        });

        let snapshot = block_snapshot_json(&transcript.history, 0, None).expect("snapshot");

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
            },
            Block::ProcessStatus {
                text: "running a long process status that wraps on narrow terminals".into(),
                event: None,
            },
            Block::Thinking {
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
    fn display_model_caches_width_independent_blocks() {
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
        let mut model = DisplayModel::new();
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
}
