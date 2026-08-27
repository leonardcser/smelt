use std::collections::{HashMap, HashSet, VecDeque};

use smelt_core::transcript_model::BlockId;

const DEFAULT_HYDRATED_BLOCK_BUDGET: usize = 32 * 1024 * 1024;
const DEFAULT_RECORD_WINDOW_BUDGET: usize = 16 * 1024 * 1024;
const DEFAULT_RENDERED_PAYLOAD_BUDGET: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptMemoryBudget {
    pub(crate) hydrated_blocks: usize,
    pub(crate) record_windows: usize,
    pub(crate) rendered_rows: usize,
}

impl Default for TranscriptMemoryBudget {
    fn default() -> Self {
        Self {
            hydrated_blocks: DEFAULT_HYDRATED_BLOCK_BUDGET,
            record_windows: DEFAULT_RECORD_WINDOW_BUDGET,
            rendered_rows: DEFAULT_RENDERED_PAYLOAD_BUDGET,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TranscriptMemorySnapshot {
    pub(crate) live_blocks: usize,
    pub(crate) stored_blocks: usize,
    pub(crate) hydrated_blocks: usize,
    pub(crate) hydrated_budget_bytes: usize,
    pub(crate) record_budget_bytes: usize,
    pub(crate) rendered_budget_bytes: usize,
    pub(crate) live_block_bytes: usize,
    pub(crate) live_tool_state_bytes: usize,
    pub(crate) hydrated_block_bytes: usize,
    pub(crate) hydrated_tool_state_bytes: usize,
    pub(crate) compact_record_bytes: usize,
    pub(crate) record_window_bytes: usize,
    pub(crate) tool_state_index_bytes: usize,
    pub(crate) block_metadata_bytes: usize,
    pub(crate) layout_bytes: usize,
    pub(crate) height_index_bytes: usize,
    pub(crate) height_index_cache_bytes: usize,
    pub(crate) visible_rows_bytes: usize,
    pub(crate) full_rows_bytes: usize,
    pub(crate) pinned_hydrated_bytes: usize,
    pub(crate) pinned_rendered_bytes: usize,
    pub(crate) hydrated_oversize_debt_bytes: usize,
    pub(crate) record_oversize_debt_bytes: usize,
    pub(crate) rendered_oversize_debt_bytes: usize,
    pub(crate) hydration_reads: u64,
    pub(crate) hydration_ranges: u64,
    pub(crate) hydration_bytes: u64,
    pub(crate) hydration_duration_us: u64,
    pub(crate) evicted_entries: u64,
    pub(crate) evicted_bytes: u64,
    pub(crate) dematerialized_entries: u64,
    pub(crate) dematerialized_bytes: u64,
}

#[derive(Default)]
pub(super) struct TranscriptHydrationState {
    pub(super) lru: VecDeque<BlockId>,
    pub(super) lru_ids: HashSet<BlockId>,
    pub(super) viewport_pins: HashSet<BlockId>,
    pub(super) projection_hydration_pins: HashSet<BlockId>,
    pub(super) pending_projection_pin: Option<BlockId>,
    pub(super) record_save_pins: HashSet<BlockId>,
    pub(super) engine_event_pins: HashSet<BlockId>,
    pub(super) search_pin: Option<BlockId>,
    pub(super) search_candidate_pins: HashSet<BlockId>,
    pub(super) record_failed: bool,
    pub(super) failed_blocks: HashSet<BlockId>,
    pub(super) operation_pins: HashMap<BlockId, usize>,
    pub(super) hydration_reads: u64,
    pub(super) hydration_ranges: u64,
    pub(super) hydration_bytes: u64,
    pub(super) hydration_duration_us: u64,
    pub(super) evicted_entries: u64,
    pub(super) evicted_bytes: u64,
    pub(super) dematerialized_entries: u64,
    pub(super) dematerialized_bytes: u64,
}

impl TranscriptHydrationState {
    pub(super) fn is_pinned(&self, id: BlockId) -> bool {
        self.viewport_pins.contains(&id)
            || self.projection_hydration_pins.contains(&id)
            || self.pending_projection_pin == Some(id)
            || self.record_save_pins.contains(&id)
            || self.engine_event_pins.contains(&id)
            || self.search_pin == Some(id)
            || self.search_candidate_pins.contains(&id)
            || self.operation_pins.contains_key(&id)
    }

    pub(super) fn touch_many(&mut self, ids: &[BlockId]) {
        let mut moved = ids.iter().copied().collect::<HashSet<_>>();
        if moved.is_empty() {
            return;
        }
        self.lru.retain(|candidate| !moved.contains(candidate));
        for id in ids {
            if moved.remove(id) {
                self.lru_ids.insert(*id);
                self.lru.push_back(*id);
            }
        }
    }

    pub(super) fn pin_operation(&mut self, ids: &[BlockId]) {
        for id in ids {
            *self.operation_pins.entry(*id).or_default() += 1;
        }
    }

    pub(super) fn unpin_operation(&mut self, ids: &[BlockId]) {
        for id in ids {
            let Some(count) = self.operation_pins.get_mut(id) else {
                continue;
            };
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.operation_pins.remove(id);
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PendingTranscriptCompaction {
    pub(super) record_len: usize,
    pub(super) next_order_index: usize,
    pub(super) next_record_index: usize,
}
