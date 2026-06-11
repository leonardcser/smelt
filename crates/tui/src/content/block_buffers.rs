//! Per-block layout cache keyed by `LayoutKey`. Theme changes invalidate the cache at the
//! `TranscriptProjection` boundary.

use crate::content::transcript_parsers::layout_block_into;
use crate::smelt_edit::{BufCreateOpts, BufId, Buffer};
use smelt_core::theme::Theme;
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, LayoutKey};
use std::collections::HashMap;

struct CachedBlock {
    key: LayoutKey,
    buf: Buffer,
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

    /// Ensure every `(id, key)` is cached, rendering misses in parallel.
    /// Capped at 8 workers to avoid oversubscribing the syntect/markdown layout.
    /// Returns the number of blocks rendered on cache misses.
    pub fn ensure_many(
        &mut self,
        history: &BlockHistory,
        ids: &[BlockId],
        keys: &[LayoutKey],
        theme: &Theme,
    ) -> usize {
        debug_assert_eq!(ids.len(), keys.len());

        struct Task<'a> {
            id: BlockId,
            key: LayoutKey,
            buf_id: BufId,
            block: &'a Block,
            tool_state: Option<&'a smelt_core::transcript_model::ToolState>,
        }

        let mut tasks: Vec<Task<'_>> = Vec::new();
        for (id, key) in ids.iter().zip(keys.iter()) {
            if self.blocks.get(id).is_some_and(|c| c.key == *key) {
                continue;
            }
            let block = &history.blocks[id];
            let tool_state = match block {
                Block::ToolCall { call_id, .. } => history.tool_states.get(call_id),
                _ => None,
            };
            let buf_id = BufId(self.next_buf_id);
            self.next_buf_id += 1;
            tasks.push(Task {
                id: *id,
                key: *key,
                buf_id,
                block,
                tool_state,
            });
        }
        if tasks.is_empty() {
            return 0;
        }
        let rendered = tasks.len();
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 8);
        let chunk_size = tasks.len().div_ceil(workers).max(1);

        let results: Vec<(BlockId, LayoutKey, Buffer)> = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for chunk in tasks.chunks(chunk_size) {
                handles.push(scope.spawn(move || {
                    let mut out: Vec<(BlockId, LayoutKey, Buffer)> =
                        Vec::with_capacity(chunk.len());
                    for t in chunk {
                        let lctx = smelt_core::content::LayoutContext::new(
                            t.key.width,
                            t.key.show_thinking,
                            t.key.view_state,
                        );
                        let mut buf = Buffer::new(t.buf_id, BufCreateOpts::default());
                        let _outcome =
                            layout_block_into(&mut buf, theme, t.block, t.tool_state, &lctx);
                        out.push((t.id, t.key, buf));
                    }
                    out
                }));
            }
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("layout worker panicked"))
                .collect()
        });

        for (id, key, buf) in results {
            self.blocks.insert(id, CachedBlock { key, buf });
        }
        rendered
    }

    /// Cached buffer for `id`, or `None` if not yet laid out (or stale).
    pub fn get(&self, id: BlockId, key: LayoutKey) -> Option<&Buffer> {
        self.blocks
            .get(&id)
            .filter(|c| c.key == key)
            .map(|c| &c.buf)
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
    }
}
