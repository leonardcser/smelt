use std::collections::HashMap;
use std::time::{Duration, Instant};

use protocol::{
    HistoryAppend, HistoryAppendResult, HistoryItem, ReasoningEffort, StyledLines, TokenUsage,
    TurnMeta,
};
use smelt_core::content::stream_parser::{StreamParser, ToolDraftUpdate, ToolStart};
use smelt_core::session::{
    ContextCheckpoint, ContextTokenIdentity, Session, SessionHeader, SessionMeta, SessionStoreRef,
};
use smelt_core::session_runtime::LiveSession;
use smelt_core::transcript_model::{Block, BlockId, BlockOrigin, ToolOutputRef, ToolStatus};

use crate::app::transcript::TranscriptDocument;
use crate::persist::SessionEpoch;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PersistenceGeneration(u64);

impl Default for PersistenceGeneration {
    fn default() -> Self {
        Self::ZERO
    }
}

impl PersistenceGeneration {
    pub(crate) const ZERO: Self = Self(0);

    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DocumentChanges {
    current: PersistenceGeneration,
    durable: PersistenceGeneration,
    acknowledged_head: smelt_store::StoreHead,
    history_dirty_from: Option<usize>,
}

impl DocumentChanges {
    fn reserve_generation(&self) -> PersistenceGeneration {
        self.current
            .checked_next()
            .expect("session persistence generation overflow")
    }

    fn record(&mut self, generation: PersistenceGeneration, result: &MutationResult) {
        if !result.canonical_changed() {
            return;
        }
        debug_assert_eq!(generation, self.reserve_generation());
        self.current = generation;
        if let Some(index) = result.history_dirty_from {
            self.history_dirty_from = Some(
                self.history_dirty_from
                    .map_or(index, |current| current.min(index)),
            );
        }
    }

    fn force_dirty(&mut self) {
        self.current = self.reserve_generation();
    }

    fn force_dirty_from(&mut self, history_index: usize) {
        self.force_dirty();
        self.history_dirty_from = Some(
            self.history_dirty_from
                .map_or(history_index, |current| current.min(history_index)),
        );
    }

    fn install_head(&mut self, head: smelt_store::StoreHead) {
        *self = Self {
            acknowledged_head: head,
            ..Self::default()
        };
    }

    fn mark_clean(&mut self, head: smelt_store::StoreHead) {
        self.durable = self.current;
        self.acknowledged_head = head;
        self.history_dirty_from = None;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SessionSaveIntent {
    pub(crate) generation: PersistenceGeneration,
    pub(crate) identity: smelt_store::SessionIdentity,
    pub(crate) metadata: smelt_store::SessionMetadata,
    pub(crate) history: smelt_store::HistorySuffix,
    pub(crate) side_tables: smelt_store::SideTableSuffixes,
    pub(crate) descriptors: Option<smelt_store::TranscriptDescriptorSuffix>,
}

struct AcknowledgementView<'a> {
    generation: PersistenceGeneration,
    previous: smelt_store::StoreHead,
    receipt: &'a smelt_store::SaveReceipt,
    coalesced: bool,
}

pub(crate) struct SessionDocument {
    session: Session,
    transcript: crate::app::transcript::LoadedTranscript,
    live_session: Option<smelt_core::session_runtime::LiveSession>,
    store_head: Option<smelt_store::StoreHead>,
}

pub(crate) struct TuiSessionDocument {
    pub(crate) transcript: TranscriptDocument,
    pub(crate) live_session: Option<LiveSession>,
    changes: DocumentChanges,
    persistence_epoch: Option<SessionEpoch>,
}

impl TuiSessionDocument {
    pub(crate) fn new(transcript: TranscriptDocument) -> Self {
        Self {
            transcript,
            live_session: None,
            changes: DocumentChanges::default(),
            persistence_epoch: None,
        }
    }

    pub(crate) fn apply(
        &mut self,
        session: &mut Session,
        parser: &mut StreamParser,
        persist_mutation: bool,
        mutation: SessionMutation,
    ) -> MutationResult {
        let reserved = persist_mutation.then(|| self.changes.reserve_generation());
        let result = SessionDocument::apply_runtime(
            session,
            self.live_session.as_mut(),
            &mut self.transcript,
            parser,
            mutation,
        );
        if let Some(generation) = reserved {
            self.changes.record(generation, &result);
        }
        result
    }

    pub(crate) fn has_session_work(&self) -> bool {
        self.changes.current > self.changes.durable
    }

    pub(crate) fn generation(&self) -> PersistenceGeneration {
        self.changes.current
    }

    pub(crate) fn durable_generation(&self) -> PersistenceGeneration {
        self.changes.durable
    }

    pub(crate) fn descriptors_persisted(&self) -> bool {
        self.transcript.history().descriptor_dirty_from().is_none()
            && self.changes.current == self.changes.durable
    }

    pub(crate) fn acknowledged_head(&self) -> smelt_store::StoreHead {
        self.changes.acknowledged_head
    }

    pub(crate) fn bind_persistence(&mut self, epoch: SessionEpoch) {
        self.persistence_epoch = Some(epoch);
    }

    pub(crate) fn unbind_persistence(&mut self, epoch: SessionEpoch) {
        if self.persistence_epoch == Some(epoch) {
            self.persistence_epoch = None;
        }
    }

    pub(crate) fn mark_session_unpersisted(&mut self) {
        self.persistence_epoch = None;
        self.changes.install_head(smelt_store::StoreHead::default());
        self.changes.force_dirty_from(0);
        self.transcript
            .history_mut()
            .require_descriptor_resave_from(0);
    }

    pub(crate) fn install_materialized_session(&mut self, descriptors_persisted: bool) {
        if descriptors_persisted {
            self.transcript.history_mut().clear_descriptor_dirty();
        } else {
            if self.transcript.history().descriptor_dirty_from().is_none() {
                self.changes.force_dirty();
            }
            self.transcript
                .history_mut()
                .require_descriptor_resave_from(0);
        }
    }

    pub(crate) fn install_loaded_full_session(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
        store_head: Option<smelt_store::StoreHead>,
    ) {
        self.live_session = None;
        self.transcript.replace_loaded_transcript(transcript);
        self.changes.install_head(store_head.unwrap_or_default());
        self.persistence_epoch = None;
        self.changes.force_dirty_from(0);
        self.transcript
            .history_mut()
            .require_descriptor_resave_from(0);
    }

    pub(crate) fn install_loaded_store_session(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
        live_session: LiveSession,
        store_head: smelt_store::StoreHead,
        repair_descriptors: bool,
    ) {
        debug_assert_eq!(
            store_head.history_len.as_usize(),
            Some(live_session.history_len())
        );
        self.live_session = Some(live_session);
        self.transcript.replace_loaded_transcript(transcript);
        self.changes.install_head(store_head);
        self.persistence_epoch = None;
        if repair_descriptors {
            // The descriptor fallback materializes a complete transcript.
            // Repair only its descriptor projection, preserving store-backed
            // history and side-table rows outside the transcript.
            self.changes.force_dirty();
            self.transcript
                .history_mut()
                .require_descriptor_resave_from(0);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_history_resave_from_for_test(&mut self, history_index: usize) {
        self.changes.force_dirty_from(history_index);
    }

    #[cfg(test)]
    pub(crate) fn dirty_history_from_for_test(&self) -> Option<usize> {
        self.changes.history_dirty_from
    }

    pub(crate) fn prepare_save(
        &mut self,
        session: &mut Session,
        metadata: RuntimeSessionMetadata,
    ) -> Result<Option<SessionSaveIntent>, String> {
        let history = self.live_session.as_ref().map_or(
            SessionHistoryRef::Materialized(&session.history),
            SessionHistoryRef::StoreBacked,
        );
        let history_len = history.len();
        let descriptor_dirty = self.transcript.history().descriptor_dirty_from().is_some();
        if self.changes.current == self.changes.durable && !descriptor_dirty {
            return Ok(None);
        }
        if self.changes.acknowledged_head == smelt_store::StoreHead::default()
            && history_len == 0
            && self.transcript.is_empty()
        {
            return Ok(None);
        }

        self.apply_runtime_metadata(session, metadata);
        let history = self.live_session.as_ref().map_or(
            SessionHistoryRef::Materialized(&session.history),
            SessionHistoryRef::StoreBacked,
        );
        let history_len = history.len();
        let acknowledged_history_len = self
            .changes
            .acknowledged_head
            .history_len
            .as_usize()
            .ok_or_else(|| "acknowledged history length exceeds platform limits".to_string())?;
        let history_start = self
            .changes
            .history_dirty_from
            .map_or(acknowledged_history_len, |dirty| {
                dirty.min(acknowledged_history_len)
            })
            .min(history_len);
        let history_items = history.range(history_start..history_len)?;
        let history = smelt_store::HistorySuffix {
            start: smelt_store::HistoryIndex::new(history_start as u64),
            final_len: smelt_store::HistoryLen::new(history_len as u64),
            items: history_items,
        };
        let identity = smelt_core::session::store_identity_from_session(session)
            .map_err(|error| error.to_string())?;
        let metadata = smelt_core::session::store_metadata_from_session(session, history_len)
            .map_err(|error| error.to_string())?;
        let side_tables = smelt_core::session::store_side_table_suffixes_from_session_at(
            session,
            history_start,
            history_len,
        )
        .map_err(|error| error.to_string())?;
        let descriptors =
            descriptor_save_plan(&self.transcript, true, self.changes.history_dirty_from)
                .map(|plan| {
                    let records = plan
                        .records
                        .iter()
                        .enumerate()
                        .map(|(offset, record)| {
                            transcript_descriptor_row(
                                plan.start_descriptor_idx + offset,
                                record,
                                &history,
                            )
                        })
                        .collect::<Result<Vec<_>, smelt_store::StoreError>>()?;
                    Ok::<_, smelt_store::StoreError>(smelt_store::TranscriptDescriptorSuffix {
                        start: smelt_store::DescriptorIndex::new(plan.start_descriptor_idx as u64),
                        records,
                    })
                })
                .transpose()
                .map_err(|error| format!("prepare transcript descriptors: {error}"))?;
        Ok(Some(SessionSaveIntent {
            generation: self.changes.current,
            identity,
            metadata,
            history,
            side_tables,
            descriptors,
        }))
    }

    pub(crate) fn acknowledge(
        &mut self,
        epoch: SessionEpoch,
        generation: PersistenceGeneration,
        receipt: &smelt_store::SaveReceipt,
        session_id: &str,
        history_len: usize,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> bool {
        self.acknowledge_from(
            epoch,
            AcknowledgementView {
                generation,
                previous: receipt.previous,
                receipt,
                coalesced: false,
            },
            session_id,
            history_len,
            checkpoint,
        )
    }

    pub(crate) fn acknowledge_convergence(
        &mut self,
        epoch: SessionEpoch,
        acknowledgement: &crate::persist::PersistenceAcknowledgement,
        session_id: &str,
        history_len: usize,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> bool {
        self.acknowledge_from(
            epoch,
            AcknowledgementView {
                generation: acknowledgement.generation,
                previous: acknowledgement.previous,
                receipt: &acknowledgement.receipt,
                coalesced: true,
            },
            session_id,
            history_len,
            checkpoint,
        )
    }

    fn acknowledge_from(
        &mut self,
        epoch: SessionEpoch,
        acknowledgement: AcknowledgementView<'_>,
        session_id: &str,
        history_len: usize,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> bool {
        let AcknowledgementView {
            generation,
            previous,
            receipt,
            coalesced,
        } = acknowledgement;
        if self.persistence_epoch != Some(epoch) || generation != self.changes.current {
            return false;
        }
        let expected_descriptor_len =
            descriptor_save_plan(&self.transcript, true, self.changes.history_dirty_from)
                .and_then(|plan| plan.start_descriptor_idx.checked_add(plan.records.len()))
                .unwrap_or_else(|| {
                    self.changes
                        .acknowledged_head
                        .descriptor_len
                        .as_usize()
                        .expect("document descriptor length originated as usize")
                });
        let revision_advanced_once = receipt
            .previous
            .revision
            .checked_add(1)
            .is_some_and(|revision| revision == receipt.current.revision);
        let revision_valid = if coalesced {
            receipt.previous.revision.get() >= previous.revision.get()
                && receipt.current.revision.get() >= receipt.previous.revision.get()
        } else {
            receipt.previous == previous
                && (receipt.current.revision == receipt.previous.revision || revision_advanced_once)
        };
        let descriptor_projection_valid =
            self.transcript
                .descriptor_total_count()
                .is_none_or(|total| {
                    expected_descriptor_len >= total
                        || self
                            .transcript
                            .can_reconcile_dense_descriptor_count(expected_descriptor_len)
                });
        if receipt.session_id != session_id
            || receipt.current.history_len.as_usize() != Some(history_len)
            || receipt.current.descriptor_len.as_usize() != Some(expected_descriptor_len)
            || previous != self.changes.acknowledged_head
            || !revision_valid
            || !descriptor_projection_valid
        {
            return false;
        }
        if let Some(live_session) = self.live_session.as_mut() {
            live_session.compact_saved_prefix(
                history_len,
                receipt.current.revision.get(),
                checkpoint,
            );
        }
        if let Some(total) = self.transcript.descriptor_total_count() {
            if expected_descriptor_len >= total {
                self.transcript
                    .note_persisted_descriptor_append(expected_descriptor_len - total);
            } else if !self
                .transcript
                .reconcile_dense_descriptor_count(expected_descriptor_len)
            {
                return false;
            }
        }
        self.transcript.history_mut().clear_descriptor_dirty();
        self.changes.mark_clean(receipt.current);
        true
    }

    pub(crate) fn mark_ephemeral_persisted(&mut self) {
        self.changes.durable = self.changes.current;
        self.changes.history_dirty_from = None;
        self.transcript.history_mut().clear_descriptor_dirty();
    }

    pub(crate) fn has_unflushed_work(&self, _session: &Session) -> bool {
        self.changes.current > self.changes.durable
    }

    fn apply_runtime_metadata(&mut self, session: &mut Session, metadata: RuntimeSessionMetadata) {
        let generation = self.changes.reserve_generation();
        let result = SessionDocument::apply_to_session(
            session,
            SessionMutation::UpdateRuntimeMetadata {
                updated_at_ms: metadata.updated_at_ms,
                mode: metadata.mode,
                reasoning_effort: metadata.reasoning_effort,
                model: metadata.model,
                fast_mode: metadata.fast_mode,
            },
        );
        self.changes.record(generation, &result);
    }
}

pub(crate) struct FullSessionDocument {
    pub(crate) session: Session,
    pub(crate) transcript: crate::app::transcript::LoadedTranscript,
}

pub(crate) struct StoreBackedSessionDocument {
    pub(crate) session: Session,
    pub(crate) transcript: crate::app::transcript::LoadedTranscript,
    pub(crate) live_session: smelt_core::session_runtime::LiveSession,
    pub(crate) store_head: smelt_store::StoreHead,
    pub(crate) repair_descriptors: bool,
}

struct SessionDescriptorSavePlan {
    start_descriptor_idx: usize,
    records: Vec<smelt_core::TranscriptBlockRecordWithId>,
}

#[derive(Clone, Copy)]
enum SessionHistoryRef<'a> {
    Materialized(&'a [HistoryItem]),
    StoreBacked(&'a LiveSession),
}

impl<'a> SessionHistoryRef<'a> {
    fn len(self) -> usize {
        match self {
            SessionHistoryRef::Materialized(history) => history.len(),
            SessionHistoryRef::StoreBacked(live) => live.history_len(),
        }
    }

    fn range(self, range: std::ops::Range<usize>) -> Result<Vec<HistoryItem>, String> {
        match self {
            SessionHistoryRef::Materialized(history) => {
                let end = range.end.min(history.len());
                let start = range.start.min(end);
                Ok(history[start..end].to_vec())
            }
            SessionHistoryRef::StoreBacked(live) => live.history_range(range),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeSessionMetadata {
    pub(crate) updated_at_ms: u64,
    pub(crate) mode: String,
    pub(crate) reasoning_effort: ReasoningEffort,
    pub(crate) model: Option<String>,
    pub(crate) fast_mode: bool,
}

pub(crate) enum SessionMutation {
    AppendHistoryItem {
        item: HistoryItem,
    },
    CommitRequestHistoryItem {
        item: HistoryItem,
        block: Option<Block>,
        first_user_message: Option<String>,
    },
    ApplyHistoryAppend {
        append: HistoryAppend,
        identity: ContextTokenIdentity,
    },
    TruncateHistoryFrom {
        index: usize,
        identity: ContextTokenIdentity,
    },
    RewindHistoryTo {
        index: usize,
        keep_checkpoint_at_boundary: bool,
        identity: ContextTokenIdentity,
    },
    AppendTranscriptBlock {
        block: Block,
    },
    InsertCheckpointMarker {
        block_index: usize,
        block: Block,
    },
    RemoveUnoriginatedTranscriptBlockAt {
        block_index: usize,
    },
    ReplaceTranscriptFromHistory {
        transcript: smelt_core::content::transcript::Transcript,
    },
    TruncateTranscriptTo {
        block_index: usize,
    },
    ClearTranscript,
    UpdateCompactionPreview {
        summary: String,
    },
    ClearCompactionPreview,
    RewriteTranscriptBlock {
        id: BlockId,
        block: Block,
    },
    AppendStreamingThinking {
        delta: String,
    },
    FlushStreamingThinking,
    AppendStreamingText {
        delta: String,
    },
    FlushStreamingText,
    SyncActiveToolElapsed {
        now: Instant,
    },
    StartTool {
        call_id: String,
        name: String,
        summary: StyledLines,
        args: HashMap<String, serde_json::Value>,
        now: Instant,
    },
    AppendActiveToolOutput {
        call_id: String,
        chunk: String,
    },
    SetActiveToolStatus {
        call_id: String,
        status: ToolStatus,
        now: Instant,
    },
    SetActiveToolUserMessage {
        call_id: String,
        message: String,
    },
    FinishTool {
        call_id: String,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
        now: Instant,
    },
    FinalizeActiveTools,
    PromoteToolDraft {
        stream_id: Option<String>,
        start: ToolStart,
        now: Instant,
    },
    ClearToolDrafts,
    UpsertToolDraft {
        update: ToolDraftUpdate,
    },
    StartExec {
        command: String,
    },
    AppendExecOutput {
        chunk: String,
    },
    FinishExec {
        exit_code: Option<i32>,
    },
    FinalizeExec,
    RecordTokenUsage {
        usage: TokenUsage,
        identity: ContextTokenIdentity,
    },
    ClearContextTokensBaselineIfMismatched {
        identity: ContextTokenIdentity,
    },
    AccumulateUsage {
        usage: TokenUsage,
        cost_usd: f64,
    },
    SetTitle {
        title: String,
        slug: String,
        snapshot_history_len: usize,
    },
    UpdateRuntimeMetadata {
        updated_at_ms: u64,
        mode: String,
        reasoning_effort: ReasoningEffort,
        model: Option<String>,
        fast_mode: bool,
    },
    SetFastMode {
        enabled: bool,
    },
    SetCwd {
        cwd: String,
    },
    FinishTurnState {
        history_len: usize,
        meta: TurnMeta,
        snapshot_context: bool,
        update_context_token_history_len: bool,
    },
    InstallContextCheckpoint {
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    },
    InstallContextCheckpointAtHistoryIndex {
        kind: String,
        summary: String,
        first_live_index: usize,
        tokens_before: Option<u32>,
        history_len: usize,
    },
    SetCheckpoint {
        checkpoint: Option<ContextCheckpoint>,
    },
    SetCheckpointTokensAfterEstimate {
        tokens: u32,
        history_len: usize,
    },
    RestoreMetadataAfterRewind {
        history_len: usize,
    },
    PruneRewindableState {
        history_len: usize,
        identity: ContextTokenIdentity,
    },
}

#[cfg(test)]
pub(crate) enum SessionMutationTarget<'a> {
    Session {
        session: &'a mut Session,
    },
    SessionAndTranscript {
        session: &'a mut Session,
        transcript: &'a mut TranscriptDocument,
    },
    LiveSessionAndTranscript {
        session: &'a mut Session,
        live_session: &'a mut LiveSession,
        transcript: &'a mut TranscriptDocument,
    },
    Transcript {
        transcript: &'a mut TranscriptDocument,
    },
    StreamParserTranscript {
        parser: &'a mut StreamParser,
        transcript: &'a mut TranscriptDocument,
    },
}

#[derive(Default)]
pub(crate) struct MutationResult {
    pub(crate) session_dirty: bool,
    #[allow(dead_code)]
    pub(crate) transcript_dirty: bool,
    pub(crate) descriptors_unpersisted: bool,
    pub(crate) context_tokens_updated: bool,
    pub(crate) applied: bool,
    pub(crate) history_idx: Option<usize>,
    pub(crate) history_dirty_from: Option<usize>,
    pub(crate) block_id: Option<BlockId>,
    pub(crate) history_append_result: Option<HistoryAppendResult>,
    pub(crate) turn_meta: Option<TurnMeta>,
}

impl MutationResult {
    fn canonical_changed(&self) -> bool {
        self.session_dirty
            || self.history_dirty_from.is_some()
            || self.transcript_dirty
            || self.descriptors_unpersisted
    }
}

impl FullSessionDocument {
    pub(crate) fn new(
        session: Session,
        transcript: crate::app::transcript::LoadedTranscript,
    ) -> Self {
        Self {
            session,
            transcript,
        }
    }
}

impl StoreBackedSessionDocument {
    pub(crate) fn new(
        session: Session,
        transcript: crate::app::transcript::LoadedTranscript,
        live_session: smelt_core::session_runtime::LiveSession,
        store_head: smelt_store::StoreHead,
    ) -> Self {
        Self {
            session,
            transcript,
            live_session,
            store_head,
            repair_descriptors: false,
        }
    }

    pub(crate) fn requiring_descriptor_repair(mut self) -> Self {
        self.repair_descriptors = true;
        self
    }
}

impl SessionDocument {
    #[cfg(test)]
    pub(crate) fn apply(
        target: SessionMutationTarget<'_>,
        mutation: SessionMutation,
    ) -> MutationResult {
        match target {
            SessionMutationTarget::Session { session } => Self::apply_to_session(session, mutation),
            SessionMutationTarget::SessionAndTranscript {
                session,
                transcript,
            } => Self::apply_to_session_and_transcript(session, transcript, mutation),
            SessionMutationTarget::LiveSessionAndTranscript {
                session,
                live_session,
                transcript,
            } => Self::apply_to_live_session_and_transcript(
                session,
                live_session,
                transcript,
                mutation,
            ),
            SessionMutationTarget::Transcript { transcript } => {
                Self::apply_to_transcript(transcript, mutation)
            }
            SessionMutationTarget::StreamParserTranscript { parser, transcript } => {
                Self::apply_to_stream_parser_transcript(parser, transcript, mutation)
            }
        }
    }

    fn apply_runtime(
        session: &mut Session,
        live_session: Option<&mut LiveSession>,
        transcript: &mut TranscriptDocument,
        parser: &mut StreamParser,
        mutation: SessionMutation,
    ) -> MutationResult {
        match mutation {
            SessionMutation::CommitRequestHistoryItem { .. } => {
                if let Some(live_session) = live_session {
                    Self::apply_to_live_session_and_transcript(
                        session,
                        live_session,
                        transcript,
                        mutation,
                    )
                } else {
                    Self::apply_to_session_and_transcript(session, transcript, mutation)
                }
            }
            SessionMutation::AppendHistoryItem { .. }
            | SessionMutation::TruncateHistoryFrom { .. }
            | SessionMutation::RewindHistoryTo { .. }
            | SessionMutation::SetCheckpoint { .. }
            | SessionMutation::SetCheckpointTokensAfterEstimate { .. } => {
                if let Some(live_session) = live_session {
                    Self::apply_to_live_session(session, live_session, mutation)
                } else {
                    Self::apply_to_session(session, mutation)
                }
            }
            SessionMutation::AppendTranscriptBlock { .. }
            | SessionMutation::InsertCheckpointMarker { .. }
            | SessionMutation::RemoveUnoriginatedTranscriptBlockAt { .. }
            | SessionMutation::ReplaceTranscriptFromHistory { .. }
            | SessionMutation::TruncateTranscriptTo { .. }
            | SessionMutation::ClearTranscript
            | SessionMutation::UpdateCompactionPreview { .. }
            | SessionMutation::ClearCompactionPreview
            | SessionMutation::RewriteTranscriptBlock { .. } => {
                Self::apply_to_transcript(transcript, mutation)
            }
            SessionMutation::AppendStreamingThinking { .. }
            | SessionMutation::FlushStreamingThinking
            | SessionMutation::AppendStreamingText { .. }
            | SessionMutation::FlushStreamingText
            | SessionMutation::SyncActiveToolElapsed { .. }
            | SessionMutation::StartTool { .. }
            | SessionMutation::AppendActiveToolOutput { .. }
            | SessionMutation::SetActiveToolStatus { .. }
            | SessionMutation::SetActiveToolUserMessage { .. }
            | SessionMutation::FinishTool { .. }
            | SessionMutation::FinalizeActiveTools
            | SessionMutation::PromoteToolDraft { .. }
            | SessionMutation::ClearToolDrafts
            | SessionMutation::UpsertToolDraft { .. }
            | SessionMutation::StartExec { .. }
            | SessionMutation::AppendExecOutput { .. }
            | SessionMutation::FinishExec { .. }
            | SessionMutation::FinalizeExec => {
                Self::apply_to_stream_parser_transcript(parser, transcript, mutation)
            }
            mutation => Self::apply_to_session(session, mutation),
        }
    }

    pub(crate) fn from_full_session(
        session: Session,
        transcript: crate::app::transcript::LoadedTranscript,
    ) -> Self {
        Self {
            session,
            transcript,
            live_session: None,
            store_head: None,
        }
    }

    pub(crate) fn from_store(
        header: SessionHeader,
        store_ref: SessionStoreRef,
        store_head: smelt_store::StoreHead,
        transcript: crate::app::transcript::LoadedTranscript,
        pid: u32,
        cwd: std::path::PathBuf,
    ) -> Self {
        let session = session_from_meta(header.meta.clone(), pid, cwd);
        let live_session = smelt_core::session_runtime::LiveSession::from_store(header, store_ref);
        Self {
            session,
            transcript,
            live_session: Some(live_session),
            store_head: Some(store_head),
        }
    }

    fn append_history_item_with_transcript_block(
        session: &mut Session,
        live_session: Option<&mut LiveSession>,
        transcript: &mut TranscriptDocument,
        item: HistoryItem,
        block: Option<Block>,
        first_user_message: Option<String>,
    ) -> MutationResult {
        let before_generation = transcript.history().descriptor_dirty_generation();
        let before_dirty_from = transcript.history().descriptor_dirty_from();
        let idx = live_session
            .as_ref()
            .map_or(session.history.len(), |live| live.history_len());
        if let Some(message) = first_user_message {
            if session.first_user_message.is_none() {
                session.first_user_message = Some(message);
                session.snapshot_metadata_at(idx + 1);
            }
        }
        if let Some(block) = block {
            transcript.push_with_origin(block, BlockOrigin::History(idx));
        }
        if let Some(live_session) = live_session {
            let appended_idx = live_session.append_history(item);
            debug_assert_eq!(appended_idx, idx);
        } else {
            session.history.push(item);
        }
        let history = transcript.history();
        MutationResult {
            session_dirty: true,
            transcript_dirty: history.descriptor_dirty_generation() != before_generation
                || history.descriptor_dirty_from() != before_dirty_from,
            applied: true,
            history_idx: Some(idx),
            history_dirty_from: Some(idx),
            ..Default::default()
        }
    }

    fn apply_to_session_and_transcript(
        session: &mut Session,
        transcript: &mut TranscriptDocument,
        mutation: SessionMutation,
    ) -> MutationResult {
        match mutation {
            SessionMutation::CommitRequestHistoryItem {
                item,
                block,
                first_user_message,
            } => Self::append_history_item_with_transcript_block(
                session,
                None,
                transcript,
                item,
                block,
                first_user_message,
            ),
            mutation => Self::apply_to_session(session, mutation),
        }
    }

    fn apply_to_live_session_and_transcript(
        session: &mut Session,
        live_session: &mut LiveSession,
        transcript: &mut TranscriptDocument,
        mutation: SessionMutation,
    ) -> MutationResult {
        match mutation {
            SessionMutation::CommitRequestHistoryItem {
                item,
                block,
                first_user_message,
            } => Self::append_history_item_with_transcript_block(
                session,
                Some(live_session),
                transcript,
                item,
                block,
                first_user_message,
            ),
            mutation => Self::apply_to_live_session(session, live_session, mutation),
        }
    }

    fn apply_to_session(session: &mut Session, mutation: SessionMutation) -> MutationResult {
        let before_rewrite = matches!(
            &mutation,
            SessionMutation::TruncateHistoryFrom { .. } | SessionMutation::RewindHistoryTo { .. }
        )
        .then(|| session.clone());
        let mut result = match mutation {
            SessionMutation::AppendHistoryItem { item } => {
                let idx = session.history.len();
                session.history.push(item);
                MutationResult {
                    session_dirty: true,
                    history_idx: Some(idx),
                    history_dirty_from: Some(idx),
                    ..Default::default()
                }
            }
            SessionMutation::CommitRequestHistoryItem { .. } => {
                unreachable!("history-with-transcript mutation requires a transcript target")
            }
            SessionMutation::ApplyHistoryAppend { append, identity } => {
                let old_len = session.history.len();
                let append_result = protocol::apply_history_append(&mut session.history, &append);
                let dirty_from = match append_result {
                    HistoryAppendResult::Unchanged => None,
                    HistoryAppendResult::Pushed => Some(old_len),
                    HistoryAppendResult::ReplacedLast | HistoryAppendResult::RemovedLast => {
                        Some(old_len.saturating_sub(1))
                    }
                };
                let turn_meta = if append_result == HistoryAppendResult::RemovedLast {
                    let turn_meta = session.prune_rewindable_snapshots(session.history.len());
                    session.clear_context_tokens_baseline_if_mismatched(&identity);
                    turn_meta
                } else {
                    None
                };
                MutationResult {
                    session_dirty: dirty_from.is_some(),
                    history_dirty_from: dirty_from,
                    history_append_result: Some(append_result),
                    turn_meta,
                    ..Default::default()
                }
            }
            SessionMutation::TruncateHistoryFrom { index, identity } => {
                let index = index.min(session.history.len());
                session.history.truncate(index);
                let turn_meta = session.prune_rewindable_snapshots(index);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                MutationResult {
                    session_dirty: true,
                    history_dirty_from: Some(index),
                    turn_meta,
                    ..Default::default()
                }
            }
            SessionMutation::RewindHistoryTo {
                index,
                keep_checkpoint_at_boundary,
                identity,
            } => {
                let index = index.min(session.history.len());
                session.history.truncate(index);
                let turn_meta = session
                    .restore_rewindable_snapshots_after_rewind(index, keep_checkpoint_at_boundary);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                MutationResult {
                    session_dirty: true,
                    history_dirty_from: Some(index),
                    turn_meta,
                    ..Default::default()
                }
            }
            SessionMutation::AppendTranscriptBlock { .. }
            | SessionMutation::InsertCheckpointMarker { .. }
            | SessionMutation::RemoveUnoriginatedTranscriptBlockAt { .. }
            | SessionMutation::ReplaceTranscriptFromHistory { .. }
            | SessionMutation::TruncateTranscriptTo { .. }
            | SessionMutation::ClearTranscript
            | SessionMutation::UpdateCompactionPreview { .. }
            | SessionMutation::ClearCompactionPreview
            | SessionMutation::RewriteTranscriptBlock { .. }
            | SessionMutation::AppendStreamingThinking { .. }
            | SessionMutation::FlushStreamingThinking
            | SessionMutation::AppendStreamingText { .. }
            | SessionMutation::FlushStreamingText
            | SessionMutation::SyncActiveToolElapsed { .. }
            | SessionMutation::StartTool { .. }
            | SessionMutation::AppendActiveToolOutput { .. }
            | SessionMutation::SetActiveToolStatus { .. }
            | SessionMutation::SetActiveToolUserMessage { .. }
            | SessionMutation::FinishTool { .. }
            | SessionMutation::FinalizeActiveTools
            | SessionMutation::PromoteToolDraft { .. }
            | SessionMutation::ClearToolDrafts
            | SessionMutation::UpsertToolDraft { .. }
            | SessionMutation::StartExec { .. }
            | SessionMutation::AppendExecOutput { .. }
            | SessionMutation::FinishExec { .. }
            | SessionMutation::FinalizeExec => MutationResult::default(),
            SessionMutation::RecordTokenUsage { usage, identity } => {
                let Some(tokens) = usage.context_tokens.or(usage.prompt_tokens) else {
                    return MutationResult::default();
                };
                if tokens == 0 {
                    return MutationResult::default();
                }
                let history_len = session.history.len();
                let changed = session.context_tokens != Some(tokens)
                    || session.context_tokens_history_len != Some(history_len)
                    || session.context_token_identity.as_ref() != Some(&identity)
                    || session.display_context_tokens != Some(tokens)
                    || session.display_context_token_identity.as_ref() != Some(&identity);
                session.record_context_tokens(tokens, identity);
                MutationResult {
                    session_dirty: changed,
                    context_tokens_updated: changed,
                    ..Default::default()
                }
            }
            SessionMutation::ClearContextTokensBaselineIfMismatched { identity } => {
                let changed = session
                    .context_token_identity
                    .as_ref()
                    .is_some_and(|current| current != &identity);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                MutationResult {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            SessionMutation::AccumulateUsage { usage, cost_usd } => {
                let before_cost = session.session_cost_usd;
                let before_usage = session.session_usage.clone();
                session.session_cost_usd += cost_usd;
                session.session_usage.accumulate(&usage);
                MutationResult {
                    session_dirty: session.session_cost_usd != before_cost
                        || session.session_usage != before_usage,
                    ..Default::default()
                }
            }
            SessionMutation::SetTitle {
                title,
                slug,
                snapshot_history_len,
            } => {
                let before_title = session.title.clone();
                let before_slug = session.slug.clone();
                let before_snapshots = session.metadata_snapshots.clone();
                session.title = Some(title);
                session.slug = Some(slug);
                session.snapshot_metadata_at(snapshot_history_len);
                MutationResult {
                    session_dirty: session.title != before_title
                        || session.slug != before_slug
                        || session.metadata_snapshots != before_snapshots,
                    ..Default::default()
                }
            }
            SessionMutation::UpdateRuntimeMetadata {
                updated_at_ms,
                mode,
                reasoning_effort,
                model,
                fast_mode,
            } => {
                let changed = session.updated_at_ms != updated_at_ms
                    || session.mode.as_deref() != Some(mode.as_str())
                    || session.reasoning_effort.as_ref() != Some(&reasoning_effort)
                    || session.model != model
                    || session.fast_mode != Some(fast_mode);
                session.updated_at_ms = updated_at_ms;
                session.mode = Some(mode);
                session.reasoning_effort = Some(reasoning_effort);
                session.model = model;
                session.fast_mode = Some(fast_mode);
                MutationResult {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            SessionMutation::SetFastMode { enabled } => {
                let changed = session.fast_mode != Some(enabled);
                session.fast_mode = Some(enabled);
                MutationResult {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            SessionMutation::SetCwd { cwd } => {
                let changed = session.cwd.as_deref() != Some(cwd.as_str());
                session.cwd = Some(cwd);
                MutationResult {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            SessionMutation::FinishTurnState {
                history_len,
                meta,
                snapshot_context,
                update_context_token_history_len,
            } => {
                let before_turn_metas = session.turn_metas.clone();
                let before_context_snapshots = session.context_snapshots.clone();
                let before_context_tokens_history_len = session.context_tokens_history_len;
                session.finish_turn_state(
                    history_len,
                    meta,
                    snapshot_context,
                    update_context_token_history_len,
                );
                let changed = session.turn_metas != before_turn_metas
                    || session.context_snapshots != before_context_snapshots
                    || session.context_tokens_history_len != before_context_tokens_history_len;
                MutationResult {
                    session_dirty: changed,
                    applied: changed,
                    ..Default::default()
                }
            }
            SessionMutation::InstallContextCheckpoint {
                kind,
                summary,
                first_live_message_index,
                tokens_before,
            } => {
                let installed = session.install_context_checkpoint(
                    kind,
                    summary,
                    first_live_message_index,
                    tokens_before,
                );
                MutationResult {
                    session_dirty: installed,
                    applied: installed,
                    ..Default::default()
                }
            }
            SessionMutation::InstallContextCheckpointAtHistoryIndex {
                kind,
                summary,
                first_live_index,
                tokens_before,
                history_len,
            } => {
                let installed = session.install_context_checkpoint_at_history_index(
                    kind,
                    summary,
                    first_live_index,
                    tokens_before,
                    history_len,
                );
                MutationResult {
                    session_dirty: installed,
                    applied: installed,
                    ..Default::default()
                }
            }
            SessionMutation::SetCheckpoint { checkpoint } => {
                if session.checkpoint == checkpoint {
                    return MutationResult::default();
                }
                session.checkpoint = checkpoint;
                MutationResult {
                    session_dirty: true,
                    ..Default::default()
                }
            }
            SessionMutation::SetCheckpointTokensAfterEstimate {
                tokens,
                history_len,
            } => {
                let changed = session.record_checkpoint_tokens_after_estimate(tokens, history_len);
                MutationResult {
                    session_dirty: changed,
                    applied: changed,
                    ..Default::default()
                }
            }
            SessionMutation::RestoreMetadataAfterRewind { history_len } => {
                let before_title = session.title.clone();
                let before_slug = session.slug.clone();
                let before_first_user_message = session.first_user_message.clone();
                let before_snapshots = session.metadata_snapshots.clone();
                session.restore_metadata_after_rewind(history_len);
                MutationResult {
                    session_dirty: session.title != before_title
                        || session.slug != before_slug
                        || session.first_user_message != before_first_user_message
                        || session.metadata_snapshots != before_snapshots,
                    ..Default::default()
                }
            }
            SessionMutation::PruneRewindableState {
                history_len,
                identity,
            } => {
                let before_turn_metas = session.turn_metas.len();
                let before_context_snapshots = session.context_snapshots.len();
                let before_metadata_snapshots = session.metadata_snapshots.len();
                let before_context_tokens = session.context_tokens;
                let before_context_token_identity = session.context_token_identity.clone();
                let turn_meta = session.prune_rewindable_snapshots(history_len);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                let changed = session.turn_metas.len() != before_turn_metas
                    || session.context_snapshots.len() != before_context_snapshots
                    || session.metadata_snapshots.len() != before_metadata_snapshots
                    || session.context_tokens != before_context_tokens
                    || session.context_token_identity != before_context_token_identity;
                MutationResult {
                    session_dirty: changed,
                    turn_meta,
                    ..Default::default()
                }
            }
        };
        if before_rewrite
            .as_ref()
            .is_some_and(|before| before == session)
        {
            result.session_dirty = false;
            result.history_dirty_from = None;
        }
        result
    }

    fn apply_to_live_session(
        session: &mut Session,
        live_session: &mut LiveSession,
        mutation: SessionMutation,
    ) -> MutationResult {
        let before_rewrite = matches!(
            &mutation,
            SessionMutation::TruncateHistoryFrom { .. } | SessionMutation::RewindHistoryTo { .. }
        )
        .then(|| {
            (
                session.clone(),
                live_session.live_start,
                live_session.live_history.len(),
            )
        });
        let mut result = match mutation {
            SessionMutation::AppendHistoryItem { item } => {
                let idx = live_session.append_history(item);
                MutationResult {
                    session_dirty: true,
                    history_idx: Some(idx),
                    history_dirty_from: Some(idx),
                    ..Default::default()
                }
            }
            SessionMutation::CommitRequestHistoryItem { .. } => {
                unreachable!("history-with-transcript mutation requires a transcript target")
            }
            SessionMutation::TruncateHistoryFrom { index, identity } => {
                let dirty_from = index.min(live_session.history_len());
                live_session.truncate_from(dirty_from);
                let turn_meta = session.prune_rewindable_snapshots(dirty_from);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                MutationResult {
                    session_dirty: true,
                    history_dirty_from: Some(dirty_from),
                    turn_meta,
                    ..Default::default()
                }
            }
            SessionMutation::RewindHistoryTo {
                index,
                keep_checkpoint_at_boundary,
                identity,
            } => {
                let dirty_from = index.min(live_session.history_len());
                live_session.truncate_from(dirty_from);
                let turn_meta = session.restore_rewindable_snapshots_after_rewind(
                    dirty_from,
                    keep_checkpoint_at_boundary,
                );
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                MutationResult {
                    session_dirty: true,
                    history_dirty_from: Some(dirty_from),
                    turn_meta,
                    ..Default::default()
                }
            }
            SessionMutation::SetCheckpoint { checkpoint } => {
                if session.checkpoint == checkpoint {
                    return MutationResult::default();
                }
                session.checkpoint = checkpoint;
                MutationResult {
                    session_dirty: true,
                    ..Default::default()
                }
            }
            SessionMutation::SetCheckpointTokensAfterEstimate {
                tokens,
                history_len,
            } => {
                let changed = session.record_checkpoint_tokens_after_estimate(tokens, history_len);
                MutationResult {
                    session_dirty: changed,
                    applied: changed,
                    ..Default::default()
                }
            }
            SessionMutation::AppendTranscriptBlock { .. }
            | SessionMutation::InsertCheckpointMarker { .. }
            | SessionMutation::RemoveUnoriginatedTranscriptBlockAt { .. }
            | SessionMutation::ReplaceTranscriptFromHistory { .. }
            | SessionMutation::TruncateTranscriptTo { .. }
            | SessionMutation::ClearTranscript
            | SessionMutation::UpdateCompactionPreview { .. }
            | SessionMutation::ClearCompactionPreview
            | SessionMutation::RewriteTranscriptBlock { .. }
            | SessionMutation::AppendStreamingThinking { .. }
            | SessionMutation::FlushStreamingThinking
            | SessionMutation::AppendStreamingText { .. }
            | SessionMutation::FlushStreamingText
            | SessionMutation::SyncActiveToolElapsed { .. }
            | SessionMutation::StartTool { .. }
            | SessionMutation::AppendActiveToolOutput { .. }
            | SessionMutation::SetActiveToolStatus { .. }
            | SessionMutation::SetActiveToolUserMessage { .. }
            | SessionMutation::FinishTool { .. }
            | SessionMutation::FinalizeActiveTools
            | SessionMutation::PromoteToolDraft { .. }
            | SessionMutation::ClearToolDrafts
            | SessionMutation::UpsertToolDraft { .. }
            | SessionMutation::StartExec { .. }
            | SessionMutation::AppendExecOutput { .. }
            | SessionMutation::FinishExec { .. }
            | SessionMutation::FinalizeExec
            | SessionMutation::ApplyHistoryAppend { .. }
            | SessionMutation::RecordTokenUsage { .. }
            | SessionMutation::ClearContextTokensBaselineIfMismatched { .. }
            | SessionMutation::AccumulateUsage { .. }
            | SessionMutation::SetTitle { .. }
            | SessionMutation::UpdateRuntimeMetadata { .. }
            | SessionMutation::SetFastMode { .. }
            | SessionMutation::SetCwd { .. }
            | SessionMutation::FinishTurnState { .. }
            | SessionMutation::InstallContextCheckpoint { .. }
            | SessionMutation::InstallContextCheckpointAtHistoryIndex { .. }
            | SessionMutation::RestoreMetadataAfterRewind { .. }
            | SessionMutation::PruneRewindableState { .. } => MutationResult::default(),
        };
        if before_rewrite.as_ref().is_some_and(
            |(before_session, before_live_start, before_live_len)| {
                before_session == session
                    && *before_live_start == live_session.live_start
                    && *before_live_len == live_session.live_history.len()
            },
        ) {
            result.session_dirty = false;
            result.history_dirty_from = None;
        }
        result
    }

    fn apply_to_stream_parser_transcript(
        parser: &mut StreamParser,
        transcript: &mut TranscriptDocument,
        mutation: SessionMutation,
    ) -> MutationResult {
        let before_generation = transcript.history().descriptor_dirty_generation();
        let before_dirty_from = transcript.history().descriptor_dirty_from();
        let mut applied = false;
        match mutation {
            SessionMutation::AppendStreamingThinking { delta } => {
                parser.append_streaming_thinking(transcript.history_mut(), &delta);
            }
            SessionMutation::FlushStreamingThinking => {
                parser.flush_streaming_thinking(transcript.history_mut());
            }
            SessionMutation::AppendStreamingText { delta } => {
                parser.append_streaming_text(transcript.history_mut(), &delta);
            }
            SessionMutation::FlushStreamingText => {
                parser.flush_streaming_text(transcript.history_mut());
            }
            SessionMutation::SyncActiveToolElapsed { now } => {
                parser.sync_active_tool_elapsed_at(transcript.history_mut(), now);
            }
            SessionMutation::StartTool {
                call_id,
                name,
                summary,
                args,
                now,
            } => {
                parser.start_tool(transcript.history_mut(), call_id, name, summary, args, now);
            }
            SessionMutation::AppendActiveToolOutput { call_id, chunk } => {
                parser.append_active_output(transcript.history_mut(), &call_id, &chunk);
            }
            SessionMutation::SetActiveToolStatus {
                call_id,
                status,
                now,
            } => {
                parser.set_active_status(transcript.history_mut(), &call_id, status, now);
            }
            SessionMutation::SetActiveToolUserMessage { call_id, message } => {
                parser.set_active_user_message(transcript.history_mut(), &call_id, message);
            }
            SessionMutation::FinishTool {
                call_id,
                status,
                output,
                engine_elapsed,
                now,
            } => {
                parser.finish_tool(
                    transcript.history_mut(),
                    &call_id,
                    status,
                    output,
                    engine_elapsed,
                    now,
                );
            }
            SessionMutation::FinalizeActiveTools => {
                parser.finalize_active_tools(transcript.history_mut());
            }
            SessionMutation::PromoteToolDraft {
                stream_id,
                start,
                now,
            } => {
                applied = parser.promote_tool_draft(
                    transcript.history_mut(),
                    stream_id.as_deref(),
                    start,
                    now,
                );
            }
            SessionMutation::ClearToolDrafts => {
                parser.clear_tool_drafts(transcript.history_mut());
            }
            SessionMutation::UpsertToolDraft { update } => {
                parser.upsert_tool_draft(transcript.history_mut(), update);
            }
            SessionMutation::StartExec { command } => {
                parser.start_exec(transcript.history_mut(), command);
            }
            SessionMutation::AppendExecOutput { chunk } => {
                parser.append_exec_output(transcript.history_mut(), &chunk);
            }
            SessionMutation::FinishExec { exit_code } => {
                parser.finish_exec(exit_code);
            }
            SessionMutation::FinalizeExec => {
                parser.finalize_exec(transcript.history_mut());
            }
            SessionMutation::AppendHistoryItem { .. }
            | SessionMutation::CommitRequestHistoryItem { .. }
            | SessionMutation::ApplyHistoryAppend { .. }
            | SessionMutation::TruncateHistoryFrom { .. }
            | SessionMutation::RewindHistoryTo { .. }
            | SessionMutation::AppendTranscriptBlock { .. }
            | SessionMutation::InsertCheckpointMarker { .. }
            | SessionMutation::RemoveUnoriginatedTranscriptBlockAt { .. }
            | SessionMutation::ReplaceTranscriptFromHistory { .. }
            | SessionMutation::TruncateTranscriptTo { .. }
            | SessionMutation::ClearTranscript
            | SessionMutation::UpdateCompactionPreview { .. }
            | SessionMutation::ClearCompactionPreview
            | SessionMutation::RewriteTranscriptBlock { .. }
            | SessionMutation::RecordTokenUsage { .. }
            | SessionMutation::ClearContextTokensBaselineIfMismatched { .. }
            | SessionMutation::AccumulateUsage { .. }
            | SessionMutation::SetTitle { .. }
            | SessionMutation::UpdateRuntimeMetadata { .. }
            | SessionMutation::SetFastMode { .. }
            | SessionMutation::SetCwd { .. }
            | SessionMutation::FinishTurnState { .. }
            | SessionMutation::InstallContextCheckpoint { .. }
            | SessionMutation::InstallContextCheckpointAtHistoryIndex { .. }
            | SessionMutation::SetCheckpoint { .. }
            | SessionMutation::SetCheckpointTokensAfterEstimate { .. }
            | SessionMutation::RestoreMetadataAfterRewind { .. }
            | SessionMutation::PruneRewindableState { .. } => {}
        }
        let history = transcript.history();
        let transcript_dirty = history.descriptor_dirty_generation() != before_generation
            || history.descriptor_dirty_from() != before_dirty_from;
        MutationResult {
            transcript_dirty,
            descriptors_unpersisted: transcript_dirty,
            applied,
            ..Default::default()
        }
    }

    fn apply_to_transcript(
        transcript: &mut TranscriptDocument,
        mutation: SessionMutation,
    ) -> MutationResult {
        let before_generation = transcript.history().descriptor_dirty_generation();
        let before_dirty_from = transcript.history().descriptor_dirty_from();
        let mut applied = false;
        let mut descriptors_unpersisted = false;
        let mut block_id = None;
        match mutation {
            SessionMutation::AppendTranscriptBlock { block } => {
                transcript.push(block);
                applied = transcript.history().descriptor_dirty_generation() != before_generation
                    || transcript.history().descriptor_dirty_from() != before_dirty_from;
            }
            SessionMutation::InsertCheckpointMarker { block_index, block } => {
                transcript.insert_checkpoint_marker_at(block_index, block);
                applied = transcript.history().descriptor_dirty_generation() != before_generation
                    || transcript.history().descriptor_dirty_from() != before_dirty_from;
            }
            SessionMutation::RemoveUnoriginatedTranscriptBlockAt { block_index } => {
                applied = transcript.remove_unoriginated_at(block_index).is_some();
            }
            SessionMutation::ReplaceTranscriptFromHistory {
                transcript: rebuilt,
            } => {
                transcript.replace_transcript(rebuilt);
                transcript.history_mut().mark_changed();
                applied = true;
                descriptors_unpersisted = true;
            }
            SessionMutation::TruncateTranscriptTo { block_index } => {
                transcript.truncate_to(block_index);
                applied = transcript.history().descriptor_dirty_generation() != before_generation
                    || transcript.history().descriptor_dirty_from() != before_dirty_from;
            }
            SessionMutation::ClearTranscript => {
                transcript.history_mut().clear();
                applied = transcript.history().descriptor_dirty_generation() != before_generation
                    || transcript.history().descriptor_dirty_from() != before_dirty_from;
            }
            SessionMutation::UpdateCompactionPreview { summary } => {
                block_id = transcript.set_compaction_preview(summary);
                applied = block_id.is_some();
            }
            SessionMutation::ClearCompactionPreview => {
                block_id = transcript.clear_compaction_preview();
                applied = block_id.is_some();
            }
            SessionMutation::RewriteTranscriptBlock { id, block } => {
                transcript.history_mut().rewrite(id, block);
                applied = true;
            }
            SessionMutation::AppendHistoryItem { .. }
            | SessionMutation::CommitRequestHistoryItem { .. }
            | SessionMutation::ApplyHistoryAppend { .. }
            | SessionMutation::TruncateHistoryFrom { .. }
            | SessionMutation::RewindHistoryTo { .. }
            | SessionMutation::AppendStreamingThinking { .. }
            | SessionMutation::FlushStreamingThinking
            | SessionMutation::AppendStreamingText { .. }
            | SessionMutation::FlushStreamingText
            | SessionMutation::SyncActiveToolElapsed { .. }
            | SessionMutation::StartTool { .. }
            | SessionMutation::AppendActiveToolOutput { .. }
            | SessionMutation::SetActiveToolStatus { .. }
            | SessionMutation::SetActiveToolUserMessage { .. }
            | SessionMutation::FinishTool { .. }
            | SessionMutation::FinalizeActiveTools
            | SessionMutation::PromoteToolDraft { .. }
            | SessionMutation::ClearToolDrafts
            | SessionMutation::UpsertToolDraft { .. }
            | SessionMutation::StartExec { .. }
            | SessionMutation::AppendExecOutput { .. }
            | SessionMutation::FinishExec { .. }
            | SessionMutation::FinalizeExec
            | SessionMutation::RecordTokenUsage { .. }
            | SessionMutation::ClearContextTokensBaselineIfMismatched { .. }
            | SessionMutation::AccumulateUsage { .. }
            | SessionMutation::SetTitle { .. }
            | SessionMutation::UpdateRuntimeMetadata { .. }
            | SessionMutation::SetFastMode { .. }
            | SessionMutation::SetCwd { .. }
            | SessionMutation::FinishTurnState { .. }
            | SessionMutation::InstallContextCheckpoint { .. }
            | SessionMutation::InstallContextCheckpointAtHistoryIndex { .. }
            | SessionMutation::SetCheckpoint { .. }
            | SessionMutation::SetCheckpointTokensAfterEstimate { .. }
            | SessionMutation::RestoreMetadataAfterRewind { .. }
            | SessionMutation::PruneRewindableState { .. } => {}
        }
        let history = transcript.history();
        MutationResult {
            transcript_dirty: history.descriptor_dirty_generation() != before_generation
                || history.descriptor_dirty_from() != before_dirty_from,
            descriptors_unpersisted,
            applied,
            block_id,
            ..Default::default()
        }
    }

    pub(crate) fn into_full(self) -> FullSessionDocument {
        FullSessionDocument::new(self.session, self.transcript)
    }

    pub(crate) fn into_store_backed(self) -> StoreBackedSessionDocument {
        StoreBackedSessionDocument::new(
            self.session,
            self.transcript,
            self.live_session
                .expect("store-backed session document includes live session state"),
            self.store_head
                .expect("store-backed session document includes a store head"),
        )
    }
}

fn descriptor_save_plan(
    transcript: &TranscriptDocument,
    descriptors_persisted: bool,
    dirty_history_from: Option<usize>,
) -> Option<SessionDescriptorSavePlan> {
    match transcript.descriptor_save_suffix(descriptors_persisted, dirty_history_from) {
        crate::app::transcript::TranscriptDescriptorSaveSuffix::Unchanged => None,
        crate::app::transcript::TranscriptDescriptorSaveSuffix::Suffix {
            descriptor_start_idx,
            descriptor_records,
        } => Some(SessionDescriptorSavePlan {
            start_descriptor_idx: descriptor_start_idx,
            records: descriptor_records,
        }),
    }
}

fn transcript_descriptor_row(
    descriptor_idx: usize,
    record: &smelt_core::TranscriptBlockRecordWithId,
    history: &smelt_store::HistorySuffix,
) -> Result<smelt_store::TranscriptDescriptorRecord, smelt_store::StoreError> {
    let owned_record;
    let record_ref = match record.record.origin {
        Some(BlockOrigin::History(index))
            if !history_suffix_contains_matching_descriptor_origin(history, index, record) =>
        {
            owned_record = smelt_core::TranscriptBlockRecord {
                origin: None,
                ..record.record.clone()
            };
            &owned_record
        }
        _ => &record.record,
    };
    smelt_core::transcript_model::transcript_descriptor_row_with_block_idx(
        descriptor_idx,
        record.block_id.get(),
        record_ref,
    )
}

fn history_suffix_contains_matching_descriptor_origin(
    history: &smelt_store::HistorySuffix,
    history_idx: usize,
    record: &smelt_core::TranscriptBlockRecordWithId,
) -> bool {
    let history_start = history
        .start
        .as_usize()
        .expect("prepared history start originated as usize");
    let history_len = history
        .final_len
        .as_usize()
        .expect("prepared history length originated as usize");
    if history_idx >= history_len {
        return false;
    }
    if history_idx < history_start {
        return true;
    }
    history
        .items
        .get(history_idx - history_start)
        .is_some_and(|item| {
            protocol::transcript_descriptor_kind_matches_history_item(
                record.record.descriptor.kind(),
                item,
            )
        })
}

fn session_from_meta(meta: SessionMeta, pid: u32, cwd: std::path::PathBuf) -> Session {
    let mut session = Session::new(pid, cwd);
    let context_token_identity = meta.context_token_identity.clone();
    session.id = meta.id;
    session.title = meta.title;
    session.slug = meta.slug;
    session.first_user_message = meta.first_user_message;
    session.created_at_ms = meta.created_at_ms;
    session.updated_at_ms = meta.updated_at_ms;
    session.mode = meta.mode;
    session.reasoning_effort = meta.reasoning_effort;
    session.model = meta.model;
    session.fast_mode = meta.fast_mode;
    session.cwd = meta.cwd;
    session.parent_id = meta.parent_id;
    session.checkpoint = meta.checkpoint;
    session.display_context_tokens = meta.context_tokens;
    session.context_token_identity = context_token_identity.clone();
    session.display_context_token_identity = meta
        .display_context_token_identity
        .or(context_token_identity);
    session
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Content, ReasoningEffort};
    use smelt_core::session::ContextTokenIdentity;

    fn token_identity() -> ContextTokenIdentity {
        ContextTokenIdentity {
            model: Some("model-a".into()),
            api_base: Some("https://api.example.test".into()),
            provider_type: Some("openai".into()),
        }
    }

    fn meta_with_token_identity() -> SessionMeta {
        let identity = token_identity();
        SessionMeta {
            id: "session-a".into(),
            title: Some("Title".into()),
            slug: Some("title".into()),
            first_user_message: Some("hello".into()),
            created_at_ms: 10,
            updated_at_ms: 20,
            mode: Some("agent".into()),
            reasoning_effort: Some(ReasoningEffort::Low),
            model: Some("model-a".into()),
            fast_mode: Some(true),
            cwd: Some("/tmp".into()),
            parent_id: Some("parent".into()),
            context_tokens: Some(42),
            context_token_identity: Some(identity),
            display_context_token_identity: None,
            history_len: Some(3),
            checkpoint: None,
            text_bytes: Some(128),
        }
    }

    fn empty_live_session_for(session: &Session, history_len: usize) -> LiveSession {
        let mut meta = meta_with_token_identity();
        meta.id = session.id.clone();
        meta.history_len = Some(history_len);
        let header = SessionHeader {
            meta,
            history_len,
            revision: 0,
            degraded_warnings: Vec::new(),
        };
        LiveSession::from_parts(header, std::path::PathBuf::new(), None)
    }

    fn apply_session(session: &mut Session, mutation: SessionMutation) -> MutationResult {
        SessionDocument::apply(SessionMutationTarget::Session { session }, mutation)
    }

    fn apply_transcript(
        transcript: &mut TranscriptDocument,
        mutation: SessionMutation,
    ) -> MutationResult {
        SessionDocument::apply(SessionMutationTarget::Transcript { transcript }, mutation)
    }

    fn apply_session_and_transcript(
        session: &mut Session,
        transcript: &mut TranscriptDocument,
        mutation: SessionMutation,
    ) -> MutationResult {
        SessionDocument::apply(
            SessionMutationTarget::SessionAndTranscript {
                session,
                transcript,
            },
            mutation,
        )
    }

    fn apply_stream_parser_transcript(
        parser: &mut StreamParser,
        transcript: &mut TranscriptDocument,
        mutation: SessionMutation,
    ) -> MutationResult {
        SessionDocument::apply(
            SessionMutationTarget::StreamParserTranscript { parser, transcript },
            mutation,
        )
    }

    #[test]
    fn session_from_meta_restores_display_token_identity_from_context_identity() {
        let session = session_from_meta(
            meta_with_token_identity(),
            1,
            std::path::PathBuf::from("/tmp"),
        );

        assert_eq!(session.id, "session-a");
        assert_eq!(session.display_context_tokens(), Some(42));
        assert_eq!(
            session.display_context_token_identity,
            session.context_token_identity
        );
    }

    #[test]
    fn full_session_load_constructs_document_before_install_projection() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.id = "full-document".into();
        session
            .history
            .push(HistoryItem::user(Content::text("hello")));
        let transcript = crate::app::transcript::LoadedTranscript::full(
            smelt_core::content::transcript::Transcript::new(),
        );

        let projection = SessionDocument::from_full_session(session, transcript).into_full();

        assert_eq!(projection.session.id, "full-document");
        assert_eq!(projection.session.history.len(), 1);
    }

    #[test]
    fn record_token_usage_mutation_updates_accounting_and_reports_dirty() {
        let identity = token_identity();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_session(
            &mut session,
            SessionMutation::RecordTokenUsage {
                usage: TokenUsage {
                    context_tokens: Some(777),
                    ..Default::default()
                },
                identity: identity.clone(),
            },
        );

        assert!(result.session_dirty);
        assert!(result.context_tokens_updated);
        assert_eq!(session.display_context_tokens(), Some(777));
        assert_eq!(session.context_token_identity, Some(identity.clone()));
        assert_eq!(session.display_context_token_identity, Some(identity));
    }

    #[test]
    fn zero_token_usage_mutation_is_noop() {
        let identity = token_identity();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_session(
            &mut session,
            SessionMutation::RecordTokenUsage {
                usage: TokenUsage {
                    context_tokens: Some(0),
                    ..Default::default()
                },
                identity,
            },
        );

        assert!(!result.session_dirty);
        assert!(!result.context_tokens_updated);
        assert_eq!(session.display_context_tokens(), None);
    }

    #[test]
    fn clear_context_tokens_baseline_mutation_reports_dirty_on_identity_mismatch() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.record_context_tokens(100, token_identity());
        let mut new_identity = token_identity();
        new_identity.model = Some("model-b".into());

        let result = apply_session(
            &mut session,
            SessionMutation::ClearContextTokensBaselineIfMismatched {
                identity: new_identity,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.context_tokens, None);
        assert_eq!(session.context_token_identity, None);
    }

    #[test]
    fn accumulate_usage_mutation_updates_accounting_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_session(
            &mut session,
            SessionMutation::AccumulateUsage {
                usage: TokenUsage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    ..Default::default()
                },
                cost_usd: 0.25,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.session_cost_usd, 0.25);
        assert_eq!(session.session_usage.prompt_tokens, Some(10));
        assert_eq!(session.session_usage.completion_tokens, Some(5));
    }

    #[test]
    fn commit_request_history_item_sets_first_user_snapshot_once() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut transcript = TranscriptDocument::new();

        let result = apply_session_and_transcript(
            &mut session,
            &mut transcript,
            SessionMutation::CommitRequestHistoryItem {
                item: HistoryItem::user(Content::text("hello")),
                block: None,
                first_user_message: Some("hello".into()),
            },
        );
        let second = apply_session_and_transcript(
            &mut session,
            &mut transcript,
            SessionMutation::CommitRequestHistoryItem {
                item: HistoryItem::user(Content::text("second")),
                block: None,
                first_user_message: Some("ignored".into()),
            },
        );

        assert!(result.session_dirty);
        assert!(second.session_dirty);
        assert_eq!(session.first_user_message.as_deref(), Some("hello"));
        assert_eq!(
            session
                .metadata_snapshots
                .iter()
                .filter(|(idx, _)| *idx == 1)
                .count(),
            1
        );
    }

    #[test]
    fn title_mutation_updates_metadata_snapshot_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_session(
            &mut session,
            SessionMutation::SetTitle {
                title: "New title".into(),
                slug: "new-title".into(),
                snapshot_history_len: 2,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.title.as_deref(), Some("New title"));
        assert_eq!(session.slug.as_deref(), Some("new-title"));
        assert!(session.metadata_snapshots.iter().any(|(idx, _)| *idx == 2));
    }

    #[test]
    fn restore_metadata_after_rewind_mutation_restores_snapshot() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        apply_session(
            &mut session,
            SessionMutation::SetTitle {
                title: "First".into(),
                slug: "first".into(),
                snapshot_history_len: 1,
            },
        );
        apply_session(
            &mut session,
            SessionMutation::SetTitle {
                title: "Second".into(),
                slug: "second".into(),
                snapshot_history_len: 2,
            },
        );

        let result = apply_session(
            &mut session,
            SessionMutation::RestoreMetadataAfterRewind { history_len: 1 },
        );

        assert!(result.session_dirty);
        assert_eq!(session.title.as_deref(), Some("First"));
        assert_eq!(session.slug.as_deref(), Some("first"));
    }

    #[test]
    fn rewind_to_start_mutation_clears_history_snapshots_and_tokens() {
        let identity = token_identity();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::user(Content::text("prompt")));
        session.record_context_tokens(500, identity.clone());
        session.snapshot_context_at(1);
        apply_session(
            &mut session,
            SessionMutation::SetTitle {
                title: "Title".into(),
                slug: "title".into(),
                snapshot_history_len: 1,
            },
        );
        session.turn_metas.push((
            1,
            TurnMeta {
                elapsed_ms: 10,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
            },
        ));

        let result = apply_session(
            &mut session,
            SessionMutation::RewindHistoryTo {
                index: 0,
                keep_checkpoint_at_boundary: false,
                identity,
            },
        );

        assert!(result.session_dirty);
        assert!(session.history.is_empty());
        assert!(session.turn_metas.is_empty());
        assert!(session.context_snapshots.is_empty());
        assert_eq!(session.current_context_tokens(), None);
        assert_eq!(session.title, None);
        assert_eq!(session.slug, None);
    }

    #[test]
    fn runtime_metadata_mutation_updates_save_metadata_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_session(
            &mut session,
            SessionMutation::UpdateRuntimeMetadata {
                updated_at_ms: 42,
                mode: "agent".into(),
                reasoning_effort: ReasoningEffort::High,
                model: Some("provider/model".into()),
                fast_mode: true,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.updated_at_ms, 42);
        assert_eq!(session.mode.as_deref(), Some("agent"));
        assert_eq!(session.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(session.model.as_deref(), Some("provider/model"));
        assert_eq!(session.fast_mode, Some(true));
    }

    #[test]
    fn cwd_mutation_updates_session_metadata_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_session(
            &mut session,
            SessionMutation::SetCwd {
                cwd: "/repo".into(),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn checkpoint_mutation_installs_checkpoint_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("old")),
            protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("old reply")),
                None,
                Vec::new(),
            )),
            HistoryItem::user(Content::text("recent")),
        ];

        let result = apply_session(
            &mut session,
            SessionMutation::InstallContextCheckpoint {
                kind: "compaction".into(),
                summary: "summary".into(),
                first_live_message_index: 2,
                tokens_before: Some(100),
            },
        );

        assert!(result.applied);
        assert!(result.session_dirty);
        assert_eq!(session.checkpoint.as_ref().unwrap().first_live_index, 2);
    }

    #[test]
    fn finish_turn_state_mutation_appends_meta_and_snapshots_context() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.context_tokens = Some(123);
        session.history = vec![HistoryItem::user(Content::text("hello"))];
        let meta = TurnMeta {
            elapsed_ms: 10,
            avg_tps: Some(2.0),
            display_tps: Some(2.0),
            interrupted: false,
            tool_elapsed: std::collections::HashMap::new(),
        };

        let result = apply_session(
            &mut session,
            SessionMutation::FinishTurnState {
                history_len: 7,
                meta,
                snapshot_context: true,
                update_context_token_history_len: true,
            },
        );

        assert!(result.session_dirty);
        assert!(result.applied);
        assert_eq!(session.turn_metas.len(), 1);
        assert_eq!(session.turn_metas[0].0, 7);
        assert_eq!(session.context_tokens_history_len, Some(7));
        assert!(session.context_snapshots.iter().any(|(idx, _)| *idx == 7));
    }

    #[test]
    fn prune_rewindable_state_mutation_truncates_snapshots_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let meta = TurnMeta {
            elapsed_ms: 10,
            avg_tps: None,
            display_tps: None,
            interrupted: false,
            tool_elapsed: std::collections::HashMap::new(),
        };
        session.turn_metas.push((2, meta.clone()));
        session.turn_metas.push((4, meta));
        session.snapshot_metadata_at(2);
        session.snapshot_metadata_at(4);

        let result = apply_session(
            &mut session,
            SessionMutation::PruneRewindableState {
                history_len: 2,
                identity: token_identity(),
            },
        );

        assert!(result.session_dirty);
        assert!(result.turn_meta.is_some());
        assert_eq!(session.turn_metas.len(), 1);
        assert_eq!(session.metadata_snapshots.len(), 1);
    }

    #[test]
    fn append_history_item_mutation_updates_history_and_reports_dirty_range() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::user(Content::text("existing")));

        let result = apply_session(
            &mut session,
            SessionMutation::AppendHistoryItem {
                item: HistoryItem::user(Content::text("new")),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(result.history_idx, Some(1));
        assert_eq!(result.history_dirty_from, Some(1));
        assert_eq!(session.history.len(), 2);
    }

    #[test]
    fn append_history_with_transcript_block_updates_both_projections() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::user(Content::text("existing")));
        let mut transcript = TranscriptDocument::new();

        let result = apply_session_and_transcript(
            &mut session,
            &mut transcript,
            SessionMutation::CommitRequestHistoryItem {
                item: HistoryItem::user(Content::text("new")),
                block: Some(Block::User {
                    text: "new".into(),
                    image_labels: Vec::new(),
                    command: false,
                }),
                first_user_message: None,
            },
        );

        assert!(result.session_dirty);
        assert!(result.transcript_dirty);
        assert_eq!(result.history_idx, Some(1));
        assert_eq!(result.history_dirty_from, Some(1));
        assert_eq!(session.history.len(), 2);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(1))
        );
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
    }

    #[test]
    fn commit_request_history_item_snapshots_first_user_message_atomically() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut transcript = TranscriptDocument::new();

        let result = apply_session_and_transcript(
            &mut session,
            &mut transcript,
            SessionMutation::CommitRequestHistoryItem {
                item: HistoryItem::user(Content::text("new")),
                block: Some(Block::User {
                    text: "new".into(),
                    image_labels: Vec::new(),
                    command: false,
                }),
                first_user_message: Some("new".into()),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(result.history_idx, Some(0));
        assert_eq!(session.first_user_message.as_deref(), Some("new"));
        assert_eq!(session.metadata_snapshots.len(), 1);
        assert_eq!(session.metadata_snapshots[0].0, 1);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn live_append_history_with_transcript_block_updates_both_projections() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut live_session = empty_live_session_for(&session, 0);
        let mut transcript = TranscriptDocument::new();

        let result = SessionDocument::apply(
            SessionMutationTarget::LiveSessionAndTranscript {
                session: &mut session,
                live_session: &mut live_session,
                transcript: &mut transcript,
            },
            SessionMutation::CommitRequestHistoryItem {
                item: HistoryItem::user(Content::text("new")),
                block: Some(Block::User {
                    text: "new".into(),
                    image_labels: Vec::new(),
                    command: false,
                }),
                first_user_message: None,
            },
        );

        assert!(result.session_dirty);
        assert!(result.transcript_dirty);
        assert_eq!(result.history_idx, Some(0));
        assert_eq!(live_session.history_len(), 1);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn runtime_apply_routes_history_append_to_live_session() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.live_session = Some(empty_live_session_for(&session, 0));
        let mut parser = StreamParser::new();

        let result = document.apply(
            &mut session,
            &mut parser,
            true,
            SessionMutation::AppendHistoryItem {
                item: HistoryItem::user(Content::text("live")),
            },
        );

        assert_eq!(result.history_idx, Some(0));
        assert!(session.history.is_empty());
        assert_eq!(
            document
                .live_session
                .as_ref()
                .expect("live session")
                .history_len(),
            1
        );
        assert_eq!(document.generation(), PersistenceGeneration::new(1));
        assert_eq!(document.dirty_history_from_for_test(), Some(0));
    }

    #[test]
    fn runtime_apply_routes_streaming_mutation_to_parser_and_transcript() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        let mut parser = StreamParser::new();

        let result = document.apply(
            &mut session,
            &mut parser,
            true,
            SessionMutation::AppendStreamingText {
                delta: "hello".into(),
            },
        );

        assert!(result.transcript_dirty);
        assert_eq!(
            document.transcript.history().descriptor_dirty_from(),
            Some(0)
        );
        assert_eq!(document.generation(), PersistenceGeneration::new(1));
    }

    #[test]
    fn apply_history_append_mutation_replaces_history_and_reports_dirty_range() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![HistoryItem::note(protocol::HistoryNote::named_context(
            "cwd", "old",
        ))];

        let result = apply_session(
            &mut session,
            SessionMutation::ApplyHistoryAppend {
                append: HistoryAppend::replace_context_note(
                    HistoryItem::note(protocol::HistoryNote::named_context("cwd", "new")),
                    "cwd",
                ),
                identity: token_identity(),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(
            result.history_append_result,
            Some(HistoryAppendResult::ReplacedLast)
        );
        assert_eq!(result.history_dirty_from, Some(0));
        assert_eq!(session.history.len(), 1);
        assert_eq!(
            session.history[0]
                .as_note()
                .map(protocol::HistoryNote::text),
            Some("new")
        );
    }

    #[test]
    fn removing_history_item_prunes_rewindable_side_tables_atomically() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![HistoryItem::note(protocol::HistoryNote::named_context(
            "cwd", "old",
        ))];
        session.snapshot_context_at(1);

        let result = apply_session(
            &mut session,
            SessionMutation::ApplyHistoryAppend {
                append: HistoryAppend::remove_context_note("cwd"),
                identity: token_identity(),
            },
        );

        assert_eq!(
            result.history_append_result,
            Some(HistoryAppendResult::RemovedLast)
        );
        assert!(session.history.is_empty());
        assert!(session.context_snapshots.is_empty());
    }

    #[test]
    fn truncate_history_mutation_updates_history_and_reports_dirty_range() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("one")),
            HistoryItem::user(Content::text("two")),
            HistoryItem::user(Content::text("three")),
        ];

        session.snapshot_context_at(3);
        let result = apply_session(
            &mut session,
            SessionMutation::TruncateHistoryFrom {
                index: 1,
                identity: token_identity(),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(result.history_dirty_from, Some(1));
        assert_eq!(session.history.len(), 1);
        assert!(session.context_snapshots.is_empty());
    }

    #[test]
    fn append_transcript_block_mutation_updates_transcript_and_reports_dirty() {
        let mut transcript = TranscriptDocument::new();

        let result = apply_transcript(
            &mut transcript,
            SessionMutation::AppendTranscriptBlock {
                block: Block::Text {
                    content: "descriptor-only text".into(),
                },
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert!(!transcript.history().is_empty());
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
    }

    #[test]
    fn empty_append_transcript_block_mutation_reports_noop() {
        let mut transcript = TranscriptDocument::new();

        let result = apply_transcript(
            &mut transcript,
            SessionMutation::AppendTranscriptBlock {
                block: Block::Text {
                    content: "  \n\t  ".into(),
                },
            },
        );

        assert!(!result.applied);
        assert!(!result.transcript_dirty);
        assert!(transcript.history().is_empty());
        assert_eq!(transcript.history().descriptor_dirty_from(), None);
    }

    #[test]
    fn rewrite_transcript_block_mutation_updates_transcript_and_reports_dirty() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "old".into(),
        });
        let id = transcript.history().order[0];
        transcript.history_mut().clear_descriptor_dirty();

        let result = apply_transcript(
            &mut transcript,
            SessionMutation::RewriteTranscriptBlock {
                id,
                block: Block::Text {
                    content: "new".into(),
                },
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
        assert!(matches!(
            transcript.history().block(id),
            Some(Block::Text { content }) if content == "new"
        ));
    }

    #[test]
    fn replace_transcript_from_history_reports_dirty_and_unpersisted() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "old".into(),
        });
        transcript.history_mut().clear_descriptor_dirty();

        let mut rebuilt = smelt_core::content::transcript::Transcript::new();
        rebuilt.push_with_origin(
            Block::Text {
                content: "rebuilt".into(),
            },
            BlockOrigin::History(0),
        );

        let result = apply_transcript(
            &mut transcript,
            SessionMutation::ReplaceTranscriptFromHistory {
                transcript: rebuilt,
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert!(result.descriptors_unpersisted);
        assert_eq!(transcript.history().len(), 1);
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn checkpoint_marker_transcript_mutation_reports_dirty() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "history text".into(),
        });
        transcript.history_mut().clear_descriptor_dirty();

        let result = apply_transcript(
            &mut transcript,
            SessionMutation::InsertCheckpointMarker {
                block_index: 0,
                block: Block::Compacted {
                    summary: "summary".into(),
                },
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::CheckpointMarker)
        );
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
    }

    #[test]
    fn remove_unoriginated_transcript_mutation_reports_when_applied() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "temporary".into(),
        });
        transcript.push_with_origin(
            Block::Text {
                content: "history".into(),
            },
            BlockOrigin::History(0),
        );
        transcript.history_mut().clear_descriptor_dirty();

        let removed = apply_transcript(
            &mut transcript,
            SessionMutation::RemoveUnoriginatedTranscriptBlockAt { block_index: 0 },
        );
        let skipped = apply_transcript(
            &mut transcript,
            SessionMutation::RemoveUnoriginatedTranscriptBlockAt { block_index: 0 },
        );

        assert!(removed.applied);
        assert!(removed.transcript_dirty);
        assert!(!skipped.applied);
        assert!(!skipped.transcript_dirty);
        assert_eq!(transcript.history().len(), 1);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn truncate_transcript_mutation_reports_dirty_when_it_removes_blocks() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "one".into(),
        });
        transcript.push(Block::Text {
            content: "two".into(),
        });
        transcript.history_mut().clear_descriptor_dirty();

        let result = apply_transcript(
            &mut transcript,
            SessionMutation::TruncateTranscriptTo { block_index: 1 },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert_eq!(transcript.history().len(), 1);
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(1));
    }

    #[test]
    fn clear_transcript_mutation_reports_dirty_and_removes_blocks() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "one".into(),
        });
        transcript.push(Block::Text {
            content: "two".into(),
        });
        transcript.history_mut().clear_descriptor_dirty();

        let result = apply_transcript(&mut transcript, SessionMutation::ClearTranscript);

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert!(transcript.history().is_empty());
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
    }

    #[test]
    fn compaction_preview_mutations_update_transcript_and_report_dirty() {
        let mut transcript = TranscriptDocument::new();

        let updated = apply_transcript(
            &mut transcript,
            SessionMutation::UpdateCompactionPreview {
                summary: "streaming summary".into(),
            },
        );
        let id = updated.block_id.expect("preview block id");

        assert!(updated.applied);
        assert!(updated.transcript_dirty);
        assert_eq!(transcript.compaction_preview_id(), Some(id));
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));

        transcript.history_mut().clear_descriptor_dirty();
        let cleared = apply_transcript(&mut transcript, SessionMutation::ClearCompactionPreview);

        assert!(cleared.applied);
        assert!(cleared.transcript_dirty);
        assert_eq!(cleared.block_id, Some(id));
        assert_eq!(transcript.compaction_preview_id(), None);
    }

    #[test]
    fn stream_parser_noop_does_not_report_ambient_transcript_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "already dirty".into(),
        });
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));

        let result = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::ClearToolDrafts,
        );

        assert!(!result.transcript_dirty);
        assert!(!result.descriptors_unpersisted);
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
    }

    #[test]
    fn streaming_text_mutation_updates_transcript_and_reports_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();

        let result = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::AppendStreamingText {
                delta: "hello".into(),
            },
        );

        assert!(result.transcript_dirty);
        assert!(result.descriptors_unpersisted);
        assert!(!transcript.history().is_empty());
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
    }

    #[test]
    fn active_tool_elapsed_mutation_updates_transcript_and_reports_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        let now = Instant::now();

        apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::StartTool {
                call_id: "tool-1".into(),
                name: "bash".into(),
                summary: StyledLines::from_plain("bash"),
                args: HashMap::new(),
                now,
            },
        );
        transcript.history_mut().clear_descriptor_dirty();

        let result = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::SyncActiveToolElapsed {
                now: now + Duration::from_secs(2),
            },
        );

        assert!(result.transcript_dirty);
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
    }

    #[test]
    fn tool_lifecycle_mutations_update_transcript_and_report_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        let now = Instant::now();

        let started = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::StartTool {
                call_id: "tool-1".into(),
                name: "bash".into(),
                summary: StyledLines::from_plain("bash"),
                args: HashMap::new(),
                now,
            },
        );
        let output = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::AppendActiveToolOutput {
                call_id: "tool-1".into(),
                chunk: "done".into(),
            },
        );
        let finished = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::FinishTool {
                call_id: "tool-1".into(),
                status: ToolStatus::Ok,
                output: None,
                engine_elapsed: None,
                now,
            },
        );

        assert!(started.transcript_dirty);
        assert!(output.transcript_dirty);
        assert!(finished.transcript_dirty);
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
        assert_eq!(transcript.history().len(), 1);
    }

    #[test]
    fn draft_tool_mutations_update_transcript_and_report_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        let now = Instant::now();

        let upserted = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::UpsertToolDraft {
                update: ToolDraftUpdate {
                    stream_id: "stream-1".into(),
                    call_id: Some("tool-1".into()),
                    name: "bash".into(),
                    summary: StyledLines::from_plain("bash"),
                    args: HashMap::new(),
                    raw_arguments: "{}".into(),
                    finished: false,
                },
            },
        );
        transcript.history_mut().clear_descriptor_dirty();
        let promoted = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::PromoteToolDraft {
                stream_id: Some("stream-1".into()),
                start: ToolStart {
                    call_id: "tool-1".into(),
                    name: "bash".into(),
                    summary: StyledLines::from_plain("bash"),
                    args: HashMap::new(),
                    preview_output: None,
                },
                now,
            },
        );

        assert!(upserted.transcript_dirty);
        assert!(promoted.applied);
        assert!(promoted.transcript_dirty);
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
        assert_eq!(transcript.history().len(), 1);
    }

    #[test]
    fn exec_lifecycle_mutations_update_transcript_and_report_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();

        let started = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::StartExec {
                command: "echo hi".into(),
            },
        );
        let output = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::AppendExecOutput { chunk: "hi".into() },
        );
        let finalized = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            SessionMutation::FinalizeExec,
        );

        assert!(started.transcript_dirty);
        assert!(output.transcript_dirty);
        assert!(!finalized.transcript_dirty);
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
        assert_eq!(transcript.history().len(), 1);
    }

    fn runtime_metadata() -> RuntimeSessionMetadata {
        RuntimeSessionMetadata {
            updated_at_ms: 20,
            mode: "agent".into(),
            reasoning_effort: ReasoningEffort::Low,
            model: Some("model-a".into()),
            fast_mode: false,
        }
    }

    fn receipt_for(
        intent: &SessionSaveIntent,
        previous: smelt_store::StoreHead,
    ) -> smelt_store::SaveReceipt {
        let descriptor_len =
            intent
                .descriptors
                .as_ref()
                .map_or(previous.descriptor_len, |suffix| {
                    smelt_store::DescriptorLen::new(
                        suffix.start.get()
                            + u64::try_from(suffix.records.len()).expect("descriptor count"),
                    )
                });
        smelt_store::SaveReceipt {
            session_id: intent.identity.id.clone(),
            previous,
            current: smelt_store::StoreHead {
                revision: previous.revision.checked_add(1).expect("revision"),
                history_len: intent.history.final_len,
                descriptor_len,
            },
        }
    }

    #[test]
    fn persistence_generation_is_checked() {
        assert_eq!(
            PersistenceGeneration::new(41).checked_next(),
            Some(PersistenceGeneration::new(42))
        );
        assert_eq!(PersistenceGeneration::new(u64::MAX).checked_next(), None);
    }

    #[test]
    fn no_op_canonical_mutation_does_not_advance_generation() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.updated_at_ms = 20;
        session.mode = Some("agent".into());
        session.reasoning_effort = Some(ReasoningEffort::Low);
        session.model = Some("model-a".into());
        session.fast_mode = Some(false);
        session.cwd = Some("/tmp".into());
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        let mut parser = StreamParser::new();

        for mutation in [
            SessionMutation::SetFastMode { enabled: false },
            SessionMutation::SetCwd { cwd: "/tmp".into() },
            SessionMutation::UpdateRuntimeMetadata {
                updated_at_ms: 20,
                mode: "agent".into(),
                reasoning_effort: ReasoningEffort::Low,
                model: Some("model-a".into()),
                fast_mode: false,
            },
            SessionMutation::TruncateHistoryFrom {
                index: 0,
                identity: token_identity(),
            },
            SessionMutation::ClearTranscript,
        ] {
            let result = document.apply(&mut session, &mut parser, true, mutation);
            assert!(!result.canonical_changed());
        }
        assert_eq!(document.generation(), PersistenceGeneration::ZERO);
    }

    #[test]
    fn materialized_intent_is_cumulative_and_matching_acknowledgement_cleans() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        let mut parser = StreamParser::new();
        document.apply(
            &mut session,
            &mut parser,
            true,
            SessionMutation::AppendHistoryItem {
                item: HistoryItem::user(Content::text("first")),
            },
        );
        document.apply(
            &mut session,
            &mut parser,
            true,
            SessionMutation::AppendHistoryItem {
                item: HistoryItem::user(Content::text("second")),
            },
        );

        let previous = document.acknowledged_head();
        let intent = document
            .prepare_save(&mut session, runtime_metadata())
            .expect("prepare intent")
            .expect("dirty intent");
        assert_eq!(intent.history.start, smelt_store::HistoryIndex::ZERO);
        assert_eq!(intent.history.final_len, smelt_store::HistoryLen::new(2));
        assert_eq!(intent.history.items.len(), 2);

        let epoch = SessionEpoch::new(7);
        let receipt = receipt_for(&intent, previous);
        document.bind_persistence(epoch);
        assert!(document.acknowledge(epoch, intent.generation, &receipt, &session.id, 2, None));
        assert_eq!(document.durable_generation(), intent.generation);
        assert_eq!(document.dirty_history_from_for_test(), None);
        assert!(!document.has_session_work());
    }

    #[test]
    fn acknowledgement_rejects_wrong_scope_and_head() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        let mut parser = StreamParser::new();
        document.apply(
            &mut session,
            &mut parser,
            true,
            SessionMutation::AppendHistoryItem {
                item: HistoryItem::user(Content::text("dirty")),
            },
        );
        let previous = document.acknowledged_head();
        let intent = document
            .prepare_save(&mut session, runtime_metadata())
            .expect("prepare intent")
            .expect("dirty intent");
        let epoch = SessionEpoch::new(3);
        let receipt = receipt_for(&intent, previous);
        document.bind_persistence(epoch);

        assert!(!document.acknowledge(
            SessionEpoch::new(4),
            intent.generation,
            &receipt,
            &session.id,
            1,
            None,
        ));
        assert!(!document.acknowledge(
            epoch,
            PersistenceGeneration::new(intent.generation.get() - 1),
            &receipt,
            &session.id,
            1,
            None,
        ));
        assert!(!document.acknowledge(
            epoch,
            PersistenceGeneration::new(intent.generation.get() + 1),
            &receipt,
            &session.id,
            1,
            None,
        ));
        assert!(!document.acknowledge(epoch, intent.generation, &receipt, &session.id, 2, None));
        let mut wrong_session = receipt.clone();
        wrong_session.session_id = "different-session".into();
        assert!(!document.acknowledge(
            epoch,
            intent.generation,
            &wrong_session,
            &session.id,
            1,
            None,
        ));
        let mut wrong_descriptor_len = receipt.clone();
        wrong_descriptor_len.current.descriptor_len = smelt_store::DescriptorLen::new(1);
        assert!(!document.acknowledge(
            epoch,
            intent.generation,
            &wrong_descriptor_len,
            &session.id,
            1,
            None,
        ));
        let mut wrong_head = receipt.clone();
        wrong_head.previous.revision = smelt_store::Revision::new(9);
        assert!(!document.acknowledge(epoch, intent.generation, &wrong_head, &session.id, 1, None,));
        let mut skipped_revision = receipt.clone();
        skipped_revision.current.revision = skipped_revision
            .previous
            .revision
            .checked_add(2)
            .expect("revision");
        assert!(!document.acknowledge(
            epoch,
            intent.generation,
            &skipped_revision,
            &session.id,
            1,
            None,
        ));
        assert!(document.has_session_work());
    }

    #[test]
    fn empty_unpublished_draft_does_not_build_intent() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        let mut parser = StreamParser::new();
        document.apply(
            &mut session,
            &mut parser,
            true,
            SessionMutation::SetFastMode { enabled: true },
        );

        assert!(document
            .prepare_save(&mut session, runtime_metadata())
            .expect("prepare draft")
            .is_none());
        assert!(document.has_session_work());
    }

    #[test]
    fn generated_mutation_sequences_build_complete_cumulative_intents() {
        for seed in 0..64_u64 {
            let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
            let mut document = TuiSessionDocument::new(TranscriptDocument::new());
            let mut parser = StreamParser::new();
            let mut state = seed.wrapping_add(1);
            for step in 0..24 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let mutation = match state % 5 {
                    0 => SessionMutation::AppendHistoryItem {
                        item: HistoryItem::user(Content::text(format!("{seed}:{step}"))),
                    },
                    1 => SessionMutation::TruncateHistoryFrom {
                        index: (state as usize) % (session.history.len() + 1),
                        identity: token_identity(),
                    },
                    2 => SessionMutation::SetFastMode {
                        enabled: state & 1 == 0,
                    },
                    3 => SessionMutation::SetCwd {
                        cwd: format!("/tmp/{seed}/{step}"),
                    },
                    _ => SessionMutation::SetTitle {
                        title: format!("title-{seed}-{step}"),
                        slug: format!("title-{seed}-{step}"),
                        snapshot_history_len: session.history.len(),
                    },
                };
                document.apply(&mut session, &mut parser, true, mutation);
            }
            document.apply(
                &mut session,
                &mut parser,
                true,
                SessionMutation::AppendHistoryItem {
                    item: HistoryItem::user(Content::text(format!("final-{seed}"))),
                },
            );

            let intent = document
                .prepare_save(&mut session, runtime_metadata())
                .expect("prepare generated intent")
                .expect("generated session has content");
            let history_len = session.history.len();
            assert_eq!(intent.history.start, smelt_store::HistoryIndex::ZERO);
            assert_eq!(
                intent.history.final_len,
                smelt_store::HistoryLen::new(history_len as u64)
            );
            assert_eq!(intent.history.items, session.history);
            assert_eq!(
                intent.identity,
                smelt_core::session::store_identity_from_session(&session).unwrap()
            );
            assert_eq!(
                intent.metadata,
                smelt_core::session::store_metadata_from_session(&session, history_len).unwrap()
            );
            assert_eq!(
                intent.side_tables,
                smelt_core::session::store_side_table_suffixes_from_session_at(
                    &session,
                    0,
                    history_len,
                )
                .unwrap()
            );
        }
    }

    #[test]
    fn materialized_descriptor_repair_preserves_store_backed_history_boundary() {
        let session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "reconstructed".into(),
        });
        let loaded = crate::app::transcript::LoadedTranscript::full(transcript);

        document.install_loaded_store_session(
            loaded,
            empty_live_session_for(&session, 2),
            smelt_store::StoreHead {
                revision: smelt_store::Revision::new(0),
                history_len: smelt_store::HistoryLen::new(2),
                descriptor_len: smelt_store::DescriptorLen::new(1),
            },
            true,
        );

        assert_eq!(document.dirty_history_from_for_test(), None);
        assert_eq!(
            document.transcript.history().descriptor_dirty_from(),
            Some(0)
        );
        assert!(document.has_session_work());
    }

    #[test]
    fn store_backed_cumulative_intent_spans_sqlite_prefix_and_live_suffix() {
        const ID: &str = "7777777777777777777777777777777777777777777777777777777777777777";
        let root = tempfile::tempdir().expect("session root");
        let mut stored = Session::new(1, std::path::PathBuf::from("/tmp"));
        stored.id = ID.into();
        stored.history = vec![
            HistoryItem::user(Content::text("stored-0")),
            HistoryItem::user(Content::text("stored-1")),
        ];
        let mut writer = smelt_store::OwnedSessionWriter::open(root.path(), ID).expect("writer");
        let command = smelt_core::session::initial_store_commit_from_session(&stored)
            .expect("initial commit");
        let receipt = writer.commit_session(&command).expect("store prefix");
        let session_dir = writer.session_dir().to_path_buf();
        let mut meta = meta_with_token_identity();
        meta.id = ID.into();
        meta.history_len = Some(2);
        let header = SessionHeader {
            meta,
            history_len: 2,
            revision: receipt.current.revision.get(),
            degraded_warnings: Vec::new(),
        };
        let store_ref = SessionStoreRef {
            db_path: session_dir.join("session.db"),
            session_dir,
        };
        let mut session = stored.clone();
        session.history.clear();
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.live_session = Some(LiveSession::from_store(header, store_ref));
        document.changes.install_head(receipt.current);
        document.set_history_resave_from_for_test(0);
        let mut parser = StreamParser::new();
        document.apply(
            &mut session,
            &mut parser,
            true,
            SessionMutation::AppendHistoryItem {
                item: HistoryItem::user(Content::text("live-2")),
            },
        );

        let intent = document
            .prepare_save(&mut session, runtime_metadata())
            .expect("prepare intent")
            .expect("dirty intent");
        assert_eq!(intent.history.start, smelt_store::HistoryIndex::ZERO);
        assert_eq!(intent.history.final_len, smelt_store::HistoryLen::new(3));
        assert_eq!(
            intent.history.items,
            vec![
                HistoryItem::user(Content::text("stored-0")),
                HistoryItem::user(Content::text("stored-1")),
                HistoryItem::user(Content::text("live-2")),
            ]
        );
        writer.release().expect("release writer");
    }

    #[test]
    fn matching_store_backed_acknowledgement_compacts_live_suffix() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.live_session = Some(empty_live_session_for(&session, 2));
        document.changes.install_head(smelt_store::StoreHead {
            revision: smelt_store::Revision::new(5),
            history_len: smelt_store::HistoryLen::new(2),
            descriptor_len: smelt_store::DescriptorLen::ZERO,
        });
        let mut parser = StreamParser::new();
        document.apply(
            &mut session,
            &mut parser,
            true,
            SessionMutation::AppendHistoryItem {
                item: HistoryItem::user(Content::text("live")),
            },
        );

        let previous = document.acknowledged_head();
        let intent = document
            .prepare_save(&mut session, runtime_metadata())
            .expect("prepare intent")
            .expect("dirty intent");
        assert_eq!(intent.history.start, smelt_store::HistoryIndex::new(2));
        assert_eq!(intent.history.items.len(), 1);
        let epoch = SessionEpoch::new(9);
        let receipt = receipt_for(&intent, previous);
        document.bind_persistence(epoch);
        assert!(document.acknowledge(epoch, intent.generation, &receipt, &session.id, 3, None));
        assert_eq!(
            document
                .live_session
                .as_ref()
                .expect("live session")
                .live_suffix_len(),
            0
        );
    }
}
