use crate::smelt_edit::{BufCreateOpts, BufId, Buffer, Theme};
use smelt_core::content::code_block::{measure_code_block, parse_code_block, CodeBlock};
use smelt_core::content::LayoutContext;
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, LayoutKey, ToolState, ViewState};
use std::collections::{HashMap, HashSet};

pub(crate) const DISPLAY_RENDERER_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayCacheKey {
    content_hash: u64,
    sidecar_hash: u64,
    renderer_version: u64,
}

impl DisplayCacheKey {
    fn new(content_hash: u64, sidecar_hash: u64) -> Self {
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

#[derive(Clone)]
pub(crate) enum DisplayBlock {
    User { block: Block },
    Mode { block: Block },
    ProcessStatus { block: Block },
    Thinking { block: Block },
    Text { block: Block },
    CodeLine { block: Block, code: CodeBlock },
    ToolCall { block: Block, state: ToolState },
    Exec { block: Block },
    Compacted { block: Block },
}

impl DisplayBlock {
    fn block(&self) -> &Block {
        match self {
            Self::User { block }
            | Self::Mode { block }
            | Self::ProcessStatus { block }
            | Self::Thinking { block }
            | Self::Text { block }
            | Self::CodeLine { block, .. }
            | Self::ToolCall { block, .. }
            | Self::Exec { block }
            | Self::Compacted { block } => block,
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
        Block::User { .. } => DisplayBlock::User {
            block: block.clone(),
        },
        Block::Mode { .. } => DisplayBlock::Mode {
            block: block.clone(),
        },
        Block::ProcessStatus { .. } => DisplayBlock::ProcessStatus {
            block: block.clone(),
        },
        Block::Thinking { .. } => DisplayBlock::Thinking {
            block: block.clone(),
        },
        Block::Text { .. } => DisplayBlock::Text {
            block: block.clone(),
        },
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
        Block::Exec { .. } => DisplayBlock::Exec {
            block: block.clone(),
        },
        Block::Compacted { .. } => DisplayBlock::Compacted {
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
        _ => measure_by_rendering(block, ctx),
    };
    ctx.view_state.measured_height(expanded_rows)
}

fn measure_by_rendering(block: &DisplayBlock, ctx: MeasureCtx) -> u64 {
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
