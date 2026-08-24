use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use protocol::{EngineEvent, InvocationId};

pub(super) const STREAM_CONTENT_SLICE_BYTES: usize = 4 * 1024 * 1024;

pub(super) struct PendingToolOutputAppend {
    pub(super) block_id: smelt_core::transcript_model::BlockId,
    pub(super) invocation_id: InvocationId,
    pub(super) source: Arc<String>,
    pub(super) offset: usize,
    pub(super) line_start: bool,
    pub(super) defer_until_frame: bool,
}

impl PendingToolOutputAppend {
    pub(super) fn new(
        block_id: smelt_core::transcript_model::BlockId,
        invocation_id: InvocationId,
        source: Arc<String>,
        offset: usize,
        line_start: bool,
    ) -> Self {
        Self {
            block_id,
            invocation_id,
            source,
            offset,
            line_start,
            defer_until_frame: true,
        }
    }
}

pub(super) enum TranscriptWork {
    ProviderContinuation(EngineEvent),
    AuxiliaryContinuation(EngineEvent),
    ToolOutputAppend(PendingToolOutputAppend),
    DeferredReasoningSummary {
        event: EngineEvent,
        block_id: smelt_core::transcript_model::BlockId,
    },
    OrderedEngineEvent(EngineEvent),
    AppendExecOutput(String),
    FinishExec(Option<i32>),
    FinalizeExec,
}

impl TranscriptWork {
    fn pending_tool_output_invocation(&self) -> Option<InvocationId> {
        match self {
            Self::ProviderContinuation(EngineEvent::ToolOutput { invocation_id, .. }) => {
                Some(*invocation_id)
            }
            Self::ToolOutputAppend(pending) => Some(pending.invocation_id),
            _ => None,
        }
    }

    fn is_main_turn_work(&self) -> bool {
        matches!(
            self,
            Self::ProviderContinuation(_)
                | Self::ToolOutputAppend(_)
                | Self::DeferredReasoningSummary { .. }
                | Self::OrderedEngineEvent(_)
        )
    }
}

#[derive(Default)]
pub(super) struct TranscriptWorkQueue {
    items: VecDeque<TranscriptWork>,
    pending_tool_outputs: HashMap<InvocationId, usize>,
    pending_tool_draft_summaries: Vec<smelt_core::transcript_model::BlockId>,
    main_turn_work_count: usize,
}

impl TranscriptWorkQueue {
    pub(super) fn push_provider_continuation(&mut self, event: EngineEvent) {
        self.push_back(TranscriptWork::ProviderContinuation(event));
    }

    pub(super) fn push_auxiliary_continuation(&mut self, event: EngineEvent) {
        self.push_back(TranscriptWork::AuxiliaryContinuation(event));
    }

    pub(super) fn push_deferred_reasoning_summary(
        &mut self,
        event: EngineEvent,
        block_id: smelt_core::transcript_model::BlockId,
    ) {
        self.push_back(TranscriptWork::DeferredReasoningSummary { event, block_id });
    }

    pub(super) fn push_front_deferred_reasoning_summary(
        &mut self,
        event: EngineEvent,
        block_id: smelt_core::transcript_model::BlockId,
    ) {
        let work = TranscriptWork::DeferredReasoningSummary { event, block_id };
        self.increment_work_counts(&work);
        self.items.push_front(work);
    }

    pub(super) fn push_ordered_engine_event(&mut self, event: EngineEvent) {
        self.push_back(TranscriptWork::OrderedEngineEvent(event));
    }

    pub(super) fn push_append_exec_output(&mut self, chunk: String) {
        self.push_back(TranscriptWork::AppendExecOutput(chunk));
    }

    pub(super) fn push_finish_exec(&mut self, exit_code: Option<i32>) {
        self.push_back(TranscriptWork::FinishExec(exit_code));
    }

    pub(super) fn push_finalize_exec(&mut self) {
        self.push_back(TranscriptWork::FinalizeExec);
    }

    pub(super) fn push_front_tool_output(&mut self, pending: PendingToolOutputAppend) {
        let work = TranscriptWork::ToolOutputAppend(pending);
        self.increment_work_counts(&work);
        self.items.push_front(work);
    }

    pub(super) fn pop_front(&mut self) -> Option<TranscriptWork> {
        let work = self.items.pop_front()?;
        self.decrement_work_counts(&work);
        Some(work)
    }

    pub(super) fn has_pending_tool_output(&self, invocation_id: InvocationId) -> bool {
        self.pending_tool_outputs
            .get(&invocation_id)
            .is_some_and(|count| *count > 0)
    }

    pub(super) fn request_tool_draft_summary(
        &mut self,
        block_id: smelt_core::transcript_model::BlockId,
    ) {
        if !self.pending_tool_draft_summaries.contains(&block_id) {
            self.pending_tool_draft_summaries.push(block_id);
        }
    }

    pub(super) fn cancel_tool_draft_summary(
        &mut self,
        block_id: smelt_core::transcript_model::BlockId,
    ) {
        self.pending_tool_draft_summaries
            .retain(|pending| *pending != block_id);
    }

    pub(super) fn take_tool_draft_summaries(
        &mut self,
    ) -> Vec<smelt_core::transcript_model::BlockId> {
        std::mem::take(&mut self.pending_tool_draft_summaries)
    }

    pub(super) fn has_main_turn_work(&self) -> bool {
        self.main_turn_work_count > 0
    }

    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty() && self.pending_tool_draft_summaries.is_empty()
    }

    pub(super) fn front_is_tool_output_append(&self) -> bool {
        matches!(
            self.items.front(),
            Some(TranscriptWork::ToolOutputAppend(_))
        )
    }

    pub(super) fn front_waits_for_hydration(&self) -> bool {
        matches!(
            self.items.front(),
            Some(TranscriptWork::DeferredReasoningSummary { .. })
        )
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.items.len()
    }

    pub(super) fn discard_main_turn_work(&mut self) {
        self.items.retain(|work| !work.is_main_turn_work());
        self.pending_tool_outputs.clear();
        self.pending_tool_draft_summaries.clear();
        self.main_turn_work_count = 0;
    }

    fn push_back(&mut self, work: TranscriptWork) {
        self.increment_work_counts(&work);
        self.items.push_back(work);
    }

    fn increment_work_counts(&mut self, work: &TranscriptWork) {
        if work.is_main_turn_work() {
            self.main_turn_work_count = self.main_turn_work_count.saturating_add(1);
        }
        let Some(invocation_id) = work.pending_tool_output_invocation() else {
            return;
        };
        *self.pending_tool_outputs.entry(invocation_id).or_default() += 1;
    }

    fn decrement_work_counts(&mut self, work: &TranscriptWork) {
        if work.is_main_turn_work() {
            self.main_turn_work_count = self.main_turn_work_count.saturating_sub(1);
        }
        let Some(invocation_id) = work.pending_tool_output_invocation() else {
            return;
        };
        let Some(count) = self.pending_tool_outputs.get_mut(&invocation_id) else {
            debug_assert!(false, "pending tool output count must exist");
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.pending_tool_outputs.remove(&invocation_id);
        }
    }
}

pub(super) fn stream_content_slice_end(source: &str, start: usize, budget: usize) -> usize {
    let target = start.saturating_add(budget).min(source.len());
    let end = smelt_buffer::text::snap(source, target);
    if end > start || end == source.len() {
        end
    } else {
        smelt_buffer::text::next_char_boundary(source, start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_draft_summary_requests_coalesce_in_order() {
        let mut work = TranscriptWorkQueue::default();
        let first = smelt_core::transcript_model::BlockId::new(1);
        let second = smelt_core::transcript_model::BlockId::new(2);

        work.request_tool_draft_summary(first);
        work.request_tool_draft_summary(first);
        work.request_tool_draft_summary(second);

        assert!(!work.has_main_turn_work());
        assert_eq!(work.take_tool_draft_summaries(), vec![first, second]);
        assert!(work.is_empty());
    }
}
