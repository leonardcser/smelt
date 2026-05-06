//! Per-block layout cache for the transcript.
//!
//! Each block gets its own cached `Buffer`, keyed by `LayoutKey`
//! (width, show_thinking, view_state, content_hash). The cache lives
//! in `tui` because display projection is a tui concern; the cached
//! Buffers carry resolved styles so theme changes invalidate the
//! cache (handled at the `TranscriptProjection` boundary by clearing
//! on generation mismatch and on theme change).

use crate::content::transcript_parsers::layout_block_into;
use crate::ui::{BufCreateOpts, BufId, Buffer};
use smelt_core::content::builder::Outcome;
use smelt_core::theme::Theme;
use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey};
use std::collections::HashMap;

/// Cached per-block layout.
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

    /// Ensure the block at `id` is laid out at the given layout key.
    /// On a cache miss, allocates a fresh per-block `Buffer` and runs
    /// `layout_block_into` against it. Returns `(buf, outcome)`.
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

    /// Drop all cached layouts.
    pub fn clear(&mut self) {
        self.blocks.clear();
    }
}
