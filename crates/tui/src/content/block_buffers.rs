//! Per-block layout cache keyed by `LayoutKey`. Theme changes invalidate the cache at the
//! `TranscriptProjection` boundary.

use crate::content::transcript_parsers::layout_block_into;
use crate::smelt_term::{BufCreateOpts, BufId, Buffer};
use smelt_core::content::builder::Outcome;
use smelt_core::theme::Theme;
use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey};
use std::collections::HashMap;

struct CachedBlock {
    key: LayoutKey,
    buf: Buffer,
    outcome: Outcome,
}

/// Per-block layout cache. Owned by `TranscriptProjection`.
pub struct BlockBufferCache {
    blocks: HashMap<BlockId, CachedBlock>,
    next_buf_id: u64,
}

impl Default for BlockBufferCache {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockBufferCache {
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            next_buf_id: 1,
        }
    }

    /// Returns `(buf, outcome)` for `id` at `key`, re-running layout on miss.
    pub fn ensure(
        &mut self,
        history: &mut BlockHistory,
        id: BlockId,
        key: LayoutKey,
        theme: &Theme,
    ) -> (&Buffer, Outcome) {
        let hit = self.blocks.get(&id).is_some_and(|c| c.key == key);
        if !hit {
            let block = &history.blocks[&id];
            let tool_state =
                if let smelt_core::transcript_model::Block::ToolCall { call_id, .. } = block {
                    history.tool_states.get(call_id)
                } else {
                    None
                };
            let lctx = smelt_core::content::LayoutContext::new(
                key.width,
                key.show_thinking,
                key.view_state,
            );
            let buf_id = BufId(self.next_buf_id);
            self.next_buf_id += 1;
            let mut buf = Buffer::new(buf_id, BufCreateOpts::default());
            let outcome = layout_block_into(&mut buf, theme, block, tool_state, &lctx);
            self.blocks.insert(id, CachedBlock { key, buf, outcome });
        }
        let entry = &self.blocks[&id];
        (&entry.buf, entry.outcome)
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
    }
}
