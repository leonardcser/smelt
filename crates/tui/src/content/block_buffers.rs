//! Per-block layout cache keyed by `LayoutKey`. Theme changes invalidate the cache at the
//! `TranscriptProjection` boundary.

use crate::content::transcript_parsers::layout_block_into;
use crate::smelt_edit::{BufCreateOpts, BufId, Buffer};
use smelt_core::theme::Theme;
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, LayoutKey};
use std::collections::{HashMap, HashSet, VecDeque};

const MAX_RENDERED_BLOCKS: usize = 512;

struct CachedBlock {
    key: LayoutKey,
    buf: Buffer,
}

/// Rendered block cache keyed by block id and layout key. Owned by `TranscriptProjection`.
pub struct RenderedBlockCache {
    blocks: HashMap<BlockId, CachedBlock>,
    recency: VecDeque<BlockId>,
    next_buf_id: u64,
}

impl Default for RenderedBlockCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderedBlockCache {
    pub(crate) const MAX_BLOCKS: usize = MAX_RENDERED_BLOCKS;

    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            recency: VecDeque::new(),
            next_buf_id: 1,
        }
    }

    fn touch(&mut self, id: BlockId) {
        self.recency.retain(|cached| *cached != id);
        self.recency.push_back(id);
    }

    fn evict_unpinned(&mut self, pinned: &[BlockId]) {
        let pinned: HashSet<BlockId> = pinned.iter().copied().collect();
        let mut deferred = Vec::new();
        while self.blocks.len() > Self::MAX_BLOCKS {
            let Some(id) = self.recency.pop_front() else {
                break;
            };
            if pinned.contains(&id) {
                deferred.push(id);
                if deferred.len() >= self.blocks.len() {
                    break;
                }
                continue;
            }
            self.blocks.remove(&id);
        }
        for id in deferred {
            if self.blocks.contains_key(&id) {
                self.recency.push_back(id);
            }
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
        let _perf = smelt_perf::perf::begin("transcript:render_block_cache:ensure_many");
        smelt_perf::perf::record_value("transcript:render_block_cache:requested", ids.len() as u64);
        debug_assert_eq!(ids.len(), keys.len());
        assert!(
            ids.len() <= Self::MAX_BLOCKS,
            "rendered block cache batches must fit inside the pinned cache capacity"
        );

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
                self.touch(*id);
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
        smelt_perf::perf::record_value("transcript:render_block_cache:misses", rendered as u64);
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 8);
        smelt_perf::perf::record_value("transcript:render_block_cache:workers", workers as u64);
        let chunk_size = tasks.len().div_ceil(workers).max(1);

        let results: Vec<(BlockId, LayoutKey, Buffer)> = {
            let _perf = smelt_perf::perf::begin("transcript:render_block_cache:layout_misses");
            std::thread::scope(|scope| {
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
            })
        };

        for (id, key, buf) in results {
            self.blocks.insert(id, CachedBlock { key, buf });
            self.touch(id);
        }
        self.evict_unpinned(ids);
        rendered
    }

    /// Cached buffer for `id`, or `None` if not yet laid out (or stale).
    pub fn get(&self, id: BlockId, key: LayoutKey) -> Option<&Buffer> {
        self.blocks
            .get(&id)
            .filter(|c| c.key == key)
            .map(|c| &c.buf)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.recency.clear();
    }
}
