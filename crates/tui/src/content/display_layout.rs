use crate::content::render_plan::{NodeLayoutKey, RenderNodeId, RenderPlan};
use crate::smelt_edit::{Buffer, Theme};
use smelt_core::content::block_layout::{
    BlockLayout, HboxItem, IrLeaf, LayoutIr, LuaLeaf, SourceViewIr, TextSpec,
};
use smelt_core::content::builder::{LineBuilder, Outcome};
use smelt_core::lua::runtime::LuaRuntime;
use smelt_core::theme::intern;
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, ToolState, ViewState};
use std::collections::{HashMap, HashSet};

pub(crate) const DISPLAY_RENDERER_VERSION: u64 = 7;

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
            u64::from(key.show_thinking),
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TranscriptRenderEnv<'a> {
    pub(crate) lua: &'a LuaRuntime,
    pub(crate) show_thinking: bool,
    pub(crate) renderer_generation: u64,
    pub(crate) renderer_cache_key: Option<u64>,
}

impl<'a> TranscriptRenderEnv<'a> {
    pub(crate) fn new(lua: &'a LuaRuntime, show_thinking: bool) -> Self {
        Self {
            lua,
            show_thinking,
            renderer_generation: lua.transcript_renderer_generation(),
            renderer_cache_key: lua.transcript_renderer_cache_key(),
        }
    }

    pub(crate) fn with_renderer(
        lua: &'a LuaRuntime,
        show_thinking: bool,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> Self {
        Self {
            lua,
            show_thinking,
            renderer_generation,
            renderer_cache_key,
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayRowIndexEntry {
    pub(crate) width: u16,
    pub(crate) show_thinking: bool,
    pub(crate) renderer_generation: u64,
    pub(crate) renderer_cache_key: Option<u64>,
    pub(crate) nodes: Vec<DisplayRowIndexNode>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayRowIndexNode {
    pub(crate) id: RenderNodeId,
    pub(crate) key: NodeLayoutKey,
    pub(crate) exact_height: u64,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayLayoutCacheEntry {
    pub(crate) id: RenderNodeId,
    pub(crate) key: DisplayCacheKey,
    pub(crate) layout: LayoutIr,
}

pub(crate) struct CompileJob {
    id: RenderNodeId,
    block_id: BlockId,
    index: usize,
    key: DisplayCacheKey,
    block: Block,
    state: Option<ToolState>,
}

impl CompileJob {
    pub(crate) fn compile(
        self,
        env: TranscriptRenderEnv<'_>,
    ) -> (RenderNodeId, DisplayCacheKey, LayoutIr) {
        let Self {
            id,
            block_id,
            index,
            key,
            block,
            state,
        } = self;
        (
            id,
            key,
            compile_block_with_lua(env, block_id, index, &block, state.as_ref()),
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MeasureCtx {
    pub width: u16,
    pub view_state: ViewState,
}

#[derive(Clone, Copy)]
pub(crate) struct RenderCtx<'a> {
    pub width: u16,
    pub view_state: ViewState,
    pub theme: &'a Theme,
    pub history: Option<&'a BlockHistory>,
}

struct CachedLayout {
    key: DisplayCacheKey,
    layout: LayoutIr,
}

#[derive(Default)]
pub(crate) struct DisplayModel {
    blocks: HashMap<RenderNodeId, CachedLayout>,
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
                    .map(|index| (index, RenderNodeId::Block(id), key))
            });
        let jobs =
            self.collect_compile_jobs(history, renderer_generation, renderer_cache_key, blocks);
        let compiled = jobs.len();
        let blocks = jobs.into_iter().map(|job| job.compile(env)).collect();
        self.insert_compiled_blocks(blocks);
        compiled
    }

    /// Returns compile jobs for cache misses. The caller can run these jobs on
    /// the current thread or schedule them onto a worker pool, then insert the
    /// results with `insert_compiled_blocks`.
    pub(crate) fn collect_compile_jobs(
        &mut self,
        history: &BlockHistory,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        blocks: impl IntoIterator<Item = (usize, RenderNodeId, NodeLayoutKey)>,
    ) -> Vec<CompileJob> {
        let _perf = smelt_perf::perf::begin("transcript:display_model:ensure_many");

        let mut jobs = Vec::new();
        let mut requested = 0;
        for (index, id, key) in blocks {
            requested += 1;
            let display_key =
                DisplayCacheKey::from_node_key(key, renderer_generation, renderer_cache_key);
            if self
                .blocks
                .get(&id)
                .is_some_and(|cached| cached.key == display_key)
            {
                continue;
            }
            let Some(block_id) = id.as_block_id() else {
                self.blocks.remove(&id);
                continue;
            };
            let Some(block) = history.blocks.get(&block_id).cloned() else {
                self.blocks.remove(&id);
                continue;
            };
            let state = match &block {
                Block::ToolCall { call_id, .. } => history.tool_state(call_id).cloned(),
                _ => None,
            };
            jobs.push(CompileJob {
                id,
                block_id,
                index,
                key: display_key,
                block,
                state,
            });
        }
        smelt_perf::perf::record_value("transcript:display_model:requested", requested);
        smelt_perf::perf::record_value("transcript:display_model:compiled", jobs.len() as u64);
        jobs
    }

    pub(crate) fn hydrate_from_cache(
        &mut self,
        history: &BlockHistory,
        plan: &RenderPlan,
        entries: Vec<DisplayLayoutCacheEntry>,
    ) -> usize {
        let mut hydrated = 0usize;
        for entry in entries {
            if !display_layout_entry_matches_history(history, plan, &entry) {
                continue;
            }
            self.blocks.insert(
                entry.id,
                CachedLayout {
                    key: entry.key,
                    layout: entry.layout,
                },
            );
            hydrated += 1;
        }
        smelt_perf::perf::record_value("transcript:display_model:hydrated", hydrated as u64);
        hydrated
    }

    pub(crate) fn cache_entries(
        &self,
        history: &BlockHistory,
        plan: &RenderPlan,
        renderer_generation: Option<u64>,
        renderer_cache_key: Option<u64>,
    ) -> Vec<DisplayLayoutCacheEntry> {
        if renderer_generation.is_some() && renderer_cache_key.is_none() {
            return Vec::new();
        }
        let mut entries = Vec::new();
        for id in plan.ids() {
            let Some(cached) = self.blocks.get(&id) else {
                continue;
            };
            if cached.key.renderer_cache_key.is_none() {
                continue;
            }
            if renderer_generation
                .is_some_and(|generation| cached.key.renderer_generation != generation)
            {
                continue;
            }
            if renderer_cache_key
                .is_some_and(|cache_key| cached.key.renderer_cache_key != Some(cache_key))
            {
                continue;
            }
            let entry = DisplayLayoutCacheEntry {
                id,
                key: cached.key,
                layout: cached.layout.clone(),
            };
            if display_layout_entry_matches_history(history, plan, &entry) {
                entries.push(entry);
            }
        }
        entries
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

fn display_layout_entry_matches_history(
    history: &BlockHistory,
    plan: &RenderPlan,
    entry: &DisplayLayoutCacheEntry,
) -> bool {
    if entry.key.renderer_version != DISPLAY_RENDERER_VERSION {
        return false;
    }
    if entry.key.renderer_cache_key.is_none() {
        return false;
    }
    if !plan.ids().any(|id| id == entry.id) {
        return false;
    }
    let Some(block_id) = entry.id.as_block_id() else {
        return false;
    };
    let Some(block) = history.blocks.get(&block_id) else {
        return false;
    };
    if history.content_hash(block_id) != entry.key.content_hash {
        return false;
    }
    let sidecar_hash = match block {
        Block::ToolCall { call_id, .. } => history
            .tool_state(call_id)
            .map(ToolState::display_hash)
            .unwrap_or(0),
        _ => 0,
    };
    sidecar_hash == entry.key.sidecar_hash
}

#[cfg(test)]
pub(crate) fn compile_block_with_show(block: &Block, show_thinking: bool) -> LayoutIr {
    let lua = LuaRuntime::new();
    compile_block_with_lua(
        TranscriptRenderEnv::new(&lua, show_thinking),
        BlockId::new(0),
        0,
        block,
        None,
    )
}

fn compile_block_with_lua(
    env: TranscriptRenderEnv<'_>,
    id: BlockId,
    index: usize,
    block: &Block,
    state: Option<&ToolState>,
) -> LayoutIr {
    let kind = block_kind(block);
    let layout = env.lua.render_transcript_layout(
        id,
        index,
        block,
        state,
        smelt_core::lua::runtime::TranscriptRenderCtx {
            show_thinking: env.show_thinking,
        },
    );
    match compile_layout_ir(&layout) {
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
    match block {
        Block::User { .. } => "user",
        Block::Mode { .. } => "mode",
        Block::ProcessStatus { .. } => "process_status",
        Block::Thinking { .. } => "thinking",
        Block::Text { .. } => "assistant",
        Block::CodeLine { .. } => "code",
        Block::ToolCall { .. } => "tool",
        Block::Exec { .. } => "exec",
        Block::Compacted { .. } => "compacted",
    }
}

pub(crate) fn compile_layout_ir(layout: &BlockLayout) -> Result<LayoutIr, String> {
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
        BlockLayout::Leaf(LuaLeaf::Diff(spec)) => {
            let ext = spec
                .lang
                .as_deref()
                .map(smelt_core::content::highlight::lang_to_ext);
            let ir = smelt_core::content::highlight::build_diff_ir_ext(
                &spec.old,
                &spec.new,
                &spec.path,
                &spec.anchor,
                ext,
            );
            Ok(BlockLayout::Leaf(IrLeaf::SourceView(SourceViewIr::Diff(
                ir,
            ))))
        }
        BlockLayout::Leaf(LuaLeaf::FileView(spec)) => {
            let ext = spec
                .lang
                .as_deref()
                .map(smelt_core::content::highlight::lang_to_ext)
                .or_else(|| {
                    std::path::Path::new(&spec.path)
                        .extension()
                        .and_then(|e| e.to_str())
                });
            let ir = smelt_core::content::highlight::build_file_view_ir(&spec.content, ext);
            Ok(BlockLayout::Leaf(IrLeaf::SourceView(SourceViewIr::Diff(
                ir,
            ))))
        }
        BlockLayout::Leaf(LuaLeaf::SourceView(ir)) => {
            Ok(BlockLayout::Leaf(IrLeaf::SourceView(ir.clone())))
        }
        BlockLayout::Vbox(items) => items
            .iter()
            .map(compile_layout_ir)
            .collect::<Result<Vec<_>, _>>()
            .map(BlockLayout::Vbox),
        BlockLayout::Hbox(items) => items
            .iter()
            .map(|item| {
                Ok(HboxItem {
                    constraint: item.constraint,
                    layout: compile_layout_ir(&item.layout)?,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(BlockLayout::Hbox),
        BlockLayout::Gutter { child, spec } => Ok(BlockLayout::Gutter {
            child: Box::new(compile_layout_ir(child)?),
            spec: spec.clone(),
        }),
        BlockLayout::Panel { child, spec } => Ok(BlockLayout::Panel {
            child: Box::new(compile_layout_ir(child)?),
            spec: spec.clone(),
        }),
        BlockLayout::Style { child, spec } => Ok(BlockLayout::Style {
            child: Box::new(compile_layout_ir(child)?),
            spec: spec.clone(),
        }),
        BlockLayout::Cap { child, spec } => Ok(BlockLayout::Cap {
            child: Box::new(compile_layout_ir(child)?),
            spec: spec.clone(),
        }),
    }
}

pub(crate) fn measure_block(layout: &LayoutIr, ctx: MeasureCtx) -> u64 {
    let _perf = smelt_perf::perf::begin("transcript:measure_block:layout");
    let expanded_rows =
        crate::content::display_renderers::measure_layout_ir(layout, ctx.width) as u64;
    ctx.view_state.measured_height(expanded_rows)
}

pub(crate) fn render_block_into(
    buf: &mut Buffer,
    layout: &LayoutIr,
    ctx: RenderCtx<'_>,
) -> Outcome {
    let outcome = {
        let mut out = LineBuilder::new(buf, ctx.theme, ctx.width);
        render_expanded_block(&mut out, layout, ctx.width as usize, ctx.history);
        out.finish()
    };
    apply_view_state(buf, ctx.theme, ctx.width, ctx.view_state, outcome)
}

fn render_expanded_block(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: usize,
    history: Option<&BlockHistory>,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:layout");
    if let Some(history) = history {
        crate::content::display_renderers::render_layout_ir_into_with_history(
            out,
            layout,
            width as u16,
            history,
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
        ViewState::Expanded => outcome,
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
        NodeLayoutKey::from_block_key(history.resolve_key(
            id,
            LayoutKey {
                width: 80,
                show_thinking: false,
                view_state: ViewState::Expanded,
                content_hash: 0,
                sidecar_hash: 0,
            },
        ))
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
            },
        )
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
            for show_thinking in [false, true] {
                let display = compile_block_with_show(&block, show_thinking);
                assert_eq!(
                    measured_rows(&display, 36),
                    rendered_rows(&display, 36),
                    "measurement mismatch for {block:?}, show_thinking={show_thinking}"
                );
            }
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
                TranscriptRenderEnv::new(&lua, key.show_thinking),
                &transcript.history,
                &[id],
                &[key],
            ),
            1
        );
        assert_eq!(
            model.ensure_many(
                TranscriptRenderEnv::new(&lua, narrow.show_thinking),
                &transcript.history,
                &[id],
                &[narrow],
            ),
            0
        );
    }
}
