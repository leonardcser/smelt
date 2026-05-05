//! Per-block layout cache for the transcript.
//!
//! Each block gets its own cached `Buffer`, keyed by `LayoutKey`
//! (width, show_thinking, view_state, content_hash). The cache lives
//! in `tui` because display projection is a tui concern; the cached
//! Buffers carry resolved styles so theme changes invalidate the
//! cache (handled at the `TranscriptProjection` boundary by clearing
//! on generation mismatch and on theme change).

use crate::ui::{BufCreateOpts, BufId, Buffer};
use smelt_core::content::layout_out::Outcome;
use smelt_core::theme::Theme;
use smelt_core::transcript_model::{BlockHistory, BlockId, LayoutKey, ViewState};
use smelt_core::transcript_present::{layout_block_into, ToolBodyRenderer};
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
        renderer: Option<&dyn ToolBodyRenderer>,
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
            let outcome = layout_block_into(&mut buf, theme, block, tool_state, &lctx, renderer);
            self.blocks.insert(id, CachedBlock { key, buf, outcome });
        }
        let entry = &self.blocks[&id];
        (&entry.buf, entry.outcome)
    }

    /// Drop all cached layouts.
    pub fn clear(&mut self) {
        self.blocks.clear();
    }

    #[allow(dead_code)]
    pub fn invalidate(&mut self, id: BlockId) {
        self.blocks.remove(&id);
    }

    /// Total rows for the full transcript at the given width/show_thinking.
    /// Used by callers that need to size the viewport before painting.
    #[allow(dead_code)]
    pub fn total_rows(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> u16 {
        let base_key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
        };
        let mut total: u32 = 0;
        let renderer_arc = history.body_renderer.clone();
        let renderer = renderer_arc.as_deref();
        for i in 0..history.order.len() {
            total += history.block_gap(i) as u32;
            let id = history.order[i];
            let key = history.resolve_key(id, base_key);
            let (_, outcome) = self.ensure(history, id, key, theme, renderer);
            total += outcome.line_count as u32;
        }
        total.min(u16::MAX as u32) as u16
    }
}
