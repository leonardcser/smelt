use std::sync::{Arc, Mutex};

use super::stream_reveal::{Baseline, StreamReveal};

use super::{
    agent::TurnLifecycle, session_document::TuiSessionDocument, transcript::ResumePreviewCache,
    CommittedTranscriptView, SessionAccess, SessionPersistence, SharedSessionState,
};

/// What changed in the session writer since the last report.
#[derive(Default)]
pub(crate) struct PersistenceReport {
    /// The writer finished opening the session, or failed to.
    pub(crate) startup: Option<Result<(), crate::persist::PersistenceCause>>,
    /// More saves became durable.
    pub(crate) durable: bool,
    pub(crate) failure: Option<crate::persist::PersistenceCause>,
    pub(crate) audit_warning: Option<String>,
}

impl PersistenceReport {
    fn is_empty(&self) -> bool {
        self.startup.is_none()
            && !self.durable
            && self.failure.is_none()
            && self.audit_warning.is_none()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SaveStatus {
    SkippedReadOnly,
    DeferredHydration,
    DeferredStartup,
    Unchanged,
    DurableEphemeral,
    Submitted,
}

/// Which channel of the response a delta streams into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamKind {
    Text,
    Thinking,
}

/// A stream whose newest glyphs the transcript reveal paces.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RevealKey {
    /// The response's text and raw thinking.
    Response,
    /// A reasoning summary part, by id.
    Summary(String),
    /// A tool call's streamed arguments, by stream id.
    ToolDraft(String),
    Compaction,
}

/// A reasoning summary part still receiving deltas.
#[derive(Default)]
struct SummaryDraft {
    text: String,
    /// Deltas arrived since the part was last rendered as finished.
    streaming: bool,
    title: Option<String>,
    previous: Option<super::transcript::ReasoningSummarySnapshot>,
}

struct SummaryPart {
    id: String,
    block_id: Option<smelt_transcript::transcript_model::BlockId>,
    after: Option<smelt_transcript::transcript_model::BlockId>,
    draft: Option<SummaryDraft>,
}

/// Owns canonical conversation state and the runtime machinery that keeps its
/// session, transcript, turn, and durable store projections aligned.
pub(crate) struct ConversationRuntime {
    session: smelt_core::session::Session,
    document: TuiSessionDocument,
    committed_transcript_view: Option<CommittedTranscriptView>,
    parser: smelt_transcript::content::stream_parser::StreamParser,
    /// Paces the newest glyphs of streamed transcript blocks on screen.
    reveal: StreamReveal<RevealKey>,
    /// The channel the response streamed into last, while its reveal runs.
    streaming_kind: Option<StreamKind>,
    // Keep completed slots until response acceptance so delayed parts retain their position.
    summary_parts: Vec<SummaryPart>,
    compaction_source: String,
    response_draft_blocks: Vec<smelt_transcript::transcript_model::BlockId>,
    /// Earliest canonical suffix whose transcript projection must be rebuilt
    /// after live stream-parser blocks finish.
    pending_transcript_history_rebuild_from: Option<usize>,
    resume_preview_cache: ResumePreviewCache,
    shared_session: Arc<Mutex<Option<SharedSessionState>>>,
    turn: TurnLifecycle,
    pub(super) submissions: super::agent::TurnSubmissions,
    persistence: Option<crate::persist::SessionPersistence>,
    /// Raised by every persistence actor this conversation spawns, so the
    /// event loop wakes to apply their acknowledgements.
    persistence_wake: Arc<tokio::sync::Notify>,
    /// The last save failure; automatic saves wait for an explicit retry.
    save_failure: Option<crate::persist::PersistenceCause>,
    observed_audit_warning: Option<String>,
    /// What the writer reported since the app last drained it.
    pending_report: PersistenceReport,
    access: SessionAccess,
    ownership_recovery: super::session_load::OwnershipRecovery,
    sessions: smelt_core::session::SessionStorage,
    storage: SessionPersistence,
}

impl ConversationRuntime {
    pub(crate) fn new(
        session: smelt_core::session::Session,
        mut transcript: super::transcript::TranscriptDocument,
        resume_preview_cache: ResumePreviewCache,
        shared_session: Arc<Mutex<Option<SharedSessionState>>>,
        turn: TurnLifecycle,
        sessions: smelt_core::session::SessionStorage,
        storage: SessionPersistence,
    ) -> Self {
        transcript.set_store_address(storage.transcript_store_address(&sessions, &session));
        let mut document = TuiSessionDocument::new(transcript);
        document.navigation =
            smelt_core::session_runtime::HistoryNavigation::from_history(&session.history);
        Self {
            session,
            document,
            committed_transcript_view: None,
            parser: smelt_transcript::content::stream_parser::StreamParser::new(),
            reveal: StreamReveal::default(),
            streaming_kind: None,
            summary_parts: Default::default(),
            compaction_source: String::new(),
            response_draft_blocks: Vec::new(),
            pending_transcript_history_rebuild_from: None,
            resume_preview_cache,
            shared_session,
            turn,
            submissions: Default::default(),
            persistence: None,
            persistence_wake: Arc::default(),
            save_failure: None,
            observed_audit_warning: None,
            pending_report: PersistenceReport::default(),
            access: SessionAccess::Owned,
            ownership_recovery: Default::default(),
            sessions,
            storage,
        }
    }

    pub(crate) fn session(&self) -> &smelt_core::session::Session {
        &self.session
    }

    pub(crate) fn install_startup_runtime(&mut self, runtime: &smelt_core::RuntimeState) {
        self.session.mode = Some(runtime.mode.as_str().to_string());
        self.session.reasoning_effort = Some(runtime.reasoning_effort.clone());
        self.session.model = runtime.active_model().map(|model| model.key.clone());
        self.session.fast_mode = Some(runtime.settings.fast_mode);
        self.turn.set_applied_mode(runtime.mode.clone());
        self.turn
            .set_applied_reasoning_effort(runtime.reasoning_effort.clone());
    }

    pub(crate) fn transcript(&self) -> &super::transcript::TranscriptDocument {
        &self.document.transcript
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn active_tool_block_id(
        &self,
        invocation_id: protocol::InvocationId,
    ) -> Option<smelt_transcript::transcript_model::BlockId> {
        self.parser.active_tool_block_id(invocation_id)
    }

    pub(crate) fn active_tool_output_content(
        &self,
        invocation_id: protocol::InvocationId,
    ) -> Option<smelt_transcript::transcript_content::TranscriptContent> {
        let block_id = self.parser.active_tool_block_id(invocation_id)?;
        self.document
            .transcript
            .history()
            .tool_state(block_id)?
            .output
            .as_ref()
            .map(|output| output.content.clone())
    }

    pub(crate) fn defer_last_reasoning_summary_hydration(
        &mut self,
    ) -> Option<smelt_transcript::transcript_model::BlockId> {
        self.document
            .transcript
            .defer_last_reasoning_summary_hydration()
    }

    pub(crate) fn deferred_engine_block_is_ready(
        &self,
        id: smelt_transcript::transcript_model::BlockId,
    ) -> bool {
        self.document.transcript.deferred_engine_block_is_ready(id)
    }

    pub(crate) fn deferred_engine_block_failed(
        &self,
        id: smelt_transcript::transcript_model::BlockId,
    ) -> bool {
        self.document.transcript.deferred_engine_block_failed(id)
    }

    pub(crate) fn release_deferred_engine_block(
        &mut self,
        id: smelt_transcript::transcript_model::BlockId,
    ) {
        self.document.transcript.release_deferred_engine_block(id);
    }

    pub(crate) fn release_all_deferred_engine_blocks(&mut self) {
        self.document
            .transcript
            .release_all_deferred_engine_blocks();
    }

    pub(crate) fn promote_last_reasoning_summary(
        &mut self,
    ) -> Option<super::transcript::ReasoningSummarySnapshot> {
        let id = self.document.transcript.history().last_block_id()?;
        if !self.response_draft_blocks.contains(&id) {
            return None;
        }
        self.document.transcript.promote_last_reasoning_summary()
    }

    pub(crate) fn transcript_compaction_preview_id(
        &self,
    ) -> Option<smelt_transcript::transcript_model::BlockId> {
        self.document.transcript.compaction_preview_id()
    }

    pub(super) fn set_transcript_search_hydration_pin(
        &mut self,
        block_id: Option<smelt_transcript::transcript_model::BlockId>,
    ) {
        self.document.transcript.set_search_hydration_pin(block_id);
    }

    pub(crate) fn transcript_row_for_search_position(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        viewport_rows: u16,
        anchor: super::transcript::TranscriptSearchAnchor,
    ) -> Option<crate::smelt_edit::RowIndex> {
        self.document
            .transcript
            .row_for_search_position(lua, width, viewport_rows, anchor)
    }

    pub(crate) fn activate_transcript_search_record_window(
        &mut self,
        width: u16,
        block_idx: u64,
        viewport_rows: u16,
    ) -> bool {
        self.document
            .transcript
            .activate_record_window_for_block_idx(width, block_idx, viewport_rows)
    }

    pub(crate) fn transcript_search_block_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        forward: bool,
    ) -> Option<smelt_transcript::transcript_model::BlockId> {
        self.document
            .transcript
            .block_id_at_or_before_row(lua, width, row, forward)
    }

    pub(crate) fn materialize_transcript_search_layout(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        block_indices: Option<&[u64]>,
    ) -> smelt_view::transcript_buf::TranscriptSearchLayout {
        match block_indices {
            Some(block_indices) => self
                .document
                .transcript
                .materialize_exact_loaded_search_layout_for_blocks(lua, width, block_indices),
            None => self
                .document
                .transcript
                .materialize_exact_loaded_search_layout(lua, width),
        }
    }

    pub(crate) fn transcript_search_total_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
    ) -> crate::smelt_edit::RowIndex {
        self.document.transcript.total_rows(lua, width)
    }

    pub(crate) fn transcript_search_anchor_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> super::transcript::TranscriptSearchAnchor {
        self.document
            .transcript
            .search_anchor_at_row(lua, width, row)
    }

    pub(crate) fn transcript_search_matches_for_row_range(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        theme: &crate::smelt_edit::Theme,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
        query: &str,
    ) -> Vec<super::transcript::TranscriptSearchMatch> {
        self.document
            .transcript
            .search_matches_for_row_range(lua, width, theme, start, count, query)
    }

    pub(crate) fn build_transcript_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        theme: &crate::smelt_edit::Theme,
    ) -> std::sync::Arc<Vec<String>> {
        self.document.transcript.build_rows(lua, width, theme)
    }

    pub(super) fn transcript_position_anchor(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        position: crate::smelt_edit::DocPosition,
    ) -> super::transcript::TranscriptPositionAnchor {
        self.document
            .transcript
            .position_anchor(lua, width, position)
    }

    pub(super) fn resolve_transcript_position_anchor(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        anchor: super::transcript::TranscriptPositionAnchor,
    ) -> crate::smelt_edit::DocPosition {
        self.document
            .transcript
            .resolve_position_anchor(lua, width, anchor)
    }

    pub(super) fn projected_transcript_search_match_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        viewport_rows: u16,
        matched: super::transcript::TranscriptSearchMatch,
    ) -> Option<crate::smelt_edit::RowIndex> {
        self.document
            .transcript
            .projected_search_match_row(lua, width, viewport_rows, matched)
    }

    pub(super) fn transcript_search_range_anchor(
        &mut self,
        matched: super::transcript::TranscriptSearchMatch,
        query: String,
    ) -> super::transcript::TranscriptSearchRangeAnchor {
        self.document.transcript.search_range_anchor(matched, query)
    }

    pub(super) fn resolve_transcript_search_range_anchor(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        theme: &crate::smelt_edit::Theme,
        anchor: super::transcript::TranscriptSearchRangeAnchor,
    ) -> super::transcript::TranscriptSearchMatch {
        self.document
            .transcript
            .resolve_search_range_anchor(lua, width, theme, anchor)
    }

    pub(crate) fn transcript_node_metadata_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<smelt_view::transcript_buf::TranscriptNodeRow> {
        self.document
            .transcript
            .node_metadata_at_row(lua, width, row)
    }

    pub(crate) fn fold_transcript_node_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        action: smelt_view::transcript_buf::FoldAction,
        activation: smelt_view::transcript_buf::FoldActivation,
    ) -> bool {
        self.document
            .transcript
            .fold_node_at_row(lua, width, row, action, activation)
    }

    pub(crate) fn fold_transcript_node(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        id: smelt_view::transcript_scene::RenderNodeId,
        action: smelt_view::transcript_buf::FoldAction,
    ) -> bool {
        self.document.transcript.fold_node(lua, width, id, action)
    }

    pub(crate) fn fold_all_transcript_nodes(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        action: smelt_view::transcript_buf::FoldAction,
    ) -> bool {
        self.document.transcript.fold_all(lua, width, action)
    }

    pub(crate) fn fold_transcript_block_kind(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        kind: &str,
        action: smelt_view::transcript_buf::FoldAction,
    ) -> bool {
        self.document
            .transcript
            .fold_block_kind(lua, width, kind, action)
    }

    pub(crate) fn materialize_transcript_block_snapshots(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
    ) -> Vec<super::transcript::TranscriptBlockSnapshot> {
        self.document
            .transcript
            .materialize_block_snapshots(lua, width)
    }

    pub(crate) fn transcript_block_jump(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        from: super::transcript::TranscriptNavigationOrigin,
        roles: &[&str],
        direction: super::transcript::TranscriptNavigationDirection,
        count: usize,
    ) -> Option<super::transcript::TranscriptNavigationBlock> {
        self.document
            .transcript
            .block_jump(lua, width, from, roles, direction, count)
    }

    pub(crate) fn transcript_record_block_reveal_position(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        record_index: usize,
        row_offset: crate::smelt_edit::RowIndex,
        viewport_rows: u16,
    ) -> Option<super::transcript::TranscriptBlockRevealPosition> {
        self.document.transcript.record_block_reveal_position(
            lua,
            width,
            record_index,
            row_offset,
            viewport_rows,
        )
    }

    pub(crate) fn drain_finished_transcript_blocks(
        &mut self,
    ) -> Vec<smelt_transcript::transcript_model::BlockId> {
        self.document.transcript.drain_finished_blocks()
    }

    pub(crate) fn with_transcript_display_document<R>(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        theme: &crate::smelt_edit::Theme,
        f: impl FnOnce(&mut dyn crate::smelt_edit::DisplayDocument) -> R,
    ) -> R {
        let mut document = super::transcript::TranscriptDisplayDocument::new(
            &mut self.document.transcript,
            lua,
            width,
            theme,
        );
        f(&mut document)
    }

    pub(super) fn trace_retained_transcript_frame(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        viewport_rows: u16,
    ) {
        self.document
            .transcript
            .trace_retained_view_frame(lua, width, viewport_rows);
    }

    pub(super) fn reanchor_retained_transcript_viewport(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        viewport_rows: u16,
        scroll_top: crate::smelt_edit::RowIndex,
    ) {
        self.document
            .transcript
            .reanchor_retained_viewport(lua, width, viewport_rows, scroll_top);
    }

    pub(super) fn prepare_transcript_window(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        theme: &std::sync::Arc<crate::smelt_edit::Theme>,
        ui: &mut crate::smelt_edit::Ui,
        request: crate::smelt_edit::MaterializeRequest,
        search_projection: super::render_loop::TranscriptSearchProjection<'_>,
    ) {
        super::render_loop::prepare_transcript_window(
            &mut self.document.transcript,
            lua,
            theme,
            ui,
            request,
            search_projection,
        );
    }

    pub(super) fn transcript_hydration_context_id(&self) -> u64 {
        self.document.transcript.hydration_context_id()
    }

    #[cfg(test)]
    pub(crate) fn invalidate_transcript_layout_for_test(&mut self) {
        self.document.transcript.invalidate_layout_for_test();
    }

    pub(super) fn transcript_hydration_is_pending(&self) -> bool {
        self.document.transcript.hydration_is_pending()
    }

    pub(super) fn take_pending_transcript_hydration_request(
        &mut self,
    ) -> Option<super::transcript_hydration::TranscriptHydrationRequest> {
        self.document.transcript.take_pending_hydration_request()
    }

    pub(super) fn install_transcript_hydration_result(
        &mut self,
        result: super::transcript_hydration::TranscriptHydrationWorkerResult,
    ) -> bool {
        self.document.transcript.install_hydration_result(result)
    }

    pub(super) fn loaded_transcript_block_ids(
        &self,
    ) -> Vec<smelt_transcript::transcript_model::BlockId> {
        self.document.transcript.history().order.clone()
    }

    pub(super) fn pin_deferred_transcript_operation(
        &mut self,
        ids: &[smelt_transcript::transcript_model::BlockId],
    ) {
        self.document.transcript.pin_deferred_operation_blocks(ids);
    }

    pub(super) fn deferred_transcript_operation_is_ready(
        &self,
        ids: &[smelt_transcript::transcript_model::BlockId],
    ) -> bool {
        self.document
            .transcript
            .deferred_operation_blocks_are_ready(ids)
    }

    pub(super) fn deferred_transcript_operation_failed(
        &self,
        ids: &[smelt_transcript::transcript_model::BlockId],
    ) -> bool {
        self.document
            .transcript
            .deferred_operation_blocks_failed(ids)
    }

    pub(super) fn request_deferred_transcript_operation(
        &mut self,
        ids: &[smelt_transcript::transcript_model::BlockId],
    ) {
        self.document
            .transcript
            .request_deferred_operation_blocks(ids);
    }

    pub(super) fn unpin_transcript_operation(
        &mut self,
        ids: &[smelt_transcript::transcript_model::BlockId],
    ) {
        self.document.transcript.unpin_operation_blocks(ids);
    }

    pub(crate) fn transcript_trace_anchor_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> super::transcript_scroll_trace::TranscriptTraceAnchor {
        self.document
            .transcript
            .trace_anchor_at_row(lua, width, row)
    }

    pub(crate) fn record_transcript_scroll_trace_event(
        &mut self,
        kind: impl Into<String>,
        data: serde_json::Value,
    ) {
        self.document
            .transcript
            .record_scroll_trace_event(kind, data);
    }

    pub(crate) fn prime_transcript_local_scroll_base(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        viewport_rows: u16,
        scroll_top: crate::smelt_edit::RowIndex,
    ) {
        self.document
            .transcript
            .prime_local_scroll_base(lua, width, viewport_rows, scroll_top);
    }

    #[cfg(test)]
    pub(crate) fn transcript_extent_store_read_count_for_harness(&self) -> usize {
        self.document
            .transcript
            .extent_store_read_count_for_harness()
    }

    #[cfg(test)]
    pub(crate) fn reset_transcript_projection_counters_for_harness(&mut self) {
        self.document
            .transcript
            .reset_projection_counters_for_harness();
    }

    #[cfg(test)]
    pub(crate) fn transcript_projection_counters_for_harness(
        &self,
    ) -> smelt_view::transcript_buf::TranscriptProjectionCounters {
        self.document.transcript.projection_counters_for_harness()
    }

    pub(crate) fn set_pending_transcript_projection(
        &mut self,
        intent: super::transcript_scroll_trace::TranscriptScrollIntent,
        restore: super::transcript::TranscriptProjectionRestore,
        local_scroll_top: Option<crate::smelt_edit::RowIndex>,
        hint: Option<super::transcript::TranscriptProjectionHint>,
    ) -> super::transcript_scroll_trace::TranscriptScrollIntent {
        self.document.transcript.set_pending_projection_with_hint(
            intent,
            restore,
            local_scroll_top,
            hint,
        )
    }

    pub(crate) fn defer_transcript_projection_until_hydrated(&mut self) {
        self.document.transcript.defer_projection_until_hydrated();
    }

    pub(crate) fn set_next_transcript_scroll_trace_input(
        &mut self,
        input: super::transcript_scroll_trace::TranscriptScrollTraceRenderInput,
    ) {
        self.document.transcript.set_next_scroll_trace_input(input);
    }

    pub(crate) fn clear_pending_transcript_local_scroll(&mut self) {
        self.document.transcript.clear_pending_local_scroll_top();
    }

    pub(crate) fn drain_transcript_compaction_slice(&mut self) -> bool {
        self.document.transcript.drain_compaction_slice()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn replace_transcript_document_for_harness(
        &mut self,
        transcript: super::transcript::TranscriptDocument,
    ) {
        self.document.transcript = transcript;
    }

    #[cfg(test)]
    pub(crate) fn replace_loaded_transcript_for_harness(
        &mut self,
        transcript: super::transcript::LoadedTranscript,
    ) {
        self.document
            .transcript
            .replace_loaded_transcript(transcript);
    }

    #[cfg(any(test, feature = "transcript-bench"))]
    pub(crate) fn ensure_transcript_blocks_hydrated_for_harness(
        &mut self,
        ids: &[smelt_transcript::transcript_model::BlockId],
    ) -> bool {
        self.document.transcript.ensure_hydrated_ids(ids)
    }

    #[cfg(test)]
    pub(crate) fn require_transcript_record_resave_from_for_harness(&mut self, index: usize) {
        self.document
            .transcript
            .history_mut()
            .require_record_resave_from(index);
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn set_transcript_store_for_harness(
        &mut self,
        store: Option<smelt_core::session::SessionStoreAddress>,
    ) {
        self.document.transcript.set_store_address(store);
    }

    #[cfg(test)]
    pub(crate) fn set_transcript_memory_budget_for_harness(
        &mut self,
        budget: super::transcript::TranscriptMemoryBudget,
    ) {
        self.document.transcript.set_memory_budget(budget);
    }

    #[cfg(test)]
    pub(crate) fn drain_transcript_compaction_for_harness(&mut self) {
        while self.document.transcript.drain_compaction_slice() {}
    }

    #[cfg(test)]
    pub(crate) fn transcript_tail_state_for_harness(
        &self,
    ) -> Option<(usize, smelt_transcript::transcript_model::BlockId, bool)> {
        let history = self.document.transcript.history();
        let id = history.last_block_id()?;
        Some((history.len(), id, history.is_materialized(id)))
    }

    #[cfg(test)]
    pub(crate) fn transcript_memory_snapshot_for_harness(
        &self,
    ) -> super::transcript::TranscriptMemorySnapshot {
        self.document.transcript.memory_snapshot()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn set_transcript_scroll_trace_for_harness(&mut self, enabled: bool) {
        self.document.transcript.set_scroll_trace_enabled(enabled);
    }

    #[cfg(test)]
    pub(crate) fn set_transcript_scroll_trace_timings_for_harness(&mut self, enabled: bool) {
        self.document
            .transcript
            .set_scroll_trace_timings_enabled(enabled);
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn take_transcript_scroll_trace_frames_for_harness(
        &mut self,
    ) -> Vec<super::transcript_scroll_trace::TranscriptScrollTraceFrame> {
        self.document.transcript.take_scroll_trace_frames()
    }

    #[cfg(test)]
    pub(crate) fn take_transcript_interaction_trace_events_for_harness(
        &mut self,
    ) -> Vec<super::transcript_scroll_trace::TranscriptInteractionTraceEvent> {
        self.document
            .transcript
            .take_scroll_trace_interaction_events()
    }

    pub(crate) fn with_pinned_transcript_blocks<R>(
        &mut self,
        ids: &[smelt_transcript::transcript_model::BlockId],
        f: impl FnOnce(&smelt_transcript::transcript_model::BlockHistory) -> R,
    ) -> Option<R> {
        if !self.document.transcript.pin_operation_blocks(ids) {
            return None;
        }
        let result = f(self.document.transcript.history());
        self.document.transcript.unpin_operation_blocks(ids);
        Some(result)
    }

    pub(crate) fn live_session(&self) -> Option<&smelt_core::session_runtime::LiveSession> {
        self.document.live_session.as_ref()
    }

    pub(crate) fn has_live_session(&self) -> bool {
        self.document.live_session.is_some()
    }

    pub(crate) fn document_revision(&self) -> u64 {
        self.document.revision()
    }

    pub(crate) fn transcript_records_persisted(&self) -> bool {
        self.document.records_persisted()
    }

    /// The history the model sees: the checkpoint summary, then the history since it.
    pub(crate) fn model_history_source(&self) -> Result<protocol::ModelHistorySource, String> {
        let checkpoint = self.session.checkpoint.as_ref();
        if let Some(live) = self.document.live_session.as_ref() {
            return live.model_history_source(checkpoint);
        }
        let (mut items, first_live_index, end_index) = self.session.model_history_range();
        let coordinates =
            protocol::ModelHistoryCoordinates::projected(items.len(), first_live_index);
        items.extend_from_slice(&self.session.history[first_live_index..end_index]);
        Ok(protocol::ModelHistorySource::projected_items(
            items,
            coordinates,
        ))
    }

    /// The commit that copies this session's unsaved changes into `forked`.
    pub(crate) fn capture_fork_commit(
        &mut self,
        forked: &mut smelt_core::session::Session,
        metadata: super::session_document::RuntimeSessionMetadata,
    ) -> Result<smelt_store::SessionCommit, String> {
        self.document
            .capture_fork_commit(forked, metadata, &self.session.history)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn clear_live_session(&mut self) {
        self.document.live_session = None;
        self.document.navigation =
            smelt_core::session_runtime::HistoryNavigation::from_history(&self.session.history);
    }

    pub(crate) fn history_navigation(&self) -> &smelt_core::session_runtime::HistoryNavigation {
        self.document
            .live_session
            .as_ref()
            .map_or(&self.document.navigation, |live| live.navigation())
    }

    pub(crate) fn install_loaded_full_session(
        &mut self,
        transcript: super::transcript::LoadedTranscript,
        store_head: smelt_store::StoreHead,
    ) {
        self.document
            .install_loaded_full_session(transcript, store_head);
    }

    pub(crate) fn install_loaded_store_session(
        &mut self,
        transcript: super::transcript::LoadedTranscript,
        live_session: smelt_core::session_runtime::LiveSession,
        store_head: smelt_store::StoreHead,
    ) {
        self.document
            .install_loaded_store_session(transcript, live_session, store_head);
    }

    pub(crate) fn install_materialized_transcript(
        &mut self,
        transcript: super::transcript::LoadedTranscript,
        records_persisted: bool,
    ) {
        self.document
            .transcript
            .replace_loaded_transcript(transcript);
        self.document
            .install_materialized_session(records_persisted);
    }

    pub(crate) fn install_rewind_prefix(
        &mut self,
        transcript: super::transcript::LoadedTranscript,
        record_count: usize,
    ) {
        self.document
            .install_rewind_prefix(transcript, record_count);
        self.clear_stream_tools();
    }

    fn apply_history_mutation(
        &mut self,
        mutation: super::session_document::HistoryMutation,
    ) -> super::session_document::DocumentChange {
        let change = self.document.apply_history(&mut self.session, mutation);
        self.load_model_context();
        change
    }

    /// Keeps the model context in memory, so requests do not read storage.
    /// After a rewind or a checkpoint change it may start before the memory
    /// window. A failed read leaves it in storage, where requests read it.
    fn load_model_context(&mut self) {
        let checkpoint = self.session.checkpoint.as_ref();
        let Some(live) = self.document.live_session.as_mut() else {
            return;
        };
        if live
            .load_from(live.model_context_start(checkpoint))
            .is_err()
        {
            smelt_perf::perf::record_value("live_session:context_load_error", 1);
        }
    }

    fn apply_transcript_mutation(
        &mut self,
        mutation: super::session_document::TranscriptMutation,
    ) -> super::session_document::DocumentChange {
        self.document.apply_transcript(mutation)
    }

    fn apply_stream_mutation(
        &mut self,
        mutation: super::session_document::StreamMutation,
    ) -> super::session_document::DocumentChange {
        self.document.apply_stream(&mut self.parser, mutation)
    }

    fn apply_usage_mutation(
        &mut self,
        mutation: super::session_document::UsageMutation,
    ) -> super::session_document::DocumentChange {
        self.document.apply_usage(&mut self.session, mutation)
    }

    fn apply_metadata_mutation(
        &mut self,
        mutation: super::session_document::MetadataMutation,
    ) -> super::session_document::DocumentChange {
        self.document.apply_metadata(&mut self.session, mutation)
    }

    fn apply_turn_state_mutation(
        &mut self,
        mutation: super::session_document::TurnStateMutation,
    ) -> super::session_document::DocumentChange {
        let change = self.document.apply_turn_state(&mut self.session, mutation);
        self.load_model_context();
        change
    }

    fn plan_history_append(
        &self,
        append: &protocol::HistoryAppend,
    ) -> Result<protocol::HistoryAppendPlan, String> {
        if let Some(live) = self.document.live_session.as_ref() {
            live.plan_history_append(append)
        } else {
            protocol::plan_history_append(self.session.history.as_slice(), append)
                .map_err(|never| match never {})
        }
    }

    pub(crate) fn apply_history_append(
        &mut self,
        append: &protocol::HistoryAppend,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> Result<protocol::HistoryAppendResult, String> {
        use protocol::HistoryAppendPlan;

        let plan = self.plan_history_append(append)?;
        let result = match plan {
            HistoryAppendPlan::Unchanged => protocol::HistoryAppendResult::Unchanged,
            HistoryAppendPlan::Push => {
                self.append_history_item(append.item.clone());
                protocol::HistoryAppendResult::Pushed
            }
            HistoryAppendPlan::ReplaceLast | HistoryAppendPlan::RemoveLast => {
                let index = self
                    .history_len()
                    .checked_sub(1)
                    .ok_or_else(|| "tail history mutation requires an existing item".to_string())?;
                self.truncate_history(index, identity)?;
                if plan == HistoryAppendPlan::ReplaceLast {
                    self.append_history_item(append.item.clone());
                }
                plan.result()
            }
        };
        Ok(result)
    }

    pub(crate) fn append_history_item(&mut self, item: protocol::HistoryItem) -> usize {
        self.apply_history_mutation(super::session_document::HistoryMutation::AppendItem { item })
            .history_idx
            .expect("history append returns its canonical index")
    }

    pub(crate) fn commit_request_history_item(
        &mut self,
        item: protocol::HistoryItem,
        block: Option<smelt_transcript::transcript_model::Block>,
        first_user_message: Option<String>,
    ) -> usize {
        self.apply_history_mutation(
            super::session_document::HistoryMutation::CommitRequestItem {
                item,
                block: block.map(Box::new),
                first_user_message,
            },
        )
        .history_idx
        .expect("request history commit returns its canonical index")
    }

    pub(crate) fn history_user_turn_count_before(&self, index: usize) -> Result<usize, String> {
        if let Some(live) = self.live_session() {
            live.user_turn_count_before(index)
        } else {
            Ok(self
                .history_navigation()
                .user_turn_count_before(index.min(self.history_len())))
        }
    }

    pub(crate) fn truncate_history(
        &mut self,
        index: usize,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> Result<Option<protocol::TurnMeta>, String> {
        let user_turn_count = self.history_user_turn_count_before(index)?;
        Ok(self
            .apply_history_mutation(super::session_document::HistoryMutation::TruncateFrom {
                index,
                user_turn_count: Some(user_turn_count),
                identity,
            })
            .turn_meta)
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn rewind_history(
        &mut self,
        index: usize,
        keep_checkpoint_at_boundary: bool,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> Result<Option<protocol::TurnMeta>, String> {
        let count = self.history_user_turn_count_before(index)?;
        Ok(self.rewind_history_with_count(index, count, keep_checkpoint_at_boundary, identity))
    }

    pub(crate) fn rewind_history_with_count(
        &mut self,
        index: usize,
        user_turn_count: usize,
        keep_checkpoint_at_boundary: bool,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> Option<protocol::TurnMeta> {
        self.apply_history_mutation(super::session_document::HistoryMutation::RewindTo {
            index,
            user_turn_count: Some(user_turn_count),
            keep_checkpoint_at_boundary,
            identity,
        })
        .turn_meta
    }

    pub(crate) fn prune_rewindable_state(
        &mut self,
        history_len: usize,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> Option<protocol::TurnMeta> {
        self.apply_turn_state_mutation(
            super::session_document::TurnStateMutation::PruneRewindable {
                history_len,
                identity,
            },
        )
        .turn_meta
    }

    pub(crate) fn update_runtime_metadata(
        &mut self,
        metadata: super::session_document::RuntimeSessionMetadata,
    ) {
        self.apply_metadata_mutation(super::session_document::MetadataMutation::UpdateRuntime {
            updated_at_ms: metadata.updated_at_ms,
            mode: metadata.mode,
            reasoning_effort: metadata.reasoning_effort,
            model: metadata.model,
            fast_mode: metadata.fast_mode,
        });
    }

    pub(crate) fn set_title(&mut self, title: String, slug: String, snapshot_history_len: usize) {
        self.apply_metadata_mutation(super::session_document::MetadataMutation::SetTitle {
            title,
            slug,
            snapshot_history_len,
        });
    }

    pub(crate) fn restore_metadata_after_rewind(&mut self, history_len: usize) {
        self.apply_metadata_mutation(
            super::session_document::MetadataMutation::RestoreAfterRewind { history_len },
        );
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        self.apply_metadata_mutation(super::session_document::MetadataMutation::SetFastMode {
            enabled,
        });
    }

    pub(crate) fn set_cwd(&mut self, cwd: String) {
        self.apply_metadata_mutation(super::session_document::MetadataMutation::SetCwd { cwd });
    }

    pub(crate) fn record_context_tokens(
        &mut self,
        usage: protocol::TokenUsage,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> bool {
        let history_len = self.history_len();
        self.apply_usage_mutation(super::session_document::UsageMutation::RecordTokens {
            usage,
            history_len,
            identity,
        })
        .context_tokens_updated
    }

    pub(crate) fn accumulate_usage(&mut self, usage: protocol::TokenUsage, cost_usd: f64) {
        self.apply_usage_mutation(super::session_document::UsageMutation::Accumulate {
            usage,
            cost_usd,
        });
    }

    pub(crate) fn finish_turn_state(
        &mut self,
        history_len: usize,
        meta: protocol::TurnMeta,
        update_context_token_history_len: bool,
    ) -> bool {
        self.apply_turn_state_mutation(super::session_document::TurnStateMutation::Finish {
            history_len,
            meta,
            update_context_token_history_len,
        })
        .applied
    }

    pub(crate) fn set_checkpoint(
        &mut self,
        checkpoint: Option<smelt_core::session::ContextCheckpoint>,
    ) {
        self.apply_turn_state_mutation(super::session_document::TurnStateMutation::SetCheckpoint {
            checkpoint,
        });
    }

    pub(crate) fn install_checkpoint(
        &mut self,
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    ) -> bool {
        self.apply_turn_state_mutation(
            super::session_document::TurnStateMutation::InstallCheckpoint {
                kind,
                summary,
                first_live_message_index,
                tokens_before,
            },
        )
        .applied
    }

    pub(crate) fn install_checkpoint_at_history_index(
        &mut self,
        kind: String,
        summary: String,
        first_live_index: usize,
        tokens_before: Option<u32>,
        history_len: usize,
    ) -> bool {
        self.apply_turn_state_mutation(
            super::session_document::TurnStateMutation::InstallCheckpointAtHistoryIndex {
                kind,
                summary,
                first_live_index,
                tokens_before,
                history_len,
            },
        )
        .applied
    }

    pub(crate) fn set_checkpoint_tokens_after_estimate(
        &mut self,
        tokens: u32,
        history_len: usize,
    ) -> bool {
        self.apply_turn_state_mutation(
            super::session_document::TurnStateMutation::SetCheckpointTokensAfterEstimate {
                tokens,
                history_len,
            },
        )
        .applied
    }

    pub(super) fn append_accepted_block(
        &mut self,
        block: smelt_transcript::transcript_model::Block,
    ) -> smelt_transcript::transcript_model::BlockId {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::AppendAccepted { block },
        )
        .block_id
        .expect("accepted user block")
    }

    pub(super) fn promote_accepted_block(
        &mut self,
        id: smelt_transcript::transcript_model::BlockId,
        history_index: usize,
    ) -> bool {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::PromoteAccepted { id, history_index },
        )
        .applied
    }

    pub(super) fn remove_accepted_block(
        &mut self,
        id: smelt_transcript::transcript_model::BlockId,
    ) {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::RemoveAccepted { id },
        );
    }

    pub(super) fn remove_submitted_user_block(
        &mut self,
        id: smelt_transcript::transcript_model::BlockId,
        history_index: usize,
    ) {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::RemoveSubmittedUser { id, history_index },
        );
    }

    pub(crate) fn append_block(
        &mut self,
        block: smelt_transcript::transcript_model::Block,
    ) -> bool {
        self.finish_stream_reveal();
        self.apply_transcript_mutation(super::session_document::TranscriptMutation::AppendBlock {
            block,
        })
        .applied
    }

    pub(crate) fn insert_checkpoint_marker(
        &mut self,
        block_index: usize,
        history_index: usize,
        block: smelt_transcript::transcript_model::Block,
    ) {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::InsertCheckpointMarker {
                block_index,
                history_index,
                block,
            },
        );
    }

    pub(crate) fn remove_unoriginated_block(&mut self, block_index: usize) -> bool {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::RemoveUnoriginatedBlockAt { block_index },
        )
        .applied
    }

    pub(crate) fn pending_transcript_history_rebuild_from(&self) -> Option<usize> {
        self.pending_transcript_history_rebuild_from
    }

    pub(crate) fn defer_transcript_history_rebuild_from(&mut self, first_index: usize) {
        self.pending_transcript_history_rebuild_from = Some(
            self.pending_transcript_history_rebuild_from
                .map_or(first_index, |pending| pending.min(first_index)),
        );
    }

    pub(crate) fn clear_pending_transcript_history_rebuild(&mut self) {
        self.pending_transcript_history_rebuild_from = None;
    }

    pub(crate) fn has_live_transcript_blocks(&self) -> bool {
        self.parser.has_live_transcript_blocks()
    }

    pub(crate) fn replace_transcript_from_history(
        &mut self,
        transcript: smelt_transcript::content::transcript::Transcript,
    ) {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::ReplaceFromHistory {
                transcript: Box::new(transcript),
            },
        );
        self.clear_pending_transcript_history_rebuild();
    }

    pub(crate) fn truncate_transcript(&mut self, block_index: usize) {
        self.apply_transcript_mutation(super::session_document::TranscriptMutation::TruncateTo {
            block_index,
        });
    }

    pub(crate) fn clear_transcript(&mut self) {
        self.clear_pending_history_appends();
        self.clear_pending_transcript_history_rebuild();
        self.apply_transcript_mutation(super::session_document::TranscriptMutation::Clear);
        self.clear_stream_parser();
    }

    pub(crate) fn update_compaction_preview(
        &mut self,
        summary: String,
        arrived_at: std::time::Instant,
    ) -> Option<smelt_transcript::transcript_model::BlockId> {
        if summary == self.compaction_source {
            if let Some(id) = self.document.transcript.compaction_preview_id() {
                return Some(id);
            }
        }
        if !summary.starts_with(&self.compaction_source) {
            self.reveal.finish(&RevealKey::Compaction);
        }
        self.compaction_source.clone_from(&summary);
        let existed = self.document.transcript.compaction_preview_id();
        let id = self
            .apply_transcript_mutation(
                super::session_document::TranscriptMutation::UpdateCompactionPreview { summary },
            )
            .block_id?;
        // A new preview's header shows at once; the summary types in below it.
        let baseline = if existed == Some(id) {
            Baseline::Current
        } else {
            Baseline::Header
        };
        self.reveal.track(RevealKey::Compaction, id.get(), baseline);
        self.reveal.arrive(&RevealKey::Compaction, arrived_at);
        Some(id)
    }

    pub(crate) fn clear_compaction_preview(&mut self) {
        self.reveal.finish(&RevealKey::Compaction);
        self.compaction_source.clear();
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::ClearCompactionPreview,
        );
    }

    pub(crate) fn rewrite_block(
        &mut self,
        id: smelt_transcript::transcript_model::BlockId,
        block: smelt_transcript::transcript_model::Block,
    ) -> bool {
        self.apply_transcript_mutation(super::session_document::TranscriptMutation::RewriteBlock {
            id,
            block,
        })
        .applied
    }

    pub(super) fn start_summary_part(&mut self, id: String) -> usize {
        if let Some(index) = self.summary_parts.iter().position(|part| part.id == id) {
            return index;
        }
        self.flush_streaming_thinking();
        let previous = self.promote_last_reasoning_summary().filter(|previous| {
            !self
                .summary_parts
                .iter()
                .any(|part| part.draft.is_some() && part.block_id == Some(previous.id))
        });
        let index = self.summary_parts.len();
        if let Some(previous) = &previous {
            self.reveal.track(
                RevealKey::Summary(id.clone()),
                previous.id.get(),
                Baseline::Current,
            );
        }
        self.summary_parts.push(SummaryPart {
            id,
            block_id: previous.as_ref().map(|previous| previous.id),
            after: self.document.transcript.history().last_block_id(),
            draft: Some(SummaryDraft {
                previous,
                ..Default::default()
            }),
        });
        index
    }

    pub(super) fn append_summary_delta(
        &mut self,
        id: String,
        title: Option<String>,
        delta: String,
        arrived_at: std::time::Instant,
    ) {
        if delta.is_empty() {
            return;
        }
        let key = RevealKey::Summary(id.clone());
        let index = self.start_summary_part(id);
        let mut parts = std::mem::take(&mut self.summary_parts);
        if let Some(draft) = parts[index].draft.as_mut() {
            if title.is_some() {
                draft.title = title;
            }
            draft.text.push_str(&delta);
            draft.streaming = true;
            self.render_summary_draft(&mut parts, index, engine::ReasoningSummaryPhase::Streaming);
        }
        self.summary_parts = parts;
        self.reveal.arrive(&key, arrived_at);
    }

    fn render_summary_draft(
        &mut self,
        parts: &mut [SummaryPart],
        index: usize,
        phase: engine::ReasoningSummaryPhase,
    ) {
        let Some(draft) = parts[index].draft.as_ref() else {
            return;
        };
        let (mut title, body) = engine::normalize_reasoning_summary(&draft.text, phase);
        if phase == engine::ReasoningSummaryPhase::Streaming {
            title = title.or_else(|| draft.title.clone());
        }
        self.render_summary(parts, index, title, &body);
    }

    fn render_summary(
        &mut self,
        parts: &mut [SummaryPart],
        index: usize,
        title: Option<String>,
        body: &str,
    ) {
        let part = &parts[index];
        if part.block_id.is_none() && title.is_none() && body.is_empty() {
            return;
        }
        let previous = part
            .draft
            .as_ref()
            .and_then(|draft| draft.previous.as_ref());
        let block = super::transcript::reasoning_summary_block(previous, title, body);
        if let Some(id) = part.block_id {
            self.rewrite_block(id, block);
        } else {
            let predecessor = parts[..index].iter().rev().find_map(|part| part.block_id);
            let history = self.document.transcript.history();
            let position = [predecessor, part.after]
                .into_iter()
                .flatten()
                .filter_map(|id| history.order.iter().position(|candidate| *candidate == id))
                .max()
                .map_or(0, |index| index + 1);
            let mutation = if let Some(before) = history.order.get(position).copied() {
                super::session_document::TranscriptMutation::InsertBlockBefore { before, block }
            } else {
                super::session_document::TranscriptMutation::AppendBlock { block }
            };
            if let Some(id) = self.apply_transcript_mutation(mutation).block_id {
                parts[index].block_id = Some(id);
                self.response_draft_blocks.push(id);
                if parts[index].draft.is_some() {
                    self.reveal.track(
                        RevealKey::Summary(parts[index].id.clone()),
                        id.get(),
                        Baseline::Empty,
                    );
                }
            }
        }
    }

    pub(super) fn finish_summary_part(
        &mut self,
        id: &str,
        title: Option<&str>,
        content: &str,
    ) -> bool {
        let Some(index) = self.summary_parts.iter().position(|part| part.id == id) else {
            return false;
        };
        let mut parts = std::mem::take(&mut self.summary_parts);
        if parts[index].draft.is_some() {
            self.render_summary(&mut parts, index, title.map(str::to_string), content);
            parts[index].draft = None;
            self.reveal
                .finish(&RevealKey::Summary(parts[index].id.clone()));
        }
        self.summary_parts = parts;
        true
    }

    /// Render the summary parts that received deltas as finished, before a
    /// block that follows them in the transcript. A part stays open for its
    /// own finish event.
    fn finish_summary_parts(&mut self) {
        let mut parts = std::mem::take(&mut self.summary_parts);
        for index in 0..parts.len() {
            let Some(draft) = parts[index].draft.as_mut() else {
                continue;
            };
            if std::mem::take(&mut draft.streaming) {
                self.render_summary_draft(
                    &mut parts,
                    index,
                    engine::ReasoningSummaryPhase::Finished,
                );
            }
        }
        self.summary_parts = parts;
        self.reveal
            .finish_where(|key| matches!(key, RevealKey::Summary(_)));
    }

    pub(super) fn append_stream_delta(
        &mut self,
        kind: StreamKind,
        delta: String,
        arrived_at: std::time::Instant,
    ) {
        use super::session_document::StreamMutation;

        if delta.is_empty() {
            return;
        }
        if kind == StreamKind::Text {
            self.finish_summary_parts();
        }
        if self.streaming_kind.is_some_and(|active| active != kind) {
            self.reveal.finish(&RevealKey::Response);
        }
        self.streaming_kind = Some(kind);
        let first = self.response_draft_boundary();
        self.apply_stream_mutation(match kind {
            StreamKind::Text => StreamMutation::AppendText { delta },
            StreamKind::Thinking => StreamMutation::AppendThinking { delta },
        });
        for &id in self.document.transcript.history().order.iter().skip(first) {
            self.reveal
                .track(RevealKey::Response, id.get(), Baseline::Empty);
        }
        self.reveal.arrive(&RevealKey::Response, arrived_at);
        self.track_response_draft_from(first);
    }

    /// Show everything the response and its summaries streamed so far, before
    /// a block that follows them in the transcript.
    pub(super) fn finish_stream_reveal(&mut self) {
        self.reveal.finish(&RevealKey::Response);
        self.streaming_kind = None;
        self.finish_summary_parts();
    }

    pub(crate) fn reveal_mut(&mut self) -> &mut StreamReveal<RevealKey> {
        &mut self.reveal
    }

    pub(crate) fn flush_streaming_thinking(&mut self) {
        if self.streaming_kind == Some(StreamKind::Thinking) {
            self.finish_stream_reveal();
        }
        self.apply_stream_mutation(super::session_document::StreamMutation::FlushThinking);
    }

    pub(crate) fn flush_streaming_text(&mut self) {
        self.finish_stream_reveal();
        self.finish_tool_draft_reveals();
        self.apply_stream_mutation(super::session_document::StreamMutation::FlushText);
    }

    pub(crate) fn sync_active_tool_elapsed(&mut self, now: std::time::Instant) {
        self.apply_stream_mutation(
            super::session_document::StreamMutation::SyncActiveToolElapsed { now },
        );
    }

    pub(crate) fn start_tool(
        &mut self,
        start: smelt_transcript::content::stream_parser::ToolStart,
        now: std::time::Instant,
    ) {
        self.finish_stream_reveal();
        self.apply_stream_mutation(super::session_document::StreamMutation::StartTool {
            start,
            now,
        });
    }

    pub(super) fn append_tool_output_line(
        &mut self,
        invocation_id: protocol::InvocationId,
        line: String,
    ) -> Option<super::transcript_work::PendingToolOutputAppend> {
        let block_id = self.parser.active_tool_block_id(invocation_id)?;
        if line.len() <= super::transcript_work::STREAM_CONTENT_SLICE_BYTES {
            self.apply_stream_mutation(super::session_document::StreamMutation::AppendToolOutput {
                invocation_id,
                line,
            });
            return None;
        }

        let source = Arc::new(line);
        let end = super::transcript_work::stream_content_slice_end(
            &source,
            0,
            super::transcript_work::STREAM_CONTENT_SLICE_BYTES,
        );
        self.append_tool_output_slice(
            block_id,
            smelt_transcript::transcript_content::SharedContentSlice::new(
                Arc::clone(&source),
                0..end,
            ),
            true,
        );
        (end < source.len()).then(|| {
            super::transcript_work::PendingToolOutputAppend::new(
                block_id,
                invocation_id,
                source,
                end,
                false,
            )
        })
    }

    pub(super) fn advance_tool_output_append(
        &mut self,
        mut pending: super::transcript_work::PendingToolOutputAppend,
    ) -> Option<super::transcript_work::PendingToolOutputAppend> {
        let start = pending.offset;
        let end = super::transcript_work::stream_content_slice_end(
            &pending.source,
            start,
            super::transcript_work::STREAM_CONTENT_SLICE_BYTES,
        );
        let chunk = smelt_transcript::transcript_content::SharedContentSlice::new(
            Arc::clone(&pending.source),
            start..end,
        );
        let line_start = std::mem::take(&mut pending.line_start);
        pending.offset = end;
        self.append_tool_output_slice(pending.block_id, chunk, line_start);
        (end < pending.source.len()).then_some(pending)
    }

    fn append_tool_output_slice(
        &mut self,
        block_id: smelt_transcript::transcript_model::BlockId,
        chunk: smelt_transcript::transcript_content::SharedContentSlice,
        line_start: bool,
    ) {
        self.apply_stream_mutation(
            super::session_document::StreamMutation::AppendToolOutputSlice {
                block_id,
                chunk,
                line_start,
            },
        );
    }

    pub(crate) fn set_tool_status(
        &mut self,
        invocation_id: protocol::InvocationId,
        status: smelt_transcript::transcript_model::ToolStatus,
        now: std::time::Instant,
    ) {
        self.apply_stream_mutation(super::session_document::StreamMutation::SetToolStatus {
            invocation_id,
            status,
            now,
        });
    }

    pub(crate) fn set_tool_user_message(
        &mut self,
        invocation_id: protocol::InvocationId,
        message: String,
    ) {
        self.apply_stream_mutation(
            super::session_document::StreamMutation::SetToolUserMessage {
                invocation_id,
                message,
            },
        );
    }

    pub(crate) fn finish_tool(
        &mut self,
        invocation_id: protocol::InvocationId,
        status: smelt_transcript::transcript_model::ToolStatus,
        output: Option<smelt_transcript::transcript_model::ToolOutputRef>,
        engine_elapsed: Option<std::time::Duration>,
        now: std::time::Instant,
    ) {
        self.apply_stream_mutation(super::session_document::StreamMutation::FinishTool {
            invocation_id,
            status,
            output,
            engine_elapsed,
            now,
        });
    }

    pub(crate) fn finalize_tools(&mut self) {
        self.apply_stream_mutation(super::session_document::StreamMutation::FinalizeTools);
    }

    pub(crate) fn promote_tool_draft(
        &mut self,
        stream_id: Option<String>,
        start: smelt_transcript::content::stream_parser::ToolStart,
        now: std::time::Instant,
    ) -> bool {
        self.finish_stream_reveal();
        if let Some(id) = &stream_id {
            self.reveal.finish(&RevealKey::ToolDraft(id.clone()));
        }
        self.apply_stream_mutation(super::session_document::StreamMutation::PromoteToolDraft {
            stream_id,
            start,
            now,
        })
        .applied
    }

    pub(crate) fn response_draft_boundary(&self) -> usize {
        self.document.transcript.history().order.len()
    }

    pub(crate) fn track_response_draft_from(&mut self, first: usize) {
        self.response_draft_blocks.extend(
            self.document
                .transcript
                .history()
                .order
                .iter()
                .skip(first)
                .copied(),
        );
    }

    pub(crate) fn accept_response_draft(&mut self) {
        self.flush_streaming_text();
        self.summary_parts.clear();
        self.response_draft_blocks.clear();
    }

    pub(crate) fn reject_response_draft(&mut self) {
        self.reveal.finish_all();
        self.streaming_kind = None;
        self.summary_parts.clear();
        self.parser.clear_response_output();
        for id in std::mem::take(&mut self.response_draft_blocks)
            .into_iter()
            .rev()
        {
            let index = self
                .document
                .transcript
                .history()
                .order
                .iter()
                .position(|block| *block == id);
            if let Some(index) = index {
                self.remove_unoriginated_block(index);
            }
        }
    }

    pub(crate) fn clear_stream_tool_drafts(&mut self) {
        self.finish_tool_draft_reveals();
        self.apply_stream_mutation(super::session_document::StreamMutation::ClearToolDrafts);
    }

    pub(crate) fn start_tool_draft(
        &mut self,
        stream_id: String,
        call_id: Option<String>,
        name: Option<String>,
        native: bool,
    ) -> Option<smelt_transcript::transcript_model::BlockId> {
        self.finish_stream_reveal();
        let first = self.response_draft_boundary();
        let id = self
            .apply_stream_mutation(super::session_document::StreamMutation::StartToolDraft {
                stream_id: stream_id.clone(),
                call_id,
                name,
                native,
            })
            .block_id?;
        let baseline = self.tool_draft_baseline(first);
        self.reveal
            .track(RevealKey::ToolDraft(stream_id), id.get(), baseline);
        Some(id)
    }

    pub(crate) fn append_tool_draft(
        &mut self,
        stream_id: String,
        call_id: Option<String>,
        name: Option<String>,
        delta: String,
        arrived_at: std::time::Instant,
    ) -> Option<(smelt_transcript::transcript_model::BlockId, bool)> {
        if delta.is_empty() {
            return None;
        }
        let key = RevealKey::ToolDraft(stream_id.clone());
        if !self.reveal.tracks(&key) {
            self.finish_stream_reveal();
        }
        let first = self.response_draft_boundary();
        let change =
            self.apply_stream_mutation(super::session_document::StreamMutation::AppendToolDraft {
                stream_id,
                call_id,
                name,
                delta,
            });
        let baseline = self.tool_draft_baseline(first);
        self.track_response_draft_from(first);
        let block_id = change.block_id?;
        self.reveal.track(key.clone(), block_id.get(), baseline);
        self.reveal.arrive(&key, arrived_at);
        Some((block_id, change.presentation_changed))
    }

    /// A draft created past response block `first` shows its header at once
    /// and types in its arguments below it.
    fn tool_draft_baseline(&self, first: usize) -> Baseline {
        if first < self.response_draft_boundary() {
            Baseline::Header
        } else {
            Baseline::Current
        }
    }

    fn finish_tool_draft_reveals(&mut self) {
        self.reveal
            .finish_where(|key| matches!(key, RevealKey::ToolDraft(_)));
    }

    pub(crate) fn finish_tool_draft(
        &mut self,
        stream_id: String,
        call_id: String,
        name: String,
        arguments: String,
    ) -> Option<smelt_transcript::transcript_model::BlockId> {
        self.reveal.finish(&RevealKey::ToolDraft(stream_id.clone()));
        self.apply_stream_mutation(super::session_document::StreamMutation::FinishToolDraft {
            stream_id,
            call_id,
            name,
            arguments,
        })
        .block_id
    }

    pub(crate) fn set_tool_draft_summary(
        &mut self,
        block_id: smelt_transcript::transcript_model::BlockId,
        summary: protocol::StyledLines,
    ) {
        self.apply_stream_mutation(
            super::session_document::StreamMutation::SetToolDraftSummary { block_id, summary },
        );
    }

    pub(crate) fn tool_draft_preview(
        &self,
        block_id: smelt_transcript::transcript_model::BlockId,
    ) -> Option<(
        String,
        std::collections::HashMap<String, serde_json::Value>,
        bool,
    )> {
        let smelt_transcript::transcript_model::Block::ToolDraft(draft) =
            self.document.transcript.history().block(block_id)?
        else {
            return None;
        };
        Some((
            draft.name.clone(),
            draft.arguments.preview().clone(),
            draft.finished,
        ))
    }

    pub(crate) fn start_exec(&mut self, command: String) {
        self.finish_stream_reveal();
        self.apply_stream_mutation(super::session_document::StreamMutation::StartExec { command });
    }

    pub(crate) fn append_exec_output(&mut self, chunk: String) {
        self.apply_stream_mutation(super::session_document::StreamMutation::AppendExecOutput {
            chunk,
        });
    }

    pub(crate) fn finish_exec(&mut self, final_output: Option<String>) {
        self.apply_stream_mutation(super::session_document::StreamMutation::FinishExec {
            final_output,
        });
    }

    pub(crate) fn begin_turn(&mut self) {
        self.reveal.finish_all();
        self.streaming_kind = None;
        self.summary_parts.clear();
        self.turn.clear_context_tokens_updated();
        self.response_draft_blocks.clear();
        self.parser.begin_turn();
    }

    pub(crate) fn is_active(&self) -> bool {
        self.turn.is_active()
    }

    pub(crate) fn active(&self) -> Option<&super::TurnState> {
        self.turn.active()
    }

    pub(crate) fn set_active(&mut self, turn: Option<super::TurnState>) {
        self.turn.set_active(turn);
    }

    pub(crate) fn clear_active(&mut self) -> Option<super::TurnState> {
        self.turn.clear_active()
    }

    pub(crate) fn active_id(&self) -> Option<u64> {
        self.turn.active_id()
    }

    pub(crate) fn next_turn_id(&self) -> u64 {
        self.turn.next_turn_id()
    }

    pub(crate) fn last_terminal_turn_id(&self) -> Option<u64> {
        self.turn.last_terminal_turn_id()
    }

    pub(crate) fn mark_terminal(&mut self, turn_id: u64) {
        self.turn.mark_terminal(turn_id);
    }

    pub(crate) fn applied_mode(&self) -> &protocol::AgentMode {
        self.turn.applied_mode()
    }

    pub(crate) fn set_applied_mode(&mut self, mode: protocol::AgentMode) {
        self.turn.set_applied_mode(mode);
    }

    pub(crate) fn applied_reasoning_effort(&self) -> &protocol::ReasoningEffort {
        self.turn.applied_reasoning_effort()
    }

    pub(crate) fn set_applied_reasoning_effort(
        &mut self,
        reasoning_effort: protocol::ReasoningEffort,
    ) {
        self.turn.set_applied_reasoning_effort(reasoning_effort);
    }

    pub(crate) fn active_permissions(
        &self,
        fallback: Arc<smelt_permissions::Permissions>,
    ) -> Arc<smelt_permissions::Permissions> {
        self.turn.active_permissions(fallback)
    }

    pub(crate) fn refresh_active_permissions(
        &mut self,
        permissions: Arc<smelt_permissions::Permissions>,
    ) {
        self.turn.refresh_active_permissions(permissions);
    }

    fn coalesce_context_history_append(
        &mut self,
        append: &super::PendingHistoryAppend,
    ) -> Result<bool, String> {
        if append.context_name().is_none() {
            return Ok(false);
        }

        self.turn.remove_matching_history_append(append);
        let history_append = append.history_append(None);
        let plan = self.plan_history_append(&history_append)?;
        debug_assert!(matches!(
            plan,
            protocol::HistoryAppendPlan::Unchanged | protocol::HistoryAppendPlan::Push
        ));
        if plan == protocol::HistoryAppendPlan::Push {
            self.turn.replace_or_push_history_append(append.clone());
        }
        Ok(true)
    }

    pub(crate) fn queue_history_append(
        &mut self,
        append: super::PendingHistoryAppend,
        mode_base: Option<&protocol::AgentMode>,
    ) -> Result<(), String> {
        if !self.coalesce_context_history_append(&append)? {
            self.turn.queue_history_append(append, mode_base);
        }
        Ok(())
    }

    pub(crate) fn pending_context_note(&self, name: &str) -> Option<Option<&str>> {
        self.turn.pending_context_note(name)
    }

    pub(crate) fn replace_or_push_history_append(
        &mut self,
        append: super::PendingHistoryAppend,
    ) -> Result<(), String> {
        if !self.coalesce_context_history_append(&append)? {
            self.turn.replace_or_push_history_append(append);
        }
        Ok(())
    }

    pub(crate) fn remove_matching_history_append(&mut self, append: &super::PendingHistoryAppend) {
        self.turn.remove_matching_history_append(append);
    }

    pub(crate) fn take_pending_history_appends(&mut self) -> Vec<super::PendingHistoryAppend> {
        self.turn.take_pending_history_appends()
    }

    pub(crate) fn take_pending_follow_up_note(&mut self) -> Option<protocol::HistoryNote> {
        self.turn.take_pending_follow_up_note()
    }

    pub(crate) fn take_matching_history_append(
        &mut self,
        item: &protocol::HistoryItem,
    ) -> Option<super::PendingHistoryAppend> {
        self.turn.take_matching_history_append(item)
    }

    pub(crate) fn clear_pending_history_appends(&mut self) {
        self.turn.clear_pending_history_appends();
    }

    pub(crate) fn retain_session_history_appends(&mut self) {
        self.turn.retain_session_history_appends();
    }

    pub(crate) fn set_pending_meta(&mut self, meta: Option<protocol::TurnMeta>) {
        self.turn.set_pending_meta(meta);
    }

    pub(crate) fn take_pending_meta(&mut self) -> Option<protocol::TurnMeta> {
        self.turn.take_pending_meta()
    }

    pub(crate) fn mark_context_tokens_updated(&mut self) {
        self.turn.mark_context_tokens_updated();
    }

    pub(crate) fn take_context_tokens_updated(&mut self) -> bool {
        self.turn.take_context_tokens_updated()
    }

    pub(crate) fn begin_dispatch(&mut self) -> Option<super::agent::DispatchingTurn> {
        self.turn.begin_dispatch()
    }

    pub(crate) fn finish_dispatch(&mut self, dispatch: super::agent::DispatchingTurn) {
        self.turn.finish_dispatch(dispatch);
    }

    pub(crate) fn begin_prepared_turn(&mut self, command: super::agent::CommandScope) {
        self.turn.begin_prepared_turn(command);
    }

    pub(crate) fn turn_command(&self) -> &super::agent::CommandScope {
        &self.turn.command
    }

    pub(crate) fn record_started_turn(
        &mut self,
        turn_id: u64,
        mode: protocol::AgentMode,
        reasoning_effort: protocol::ReasoningEffort,
    ) {
        self.turn
            .record_started_turn(turn_id, mode, reasoning_effort);
        self.session.last_turn_id = self.session.last_turn_id.max(turn_id);
    }

    pub(crate) fn issue_continuation_token(&mut self) -> u64 {
        self.turn.issue_continuation_token()
    }

    pub(crate) fn clear_continuation(&mut self) {
        self.turn.clear_continuation();
    }

    pub(crate) fn turn_pause(&self) -> Option<super::agent::TurnPause> {
        self.turn.pause
    }

    pub(crate) fn set_turn_pause(&mut self, pause: Option<super::agent::TurnPause>) {
        self.turn.pause = pause;
    }

    pub(crate) fn continuation_token(&self) -> Option<u64> {
        self.turn.pending_continuation_token()
    }

    pub(crate) fn consume_continuation(&mut self, token: u64) -> bool {
        self.turn.consume_continuation(token)
    }

    pub(crate) fn invalidate_turn_callbacks(&mut self) {
        self.turn.invalidate_turn_callbacks();
    }

    pub(crate) fn cancel_generation(&self) -> u64 {
        self.turn.cancel_generation()
    }

    pub(crate) fn remove_pending_tool(&mut self, invocation_id: protocol::InvocationId) {
        if let Some(turn) = self.turn.active_mut() {
            turn.pending
                .retain(|pending| pending.invocation_id != invocation_id);
        }
    }

    pub(crate) fn clear_pending_tools(&mut self) {
        if let Some(turn) = self.turn.active_mut() {
            turn.pending.clear();
        }
    }

    #[cfg(test)]
    pub(crate) fn install_live_session_for_harness(
        &mut self,
        live_session: smelt_core::session_runtime::LiveSession,
    ) {
        self.document.live_session = Some(live_session);
    }

    #[cfg(test)]
    pub(crate) fn set_history_resave_from_for_harness(&mut self, history_index: usize) {
        self.document
            .set_history_resave_from_for_test(history_index);
    }

    /// Makes the next session save fail with `error`.
    #[cfg(test)]
    pub(crate) fn inject_commit_failure(&self, error: smelt_store::StoreError) {
        self.persistence
            .as_ref()
            .expect("persistent session writer")
            .fail_next_save(error);
    }

    #[cfg(test)]
    pub(crate) fn install_session_for_harness(&mut self, session: smelt_core::session::Session) {
        self.session = session;
        self.clear_live_session();
    }

    #[cfg(test)]
    pub(crate) fn set_session_id_for_harness(&mut self, id: String) {
        self.session.id = id;
    }

    #[cfg(test)]
    pub(crate) fn set_session_mode_for_harness(&mut self, mode: Option<String>) {
        self.session.mode = mode;
    }

    #[cfg(test)]
    pub(crate) fn record_context_tokens_for_harness(
        &mut self,
        tokens: u32,
        identity: smelt_core::session::ContextTokenIdentity,
    ) {
        let history_len = self.history_len();
        self.session
            .record_context_tokens(tokens, history_len, identity);
    }

    #[cfg(test)]
    pub(crate) fn set_context_token_baseline_for_harness(&mut self, tokens: Option<u32>) {
        self.session.context_tokens = tokens;
        self.session.context_tokens_history_len = tokens.map(|_| self.session.history.len());
        if tokens.is_none() {
            self.session.display_context_tokens = None;
            self.session.display_context_token_identity = None;
        }
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn replace_history_for_harness(&mut self, history: Vec<protocol::HistoryItem>) {
        self.session.history = history;
        self.clear_live_session();
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn reset_context_accounting_for_harness(&mut self) {
        self.session.context_tokens = None;
        self.session.context_tokens_history_len = None;
        self.session.context_token_identity = None;
        self.session.display_context_tokens = None;
        self.session.display_context_token_identity = None;
        self.session.checkpoint = None;
        self.session.context_snapshots.clear();
        self.session.turn_metas.clear();
    }

    pub(crate) fn pending_history_appends(&self) -> &[super::PendingHistoryAppend] {
        self.turn.pending_history_appends()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn pending_history_append_count(&self) -> usize {
        self.turn.pending_history_append_count()
    }

    #[cfg(test)]
    pub(crate) fn pending_continuation_token(&self) -> Option<u64> {
        self.turn.pending_continuation_token()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn streaming_state(&self) -> (bool, bool, bool) {
        (
            self.parser.has_active_text(),
            self.parser.has_active_thinking(),
            self.parser.has_active_exec(),
        )
    }

    pub(crate) fn set_active_tools_paused(&mut self, paused: bool, now: std::time::Instant) {
        self.parser
            .set_active_tools_paused(self.document.transcript.history_mut(), paused, now);
    }

    pub(crate) fn clear_stream_parser(&mut self) {
        self.reveal.finish_all();
        self.streaming_kind = None;
        self.summary_parts.clear();
        self.compaction_source.clear();
        self.response_draft_blocks.clear();
        self.parser.clear();
    }

    pub(crate) fn clear_stream_tools(&mut self) {
        self.parser.clear_tools();
    }

    pub(crate) fn clear_token_baseline(
        &mut self,
        identity: smelt_core::session::ContextTokenIdentity,
    ) {
        self.apply_usage_mutation(
            super::session_document::UsageMutation::ClearTokenBaselineIfMismatched { identity },
        );
    }

    pub(crate) fn clear_token_baseline_for_loaded_model(
        &mut self,
        identity: smelt_core::session::ContextTokenIdentity,
    ) {
        self.document
            .clear_token_baseline_for_loaded_model(&mut self.session, identity);
    }

    pub(crate) fn tool_draft_state(&self, call_id: &str) -> (Option<String>, bool) {
        let Some(block_id) = self
            .parser
            .tool_draft_block_for_call(self.document.transcript.history(), call_id)
        else {
            return (None, false);
        };
        let Some(smelt_transcript::transcript_model::Block::ToolDraft(draft)) =
            self.document.transcript.history().block(block_id)
        else {
            return (None, false);
        };
        (Some(draft.stream_id.clone()), draft.finished)
    }

    pub(crate) fn next_transcript_refresh_at(&self) -> Option<std::time::Instant> {
        self.document.transcript.next_refresh_at()
    }

    pub(crate) fn take_resume_preview(
        &mut self,
        key: &str,
    ) -> Option<super::transcript::TranscriptDocument> {
        self.resume_preview_cache.take(key)
    }

    pub(crate) fn resume_preview(
        &self,
        key: &str,
    ) -> Option<&super::transcript::TranscriptDocument> {
        self.resume_preview_cache.get(key)
    }

    pub(crate) fn store_resume_preview(
        &mut self,
        key: String,
        view: super::transcript::TranscriptDocument,
    ) {
        self.resume_preview_cache.store(key, view);
    }

    pub(crate) fn invalidate_transcript_theme(&mut self) {
        self.document.transcript.invalidate_theme();
        self.resume_preview_cache.invalidate_theme();
    }

    pub(crate) fn set_transcript_inline_options(
        &mut self,
        options: smelt_transcript::content::highlight::InlineOptions,
    ) {
        self.document.transcript.set_inline_options(options.clone());
        self.resume_preview_cache.set_inline_options(options);
    }

    pub(crate) fn invalidate_transcript_renderer(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) {
        self.document
            .transcript
            .invalidate_renderer_if_changed(generation, cache_key);
        self.resume_preview_cache
            .invalidate_renderer_if_changed(generation, cache_key);
    }

    pub(crate) fn is_ephemeral(&self) -> bool {
        self.storage.is_ephemeral()
    }

    pub(crate) fn current_artifact_dir(&self) -> std::path::PathBuf {
        self.storage.artifact_dir(&self.sessions, &self.session)
    }

    pub(crate) fn sessions(&self) -> &smelt_core::session::SessionStorage {
        &self.sessions
    }

    pub(crate) fn history_len(&self) -> usize {
        self.document
            .live_session
            .as_ref()
            .map_or(self.session.history.len(), |live| live.history_len())
    }

    pub(crate) fn history_is_empty(&self) -> bool {
        self.document
            .live_session
            .as_ref()
            .map_or(self.session.history.is_empty(), |live| live.is_empty())
    }

    pub(crate) fn history_range(
        &self,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<protocol::HistoryItem>, String> {
        if let Some(live) = &self.document.live_session {
            return live.history_range(range);
        }
        let end = range.end.min(self.session.history.len());
        let start = range.start.min(end);
        Ok(self.session.history[start..end].to_vec())
    }

    pub(crate) fn scan_history(
        &self,
        range: std::ops::Range<usize>,
        chunk_items: usize,
        mut visit: impl FnMut(usize, &[protocol::HistoryItem]),
    ) -> Result<(), String> {
        if let Some(live) = &self.document.live_session {
            return live.scan_history(range, chunk_items, visit);
        }
        let end = range.end.min(self.session.history.len());
        let start = range.start.min(end);
        smelt_perf::perf::record_value(
            "session:history:scanned_items",
            end.saturating_sub(start) as u64,
        );
        let chunk_items = chunk_items.max(1);
        for (chunk_offset, items) in self.session.history[start..end]
            .chunks(chunk_items)
            .enumerate()
        {
            visit(
                start.saturating_add(chunk_offset.saturating_mul(chunk_items)),
                items,
            );
        }
        Ok(())
    }

    pub(crate) fn history_range_read(
        &self,
        range: std::ops::Range<usize>,
    ) -> smelt_core::session_runtime::HistoryRead {
        if let Some(live) = &self.document.live_session {
            return live.history_range_read(range);
        }
        let end = range.end.min(self.session.history.len());
        let start = range.start.min(end);
        smelt_core::session_runtime::HistoryRead::loaded(self.session.history[start..end].to_vec())
    }

    pub(crate) fn history_tail_read(
        &self,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<smelt_core::session_runtime::HistoryRead, String> {
        if let Some(live) = &self.document.live_session {
            return live.history_tail_read(max_items, max_bytes);
        }
        smelt_core::session_runtime::bounded_history_tail(
            &self.session.history,
            max_items,
            max_bytes,
        )
        .map(smelt_core::session_runtime::HistoryRead::loaded)
    }

    pub(crate) fn history_tail(
        &self,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<Vec<protocol::HistoryItem>, String> {
        if let Some(live) = &self.document.live_session {
            return live.history_tail(max_items, max_bytes);
        }
        smelt_core::session_runtime::bounded_history_tail(
            &self.session.history,
            max_items,
            max_bytes,
        )
    }

    pub(crate) fn has_resume_hint_messages(&self) -> bool {
        self.history_len() > 0
            || !self.document.transcript.is_empty()
            || self.document.live_session.is_some()
    }

    pub(crate) fn shutdown_context(&self) -> super::ShutdownContext {
        super::ShutdownContext {
            session_id: self.session.id.clone(),
            has_messages: self.has_resume_hint_messages(),
            ephemeral: self.is_ephemeral(),
        }
    }

    pub(crate) fn publish_shared_state(&self) {
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = Some(SharedSessionState {
                id: self.session.id.clone(),
                has_messages: self.has_resume_hint_messages(),
                ephemeral: self.is_ephemeral(),
            });
        }
    }

    pub(crate) fn clear_shared_state(&self) {
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = None;
        }
    }

    pub(crate) fn commit_transcript_view(&mut self, state: super::TranscriptViewState) -> bool {
        if self
            .committed_transcript_view
            .as_ref()
            .is_some_and(|view| view.state == state)
        {
            return false;
        }
        let revision = self
            .committed_transcript_view
            .as_ref()
            .map(|view| view.revision.wrapping_add(1))
            .unwrap_or(1);
        self.committed_transcript_view = Some(CommittedTranscriptView { revision, state });
        true
    }

    pub(crate) fn committed_transcript_view(&self) -> Option<CommittedTranscriptView> {
        self.committed_transcript_view.clone()
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.access.is_read_only()
            && !(self.writer_is_opening()
                && self.document.durable_head() == smelt_store::StoreHead::default())
    }

    pub(crate) fn writer_is_opening(&self) -> bool {
        matches!(self.access, SessionAccess::Opening)
    }

    pub(crate) fn read_only_reason(&self) -> String {
        match &self.access {
            SessionAccess::Opening => "session writer is opening".into(),
            SessionAccess::WaitingForOwnership => {
                "this session is already open in another window".into()
            }
            SessionAccess::ReadOnly { reason } => reason.clone(),
            SessionAccess::Owned => "session is read-only".to_string(),
        }
    }

    pub(crate) fn mark_read_only(&mut self, reason: String) {
        self.ownership_recovery.cancel();
        self.access = SessionAccess::ReadOnly { reason };
        self.document.disable_change_tracking();
    }

    pub(super) fn wait_for_ownership(
        &mut self,
        width: u16,
        target_rows: u16,
        events: tokio::sync::mpsc::UnboundedSender<super::AppEvent>,
    ) -> Result<(), String> {
        self.ownership_recovery.request(
            self.sessions.clone(),
            self.session.id.clone(),
            width,
            target_rows,
            events,
        )?;
        self.access = SessionAccess::WaitingForOwnership;
        self.document.disable_change_tracking();
        Ok(())
    }

    pub(super) fn take_ownership_recovery(
        &mut self,
    ) -> Option<Result<super::session_load::RecoveredSession, String>> {
        if !matches!(self.access, SessionAccess::WaitingForOwnership)
            || self
                .persistence
                .as_ref()
                .is_some_and(|actor| !actor.is_finished())
        {
            return None;
        }
        self.ownership_recovery.take_result()
    }

    #[cfg(test)]
    pub(crate) fn take_ownership_observer(
        &mut self,
    ) -> std::sync::mpsc::Receiver<super::session_load::OwnershipRecoveryEvent> {
        self.ownership_recovery.take_observer()
    }

    pub(crate) fn has_persistence(&self) -> bool {
        self.persistence.is_some()
    }

    pub(crate) fn has_unflushed_work(&self) -> bool {
        !self.is_ephemeral() && self.document.has_unsaved_work()
    }

    #[cfg(test)]
    pub(crate) fn shared_state(&self) -> Option<SharedSessionState> {
        self.shared_session.lock().ok()?.clone()
    }

    #[cfg(test)]
    pub(crate) fn persistence_status(&self) -> Option<crate::persist::PersistenceStatus> {
        self.persistence
            .as_ref()
            .map(crate::persist::SessionPersistence::status)
    }

    pub(crate) fn reset(&mut self, pid: u32, cwd: std::path::PathBuf) -> String {
        self.ownership_recovery.cancel();
        let old_id = self.session.id.clone();
        self.session = smelt_core::session::Session::new(pid, cwd);
        self.clear_live_session();
        self.document.transcript.set_store_address(
            self.storage
                .transcript_store_address(&self.sessions, &self.session),
        );
        self.access = SessionAccess::Owned;
        self.document.enable_change_tracking();
        self.turn.reset_session();
        self.document.mark_session_unpersisted();
        self.save_failure = None;
        self.clear_shared_state();
        old_id
    }

    pub(crate) fn install_loaded_session(
        &mut self,
        loaded: smelt_core::session::Session,
    ) -> Option<String> {
        self.ownership_recovery.cancel();
        self.session = loaded;
        self.clear_live_session();
        self.turn.reset_session();
        self.turn.restore_last_turn(self.session.last_turn_id);
        self.save_failure = None;
        self.session.cwd.clone()
    }

    pub(crate) fn claim_writer_access(&mut self) -> Result<(), crate::persist::PersistenceCause> {
        self.claim_writer_access_with_writer(None)
    }

    /// Starts the session writer. `writer` is one the caller already opened.
    pub(super) fn claim_writer_access_with_writer(
        &mut self,
        writer: Option<smelt_store::SessionWriter>,
    ) -> Result<(), crate::persist::PersistenceCause> {
        debug_assert!(self.persistence.is_none());
        if self.is_ephemeral() {
            self.access = SessionAccess::Owned;
            self.document.enable_change_tracking();
            return Ok(());
        }
        let persistence = crate::persist::SessionPersistence::spawn(
            self.sessions.store().clone(),
            self.session.id.clone(),
            writer,
            Arc::clone(&self.persistence_wake),
        )?;
        self.persistence = Some(persistence);
        self.observed_audit_warning = None;
        self.access = SessionAccess::Opening;
        self.document.enable_change_tracking();
        Ok(())
    }

    pub(crate) fn persistence_wake(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.persistence_wake)
    }

    /// Everything the writer reported since the last drain.
    pub(crate) fn drain_persistence_report(&mut self) -> Option<PersistenceReport> {
        if let Some(status) = self
            .persistence
            .as_ref()
            .and_then(crate::persist::SessionPersistence::take_changed_status)
        {
            self.apply_persistence_status(status);
        }
        let report = std::mem::take(&mut self.pending_report);
        (!report.is_empty()).then_some(report)
    }

    /// Applies the writer's status and keeps what changed for the next drain.
    /// Applying the same status again changes nothing.
    fn apply_persistence_status(&mut self, status: crate::persist::PersistenceStatus) {
        let startup = match &status.opened {
            Some(opened) if self.writer_is_opening() => {
                match opened {
                    Ok(()) => self.access = SessionAccess::Owned,
                    Err(cause) => self.mark_read_only(cause.message.clone()),
                }
                Some(opened.clone())
            }
            _ => None,
        };
        let durable = status
            .durable
            .is_some_and(|durable| self.document.acknowledge(durable, &mut self.session));
        if durable {
            self.save_failure = None;
        }
        let failure = status.failure.and_then(|(seq, cause)| {
            if !self.document.fail_save(seq) {
                return None;
            }
            if cause.class == crate::persist::PersistenceFailureClass::Ownership {
                self.mark_read_only(cause.message.clone());
            }
            self.save_failure = Some(cause.clone());
            Some(cause)
        });
        let audit_warning = (status.audit_warning != self.observed_audit_warning)
            .then(|| {
                self.observed_audit_warning = status.audit_warning.clone();
                status.audit_warning
            })
            .flatten();
        let report = &mut self.pending_report;
        report.startup = startup.or(report.startup.take());
        report.durable |= durable;
        report.failure = failure.or(report.failure.take());
        report.audit_warning = audit_warning.or(report.audit_warning.take());
    }

    fn apply_current_persistence_status(&mut self) {
        if let Some(status) = self
            .persistence
            .as_ref()
            .map(crate::persist::SessionPersistence::status)
        {
            self.apply_persistence_status(status);
        }
    }

    /// Why closing now would lose changes, if it would.
    fn unsaved_reason(&self, outcome: crate::persist::PersistenceFlushOutcome) -> Option<String> {
        self.document.has_unsaved_work().then(|| {
            self.save_failure.as_ref().map_or_else(
                || {
                    outcome
                        .into_result()
                        .err()
                        .unwrap_or_else(|| "session has unsaved changes".into())
                },
                |cause| cause.message.clone(),
            )
        })
    }

    /// Whether automatic saves wait for an explicit retry.
    pub(crate) fn automatic_save_blocked(&self) -> bool {
        self.save_failure.is_some()
    }

    pub(crate) fn save(
        &mut self,
        metadata: super::session_document::RuntimeSessionMetadata,
    ) -> Result<SaveStatus, String> {
        if self.writer_is_opening() && self.is_read_only() {
            return Ok(SaveStatus::DeferredStartup);
        }
        if self.is_read_only() {
            return Ok(SaveStatus::SkippedReadOnly);
        }
        if self.is_ephemeral() {
            self.apply_metadata_mutation(metadata.into_mutation());
            self.publish_shared_state();
            self.document.mark_ephemeral_persisted();
            return Ok(SaveStatus::DurableEphemeral);
        }
        if let Some(live) = self.document.live_session.as_ref() {
            smelt_perf::perf::record_value(
                "live_session:suffix_items",
                live.live_suffix_len() as u64,
            );
        }
        let save = match self.document.capture_save(&mut self.session, metadata) {
            Ok(Some(save)) => save,
            Ok(None) => return Ok(SaveStatus::Unchanged),
            Err(super::session_document::SaveCaptureError::HydrationPending) => {
                return Ok(SaveStatus::DeferredHydration);
            }
            Err(super::session_document::SaveCaptureError::Invalid(message)) => {
                self.save_failure =
                    Some(crate::persist::PersistenceCause::invariant(message.clone()));
                return Err(message);
            }
        };
        if self.persistence.is_none() {
            if let Err(cause) = self.claim_writer_access() {
                self.mark_read_only(cause.message.clone());
                return Err(cause.message);
            }
        }
        self.publish_shared_state();
        self.persistence
            .as_mut()
            .ok_or_else(|| "persistence actor is unavailable".to_string())?
            .submit(save)
            .map_err(|cause| cause.message)?;
        Ok(SaveStatus::Submitted)
    }

    /// Clears a save failure and saves again.
    pub(crate) fn retry_blocked_persistence(
        &mut self,
        metadata: super::session_document::RuntimeSessionMetadata,
    ) -> Result<SaveStatus, String> {
        if self.save_failure.take().is_none() {
            return Ok(SaveStatus::Unchanged);
        }
        self.document.transcript.retry_failed_hydrations();
        self.save(metadata)
    }

    /// Waits until every submitted save is durable. A save that failed before
    /// reaching the writer fails the flush too.
    pub(crate) fn flush_persistence_until(
        &mut self,
        deadline: std::time::Instant,
    ) -> crate::persist::PersistenceFlushOutcome {
        let Some(persistence) = self.persistence.as_ref() else {
            return crate::persist::PersistenceFlushOutcome::Failed(
                self.save_failure.clone().unwrap_or_else(|| {
                    crate::persist::PersistenceCause::unavailable(
                        "persistence actor is unavailable",
                    )
                }),
            );
        };
        let outcome = persistence.flush(deadline);
        self.apply_current_persistence_status();
        match (outcome, &self.save_failure) {
            (crate::persist::PersistenceFlushOutcome::Durable, Some(cause)) => {
                crate::persist::PersistenceFlushOutcome::Failed(cause.clone())
            }
            (outcome, _) => outcome,
        }
    }

    /// Whether the writer may close without losing changes: `None` while saves
    /// are still pending, then `Ok` once they are durable or why not. Without a
    /// writer there is nothing to wait for.
    pub(crate) fn poll_persistence_settled(&mut self) -> Option<Result<(), String>> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Some(Ok(()));
        };
        let outcome = persistence.try_flush()?;
        self.apply_current_persistence_status();
        Some(self.unsaved_reason(outcome).map_or(Ok(()), Err))
    }

    #[cfg(test)]
    pub(crate) fn pause_persistence(&self) -> std::sync::mpsc::Sender<()> {
        self.persistence
            .as_ref()
            .expect("persistence actor is running")
            .pause()
    }

    pub(crate) fn append_request_audit(
        &mut self,
        entry: protocol::request_log::RequestLogEntry,
        payload_mode: smelt_store::RequestAuditPayloadMode,
    ) -> Result<bool, crate::persist::PersistenceCause> {
        if self.is_ephemeral() || self.is_read_only() {
            return Ok(false);
        }
        let Some(actor) = self.persistence.as_mut() else {
            return Ok(false);
        };
        actor.append_request_audit(entry, payload_mode)?;
        Ok(true)
    }

    pub(crate) fn begin_persistence_close(
        &self,
        deadline: std::time::Instant,
    ) -> Option<crate::persist::PendingPersistenceClose> {
        self.persistence
            .as_ref()
            .map(|actor| actor.begin_close(deadline))
    }

    /// The close result once the writer stopped. A cancelled close keeps the
    /// session: the stopped writer is dropped and every save it did not confirm
    /// is rewritten by the next writer.
    pub(crate) fn poll_persistence_close(
        &mut self,
        pending: &crate::persist::PendingPersistenceClose,
        cancelled: bool,
        now: std::time::Instant,
    ) -> Option<Result<Option<String>, String>> {
        let persistence = self
            .persistence
            .as_mut()
            .expect("closing writer remains owned");
        let outcome = persistence.poll_close(pending, now)?;
        if !cancelled {
            return Some(
                self.finish_persistence_close(outcome, crate::persist::ClosePolicy::RequireDurable),
            );
        }
        if !persistence.is_finished() {
            return Some(Err(
                "persistence writer has not finished releasing ownership".into(),
            ));
        }
        self.apply_current_persistence_status();
        self.release_stopped_writer();
        self.save_failure = None;
        Some(Ok(None))
    }

    pub(crate) fn close_persistence_until(
        &mut self,
        policy: crate::persist::ClosePolicy,
        deadline: std::time::Instant,
    ) -> Result<Option<String>, String> {
        let Some(persistence) = self.persistence.as_mut() else {
            return match (&self.save_failure, policy) {
                (Some(cause), crate::persist::ClosePolicy::RequireDurable) => {
                    Err(cause.message.clone())
                }
                (cause, _) => Ok(cause.as_ref().map(|cause| cause.message.clone())),
            };
        };
        if policy == crate::persist::ClosePolicy::RequireDurable {
            // Keep the writer when the close would be refused anyway.
            let outcome = persistence.flush(deadline);
            self.apply_current_persistence_status();
            if let Some(reason) = self.unsaved_reason(outcome) {
                return Err(reason);
            }
        }
        let persistence = self.persistence.as_mut().expect("writer checked above");
        let outcome = persistence.close(deadline);
        self.finish_persistence_close(outcome, policy)
    }

    /// Applies the writer's final status. Unsaved changes fail a close that requires them.
    fn finish_persistence_close(
        &mut self,
        outcome: crate::persist::PersistenceFlushOutcome,
        policy: crate::persist::ClosePolicy,
    ) -> Result<Option<String>, String> {
        let released = self
            .persistence
            .as_ref()
            .expect("closing writer")
            .is_finished();
        self.apply_current_persistence_status();
        let unsaved = self.unsaved_reason(outcome);
        if !released {
            return Err(unsaved.unwrap_or_else(|| {
                "persistence writer has not finished releasing ownership".into()
            }));
        }
        self.release_stopped_writer();
        if let (Some(message), crate::persist::ClosePolicy::RequireDurable) = (&unsaved, policy) {
            // The session stays open; the caller gives it a writer again.
            return Err(message.clone());
        }
        self.save_failure = None;
        Ok(unsaved)
    }

    /// Drops a writer that has stopped. Saves it did not confirm are rewritten
    /// by the next writer.
    fn release_stopped_writer(&mut self) {
        self.persistence = None;
        self.document.fail_all_in_flight();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn streaming_save_keeps_deltas_that_arrive_after_its_capture() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_submitted_turn("initial request");
        let release = app.pause_persistence();
        app.feed_one(crate::event_source::SourceEvent::engine(
            protocol::EngineEvent::TextDelta {
                delta: "captured streaming text α".into(),
            },
        ));
        app.clock.advance(crate::app::stream_reveal::MAX_SPREAD);
        app.render_silent();
        app.save_session();
        assert!(!app.tui.conversation.document.has_uncaptured_work());
        app.feed_one(crate::event_source::SourceEvent::engine(
            protocol::EngineEvent::TextDelta {
                delta: " newer text 界".into(),
            },
        ));
        app.clock.advance(crate::app::stream_reveal::MAX_SPREAD);
        app.render_silent();
        assert!(app.tui.conversation.document.has_uncaptured_work());
        release.send(()).unwrap();
        app.save_session_and_flush();
        assert!(!app.tui.conversation.has_unflushed_work());

        let reader = app
            .tui
            .core
            .sessions
            .reader(&app.session_snapshot().id)
            .unwrap();
        let count = reader.head().unwrap().transcript_record_count.get();
        let saved = reader.transcript_range(count - 1, count).unwrap();
        let record =
            smelt_core::TranscriptBlockRecord::try_from(saved.into_iter().next().unwrap()).unwrap();
        assert_eq!(
            record.block.raw_text().unwrap(),
            "captured streaming text α newer text 界"
        );
        assert_eq!(record.content_hash, record.block.content_hash());
    }
}
