use crate::smelt_edit::{BufCreateOpts, BufId, Buffer, Theme};
use smelt_core::content::code_block::{measure_code_block, parse_code_block, CodeBlock};
use smelt_core::content::LayoutContext;
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, LayoutKey, ToolState, ViewState};
use std::collections::{HashMap, HashSet};

pub(crate) const DISPLAY_RENDERER_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayCacheKey {
    pub(crate) content_hash: u64,
    pub(crate) sidecar_hash: u64,
    pub(crate) renderer_version: u64,
}

impl DisplayCacheKey {
    pub(crate) fn new(content_hash: u64, sidecar_hash: u64) -> Self {
        Self {
            content_hash,
            sidecar_hash,
            renderer_version: DISPLAY_RENDERER_VERSION,
        }
    }

    fn from_layout_key(key: LayoutKey) -> Self {
        Self::new(key.content_hash, key.sidecar_hash)
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum DisplayBlock {
    Legacy { block: Block },
    CodeLine { block: Block, code: CodeBlock },
    ToolCall { block: Block, state: ToolState },
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayCacheEntry {
    pub(crate) id: BlockId,
    pub(crate) key: DisplayCacheKey,
    pub(crate) block: DisplayBlock,
}

impl DisplayBlock {
    fn block(&self) -> &Block {
        match self {
            Self::Legacy { block }
            | Self::CodeLine { block, .. }
            | Self::ToolCall { block, .. } => block,
        }
    }

    fn tool_state(&self) -> Option<&ToolState> {
        match self {
            Self::ToolCall { state, .. } => Some(state),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MeasureCtx {
    pub width: u16,
    pub show_thinking: bool,
    pub view_state: ViewState,
}

#[derive(Clone, Copy)]
pub(crate) struct RenderCtx<'a> {
    pub width: u16,
    pub show_thinking: bool,
    pub view_state: ViewState,
    pub theme: &'a Theme,
}

struct CachedDisplayBlock {
    key: DisplayCacheKey,
    block: DisplayBlock,
}

#[derive(Default)]
pub(crate) struct DisplayModel {
    blocks: HashMap<BlockId, CachedDisplayBlock>,
}

impl DisplayModel {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.blocks.len()
    }

    pub(crate) fn hydrate_many(
        &mut self,
        history: &mut BlockHistory,
        entries: Vec<DisplayCacheEntry>,
    ) -> usize {
        let mut hydrated = 0;
        for entry in entries {
            if self.hydrate_one(history, entry) {
                hydrated += 1;
            }
        }
        hydrated
    }

    fn hydrate_one(&mut self, history: &mut BlockHistory, entry: DisplayCacheEntry) -> bool {
        let Some(current_block) = history.blocks.get(&entry.id).cloned() else {
            return false;
        };
        if entry.block.block() != &current_block {
            return false;
        }

        if let DisplayBlock::ToolCall { state, .. } = &entry.block {
            let Block::ToolCall { call_id, .. } = &current_block else {
                return false;
            };
            let Some(current_state) = history.tool_states.get_mut(call_id) else {
                return false;
            };
            let mut candidate = current_state.clone();
            candidate.body = state.body.clone();
            if candidate.display_hash() != entry.key.sidecar_hash {
                return false;
            }
            current_state.body = candidate.body;
        }

        let key = history.resolve_key(
            entry.id,
            LayoutKey {
                width: 0,
                show_thinking: false,
                view_state: ViewState::Expanded,
                content_hash: 0,
                sidecar_hash: 0,
            },
        );
        let display_key = DisplayCacheKey::from_layout_key(key);
        if display_key != entry.key {
            return false;
        }

        self.blocks.insert(
            entry.id,
            CachedDisplayBlock {
                key: display_key,
                block: entry.block,
            },
        );
        true
    }

    pub(crate) fn cache_entries(&self, order: &[BlockId]) -> Vec<DisplayCacheEntry> {
        order
            .iter()
            .filter_map(|id| {
                self.blocks.get(id).map(|cached| DisplayCacheEntry {
                    id: *id,
                    key: cached.key,
                    block: cached.block.clone(),
                })
            })
            .collect()
    }

    pub(crate) fn ensure_many(
        &mut self,
        history: &BlockHistory,
        ids: &[BlockId],
        keys: &[LayoutKey],
    ) -> usize {
        let _perf = smelt_perf::perf::begin("transcript:display_model:ensure_many");
        smelt_perf::perf::record_value("transcript:display_model:requested", ids.len() as u64);
        debug_assert_eq!(ids.len(), keys.len());

        let mut compiled = 0;
        for (&id, &key) in ids.iter().zip(keys.iter()) {
            let display_key = DisplayCacheKey::from_layout_key(key);
            if self
                .blocks
                .get(&id)
                .is_some_and(|cached| cached.key == display_key)
            {
                continue;
            }
            let Some(block) = history.blocks.get(&id) else {
                self.blocks.remove(&id);
                continue;
            };
            let state = match block {
                Block::ToolCall { call_id, .. } => history.tool_states.get(call_id),
                _ => None,
            };
            self.blocks.insert(
                id,
                CachedDisplayBlock {
                    key: display_key,
                    block: compile_block(block, state),
                },
            );
            compiled += 1;
        }
        smelt_perf::perf::record_value("transcript:display_model:compiled", compiled as u64);
        compiled
    }

    pub(crate) fn retain_order(&mut self, order: &[BlockId]) {
        let live: HashSet<BlockId> = order.iter().copied().collect();
        self.blocks.retain(|id, _| live.contains(id));
    }

    pub(crate) fn get(&self, id: BlockId, key: LayoutKey) -> Option<&DisplayBlock> {
        let display_key = DisplayCacheKey::from_layout_key(key);
        self.blocks
            .get(&id)
            .filter(|cached| cached.key == display_key)
            .map(|cached| &cached.block)
    }
}

pub(crate) fn compile_block(block: &Block, state: Option<&ToolState>) -> DisplayBlock {
    match block {
        Block::CodeLine { content, lang } => DisplayBlock::CodeLine {
            block: block.clone(),
            code: parse_code_block(&[content.as_str()], lang),
        },
        Block::ToolCall { call_id, .. } => DisplayBlock::ToolCall {
            block: block.clone(),
            state: state
                .cloned()
                .unwrap_or_else(|| panic!("missing ToolState for tool call `{call_id}`")),
        },
        _ => DisplayBlock::Legacy {
            block: block.clone(),
        },
    }
}

pub(crate) fn measure_block(block: &DisplayBlock, ctx: MeasureCtx) -> u64 {
    let expanded_rows = match block {
        DisplayBlock::CodeLine { code, .. } => measure_code_block(code, ctx.width as usize) as u64,
        DisplayBlock::ToolCall { block, state } => match block {
            Block::ToolCall { name, summary, .. } => {
                crate::content::transcript_parsers::measure_tool_height(
                    name,
                    summary,
                    state.status,
                    state.elapsed,
                    state.output.as_deref(),
                    state.user_message.as_deref(),
                    state.body.as_ref(),
                    ctx.width as usize,
                ) as u64
            }
            _ => unreachable!("tool display block must wrap a tool call block"),
        },
        DisplayBlock::Legacy { .. } => measure_legacy_by_rendering(block, ctx),
    };
    ctx.view_state.measured_height(expanded_rows)
}

fn measure_legacy_by_rendering(block: &DisplayBlock, ctx: MeasureCtx) -> u64 {
    let theme = Theme::default();
    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let render_ctx = RenderCtx {
        width: ctx.width,
        show_thinking: ctx.show_thinking,
        view_state: ViewState::Expanded,
        theme: &theme,
    };
    render_block_into(&mut buf, block, render_ctx).line_count as u64
}

pub(crate) fn render_block_into(
    buf: &mut Buffer,
    block: &DisplayBlock,
    ctx: RenderCtx<'_>,
) -> smelt_core::content::builder::Outcome {
    let lctx = LayoutContext::new(ctx.width, ctx.show_thinking, ctx.view_state);
    crate::content::transcript_parsers::layout_block_into(
        buf,
        ctx.theme,
        block.block(),
        block.tool_state(),
        &lctx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::content::block_layout::{BlockLayout, IrLeaf, TextSpec, ToolBody};
    use smelt_core::content::transcript::Transcript;
    use smelt_core::transcript_model::{ToolOutput, ToolStatus};

    fn base_key(history: &BlockHistory, id: BlockId) -> LayoutKey {
        history.resolve_key(
            id,
            LayoutKey {
                width: 80,
                show_thinking: false,
                view_state: ViewState::Expanded,
                content_hash: 0,
                sidecar_hash: 0,
            },
        )
    }

    #[test]
    fn hydrated_cache_entries_avoid_recompile() {
        let mut transcript = Transcript::new();
        transcript.push(Block::CodeLine {
            content: "fn main() {}".into(),
            lang: "rust".into(),
        });
        let id = transcript.history.order[0];
        let key = base_key(&transcript.history, id);

        let mut model = DisplayModel::new();
        assert_eq!(model.ensure_many(&transcript.history, &[id], &[key]), 1);
        let entries = model.cache_entries(&transcript.history.order);

        let mut hydrated = DisplayModel::new();
        assert_eq!(hydrated.hydrate_many(&mut transcript.history, entries), 1);
        assert_eq!(hydrated.ensure_many(&transcript.history, &[id], &[key]), 0);
    }

    #[test]
    fn hydrated_tool_entry_installs_cached_body() {
        let block = Block::ToolCall {
            call_id: "call-1".into(),
            name: "write_file".into(),
            summary: "write file".into(),
            args: Default::default(),
        };
        let body = ToolBody::Layout(BlockLayout::Leaf(IrLeaf::Text(TextSpec {
            content: "cached body".into(),
            hl_group: None,
        })));

        let mut warm = Transcript::new();
        warm.push_tool_call(
            block.clone(),
            ToolState {
                status: ToolStatus::Ok,
                elapsed: None,
                output: Some(Box::new(ToolOutput {
                    content: "ok".into(),
                    is_error: false,
                    metadata: None,
                })),
                user_message: None,
                body: Some(body),
                layout_revision: 1,
            },
        );
        let id = warm.history.order[0];
        let key = base_key(&warm.history, id);
        let mut model = DisplayModel::new();
        assert_eq!(model.ensure_many(&warm.history, &[id], &[key]), 1);
        let entries = model.cache_entries(&warm.history.order);

        let mut cold = Transcript::new();
        cold.push_tool_call(
            block,
            ToolState {
                status: ToolStatus::Ok,
                elapsed: None,
                output: Some(Box::new(ToolOutput {
                    content: "ok".into(),
                    is_error: false,
                    metadata: None,
                })),
                user_message: None,
                body: None,
                layout_revision: 0,
            },
        );
        let mut hydrated = DisplayModel::new();
        assert_eq!(hydrated.hydrate_many(&mut cold.history, entries), 1);
        assert!(cold.history.tool_states["call-1"].body.is_some());
        let cold_key = base_key(&cold.history, id);
        assert_eq!(hydrated.ensure_many(&cold.history, &[id], &[cold_key]), 0);
    }
}
