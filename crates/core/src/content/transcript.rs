//! Transcript domain state: block store.
//!
//! `Transcript` owns the block history. Streaming input parsing lives in
//! `StreamParser` (owned by `TuiApp`). Display projection lives in
//! `tui::content::transcript_snapshot` — projection is a tui concern.

use crate::transcript_model::{Block, BlockHistory, BlockId, ToolState, ViewState};

pub struct Transcript {
    pub history: BlockHistory,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            history: BlockHistory::new(),
        }
    }

    // ── Accessors ─────────────────────────────────────────────────────

    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.history.blocks.get(&id)
    }

    pub fn block_view_state(&self, id: BlockId) -> ViewState {
        self.history.view_state(id)
    }

    pub fn set_block_view_state(&mut self, id: BlockId, state: ViewState) {
        self.history.set_view_state(id, state);
    }

    pub fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.history.drain_finished_blocks()
    }

    // ── Mutations ─────────────────────────────────────────────────────

    pub fn push(&mut self, block: Block) {
        let block = match block {
            Block::Text { content } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::Text {
                    content: t.to_string(),
                }
            }
            Block::Thinking { content } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::Thinking {
                    content: t.to_string(),
                }
            }
            Block::Compacted { summary } => {
                let t = summary.trim();
                if t.is_empty() {
                    return;
                }
                Block::Compacted {
                    summary: t.to_string(),
                }
            }
            other => other,
        };
        self.history.push(block);
    }

    pub fn push_tool_call(&mut self, block: Block, state: ToolState) {
        debug_assert!(matches!(block, Block::ToolCall { .. }));
        let call_id = match &block {
            Block::ToolCall { call_id, .. } => call_id.clone(),
            _ => return,
        };
        self.history.push_with_state(block, call_id, state);
    }

    pub fn truncate_to(&mut self, block_idx: usize) {
        self.history.truncate(block_idx);
    }

    pub fn user_turns(&self) -> Vec<(usize, String)> {
        self.history
            .order
            .iter()
            .enumerate()
            .filter_map(|(i, id)| match self.history.blocks.get(id) {
                Some(Block::User { text, .. }) => Some((i, text.clone())),
                _ => None,
            })
            .collect()
    }
}
