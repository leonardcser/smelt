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
#[cfg(test)]
use smelt_core::session_save::DurableCursor;
#[cfg(test)]
use smelt_core::session_save::SubmittedHistoryRange;
pub(crate) use smelt_core::session_save::{
    DescriptorAppendSubmission, DocumentGeneration, SessionSaveSkipReason,
};
use smelt_core::session_save::{
    DocumentDirtyState, PersistDelta, PersistDescriptorDelta, SaveAckPlan, SessionPersistState,
    SessionSavePlan, SessionSaveState,
};
use smelt_core::transcript_model::{Block, BlockId, BlockOrigin, ToolOutputRef, ToolStatus};

use crate::app::transcript::TranscriptDocument;

pub(crate) struct SessionDocument {
    session: Session,
    transcript: crate::app::transcript::LoadedTranscript,
    live_session: Option<smelt_core::session_runtime::LiveSession>,
}

pub(crate) struct TuiSessionDocument {
    pub(crate) transcript: TranscriptDocument,
    pub(crate) live_session: Option<LiveSession>,
    persist: SessionPersistState,
}

impl TuiSessionDocument {
    pub(crate) fn new(transcript: TranscriptDocument) -> Self {
        Self {
            transcript,
            live_session: None,
            persist: SessionPersistState::new(),
        }
    }

    pub(crate) fn apply(
        &mut self,
        session: &mut Session,
        parser: &mut StreamParser,
        persist_mutation: bool,
        mutation: SessionMutation,
    ) -> MutationResult {
        let persistence = if !persist_mutation {
            SessionMutationPersistence::ReadOnly
        } else if self.live_session.is_some() {
            SessionMutationPersistence::Live(&mut self.persist)
        } else {
            SessionMutationPersistence::Full(&mut self.persist)
        };
        SessionDocument::apply_runtime(
            session,
            self.live_session.as_mut(),
            &mut self.transcript,
            parser,
            persistence,
            mutation,
        )
    }

    pub(crate) fn has_session_work(&self) -> bool {
        self.persist.has_session_work()
    }

    pub(crate) fn descriptors_persisted(&self) -> bool {
        self.persist.descriptors_persisted()
    }

    pub(crate) fn mark_history_resave_required(&mut self, history_index: usize) {
        self.persist.require_history_resave_from(history_index);
    }

    pub(crate) fn mark_session_unpersisted(&mut self) {
        self.persist.reset_unpersisted();
    }

    pub(crate) fn install_materialized_session(&mut self, descriptors_persisted: bool) {
        let descriptor_len = durable_descriptor_len_for_transcript(&self.transcript);
        self.persist
            .install_materialized_session(descriptors_persisted, descriptor_len);
    }

    pub(crate) fn install_loaded_full_session(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
        writable: bool,
        history_len: usize,
        revision: u64,
    ) {
        self.live_session = None;
        self.transcript.replace_loaded_transcript(transcript);
        let descriptor_len = durable_descriptor_len_for_transcript(&self.transcript);
        self.persist
            .install_loaded_full_session(writable, history_len, descriptor_len, revision);
    }

    pub(crate) fn install_loaded_store_session(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
        live_session: LiveSession,
        writable: bool,
        history_len: usize,
        persisted_descriptor_len: Option<usize>,
    ) {
        let revision = live_session.header.revision;
        self.live_session = Some(live_session);
        self.transcript.replace_loaded_transcript(transcript);
        let descriptor_len = persisted_descriptor_len
            .unwrap_or_else(|| durable_descriptor_len_for_transcript(&self.transcript));
        self.persist
            .install_loaded_store_session(writable, history_len, descriptor_len, revision);
        if persisted_descriptor_len.is_some() {
            // The read-only compatibility fallback built a complete in-memory
            // transcript. Defer descriptor repair until the next owned save.
            self.persist.descriptors_persisted = false;
        }
    }

    #[cfg(test)]
    pub(crate) fn current_generation_for_test(&self) -> DocumentGeneration {
        self.persist.current_generation(&self.transcript)
    }

    #[cfg(test)]
    pub(crate) fn set_pending_save_for_test(
        &mut self,
        save_id: u64,
        session_id: String,
        kind: crate::persist::PersistSaveKind,
        generation: DocumentGeneration,
        history_len: usize,
    ) {
        self.persist
            .set_pending_save_for_test(save_id, session_id, kind, generation, history_len);
    }

    #[cfg(test)]
    pub(crate) fn set_history_resave_from_for_test(&mut self, history_index: usize) {
        self.persist.dirty_history_from = Some(history_index);
        self.persist.session_dirty = true;
        self.persist.bump_dirty_generation();
    }

    #[cfg(test)]
    pub(crate) fn durable_history_len_for_test(&self) -> usize {
        self.persist.durable.store_history_len
    }

    #[cfg(test)]
    pub(crate) fn dirty_history_from_for_test(&self) -> Option<usize> {
        self.persist.dirty_history_from
    }

    pub(crate) fn prepare_save(
        &mut self,
        session: &mut Session,
        metadata: RuntimeSessionMetadata,
        blobs_pending: bool,
    ) -> Result<PreparedSessionSave, String> {
        if self.live_session.is_some() {
            return self.prepare_live_save(session, metadata, blobs_pending);
        }
        self.prepare_full_save(session, metadata, blobs_pending)
    }

    pub(crate) fn prepare_request_history_append_save(
        &mut self,
        session: &mut Session,
        metadata: RuntimeSessionMetadata,
        request: RuntimeRequestHistoryAppendSave<'_>,
    ) -> Result<PreparedSessionSave, String> {
        self.apply_runtime_metadata(session, metadata);
        let can_persist_suffix = self
            .persist
            .can_persist_request_append_at(request.history_index);
        let generation = self.persist.current_generation(&self.transcript);
        if can_persist_suffix {
            let plan = SessionDocument::build_request_history_append_save(
                session,
                &self.transcript,
                generation,
                request.history_index,
                request.descriptor_order_start,
                request.item,
                request.include_side_tables,
            )
            .map_err(|err| err.to_string())?;
            return Ok(PreparedSessionSave::RequestHistoryAppend {
                generation: plan.generation,
                delta: plan.delta,
                descriptor_append: plan.descriptor_append,
            });
        }
        if let Some(live_session) = self.live_session.as_ref() {
            let input = SessionSaveInput {
                session,
                history: SessionHistoryRef::StoreBacked(live_session),
                transcript: &self.transcript,
                state: self.persist.live_save_state(
                    &self.transcript,
                    live_session,
                    request.blobs_pending,
                ),
            };
            return SessionDocument::prepare_save_from_input(input);
        }
        let input = SessionSaveInput {
            session,
            history: SessionHistoryRef::Materialized(&session.history),
            transcript: &self.transcript,
            state: self.persist.full_save_state(
                &self.transcript,
                session.history.len(),
                request.blobs_pending,
            ),
        };
        SessionDocument::prepare_save_from_input(input)
    }

    fn prepare_full_save(
        &mut self,
        session: &mut Session,
        metadata: RuntimeSessionMetadata,
        blobs_pending: bool,
    ) -> Result<PreparedSessionSave, String> {
        let preflight = SessionSaveInput {
            session,
            history: SessionHistoryRef::Materialized(&session.history),
            transcript: &self.transcript,
            state: self.persist.full_save_state(
                &self.transcript,
                session.history.len(),
                blobs_pending,
            ),
        };
        if let SessionSavePlan::Skip(reason) = SessionDocument::select_save_plan(preflight) {
            return Ok(PreparedSessionSave::Skip(reason));
        }
        self.apply_runtime_metadata(session, metadata);
        let input = SessionSaveInput {
            session,
            history: SessionHistoryRef::Materialized(&session.history),
            transcript: &self.transcript,
            state: self.persist.full_save_state(
                &self.transcript,
                session.history.len(),
                blobs_pending,
            ),
        };
        SessionDocument::prepare_save_from_input(input)
    }

    fn prepare_live_save(
        &mut self,
        session: &mut Session,
        metadata: RuntimeSessionMetadata,
        blobs_pending: bool,
    ) -> Result<PreparedSessionSave, String> {
        let Some(live_session) = self.live_session.as_ref() else {
            return self.prepare_full_save(session, metadata, blobs_pending);
        };
        let preflight = SessionSaveInput {
            session,
            history: SessionHistoryRef::StoreBacked(live_session),
            transcript: &self.transcript,
            state: self
                .persist
                .live_save_state(&self.transcript, live_session, blobs_pending),
        };
        if let SessionSavePlan::Skip(reason) = SessionDocument::select_save_plan(preflight) {
            return Ok(PreparedSessionSave::Skip(reason));
        }
        self.apply_runtime_metadata(session, metadata);
        let live_session = self
            .live_session
            .as_ref()
            .expect("live session present after metadata update");
        let input = SessionSaveInput {
            session,
            history: SessionHistoryRef::StoreBacked(live_session),
            transcript: &self.transcript,
            state: self
                .persist
                .live_save_state(&self.transcript, live_session, blobs_pending),
        };
        SessionDocument::prepare_save_from_input(input)
    }

    pub(crate) fn is_save_queued(&self) -> bool {
        self.persist.is_save_queued()
    }

    pub(crate) fn clear_queued_save(&mut self) {
        self.persist.clear_queued_save();
    }

    pub(crate) fn queue_save(&mut self) {
        self.persist.queue_save();
    }

    pub(crate) fn has_pending_save(&self) -> bool {
        self.persist.has_pending_save()
    }

    pub(crate) fn submit_metadata_save(
        &mut self,
        session_id: String,
        generation: DocumentGeneration,
        state: smelt_store::SessionState,
        side_tables: smelt_store::SideTableSuffixes,
    ) -> Option<smelt_core::session_save::SubmittedSessionCommit> {
        self.persist
            .begin_metadata_commit(session_id, generation, state, side_tables)
    }

    pub(crate) fn submit_history_save(
        &mut self,
        session_id: String,
        generation: DocumentGeneration,
        delta: PersistDelta,
        descriptor_append: Option<DescriptorAppendSubmission>,
    ) -> Result<Option<smelt_core::session_save::SubmittedSessionCommit>, String> {
        self.persist
            .begin_history_commit(session_id, generation, delta, descriptor_append)
    }

    pub(crate) fn mark_persisted(
        &mut self,
        receipt: &smelt_store::SaveReceipt,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> bool {
        SessionDocument::mark_persisted(
            &mut self.persist,
            &mut self.transcript,
            self.live_session.as_mut(),
            checkpoint,
            receipt,
        )
    }

    pub(crate) fn mark_persist_failed(&mut self, err: &crate::persist::PersistFailure) {
        SessionDocument::mark_persist_failed(
            &mut self.persist,
            &mut self.transcript,
            self.live_session.as_mut(),
            err,
        );
    }

    pub(crate) fn mark_ephemeral_persisted(
        &mut self,
        descriptor_append: Option<DescriptorAppendSubmission>,
    ) {
        SessionDocument::mark_ephemeral_persisted(
            &mut self.persist,
            &mut self.transcript,
            self.live_session.as_mut(),
            descriptor_append,
        );
    }

    pub(crate) fn has_unflushed_work(&self, session: &Session) -> bool {
        SessionDocument::has_unflushed_work_for(
            &self.persist,
            session,
            self.live_session.as_ref(),
            &self.transcript,
        )
    }

    fn apply_runtime_metadata(&mut self, session: &mut Session, metadata: RuntimeSessionMetadata) {
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
        self.persist
            .record_mutation(result.session_dirty, result.history_dirty_from);
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
    pub(crate) persisted_descriptor_len: Option<usize>,
}

struct SessionDescriptorSavePlan {
    start_descriptor_idx: usize,
    records: Vec<smelt_core::TranscriptBlockRecordWithId>,
}

pub(crate) enum PreparedSessionSave {
    Skip(SessionSaveSkipReason),
    MetadataOnly {
        generation: DocumentGeneration,
        state: Box<smelt_store::SessionState>,
        side_tables: Box<smelt_store::SideTableSuffixes>,
    },
    History {
        generation: DocumentGeneration,
        delta: Box<PersistDelta>,
    },
    RequestHistoryAppend {
        generation: DocumentGeneration,
        delta: Box<PersistDelta>,
        descriptor_append: DescriptorAppendSubmission,
    },
}

struct RequestHistoryAppendSavePlan {
    generation: DocumentGeneration,
    delta: Box<PersistDelta>,
    descriptor_append: DescriptorAppendSubmission,
}

#[derive(Clone, Copy)]
enum SessionHistoryRef<'a> {
    Materialized(&'a [HistoryItem]),
    StoreBacked(&'a LiveSession),
}

#[derive(Clone, Copy)]
struct SessionSaveInput<'a> {
    session: &'a Session,
    history: SessionHistoryRef<'a>,
    transcript: &'a TranscriptDocument,
    state: SessionSaveState,
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
    pub(crate) model: String,
    pub(crate) fast_mode: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct RuntimeRequestHistoryAppendSave<'a> {
    pub(crate) history_index: usize,
    pub(crate) descriptor_order_start: usize,
    pub(crate) item: &'a HistoryItem,
    pub(crate) include_side_tables: bool,
    pub(crate) blobs_pending: bool,
}

trait SessionPersistStateTuiExt {
    fn current_generation(&self, transcript: &TranscriptDocument) -> DocumentGeneration;
    fn full_save_state(
        &self,
        transcript: &TranscriptDocument,
        history_len: usize,
        blobs_pending: bool,
    ) -> SessionSaveState;
    fn live_save_state(
        &self,
        transcript: &TranscriptDocument,
        live_session: &LiveSession,
        blobs_pending: bool,
    ) -> SessionSaveState;
}

impl SessionPersistStateTuiExt for SessionPersistState {
    fn current_generation(&self, transcript: &TranscriptDocument) -> DocumentGeneration {
        self.generation_for_descriptor(transcript.history().descriptor_dirty_generation())
    }

    fn full_save_state(
        &self,
        transcript: &TranscriptDocument,
        history_len: usize,
        blobs_pending: bool,
    ) -> SessionSaveState {
        self.save_state(
            self.current_generation(transcript),
            transcript.history().descriptor_dirty_from(),
            history_len,
            blobs_pending,
            true,
        )
    }

    fn live_save_state(
        &self,
        transcript: &TranscriptDocument,
        live_session: &LiveSession,
        blobs_pending: bool,
    ) -> SessionSaveState {
        self.save_state(
            self.current_generation(transcript),
            transcript.history().descriptor_dirty_from(),
            live_session.history_len(),
            blobs_pending,
            false,
        )
    }
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
        model: String,
        fast_mode: bool,
    },
    SetFastMode {
        enabled: bool,
    },
    SetCwd {
        cwd: String,
    },
    RestoreCwd {
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
    LiveSession {
        session: &'a mut Session,
        live_session: &'a mut LiveSession,
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

enum SessionMutationPersistence<'a> {
    ReadOnly,
    Full(&'a mut SessionPersistState),
    Live(&'a mut SessionPersistState),
}

#[derive(Clone, Copy)]
enum MutationRecordTarget {
    Session,
    LiveSession,
    Transcript,
    StreamParserTranscript,
    SessionAndTranscript,
    LiveSessionAndTranscript,
}

impl MutationRecordTarget {
    fn touches_session(self) -> bool {
        matches!(
            self,
            Self::Session
                | Self::SessionAndTranscript
                | Self::LiveSession
                | Self::LiveSessionAndTranscript
        )
    }

    fn touches_live_session(self) -> bool {
        matches!(self, Self::LiveSession | Self::LiveSessionAndTranscript)
    }

    fn touches_transcript(self) -> bool {
        matches!(
            self,
            Self::Transcript
                | Self::StreamParserTranscript
                | Self::SessionAndTranscript
                | Self::LiveSessionAndTranscript
        )
    }
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
    ) -> Self {
        Self {
            session,
            transcript,
            live_session,
            persisted_descriptor_len: None,
        }
    }

    pub(crate) fn with_persisted_descriptor_len(mut self, descriptor_len: usize) -> Self {
        self.persisted_descriptor_len = Some(descriptor_len);
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
            SessionMutationTarget::LiveSession {
                session,
                live_session,
            } => Self::apply_to_live_session(session, live_session, mutation),
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
        persistence: SessionMutationPersistence<'_>,
        mutation: SessionMutation,
    ) -> MutationResult {
        let (result, target) = match mutation {
            SessionMutation::CommitRequestHistoryItem { .. } => {
                if let Some(live_session) = live_session {
                    (
                        Self::apply_to_live_session_and_transcript(
                            session,
                            live_session,
                            transcript,
                            mutation,
                        ),
                        MutationRecordTarget::LiveSessionAndTranscript,
                    )
                } else {
                    (
                        Self::apply_to_session_and_transcript(session, transcript, mutation),
                        MutationRecordTarget::SessionAndTranscript,
                    )
                }
            }
            SessionMutation::AppendHistoryItem { .. }
            | SessionMutation::TruncateHistoryFrom { .. }
            | SessionMutation::RewindHistoryTo { .. }
            | SessionMutation::SetCheckpoint { .. }
            | SessionMutation::SetCheckpointTokensAfterEstimate { .. } => {
                if let Some(live_session) = live_session {
                    (
                        Self::apply_to_live_session(session, live_session, mutation),
                        MutationRecordTarget::LiveSession,
                    )
                } else {
                    (
                        Self::apply_to_session(session, mutation),
                        MutationRecordTarget::Session,
                    )
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
            | SessionMutation::RewriteTranscriptBlock { .. } => (
                Self::apply_to_transcript(transcript, mutation),
                MutationRecordTarget::Transcript,
            ),
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
            | SessionMutation::FinalizeExec => (
                Self::apply_to_stream_parser_transcript(parser, transcript, mutation),
                MutationRecordTarget::StreamParserTranscript,
            ),
            mutation => (
                Self::apply_to_session(session, mutation),
                MutationRecordTarget::Session,
            ),
        };
        Self::record_runtime_mutation(persistence, target, &result);
        result
    }

    fn record_runtime_mutation(
        persistence: SessionMutationPersistence<'_>,
        target: MutationRecordTarget,
        result: &MutationResult,
    ) {
        match persistence {
            SessionMutationPersistence::ReadOnly => {}
            SessionMutationPersistence::Full(persist) => {
                if target.touches_session() {
                    persist.record_mutation(result.session_dirty, result.history_dirty_from);
                }
                if target.touches_transcript() {
                    persist
                        .record_transcript_descriptors_unpersisted(result.descriptors_unpersisted);
                }
            }
            SessionMutationPersistence::Live(persist) => {
                if target.touches_live_session() || target.touches_session() {
                    persist.record_mutation(result.session_dirty, result.history_dirty_from);
                }
                if target.touches_transcript() {
                    persist
                        .record_transcript_descriptors_unpersisted(result.descriptors_unpersisted);
                }
            }
        }
    }

    fn has_unflushed_work(state: DocumentDirtyState) -> bool {
        smelt_core::session_save::has_unflushed_work(state)
    }

    pub(crate) fn has_unflushed_work_for(
        persist: &SessionPersistState,
        session: &Session,
        live_session: Option<&LiveSession>,
        transcript: &TranscriptDocument,
    ) -> bool {
        let history_len = live_session.map_or(session.history.len(), LiveSession::history_len);
        Self::has_unflushed_work(persist.dirty_state(
            transcript.history().descriptor_dirty_from(),
            history_len,
            transcript.history().is_empty(),
        ))
    }

    #[cfg(test)]
    fn plan_save_ack(
        submitted: DocumentGeneration,
        current: DocumentGeneration,
        kind: crate::persist::PersistSaveKind,
    ) -> SaveAckPlan {
        smelt_core::session_save::plan_save_ack_for_kind(submitted, current, kind)
    }

    pub(crate) fn mark_persisted(
        persist: &mut SessionPersistState,
        transcript: &mut TranscriptDocument,
        live_session: Option<&mut LiveSession>,
        checkpoint: Option<&ContextCheckpoint>,
        receipt: &smelt_store::SaveReceipt,
    ) -> bool {
        let history_len = receipt
            .history_len
            .as_usize()
            .expect("saved history length originated as usize");
        let current_generation = persist.current_generation(transcript);
        let Some(outcome) = persist.ack_save(receipt, current_generation) else {
            return false;
        };
        if let Some(descriptor_append) = outcome.descriptor_append {
            Self::mark_request_history_append_persisted(transcript, descriptor_append);
        }
        match outcome.plan {
            SaveAckPlan::MarkClean { clear_descriptors } => {
                if let Some(live_session) = live_session {
                    if matches!(outcome.kind, crate::persist::PersistSaveKind::History) {
                        live_session.compact_saved_prefix(
                            history_len,
                            receipt.revision.get(),
                            checkpoint,
                        );
                    }
                }
                if clear_descriptors {
                    transcript.history_mut().clear_descriptor_dirty();
                }
            }
            SaveAckPlan::SaveAgain => {}
        }
        persist.is_save_queued()
    }

    pub(crate) fn mark_persist_failed(
        persist: &mut SessionPersistState,
        transcript: &mut TranscriptDocument,
        live_session: Option<&mut LiveSession>,
        failure: &crate::persist::PersistFailure,
    ) {
        let plan = persist.record_save_failure(
            failure.save_id,
            &failure.session_id,
            failure.commit_failure.as_ref(),
        );
        if live_session.is_some() && failure.commit_failure.is_none() {
            persist.mark_history_dirty_from(0);
        }
        if let Some(current) = plan.reconcile_descriptor_len {
            transcript.reconcile_dense_descriptor_count(current);
        }
    }

    pub(crate) fn mark_ephemeral_persisted(
        persist: &mut SessionPersistState,
        transcript: &mut TranscriptDocument,
        live_session: Option<&mut LiveSession>,
        descriptor_append: Option<DescriptorAppendSubmission>,
    ) {
        persist.mark_clean();
        if let Some(descriptor_append) = descriptor_append {
            Self::mark_request_history_append_persisted(transcript, descriptor_append);
        }
        Self::clear_persisted_descriptor_work(transcript, live_session);
    }

    fn select_save_plan(input: SessionSaveInput<'_>) -> SessionSavePlan {
        smelt_core::session_save::plan_session_save(input.state)
    }

    fn build_request_history_append_save(
        session: &Session,
        transcript: &TranscriptDocument,
        generation: DocumentGeneration,
        history_index: usize,
        descriptor_order_start: usize,
        item: &HistoryItem,
        include_side_tables: bool,
    ) -> Result<RequestHistoryAppendSavePlan, smelt_store::StoreError> {
        let descriptor_start_idx = transcript.descriptor_total_count().unwrap_or_else(|| {
            transcript
                .history()
                .descriptor_record_index_for_order_index(descriptor_order_start)
        });
        let descriptor_records = transcript
            .history()
            .descriptor_records_with_ids_from(descriptor_order_start);
        let descriptor_count = descriptor_records.len();
        let history_len = history_index.saturating_add(1);
        let state = smelt_core::session::store_state_from_session(session, history_len)?;
        let side_tables = if include_side_tables {
            smelt_core::session::store_side_table_suffixes_from_session(session, history_index)?
        } else {
            smelt_store::SideTableSuffixes {
                start: smelt_store::HistoryIndex::new(history_index as u64),
                turn_metas: Vec::new(),
                metadata_snapshots: Vec::new(),
                context_snapshots: Vec::new(),
            }
        };
        Ok(RequestHistoryAppendSavePlan {
            generation,
            delta: Box::new(PersistDelta {
                state,
                history: smelt_store::HistorySuffix {
                    start: smelt_store::HistoryIndex::new(history_index as u64),
                    final_len: smelt_store::HistoryLen::new(history_len as u64),
                    items: vec![item.clone()],
                },
                side_tables,
                descriptors: Some(PersistDescriptorDelta {
                    start_descriptor_idx: descriptor_start_idx,
                    records: descriptor_records,
                }),
            }),
            descriptor_append: DescriptorAppendSubmission {
                count: descriptor_count,
                had_descriptor_total: transcript.descriptor_total_count().is_some(),
            },
        })
    }

    fn prepare_save_from_input(input: SessionSaveInput<'_>) -> Result<PreparedSessionSave, String> {
        let plan = Self::select_save_plan(input);
        Self::prepare_session_save_from_plan(input, plan)
    }

    fn prepare_session_save_from_plan(
        input: SessionSaveInput<'_>,
        plan: SessionSavePlan,
    ) -> Result<PreparedSessionSave, String> {
        match plan {
            SessionSavePlan::Skip(reason) => Ok(PreparedSessionSave::Skip(reason)),
            SessionSavePlan::MetadataOnly { generation } => {
                let state = smelt_core::session::store_state_from_session(
                    input.session,
                    input.state.history_len,
                )
                .map_err(|err| err.to_string())?;
                let side_tables = smelt_core::session::store_side_table_suffixes_from_session_at(
                    input.session,
                    input.state.history_len,
                    input.state.history_len,
                )
                .map_err(|err| err.to_string())?;
                Ok(PreparedSessionSave::MetadataOnly {
                    generation,
                    state: Box::new(state),
                    side_tables: Box::new(side_tables),
                })
            }
            SessionSavePlan::History {
                generation,
                history_start_idx,
                dirty_history_from,
            } => {
                let descriptor_delta = descriptor_save_plan(
                    input.transcript,
                    input.state.descriptors_persisted,
                    dirty_history_from,
                );
                let history_len = input.state.history_len;
                debug_assert_eq!(history_len, input.history.len());
                debug_assert!(history_start_idx <= input.state.durable_history_len);
                let history = input.history.range(history_start_idx..history_len)?;
                debug_assert_eq!(history_len, history_start_idx.saturating_add(history.len()));
                let state =
                    smelt_core::session::store_state_from_session(input.session, history_len)
                        .map_err(|err| err.to_string())?;
                let side_tables = smelt_core::session::store_side_table_suffixes_from_session_at(
                    input.session,
                    history_start_idx,
                    history_len,
                )
                .map_err(|err| err.to_string())?;
                Ok(PreparedSessionSave::History {
                    generation,
                    delta: Box::new(PersistDelta {
                        state,
                        history: smelt_store::HistorySuffix {
                            start: smelt_store::HistoryIndex::new(history_start_idx as u64),
                            final_len: smelt_store::HistoryLen::new(history_len as u64),
                            items: history,
                        },
                        side_tables,
                        descriptors: descriptor_delta.map(|delta| PersistDescriptorDelta {
                            start_descriptor_idx: delta.start_descriptor_idx,
                            records: delta.records,
                        }),
                    }),
                })
            }
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
        }
    }

    pub(crate) fn from_store(
        header: SessionHeader,
        store_ref: SessionStoreRef,
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
        match mutation {
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
                session.record_context_tokens(tokens, identity);
                MutationResult {
                    session_dirty: true,
                    context_tokens_updated: true,
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
                session.session_cost_usd += cost_usd;
                session.session_usage.accumulate(&usage);
                MutationResult {
                    session_dirty: true,
                    ..Default::default()
                }
            }
            SessionMutation::SetTitle {
                title,
                slug,
                snapshot_history_len,
            } => {
                session.title = Some(title);
                session.slug = Some(slug);
                session.snapshot_metadata_at(snapshot_history_len);
                MutationResult {
                    session_dirty: true,
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
                session.updated_at_ms = updated_at_ms;
                session.mode = Some(mode);
                session.reasoning_effort = Some(reasoning_effort);
                session.model = Some(model);
                session.fast_mode = Some(fast_mode);
                MutationResult {
                    session_dirty: true,
                    ..Default::default()
                }
            }
            SessionMutation::SetFastMode { enabled } => {
                session.fast_mode = Some(enabled);
                MutationResult {
                    session_dirty: true,
                    ..Default::default()
                }
            }
            SessionMutation::SetCwd { cwd } => {
                session.cwd = Some(cwd);
                MutationResult {
                    session_dirty: true,
                    ..Default::default()
                }
            }
            SessionMutation::RestoreCwd { cwd } => {
                session.cwd = Some(cwd);
                MutationResult::default()
            }
            SessionMutation::FinishTurnState {
                history_len,
                meta,
                snapshot_context,
                update_context_token_history_len,
            } => {
                session.finish_turn_state(
                    history_len,
                    meta,
                    snapshot_context,
                    update_context_token_history_len,
                );
                MutationResult {
                    session_dirty: true,
                    applied: true,
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
                session.restore_metadata_after_rewind(history_len);
                MutationResult {
                    session_dirty: true,
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
        }
    }

    fn apply_to_live_session(
        session: &mut Session,
        live_session: &mut LiveSession,
        mutation: SessionMutation,
    ) -> MutationResult {
        match mutation {
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
            | SessionMutation::RestoreCwd { .. }
            | SessionMutation::FinishTurnState { .. }
            | SessionMutation::InstallContextCheckpoint { .. }
            | SessionMutation::InstallContextCheckpointAtHistoryIndex { .. }
            | SessionMutation::RestoreMetadataAfterRewind { .. }
            | SessionMutation::PruneRewindableState { .. } => MutationResult::default(),
        }
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
            | SessionMutation::RestoreCwd { .. }
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
            | SessionMutation::RestoreCwd { .. }
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

    pub(crate) fn clear_persisted_descriptor_work(
        transcript: &mut TranscriptDocument,
        _live_session: Option<&mut LiveSession>,
    ) {
        transcript.history_mut().clear_descriptor_dirty();
    }

    pub(crate) fn mark_request_history_append_persisted(
        transcript: &mut TranscriptDocument,
        descriptor_append: DescriptorAppendSubmission,
    ) {
        if descriptor_append.had_descriptor_total {
            transcript.note_persisted_descriptor_append(descriptor_append.count);
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
        )
    }
}

fn durable_descriptor_len_for_transcript(transcript: &TranscriptDocument) -> usize {
    transcript
        .descriptor_total_count()
        .unwrap_or_else(|| transcript.history().descriptor_records().len())
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

    fn save_receipt(save_id: u64, history_len: usize, revision: u64) -> smelt_store::SaveReceipt {
        smelt_store::SaveReceipt {
            session_id: "session-a".into(),
            save_id: smelt_store::SaveId::new(save_id),
            previous_revision: smelt_store::Revision::new(revision.saturating_sub(1)),
            revision: smelt_store::Revision::new(revision),
            history_len: smelt_store::HistoryLen::new(history_len as u64),
            descriptor_len: smelt_store::DescriptorLen::ZERO,
        }
    }

    fn prepare_request_append(
        persist: SessionPersistState,
        session: &mut Session,
        transcript: TranscriptDocument,
        live_session: Option<LiveSession>,
        history_index: usize,
        descriptor_order_start: usize,
        item: HistoryItem,
    ) -> Result<PreparedSessionSave, String> {
        let mut document = TuiSessionDocument {
            transcript,
            live_session,
            persist,
        };
        document.prepare_request_history_append_save(
            session,
            RuntimeSessionMetadata {
                updated_at_ms: 20,
                mode: "agent".into(),
                reasoning_effort: ReasoningEffort::Low,
                model: "model-a".into(),
                fast_mode: false,
            },
            RuntimeRequestHistoryAppendSave {
                history_index,
                descriptor_order_start,
                item: &item,
                include_side_tables: true,
                blobs_pending: false,
            },
        )
    }

    fn token_identity() -> ContextTokenIdentity {
        ContextTokenIdentity {
            model: "model-a".into(),
            api_base: "https://api.example.test".into(),
            provider_type: "openai".into(),
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
        new_identity.model = "model-b".into();

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
                model: "provider/model".into(),
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
    fn restore_cwd_mutation_updates_session_metadata_without_dirtying() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_session(
            &mut session,
            SessionMutation::RestoreCwd {
                cwd: "/repo".into(),
            },
        );

        assert!(!result.session_dirty);
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
        let mut live_session = empty_live_session_for(&session, 0);
        let mut transcript = TranscriptDocument::new();
        let mut parser = StreamParser::new();
        let mut persist = SessionPersistState::new();

        let result = SessionDocument::apply_runtime(
            &mut session,
            Some(&mut live_session),
            &mut transcript,
            &mut parser,
            SessionMutationPersistence::Live(&mut persist),
            SessionMutation::AppendHistoryItem {
                item: HistoryItem::user(Content::text("live")),
            },
        );

        assert_eq!(result.history_idx, Some(0));
        assert!(session.history.is_empty());
        assert_eq!(live_session.history_len(), 1);
        assert!(persist.session_dirty);
        assert_eq!(persist.dirty_history_from, Some(0));
    }

    #[test]
    fn runtime_apply_routes_streaming_mutation_to_parser_and_transcript() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut transcript = TranscriptDocument::new();
        let mut parser = StreamParser::new();
        let mut persist = SessionPersistState::new();

        let result = SessionDocument::apply_runtime(
            &mut session,
            None,
            &mut transcript,
            &mut parser,
            SessionMutationPersistence::Full(&mut persist),
            SessionMutation::AppendStreamingText {
                delta: "hello".into(),
            },
        );

        assert!(result.transcript_dirty);
        assert_eq!(transcript.history().descriptor_dirty_from(), Some(0));
        assert!(!persist.session_dirty);
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

    #[test]
    fn full_save_plan_uses_metadata_only_for_clean_persisted_history() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![HistoryItem::user(Content::text("persisted"))];
        let transcript = TranscriptDocument::new();

        let plan = SessionDocument::select_save_plan(SessionSaveInput {
            session: &session,
            history: SessionHistoryRef::Materialized(&session.history),
            transcript: &transcript,
            state: SessionSaveState {
                generation: DocumentGeneration::new(0, 0),
                store_ready: true,
                descriptors_persisted: true,
                session_dirty: true,
                dirty_history_from: None,
                descriptor_dirty_from: transcript.history().descriptor_dirty_from(),
                history_len: session.history.len(),
                durable_history_len: session.history.len(),
                blobs_pending: false,
                supports_metadata_only: true,
            },
        });

        assert!(matches!(plan, SessionSavePlan::MetadataOnly { .. }));
    }

    #[test]
    fn full_save_plan_rewrites_history_when_metadata_len_is_stale() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![HistoryItem::user(Content::text("persisted"))];
        let transcript = TranscriptDocument::new();

        let plan = SessionDocument::select_save_plan(SessionSaveInput {
            session: &session,
            history: SessionHistoryRef::Materialized(&session.history),
            transcript: &transcript,
            state: SessionSaveState {
                generation: DocumentGeneration::new(0, 0),
                store_ready: true,
                descriptors_persisted: true,
                session_dirty: true,
                dirty_history_from: None,
                descriptor_dirty_from: transcript.history().descriptor_dirty_from(),
                history_len: session.history.len(),
                durable_history_len: 0,
                blobs_pending: false,
                supports_metadata_only: true,
            },
        });

        assert!(matches!(
            plan,
            SessionSavePlan::History {
                generation: _,
                history_start_idx: 0,
                dirty_history_from: Some(0),
            }
        ));
    }

    #[test]
    fn save_plan_clamps_dirty_marker_to_durable_history_len() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("stored 0")),
            HistoryItem::user(Content::text("stored 1")),
            HistoryItem::user(Content::text("live 2")),
        ];
        let transcript = TranscriptDocument::new();

        let plan = SessionDocument::select_save_plan(SessionSaveInput {
            session: &session,
            history: SessionHistoryRef::Materialized(&session.history),
            transcript: &transcript,
            state: SessionSaveState {
                generation: DocumentGeneration::new(0, 0),
                store_ready: true,
                descriptors_persisted: true,
                session_dirty: true,
                dirty_history_from: Some(5),
                descriptor_dirty_from: transcript.history().descriptor_dirty_from(),
                history_len: session.history.len(),
                durable_history_len: 2,
                blobs_pending: false,
                supports_metadata_only: true,
            },
        });

        assert!(matches!(
            plan,
            SessionSavePlan::History {
                history_start_idx: 2,
                ..
            }
        ));
    }

    #[test]
    fn save_plan_clamps_dirty_marker_to_current_history_len() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("stored 0")),
            HistoryItem::user(Content::text("stored 1")),
        ];
        let transcript = TranscriptDocument::new();

        let plan = SessionDocument::select_save_plan(SessionSaveInput {
            session: &session,
            history: SessionHistoryRef::Materialized(&session.history),
            transcript: &transcript,
            state: SessionSaveState {
                generation: DocumentGeneration::new(0, 0),
                store_ready: true,
                descriptors_persisted: true,
                session_dirty: true,
                dirty_history_from: Some(5),
                descriptor_dirty_from: transcript.history().descriptor_dirty_from(),
                history_len: session.history.len(),
                durable_history_len: 5,
                blobs_pending: false,
                supports_metadata_only: true,
            },
        });

        assert!(matches!(
            plan,
            SessionSavePlan::History {
                history_start_idx: 2,
                ..
            }
        ));
    }

    #[test]
    fn request_history_append_falls_back_when_append_index_is_not_durable_len() {
        let mut persist = SessionPersistState::new();
        persist.install_loaded_store_session(true, 1, 0, 0);
        persist.mark_history_dirty_from(2);
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("stored")),
            HistoryItem::user(Content::text("unsaved")),
            HistoryItem::user(Content::text("request")),
        ];
        let transcript = TranscriptDocument::new();
        let item = session.history[2].clone();

        let prepared = prepare_request_append(persist, &mut session, transcript, None, 2, 0, item)
            .expect("prepare request append fallback");

        assert!(matches!(
            prepared,
            PreparedSessionSave::History { ref delta, .. }
                if delta.history.start.get() == 1 && delta.history.final_len.get() == 3
        ));
    }

    #[test]
    fn full_save_plan_includes_descriptor_suffix() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![HistoryItem::user(Content::text("persisted"))];
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "descriptor-only text".into(),
        });

        let prepared = SessionDocument::prepare_save_from_input(SessionSaveInput {
            session: &session,
            history: SessionHistoryRef::Materialized(&session.history),
            transcript: &transcript,
            state: SessionSaveState {
                generation: DocumentGeneration::new(0, 0),
                store_ready: true,
                descriptors_persisted: true,
                session_dirty: false,
                dirty_history_from: None,
                descriptor_dirty_from: transcript.history().descriptor_dirty_from(),
                history_len: session.history.len(),
                durable_history_len: session.history.len(),
                blobs_pending: false,
                supports_metadata_only: true,
            },
        })
        .expect("prepare save");

        match prepared {
            PreparedSessionSave::History { delta, .. } => {
                assert_eq!(delta.history.start.get(), session.history.len() as u64);
                let descriptors = delta.descriptors.expect("descriptor delta");
                assert_eq!(descriptors.start_descriptor_idx, 0);
                assert_eq!(descriptors.records.len(), 1);
            }
            _ => panic!("expected descriptor history save"),
        }
    }

    #[test]
    fn request_history_append_save_plan_builds_descriptor_and_side_table_suffix() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("persisted")),
            HistoryItem::user(Content::text("new")),
        ];
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "persisted".into(),
        });
        transcript.history_mut().clear_descriptor_dirty();
        let descriptor_order_start = transcript.history().len();
        transcript.push(Block::User {
            text: "new".into(),
            image_labels: Vec::new(),
        });

        let generation = DocumentGeneration::new(3, 4);
        let plan = SessionDocument::build_request_history_append_save(
            &session,
            &transcript,
            generation,
            1,
            descriptor_order_start,
            &HistoryItem::user(Content::text("new")),
            true,
        )
        .expect("prepare request append save");

        let (plan_generation, delta, descriptor_append) =
            (plan.generation, plan.delta, plan.descriptor_append);

        assert_eq!(plan_generation, generation);
        assert_eq!(delta.history.start.get(), 1);
        assert_eq!(delta.history.final_len.get(), 2);
        assert_eq!(delta.history.items.len(), 1);
        assert_eq!(delta.side_tables.start.get(), 1);
        let descriptors = delta.descriptors.as_ref().expect("descriptor delta");
        assert_eq!(descriptors.start_descriptor_idx, 1);
        assert_eq!(descriptor_append.count, 1);
        assert_eq!(descriptors.records.len(), 1);
        assert!(!descriptor_append.had_descriptor_total);
    }

    #[test]
    fn request_history_append_planner_falls_back_to_full_save_when_dirty_range_exists() {
        let mut persist = SessionPersistState::new();
        persist.install_loaded_store_session(true, 1, 0, 0);
        persist.mark_history_dirty_from(0);
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("old")),
            HistoryItem::user(Content::text("new")),
        ];
        let transcript = TranscriptDocument::new();
        let item = session.history[1].clone();

        let prepared = prepare_request_append(persist, &mut session, transcript, None, 1, 0, item)
            .expect("prepare request append fallback");

        assert!(matches!(
            prepared,
            PreparedSessionSave::History { ref delta, .. }
                if delta.history.start.get() == 0 && delta.history.final_len.get() == 2
        ));
    }

    #[test]
    fn request_history_append_planner_falls_back_when_live_dirty_range_exists() {
        let mut persist = SessionPersistState::new();
        persist.install_loaded_store_session(true, 0, 0, 0);
        persist.mark_history_dirty_from(0);
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut live_session = empty_live_session_for(&session, 0);
        live_session.append_history(HistoryItem::user(Content::text("dirty")));
        live_session.append_history(HistoryItem::user(Content::text("new")));
        let transcript = TranscriptDocument::new();
        let item = HistoryItem::user(Content::text("new"));

        let prepared = prepare_request_append(
            persist,
            &mut session,
            transcript,
            Some(live_session),
            1,
            0,
            item,
        )
        .expect("prepare request append fallback");

        assert!(matches!(
            prepared,
            PreparedSessionSave::History { ref delta, .. }
                if delta.history.start.get() == 0 && delta.history.final_len.get() == 2
        ));
    }

    #[test]
    fn request_history_append_planner_builds_current_live_append() {
        let mut persist = SessionPersistState::new();
        persist.install_loaded_store_session(true, 1, 0, 0);
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut live_session = empty_live_session_for(&session, 1);
        let item = HistoryItem::user(Content::text("new"));
        live_session.append_history(item.clone());
        let transcript = TranscriptDocument::new();

        let prepared = prepare_request_append(
            persist,
            &mut session,
            transcript,
            Some(live_session),
            1,
            0,
            item,
        )
        .expect("prepare request append");

        assert!(matches!(
            prepared,
            PreparedSessionSave::RequestHistoryAppend { .. }
        ));
    }

    #[test]
    fn prepared_full_save_builds_history_and_descriptor_delta() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("persisted")),
            HistoryItem::user(Content::text("dirty")),
        ];
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "descriptor-only text".into(),
        });

        let prepared = SessionDocument::prepare_save_from_input(SessionSaveInput {
            session: &session,
            history: SessionHistoryRef::Materialized(&session.history),
            transcript: &transcript,
            state: SessionSaveState {
                generation: DocumentGeneration::new(0, 0),
                store_ready: true,
                descriptors_persisted: true,
                session_dirty: true,
                dirty_history_from: Some(1),
                descriptor_dirty_from: transcript.history().descriptor_dirty_from(),
                history_len: session.history.len(),
                durable_history_len: 1,
                blobs_pending: false,
                supports_metadata_only: true,
            },
        })
        .expect("prepare save");

        match prepared {
            PreparedSessionSave::History {
                generation: _,
                delta,
            } => {
                assert_eq!(delta.history.start.get(), 1);
                assert_eq!(delta.history.final_len.get(), 2);
                assert_eq!(delta.history.items.len(), 1);
                assert_eq!(delta.side_tables.start.get(), 1);
                let descriptors = delta.descriptors.expect("descriptor delta");
                assert_eq!(descriptors.start_descriptor_idx, 0);
                assert_eq!(descriptors.records.len(), 1);
            }
            _ => panic!("expected prepared history save"),
        }
    }

    #[test]
    fn live_save_plan_skips_unchanged_store_backed_session() {
        let transcript = TranscriptDocument::new();
        let session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let live_session = empty_live_session_for(&session, 2);

        let plan = SessionDocument::select_save_plan(SessionSaveInput {
            session: &session,
            history: SessionHistoryRef::StoreBacked(&live_session),
            transcript: &transcript,
            state: SessionSaveState {
                generation: DocumentGeneration::new(0, 0),
                store_ready: true,
                descriptors_persisted: true,
                session_dirty: false,
                dirty_history_from: None,
                descriptor_dirty_from: transcript.history().descriptor_dirty_from(),
                history_len: 2,
                durable_history_len: 2,
                blobs_pending: false,
                supports_metadata_only: false,
            },
        });

        assert!(matches!(
            plan,
            SessionSavePlan::Skip(SessionSaveSkipReason::Unchanged)
        ));
    }

    #[test]
    fn live_session_metadata_mutation_forces_metadata_save() {
        let transcript = TranscriptDocument::new();
        let mut persist = SessionPersistState::new();
        persist.install_loaded_store_session(true, 2, 0, 0);
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut live_session = empty_live_session_for(&session, 2);
        let checkpoint = ContextCheckpoint {
            kind: "compaction".into(),
            summary: "summary".into(),
            first_live_index: 2,
            created_at_ms: 10,
            tokens_before: Some(100),
            tokens_after_estimate: None,
            tokens_after_estimate_history_len: None,
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        };

        let result = SessionDocument::apply(
            SessionMutationTarget::LiveSession {
                session: &mut session,
                live_session: &mut live_session,
            },
            SessionMutation::SetCheckpoint {
                checkpoint: Some(checkpoint),
            },
        );
        persist.record_mutation(result.session_dirty, result.history_dirty_from);

        let plan = SessionDocument::select_save_plan(SessionSaveInput {
            session: &session,
            history: SessionHistoryRef::StoreBacked(&live_session),
            transcript: &transcript,
            state: persist.live_save_state(&transcript, &live_session, false),
        });

        assert!(matches!(
            plan,
            SessionSavePlan::History {
                generation: _,
                history_start_idx: 2,
                dirty_history_from: None,
            }
        ));
    }

    #[test]
    fn live_save_plan_uses_dirty_history_start() {
        let transcript = TranscriptDocument::new();
        let session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let live_session = empty_live_session_for(&session, 3);

        let plan = SessionDocument::select_save_plan(SessionSaveInput {
            session: &session,
            history: SessionHistoryRef::StoreBacked(&live_session),
            transcript: &transcript,
            state: SessionSaveState {
                generation: DocumentGeneration::new(0, 0),
                store_ready: true,
                descriptors_persisted: true,
                session_dirty: false,
                dirty_history_from: Some(1),
                descriptor_dirty_from: transcript.history().descriptor_dirty_from(),
                history_len: 3,
                durable_history_len: 3,
                blobs_pending: false,
                supports_metadata_only: false,
            },
        });

        assert!(matches!(
            plan,
            SessionSavePlan::History {
                generation: _,
                history_start_idx: 1,
                dirty_history_from: Some(1),
            }
        ));
    }

    #[test]
    fn prepared_live_save_builds_history_delta_from_live_session() {
        let session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut meta = meta_with_token_identity();
        meta.id = session.id.clone();
        meta.history_len = Some(0);
        let header = SessionHeader {
            meta,
            history_len: 0,
            revision: 0,
        };
        let mut live_session = LiveSession::from_parts(header, std::path::PathBuf::new(), None);
        live_session.append_history(HistoryItem::user(Content::text("live")));
        let transcript = TranscriptDocument::new();

        let prepared = SessionDocument::prepare_save_from_input(SessionSaveInput {
            session: &session,
            history: SessionHistoryRef::StoreBacked(&live_session),
            transcript: &transcript,
            state: SessionSaveState {
                generation: DocumentGeneration::new(0, 0),
                store_ready: true,
                descriptors_persisted: true,
                session_dirty: false,
                dirty_history_from: Some(0),
                descriptor_dirty_from: transcript.history().descriptor_dirty_from(),
                history_len: live_session.history_len(),
                durable_history_len: 0,
                blobs_pending: false,
                supports_metadata_only: false,
            },
        })
        .expect("prepare live save");

        match prepared {
            PreparedSessionSave::History {
                generation: _,
                delta,
            } => {
                assert_eq!(delta.history.start.get(), 0);
                assert_eq!(delta.history.final_len.get(), 1);
                assert_eq!(delta.history.items.len(), 1);
                assert_eq!(delta.side_tables.start.get(), 0);
                assert!(delta.descriptors.is_none());
            }
            _ => panic!("expected prepared live history save"),
        }
    }

    #[test]
    fn save_ack_plan_marks_matching_history_generation_clean() {
        let plan = SessionDocument::plan_save_ack(
            DocumentGeneration::new(3, 4),
            DocumentGeneration::new(3, 4),
            crate::persist::PersistSaveKind::History,
        );

        assert_eq!(
            plan,
            SaveAckPlan::MarkClean {
                clear_descriptors: true
            }
        );
    }

    #[test]
    fn save_ack_plan_queues_mismatched_generation() {
        let plan = SessionDocument::plan_save_ack(
            DocumentGeneration::new(3, 4),
            DocumentGeneration::new(4, 4),
            crate::persist::PersistSaveKind::Metadata,
        );

        assert_eq!(plan, SaveAckPlan::SaveAgain);
    }

    #[test]
    fn persist_state_ack_clears_matching_generation() {
        let mut state = SessionPersistState::new();
        state.mark_history_dirty_from(2);
        state.descriptors_persisted = false;
        let generation = DocumentGeneration::new(state.dirty_generation, 5);
        let save_id = 1;
        state.set_pending_save_submission_for_test(
            save_id,
            "session-a".into(),
            crate::persist::PersistSaveKind::History,
            generation,
            SubmittedHistoryRange {
                start_idx: 2,
                len: 3,
            },
            None,
        );

        let plan = state.ack_save(&save_receipt(save_id, 3, 7), generation);

        assert_eq!(
            plan,
            Some(smelt_core::session_save::SaveAckOutcome {
                plan: SaveAckPlan::MarkClean {
                    clear_descriptors: true
                },
                kind: crate::persist::PersistSaveKind::History,
                descriptor_append: None,
            })
        );
        assert!(!state.session_dirty);
        assert_eq!(state.dirty_history_from, None);
        assert_eq!(state.durable.store_history_len, 3);
        assert!(state.descriptors_persisted);
    }

    #[test]
    fn persist_state_ack_with_unexpected_history_len_forces_retry() {
        let mut state = SessionPersistState::new();
        state.install_loaded_store_session(true, 1, 0, 7);
        let save_id = 1;
        state.set_pending_save_submission_for_test(
            save_id,
            "session-a".into(),
            crate::persist::PersistSaveKind::History,
            DocumentGeneration::new(0, 0),
            SubmittedHistoryRange {
                start_idx: 1,
                len: 2,
            },
            None,
        );

        let plan = state.ack_save(&save_receipt(save_id, 1, 8), DocumentGeneration::new(0, 0));

        assert_eq!(
            plan,
            Some(smelt_core::session_save::SaveAckOutcome {
                plan: SaveAckPlan::SaveAgain,
                kind: crate::persist::PersistSaveKind::History,
                descriptor_append: None,
            })
        );
        assert!(state.is_save_queued());
        assert_eq!(state.dirty_history_from, Some(1));
        assert_eq!(
            state.durable,
            DurableCursor {
                store_history_len: 1,
                descriptor_len: 0,
                revision: 7,
            }
        );
    }

    #[test]
    fn document_ack_clears_matching_descriptor_generation() {
        let mut state = SessionPersistState::new();
        state.mark_history_dirty_from(0);
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "dirty descriptor".into(),
        });
        let generation = state.current_generation(&transcript);
        let save_id = 1;
        state.set_pending_save_submission_for_test(
            save_id,
            "session-a".into(),
            crate::persist::PersistSaveKind::History,
            generation,
            SubmittedHistoryRange {
                start_idx: 0,
                len: 1,
            },
            None,
        );

        let save_queued = SessionDocument::mark_persisted(
            &mut state,
            &mut transcript,
            None,
            None,
            &save_receipt(save_id, 1, 7),
        );

        assert!(!save_queued);
        assert!(!state.session_dirty);
        assert_eq!(state.dirty_history_from, None);
        assert_eq!(transcript.history().descriptor_dirty_from(), None);
    }

    #[test]
    fn document_ack_records_request_descriptor_append() {
        let mut state = SessionPersistState::new();
        state.mark_history_dirty_from(0);
        let descriptor = smelt_store::TranscriptDescriptorRecord {
            block_idx: 0,
            history_idx: None,
            kind: "text".into(),
            tool_call_id: None,
            tool_name: None,
            content_hash: "old".into(),
            estimated_text_bytes: 3,
            preview_text: "old".into(),
            indexed_text: "old".into(),
            descriptor_json: serde_json::to_string(&smelt_core::TranscriptBlockDescriptor::Text {
                content: "old".into(),
            })
            .unwrap(),
            origin_json: None,
            tool_state_json: None,
        };
        let loaded = crate::app::transcript::LoadedTranscript::from_descriptor_slice(
            smelt_store::TranscriptDescriptorSlice::new(
                smelt_store::TranscriptDescriptorIndex::new(0),
                1,
                smelt_store::TranscriptDescriptorHydration::Hydrated,
                vec![descriptor],
            ),
            std::path::PathBuf::new(),
        )
        .expect("loaded transcript");
        let mut transcript = TranscriptDocument::from_loaded_transcript(loaded);
        transcript.push(Block::Text {
            content: "new descriptor".into(),
        });
        let before_ack_total = transcript
            .descriptor_total_count()
            .expect("descriptor total");
        let generation = state.current_generation(&transcript);
        let save_id = 1;
        state.set_pending_save_submission_for_test(
            save_id,
            "session-a".into(),
            crate::persist::PersistSaveKind::History,
            generation,
            SubmittedHistoryRange {
                start_idx: 1,
                len: 2,
            },
            Some(DescriptorAppendSubmission {
                count: 1,
                had_descriptor_total: true,
            }),
        );

        let save_queued = SessionDocument::mark_persisted(
            &mut state,
            &mut transcript,
            None,
            None,
            &save_receipt(save_id, 2, 7),
        );

        assert!(!save_queued);
        assert_eq!(
            transcript.descriptor_total_count(),
            Some(before_ack_total.saturating_add(1))
        );
        assert!(!state.is_save_queued());
    }

    #[test]
    fn persist_state_ack_queues_mismatched_generation() {
        let mut state = SessionPersistState::new();
        state.mark_session_dirty();
        let save_id = 1;
        state.set_pending_save_submission_for_test(
            save_id,
            "session-a".into(),
            crate::persist::PersistSaveKind::Metadata,
            DocumentGeneration::new(state.dirty_generation, 1),
            SubmittedHistoryRange {
                start_idx: 0,
                len: 0,
            },
            None,
        );
        state.mark_session_dirty();

        let plan = state.ack_save(
            &save_receipt(save_id, 0, 7),
            DocumentGeneration::new(state.dirty_generation, 1),
        );

        assert_eq!(
            plan,
            Some(smelt_core::session_save::SaveAckOutcome {
                plan: SaveAckPlan::SaveAgain,
                kind: crate::persist::PersistSaveKind::Metadata,
                descriptor_append: None,
            })
        );
        assert!(state.save_pending);
        assert!(state.session_dirty);
    }

    #[test]
    fn unrecoverable_document_failure_forgets_pending_save_without_retry() {
        let session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut live_session = empty_live_session_for(&session, 3);
        let mut state = SessionPersistState::new();
        state.install_loaded_store_session(true, 3, 0, 7);
        let save_id = 1;
        state.set_pending_save_submission_for_test(
            save_id,
            "session-a".into(),
            crate::persist::PersistSaveKind::History,
            DocumentGeneration::new(0, 0),
            SubmittedHistoryRange {
                start_idx: 0,
                len: 3,
            },
            None,
        );

        let mut transcript = TranscriptDocument::new();
        SessionDocument::mark_persist_failed(
            &mut state,
            &mut transcript,
            Some(&mut live_session),
            &crate::persist::PersistFailure {
                save_id,
                session_id: "session-a".into(),
                message: "disk full".into(),
                commit_failure: None,
            },
        );

        assert!(!state.has_pending_save());
        assert!(!state.is_save_queued());
        assert!(state.session_dirty);
        assert_eq!(state.dirty_history_from, Some(0));
        assert!(state.store_ready);
        assert!(state.descriptors_persisted);
        assert_eq!(
            state.durable,
            DurableCursor {
                store_history_len: 3,
                descriptor_len: 0,
                revision: 7,
            }
        );
    }

    #[test]
    fn document_failure_reconciles_structured_stale_descriptor_base() {
        let mut state = SessionPersistState::new();
        state.install_loaded_store_session(true, 1, 303, 7);
        let save_id = 1;
        state.set_pending_save_submission_for_test(
            save_id,
            "session-a".into(),
            crate::persist::PersistSaveKind::History,
            DocumentGeneration::new(0, 0),
            SubmittedHistoryRange {
                start_idx: 1,
                len: 1,
            },
            None,
        );
        let mut transcript = TranscriptDocument::new();

        SessionDocument::mark_persist_failed(
            &mut state,
            &mut transcript,
            None,
            &crate::persist::PersistFailure {
                save_id,
                session_id: "session-a".into(),
                message: "save session database: stale descriptor base".into(),
                commit_failure: Some(smelt_store::SessionCommitFailure::StaleDescriptorBase {
                    base: smelt_store::DescriptorLen::new(303),
                    current: smelt_store::DescriptorLen::new(111),
                }),
            },
        );

        assert_eq!(state.durable.descriptor_len, 111);
        assert_eq!(transcript.descriptor_total_count(), Some(111));
        assert!(state.is_save_queued());
    }

    #[test]
    fn document_dirty_state_reports_unflushed_work() {
        assert!(!SessionDocument::has_unflushed_work(
            DocumentDirtyState::default()
        ));
        assert!(SessionDocument::has_unflushed_work(DocumentDirtyState {
            pending_or_queued_save: true,
            ..Default::default()
        }));
        assert!(SessionDocument::has_unflushed_work(DocumentDirtyState {
            session_dirty: true,
            ..Default::default()
        }));
        assert!(SessionDocument::has_unflushed_work(DocumentDirtyState {
            descriptor_dirty_from: Some(0),
            ..Default::default()
        }));
        assert!(SessionDocument::has_unflushed_work(DocumentDirtyState {
            store_ready: false,
            history_len: 1,
            ..Default::default()
        }));
        assert!(SessionDocument::has_unflushed_work(DocumentDirtyState {
            descriptors_persisted: false,
            transcript_empty: false,
            ..Default::default()
        }));
    }
}
