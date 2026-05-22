//! `Transcript` owns the block history. Streaming parsing lives in `StreamParser`; display projection in `tui`.

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript_model::ToolStatus;

    fn tool_state() -> ToolState {
        ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            output: None,
            user_message: None,
            render_cache: None,
            layout_revision: 0,
        }
    }

    #[test]
    fn new_creates_empty_transcript() {
        let t = Transcript::new();
        assert!(t.history.is_empty());
    }

    #[test]
    fn default_matches_new() {
        let t = Transcript::default();
        assert!(t.history.is_empty());
    }

    #[test]
    fn push_text_block_records_it() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "hello".into(),
        });
        assert_eq!(t.history.len(), 1);
    }

    #[test]
    fn push_trims_text_content() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "  hello  ".into(),
        });
        match t.history.block_at(0) {
            Block::Text { content } => assert_eq!(content, "hello"),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn push_drops_blank_text_block() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "   ".into(),
        });
        assert!(t.history.is_empty());
    }

    #[test]
    fn push_drops_blank_thinking_block() {
        let mut t = Transcript::new();
        t.push(Block::Thinking {
            content: "\n\n".into(),
        });
        assert!(t.history.is_empty());
    }

    #[test]
    fn push_drops_blank_compacted_block() {
        let mut t = Transcript::new();
        t.push(Block::Compacted { summary: "".into() });
        assert!(t.history.is_empty());
    }

    #[test]
    fn push_user_block_keeps_raw_text_untrimmed() {
        // The trim path covers only Text/Thinking/Compacted variants.
        let mut t = Transcript::new();
        t.push(Block::User {
            text: "  question  ".into(),
            image_labels: vec![],
        });
        match t.history.block_at(0) {
            Block::User { text, .. } => assert_eq!(text, "  question  "),
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn push_tool_call_registers_state_and_block() {
        let mut t = Transcript::new();
        t.push_tool_call(
            Block::ToolCall {
                call_id: "id-1".into(),
                name: "exec".into(),
                summary: "ls".into(),
                args: Default::default(),
            },
            tool_state(),
        );
        assert_eq!(t.history.len(), 1);
        assert!(t.history.tool_states.contains_key("id-1"));
    }

    #[test]
    fn block_returns_some_for_known_id_and_none_otherwise() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "x".into(),
        });
        let id = t.history.order[0];
        assert!(t.block(id).is_some());
        let missing = crate::transcript_model::BlockId::new(9999);
        assert!(t.block(missing).is_none());
    }

    #[test]
    fn view_state_defaults_to_expanded_and_can_be_set() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "x".into(),
        });
        let id = t.history.order[0];
        assert_eq!(t.block_view_state(id), ViewState::Expanded);
        t.set_block_view_state(id, ViewState::Collapsed);
        assert_eq!(t.block_view_state(id), ViewState::Collapsed);
    }

    #[test]
    fn truncate_to_drops_trailing_blocks() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "a".into(),
        });
        t.push(Block::Text {
            content: "b".into(),
        });
        t.push(Block::Text {
            content: "c".into(),
        });
        t.truncate_to(1);
        assert_eq!(t.history.len(), 1);
    }

    #[test]
    fn user_turns_returns_only_user_blocks_with_index() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "ignored".into(),
        });
        t.push(Block::User {
            text: "first".into(),
            image_labels: vec![],
        });
        t.push(Block::Text {
            content: "between".into(),
        });
        t.push(Block::User {
            text: "second".into(),
            image_labels: vec![],
        });
        let turns = t.user_turns();
        assert_eq!(
            turns,
            vec![(1usize, "first".into()), (3usize, "second".into())]
        );
    }

    #[test]
    fn drain_finished_blocks_returns_empty_when_none_streaming() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "x".into(),
        });
        let drained = t.drain_finished_blocks();
        assert!(drained.is_empty());
    }
}
