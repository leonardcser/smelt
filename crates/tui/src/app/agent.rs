use crate::app::{
    history::HistoryDeltaKind, CommandTurnStart, NotificationOperation, PendingHistoryAppend,
    PendingHistoryDelivery, PendingHistoryLifecycle, PendingTool, SessionControl, TuiApp,
    TurnState, CONFIRM_DEFER_MS,
};
use protocol::{Content, ContentPart, Decision, HistoryItem, UiCommand};
use smelt_core::working::{TurnOutcome, TurnPhase};
use smelt_core::*;
use std::path::PathBuf;
use std::time::Duration;

/// Owns the state machine data that exists only for an agent turn lifecycle.
///
/// `TuiApp` still orchestrates persistence, rendering, and engine I/O, while this
/// subsystem owns turn identity, callback invalidation, continuation, request-scoped
/// history, and the temporary dispatch state needed when an active turn is lent out.
pub(crate) struct TurnLifecycle {
    active: Option<TurnState>,
    next_turn_id: u64,
    last_terminal_turn_id: Option<u64>,
    next_continuation_token: u64,
    pending_continuation_token: Option<u64>,
    pub(super) pause: Option<TurnPause>,
    pub(super) command: CommandScope,
    pending_meta: Option<protocol::TurnMeta>,
    pending_history_appends: Vec<PendingHistoryAppend>,
    context_tokens_updated: bool,
    applied_mode: protocol::AgentMode,
    applied_reasoning_effort: protocol::ReasoningEffort,
    cancel_generation: u64,
    dispatching_turn_id: Option<u64>,
    dispatching_permissions: Option<std::sync::Arc<smelt_core::permissions::Permissions>>,
}

/// The command a turn runs for and the request settings it scopes to the turn.
/// A resumed turn keeps the scope of the turn it continues.
#[derive(Clone, Debug, Default)]
pub(crate) struct CommandScope {
    /// The custom command's name, or `None` for a plain request.
    pub(crate) name: Option<String>,
    pub(crate) overrides: smelt_core::custom_commands::CommandOverrides,
}

/// Stops automatic dispatch without treating an interrupted turn as idle work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnPause {
    pub(crate) kind: Option<protocol::EngineAskErrorKind>,
    pub(crate) retry_at_ms: Option<u64>,
}

pub(crate) struct DispatchingTurn {
    pub(crate) state: TurnState,
    previous_turn_id: Option<u64>,
    previous_permissions: Option<std::sync::Arc<smelt_core::permissions::Permissions>>,
}

pub(super) struct LuaToolCompletion {
    pub(super) content: String,
    pub(super) is_error: bool,
    pub(super) metadata: Option<serde_json::Value>,
    pub(super) display_content: Vec<protocol::ToolDisplayContent>,
    pub(super) attachment: Option<Box<protocol::ToolAttachment>>,
}

impl TurnLifecycle {
    pub(crate) fn new(
        applied_mode: protocol::AgentMode,
        applied_reasoning_effort: protocol::ReasoningEffort,
    ) -> Self {
        Self {
            active: None,
            next_turn_id: 1,
            last_terminal_turn_id: None,
            next_continuation_token: 1,
            pending_continuation_token: None,
            pause: None,
            command: CommandScope::default(),
            pending_meta: None,
            pending_history_appends: Vec::new(),
            context_tokens_updated: false,
            applied_mode,
            applied_reasoning_effort,
            cancel_generation: 0,
            dispatching_turn_id: None,
            dispatching_permissions: None,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn active(&self) -> Option<&TurnState> {
        self.active.as_ref()
    }

    pub(crate) fn active_mut(&mut self) -> Option<&mut TurnState> {
        self.active.as_mut()
    }

    pub(crate) fn set_active(&mut self, turn: Option<TurnState>) {
        self.active = turn;
    }

    pub(crate) fn clear_active(&mut self) -> Option<TurnState> {
        self.active.take()
    }

    pub(crate) fn next_turn_id(&self) -> u64 {
        self.next_turn_id
    }

    pub(crate) fn last_terminal_turn_id(&self) -> Option<u64> {
        self.last_terminal_turn_id
    }

    pub(crate) fn mark_terminal(&mut self, turn_id: u64) {
        self.last_terminal_turn_id = Some(
            self.last_terminal_turn_id
                .map_or(turn_id, |previous| previous.max(turn_id)),
        );
    }

    /// Continues turn numbering after `last_turn_id`, the session's last started
    /// turn. A loaded session runs no turn, so that turn is finished.
    pub(crate) fn restore_last_turn(&mut self, last_turn_id: u64) {
        self.last_terminal_turn_id = (last_turn_id > 0).then_some(last_turn_id);
        self.next_turn_id = self.next_turn_id.max(last_turn_id.saturating_add(1));
    }

    pub(crate) fn applied_mode(&self) -> &protocol::AgentMode {
        &self.applied_mode
    }

    pub(crate) fn set_applied_mode(&mut self, mode: protocol::AgentMode) {
        self.applied_mode = mode;
    }

    pub(crate) fn applied_reasoning_effort(&self) -> &protocol::ReasoningEffort {
        &self.applied_reasoning_effort
    }

    pub(crate) fn set_applied_reasoning_effort(
        &mut self,
        reasoning_effort: protocol::ReasoningEffort,
    ) {
        self.applied_reasoning_effort = reasoning_effort;
    }

    pub(crate) fn set_pending_meta(&mut self, meta: Option<protocol::TurnMeta>) {
        self.pending_meta = meta;
    }

    pub(crate) fn take_pending_meta(&mut self) -> Option<protocol::TurnMeta> {
        self.pending_meta.take()
    }

    pub(crate) fn mark_context_tokens_updated(&mut self) {
        self.context_tokens_updated = true;
    }

    pub(crate) fn clear_context_tokens_updated(&mut self) {
        self.context_tokens_updated = false;
    }

    pub(crate) fn take_context_tokens_updated(&mut self) -> bool {
        std::mem::take(&mut self.context_tokens_updated)
    }

    pub(crate) fn pending_history_appends(&self) -> &[PendingHistoryAppend] {
        &self.pending_history_appends
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn pending_history_append_count(&self) -> usize {
        self.pending_history_appends.len()
    }

    pub(crate) fn clear_pending_history_appends(&mut self) {
        self.pending_history_appends.clear();
    }

    pub(crate) fn retain_session_history_appends(&mut self) {
        self.pending_history_appends
            .retain(|pending| pending.lifecycle() == PendingHistoryLifecycle::SessionScoped);
    }

    pub(crate) fn take_pending_history_appends(&mut self) -> Vec<PendingHistoryAppend> {
        std::mem::take(&mut self.pending_history_appends)
    }

    pub(crate) fn take_pending_follow_up_note(&mut self) -> Option<protocol::HistoryNote> {
        // Earlier appends stay in chronological order as context for this trigger.
        let index = self.pending_history_appends.iter().rposition(|append| {
            append.delivery() == PendingHistoryDelivery::FollowUpIfUnconsumed
        })?;
        let append = self.pending_history_appends.remove(index);
        match append.item {
            HistoryItem::Note(note) => Some(note),
            _ => unreachable!("follow-up append must contain a history note"),
        }
    }

    pub(crate) fn pending_context_note(&self, name: &str) -> Option<Option<&str>> {
        self.pending_history_appends()
            .iter()
            .rev()
            .find(|pending| pending.context_name.as_deref() == Some(name))
            .map(|pending| {
                if pending.clear_context {
                    None
                } else {
                    pending.item.as_note().map(protocol::HistoryNote::text)
                }
            })
    }

    pub(crate) fn queue_history_append(
        &mut self,
        append: PendingHistoryAppend,
        mode_base: Option<&protocol::AgentMode>,
    ) {
        if append.coalescing_note_kind() == Some(protocol::HistoryNoteKind::ModeChange) {
            let Some(new_mode) = append.mode() else {
                self.replace_or_push_history_append(append);
                return;
            };
            let existing = self.pending_history_appends.iter().position(|pending| {
                pending.coalescing_note_kind() == Some(protocol::HistoryNoteKind::ModeChange)
            });
            if let Some(index) = existing {
                if mode_base.is_some_and(|base| base.as_str() == new_mode) {
                    self.pending_history_appends.remove(index);
                } else {
                    self.pending_history_appends[index] = append;
                }
            } else if mode_base.is_none_or(|base| base.as_str() != new_mode) {
                self.pending_history_appends.push(append);
            }
            return;
        }
        self.replace_or_push_history_append(append);
    }

    pub(crate) fn replace_or_push_history_append(&mut self, append: PendingHistoryAppend) {
        if append.coalescing_note_kind().is_some() {
            if let Some(existing) = self
                .pending_history_appends
                .iter_mut()
                .find(|pending| pending.same_coalescing_target(&append))
            {
                *existing = append;
                return;
            }
        }
        self.pending_history_appends.push(append);
    }

    pub(crate) fn remove_matching_history_append(&mut self, append: &PendingHistoryAppend) {
        self.pending_history_appends
            .retain(|pending| !pending.same_coalescing_target(append));
    }

    pub(crate) fn take_matching_history_append(
        &mut self,
        item: &protocol::HistoryItem,
    ) -> Option<PendingHistoryAppend> {
        let index = self
            .pending_history_appends()
            .iter()
            .position(|append| append.matches_history_item(item))?;
        Some(self.pending_history_appends.remove(index))
    }

    pub(crate) fn active_id(&self) -> Option<u64> {
        self.active
            .as_ref()
            .map(|turn| turn.turn_id)
            .or(self.dispatching_turn_id)
    }

    pub(crate) fn active_permissions(
        &self,
        fallback: std::sync::Arc<smelt_core::permissions::Permissions>,
    ) -> std::sync::Arc<smelt_core::permissions::Permissions> {
        self.active
            .as_ref()
            .map(|turn| turn.permissions.clone())
            .or_else(|| self.dispatching_permissions.clone())
            .unwrap_or(fallback)
    }

    pub(crate) fn refresh_active_permissions(
        &mut self,
        permissions: std::sync::Arc<smelt_core::permissions::Permissions>,
    ) {
        if self.dispatching_turn_id.is_some() {
            self.dispatching_permissions = Some(permissions);
        } else if let Some(turn) = self.active.as_mut() {
            turn.permissions = permissions;
        }
    }

    pub(crate) fn begin_dispatch(&mut self) -> Option<DispatchingTurn> {
        let state = self.active.take()?;
        let previous_turn_id = self.dispatching_turn_id.replace(state.turn_id);
        let previous_permissions = self
            .dispatching_permissions
            .replace(state.permissions.clone());
        Some(DispatchingTurn {
            state,
            previous_turn_id,
            previous_permissions,
        })
    }

    pub(crate) fn finish_dispatch(&mut self, mut dispatch: DispatchingTurn) {
        if let Some(permissions) = &self.dispatching_permissions {
            if !std::sync::Arc::ptr_eq(permissions, &dispatch.state.permissions) {
                dispatch.state.permissions = permissions.clone();
            }
        }
        self.dispatching_turn_id = dispatch.previous_turn_id;
        self.dispatching_permissions = dispatch.previous_permissions;
        self.active = Some(dispatch.state);
    }

    pub(crate) fn begin_prepared_turn(&mut self, command: CommandScope) {
        self.pending_continuation_token = None;
        self.pause = None;
        self.command = command;
    }

    pub(crate) fn record_started_turn(
        &mut self,
        turn_id: u64,
        mode: protocol::AgentMode,
        reasoning_effort: protocol::ReasoningEffort,
    ) {
        self.next_turn_id = turn_id.checked_add(1).unwrap_or(turn_id);
        self.applied_mode = mode;
        self.applied_reasoning_effort = reasoning_effort;
    }

    pub(crate) fn issue_continuation_token(&mut self) -> u64 {
        let token = self.next_continuation_token;
        self.next_continuation_token = self.next_continuation_token.wrapping_add(1).max(1);
        self.pending_continuation_token = Some(token);
        token
    }

    pub(crate) fn pending_continuation_token(&self) -> Option<u64> {
        self.pending_continuation_token
    }

    pub(crate) fn clear_continuation(&mut self) {
        self.pending_continuation_token = None;
    }

    pub(crate) fn consume_continuation(&mut self, token: u64) -> bool {
        if self.pending_continuation_token != Some(token) {
            return false;
        }
        self.pending_continuation_token = None;
        true
    }

    pub(crate) fn invalidate_turn_callbacks(&mut self) {
        self.cancel_generation = self.cancel_generation.wrapping_add(1);
        self.clear_continuation();
    }

    pub(crate) fn cancel_generation(&self) -> u64 {
        self.cancel_generation
    }

    pub(crate) fn reset_session(&mut self) {
        self.last_terminal_turn_id = None;
        self.pending_meta = None;
        self.pending_history_appends.clear();
        self.context_tokens_updated = false;
        self.pause = None;
        self.command = CommandScope::default();
        self.clear_continuation();
    }
}

struct StagedTurnRollback {
    history_len: Option<usize>,
    user_turn_count: usize,
    transcript_len: Option<usize>,
    submitted_block: Option<(smelt_core::transcript_model::BlockId, usize)>,
}

struct PreparedTurn {
    input: protocol::StartTurnInput,
    history: protocol::ModelHistorySource,
    submitted_history_idx: usize,
    model_target: protocol::ModelTarget,
    request_config: protocol::RequestRuntimeConfig,
    reasoning_effort: protocol::ReasoningEffort,
    permission_overrides: Option<protocol::PermissionOverrides>,
    permissions: std::sync::Arc<smelt_core::permissions::Permissions>,
    command: CommandScope,
    rewind_history_idx: Option<usize>,
    rollback: Option<StagedTurnRollback>,
}

#[derive(Clone, Copy)]
enum PendingTurnSubmitState {
    Context {
        provisional: Option<smelt_core::transcript_model::BlockId>,
    },
    Input {
        request_id: u64,
        block_id: smelt_core::transcript_model::BlockId,
        history_epoch: u64,
    },
}

pub struct TurnInputPrepared {
    request_id: u64,
    content: Content,
    observations: Vec<(String, smelt_core::fs::FileState)>,
}

impl std::fmt::Debug for TurnInputPrepared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnInputPrepared")
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
pub(super) struct TurnSubmissions {
    #[cfg(test)]
    input_pause: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    next_operation_id: u64,
    input_operations: super::session_load::SessionOperationRuntime,
    pub(super) accepted: Option<PendingTurnDispatch>,
}

#[cfg(test)]
impl TurnSubmissions {
    pub(super) fn pause_input(
        &mut self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (started, waiting) = std::sync::mpsc::channel();
        let (release, pause) = std::sync::mpsc::channel();
        self.input_pause = Some((started, pause));
        (waiting, release)
    }
}

pub(super) struct PendingTurnDispatch {
    dispatch: PreparedTurnDispatch,
    submit_state: PendingTurnSubmitState,
}

struct PreparedTurnDispatch {
    restore_prompt: Option<super::prompt_runtime::PendingPromptRecovery>,
    operation_id: u64,
    mode: protocol::AgentMode,
    fast_mode: bool,
    turn: PreparedTurn,
    system_prompt: String,
    tools: Vec<protocol::ToolDef>,
}

fn is_resumable_turn_error(kind: Option<protocol::EngineAskErrorKind>) -> bool {
    matches!(
        kind,
        Some(protocol::EngineAskErrorKind::Quota | protocol::EngineAskErrorKind::RateLimited)
    )
}

fn annotate_cwd_metadata(
    metadata: &mut Option<serde_json::Value>,
    committed: bool,
    error: Option<&str>,
) {
    let Some(serde_json::Value::Object(fields)) = metadata else {
        return;
    };
    fields.insert("pending".into(), serde_json::Value::Bool(false));
    fields.insert("cwd_committed".into(), serde_json::Value::Bool(committed));
    if let Some(error) = error {
        fields.insert(
            "cwd_error".into(),
            serde_json::Value::String(error.to_string()),
        );
    } else {
        fields.remove("cwd_error");
    }
}

impl TuiApp {
    /// Send a permission decision to the local engine.
    pub(crate) fn send_permission_decision(
        &mut self,
        request_id: u64,
        approved: bool,
        message: Option<String>,
    ) {
        self.core.engine.send(UiCommand::PermissionDecision {
            request_id,
            approved,
            message,
        });
    }

    pub(crate) fn active_permissions(
        &self,
    ) -> std::sync::Arc<smelt_core::permissions::Permissions> {
        self.conversation
            .active_permissions(self.core.permissions.snapshot())
    }

    pub(crate) fn with_dispatched_turn<R>(
        &mut self,
        body: impl FnOnce(&mut Self, &mut TurnState) -> R,
    ) -> Option<R> {
        let mut dispatch = self.conversation.begin_dispatch()?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            body(self, &mut dispatch.state)
        }));
        self.conversation.finish_dispatch(dispatch);
        match result {
            Ok(value) => Some(value),
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    pub(super) fn refresh_active_turn_permissions(&mut self) {
        self.conversation
            .refresh_active_permissions(self.core.permissions.snapshot());
    }

    fn agent_project_context(&self) -> protocol::AgentProjectContext {
        protocol::AgentProjectContext {
            cwd: self.core.env.cwd(),
            instructions: self.prompt_inputs.instructions.clone(),
            skill_section: self.prompt_inputs.skill_section.clone(),
            system_prompt_override: self.prompt_inputs.system_prompt_override.clone(),
            system_prompt: self.assemble_system_prompt(),
            tools: self.lua.tool_defs(
                self.core.config.mode.clone(),
                smelt_core::lua::ToolVisibility::Interactive,
            ),
        }
    }

    pub(crate) fn publish_agent_project_context(&self) {
        self.core
            .engine
            .send(protocol::UiCommand::UpdateAgentProjectContext(Box::new(
                self.agent_project_context(),
            )));
    }

    fn prepare_turn_context(&mut self) -> (String, Vec<protocol::ToolDef>) {
        let context = {
            let _perf = smelt_perf::perf::begin("agent:project_context");
            self.agent_project_context()
        };
        self.apply_pending_history_appends_for_request();
        (context.system_prompt, context.tools)
    }

    fn prepare_user_visible_turn(&mut self) {
        let _perf = smelt_perf::perf::begin("agent:prepare_visible_turn");
        self.dismiss_notification_for_turn_start();
        self.clear_prompt_prediction();
        self.platform.set_sleep_inhibited(true);
        self.begin_turn();
        self.ensure_current_context_note();
        self.apply_pending_history_appends_for_request();
    }

    fn publish_turn_input(&mut self, submitted: Option<String>) {
        match submitted {
            Some(text) if !text.is_empty() => self.publish_input_submit(text),
            _ => self.invalidate_prompt_prediction(),
        }
    }

    /// Whether a finished turn exists to continue from, with an error if not.
    fn has_continuation_target(&mut self) -> bool {
        if self.conversation.last_terminal_turn_id().is_some() {
            return true;
        }
        self.notify_turn_error_sticky(
            "cannot continue: this session has no prior turn; submit a new request first".into(),
        );
        false
    }

    fn accept_user_input(
        &mut self,
        display: &str,
        content: Content,
        sent_at_ms: u64,
        placement: Option<protocol::history::ImagePlacement>,
        model_target: protocol::ModelTarget,
    ) -> Option<TurnState> {
        self.prepare_user_visible_turn();
        let history_len = self.session_history_len();
        let block_id = self.conversation.append_accepted_block(Block::User {
            text: display.to_string(),
            image_labels: content.image_labels(),
            command: false,
            sent_at_ms: Some(sent_at_ms),
        });
        let expand_files =
            !smelt_core::file_ref::parse_at_file_refs(&content.text_content()).is_empty();
        let input = if expand_files || content.image_count() > 0 {
            protocol::StartTurnInput::user_with_display(content.clone(), display)
                .with_image_placement(placement)
        } else {
            protocol::StartTurnInput::user(content.clone())
        };
        let turn = PreparedTurn {
            input: input.with_sent_at_ms(sent_at_ms),
            history: self.model_history_source(),
            submitted_history_idx: history_len,
            model_target,
            request_config: self.core.config.request_runtime_config(),
            reasoning_effort: self.core.config.reasoning_effort.clone(),
            permission_overrides: None,
            permissions: self.core.permissions.snapshot(),
            command: CommandScope::default(),
            rewind_history_idx: None,
            rollback: None,
        };
        let mut dispatch = self.prepare_turn_dispatch(turn, Some(block_id))?;
        dispatch.turn.history = self.model_history_source();
        dispatch.turn.submitted_history_idx = self.session_history_len();
        if !expand_files {
            return self.stage_accepted_input(dispatch, block_id, content, Vec::new());
        }
        let cwd = self.workspace.cwd().to_owned();
        let events = self.platform.app_event_sender();
        #[cfg(test)]
        let pause = self.conversation.submissions.input_pause.take();
        let request = self
            .conversation
            .submissions
            .input_operations
            .request(move |token| {
                #[cfg(test)]
                if let Some((started, pause)) = pause {
                    let _ = started.send(());
                    let _ = pause.recv();
                }
                if token.cancelled() {
                    return;
                }
                let mut observations = Vec::new();
                let mut expand = |text: &str| {
                    let expanded = smelt_core::file_ref::prepare_at_file_refs(text, &cwd);
                    observations.extend(expanded.observations);
                    expanded.text
                };
                let content = match content {
                    Content::Text(text) => Content::Text(expand(&text)),
                    Content::Parts(parts) => Content::Parts(
                        parts
                            .into_iter()
                            .map(|part| match part {
                                ContentPart::Text { text } if !token.cancelled() => {
                                    ContentPart::Text {
                                        text: expand(&text),
                                    }
                                }
                                other => other,
                            })
                            .collect(),
                    ),
                };
                if !token.cancelled() {
                    let _ = events.send(super::AppEvent::TurnInputPrepared(Box::new(
                        TurnInputPrepared {
                            request_id: token.generation,
                            content,
                            observations,
                        },
                    )));
                }
            });
        match request {
            Ok(request_id) => {
                self.conversation.submissions.accepted = Some(PendingTurnDispatch {
                    dispatch,
                    submit_state: PendingTurnSubmitState::Input {
                        request_id,
                        block_id,
                        history_epoch: self.core.signals.get::<u64>("history_epoch").unwrap_or(0),
                    },
                });
            }
            Err(message) => {
                self.conversation.remove_accepted_block(block_id);
                self.fail_prepared_turn_dispatch(dispatch, message);
            }
        }
        self.request_urgent_render();
        None
    }

    pub(super) fn handle_turn_input_prepared(&mut self, result: TurnInputPrepared) {
        let current = self.conversation.submissions.accepted.as_ref().is_some_and(|pending| {
            matches!(pending.submit_state, PendingTurnSubmitState::Input { request_id, .. }
                if request_id == result.request_id && self.conversation.submissions.input_operations.is_current(request_id))
        });
        if !current {
            return;
        }
        let matches = self.conversation.submissions.accepted.as_ref().is_some_and(|pending| {
            matches!(pending.submit_state, PendingTurnSubmitState::Input { request_id, history_epoch, .. }
                if request_id == result.request_id
                    && self.conversation.submissions.input_operations.is_current(request_id)
                    && history_epoch == self.core.signals.get::<u64>("history_epoch").unwrap_or(0)
                    && pending.dispatch.turn.submitted_history_idx == self.session_history_len())
        });
        if !matches {
            self.fail_pending_turn_submission("session history changed during request preparation");
            return;
        }
        let pending = self
            .conversation
            .submissions
            .accepted
            .take()
            .expect("matching accepted input");
        let PendingTurnSubmitState::Input { block_id, .. } = pending.submit_state else {
            unreachable!()
        };
        let turn = self.stage_accepted_input(
            pending.dispatch,
            block_id,
            result.content,
            result.observations,
        );
        self.conversation.set_active(turn);
        self.request_urgent_render();
    }

    fn stage_accepted_input(
        &mut self,
        mut dispatch: PreparedTurnDispatch,
        block_id: smelt_core::transcript_model::BlockId,
        prepared_content: Content,
        observations: Vec<(String, smelt_core::fs::FileState)>,
    ) -> Option<TurnState> {
        let protocol::StartTurnInput::User {
            content,
            display,
            sent_at_ms,
            image_placement,
            ..
        } = &mut dispatch.turn.input
        else {
            unreachable!()
        };
        *content = prepared_content;
        let first_user_message = self
            .conversation
            .session()
            .first_user_message
            .is_none()
            .then(|| content.text_content().into_owned());
        let mut item = protocol::history_item_from_user_content(content.clone());
        if let HistoryItem::User {
            display: slot,
            sent_at_ms: timestamp,
            image_placement: placement,
            ..
        } = &mut item
        {
            *slot = display.clone();
            *timestamp = *sent_at_ms;
            *placement = image_placement.clone();
        }
        let history_index = self.session_history_len();
        dispatch.turn.rollback = Some(StagedTurnRollback {
            history_len: Some(history_index),
            user_turn_count: self.conversation.history_navigation().user_turn_count(),
            transcript_len: None,
            submitted_block: Some((block_id, history_index)),
        });
        self.stage_request_history_item_with_first_user(item, None, first_user_message);
        assert!(self
            .conversation
            .promote_accepted_block(block_id, history_index));
        dispatch.turn.rewind_history_idx = Some(history_index);
        for (path, state) in observations {
            self.core.files.set(&path, state);
        }
        self.start_prepared_turn(dispatch)
    }

    pub(crate) fn begin_agent_turn(
        &mut self,
        display: &str,
        content: Content,
        sent_at_ms: u64,
    ) -> Option<TurnState> {
        self.begin_agent_turn_with_placement(display, content, sent_at_ms, None)
    }

    pub(crate) fn begin_agent_turn_with_placement(
        &mut self,
        display: &str,
        content: Content,
        sent_at_ms: u64,
        placement: Option<protocol::history::ImagePlacement>,
    ) -> Option<TurnState> {
        let _perf = smelt_perf::perf::begin("agent:begin_turn");
        if self.block_read_only_mutation("submit a turn to this read-only session") {
            return None;
        }
        let model_target = self.resolve_model_target()?;
        let text = content.text_content();
        let submitted = match smelt_buffer::text::trim_whitespace(&text) {
            "" => None,
            trimmed => Some(trimmed.to_string()),
        };
        if content.is_empty() {
            if !self.has_continuation_target() {
                return None;
            }
            self.prepare_user_visible_turn();
            self.publish_turn_input(submitted);
            let history = self.model_history_source();
            let submitted_history_idx = self.session_history_len().checked_sub(1)?;
            return self.dispatch_prepared_turn(PreparedTurn {
                input: protocol::StartTurnInput::user(content),
                history,
                submitted_history_idx,
                model_target,
                request_config: self.core.config.request_runtime_config(),
                reasoning_effort: self.core.config.reasoning_effort.clone(),
                permission_overrides: None,
                permissions: self.core.permissions.snapshot(),
                command: CommandScope::default(),
                rewind_history_idx: None,
                rollback: None,
            });
        }
        self.accept_user_input(display, content, sent_at_ms, placement, model_target)
    }

    fn rollback_staged_turn(&mut self, rollback: &StagedTurnRollback) {
        if let Some((id, history_index)) = rollback.submitted_block {
            self.conversation
                .remove_submitted_user_block(id, history_index);
        }
        if let Some(history_len) = rollback.history_len {
            self.rewind_session_history_to(history_len, rollback.user_turn_count, false);
        }
        if let Some(transcript_len) = rollback.transcript_len {
            self.truncate_to(transcript_len);
        }
        self.sync_session_snapshot();
        self.publish_history_delta(HistoryDeltaKind::SubmitFailed);
    }

    fn prepare_turn_dispatch(
        &mut self,
        turn: PreparedTurn,
        provisional: Option<smelt_core::transcript_model::BlockId>,
    ) -> Option<PreparedTurnDispatch> {
        self.conversation.begin_prepared_turn(turn.command.clone());
        let operation_id = self
            .conversation
            .submissions
            .next_operation_id
            .checked_add(1)
            .expect("accepted operation ID overflow");
        self.conversation.submissions.next_operation_id = operation_id;
        debug_assert!(self.conversation.submissions.accepted.is_none());
        self.conversation.submissions.accepted = Some(PendingTurnDispatch {
            dispatch: PreparedTurnDispatch {
                restore_prompt: None,
                operation_id,
                turn,
                system_prompt: String::new(),
                tools: Vec::new(),
                mode: self.core.config.mode.clone(),
                fast_mode: self.fast_mode(),
            },
            submit_state: PendingTurnSubmitState::Context { provisional },
        });
        self.working.begin(TurnPhase::Working);
        if provisional.is_some() {
            let submitted = self
                .conversation
                .submissions
                .accepted
                .as_ref()
                .expect("accepted input")
                .dispatch
                .turn
                .input
                .provider_content()
                .text_content()
                .into_owned();
            let submitted = smelt_buffer::text::trim_whitespace(&submitted);
            self.publish_turn_input((!submitted.is_empty()).then(|| submitted.to_owned()));
        }
        self.core.signals.emit_dyn(
            "turn_start",
            std::rc::Rc::new(smelt_core::signals::EventStub),
        );
        self.pump_lua();
        if self
            .conversation
            .submissions
            .accepted
            .as_ref()
            .is_none_or(|pending| pending.dispatch.operation_id != operation_id)
        {
            return None;
        }
        let (system_prompt, tools) = self.prepare_turn_context();
        let mode = self.core.config.mode.clone();
        let fast_mode = self.fast_mode();
        let mut pending = self
            .conversation
            .submissions
            .accepted
            .take()
            .expect("matching accepted operation");
        pending.dispatch.system_prompt = system_prompt;
        pending.dispatch.tools = tools;
        pending.dispatch.mode = mode;
        pending.dispatch.fast_mode = fast_mode;
        Some(pending.dispatch)
    }

    fn dispatch_prepared_turn(&mut self, turn: PreparedTurn) -> Option<TurnState> {
        let dispatch = self.prepare_turn_dispatch(turn, None)?;
        self.start_prepared_turn(dispatch)
    }

    pub(super) fn preserve_pending_prompt_submission(
        &mut self,
        mut recovery: super::prompt_runtime::PendingPromptRecovery,
    ) {
        recovery.record_applied_edit(crate::input::prompt_ctx_ref(&self.ui).buf);
        if let Some(pending) = self.conversation.submissions.accepted.as_mut() {
            pending.dispatch.restore_prompt = Some(recovery);
        }
    }

    fn fail_prepared_turn_dispatch(&mut self, dispatch: PreparedTurnDispatch, message: String) {
        if let Some(rollback) = dispatch.turn.rollback.as_ref() {
            self.rollback_staged_turn(rollback);
        }
        if let Some(recovery) = dispatch.restore_prompt {
            let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
            self.prompt.restore_failed_submission(&mut pctx, recovery);
        }
        self.platform.set_sleep_inhibited(false);
        self.working.finish(TurnOutcome::Errored);
        self.notify_turn_error_sticky(message);
    }

    /// Sends the prepared turn to the engine and saves its request.
    fn start_prepared_turn(&mut self, dispatch: PreparedTurnDispatch) -> Option<TurnState> {
        let turn_id = self.conversation.next_turn_id();
        let PreparedTurnDispatch {
            turn,
            system_prompt,
            tools,
            mode,
            fast_mode,
            operation_id: _,
            restore_prompt: _,
        } = dispatch;
        self.conversation
            .record_started_turn(turn_id, mode.clone(), turn.reasoning_effort.clone());
        self.core
            .session_control
            .begin_turn(&self.conversation.session().id, turn_id);

        if smelt_provider::BackendKind::is_claude_code(&turn.model_target.provider_type) {
            smelt_core::permissions::claude_code::grant_plan_artifacts(
                &self.core.permissions,
                &self
                    .core
                    .sessions
                    .artifact_dir_for(self.conversation.session()),
            );
        }
        let permissions = turn.permissions.clone();
        let native_prompt =
            (smelt_provider::BackendKind::is_claude_code(&turn.model_target.provider_type))
                .then(|| self.native_system_prompt());
        let payload = protocol::StartTurnPayload {
            turn_id,
            input: turn.input,
            mode,
            model_target: turn.model_target,
            request_config: turn.request_config,
            reasoning_effort: turn.reasoning_effort,
            fast_mode,
            control_env: self.core.session_control.agent_env(),
            history: turn.history,
            session_id: self.conversation.session().id.clone(),
            artifact_dir: self
                .core
                .sessions
                .artifact_dir_for(self.conversation.session()),
            ephemeral: self.ephemeral(),
            permission_overrides: turn.permission_overrides,
            system_prompt: Some(system_prompt),
            native_prompt,
            tool_visibility: protocol::ToolVisibility::Interactive,
            tools,
        };
        if self
            .core
            .engine
            .try_send(UiCommand::StartTurn(Box::new(payload)))
            .is_err()
        {
            self.platform.set_sleep_inhibited(false);
            self.working.finish(TurnOutcome::Errored);
            self.notify_application_error_sticky(
                "engine stopped before accepting the request".into(),
            );
            return None;
        }
        for input in self.prompt.queued_request_inputs() {
            self.core.engine.send(UiCommand::Steer { input });
        }
        smelt_perf::perf::record_value("agent:dispatch:start_turn:turn_id", turn_id);
        smelt_perf::perf::record_value(
            "agent:dispatch:start_turn:at_us",
            smelt_perf::perf::timestamp_us(),
        );
        self.save_session();

        Some(TurnState {
            turn_id,
            pending: Vec::new(),
            permissions,
            submitted_history_idx: turn.submitted_history_idx,
            rewind_history_idx: turn.rewind_history_idx,
            assistant_output_started: false,
            _perf: smelt_perf::perf::begin("agent:turn"),
        })
    }

    pub(crate) fn turn_submission_is_pending(&self) -> bool {
        self.conversation.submissions.accepted.is_some()
    }

    pub(super) fn preparing_input_for_rewind(&self) -> Option<super::history::RewindInput> {
        let pending = self.conversation.submissions.accepted.as_ref()?;
        if !matches!(
            pending.submit_state,
            PendingTurnSubmitState::Input { .. }
                | PendingTurnSubmitState::Context {
                    provisional: Some(_)
                }
        ) {
            return None;
        }
        let protocol::StartTurnInput::User {
            content,
            display,
            image_placement,
            ..
        } = &pending.dispatch.turn.input
        else {
            return None;
        };
        let mut item = HistoryItem::user(content.clone());
        if let HistoryItem::User {
            display: slot,
            image_placement: placement,
            ..
        } = &mut item
        {
            *slot = display.clone();
            *placement = image_placement.clone();
        }
        super::history::rewind_input(&item)
    }

    pub(super) fn pending_turn_rewind_history_index(&self) -> Option<usize> {
        self.conversation
            .submissions
            .accepted
            .as_ref()
            .and_then(|pending| pending.dispatch.turn.rewind_history_idx)
    }

    pub(super) fn abandon_pending_turn_submission(&mut self) {
        self.conversation.submissions.input_operations.cancel();
        if let Some(pending) = self.conversation.submissions.accepted.take() {
            match pending.submit_state {
                PendingTurnSubmitState::Input { block_id, .. }
                | PendingTurnSubmitState::Context {
                    provisional: Some(block_id),
                } => self.conversation.remove_accepted_block(block_id),
                PendingTurnSubmitState::Context { provisional: None } => {}
            }
        }
    }

    pub(super) fn cancel_pending_turn_submission(&mut self) {
        let Some(pending) = self.conversation.submissions.accepted.take() else {
            return;
        };
        match pending.submit_state {
            PendingTurnSubmitState::Context { provisional: None } => {
                if let Some(rollback) = pending.dispatch.turn.rollback.as_ref() {
                    self.rollback_staged_turn(rollback);
                }
            }
            PendingTurnSubmitState::Input { block_id, .. }
            | PendingTurnSubmitState::Context {
                provisional: Some(block_id),
            } => {
                self.conversation.submissions.input_operations.cancel();
                self.conversation.remove_accepted_block(block_id);
            }
        }
    }

    /// Fails the turn that is still preparing, restoring what it staged.
    pub(super) fn fail_pending_turn_submission(&mut self, message: &str) -> bool {
        let Some(mut pending) = self.conversation.submissions.accepted.take() else {
            return false;
        };
        match pending.submit_state {
            PendingTurnSubmitState::Context { provisional: None } => {}
            PendingTurnSubmitState::Input { block_id, .. }
            | PendingTurnSubmitState::Context {
                provisional: Some(block_id),
            } => {
                self.conversation.submissions.input_operations.cancel();
                self.conversation.remove_accepted_block(block_id);
                pending.dispatch.turn.rollback = None;
            }
        }
        self.fail_prepared_turn_dispatch(pending.dispatch, message.to_owned());
        true
    }

    pub(crate) fn begin_process_status_turn(
        &mut self,
        history_note: protocol::HistoryNote,
    ) -> Option<TurnState> {
        if self.block_read_only_mutation("submit a turn to this read-only session") {
            return None;
        }
        let model_target = self.resolve_model_target()?;
        self.invalidate_prompt_prediction();
        let lua = self.lua.execution();
        let block = crate::lua::scope_app(self, || {
            crate::app::history::history_note_to_block(&lua, &history_note)
        });
        let adds_history = !history_note.text().is_empty();
        let adds_block = block.is_some();
        if !adds_history && self.session_history_len() == 0 {
            return None;
        }
        self.prepare_user_visible_turn();
        let rollback = StagedTurnRollback {
            history_len: adds_history.then(|| self.session_history_len()),
            user_turn_count: self.conversation.history_navigation().user_turn_count(),
            transcript_len: adds_block
                .then(|| self.conversation.transcript().history().order.len()),
            submitted_block: None,
        };
        let history = if adds_history {
            self.stage_request_history_item(HistoryItem::note(history_note.clone()), block)
        } else {
            if let Some(block) = block {
                self.push_block(block);
            }
            self.model_history_source()
        };
        let submitted_history_idx = self.session_history_len().checked_sub(1)?;
        let request_config = self.core.config.request_runtime_config();
        self.dispatch_prepared_turn(PreparedTurn {
            input: protocol::StartTurnInput::note(history_note),
            history,
            submitted_history_idx,
            model_target,
            request_config,
            reasoning_effort: self.core.config.reasoning_effort.clone(),
            permission_overrides: None,
            permissions: self.core.permissions.snapshot(),
            command: CommandScope::default(),
            rewind_history_idx: None,
            rollback: (adds_history || adds_block).then_some(rollback),
        })
    }

    fn custom_command_parts(
        &self,
        cmd: smelt_core::custom_commands::CustomCommand,
    ) -> (String, String, CommandScope) {
        let scope = CommandScope {
            name: Some(cmd.name),
            overrides: cmd.overrides,
        };
        (format!("/{}", cmd.display), cmd.body, scope)
    }

    pub(crate) fn begin_custom_command_turn(
        &mut self,
        cmd: smelt_core::custom_commands::CustomCommand,
        sent_at_ms: u64,
    ) -> Option<TurnState> {
        let (display, evaluated, scope) = self.custom_command_parts(cmd);
        self.begin_command_request_turn(
            display,
            evaluated,
            scope,
            CommandTurnStart::Fresh,
            sent_at_ms,
        )
    }

    pub(crate) fn begin_custom_command_continuation(
        &mut self,
        cmd: smelt_core::custom_commands::CustomCommand,
        sent_at_ms: u64,
    ) -> Option<TurnState> {
        let (display, evaluated, scope) = self.custom_command_parts(cmd);
        self.begin_command_request_turn(
            display,
            evaluated,
            scope,
            CommandTurnStart::ContinueFromLast,
            sent_at_ms,
        )
    }

    fn resolve_command_model_target(
        &mut self,
        overrides: &smelt_core::custom_commands::CommandOverrides,
    ) -> Option<(protocol::ModelTarget, protocol::ModelCatalogMetadata)> {
        let target_model = overrides.model.as_deref();
        let target_provider = overrides.provider.as_deref();
        let resolved = match (target_model, target_provider) {
            (Some(reference), provider) => smelt_core::config::resolve_model_ref_with_provider(
                &self.core.config.available_models,
                reference,
                provider,
            ),
            (None, Some(provider)) => smelt_core::config::resolve_provider_ref(
                &self.core.config.available_models,
                provider,
            ),
            (None, None) => {
                let catalog = self
                    .core
                    .config
                    .active_model()
                    .map(|model| model.reasoning_catalog())
                    .unwrap_or_default();
                return self.resolve_model_target().map(|target| (target, catalog));
            }
        };
        let resolved = match resolved {
            Ok(model) => model.clone(),
            Err(error) => {
                self.notify_operation_error_sticky(
                    NotificationOperation::TurnStart,
                    error.to_string(),
                );
                return None;
            }
        };
        let api_key = self.resolve_api_key_for_env(&resolved.api_key_env)?;
        Some((resolved.target(api_key), resolved.reasoning_catalog()))
    }

    pub(crate) fn begin_command_request_turn(
        &mut self,
        display: String,
        evaluated: String,
        scope: CommandScope,
        start: CommandTurnStart,
        sent_at_ms: u64,
    ) -> Option<TurnState> {
        let CommandScope { name, overrides } = scope;
        if self.block_read_only_mutation("submit a turn to this read-only session") {
            return None;
        }
        let submitted = match smelt_buffer::text::trim_whitespace(&evaluated) {
            "" => None,
            trimmed => Some(trimmed.to_string()),
        };
        let (mut model_target, model_catalog) = self.resolve_command_model_target(&overrides)?;
        let reasoning = if let Some(label) = overrides.reasoning_effort.as_deref() {
            let Some(effort) = protocol::ReasoningEffort::parse(label) else {
                self.notify_operation_error_sticky(
                    NotificationOperation::TurnStart,
                    "reasoning effort must not be empty".into(),
                );
                return None;
            };
            if !model_catalog.supports_reasoning_effort(&effort) {
                self.notify_operation_error_sticky(
                    NotificationOperation::TurnStart,
                    format!(
                        "reasoning effort '{}' is not supported by model '{}'",
                        effort.label(),
                        model_target.model
                    ),
                );
                return None;
            }
            effort
        } else {
            model_catalog.reconcile_reasoning_effort(self.core.config.reasoning_effort.clone())
        };
        if matches!(start, CommandTurnStart::ContinueFromLast) && !self.has_continuation_target() {
            return None;
        }
        if evaluated.is_empty() && self.session_history_len() == 0 {
            return None;
        }
        self.prepare_user_visible_turn();
        let rollback = StagedTurnRollback {
            history_len: (!evaluated.is_empty()).then(|| self.session_history_len()),
            user_turn_count: self.conversation.history_navigation().user_turn_count(),
            transcript_len: Some(self.conversation.transcript().history().order.len()),
            submitted_block: None,
        };

        let history = if !evaluated.is_empty() {
            let first_user_message = self
                .conversation
                .session()
                .first_user_message
                .is_none()
                .then(|| display.clone());
            self.stage_request_history_item_with_first_user(
                protocol::HistoryItem::user_command(
                    Content::text(evaluated.clone()),
                    display.clone(),
                )
                .with_sent_at_ms(sent_at_ms),
                Some(Block::User {
                    text: display.clone(),
                    image_labels: vec![],
                    command: true,
                    sent_at_ms: Some(sent_at_ms),
                }),
                first_user_message,
            )
        } else {
            if !display.is_empty() {
                self.push_block(Block::User {
                    text: display.clone(),
                    image_labels: vec![],
                    command: true,
                    sent_at_ms: Some(sent_at_ms),
                });
            }
            self.model_history_source()
        };
        self.publish_turn_input(submitted);

        let model_config_overrides = {
            let o = &overrides;
            if o.temperature.is_some()
                || o.top_p.is_some()
                || o.top_k.is_some()
                || o.min_p.is_some()
                || o.repeat_penalty.is_some()
            {
                Some(protocol::ModelConfigOverrides {
                    temperature: o.temperature,
                    top_p: o.top_p,
                    top_k: o.top_k,
                    min_p: o.min_p,
                    repeat_penalty: o.repeat_penalty,
                    tool_calling: None,
                    max_tokens: None,
                    thinking_budgets: None,
                })
            } else {
                None
            }
        };

        if let Some(ref request_overrides) = model_config_overrides {
            model_target.config = model_target.config.with_overrides(request_overrides);
        }

        let permission_overrides = {
            let o = &overrides;
            if o.tools.is_some() || !o.subcommands.is_empty() {
                let to_override =
                    |r: &smelt_core::custom_commands::RuleOverride| protocol::RuleSetOverride {
                        allow: r.allow.clone(),
                        ask: r.ask.clone(),
                        deny: r.deny.clone(),
                    };
                Some(protocol::PermissionOverrides {
                    tools: o.tools.as_ref().map(to_override),
                    subcommands: o
                        .subcommands
                        .iter()
                        .map(|(k, v)| (k.clone(), to_override(v)))
                        .collect(),
                })
            } else {
                None
            }
        };

        let current_permissions = self.core.permissions.snapshot();
        let permissions = permission_overrides
            .as_ref()
            .map(|overrides| std::sync::Arc::new(current_permissions.with_overrides(overrides)))
            .unwrap_or(current_permissions);

        if matches!(start, CommandTurnStart::ContinueFromLast) {
            self.working.continue_from_last(TurnPhase::Working);
        }

        let submitted_history_idx = self.session_history_len().checked_sub(1)?;
        let rewind_history_idx = (!evaluated.is_empty()).then_some(submitted_history_idx);
        let request_config = self.core.config.request_runtime_config();
        self.dispatch_prepared_turn(PreparedTurn {
            input: protocol::StartTurnInput::user_command(Content::text(evaluated), display)
                .with_sent_at_ms(sent_at_ms),
            history,
            submitted_history_idx,
            model_target,
            request_config,
            reasoning_effort: reasoning,
            permission_overrides,
            permissions,
            command: CommandScope { name, overrides },
            rewind_history_idx,
            rollback: Some(rollback),
        })
    }

    fn record_finished_turn_state(&mut self, ui_meta: protocol::TurnMeta) {
        let mut meta = match self.conversation.take_pending_meta() {
            Some(engine_meta) => protocol::TurnMeta {
                elapsed_ms: ui_meta.elapsed_ms,
                avg_tps: engine_meta.avg_tps.or(ui_meta.avg_tps),
                display_tps: engine_meta.display_tps.or(ui_meta.display_tps),
                interrupted: engine_meta.interrupted,
            },
            None => ui_meta,
        };
        if meta.display_tps.is_none() {
            meta.display_tps = meta.avg_tps.or_else(|| self.working.display_tps());
        }
        let history_len = self.session_history_len();
        let update_context_token_history_len = self.conversation.take_context_tokens_updated();
        self.conversation
            .finish_turn_state(history_len, meta, update_context_token_history_len);
    }

    fn cancel_turn_lua_tasks(&mut self) {
        self.lua.cancel_turn_tasks();
        self.discard_model_tool_cwd_change();
    }

    /// Ends the native session with everything it runs, for history it no
    /// longer matches.
    pub(crate) fn cancel_native_session(&mut self) {
        if self.core.native_session.is_some() {
            self.core.engine.send(UiCommand::EndNativeSession);
            self.end_native_session();
        }
    }

    /// Cancel tasks and invalidate late callbacks without finalizing the turn again.
    pub(crate) fn cancel_turn_work(&mut self) {
        self.cancel_request_hook();
        self.cancel_pending_rewind();
        self.platform.set_sleep_inhibited(false);
        self.core.engine.send(UiCommand::Cancel);
        self.forget_native_work();
        self.cancel_turn_lua_tasks();
        self.conversation.invalidate_turn_callbacks();
        self.busy_stack.clear();
        self.discard_pending_transcript_work();
        self.clear_compaction_preview();
    }

    /// Stop the engine turn without saving session or triggering auto-compact; used before rewind/clear.
    pub(crate) fn cancel_agent(&mut self) {
        if self.cancel_session_handoff() {
            return;
        }
        let turn_id = self.conversation.active().map(|turn| turn.turn_id);
        self.cancel_turn_work();
        self.conversation.set_turn_pause(Some(TurnPause {
            kind: Some(protocol::EngineAskErrorKind::Cancelled),
            retry_at_ms: None,
        }));
        // A turn is ending without going through `finish_turn`. Commit any
        // in-flight streaming buffers so the post-cancel state honors the
        // "no agent ⇒ no active stream" invariant (an empty thinking delta
        // arrived right before this can leave an empty `active_thinking`
        // sentinel that lingers past `agent = None`).
        self.flush_streaming_thinking();
        self.flush_streaming_text();
        self.clear_tool_drafts();
        self.conversation.clear_pending_history_appends();
        let meta = self.working.finish(TurnOutcome::Cancelled);
        self.record_finished_turn_state(meta);
        self.sync_agent_mode_applied();
        self.sync_reasoning_effort_applied();
        if let Some(turn_id) = turn_id {
            self.conversation.mark_terminal(turn_id);
        }
        self.save_session();
        self.prompt.clear_queue();
    }

    pub(crate) fn resume_paused_turn(&mut self, token: Option<u64>) -> bool {
        let Some(pause) = self.conversation.turn_pause() else {
            return false;
        };
        if self.prompt_input_is_busy()
            || self.modal_blocks_agent()
            || token.is_some_and(|token| self.conversation.continuation_token() != Some(token))
        {
            return false;
        }
        let scope = self.conversation.turn_command().clone();
        let sent_at_ms = engine::clock::unix_time_ms(self.core.clock.as_ref());
        let turn = self.begin_command_request_turn(
            String::new(),
            String::new(),
            scope,
            CommandTurnStart::ContinueFromLast,
            sent_at_ms,
        );
        let started = turn.is_some() || self.turn_submission_is_pending();
        self.conversation.set_active(turn);
        if !started {
            self.conversation.set_turn_pause(Some(pause));
            self.conversation.clear_continuation();
        }
        started
    }

    pub(crate) fn discard_turn(&mut self, end: crate::app::TurnEnd) {
        if matches!(end, crate::app::TurnEnd::Cancelled) && self.cancel_session_handoff() {
            return;
        }
        self.cancel_request_hook();
        if self.conversation.is_active() {
            let start_queued = self.finish_turn(end);
            self.conversation.clear_active();
            if start_queued {
                self.start_next_queued_input_if_idle();
            }
        } else if matches!(end, crate::app::TurnEnd::Cancelled) {
            if self.conversation.turn_pause().is_none() {
                self.conversation.set_turn_pause(Some(TurnPause {
                    kind: Some(protocol::EngineAskErrorKind::Cancelled),
                    retry_at_ms: None,
                }));
            }
            self.cancel_turn_work();
            // Archive an interrupted outcome so the prompt bar shows
            // "interrupted" rather than falling back to idle/done.
            self.working.finish(TurnOutcome::Cancelled);
            self.cancel_pending_turn_submission();
        }
    }

    pub(crate) fn finish_turn(&mut self, end: crate::app::TurnEnd) -> bool {
        self.cancel_request_hook();
        let _perf = smelt_perf::perf::begin("tui:finish_turn");
        use crate::app::TurnEnd;

        let turn_id = self.conversation.active().map(|turn| turn.turn_id);

        match end {
            TurnEnd::Cancelled => self.cancel_turn_work(),
            TurnEnd::Complete | TurnEnd::Errored { .. } => {
                self.platform.set_sleep_inhibited(false);
            }
        }

        let interrupted = !matches!(end, TurnEnd::Complete);
        let (error_kind, retry_at_ms) = match &end {
            TurnEnd::Errored { kind, retry_at_ms } => (*kind, *retry_at_ms),
            _ => (None, None),
        };
        self.conversation.set_turn_pause(match end {
            TurnEnd::Complete => None,
            TurnEnd::Cancelled => Some(TurnPause {
                kind: Some(protocol::EngineAskErrorKind::Cancelled),
                retry_at_ms: None,
            }),
            TurnEnd::Errored { kind, retry_at_ms } => Some(TurnPause { kind, retry_at_ms }),
        });
        let resumable = interrupted && is_resumable_turn_error(error_kind);
        let continuation_token = if !interrupted || resumable {
            Some(self.conversation.issue_continuation_token())
        } else {
            self.conversation.clear_continuation();
            None
        };
        self.core.signals.emit_dyn(
            "turn_end",
            std::rc::Rc::new(smelt_core::signals::TurnEnd {
                cancelled: interrupted,
                continuation_token,
                error_kind: error_kind.map(|kind| kind.as_str().to_string()),
                retry_at_ms,
            }),
        );
        {
            let _perf = smelt_perf::perf::begin("tui:finish_turn:pump_lua");
            self.pump_lua();
        }
        {
            let _perf = smelt_perf::perf::begin("tui:finish_turn:flush_streams");
            self.flush_streaming_thinking();
            self.flush_streaming_text();
            self.clear_tool_drafts();
            self.finish_transcript_turn();
        }

        if matches!(end, TurnEnd::Complete) {
            // An append with follow-up delivery still pending at the turn boundary
            // was not included in an LLM request. Keep one as the follow-up trigger;
            // any others are committed below as context for that same turn.
            if let Some(note) = self.conversation.take_pending_follow_up_note() {
                self.prompt.queue_front(
                    crate::app::QueueStage::Turn,
                    crate::app::QueuedInput::ProcessStatus(note),
                );
            }
        }

        let (meta, start_queued) = {
            let _perf = smelt_perf::perf::begin("tui:finish_turn:working_finish");
            match end {
                TurnEnd::Complete => {
                    let start_queued = !self.prompt.queue_is_empty() && !self.busy_stack.is_busy();
                    let meta = if start_queued {
                        self.working
                            .finish_and_continue(TurnOutcome::Done, TurnPhase::Working)
                    } else {
                        self.working.finish(TurnOutcome::Done)
                    };
                    self.clear_prompt_prediction();
                    (meta, start_queued)
                }
                TurnEnd::Cancelled => {
                    self.conversation.retain_session_history_appends();
                    let meta = self.working.finish(TurnOutcome::Cancelled);
                    self.drain_queued_inputs_into_prompt();
                    self.restore_session_metadata_after_rewind(self.session_history_len());
                    (meta, false)
                }
                TurnEnd::Errored { .. } => {
                    let meta = self.working.finish(TurnOutcome::Errored);
                    (meta, false)
                }
            }
        };

        {
            let _perf = smelt_perf::perf::begin("tui:finish_turn:document_state");
            self.record_finished_turn_state(meta);
        }
        if matches!(end, TurnEnd::Complete | TurnEnd::Errored { .. }) {
            self.apply_pending_history_appends_for_request();
        }
        self.sync_agent_mode_applied();
        self.sync_reasoning_effort_applied();
        if let Some(turn_id) = turn_id {
            self.conversation.mark_terminal(turn_id);
        }
        if matches!(end, TurnEnd::Complete) {
            self.schedule_session_save();
        } else {
            self.save_session();
        }
        if let Some(turn_id) = turn_id {
            if let Some(warning) = self.core.session_control.finish_turn(
                &self.conversation.session().id,
                turn_id,
                matches!(end, TurnEnd::Complete) && self.completion_can_apply(),
            ) {
                self.notify_warn(warning.into());
            }
        }
        start_queued
    }

    /// Whether nothing continues the work that just ended, so a session
    /// completion it requested can apply. Background work does not count: it
    /// clears the completion once it wakes the agent.
    pub(super) fn completion_can_apply(&self) -> bool {
        self.prompt.queue_is_empty()
            && !self.busy_stack.is_busy()
            && !self.continuation_is_scheduled()
            && self.public_attention_reason().is_none()
    }

    /// Invokes the Lua handler for a plugin-defined tool; synchronous handlers resolve immediately, async ones park until `drive_tasks` completes them.
    pub(crate) fn handle_tool_call(
        &mut self,
        request_id: u64,
        invocation_id: protocol::InvocationId,
        call_id: String,
        tool_name: String,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        let mode = self.core.config.mode.clone();
        let session_id = self.conversation.session().id.clone();
        let artifact_dir = self.current_artifact_dir();
        let now = self.core.clock.instant_now();
        let lua = self.lua.execution();
        let lua_call_id = call_id.clone();
        let (invocation, result) = crate::lua::scope_app(self, move || {
            lua.execute_tool_with_context(
                &tool_name,
                &args,
                crate::lua::ToolCallIds {
                    invocation_id,
                    request_id,
                    call_id: &lua_call_id,
                },
                crate::lua::ToolEnv {
                    mode,
                    session_id: &session_id,
                    artifact_dir: &artifact_dir,
                },
                now,
            )
        });
        match result {
            crate::lua::ToolExecResult::Immediate {
                content,
                is_error,
                metadata,
                display_content,
                attachment,
            } => {
                self.complete_lua_tool(
                    invocation,
                    call_id,
                    LuaToolCompletion {
                        content,
                        is_error,
                        metadata,
                        display_content,
                        attachment,
                    },
                );
            }
            crate::lua::ToolExecResult::Pending => {}
        }
    }

    pub(super) fn complete_lua_tool(
        &mut self,
        invocation: smelt_core::lua::ToolInvocationContext,
        call_id: String,
        completion: LuaToolCompletion,
    ) {
        let cwd_change = match self.defer_tool_cwd_result(invocation, !completion.is_error) {
            Ok(true) => {
                self.park_cwd_tool_completion(call_id, completion);
                return;
            }
            Ok(false) => Ok(false),
            Err(error) => Err(error),
        };
        self.finish_lua_tool(invocation, call_id, completion, cwd_change);
    }

    pub(super) fn finish_lua_tool(
        &mut self,
        invocation: smelt_core::lua::ToolInvocationContext,
        call_id: String,
        completion: LuaToolCompletion,
        cwd_change: Result<bool, String>,
    ) {
        let LuaToolCompletion {
            mut content,
            mut is_error,
            mut metadata,
            display_content,
            attachment,
        } = completion;
        match cwd_change {
            Ok(true) => {
                self.refresh_active_turn_permissions();
                annotate_cwd_metadata(&mut metadata, true, None);
            }
            Ok(false) => {}
            Err(error) => {
                if !content.trim().is_empty() {
                    content = format!("{error}\n\nOriginal tool result:\n{content}");
                } else {
                    content.clone_from(&error);
                }
                is_error = true;
                annotate_cwd_metadata(&mut metadata, false, Some(&error));
            }
        }
        self.core.engine.send(protocol::UiCommand::ToolResult {
            request_id: invocation.request_id,
            invocation_id: invocation.invocation_id,
            call_id,
            content,
            is_error,
            metadata,
            display_content,
            attachment: attachment.map(|attachment| *attachment),
        });
    }

    pub(crate) fn resolve_model_target(&mut self) -> Option<protocol::ModelTarget> {
        let Some(active) = self.core.config.active_model().cloned() else {
            self.notify_operation_error_sticky(
                NotificationOperation::TurnStart,
                "no model is available; configure a provider or wait for model refresh".into(),
            );
            return None;
        };
        let retry_missing_credentials = matches!(
            active.availability,
            smelt_core::ModelAvailability::Unavailable {
                reason: smelt_core::ModelUnavailableReason::MissingCredentials,
            }
        );
        if matches!(
            active.availability,
            smelt_core::ModelAvailability::Unavailable { .. }
        ) && !retry_missing_credentials
        {
            self.notify_operation_error_sticky(
                NotificationOperation::TurnStart,
                format!("model '{}' is unavailable", active.key),
            );
            return None;
        }
        let Some(api_key) = self.resolve_api_key_for_env(&active.api_key_env) else {
            if let Some(current) = self.core.config.active_model_mut() {
                current.availability = smelt_core::ModelAvailability::Unavailable {
                    reason: smelt_core::ModelUnavailableReason::MissingCredentials,
                };
                self.core.config.revision = self.core.config.revision.wrapping_add(1);
            }
            return None;
        };
        if retry_missing_credentials {
            if let Some(current) = self.core.config.active_model_mut() {
                current.availability = smelt_core::ModelAvailability::Available;
                self.core.config.revision = self.core.config.revision.wrapping_add(1);
            }
        }
        Some(active.target(api_key))
    }

    pub(crate) fn resolve_api_key_for_env(&mut self, key_env: &str) -> Option<String> {
        match lookup_api_key(key_env, |v| std::env::var(v)) {
            Ok(key) => Some(key),
            Err(err) => {
                self.notify_operation_error_sticky(NotificationOperation::TurnStart, err.message());
                None
            }
        }
    }

    pub(crate) fn handle_job_completed(&mut self, completion: smelt_core::process::JobCompletion) {
        self.core.jobs.acknowledge_completion(&completion.id);
        let id = display_safe_process_id(&completion.id);
        let event = protocol::ProcessStatusEvent::background_process_completed(
            id,
            completion.exit_code,
            completion.termination,
        );
        let note = protocol::HistoryNote::process_status_with_output(event, completion.output);
        if self.agent_is_running() || self.conversation.turn_pause().is_some() {
            self.queue_history_append(crate::app::PendingHistoryAppend::process_status(note));
        } else if self.prompt_input_is_busy() || !self.prompt.queue_is_empty() {
            self.prompt
                .try_queue_turn(crate::app::QueuedInput::ProcessStatus(note));
        } else {
            let turn = self.begin_process_status_turn(note);
            self.conversation.set_active(turn);
        }
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn session_path_grants(&self) -> Vec<smelt_core::permissions::SessionPathGrant> {
        self.core
            .permissions
            .approvals()
            .read()
            .unwrap()
            .session_path_grants()
    }

    pub(crate) fn grant_session_path(
        &mut self,
        mode: Option<protocol::AgentMode>,
        tool: String,
        access: smelt_core::permissions::PathAccess,
        dir: PathBuf,
    ) {
        let approval_store = self.core.permissions.approvals();
        let mut approvals = approval_store.write().unwrap();
        if let Some(mode) = mode {
            approvals.add_session_path_grant(mode, tool, access, dir);
        } else {
            approvals.add_session_path_trust(tool, access, dir);
        }
    }

    pub(crate) fn sync_permissions(
        &mut self,
        session_entries: Option<Vec<PermissionEntry>>,
        session_path_grants: Option<Vec<smelt_core::permissions::SessionPathGrant>>,
        workspace: Option<smelt_core::permissions::store::Replacement>,
        repository: Option<smelt_core::permissions::store::Replacement>,
    ) -> Result<(), String> {
        if workspace.is_some() && repository.is_some() {
            return Err(
                "sync one persisted permission scope at a time so each replacement is atomic"
                    .into(),
            );
        }

        let session_entries = session_entries
            .map(|entries| {
                let mut tools = Vec::new();
                let mut dirs = Vec::new();
                for entry in entries {
                    if entry.tool.is_empty() {
                        return Err("session permission tool name cannot be empty".to_string());
                    }
                    if entry.tool == "directory" {
                        if entry.pattern.is_empty() {
                            return Err("session permission directory cannot be empty".to_string());
                        }
                        dirs.push(PathBuf::from(entry.pattern));
                    } else {
                        let pattern = (entry.pattern != "*").then_some(entry.pattern);
                        if let Some(pattern) = pattern.as_deref() {
                            glob::Pattern::new(pattern).map_err(|error| {
                                format!("invalid session pattern for {}: {error}", entry.tool)
                            })?;
                        }
                        tools.push(smelt_core::permissions::SessionToolApproval {
                            tool: entry.tool,
                            pattern,
                        });
                    }
                }
                Ok((tools, dirs))
            })
            .transpose()?;

        if let Some(replacement) = workspace {
            self.core
                .permission_store
                .replace(
                    self.workspace.cwd(),
                    smelt_core::permissions::store::PersistenceScope::Workspace,
                    replacement,
                )
                .map_err(|error| format!("replace workspace permissions: {error}"))?;
        }
        if let Some(replacement) = repository {
            let repository_key = self
                .workspace
                .repository_permission_context()
                .map(|(key, _)| key.to_path_buf())
                .ok_or_else(|| "repository permissions require a Git repository".to_string())?;
            self.core
                .permission_store
                .replace(
                    &repository_key.to_string_lossy(),
                    smelt_core::permissions::store::PersistenceScope::Repository,
                    replacement,
                )
                .map_err(|error| format!("replace repository permissions: {error}"))?;
        }

        if session_entries.is_some() || session_path_grants.is_some() {
            let approval_store = self.core.permissions.approvals();
            let mut approvals = approval_store
                .write()
                .unwrap_or_else(|error| error.into_inner());
            if let Some((tools, dirs)) = session_entries {
                approvals.set_session_entries(tools, dirs);
            }
            if let Some(path_grants) = session_path_grants {
                approvals.set_session_path_grants(path_grants);
            }
        }
        Ok(())
    }

    pub(crate) fn revoke_permission(
        &mut self,
        scope: &str,
        tool: &str,
        pattern: &str,
    ) -> Result<bool, String> {
        if scope == "session" {
            return Ok(self
                .core
                .permissions
                .approvals()
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .remove_session_entry(tool, pattern));
        }

        let (root, persistence_scope) = match scope {
            "workspace" => (
                std::path::PathBuf::from(self.workspace.cwd()),
                smelt_core::permissions::store::PersistenceScope::Workspace,
            ),
            "repository" => (
                self.workspace
                    .repository_permission_context()
                    .map(|(key, _)| key.to_path_buf())
                    .ok_or_else(|| "repository permissions require a Git repository".to_string())?,
                smelt_core::permissions::store::PersistenceScope::Repository,
            ),
            _ => return Err(format!("unsupported permission scope `{scope}`")),
        };
        self.core
            .permission_store
            .remove(&root.to_string_lossy(), persistence_scope, tool, pattern)
            .map_err(|error| format!("remove {scope} permission: {error}"))
    }

    pub(crate) fn clear_session_scoped_permissions_for_session_boundary(&mut self) {
        self.core
            .permissions
            .approvals()
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .clear_session();
    }

    fn reject_failed_persistent_permission(
        &mut self,
        invocation_id: protocol::InvocationId,
        request_id: u64,
        error: String,
    ) -> bool {
        let message = format!("permission was not saved: {error}");
        self.notify_workspace_error_sticky(message.clone());
        self.send_permission_decision(request_id, false, Some(message));
        self.finish_tool(invocation_id, ToolStatus::Denied, None, None);
        self.conversation.remove_pending_tool(invocation_id);
        false
    }

    /// Resolves a confirm dialog choice; returns `true` if the agent should be cancelled.
    pub(crate) fn resolve_confirm(
        &mut self,
        (choice, message): (ConfirmChoice, Option<String>),
        invocation_id: protocol::InvocationId,
        request_id: u64,
        tool_name: &str,
    ) -> bool {
        let label = match &choice {
            ConfirmChoice::Yes => "approved",
            ConfirmChoice::Grant(option) => option.label.as_str(),
            ConfirmChoice::No => "denied",
        };
        if let Some(ref msg) = message {
            self.set_active_user_message(invocation_id, format!("{label}: {msg}"));
        }
        match choice {
            ConfirmChoice::Yes => {
                self.set_active_status(invocation_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::Grant(option) => {
                match option.target {
                    ApprovalTarget::Session => {
                        let approval_store = self.core.permissions.approvals();
                        let mut approvals = approval_store.write().unwrap();
                        for grant in option.grants {
                            approvals.add_session_grant(grant);
                        }
                    }
                    ApprovalTarget::Workspace { root } => {
                        let result = add_persisted_grants(
                            &self.core.permission_store,
                            &root,
                            smelt_core::permissions::store::PersistenceScope::Workspace,
                            &option.grants,
                        );
                        if let Err(error) = result {
                            return self.reject_failed_persistent_permission(
                                invocation_id,
                                request_id,
                                error,
                            );
                        }
                    }
                    ApprovalTarget::Repository { key } => {
                        let result = add_persisted_grants(
                            &self.core.permission_store,
                            &key,
                            smelt_core::permissions::store::PersistenceScope::Repository,
                            &option.grants,
                        );
                        if let Err(error) = result {
                            return self.reject_failed_persistent_permission(
                                invocation_id,
                                request_id,
                                error,
                            );
                        }
                    }
                }
                self.set_active_status(invocation_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::No => {
                self.lua.release_tool_preparation(invocation_id);
                let has_message = message.is_some();
                self.send_permission_decision(request_id, false, message);
                self.finish_tool(invocation_id, ToolStatus::Denied, None, None);
                if has_message {
                    self.conversation.remove_pending_tool(invocation_id);
                    false
                } else {
                    engine::log::entry(
                        engine::log::Level::Info,
                        "agent_stop",
                        &serde_json::json!({
                            "reason": "confirm_denied",
                            "tool": tool_name,
                        }),
                    );
                    self.conversation.clear_pending_tools();
                    true
                }
            }
        }
    }

    fn permission_decision_for_confirm(&self, req: &ConfirmRequest) -> Decision {
        if let Some(error) = req.tool_paths_error.as_ref() {
            return Decision::Error(error.clone());
        }
        self.active_permissions()
            .evaluate_tool_with_paths_and_approvals(
                self.conversation.applied_mode().clone(),
                smelt_core::permissions::ToolOrigin::Lua,
                &req.tool_name,
                &req.args,
                &req.tool_paths,
            )
            .decision
    }

    fn resolve_confirm_by_policy(
        &mut self,
        req: &ConfirmRequest,
        approved: bool,
        handle_id: Option<u64>,
        label: &'static str,
        pending: Option<&mut Vec<PendingTool>>,
    ) {
        if let Some(handle_id) = handle_id {
            self.core.confirms.take(handle_id);
            self.core.signals.emit_dyn(
                "confirm_resolved",
                std::rc::Rc::new(smelt_core::signals::ConfirmResolved {
                    handle_id,
                    decision: label.into(),
                }),
            );
        }

        if approved {
            self.set_active_status(req.invocation_id, ToolStatus::Pending);
            self.send_permission_decision(req.request_id, true, None);
        } else {
            self.send_permission_decision(req.request_id, false, None);
            self.finish_tool(req.invocation_id, ToolStatus::Denied, None, None);
            if let Some(pending) = pending {
                pending.retain(|pending| pending.invocation_id != req.invocation_id);
            } else {
                self.conversation.remove_pending_tool(req.invocation_id);
            }
        }
    }

    pub(crate) fn resolve_open_confirm_for_current_mode(&mut self, handle_id: u64) -> bool {
        let Some(req) = self
            .core
            .confirms
            .get(handle_id)
            .map(|entry| entry.req.clone())
        else {
            return false;
        };

        match self.permission_decision_for_confirm(&req) {
            Decision::Allow => {
                self.resolve_confirm_by_policy(&req, true, Some(handle_id), "auto_allow", None);
                true
            }
            Decision::Deny => {
                self.resolve_confirm_by_policy(&req, false, Some(handle_id), "auto_deny", None);
                true
            }
            Decision::Error(error) => {
                self.notify_workspace_error_sticky(error);
                false
            }
            Decision::Ask => false,
        }
    }

    /// Dispatches one engine-event control signal; returns the same
    /// `SessionControl` variant so callers can decide whether to continue
    /// draining, end the turn, or surface an error.
    pub(crate) fn dispatch_control(
        &mut self,
        ctrl: SessionControl,
        turn: &mut TurnState,
    ) -> SessionControl {
        self.dispatch_scoped_control(ctrl, &turn.permissions, &mut turn.pending)
    }

    pub(crate) fn dispatch_scoped_control(
        &mut self,
        ctrl: SessionControl,
        permissions: &smelt_core::permissions::Permissions,
        pending: &mut Vec<PendingTool>,
    ) -> SessionControl {
        let should_queue = self
            .timers
            .last_keypress
            .is_some_and(|t| t.elapsed() < Duration::from_millis(CONFIRM_DEFER_MS))
            && !self.prompt_buf().source().is_empty();

        match ctrl {
            SessionControl::Continue => SessionControl::Continue,
            SessionControl::Done => SessionControl::Done,
            SessionControl::Error { kind, retry_at_ms } => {
                SessionControl::Error { kind, retry_at_ms }
            }
            SessionControl::NeedsConfirm(mut req) => {
                if req.tool_name.is_empty() {
                    req.tool_name = pending.last().map(|p| p.name.clone()).unwrap_or_default();
                }
                if let Some(error) = req.tool_paths_error.clone() {
                    self.send_permission_decision(req.request_id, false, Some(error));
                    self.finish_tool(req.invocation_id, ToolStatus::Denied, None, None);
                    pending.retain(|pending| pending.invocation_id != req.invocation_id);
                    return SessionControl::Continue;
                }

                let outcome = permissions.evaluate_tool_with_paths_and_approvals(
                    self.conversation.applied_mode().clone(),
                    smelt_core::permissions::ToolOrigin::Lua,
                    &req.tool_name,
                    &req.args,
                    &req.tool_paths,
                );
                match outcome.decision {
                    Decision::Allow => {
                        self.resolve_confirm_by_policy(&req, true, None, "auto_allow", None);
                        return SessionControl::Continue;
                    }
                    Decision::Deny => {
                        self.resolve_confirm_by_policy(
                            &req,
                            false,
                            None,
                            "auto_deny",
                            Some(pending),
                        );
                        return SessionControl::Continue;
                    }
                    Decision::Error(ref error) => {
                        self.notify_workspace_error_sticky(error.clone());
                    }
                    Decision::Ask => {}
                }

                if should_queue {
                    self.set_active_status(req.invocation_id, ToolStatus::Confirm);
                    self.overlays.defer_confirm(req);
                    return SessionControl::Continue;
                }

                let options = permissions.approval_options(
                    &req.tool_name,
                    &req.approval_candidates,
                    &outcome,
                );
                req.grant_options = confirm_grant_options(
                    options.grant_sets,
                    self.workspace.cwd(),
                    self.workspace.repository_permission_context(),
                    self.core.env.home(),
                );

                self.close_focused_non_blocking_overlay();
                self.set_active_status(req.invocation_id, ToolStatus::Confirm);

                let snapshot = smelt_core::signals::ConfirmRequested {
                    handle_id: 0,
                    tool_name: protocol::displayed_tool_name(
                        req.provider.as_deref(),
                        &req.tool_name,
                    ),
                    summary: req.summary.clone(),
                    args: req.args.clone(),
                    grant_options: req.grant_options.clone(),
                };
                let handle_id = self.core.confirms.register(*req);
                self.core.signals.set_dyn(
                    "confirm_requested",
                    std::rc::Rc::new(smelt_core::signals::ConfirmRequested {
                        handle_id,
                        ..snapshot
                    }),
                );
                let lua = self.lua.execution();
                crate::lua::scope_app(self, move || lua.fire_confirm_open(handle_id));
                SessionControl::Continue
            }
        }
    }
}

fn confirm_grant_options(
    grant_sets: Vec<Vec<smelt_core::permissions::PermissionGrant>>,
    cwd: &str,
    repository: Option<(&std::path::Path, &std::path::Path)>,
    home: &std::path::Path,
) -> Vec<smelt_core::transcript_model::ConfirmApprovalOption> {
    let mut out = Vec::new();
    for (idx, grants) in grant_sets.into_iter().enumerate() {
        let subject = smelt_core::permissions::PermissionGrant::display_subjects(&grants, home);
        out.push(smelt_core::transcript_model::ConfirmApprovalOption {
            id: format!("grant_{idx}_session"),
            label: format!("allow {subject} for this session"),
            target: ApprovalTarget::Session,
            grants: grants.clone(),
        });
        out.push(smelt_core::transcript_model::ConfirmApprovalOption {
            id: format!("grant_{idx}_workspace"),
            label: format!(
                "allow {subject} in {} {}",
                if repository.is_some() {
                    "this worktree"
                } else {
                    "workspace"
                },
                pretty_cwd(cwd, home)
            ),
            target: ApprovalTarget::Workspace {
                root: std::path::PathBuf::from(cwd),
            },
            grants: grants.clone(),
        });
        if let Some((repository_key, display_root)) = repository {
            out.push(smelt_core::transcript_model::ConfirmApprovalOption {
                id: format!("grant_{idx}_repository"),
                label: format!(
                    "allow {subject} in project {} (all worktrees)",
                    pretty_path(display_root, home)
                ),
                target: ApprovalTarget::Repository {
                    key: repository_key.to_path_buf(),
                },
                grants,
            });
        }
    }
    out
}

fn pretty_cwd(cwd: &str, home: &std::path::Path) -> String {
    pretty_path(std::path::Path::new(cwd), home)
}

fn pretty_path(path: &std::path::Path, home: &std::path::Path) -> String {
    engine::paths::collapse_tilde_from(path, home)
        .to_string_lossy()
        .into_owned()
}

fn add_persisted_grants(
    store: &smelt_core::permissions::store::PermissionStore,
    root: &std::path::Path,
    scope: smelt_core::permissions::store::PersistenceScope,
    grants: &[smelt_core::permissions::PermissionGrant],
) -> Result<(), String> {
    store
        .add_grants(&root.to_string_lossy(), scope, grants)
        .map_err(|error| format!("save persisted permission: {error}"))
}

fn display_safe_process_id(id: &str) -> String {
    id.chars()
        .map(|ch| if ch.is_control() { '\u{FFFD}' } else { ch })
        .collect()
}

/// Reason an API-key env lookup failed; carries enough context for the
/// caller to surface a user-facing error.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApiKeyError {
    /// The env var is not set in the process environment.
    NotSet { var: String },
    /// The env var exists but its bytes are not valid Unicode.
    NotUnicode { var: String },
}

impl ApiKeyError {
    pub(crate) fn message(&self) -> String {
        match self {
            ApiKeyError::NotSet { var } => format!(
                "environment variable '{var}' is not set but is required for API authentication"
            ),
            ApiKeyError::NotUnicode { var } => format!(
                "environment variable '{var}' contains non-Unicode data and cannot be used as an API key"
            ),
        }
    }
}

/// Look up an API key value via the injected `get_env` resolver.
///
/// An empty `key_env` is treated as "no auth required" and returns an empty
/// key. Otherwise the resolver is queried; `VarError::NotPresent` and
/// `NotUnicode` map to typed errors so the dispatcher can format a stable
/// user-facing message.
///
/// The resolver indirection keeps tests off the process-global env.
pub(crate) fn lookup_api_key(
    key_env: &str,
    get_env: impl FnOnce(&str) -> Result<String, std::env::VarError>,
) -> Result<String, ApiKeyError> {
    if key_env.is_empty() {
        return Ok(String::new());
    }
    match get_env(key_env) {
        Ok(key) => Ok(key),
        Err(std::env::VarError::NotPresent) => Err(ApiKeyError::NotSet {
            var: key_env.to_string(),
        }),
        Err(std::env::VarError::NotUnicode(_)) => Err(ApiKeyError::NotUnicode {
            var: key_env.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_reader(app: &crate::app::test_harness::TestApp) -> smelt_store::SessionReader {
        app.app
            .core
            .sessions
            .reader(&app.app.conversation.session().id)
            .expect("open stored session")
    }

    fn stored_history(reader: &smelt_store::SessionReader) -> Vec<protocol::HistoryItem> {
        let head = reader.head().expect("read session head");
        reader
            .history_range(0, head.history_len.get())
            .expect("read session history")
    }

    fn stored_last_turn_id(app: &crate::app::test_harness::TestApp) -> u64 {
        session_reader(app)
            .state()
            .expect("read session state")
            .metadata
            .last_turn_id
    }

    fn save_record_backed_session(
        app: &crate::app::test_harness::TestApp,
        session: &smelt_core::session::Session,
        transcript_text: &str,
    ) {
        app.app.core.sessions.save(session).expect("save session");
        let address = app.app.core.sessions.address(session.id.clone());
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(smelt_core::Block::Text {
            content: transcript_text.into(),
        });
        crate::persist::write_transcript_record_suffix(
            &address,
            0,
            &transcript.history.block_records(),
        )
        .expect("save canonical transcript records");
    }

    #[test]
    fn turn_lifecycle_owns_continuation_and_callback_generations() {
        let mut lifecycle = TurnLifecycle::new(
            protocol::AgentMode::parse("normal").unwrap(),
            protocol::ReasoningEffort::Off,
        );

        assert_eq!(lifecycle.cancel_generation(), 0);
        lifecycle.invalidate_turn_callbacks();
        assert_eq!(lifecycle.cancel_generation(), 1);

        let token = lifecycle.issue_continuation_token();
        assert_eq!(token, 1);
        assert!(!lifecycle.consume_continuation(token + 1));
        assert_eq!(lifecycle.pending_continuation_token(), Some(token));
        assert!(lifecycle.consume_continuation(token));
        assert_eq!(lifecycle.pending_continuation_token(), None);
    }

    #[test]
    fn turn_lifecycle_replaces_mode_changes_and_removes_base_mode() {
        let base = protocol::AgentMode::parse("normal").unwrap();
        let mut lifecycle = TurnLifecycle::new(base.clone(), protocol::ReasoningEffort::Off);

        lifecycle.queue_history_append(
            PendingHistoryAppend::mode_change("plan".to_string(), "plan mode".to_string()),
            Some(&base),
        );
        lifecycle.queue_history_append(
            PendingHistoryAppend::mode_change("auto".to_string(), "auto mode".to_string()),
            Some(&base),
        );

        assert_eq!(lifecycle.pending_history_append_count(), 1);
        assert_eq!(lifecycle.pending_history_appends()[0].mode(), Some("auto"));

        lifecycle.queue_history_append(
            PendingHistoryAppend::mode_change("normal".to_string(), "normal mode".to_string()),
            Some(&base),
        );
        assert_eq!(lifecycle.pending_history_append_count(), 0);
    }

    #[test]
    fn turn_lifecycle_replaces_and_removes_named_context() {
        let mut lifecycle = TurnLifecycle::new(
            protocol::AgentMode::parse("normal").unwrap(),
            protocol::ReasoningEffort::Off,
        );

        lifecycle.replace_or_push_history_append(PendingHistoryAppend::context(
            "project".to_string(),
            "first".to_string(),
        ));
        lifecycle.replace_or_push_history_append(PendingHistoryAppend::context(
            "project".to_string(),
            "second".to_string(),
        ));
        assert_eq!(lifecycle.pending_history_append_count(), 1);
        assert_eq!(
            lifecycle.pending_context_note("project"),
            Some(Some("second"))
        );

        lifecycle.replace_or_push_history_append(PendingHistoryAppend::clear_context(
            "project".to_string(),
        ));
        assert_eq!(lifecycle.pending_history_append_count(), 1);
        assert_eq!(lifecycle.pending_context_note("project"), Some(None));
        assert_eq!(lifecycle.pending_context_note("other"), None);
    }

    #[test]
    fn conversation_cancels_context_updates_that_restore_committed_state() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.conversation.append_history_item(HistoryItem::note(
            protocol::HistoryNote::named_context("project", "committed"),
        ));

        app.app
            .set_context_note("project".into(), Some("pending".into()));
        assert_eq!(app.app.conversation.pending_history_append_count(), 1);
        app.app
            .set_context_note("project".into(), Some("committed".into()));
        assert_eq!(app.app.conversation.pending_history_append_count(), 0);

        app.app.set_context_note("project".into(), None);
        assert_eq!(app.app.conversation.pending_history_append_count(), 1);
        app.app
            .set_context_note("project".into(), Some("committed".into()));
        assert_eq!(app.app.conversation.pending_history_append_count(), 0);
    }

    #[test]
    fn turn_lifecycle_extracts_matching_history_append() {
        let mut lifecycle = TurnLifecycle::new(
            protocol::AgentMode::parse("normal").unwrap(),
            protocol::ReasoningEffort::Off,
        );
        let append = PendingHistoryAppend::context("project".to_string(), "text".to_string());
        let item = append.history_item();
        lifecycle.replace_or_push_history_append(append);

        let extracted = lifecycle
            .take_matching_history_append(&item)
            .expect("matching append exists");
        assert_eq!(extracted.history_item(), item);
        assert_eq!(lifecycle.pending_history_append_count(), 0);
    }

    #[test]
    fn turn_lifecycle_retains_only_session_scoped_history() {
        let base = protocol::AgentMode::parse("normal").unwrap();
        let mut lifecycle = TurnLifecycle::new(base.clone(), protocol::ReasoningEffort::Off);
        lifecycle.queue_history_append(
            PendingHistoryAppend::mode_change("plan".to_string(), "plan mode".to_string()),
            Some(&base),
        );
        lifecycle.replace_or_push_history_append(PendingHistoryAppend::context(
            "project".to_string(),
            "text".to_string(),
        ));

        lifecycle.retain_session_history_appends();

        assert_eq!(lifecycle.pending_history_append_count(), 1);
        assert_eq!(lifecycle.pending_history_appends()[0].mode(), Some("plan"));
    }

    #[test]
    fn turn_lifecycle_keeps_active_identity_while_state_is_dispatched() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(41);

        let dispatch = app
            .app
            .conversation
            .begin_dispatch()
            .expect("active turn enters dispatch");
        assert!(!app.app.conversation.is_active());
        assert_eq!(app.app.conversation.active_id(), Some(41));

        app.app.conversation.finish_dispatch(dispatch);
        assert_eq!(
            app.app.conversation.active().map(|turn| turn.turn_id),
            Some(41)
        );
    }

    #[test]
    fn dispatched_turn_is_restored_before_panic_resumes() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(42);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.app.with_dispatched_turn::<()>(|_, _| panic!("boom"));
        }));

        assert!(panic.is_err());
        assert_eq!(app.app.conversation.active_id(), Some(42));
        assert_eq!(
            app.app.conversation.active().map(|turn| turn.turn_id),
            Some(42)
        );
    }

    fn process_status_blocks(app: &crate::app::test_harness::TestApp) -> Vec<String> {
        let history = app.app.conversation.transcript().history();
        history
            .order
            .iter()
            .filter_map(|id| history.block(*id))
            .filter_map(|block| match block {
                Block::ProcessStatus { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn user_blocks(app: &crate::app::test_harness::TestApp) -> Vec<String> {
        let history = app.app.conversation.transcript().history();
        history
            .order
            .iter()
            .filter_map(|id| history.block(*id))
            .filter_map(|block| match block {
                Block::User { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn perf_value_max(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
        snapshot
            .values
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.max)
            .unwrap_or(0)
    }

    fn assert_perf_value_absent(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
        let value = perf_value_max(snapshot, label);
        assert_eq!(value, 0, "{label} recorded {value}, expected no samples");
    }

    fn assert_perf_value_at_most(snapshot: &smelt_perf::perf::Snapshot, label: &str, max: u64) {
        let value = perf_value_max(snapshot, label);
        assert!(
            value <= max,
            "{label} recorded max {value}, expected <= {max}"
        );
    }

    fn assert_no_full_request_start_reads(snapshot: &smelt_perf::perf::Snapshot) {
        for label in [
            "store:history:read_all",
            "store:history:read_all_rows",
            "store:session:load_full_snapshot",
            "store:session:full_snapshot_rows_read",
            "store:transcript:search_blob_full",
            "store:transcript:read_records_full",
            "store:transcript:records_full_loaded",
            "session:rebuild_transcript_full_fallback",
            "session:display_only_load_full",
            "transcript:build_from_session:history_items",
        ] {
            assert_perf_value_absent(snapshot, label);
        }
    }

    fn large_saved_session_app(history_len: usize) -> crate::app::test_harness::TestApp {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user 0".into());
        session.history = (0..history_len)
            .map(|idx| {
                if idx.is_multiple_of(2) {
                    HistoryItem::user(Content::text(format!("old user {idx}")))
                } else {
                    HistoryItem::Assistant(protocol::AssistantStep::terminal(
                        Some(Content::text(format!("old assistant {idx}"))),
                        None,
                        Vec::new(),
                    ))
                }
            })
            .collect();
        app.load_session(session);
        app.app.restore_screen();
        app.app.ensure_current_context_note();
        app.app.apply_pending_history_appends_for_request();
        app.app.save_session();
        app.app.flush_persist();
        app
    }

    #[test]
    fn starting_user_turn_dismisses_visible_notification() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .notify_turn_error_sticky("rate limit exceeded".to_string());
        assert!(app.app.notification_win().is_some());

        app.start_submitted_turn("try again");

        assert!(app.app.notification_win().is_none());
    }

    #[test]
    fn starting_user_turn_dismisses_turn_start_failure() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.notify_operation_error_sticky(
            crate::app::NotificationOperation::TurnStart,
            "missing credentials".to_string(),
        );

        app.start_submitted_turn("try again");

        assert!(app.app.notification_win().is_none());
    }

    #[test]
    fn starting_user_turn_preserves_session_notification() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let session_id = app.app.conversation.session().id.clone();
        app.app
            .notify_session_error_sticky("session failure".to_string());

        app.start_submitted_turn("try again");

        assert!(app.app.overlays.notification().is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Session(owner_session_id)
                    if owner_session_id == &session_id
            ) && notification.summary == "session failure"
        }));
    }

    #[test]
    fn starting_user_turn_preserves_application_notification() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .notify_application_error_sticky("application failure".to_string());

        app.start_submitted_turn("try again");

        assert!(app.app.overlays.notification().is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Application
            ) && notification.summary == "application failure"
        }));
    }

    #[test]
    fn starting_command_continuation_dismisses_visible_notification() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_submitted_turn("previous");
        app.app.discard_turn(crate::app::TurnEnd::Complete);
        app.wait_for_turn_dispatch();
        app.app
            .notify_turn_error_sticky("quota exceeded".to_string());
        assert!(app.app.notification_win().is_some());

        let turn = app.app.begin_command_request_turn(
            "continue".into(),
            String::new(),
            crate::app::CommandScope::default(),
            crate::app::CommandTurnStart::ContinueFromLast,
            0,
        );
        assert!(turn.is_some());

        assert!(app.app.notification_win().is_none());
    }

    #[test]
    fn user_turn_commits_request_before_dispatch_without_duplicate_history() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.start_submitted_turn("first request");

        assert!(matches!(
            app.app.conversation.session().history.last(),
            Some(HistoryItem::User { content, .. }) if content.text_content() == "first request"
        ));
        let payload = app
            .drain_engine_sends()
            .into_iter()
            .find_map(|cmd| match cmd {
                protocol::UiCommand::StartTurn(payload) => Some(payload),
                _ => None,
            })
            .expect("turn dispatched");
        assert_eq!(
            payload.history.items.len(),
            app.app.conversation.session().history.len() - 1
        );
        assert_eq!(
            payload.input.provider_content().text_content(),
            "first request"
        );

        app.app.flush_persist();
        let loaded = crate::app::history::load_saved_session(
            &app.app.core.sessions,
            &app.app.conversation.session().id,
        )
        .expect("session saved");
        assert!(matches!(
            loaded.history.last(),
            Some(HistoryItem::User { content, .. }) if content.text_content() == "first request"
        ));
    }

    #[test]
    fn terminal_turn_saves_final_history_and_turn_id() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_submitted_turn("finish me");
        let turn_id = app.current_turn_id().unwrap();
        app.app
            .session_append_history(HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(Content::text("finished")),
                None,
                Vec::new(),
            )));

        app.app.discard_turn(crate::app::TurnEnd::Complete);
        app.app.save_session_and_flush();

        assert_eq!(stored_last_turn_id(&app), turn_id);
        assert!(matches!(
            stored_history(&session_reader(&app)).last(),
            Some(HistoryItem::Assistant { .. })
        ));
    }

    #[test]
    fn escape_interrupt_is_not_blocked_by_busy_persistence() {
        let mut app = crate::app::test_harness::TestApp::builder()
            .with_vim(true)
            .build();
        app.start_submitted_turn("interrupt while persistence is busy");
        app.press(crossterm::event::KeyCode::Esc);
        assert!(app.agent_running(), "first Escape is the local Vim action");
        app.app.flush_persist();
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        app.press(crossterm::event::KeyCode::Esc);
        let elapsed = started.elapsed();
        release.send(()).expect("resume persistence actor");

        assert!(!app.agent_running(), "Escape cancels the agent");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "interrupt waited {elapsed:?} for busy session persistence"
        );
        let flush = app.app.flush_persist();
        assert!(
            matches!(flush, crate::persist::PersistenceFlushOutcome::Durable),
            "cancel did not become durable: {flush:?}"
        );
    }

    #[test]
    fn first_message_rewind_restores_input_while_the_writer_is_busy() {
        for vim in [false, true] {
            for to_start in [false, true] {
                let mut app = crate::app::test_harness::TestApp::builder()
                    .with_vim(vim)
                    .build();
                app.start_submitted_turn("first request");
                app.app.flush_persist();
                let turn_id = app.app.conversation.active_id().unwrap();
                let release = app.app.conversation.pause_persistence();

                if to_start {
                    app.app.rewind_to_history_index(None, vim);
                    app.type_text("first request");
                } else {
                    app.press(crossterm::event::KeyCode::Esc);
                    app.press(crossterm::event::KeyCode::Esc);
                }
                assert!(!app.agent_running());
                assert_eq!(app.state().prompt_text, "first request");
                assert!(app.app.conversation.turn_pause().is_none());
                assert!(app
                    .app
                    .model_history()
                    .iter()
                    .all(|item| !matches!(item, HistoryItem::User { .. })));

                app.press(crossterm::event::KeyCode::Enter);
                app.wait_for_turn_dispatch();
                assert!(app.agent_running());
                assert!(app.state().queued_inputs.is_empty());
                release.send(()).unwrap();
                let flush = app.app.flush_persist();
                assert!(
                    matches!(flush, crate::persist::PersistenceFlushOutcome::Durable),
                    "{flush:?}"
                );
                assert!(app.app.prompt.queue_is_empty());
                assert_eq!(
                    app.app.model_history().iter().filter(|item| matches!(item, HistoryItem::User { content, .. } if content.text_content() == "first request")).count(),
                    1
                );
                assert_ne!(app.app.conversation.active_id(), Some(turn_id));
            }
        }
    }

    #[test]
    fn rewind_during_active_turn_is_not_blocked_by_persistence() {
        let mut app = large_saved_session_app(4);
        let rewind_history_idx = app.app.rewind_turns().unwrap()[0].history_idx;
        app.start_submitted_turn("rewind this active turn");
        app.app.flush_persist();
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        app.app
            .rewind_to_history_index(Some(rewind_history_idx), false);
        let elapsed = started.elapsed();
        release.send(()).expect("resume persistence actor");

        assert!(!app.agent_running(), "rewind cancels the active turn");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "rewind waited {elapsed:?} for canonical turn cancellation"
        );
        app.app.flush_persist();
        assert_eq!(stored_history(&session_reader(&app)).len(), 0);
    }

    #[test]
    fn reset_fails_promptly_when_current_session_persistence_is_busy() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("durable baseline"),
        ));
        app.app.save_session_and_flush();
        let original_id = app.app.conversation.session().id.clone();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("preserve this change"),
        ));
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        app.app.reset_session();
        let elapsed = started.elapsed();

        assert_eq!(
            app.app.conversation.session().id,
            original_id,
            "failed reset keeps the current session"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "reset waited {elapsed:?} for busy session persistence"
        );
        release.send(()).expect("resume persistence actor");
        app.app.flush_persist();
        assert_eq!(stored_history(&session_reader(&app)).len(), 2);
    }

    #[test]
    fn session_switch_fails_promptly_when_current_persistence_is_busy() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("switch target"),
        ));
        app.app.push_block(smelt_core::Block::Text {
            content: "switch target transcript".into(),
        });
        app.app.save_session_and_flush();
        let target_id = app.app.conversation.session().id.clone();
        app.app.reset_session();
        let current_id = app.app.conversation.session().id.clone();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("current baseline"),
        ));
        app.app.save_session_and_flush();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("preserve current change"),
        ));
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        let loaded = app.app.load_session_by_id(&target_id);
        let elapsed = started.elapsed();

        assert!(!loaded, "busy persistence prevents the session switch");
        assert_eq!(
            app.app.conversation.session().id,
            current_id,
            "failed switch keeps the current session"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "session switch waited {elapsed:?} for busy session persistence"
        );
        release.send(()).expect("resume persistence actor");
        app.app.flush_persist();
        assert_eq!(stored_history(&session_reader(&app)).len(), 2);
    }

    #[test]
    fn fork_fails_promptly_when_current_persistence_is_busy() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("durable fork baseline"),
        ));
        app.app.save_session_and_flush();
        let original_id = app.app.conversation.session().id.clone();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("preserve before fork"),
        ));
        let session_ids_before = app
            .app
            .core
            .sessions
            .store()
            .ids_with_prefix("", u32::MAX)
            .expect("list sessions before fork");
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        app.app.fork_session();
        let elapsed = started.elapsed();

        assert_eq!(
            app.app.conversation.session().id,
            original_id,
            "failed fork keeps the current session"
        );
        assert_eq!(
            app.app
                .core
                .sessions
                .store()
                .ids_with_prefix("", u32::MAX)
                .expect("list sessions after fork"),
            session_ids_before,
            "failed fork does not publish a partial destination"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "fork waited {elapsed:?} for busy session persistence"
        );
        release.send(()).expect("resume persistence actor");
        app.app.flush_persist();
        assert_eq!(stored_history(&session_reader(&app)).len(), 2);
    }

    #[test]
    fn deleting_a_fork_source_keeps_the_fork_while_its_writer_is_busy() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("fork before delete"),
        ));
        app.app.push_block(smelt_core::Block::Text {
            content: "fork before delete transcript".into(),
        });
        app.app.save_session_and_flush();
        let source_id = app.app.conversation.session().id.clone();
        app.fork_session();
        assert_ne!(app.app.conversation.session().id, source_id);
        app.set_lua_string_global("SOURCE_SESSION_ID", source_id.clone())
            .expect("install source session id");
        let release = app.app.conversation.pause_persistence();

        let result = app.run_lua_result("smelt.session.delete(SOURCE_SESSION_ID)");

        release.send(()).expect("resume persistence actor");
        assert!(result.is_ok(), "{result:?}");
        assert!(app.app.core.sessions.reader(&source_id).is_err());
        app.app.flush_persist();
        assert_eq!(stored_history(&session_reader(&app)).len(), 1);
    }

    #[test]
    fn session_load_rejects_missing_transcript_records_off_thread() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut target =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        target
            .history
            .push(protocol::HistoryItem::note(protocol::HistoryNote::context(
                "recordless resume target",
            )));
        let target_id = target.id.clone();
        app.app
            .core
            .sessions
            .save(&target)
            .expect("save recordless resume target");
        let current_id = app.app.conversation.session().id.clone();
        app.app
            .session_load
            .set_delay(std::time::Duration::from_millis(500));
        app.set_lua_string_global("TARGET_SESSION_ID", target_id)
            .expect("install target session id");

        let started = std::time::Instant::now();
        app.run_lua_result("smelt.session.load(TARGET_SESSION_ID)")
            .expect("request session load through the user-facing API");
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "session load blocked the UI for {elapsed:?}"
        );
        assert_eq!(app.app.conversation.session().id, current_id);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(event) = app.try_recv_app_event() {
                let completed = matches!(&event, crate::app::AppEvent::SessionLoadCompleted(_));
                app.handle_app_event(event);
                if completed {
                    break;
                }
            } else {
                assert!(
                    std::time::Instant::now() < deadline,
                    "background session load did not complete"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        assert_eq!(app.app.conversation.session().id, current_id);
    }

    #[test]
    fn session_load_adoption_does_not_wait_for_writer_release() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.ensure_writer_ready();
        app.app.show_user_message("retained source", Vec::new());
        app.save_session_and_flush();
        let source_id = app.session_snapshot().id.clone();
        let release = app.app.conversation.pause_persistence();
        let target =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        let target_id = target.id.clone();
        save_record_backed_session(&app, &target, "resume destination");
        app.set_lua_string_global("TARGET_SESSION_ID", target_id.clone())
            .unwrap();
        app.run_lua_result("smelt.session.load(TARGET_SESSION_ID)")
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let elapsed = loop {
            if let Some(event) = app.try_recv_app_event() {
                let completed = matches!(&event, crate::app::AppEvent::SessionLoadCompleted(_));
                let started = std::time::Instant::now();
                app.handle_app_event(event);
                if completed {
                    break started.elapsed();
                }
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        };
        wait_for_source_close(&mut app);
        let retained_id = app.session_snapshot().id.clone();
        let retained_actor = app.app.conversation.has_persistence();
        let display = app.render_unsettled_silent().text();
        app.animate_with_virtual_clock();
        let mut dots = std::collections::HashSet::new();
        for _ in 0..8 {
            let started = std::time::Instant::now();
            let rendered = app.advance_animation(150);
            let frame = rendered.text();
            let dot = app
                .working_dot_style(&rendered)
                .expect("styled working dot");
            assert!(frame.contains('●'), "{frame}");
            dots.insert(dot);
            assert!(started.elapsed() < std::time::Duration::from_millis(250));
        }
        assert!(
            dots.len() >= 4,
            "the coal must keep breathing while the UI waits"
        );
        app.type_text("draft during resume");
        assert_eq!(app.state().prompt_text, "draft during resume");
        for key in [3, 12] {
            let started = std::time::Instant::now();
            app.press(crossterm::event::KeyCode::F(key));
            app.render_unsettled_silent();
            assert!(started.elapsed() < std::time::Duration::from_millis(250));
            app.press(crossterm::event::KeyCode::F(key));
        }
        release.send(()).unwrap();
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "resume adoption blocked UI for {elapsed:?}"
        );
        assert_eq!(retained_id, source_id);
        assert!(retained_actor);
        assert!(display.contains("retained source"));
        app.wait_for_session_lifecycle();
        assert_eq!(app.session_snapshot().id, target_id);
        app.render_silent();
        assert!(app
            .render_unsettled_silent()
            .text()
            .contains("resume destination"));
        app.assert_invariants();
    }

    #[test]
    fn session_resume_reuses_preparation_only_when_head_and_layout_are_unchanged() {
        for change in ["unchanged", "head", "width", "height"] {
            let mut app = crate::app::test_harness::TestApp::builder().build();
            app.ensure_writer_ready();
            app.app.show_user_message("source display", Vec::new());
            app.save_session_and_flush();
            let mut target =
                smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
            save_record_backed_session(&app, &target, "destination display");
            let release = app.app.conversation.pause_persistence();
            app.set_lua_string_global("TARGET_SESSION_ID", target.id.clone())
                .unwrap();
            smelt_perf::perf::clear();
            smelt_perf::perf::set_enabled(true);
            app.run_lua_result("smelt.session.load(TARGET_SESSION_ID)")
                .unwrap();
            wait_for_session_load_result(&mut app);
            wait_for_source_close(&mut app);
            match change {
                "head" => {
                    target
                        .history
                        .push(protocol::HistoryItem::user(Content::text(
                            "new destination user",
                        )));
                    save_record_backed_session(&app, &target, "updated destination display");
                }
                "width" => app.feed_one(crate::event_source::SourceEvent::Resize {
                    width: app.app.last_width + 10,
                    height: app.app.last_height,
                }),
                "height" => app.feed_one(crate::event_source::SourceEvent::Resize {
                    width: app.app.last_width,
                    height: app.app.last_height + 10,
                }),
                _ => {}
            }
            release.send(()).unwrap();
            app.wait_for_session_lifecycle();
            let perf = smelt_perf::perf::snapshot();
            smelt_perf::perf::set_enabled(false);
            let prepared = perf
                .durations
                .iter()
                .find(|row| row.label == "session:resume:prepare")
                .unwrap()
                .count;
            let reused = perf
                .values
                .iter()
                .find(|row| row.label == "session:resume:reused")
                .map_or(0, |row| row.count);
            assert_eq!(
                prepared,
                if change == "unchanged" { 1 } else { 2 },
                "{change}"
            );
            assert_eq!(reused, usize::from(change == "unchanged"), "{change}");
            assert_eq!(app.session_snapshot().id, target.id);
            assert_eq!(
                app.app.conversation.history_navigation().user_turn_count(),
                usize::from(change == "head")
            );
            app.render_silent();
            let frame = app.render_unsettled_silent().text();
            assert!(
                frame.contains(if change == "head" {
                    "updated destination display"
                } else {
                    "destination display"
                }),
                "{frame}"
            );
            app.assert_invariants();
        }
    }

    fn wait_for_session_load_result(app: &mut crate::app::test_harness::TestApp) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(event) = app.try_recv_app_event() {
                let completed = matches!(&event, crate::app::AppEvent::SessionLoadCompleted(_));
                app.handle_app_event(event);
                if completed {
                    return;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session read did not finish"
            );
            std::thread::yield_now();
        }
    }

    fn wait_for_source_close(app: &mut crate::app::test_harness::TestApp) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !app.app.session_load.is_releasing_source() {
            assert!(
                std::time::Instant::now() < deadline,
                "source session did not start closing"
            );
            app.app.drain_persist_reports();
            app.app.poll_session_adoption();
            std::thread::yield_now();
        }
    }

    #[test]
    fn cancelling_session_load_during_release_retains_and_reopens_source() {
        let mut app = crate::app::test_harness::TestApp::builder()
            .with_vim(false)
            .build();
        app.ensure_writer_ready();
        app.app.show_user_message("retained source", Vec::new());
        app.save_session_and_flush();
        let source_id = app.session_snapshot().id.clone();
        let target =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        save_record_backed_session(&app, &target, "cancelled destination");
        let release = app.app.conversation.pause_persistence();
        app.app.request_session_load(&target.id);
        wait_for_session_load_result(&mut app);
        wait_for_source_close(&mut app);
        app.app.publish_diff_signals();
        app.render_unsettled_silent();
        let busy: bool = app.eval_lua("return smelt.work.is_busy()").unwrap();
        assert!(busy);
        app.press(crossterm::event::KeyCode::Esc);
        app.press(crossterm::event::KeyCode::Esc);
        app.app.drain_persist_reports();
        assert_eq!(app.session_snapshot().id, source_id);
        assert!(app.app.conversation.has_persistence());
        release.send(()).unwrap();
        app.wait_for_session_lifecycle();
        assert_eq!(app.session_snapshot().id, source_id);
        assert!(app.app.conversation.has_persistence());
        assert!(!app.app.busy_stack.is_busy());
        app.session_append_history(protocol::HistoryItem::user(Content::text(
            "after cancelled resume",
        )));
        app.save_session_and_flush();
        assert_eq!(app.app.session_history_len(), 1);
        app.assert_invariants();
    }

    #[test]
    fn latest_session_load_during_release_adopts_only_latest_destination() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.ensure_writer_ready();
        app.app.show_user_message("retained source", Vec::new());
        app.save_session_and_flush();
        let source_id = app.session_snapshot().id.clone();
        let first =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        save_record_backed_session(&app, &first, "obsolete destination");
        let latest =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        save_record_backed_session(&app, &latest, "latest destination");
        let release = app.app.conversation.pause_persistence();
        app.app.request_session_load(&first.id);
        wait_for_session_load_result(&mut app);
        wait_for_source_close(&mut app);
        app.app.request_session_load(&latest.id);
        wait_for_session_load_result(&mut app);
        assert_eq!(app.session_snapshot().id, source_id);
        release.send(()).unwrap();
        app.wait_for_session_lifecycle();
        assert_eq!(app.session_snapshot().id, latest.id);
        assert!(!app.app.busy_stack.is_busy());
        app.assert_invariants();
    }

    #[test]
    fn session_load_close_timeout_preserves_source_and_can_retry() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.ensure_writer_ready();
        app.app.show_user_message("retained source", Vec::new());
        app.save_session_and_flush();
        let source_id = app.session_snapshot().id.clone();
        let target =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        save_record_backed_session(&app, &target, "retry destination");
        let resume = app.app.conversation.pause_persistence();
        app.app.request_session_load(&target.id);
        wait_for_session_load_result(&mut app);
        let deadline = std::time::Instant::now()
            + crate::persist::DEFAULT_PERSISTENCE_DEADLINE
            + std::time::Duration::from_secs(5);
        while app.app.session_load.is_pending() {
            let started = std::time::Instant::now();
            app.app.drain_persist_reports();
            assert!(started.elapsed() < std::time::Duration::from_millis(100));
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(app.session_snapshot().id, source_id);
        assert!(app.app.conversation.has_persistence());
        assert!(!app.app.busy_stack.is_busy());
        resume.send(()).unwrap();
        app.app.request_session_load(&target.id);
        app.wait_for_session_lifecycle();
        assert_eq!(app.session_snapshot().id, target.id);
        app.assert_invariants();
    }

    #[test]
    fn source_mutation_during_release_cancels_adoption_without_losing_work() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.ensure_writer_ready();
        app.app.show_user_message("retained source", Vec::new());
        app.save_session_and_flush();
        let source_id = app.session_snapshot().id.clone();
        let target =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        save_record_backed_session(&app, &target, "obsolete destination");
        let release = app.app.conversation.pause_persistence();
        app.app.request_session_load(&target.id);
        wait_for_session_load_result(&mut app);
        wait_for_source_close(&mut app);
        app.session_append_history(protocol::HistoryItem::user(Content::text(
            "retained mutation",
        )));
        release.send(()).unwrap();
        app.wait_for_session_lifecycle();
        assert_eq!(app.session_snapshot().id, source_id);
        assert_eq!(app.app.session_history_len(), 1);
        app.save_session_and_flush();
        assert_eq!(session_reader(&app).head().unwrap().history_len.get(), 1);
        app.assert_invariants();
    }

    #[test]
    fn same_session_resume_refreshes_after_final_source_save() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.ensure_writer_ready();
        app.session_append_history(protocol::HistoryItem::user(Content::text("stored user")));
        app.app.show_user_message("stored user", Vec::new());
        app.save_session_and_flush();
        let source_id = app.session_snapshot().id.clone();
        let resume = app.app.conversation.pause_persistence();
        app.session_append_history(protocol::HistoryItem::user(Content::text("unsaved user")));
        app.app.show_user_message("unsaved user", Vec::new());
        app.app.request_session_load(&source_id);
        wait_for_session_load_result(&mut app);
        resume.send(()).unwrap();
        app.wait_for_session_lifecycle();
        assert_eq!(app.session_snapshot().id, source_id);
        assert_eq!(
            app.app.session_history_len(),
            2,
            "adopted pre-flush snapshot"
        );
        app.render_silent();
        assert!(app
            .render_unsettled_silent()
            .text()
            .contains("unsaved user"));
        app.assert_invariants();
    }

    #[test]
    fn latest_session_load_request_wins_without_head_of_line_blocking() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut first =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        first
            .history
            .push(protocol::HistoryItem::note(protocol::HistoryNote::context(
                "stale resume target",
            )));
        let first_id = first.id.clone();
        save_record_backed_session(&app, &first, "stale resume transcript");
        let mut latest =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        latest
            .history
            .push(protocol::HistoryItem::note(protocol::HistoryNote::context(
                "latest resume target",
            )));
        let latest_id = latest.id.clone();
        save_record_backed_session(&app, &latest, "latest resume transcript");
        let current_id = app.app.conversation.session().id.clone();
        app.app
            .session_load
            .set_delay(std::time::Duration::from_millis(500));
        app.set_lua_string_global("FIRST_SESSION_ID", first_id.clone())
            .expect("install first session id");
        app.set_lua_string_global("LATEST_SESSION_ID", latest_id.clone())
            .expect("install latest session id");

        app.run_lua_result("smelt.session.load(FIRST_SESSION_ID)")
            .expect("request first session load");
        std::thread::sleep(std::time::Duration::from_millis(25));
        app.app.session_load.set_delay(std::time::Duration::ZERO);
        let latest_requested_at = std::time::Instant::now();
        app.run_lua_result("smelt.session.load(LATEST_SESSION_ID)")
            .expect("request latest session load");
        assert_eq!(app.app.conversation.session().id, current_id);

        let deadline = latest_requested_at + std::time::Duration::from_millis(250);
        while app.app.conversation.session().id != latest_id {
            app.app.drain_persist_reports();
            if let Some(event) = app.try_recv_app_event() {
                app.handle_app_event(event);
            } else {
                assert!(
                    std::time::Instant::now() < deadline,
                    "latest background session load did not complete"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        assert_ne!(app.app.conversation.session().id, first_id);
        assert_eq!(app.app.session_history_len(), 1);
    }

    #[test]
    fn title_target_beyond_current_history_is_rejected() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::user(Content::text("current request")));
        let history_len = app.app.session_history_len();

        app.app.set_session_title(
            "Future title".into(),
            "future-title".into(),
            Some(history_len + 1),
        );

        assert_eq!(app.app.conversation.session().title, None);
        assert!(app.app.conversation.session().metadata_snapshots.is_empty());
    }

    #[test]
    fn session_resume_interrupts_running_turn_without_redispatch() {
        let runtime = tempfile::tempdir().expect("create shared runtime root");
        let session_id;
        let turn_id;
        {
            let mut app = crate::app::test_harness::TestApp::builder()
                .with_runtime_home(runtime.path())
                .build();
            app.start_submitted_turn("before restart");
            turn_id = app.current_turn_id().expect("turn starts");
            let _ = app.app.flush_persist();
            session_id = app.app.conversation.session().id.clone();
            assert_eq!(stored_last_turn_id(&app), turn_id);
        }

        let mut resumed = crate::app::test_harness::TestApp::builder()
            .with_runtime_home(runtime.path())
            .build();
        resumed.clear_actions();
        let prepared = crate::app::session_load::prepare_session_load(
            &resumed.app.core.sessions,
            &session_id,
            80,
            24,
            &|| false,
        )
        .unwrap();
        assert!(resumed.app.install_prepared_session_load(prepared));
        assert!(resumed.app.conversation.writer_is_opening());
        let title = resumed.app.conversation.session().title.clone();
        resumed
            .eval_lua::<()>("smelt.session.title.set('blocked while opening')")
            .unwrap();
        assert_eq!(resumed.app.conversation.session().title, title);
        assert!(resumed
            .overlays_probe()
            .notification()
            .unwrap()
            .summary
            .contains("still opening"));
        resumed.wait_for_session_lifecycle();

        assert_eq!(
            resumed.app.conversation.session().id,
            session_id,
            "resume failed: {:?}",
            resumed
                .app
                .overlays
                .notification()
                .map(|notification| notification.summary.as_str())
        );
        assert!(!resumed.agent_running());
        assert!(resumed.actions().iter().all(|action| !matches!(
            action,
            crate::app::test_harness::Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )));
        assert_eq!(
            resumed.app.conversation.last_terminal_turn_id(),
            Some(turn_id)
        );
        assert!(!resumed.app.conversation.is_read_only());
        resumed.start_submitted_turn("after restart");
        assert_eq!(resumed.current_turn_id(), Some(turn_id + 1));
    }

    #[test]
    fn request_start_dispatches_store_history_without_full_reads() {
        const BASE_HISTORY_LEN: usize = 128;
        let mut app = large_saved_session_app(BASE_HISTORY_LEN);
        let old_history_len = app.app.conversation.session().history.len();
        assert!(old_history_len >= BASE_HISTORY_LEN);

        smelt_perf::perf::set_enabled(true);
        smelt_perf::perf::clear();
        app.start_submitted_turn("new request");
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);

        assert_no_full_request_start_reads(&snapshot);
        assert_perf_value_at_most(&snapshot, "store:history:dirty_suffix_rows", 1);
        assert_perf_value_at_most(&snapshot, "store:session:history_rows_inserted", 1);
        assert_perf_value_at_most(&snapshot, "store:transcript:dirty_record_suffix_rows", 1);
        assert_perf_value_at_most(&snapshot, "store:transcript:record_db_rows_inserted", 1);

        let payload = app
            .drain_engine_sends()
            .into_iter()
            .find_map(|cmd| match cmd {
                protocol::UiCommand::StartTurn(payload) => Some(payload),
                _ => None,
            })
            .expect("turn dispatched");
        assert_eq!(
            payload.history.coordinates,
            protocol::ModelHistoryCoordinates::canonical()
        );
        assert_eq!(payload.history.items.len(), old_history_len);
        assert_eq!(
            payload.input.provider_content().text_content(),
            "new request"
        );
        assert_eq!(
            app.app.conversation.session().history.len(),
            old_history_len + 1
        );
    }

    #[test]
    fn request_start_appends_clear_for_deep_named_context() {
        const TRAILING_HISTORY_LEN: usize = 1024;
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user".into());
        session
            .history
            .push(HistoryItem::note(protocol::HistoryNote::named_context(
                "goal", "old goal",
            )));
        session.history.extend((0..TRAILING_HISTORY_LEN).map(|idx| {
            if idx.is_multiple_of(2) {
                HistoryItem::user(Content::text(format!("old user {idx}")))
            } else {
                HistoryItem::Assistant(protocol::AssistantStep::terminal(
                    Some(Content::text(format!("old assistant {idx}"))),
                    None,
                    Vec::new(),
                ))
            }
        }));
        app.load_session(session);
        app.app.restore_screen();
        app.app.ensure_current_context_note();
        app.app.apply_pending_history_appends_for_request();
        app.app.save_session();
        app.app.flush_persist();

        app.app.set_context_note("goal".into(), None);
        smelt_perf::perf::set_enabled(true);
        smelt_perf::perf::clear();
        app.start_submitted_turn("new request");
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);

        assert_no_full_request_start_reads(&snapshot);
        assert_perf_value_at_most(&snapshot, "store:history:dirty_suffix_rows", 2);
        assert_perf_value_at_most(&snapshot, "store:session:history_rows_inserted", 2);
        assert_perf_value_at_most(&snapshot, "store:session:history_rows_deleted", 0);
        let tail = app
            .app
            .session_history_range(
                app.app.session_history_len().saturating_sub(2)..app.app.session_history_len(),
            )
            .unwrap();
        assert!(tail[0]
            .as_note()
            .is_some_and(|note| note.context_name() == Some("goal") && note.text().is_empty()));
        assert!(matches!(tail[1], HistoryItem::User { .. }));
    }

    #[test]
    fn command_turn_metadata_snapshot_matches_committed_request() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.ensure_writer_ready();
        let turn = app.app.begin_command_request_turn(
            "/fix".into(),
            "fix it".into(),
            crate::app::CommandScope::default(),
            crate::app::CommandTurnStart::Fresh,
            0,
        );
        app.app.conversation.set_active(turn);
        app.wait_for_turn_dispatch();
        assert!(app.agent_running(), "test app has a usable model");

        assert_eq!(
            app.app.conversation.session().first_user_message.as_deref(),
            Some("/fix")
        );
        assert_eq!(
            app.app
                .conversation
                .session()
                .metadata_snapshots
                .as_slice()
                .last()
                .map(|(idx, _)| *idx),
            Some(app.app.conversation.session().history.len())
        );
        assert!(matches!(
            app.app.conversation.session().history.last(),
            Some(HistoryItem::User {
                display: Some(display),
                command: true,
                ..
            }) if display == "/fix"
        ));
        let transcript = app.app.conversation.transcript().history();
        assert!(transcript
            .order
            .last()
            .and_then(|id| transcript.block(*id))
            .is_some_and(|block| matches!(
                block,
                Block::User {
                    text,
                    command: true,
                    ..
                } if text == "/fix"
            )));
        assert!(app.drain_engine_sends().into_iter().any(|command| matches!(
            command,
            protocol::UiCommand::StartTurn(payload)
                if matches!(payload.input, protocol::StartTurnInput::User { command: true, .. })
        )));
    }

    #[test]
    fn expanding_at_file_records_absolute_path_as_read() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("note.txt");
        std::fs::write(&file, "hello\nworld").unwrap();

        let mut app = crate::app::test_harness::TestApp::builder()
            .with_cwd(tmp.path())
            .build();
        app.type_text("summarize @note.txt");
        app.press(crossterm::event::KeyCode::Esc);
        app.press(crossterm::event::KeyCode::Enter);
        app.wait_for_turn_dispatch();
        let expanded = app
            .drain_engine_sends()
            .into_iter()
            .find_map(|command| match command {
                protocol::UiCommand::StartTurn(payload) => {
                    Some(payload.input.provider_content().text_content().into_owned())
                }
                _ => None,
            })
            .expect("prepared request");

        let path = file.to_string_lossy();
        assert!(expanded.contains(&format!(
            "<attached_file path=\"{path}\" tool=\"read_file\" already_read=\"true\" source=\"user_attachment\">"
        )));
        assert!(expanded.contains("Called the read_file tool with the following input:"));
        assert!(expanded.contains("   1\thello"));
        assert!(app.app.core.files.has(&path));
    }

    #[test]
    fn expanding_at_notebook_uses_notebook_renderer() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("nb.ipynb");
        std::fs::write(
            &file,
            r##"{"cells":[{"cell_type":"markdown","id":"intro","source":["# Title\n"]}]}"##,
        )
        .unwrap();

        let mut app = crate::app::test_harness::TestApp::builder()
            .with_cwd(tmp.path())
            .build();
        app.type_text("summarize @nb.ipynb");
        app.press(crossterm::event::KeyCode::Esc);
        app.press(crossterm::event::KeyCode::Enter);
        app.wait_for_turn_dispatch();
        let expanded = app
            .drain_engine_sends()
            .into_iter()
            .find_map(|command| match command {
                protocol::UiCommand::StartTurn(payload) => {
                    Some(payload.input.provider_content().text_content().into_owned())
                }
                _ => None,
            })
            .expect("prepared request");

        let path = file.to_string_lossy();
        assert!(expanded.contains(&format!(
            "<attached_file path=\"{path}\" tool=\"read_file\" already_read=\"true\" source=\"user_attachment\">"
        )));
        assert!(expanded.contains("Called the read_file tool with the following input:"));
        assert!(expanded.contains("--- Cell 0 [markdown] id=intro ---"));
        assert!(app.app.core.files.has(&path));
    }

    #[test]
    fn idle_job_completion_starts_turn_with_process_status_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id: "1234".into(),
                exit_code: Some(0),
                termination: protocol::JobTermination::Exited,
                output: String::new(),
            });

        app.wait_for_turn_dispatch();
        assert!(app.app.agent_is_running());
        assert_eq!(user_blocks(&app), Vec::<String>::new());
        assert_eq!(
            process_status_blocks(&app),
            vec!["background process 1234 finished successfully"]
        );
    }

    #[test]
    fn idle_oom_completion_starts_turn_with_distinct_process_status() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id: "proc_123".into(),
                exit_code: None,
                termination: protocol::JobTermination::OutOfMemory,
                output: String::new(),
            });

        app.wait_for_turn_dispatch();
        assert!(app.app.agent_is_running());
        assert_eq!(
            process_status_blocks(&app),
            vec!["background process proc_123 was terminated after an out-of-memory event"]
        );
    }

    #[test]
    fn process_status_resize_keeps_transcript_cursor_in_bounds() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id: String::new(),
                exit_code: None,
                termination: protocol::JobTermination::Signaled,
                output: String::new(),
            });
        app.render_silent();
        app.feed_one(crate::app::test_harness::SourceEvent::Resize {
            width: 1,
            height: 12,
        });
        app.render_silent();
        app.feed_one(crate::app::test_harness::SourceEvent::Resize {
            width: 50,
            height: 1,
        });
        app.render_silent();

        app.assert_invariants();
    }

    #[test]
    fn running_agent_job_completion_queues_process_status_append() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(7);

        let before = app
            .app
            .conversation
            .pending_history_appends()
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();

        app.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id: "4242".into(),
                exit_code: Some(9),
                termination: protocol::JobTermination::Exited,
                output: String::new(),
            });

        assert!(process_status_blocks(&app).is_empty());
        let expected = protocol::HistoryItem::note(protocol::HistoryNote::process_status_event(
            protocol::ProcessStatusEvent::background_process_completed(
                "4242",
                Some(9),
                protocol::JobTermination::Exited,
            ),
        ));
        let after = app
            .app
            .conversation
            .pending_history_appends()
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();
        assert_eq!(&after[..before.len()], before.as_slice());
        assert_eq!(&after[before.len()..], std::slice::from_ref(&expected));

        let process_status_appends: Vec<_> = app
            .app
            .conversation
            .pending_history_appends()
            .iter()
            .filter(|append| {
                append.history_item().note_kind() == Some(protocol::HistoryNoteKind::ProcessStatus)
            })
            .collect();
        assert_eq!(process_status_appends.len(), 1);
        assert_eq!(process_status_appends[0].history_item(), expected);
    }

    #[test]
    fn queued_process_status_starts_process_status_turn() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let note = protocol::HistoryNote::process_status("background process 77 exited");
        let text = note.text().to_string();

        assert!(
            app.app
                .start_queued_input(crate::app::QueuedInput::ProcessStatus(note))
                .is_ok(),
            "test app has a usable model"
        );

        app.wait_for_turn_dispatch();
        assert!(app.app.agent_is_running());
        assert_eq!(process_status_blocks(&app), vec![text]);
        assert!(user_blocks(&app).is_empty());
    }

    #[test]
    fn process_status_history_update_stays_out_of_lua_conversation() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id: "751225".into(),
                exit_code: Some(1),
                termination: protocol::JobTermination::Exited,
                output: String::new(),
            });
        app.wait_for_turn_dispatch();
        let turn_id = app
            .app
            .active_agent_turn_id()
            .expect("process turn started");
        let (start_content, current_note) = app
            .drain_engine_sends()
            .into_iter()
            .find_map(|cmd| match cmd {
                protocol::UiCommand::StartTurn(payload) => Some((
                    payload.input.provider_content(),
                    payload.input.note_ref().cloned(),
                )),
                _ => None,
            })
            .expect("process turn dispatched to engine");
        let current_note = current_note.expect("process turn carries typed note");
        let expected_note = protocol::HistoryNote::process_status_event(
            protocol::ProcessStatusEvent::background_process_completed(
                "751225",
                Some(1),
                protocol::JobTermination::Exited,
            ),
        );
        assert_eq!(current_note, expected_note);
        assert_eq!(
            start_content.text_content(),
            protocol::process_status_note("background process 751225 exited with code 1")
        );

        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::HistoryUpdated {
                turn_id,
                update: protocol::CanonicalHistoryDelta::new(
                    0,
                    vec![
                        HistoryItem::user(Content::text("previous user message")),
                        HistoryItem::note(current_note.clone()),
                    ],
                ),
            },
        ));

        assert!(matches!(
            &app.session_history()[1],
            HistoryItem::Note(protocol::HistoryNote::ProcessStatus { text, event, .. })
                if text == "background process 751225 exited with code 1"
                    && event.as_ref().and_then(protocol::ProcessStatusEvent::process_id) == Some("751225")
                    && event.as_ref().and_then(|event| event.exit_code()) == Some(1)
        ));

        app.run_lua_result(
            r#"
                smelt.spawn(function()
                    local rows = smelt.session.conversation()
                    local contains_marker = false
                    for _, row in ipairs(rows) do
                        if string.find(row.content, "%[smelt:process%]") then
                            contains_marker = true
                        end
                    end
                    read = { #rows, rows[1] and rows[1].content or "", contains_marker }
                end)
                "#,
        )
        .expect("conversation query succeeds");
        app.drive_lua_tasks();
        let (count, first_content, contains_marker): (i64, String, bool) = app
            .eval_lua("return read[1], read[2], read[3]")
            .expect("conversation query returns");

        assert_eq!(count, 1);
        assert_eq!(first_content, "previous user message");
        assert!(!contains_marker);
    }

    #[test]
    fn lua_conversation_limit_returns_recent_rows() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.conversation.replace_history_for_harness(vec![
            HistoryItem::user(Content::text("first user")),
            HistoryItem::note(protocol::HistoryNote::named_context("hidden", "note")),
            protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("first assistant")),
                None,
                Vec::new(),
            )),
            HistoryItem::user(Content::text("second user")),
            protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("second assistant")),
                None,
                Vec::new(),
            )),
        ]);

        app.run_lua_result(
            r#"
                smelt.spawn(function()
                    local rows = smelt.session.conversation({ limit = 2 })
                    read = { #rows, rows[1] and rows[1].content or "", rows[2] and rows[2].content or "" }
                end)
                "#,
        )
        .expect("conversation limit query succeeds");
        app.drive_lua_tasks();
        let (count, first, second): (i64, String, String) = app
            .eval_lua("return read[1], read[2], read[3]")
            .expect("conversation limit query returns");

        assert_eq!(count, 2);
        assert_eq!(first, "second user");
        assert_eq!(second, "second assistant");
    }

    #[tokio::test]
    async fn cancelling_turn_does_not_kill_registered_background_process() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let supervisor = app.app.core.jobs.clone();
        let id = supervisor
            .spawn_background(
                "echo alive; sleep 30",
                &smelt_core::process::ShellSpec::default(),
                &app.app.core.env.cwd(),
                std::time::Instant::now(),
            )
            .await
            .unwrap();
        app.start_turn(1);

        app.cancel();

        assert!(supervisor.snapshot_output(&id).unwrap().running);
        assert_eq!(supervisor.running_count(), 1);
        let _ = supervisor.stop(&id).await;
    }

    #[test]
    fn empty_key_env_returns_empty_string_without_consulting_environment() {
        // Callers configured with no API key (e.g. local-only models) hit
        // this path; the resolver must never be invoked.
        let mut called = false;
        let out = lookup_api_key("", |_| {
            called = true;
            Ok("nope".into())
        });
        assert_eq!(out, Ok(String::new()));
        assert!(!called, "resolver should be skipped for empty key_env");
    }

    #[test]
    fn nonempty_key_env_returns_resolved_value() {
        let out = lookup_api_key("MY_KEY", |var| {
            assert_eq!(var, "MY_KEY");
            Ok("secret".into())
        });
        assert_eq!(out, Ok("secret".into()));
    }

    #[test]
    fn missing_env_var_maps_to_not_set_error_with_var_name() {
        let out = lookup_api_key("MY_KEY", |_| Err(std::env::VarError::NotPresent));
        assert_eq!(
            out,
            Err(ApiKeyError::NotSet {
                var: "MY_KEY".into()
            })
        );
    }

    #[test]
    fn non_unicode_env_var_maps_to_not_unicode_error() {
        // `std::env::VarError::NotUnicode` carries an `OsString`; we don't
        // care about its payload - only the variant matters for our message.
        let out = lookup_api_key("MY_KEY", |_| {
            Err(std::env::VarError::NotUnicode(std::ffi::OsString::new()))
        });
        assert_eq!(
            out,
            Err(ApiKeyError::NotUnicode {
                var: "MY_KEY".into()
            })
        );
    }

    #[test]
    fn not_set_error_message_names_the_missing_var() {
        let msg = ApiKeyError::NotSet {
            var: "OPENAI_API_KEY".into(),
        }
        .message();
        assert!(msg.contains("OPENAI_API_KEY"));
        assert!(msg.contains("not set"));
    }

    #[test]
    fn not_unicode_error_message_names_the_var() {
        let msg = ApiKeyError::NotUnicode {
            var: "WEIRD_KEY".into(),
        }
        .message();
        assert!(msg.contains("WEIRD_KEY"));
        assert!(msg.contains("non-Unicode"));
    }
}
