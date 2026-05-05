//! Per-block layout cache for the transcript.
//!
//! Each block gets its own cached `DisplayBlock`, keyed by `LayoutKey`
//! (width, show_thinking, view_state, content_hash). The cache lives in
//! `tui` because display projection is a tui concern.
//!
//! The cache stores `DisplayBlock` (semantic colors) — not `ProjectedLine`
//! (resolved colors) — so theme changes don't invalidate the cache.

use smelt_core::content::DisplayBlock;
use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey, ViewState};
use smelt_core::transcript_present::ToolBodyRenderer;
use std::collections::HashMap;

/// Cached layout for a single block.
struct CachedBlock {
    key: LayoutKey,
    display: DisplayBlock,
}

/// Per-block layout cache. Owned by `TranscriptProjection` (or whatever
/// drives the transcript paint pass).
pub struct BlockBufferCache {
    blocks: HashMap<BlockId, CachedBlock>,
}

impl BlockBufferCache {
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
        }
    }

    /// Ensure the block at `id` is laid out at the given layout key.
    /// On a cache miss, calls `layout_block` via `core` and stores the
    /// resulting `DisplayBlock`.
    pub fn ensure(
        &mut self,
        history: &mut BlockHistory,
        id: BlockId,
        key: LayoutKey,
        renderer: Option<&dyn ToolBodyRenderer>,
    ) -> &DisplayBlock {
        let hit = self.blocks.get(&id).is_some_and(|c| c.key == key);
        if hit {
            return &self.blocks[&id].display;
        }

        let block = &history.blocks[&id];
        let tool_state =
            if let smelt_core::transcript_model::Block::ToolCall { call_id, .. } = block {
                history.tool_states.get(call_id)
            } else {
                None
            };

        let lctx =
            smelt_core::content::LayoutContext::new(key.width, key.show_thinking, key.view_state);

        let display =
            smelt_core::transcript_present::layout_block(block, tool_state, &lctx, renderer);

        self.blocks.insert(id, CachedBlock { key, display });

        &self.blocks[&id].display
    }

    #[allow(dead_code)]
    /// Drop the cached layout for a single block.
    pub fn invalidate(&mut self, id: BlockId) {
        self.blocks.remove(&id);
    }

    /// Drop all cached layouts.
    pub fn clear(&mut self) {
        self.blocks.clear();
    }

    #[allow(dead_code)]
    /// Total rows for the full transcript at the given width/show_thinking.
    pub fn total_rows(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
    ) -> u16 {
        let base_key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
        };

        let mut total: u32 = 0;
        for i in 0..history.order.len() {
            total += history.block_gap(i) as u32;
            let id = history.order[i];
            let key = history.resolve_key(id, base_key);
            let display = self.ensure(history, id, key, None);
            total += display.lines.len() as u32;
        }
        total.min(u16::MAX as u32) as u16
    }
}
