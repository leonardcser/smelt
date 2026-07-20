use crate::app::{
    CommandTurnStart, DeferredDialog, PendingHistoryLifecycle, PendingTool, SessionControl, TuiApp,
    TurnState, CONFIRM_DEFER_MS,
};
use protocol::{Content, ContentPart, Decision, HistoryItem, UiCommand};
use smelt_core::working::{TurnOutcome, TurnPhase};
use smelt_core::*;
use std::path::PathBuf;
use std::time::Duration;

struct StagedTurnRollback {
    history_len: Option<usize>,
    transcript_len: Option<usize>,
}

struct PreparedTurn {
    input: protocol::StartTurnInput,
    history: protocol::ModelHistorySource,
    kind: smelt_store::TurnKind,
    submitted_history_idx: smelt_store::HistoryIndex,
    continuation_of: Option<smelt_store::TurnId>,
    model_target: protocol::ModelTarget,
    request_config: protocol::RequestRuntimeConfig,
    reasoning_effort: protocol::ReasoningEffort,
    permission_overrides: Option<protocol::PermissionOverrides>,
    permissions: std::sync::Arc<smelt_core::permissions::Permissions>,
    rewind_block_idx: Option<usize>,
    rollback: Option<StagedTurnRollback>,
}

fn is_resumable_turn_error(
    kind: Option<protocol::EngineAskErrorKind>,
    retry_at_ms: Option<u64>,
) -> bool {
    retry_at_ms.is_some()
        && matches!(
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
        self.agent
            .as_ref()
            .map(|turn| turn.permissions.clone())
            .or_else(|| self.dispatching_turn_permissions.clone())
            .unwrap_or_else(|| self.core.permissions.snapshot())
    }

    fn refresh_active_turn_permissions(&mut self) {
        let permissions = self.core.permissions.snapshot();
        if self.dispatching_turn_id.is_some() {
            self.dispatching_turn_permissions = Some(permissions);
        } else if let Some(turn) = self.agent.as_mut() {
            turn.permissions = permissions;
        }
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
        self.dismiss_notification();
        self.clear_prompt_prediction();
        self.sleep_inhibit.acquire();
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

    fn continuation_target(&mut self) -> Option<smelt_store::TurnId> {
        match self.last_terminal_turn_id {
            Some(turn_id) => Some(smelt_store::TurnId::new(turn_id)),
            None => {
                self.notify_error_sticky(
                    "cannot continue: this session has no durable prior turn; submit a new request first"
                        .into(),
                );
                None
            }
        }
    }

    fn require_reopen_after_submit_failure(&mut self, cause: &crate::persist::PersistenceCause) {
        let reason = format!(
            "a canonical turn may already be durable; reopen the session before retrying ({})",
            cause.message
        );
        self.session_access = crate::app::SessionAccess::ReadOnly { reason };
    }

    fn expand_at_file_refs_in_text(&mut self, text: &str) -> String {
        smelt_core::file_ref::expand_at_file_refs(text, &self.cwd, &self.core.files)
    }

    fn expand_at_file_refs(&mut self, content: Content) -> Content {
        match content {
            Content::Text(text) => Content::Text(self.expand_at_file_refs_in_text(&text)),
            Content::Parts(parts) => Content::Parts(
                parts
                    .into_iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => ContentPart::Text {
                            text: self.expand_at_file_refs_in_text(&text),
                        },
                        other => other,
                    })
                    .collect(),
            ),
        }
    }

    pub(crate) fn begin_agent_turn(
        &mut self,
        display: &str,
        content: Content,
    ) -> Option<TurnState> {
        let _perf = smelt_perf::perf::begin("agent:begin_turn");
        if self.block_read_only_mutation("submit a turn to this read-only session") {
            return None;
        }
        let model_target = self.resolve_model_target()?;
        let content = self.expand_at_file_refs(content);
        let text = content.text_content();
        let submitted = match text.trim() {
            "" => None,
            trimmed => Some(trimmed.to_string()),
        };
        if content.is_empty() {
            let continuation_of = self.continuation_target()?;
            self.prepare_user_visible_turn();
            self.publish_turn_input(submitted);
            let history = self.model_history_source();
            let submitted_history_idx = self.session_history_len().checked_sub(1)?;
            return self.dispatch_prepared_turn(PreparedTurn {
                input: protocol::StartTurnInput::user(content, None),
                history,
                kind: smelt_store::TurnKind::Continuation,
                submitted_history_idx: smelt_store::HistoryIndex::new(submitted_history_idx as u64),
                continuation_of: Some(continuation_of),
                model_target,
                request_config: self.core.config.request_runtime_config(),
                reasoning_effort: self.core.config.reasoning_effort,
                permission_overrides: None,
                permissions: self.core.permissions.snapshot(),
                rewind_block_idx: None,
                rollback: None,
            });
        }
        self.prepare_user_visible_turn();
        let rollback = StagedTurnRollback {
            history_len: Some(self.session_history_len()),
            transcript_len: Some(self.session_document.transcript.history().order.len()),
        };
        let first_user_message = self
            .core
            .session
            .first_user_message
            .is_none()
            .then(|| text.clone().into_owned());
        let history = self.stage_request_history_item_with_first_user(
            protocol::history_item_from_user_content(content.clone()),
            Some(Block::User {
                text: display.to_string(),
                image_labels: content.image_labels(),
                command: false,
            }),
            first_user_message,
        );
        let submitted_history_idx = self.session_history_len().checked_sub(1)?;
        let rewind_block_idx = self.last_user_block_index();
        self.publish_turn_input(submitted);
        self.dispatch_prepared_turn(PreparedTurn {
            input: protocol::StartTurnInput::user(content),
            history,
            kind: smelt_store::TurnKind::User,
            submitted_history_idx: smelt_store::HistoryIndex::new(submitted_history_idx as u64),
            continuation_of: None,
            model_target,
            request_config: self.core.config.request_runtime_config(),
            reasoning_effort: self.core.config.reasoning_effort,
            permission_overrides: None,
            permissions: self.core.permissions.snapshot(),
            rewind_block_idx,
            rollback: Some(rollback),
        })
    }

    fn rollback_staged_turn(&mut self, rollback: &StagedTurnRollback) {
        if let Some(history_len) = rollback.history_len {
            self.rewind_session_history_to(history_len, false);
        }
        if let Some(transcript_len) = rollback.transcript_len {
            self.truncate_to(transcript_len);
        }
        self.sync_session_snapshot();
        self.publish_history_delta("submit_failed");
    }

    fn dispatch_prepared_turn(&mut self, turn: PreparedTurn) -> Option<TurnState> {
        self.pending_continuation_token = None;
        self.working.begin(TurnPhase::Working);

        self.core.signals.set_dyn(
            "turn_start",
            std::rc::Rc::new(smelt_core::signals::EventStub),
        );
        self.pump_lua();

        let (system_prompt, tools) = self.prepare_turn_context();
        let (turn_id, submitted_revision, required_generation) = if self.ephemeral() {
            let turn_id = self.next_turn_id;
            (turn_id, 0, self.session_document.generation().get())
        } else {
            let acknowledgement = match self.submit_canonical_turn(smelt_store::NewTurn {
                kind: turn.kind,
                submitted_history_idx: turn.submitted_history_idx,
                continuation_of: turn.continuation_of,
                created_at_ms: session::now_ms(),
            }) {
                Ok(acknowledgement) => acknowledgement,
                Err(cause) => {
                    if cause.definitely_not_committed() {
                        if let Some(rollback) = turn.rollback.as_ref() {
                            self.rollback_staged_turn(rollback);
                        }
                    } else if cause.requires_reopen() {
                        self.require_reopen_after_submit_failure(&cause);
                    }
                    self.sleep_inhibit.release();
                    self.working.finish(TurnOutcome::Errored);
                    self.notify_session_save_failure(&self.core.session.id.clone(), &cause.message);
                    return None;
                }
            };
            (
                acknowledgement.receipt.turn_id.get(),
                acknowledgement.receipt.session.current.revision.get(),
                acknowledgement.generation.get(),
            )
        };
        self.next_turn_id = turn_id.checked_add(1).unwrap_or(turn_id);

        let permissions = turn.permissions.clone();
        self.applied_agent_mode = self.core.config.mode.clone();
        self.applied_reasoning_effort = turn.reasoning_effort;
        let payload = protocol::StartTurnPayload {
            turn_id,
            input: turn.input,
            mode: self.core.config.mode.clone(),
            model_target: turn.model_target,
            request_config: turn.request_config,
            reasoning_effort: turn.reasoning_effort,
            fast_mode: self.fast_mode_active(),
            history: turn.history,
            session_id: self.core.session.id.clone(),
            session_dir: self.current_session_dir(),
            persistence: protocol::PersistenceScope {
                epoch: self
                    .persistence
                    .as_ref()
                    .map_or(0, |actor| actor.epoch().get()),
                required_generation,
                store_revision: submitted_revision,
            },
            permission_overrides: turn.permission_overrides,
            system_prompt: Some(system_prompt),
            tools,
        };
        if self
            .core
            .engine
            .try_send(UiCommand::StartTurn(Box::new(payload)))
            .is_err()
        {
            if !self.ephemeral() {
                let _ = self.enqueue_canonical_turn_transition(
                    smelt_store::TurnId::new(turn_id),
                    smelt_store::TurnState::Failed,
                    Some("engine_channel_rejected".into()),
                );
            }
            self.sleep_inhibit.release();
            self.working.finish(TurnOutcome::Errored);
            self.notify_error_sticky("engine stopped before accepting the request".into());
            return None;
        }
        smelt_perf::perf::record_value("agent:dispatch:start_turn:turn_id", turn_id);
        smelt_perf::perf::record_value("agent:dispatch:start_turn:revision", submitted_revision);
        smelt_perf::perf::record_value(
            "agent:dispatch:start_turn:at_us",
            smelt_perf::perf::timestamp_us(),
        );

        if !self.ephemeral() {
            if let Err(cause) = self.enqueue_canonical_turn_transition(
                smelt_store::TurnId::new(turn_id),
                smelt_store::TurnState::Running,
                None,
            ) {
                self.notify_session_save_failure(&self.core.session.id.clone(), &cause.message);
            }
        }

        Some(TurnState {
            turn_id,
            canonical: !self.ephemeral(),
            pending: Vec::new(),
            permissions,
            rewind_block_idx: turn.rewind_block_idx,
            assistant_output_started: false,
            _perf: smelt_perf::perf::begin("agent:turn"),
        })
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
        let block = crate::app::history::history_note_to_block(&self.lua, &history_note);
        let adds_history = !history_note.text().is_empty();
        let adds_block = block.is_some();
        if !adds_history && self.session_history_len() == 0 {
            return None;
        }
        self.prepare_user_visible_turn();
        let rollback = StagedTurnRollback {
            history_len: adds_history.then(|| self.session_history_len()),
            transcript_len: adds_block
                .then(|| self.session_document.transcript.history().order.len()),
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
            kind: smelt_store::TurnKind::Note,
            submitted_history_idx: smelt_store::HistoryIndex::new(submitted_history_idx as u64),
            continuation_of: None,
            model_target,
            request_config,
            reasoning_effort: self.core.config.reasoning_effort,
            permission_overrides: None,
            permissions: self.core.permissions.snapshot(),
            rewind_block_idx: None,
            rollback: (adds_history || adds_block).then_some(rollback),
        })
    }

    fn custom_command_parts(
        &self,
        cmd: smelt_core::custom_commands::CustomCommand,
    ) -> (
        String,
        String,
        smelt_core::custom_commands::CommandOverrides,
    ) {
        let evaluated = if self.core.config.settings.redact_secrets {
            engine::redact::redact(&cmd.body)
        } else {
            cmd.body
        };
        let display = if self.core.config.settings.redact_secrets {
            engine::redact::redact(&format!("/{}", cmd.display))
        } else {
            format!("/{}", cmd.display)
        };
        (display, evaluated, cmd.overrides)
    }

    pub(crate) fn begin_custom_command_turn(
        &mut self,
        cmd: smelt_core::custom_commands::CustomCommand,
    ) -> Option<TurnState> {
        let (display, evaluated, overrides) = self.custom_command_parts(cmd);
        self.begin_command_request_turn(display, evaluated, overrides, CommandTurnStart::Fresh)
    }

    pub(crate) fn begin_custom_command_continuation(
        &mut self,
        cmd: smelt_core::custom_commands::CustomCommand,
    ) -> Option<TurnState> {
        let (display, evaluated, overrides) = self.custom_command_parts(cmd);
        self.begin_command_request_turn(
            display,
            evaluated,
            overrides,
            CommandTurnStart::ContinueFromLast,
        )
    }

    fn resolve_command_model_target(
        &mut self,
        overrides: &smelt_core::custom_commands::CommandOverrides,
    ) -> Option<protocol::ModelTarget> {
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
            (None, None) => return self.resolve_model_target(),
        };
        let resolved = match resolved {
            Ok(model) => model.clone(),
            Err(error) => {
                self.notify_error_sticky(error.to_string());
                return None;
            }
        };
        let api_key = self.resolve_api_key_for_env(&resolved.api_key_env)?;
        Some(resolved.target(api_key))
    }

    pub(crate) fn begin_command_request_turn(
        &mut self,
        display: String,
        evaluated: String,
        overrides: smelt_core::custom_commands::CommandOverrides,
        start: CommandTurnStart,
    ) -> Option<TurnState> {
        if self.block_read_only_mutation("submit a turn to this read-only session") {
            return None;
        }
        let submitted = match evaluated.trim() {
            "" => None,
            trimmed => Some(trimmed.to_string()),
        };
        let mut model_target = self.resolve_command_model_target(&overrides)?;
        let (kind, continuation_of) = match start {
            CommandTurnStart::Fresh => (smelt_store::TurnKind::Command, None),
            CommandTurnStart::ContinueFromLast => (
                smelt_store::TurnKind::Continuation,
                Some(self.continuation_target()?),
            ),
        };
        if evaluated.is_empty() && self.session_history_len() == 0 {
            return None;
        }
        self.prepare_user_visible_turn();
        let rollback = StagedTurnRollback {
            history_len: (!evaluated.is_empty()).then(|| self.session_history_len()),
            transcript_len: Some(self.session_document.transcript.history().order.len()),
        };

        let history = if !evaluated.is_empty() {
            let first_user_message = self
                .core
                .session
                .first_user_message
                .is_none()
                .then(|| display.clone());
            self.stage_request_history_item_with_first_user(
                protocol::HistoryItem::user_command(
                    Content::text(evaluated.clone()),
                    display.clone(),
                ),
                Some(Block::User {
                    text: display.clone(),
                    image_labels: vec![],
                    command: true,
                }),
                first_user_message,
            )
        } else {
            self.push_block(Block::User {
                text: display.clone(),
                image_labels: vec![],
                command: true,
            });
            self.model_history_source()
        };
        let rewind_block_idx = if !evaluated.is_empty() {
            self.user_turns().last().map(|(idx, _)| *idx)
        } else {
            None
        };
        self.publish_turn_input(submitted);

        let reasoning = overrides
            .reasoning_effort
            .as_deref()
            .map(|s| match s.to_lowercase().as_str() {
                "low" => protocol::ReasoningEffort::Low,
                "medium" => protocol::ReasoningEffort::Medium,
                "high" => protocol::ReasoningEffort::High,
                _ => protocol::ReasoningEffort::Off,
            })
            .unwrap_or(self.core.config.reasoning_effort);

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
        let request_config = self.core.config.request_runtime_config();
        self.dispatch_prepared_turn(PreparedTurn {
            input: protocol::StartTurnInput::user_command(Content::text(evaluated), display),
            history,
            kind,
            submitted_history_idx: smelt_store::HistoryIndex::new(submitted_history_idx as u64),
            continuation_of,
            model_target,
            request_config,
            reasoning_effort: reasoning,
            permission_overrides,
            permissions,
            rewind_block_idx,
            rollback: Some(rollback),
        })
    }

    fn record_finished_turn_state(&mut self, ui_meta: protocol::TurnMeta) {
        let mut meta = match self.pending_turn_meta.take() {
            Some(engine_meta) => protocol::TurnMeta {
                elapsed_ms: ui_meta.elapsed_ms,
                avg_tps: engine_meta.avg_tps.or(ui_meta.avg_tps),
                display_tps: engine_meta.display_tps.or(ui_meta.display_tps),
                interrupted: engine_meta.interrupted,
                tool_elapsed: engine_meta.tool_elapsed,
            },
            None => ui_meta,
        };
        if meta.display_tps.is_none() {
            meta.display_tps = meta.avg_tps.or_else(|| self.working.display_tps());
        }
        let history_len = self.session_history_len();
        let snapshot_context = !self.session_is_read_only();
        let update_context_token_history_len =
            snapshot_context && self.context_tokens_updated_this_turn;
        if snapshot_context {
            self.context_tokens_updated_this_turn = false;
        }
        self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::FinishTurnState {
                history_len,
                meta,
                snapshot_context,
                update_context_token_history_len,
            },
        );
    }

    fn cancel_turn_lua_tasks(&mut self) {
        self.lua.cancel_turn_tasks();
        self.discard_model_tool_cwd_change();
    }

    fn commit_terminal_turn(
        &mut self,
        turn_id: u64,
        state: smelt_store::TurnState,
        reason: Option<String>,
    ) -> bool {
        if self.ephemeral() {
            self.last_terminal_turn_id = Some(turn_id);
            self.save_session();
            return true;
        }
        match self.commit_canonical_turn_transition(
            smelt_store::TurnId::new(turn_id),
            state,
            reason,
        ) {
            Ok(_) => {
                self.last_terminal_turn_id = Some(turn_id);
                true
            }
            Err(cause) => {
                self.notify_session_save_failure(&self.core.session.id.clone(), &cause.message);
                false
            }
        }
    }

    /// Stop the engine turn without saving session or triggering auto-compact; used before rewind/clear.
    pub(crate) fn cancel_agent(&mut self) {
        let turn = self
            .agent
            .as_ref()
            .map(|turn| (turn.turn_id, turn.canonical));
        self.sleep_inhibit.release();
        self.core.engine.send(UiCommand::Cancel);
        self.cancel_turn_lua_tasks();
        self.cancel_generation = self.cancel_generation.wrapping_add(1);
        self.busy_stack.clear();
        // A turn is ending without going through `finish_turn`. Commit any
        // in-flight streaming buffers so the post-cancel state honors the
        // "no agent ⇒ no active stream" invariant (an empty thinking delta
        // arrived right before this can leave an empty `active_thinking`
        // sentinel that lingers past `agent = None`).
        self.flush_streaming_thinking();
        self.flush_streaming_text();
        self.clear_tool_drafts();
        self.clear_compaction_preview();
        self.pending_history_appends.clear();
        let meta = self.working.finish(TurnOutcome::Cancelled);
        self.record_finished_turn_state(meta);
        self.sync_agent_mode_applied();
        self.sync_reasoning_effort_applied();
        match turn {
            Some((turn_id, true)) => {
                self.commit_terminal_turn(
                    turn_id,
                    smelt_store::TurnState::Cancelled,
                    Some("user_cancelled".into()),
                );
            }
            Some((_, false)) | None => self.save_session(),
        }
        self.queued_inputs.clear();
    }

    pub(crate) fn consume_continuation_token(&mut self, token: u64) -> bool {
        if self.pending_continuation_token == Some(token) {
            self.pending_continuation_token = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn discard_turn(&mut self, end: crate::app::TurnEnd) {
        let was_running = self.agent.is_some();
        if was_running {
            let start_queued = self.finish_turn(end);
            self.agent = None;
            if start_queued {
                self.start_next_queued_input_if_idle();
            }
        } else if matches!(end, crate::app::TurnEnd::Cancelled) {
            // No active turn but user requested cancel - still notify the
            // engine and kill any stale turn-owned Lua tasks (tool calls,
            // bash executions, etc.). App-scoped background work survives.
            self.core.engine.send(UiCommand::Cancel);
            self.cancel_turn_lua_tasks();
            self.cancel_generation = self.cancel_generation.wrapping_add(1);
            self.busy_stack.clear();
            self.clear_compaction_preview();
            // Archive an interrupted outcome so the prompt bar shows
            // "interrupted" rather than falling back to idle/done.
            self.working.finish(TurnOutcome::Cancelled);
        }
    }

    pub(crate) fn finish_turn(&mut self, end: crate::app::TurnEnd) -> bool {
        let _perf = smelt_perf::perf::begin("tui:finish_turn");
        use crate::app::TurnEnd;

        let turn = self
            .agent
            .as_ref()
            .map(|turn| (turn.turn_id, turn.canonical));
        let (terminal_state, terminal_reason) = match &end {
            TurnEnd::Complete => (smelt_store::TurnState::Completed, None),
            TurnEnd::Cancelled => (
                smelt_store::TurnState::Cancelled,
                Some("user_cancelled".to_string()),
            ),
            TurnEnd::Errored { kind, .. } => (
                smelt_store::TurnState::Failed,
                Some(kind.map_or_else(
                    || "engine_error".to_string(),
                    |kind| format!("engine_error:{}", kind.as_str()),
                )),
            ),
        };

        self.sleep_inhibit.release();
        match end {
            TurnEnd::Cancelled => {
                self.core.engine.send(UiCommand::Cancel);
                self.cancel_turn_lua_tasks();
                self.cancel_generation = self.cancel_generation.wrapping_add(1);
                self.busy_stack.clear();
            }
            TurnEnd::Complete | TurnEnd::Errored { .. } => {}
        }

        let interrupted = !matches!(end, TurnEnd::Complete);
        let (error_kind, retry_at_ms) = match &end {
            TurnEnd::Errored { kind, retry_at_ms } => (*kind, *retry_at_ms),
            _ => (None, None),
        };
        let resumable = interrupted && is_resumable_turn_error(error_kind, retry_at_ms);
        let continuation_token = if !interrupted || resumable {
            let token = self.next_continuation_token;
            self.next_continuation_token = self.next_continuation_token.wrapping_add(1).max(1);
            self.pending_continuation_token = Some(token);
            Some(token)
        } else {
            self.pending_continuation_token = None;
            None
        };
        self.core.signals.set_dyn(
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

        let (meta, start_queued) = {
            let _perf = smelt_perf::perf::begin("tui:finish_turn:working_finish");
            match end {
                TurnEnd::Complete => {
                    let start_queued = !self.queued_inputs.is_empty() && !self.busy_stack.is_busy();
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
                    self.pending_history_appends.retain(|pending| {
                        pending.lifecycle() == PendingHistoryLifecycle::SessionScoped
                    });
                    let meta = self.working.finish(TurnOutcome::Cancelled);
                    self.drain_queued_inputs_into_prompt();
                    self.restore_session_metadata_after_rewind(self.session_history_len());
                    (meta, false)
                }
                TurnEnd::Errored { .. } => {
                    self.pending_history_appends.retain(|pending| {
                        pending.lifecycle() == PendingHistoryLifecycle::SessionScoped
                    });
                    let meta = self.working.finish(TurnOutcome::Errored);
                    // On error the queue is preserved so the user can resubmit.
                    (meta, false)
                }
            }
        };

        {
            let _perf = smelt_perf::perf::begin("tui:finish_turn:document_state");
            self.record_finished_turn_state(meta);
        }
        if matches!(end, TurnEnd::Complete) {
            self.apply_pending_history_appends_for_request();
        }
        self.sync_agent_mode_applied();
        self.sync_reasoning_effort_applied();
        let terminal_committed = match turn {
            Some((turn_id, true)) => {
                self.commit_terminal_turn(turn_id, terminal_state, terminal_reason)
            }
            Some((turn_id, false)) => {
                self.last_terminal_turn_id = Some(turn_id);
                if matches!(end, TurnEnd::Complete) {
                    self.schedule_session_save();
                } else {
                    self.save_session();
                }
                true
            }
            None => {
                if matches!(end, TurnEnd::Complete) {
                    self.schedule_session_save();
                } else {
                    self.save_session();
                }
                true
            }
        };
        start_queued && terminal_committed
    }

    /// Invokes the Lua handler for a plugin-defined tool; synchronous handlers resolve immediately, async ones park until `drive_tasks` completes them.
    pub(crate) fn handle_tool_call(
        &mut self,
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        let mode = self.core.config.mode.clone();
        let session_id = self.core.session.id.clone();
        let session_dir = self.current_session_dir();
        let (invocation, result) = self.lua.execute_tool_with_context(
            &tool_name,
            &args,
            request_id,
            &call_id,
            crate::lua::ToolEnv {
                mode,
                session_id: &session_id,
                session_dir: &session_dir,
            },
            self.core.clock.instant_now(),
        );
        match result {
            crate::lua::ToolExecResult::Immediate {
                content,
                is_error,
                metadata,
            } => {
                self.complete_lua_tool(invocation, call_id, content, is_error, metadata);
            }
            crate::lua::ToolExecResult::Pending => {}
        }
    }

    pub(crate) fn complete_lua_tool(
        &mut self,
        invocation: smelt_core::lua::ToolInvocationContext,
        call_id: String,
        mut content: String,
        mut is_error: bool,
        mut metadata: Option<serde_json::Value>,
    ) {
        match self.commit_tool_cwd_change(invocation, !is_error) {
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
            call_id,
            content,
            is_error,
            metadata,
        });
    }

    pub(crate) fn resolve_model_target(&mut self) -> Option<protocol::ModelTarget> {
        let Some(active) = self.core.config.active_model().cloned() else {
            self.notify_error_sticky(
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
            self.notify_error_sticky(format!("model '{}' is unavailable", active.key));
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
                self.notify_error_sticky(err.message());
                None
            }
        }
    }

    pub(crate) fn handle_process_completed(&mut self, id: String, exit_code: Option<i32>) {
        let id = display_safe_process_id(&id);
        let event = protocol::ProcessStatusEvent::background_process_completed(id, exit_code);
        let note = protocol::HistoryNote::process_status_event(event);
        if self.agent_is_running() {
            self.queue_history_append(crate::app::PendingHistoryAppend::process_status(note));
        } else if self.prompt_input_is_busy() {
            self.queued_inputs
                .try_push_turn(crate::app::QueuedInput::ProcessStatus(note));
        } else {
            self.agent = self.begin_process_status_turn(note);
        }
    }

    pub(crate) fn session_permission_entries(&self) -> Vec<PermissionEntry> {
        let approvals = self.core.permissions.approvals();
        let rt = approvals.read().unwrap();
        let mut entries = Vec::new();
        for approval in rt.session_tool_approvals() {
            entries.push(PermissionEntry {
                tool: approval.tool,
                pattern: approval.pattern.unwrap_or_else(|| "*".into()),
            });
        }
        for dir in rt.session_dirs() {
            entries.push(PermissionEntry {
                tool: "directory".into(),
                pattern: dir.display().to_string(),
            });
        }
        entries
    }

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
        session_entries: Vec<PermissionEntry>,
        session_path_grants: Vec<smelt_core::permissions::SessionPathGrant>,
        workspace_rules: Vec<smelt_core::permissions::store::Rule>,
    ) {
        let mut session_tools = Vec::new();
        let mut session_dirs: Vec<PathBuf> = Vec::new();
        for entry in session_entries {
            if entry.tool == "directory" {
                session_dirs.push(std::path::PathBuf::from(&entry.pattern));
            } else {
                session_tools.push(smelt_core::permissions::SessionToolApproval {
                    tool: entry.tool,
                    pattern: (entry.pattern != "*").then_some(entry.pattern),
                });
            }
        }

        smelt_core::permissions::store::save(&self.cwd, &workspace_rules);
        {
            let approval_store = self.core.permissions.approvals();
            approval_store.write().unwrap().set_session(
                session_tools,
                session_dirs,
                session_path_grants,
            );
        }
        self.reconcile_permissions();
    }

    fn reload_workspace_permissions(&mut self) {
        self.reconcile_permissions();
    }

    pub(crate) fn reset_session_permissions(&mut self) {
        self.core
            .permissions
            .approvals()
            .write()
            .unwrap()
            .clear_session();
    }

    /// Resolves a confirm dialog choice; returns `true` if the agent should be cancelled.
    pub(crate) fn resolve_confirm(
        &mut self,
        (choice, message): (ConfirmChoice, Option<String>),
        call_id: &str,
        request_id: u64,
        tool_name: &str,
    ) -> bool {
        let label = match &choice {
            ConfirmChoice::Yes => "approved",
            ConfirmChoice::Grant(option) => option.label.as_str(),
            ConfirmChoice::No => "denied",
        };
        if let Some(ref msg) = message {
            self.set_active_user_message(call_id, format!("{label}: {msg}"));
        }
        match choice {
            ConfirmChoice::Yes => {
                self.set_active_status(call_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::Grant(option) => {
                match option.scope {
                    ApprovalScope::Session => {
                        let approval_store = self.core.permissions.approvals();
                        let mut approvals = approval_store.write().unwrap();
                        for grant in option.grants {
                            approvals.add_session_grant(grant);
                        }
                    }
                    ApprovalScope::Workspace => {
                        for grant in option.grants {
                            add_workspace_grant(&self.cwd, grant);
                        }
                        self.reload_workspace_permissions();
                    }
                }
                self.set_active_status(call_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::No => {
                let has_message = message.is_some();
                self.send_permission_decision(request_id, false, message);
                self.finish_tool(call_id, ToolStatus::Denied, None, None);
                if has_message {
                    if let Some(ref mut ag) = self.agent {
                        ag.pending.retain(|p| p.call_id != call_id);
                    }
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
                    if let Some(ref mut ag) = self.agent {
                        ag.pending.clear();
                    }
                    true
                }
            }
        }
    }

    fn permission_decision_for_confirm(&self, req: &ConfirmRequest) -> Decision {
        self.active_permissions()
            .evaluate_tool_with_approvals(
                self.applied_agent_mode.clone(),
                smelt_core::permissions::ToolOrigin::Lua,
                &req.tool_name,
                &req.args,
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
            self.core.signals.set_dyn(
                "confirm_resolved",
                std::rc::Rc::new(smelt_core::signals::ConfirmResolved {
                    handle_id,
                    decision: label.into(),
                }),
            );
        }

        if approved {
            self.set_active_status(&req.call_id, ToolStatus::Pending);
            self.send_permission_decision(req.request_id, true, None);
        } else {
            self.send_permission_decision(req.request_id, false, None);
            self.finish_tool(&req.call_id, ToolStatus::Denied, None, None);
            if let Some(pending) = pending {
                pending.retain(|p| p.call_id != req.call_id);
            } else if let Some(ref mut ag) = self.agent {
                ag.pending.retain(|p| p.call_id != req.call_id);
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
            Decision::Ask | Decision::Error(_) => false,
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
                    req.tool_name = turn
                        .pending
                        .last()
                        .map(|p| p.name.clone())
                        .unwrap_or_default();
                }

                let outcome = turn.permissions.evaluate_tool_with_approvals(
                    self.applied_agent_mode.clone(),
                    smelt_core::permissions::ToolOrigin::Lua,
                    &req.tool_name,
                    &req.args,
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
                            Some(&mut turn.pending),
                        );
                        return SessionControl::Continue;
                    }
                    Decision::Ask | Decision::Error(_) => {}
                }

                if should_queue {
                    self.set_active_status(&req.call_id, ToolStatus::Confirm);
                    self.pending_dialog = true;
                    self.pending_dialogs.push_back(DeferredDialog::Confirm(req));
                    return SessionControl::Continue;
                }

                let options = turn.permissions.approval_options(
                    &req.tool_name,
                    &req.approval_candidates,
                    &outcome,
                );
                req.grant_options = confirm_grant_options(options.grant_sets, &self.cwd);

                self.close_focused_non_blocking_overlay();
                self.set_active_status(&req.call_id, ToolStatus::Confirm);

                let snapshot = smelt_core::signals::ConfirmRequested {
                    handle_id: 0,
                    tool_name: req.tool_name.clone(),
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
                self.lua.fire_confirm_open(handle_id);
                SessionControl::Continue
            }
        }
    }
}

fn confirm_grant_options(
    grant_sets: Vec<Vec<smelt_core::permissions::PermissionGrant>>,
    cwd: &str,
) -> Vec<smelt_core::transcript_model::ConfirmApprovalOption> {
    let mut out = Vec::new();
    for grants in grant_sets {
        let subject = smelt_core::permissions::PermissionGrant::display_subjects(&grants);
        let idx = out.len();
        out.push(smelt_core::transcript_model::ConfirmApprovalOption {
            id: format!("grant_{idx}_session"),
            label: format!("allow {subject} for this session"),
            scope: ApprovalScope::Session,
            grants: grants.clone(),
        });
        out.push(smelt_core::transcript_model::ConfirmApprovalOption {
            id: format!("grant_{idx}_workspace"),
            label: format!("allow {subject} in {}", pretty_cwd(cwd)),
            scope: ApprovalScope::Workspace,
            grants,
        });
    }
    out
}

fn pretty_cwd(cwd: &str) -> String {
    engine::paths::collapse_tilde(std::path::Path::new(cwd))
        .to_string_lossy()
        .into_owned()
}

fn add_workspace_grant(cwd: &str, grant: smelt_core::permissions::PermissionGrant) {
    match grant {
        smelt_core::permissions::PermissionGrant::Tool { tool } => {
            smelt_core::permissions::store::add_tool(cwd, &tool, Vec::new());
        }
        smelt_core::permissions::PermissionGrant::Command { tool, pattern } => {
            smelt_core::permissions::store::add_tool(cwd, &tool, vec![pattern]);
        }
        smelt_core::permissions::PermissionGrant::PathPrefix { dir } => {
            let dir = engine::paths::collapse_tilde(&dir)
                .to_string_lossy()
                .into_owned();
            smelt_core::permissions::store::add_dir(cwd, &dir);
        }
    }
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

    fn process_status_blocks(app: &crate::app::test_harness::TestApp) -> Vec<String> {
        let history = app.app.session_document.transcript.history();
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
        let history = app.app.session_document.transcript.history();
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

    fn perf_value_count(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> usize {
        snapshot
            .values
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.count)
            .unwrap_or(0)
    }

    fn perf_value_total(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
        snapshot
            .values
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.total)
            .unwrap_or(0)
    }

    fn perf_value_last(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> Option<u64> {
        snapshot
            .values
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.last)
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
            "store:transcript:read_descriptors_full",
            "store:transcript:descriptors_full_loaded",
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
        app.app.load_session(session);
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
            .notify_error_sticky("rate limit exceeded".to_string());
        assert!(app.app.notification_win().is_some());

        let turn = app
            .app
            .begin_agent_turn("try again", Content::text("try again"))
            .expect("test app has a usable model");
        app.app.agent = Some(turn);

        assert!(app.app.notification_win().is_none());
    }

    #[test]
    fn starting_command_continuation_dismisses_visible_notification() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let previous = app
            .app
            .begin_agent_turn("previous", Content::text("previous"))
            .expect("previous turn starts");
        app.app.agent = Some(previous);
        app.app.discard_turn(crate::app::TurnEnd::Complete);
        app.app.notify_error_sticky("quota exceeded".to_string());
        assert!(app.app.notification_win().is_some());

        let turn = app
            .app
            .begin_command_request_turn(
                "continue".into(),
                String::new(),
                smelt_core::custom_commands::CommandOverrides::default(),
                crate::app::CommandTurnStart::ContinueFromLast,
            )
            .expect("test app has a usable model");
        app.app.agent = Some(turn);

        assert!(app.app.notification_win().is_none());
    }

    #[test]
    fn user_turn_commits_request_before_dispatch_without_duplicate_history() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        let turn = app
            .app
            .begin_agent_turn("first request", Content::text("first request"))
            .expect("test app has a usable model");
        app.app.agent = Some(turn);

        assert!(matches!(
            app.app.core.session.history.last(),
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
        let protocol::ModelHistorySource::Store { end_index, .. } = &payload.history else {
            panic!("interactive turn should dispatch store-backed model history");
        };
        assert_eq!(*end_index, app.app.core.session.history.len() - 1);
        assert_eq!(
            payload.input.provider_content().text_content(),
            "first request"
        );

        app.app.flush_persist();
        let loaded = crate::app::history::materialize_full_session(
            &app.app.core.session.id,
            crate::app::history::FullSessionMaterializationReason::TestSavedSessionAssertion,
        )
        .expect("session saved");
        assert!(matches!(
            loaded.history.last(),
            Some(HistoryItem::User { content, .. }) if content.text_content() == "first request"
        ));
    }

    #[test]
    fn terminal_turn_commits_final_history_and_completed_state_together() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let turn = app
            .app
            .begin_agent_turn("finish me", Content::text("finish me"))
            .expect("turn starts");
        let turn_id = turn.turn_id;
        app.app.agent = Some(turn);
        app.app
            .session_append_history(HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(Content::text("finished")),
                None,
                Vec::new(),
            )));

        app.app.discard_turn(crate::app::TurnEnd::Complete);

        let reader = smelt_store::SessionReader::open_existing(smelt_core::session::dir_for(
            &app.app.core.session,
        ))
        .expect("open completed session");
        let stored = reader
            .turn(smelt_store::TurnId::new(turn_id))
            .unwrap()
            .expect("stored turn");
        assert_eq!(stored.state, smelt_store::TurnState::Completed);
        let head = reader.store_head().unwrap();
        let history = reader
            .read_history_items_range(0..head.history_len.get() as usize)
            .unwrap();
        assert!(matches!(
            history.last(),
            Some(HistoryItem::Assistant { .. })
        ));
        assert!(stored
            .finished_at_ms
            .is_some_and(|finished| finished >= stored.created_at_ms));
    }

    #[test]
    fn terminal_transition_failure_keeps_queued_turn_from_starting() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let turn = app
            .app
            .begin_agent_turn("first", Content::text("first"))
            .expect("turn starts");
        let turn_id = turn.turn_id;
        app.app.agent = Some(turn);
        let _ = app.app.flush_persist();
        app.push_queued_message("must remain queued".into());
        app.clear_actions();
        app.app
            .persistence
            .as_ref()
            .expect("persistence actor")
            .inject_commit_failure(smelt_store::SessionCommitFailure::OwnershipLost);

        app.app.discard_turn(crate::app::TurnEnd::Complete);

        assert!(!app.agent_running());
        assert_eq!(app.state().queued_inputs, vec!["must remain queued"]);
        assert!(app.actions().iter().all(|action| !matches!(
            action,
            crate::app::test_harness::Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )));
        assert!(app.app.session_document_has_unflushed_work());
        let reader = smelt_store::SessionReader::open_existing(smelt_core::session::dir_for(
            &app.app.core.session,
        ))
        .unwrap();
        assert_eq!(
            reader
                .turn(smelt_store::TurnId::new(turn_id))
                .unwrap()
                .unwrap()
                .state,
            smelt_store::TurnState::Running
        );
    }

    #[test]
    fn enter_submits_one_canonical_turn_before_engine_dispatch() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.type_text("phase zero submit");
        app.clear_actions();

        smelt_perf::perf::set_enabled(true);
        smelt_perf::perf::clear();
        app.press(crossterm::event::KeyCode::Enter);
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);

        let mut dispatched_turns = app.actions().iter().filter_map(|action| match action {
            crate::app::test_harness::Action::EngineSend(command) => match command.as_ref() {
                protocol::UiCommand::StartTurn(payload) => Some(payload.as_ref()),
                _ => None,
            },
            _ => None,
        });
        let payload = dispatched_turns.next().expect("Enter dispatched StartTurn");
        assert!(
            dispatched_turns.next().is_none(),
            "Enter must dispatch exactly one StartTurn"
        );
        assert_eq!(
            perf_value_count(&snapshot, "agent:dispatch:start_turn:at_us"),
            1,
            "dispatch instrumentation should observe exactly one StartTurn"
        );
        assert_eq!(
            perf_value_total(&snapshot, "store:transaction:submit_turn:attempts"),
            1,
            "Enter should issue exactly one SubmitTurn transaction"
        );
        assert_eq!(
            perf_value_total(&snapshot, "store:transaction:submit_turn:committed"),
            1,
            "Enter should commit exactly one ready turn before dispatch"
        );
        assert_eq!(
            perf_value_total(&snapshot, "store:transaction:session_commit:attempts"),
            0,
            "Enter must not issue a preliminary ordinary save transaction"
        );
        assert_eq!(
            perf_value_last(&snapshot, "agent:dispatch:start_turn:turn_id"),
            Some(payload.turn_id)
        );

        let committed_at =
            perf_value_last(&snapshot, "store:transaction:submit_turn:committed_at_us")
                .expect("SubmitTurn commit timestamp");
        let dispatched_at = perf_value_last(&snapshot, "agent:dispatch:start_turn:at_us")
            .expect("dispatch timestamp");
        assert!(
            committed_at <= dispatched_at,
            "expected SubmitTurn commit ({committed_at}) before dispatch ({dispatched_at})"
        );
        for metric in [
            "persist:projection:compat_export:requested_at_us",
            "session:compat_export:meta:completed_at_us",
            "session:compat_export:content:completed_at_us",
        ] {
            if let Some(exported_at) = perf_value_last(&snapshot, metric) {
                assert!(
                    dispatched_at <= exported_at,
                    "compatibility export {metric} completed before dispatch"
                );
            }
        }

        let session_dir = smelt_core::session::dir_for(&app.app.core.session);
        let reader = smelt_store::SessionReader::open_database(session_dir.join("session.db"))
            .expect("open committed session");
        let head = reader.store_head().expect("read committed head");
        let stored_turn = reader
            .turn(smelt_store::TurnId::new(payload.turn_id))
            .expect("read submitted turn")
            .expect("submitted turn exists");
        assert_eq!(stored_turn.kind, smelt_store::TurnKind::User);
        assert!(matches!(
            stored_turn.state,
            smelt_store::TurnState::Ready | smelt_store::TurnState::Running
        ));
        assert_eq!(
            stored_turn.submitted_revision.get(),
            payload.persistence.store_revision
        );
        assert_eq!(
            perf_value_last(&snapshot, "agent:dispatch:start_turn:revision"),
            Some(stored_turn.submitted_revision.get())
        );
        assert!(head.revision >= stored_turn.submitted_revision);
        let history = reader
            .read_history_items_range(0..head.history_len.get() as usize)
            .expect("read committed history");
        assert!(matches!(
            history.last(),
            Some(HistoryItem::User { content, .. }) if content.text_content() == "phase zero submit"
        ));
    }

    #[test]
    fn canonical_submit_failure_preserves_prompt_and_prevents_dispatch() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::note(protocol::HistoryNote::context(
                "published baseline",
            )));
        app.app.save_session_and_flush();
        app.type_text("retry this exact input");
        app.app
            .persistence
            .as_ref()
            .expect("persistence actor")
            .inject_commit_failure(smelt_store::SessionCommitFailure::InvalidCommand {
                message: "injected pre-commit rejection".into(),
            });

        app.press(crossterm::event::KeyCode::Enter);

        assert_eq!(app.state().prompt_text, "retry this exact input");
        assert!(!app.state().agent_running);
        assert_eq!(app.app.session_history_len(), 1);
        assert!(app.actions().iter().all(|action| !matches!(
            action,
            crate::app::test_harness::Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )));

        app.app
            .persistence
            .as_ref()
            .unwrap()
            .retry_blocked()
            .unwrap();
        app.clear_actions();
        app.press(crossterm::event::KeyCode::Enter);
        assert!(app.agent_running());
        let _ = app.app.flush_persist();
        let reader = smelt_store::SessionReader::open_existing(smelt_core::session::dir_for(
            &app.app.core.session,
        ))
        .unwrap();
        let head = reader.store_head().unwrap();
        let history = reader
            .read_history_items_range(0..head.history_len.get() as usize)
            .unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(
            history
                .iter()
                .filter(|item| matches!(item, HistoryItem::User { content, .. } if content.text_content() == "retry this exact input"))
                .count(),
            1
        );
    }

    #[test]
    fn command_submit_failure_rolls_back_staged_history_and_retries_once() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::note(protocol::HistoryNote::context(
                "published baseline",
            )));
        app.app.save_session_and_flush();
        let baseline_history_len = app.app.session_history_len();
        let baseline_transcript_len = app.app.session_document.transcript.history().order.len();
        app.app
            .persistence
            .as_ref()
            .expect("persistence actor")
            .inject_commit_failure(smelt_store::SessionCommitFailure::InvalidCommand {
                message: "injected command rejection".into(),
            });

        assert!(app
            .app
            .begin_command_request_turn(
                "/retry".into(),
                "retry command".into(),
                smelt_core::custom_commands::CommandOverrides::default(),
                crate::app::CommandTurnStart::Fresh,
            )
            .is_none());

        assert_eq!(app.app.session_history_len(), baseline_history_len);
        assert_eq!(
            app.app.session_document.transcript.history().order.len(),
            baseline_transcript_len
        );
        assert!(app.app.core.session.first_user_message.is_none());
        assert!(user_blocks(&app).is_empty());
        assert!(app
            .drain_engine_sends()
            .into_iter()
            .all(|command| !matches!(command, protocol::UiCommand::StartTurn(_))));

        app.app
            .persistence
            .as_ref()
            .unwrap()
            .retry_blocked()
            .unwrap();
        let turn = app
            .app
            .begin_command_request_turn(
                "/retry".into(),
                "retry command".into(),
                smelt_core::custom_commands::CommandOverrides::default(),
                crate::app::CommandTurnStart::Fresh,
            )
            .expect("command retry starts");
        app.app.agent = Some(turn);
        let reader = smelt_store::SessionReader::open_existing(smelt_core::session::dir_for(
            &app.app.core.session,
        ))
        .unwrap();
        let head = reader.store_head().unwrap();
        let history = reader
            .read_history_items_range(0..head.history_len.get() as usize)
            .unwrap();
        assert_eq!(history.len(), baseline_history_len + 1);
        assert_eq!(
            history
                .iter()
                .filter(|item| matches!(item, HistoryItem::User { content, .. } if content.text_content() == "retry command"))
                .count(),
            1
        );
    }

    #[test]
    fn note_submit_failure_rolls_back_staged_history_and_retries_once() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::note(protocol::HistoryNote::context(
                "published baseline",
            )));
        app.app.save_session_and_flush();
        let baseline_history_len = app.app.session_history_len();
        let baseline_transcript_len = app.app.session_document.transcript.history().order.len();
        let note = protocol::HistoryNote::process_status("background process 9 finished");
        app.app
            .persistence
            .as_ref()
            .expect("persistence actor")
            .inject_commit_failure(smelt_store::SessionCommitFailure::InvalidCommand {
                message: "injected note rejection".into(),
            });

        assert!(app.app.begin_process_status_turn(note.clone()).is_none());

        assert_eq!(app.app.session_history_len(), baseline_history_len);
        assert_eq!(
            app.app.session_document.transcript.history().order.len(),
            baseline_transcript_len
        );
        assert!(process_status_blocks(&app).is_empty());
        assert!(app
            .drain_engine_sends()
            .into_iter()
            .all(|command| !matches!(command, protocol::UiCommand::StartTurn(_))));

        app.app
            .persistence
            .as_ref()
            .unwrap()
            .retry_blocked()
            .unwrap();
        let turn = app
            .app
            .begin_process_status_turn(note)
            .expect("note retry starts");
        app.app.agent = Some(turn);
        let reader = smelt_store::SessionReader::open_existing(smelt_core::session::dir_for(
            &app.app.core.session,
        ))
        .unwrap();
        let head = reader.store_head().unwrap();
        let history = reader
            .read_history_items_range(0..head.history_len.get() as usize)
            .unwrap();
        assert_eq!(history.len(), baseline_history_len + 1);
        assert_eq!(
            history
                .iter()
                .filter(|item| item.note_kind() == Some(protocol::HistoryNoteKind::ProcessStatus))
                .count(),
            1
        );
    }

    #[test]
    fn legacy_history_without_turn_rows_explains_continuation_requirement() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::user(Content::text("legacy request")));
        app.app
            .session_append_history(HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(Content::text("legacy response")),
                None,
                Vec::new(),
            )));
        app.app.save_session_and_flush();
        assert_eq!(app.app.last_terminal_turn_id, None);

        assert!(app.app.begin_agent_turn("", Content::text("")).is_none());
        assert!(app.app.notification.as_ref().is_some_and(|notification| {
            notification.summary.contains("no durable prior turn")
        }));
        assert!(app
            .app
            .begin_command_request_turn(
                "/continue".into(),
                String::new(),
                smelt_core::custom_commands::CommandOverrides::default(),
                crate::app::CommandTurnStart::ContinueFromLast,
            )
            .is_none());
        assert!(app
            .drain_engine_sends()
            .into_iter()
            .all(|command| !matches!(command, protocol::UiCommand::StartTurn(_))));
        let reader = smelt_store::SessionReader::open_existing(smelt_core::session::dir_for(
            &app.app.core.session,
        ))
        .unwrap();
        assert!(reader.turns().unwrap().is_empty());
    }

    #[test]
    fn engine_rejection_records_failed_after_durable_submit() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.disconnect_engine_commands();

        assert!(app
            .app
            .begin_agent_turn("rejected", Content::text("rejected"))
            .is_none());
        let _ = app.app.flush_persist();

        let reader = smelt_store::SessionReader::open_existing(smelt_core::session::dir_for(
            &app.app.core.session,
        ))
        .expect("submitted session was published");
        let turns = reader.turns().expect("read turns");
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].state, smelt_store::TurnState::Failed);
        assert_eq!(
            turns[0].terminal_reason.as_deref(),
            Some("engine_channel_rejected")
        );
    }

    #[test]
    fn committed_ready_turn_is_interrupted_after_receipt_publication_failure() {
        let session_id;
        {
            let mut app = crate::app::test_harness::TestApp::builder().build();
            app.app
                .session_append_history(HistoryItem::note(protocol::HistoryNote::context(
                    "published baseline",
                )));
            app.app.save_session_and_flush();
            session_id = app.app.core.session.id.clone();
            app.type_text("durable before dispatch failure");
            app.clear_actions();
            app.app
                .persistence
                .as_ref()
                .expect("persistence actor")
                .inject_publish_failure();

            app.press(crossterm::event::KeyCode::Enter);

            assert!(!app.state().agent_running);
            assert!(app.actions().iter().all(|action| !matches!(
                action,
                crate::app::test_harness::Action::EngineSend(command)
                    if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
            )));
            let reader = smelt_store::SessionReader::open_existing(
                smelt_core::session::dir_for_id(&session_id),
            )
            .expect("open committed session");
            assert_eq!(
                reader.turns().unwrap()[0].state,
                smelt_store::TurnState::Ready
            );
            assert!(app.app.session_access.is_read_only());
            let staged_history_len = app.app.session_history_len();
            let staged_transcript_len = app.app.session_document.transcript.history().order.len();
            app.clear_actions();

            app.press(crossterm::event::KeyCode::Enter);

            assert_eq!(app.app.session_history_len(), staged_history_len);
            assert_eq!(
                app.app.session_document.transcript.history().order.len(),
                staged_transcript_len
            );
            assert!(app.actions().iter().all(|action| !matches!(
                action,
                crate::app::test_harness::Action::EngineSend(command)
                    if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
            )));
            assert_eq!(reader.turns().unwrap().len(), 1);
        }

        let session_dir = smelt_core::session::dir_for_id(&session_id);
        let root = session_dir.parent().expect("session root");
        let writer = smelt_store::OwnedSessionWriter::open(root, &session_id)
            .expect("writable restart recovers nonterminal turn");
        let recovery = writer
            .startup_recovery()
            .expect("ready turn was interrupted on restart");
        assert_eq!(
            recovery.interrupted_turns,
            vec![smelt_store::TurnId::new(1)]
        );
        let reader = smelt_store::SessionReader::open_existing(session_dir).unwrap();
        assert_eq!(
            reader
                .turn(smelt_store::TurnId::new(1))
                .unwrap()
                .unwrap()
                .state,
            smelt_store::TurnState::Interrupted
        );
        writer.release().unwrap();
    }

    #[test]
    fn session_resume_interrupts_running_turn_without_redispatch() {
        let home = crate::app::test_harness::test_home_guard();
        let session_id;
        let turn_id;
        {
            let mut app =
                crate::app::test_harness::TestApp::builder().build_with_test_home_guard(&home);
            let turn = app
                .app
                .begin_agent_turn("before restart", Content::text("before restart"))
                .expect("turn starts");
            turn_id = turn.turn_id;
            app.app.agent = Some(turn);
            let _ = app.app.flush_persist();
            session_id = app.app.core.session.id.clone();
            let reader = smelt_store::SessionReader::open_existing(
                smelt_core::session::dir_for_id(&session_id),
            )
            .unwrap();
            assert_eq!(
                reader
                    .turn(smelt_store::TurnId::new(turn_id))
                    .unwrap()
                    .unwrap()
                    .state,
                smelt_store::TurnState::Running
            );
        }

        let mut resumed =
            crate::app::test_harness::TestApp::builder().build_without_test_home_reset(&home);
        resumed.clear_actions();
        resumed.app.load_session_by_id(&session_id);

        assert_eq!(
            resumed.app.core.session.id,
            session_id,
            "resume failed: {:?}",
            resumed
                .app
                .notification
                .as_ref()
                .map(|notification| notification.summary.as_str())
        );
        assert!(!resumed.agent_running());
        assert!(resumed.actions().iter().all(|action| !matches!(
            action,
            crate::app::test_harness::Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )));
        assert_eq!(resumed.app.last_terminal_turn_id, Some(turn_id));
        assert!(!resumed.app.session_access.is_read_only());
        let reader =
            smelt_store::SessionReader::open_existing(smelt_core::session::dir_for_id(&session_id))
                .unwrap();
        assert_eq!(
            reader
                .turn(smelt_store::TurnId::new(turn_id))
                .unwrap()
                .unwrap()
                .state,
            smelt_store::TurnState::Interrupted
        );
        assert_eq!(
            resumed.app.session_document.acknowledged_head(),
            reader.store_head().unwrap()
        );
    }

    #[test]
    fn request_start_dispatches_store_history_without_full_reads() {
        const BASE_HISTORY_LEN: usize = 128;
        let mut app = large_saved_session_app(BASE_HISTORY_LEN);
        let old_history_len = app.app.core.session.history.len();
        assert!(old_history_len >= BASE_HISTORY_LEN);

        smelt_perf::perf::set_enabled(true);
        smelt_perf::perf::clear();
        let turn = app
            .app
            .begin_agent_turn("new request", Content::text("new request"))
            .expect("test app has a usable model");
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);
        app.app.agent = Some(turn);

        assert_no_full_request_start_reads(&snapshot);
        assert_perf_value_at_most(&snapshot, "store:history:dirty_suffix_rows", 1);
        assert_perf_value_at_most(&snapshot, "store:session:history_rows_inserted", 1);
        assert_perf_value_at_most(
            &snapshot,
            "store:transcript:dirty_descriptor_suffix_rows",
            1,
        );
        assert_perf_value_at_most(&snapshot, "store:transcript:descriptor_db_rows_inserted", 1);

        let payload = app
            .drain_engine_sends()
            .into_iter()
            .find_map(|cmd| match cmd {
                protocol::UiCommand::StartTurn(payload) => Some(payload),
                _ => None,
            })
            .expect("turn dispatched");
        match payload.history {
            protocol::ModelHistorySource::Store {
                ref prefix,
                first_live_index,
                end_index,
                ref suffix,
                ..
            } => {
                assert!(prefix.is_empty());
                assert_eq!(first_live_index, 0);
                assert_eq!(end_index, old_history_len);
                assert!(suffix.is_empty());
            }
            protocol::ModelHistorySource::Items { .. } => {
                panic!("request start cloned full history")
            }
        }
        assert_eq!(
            payload.input.provider_content().text_content(),
            "new request"
        );
        assert_eq!(app.app.core.session.history.len(), old_history_len + 1);
    }

    #[test]
    fn command_turn_metadata_snapshot_matches_committed_request() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        let turn = app
            .app
            .begin_command_request_turn(
                "/fix".into(),
                "fix it".into(),
                smelt_core::custom_commands::CommandOverrides::default(),
                crate::app::CommandTurnStart::Fresh,
            )
            .expect("test app has a usable model");
        app.app.agent = Some(turn);

        assert_eq!(
            app.app.core.session.first_user_message.as_deref(),
            Some("/fix")
        );
        assert_eq!(
            app.app
                .core
                .session
                .metadata_snapshots
                .as_slice()
                .last()
                .map(|(idx, _)| *idx),
            Some(app.app.core.session.history.len())
        );
        assert!(matches!(
            app.app.core.session.history.last(),
            Some(HistoryItem::User {
                display: Some(display),
                command: true,
                ..
            }) if display == "/fix"
        ));
        let transcript = app.app.session_document.transcript.history();
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

        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.cwd = tmp.path().to_string_lossy().into_owned();
        let expanded = app.app.expand_at_file_refs_in_text("summarize @note.txt");

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

        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.cwd = tmp.path().to_string_lossy().into_owned();
        let expanded = app.app.expand_at_file_refs_in_text("summarize @nb.ipynb");

        let path = file.to_string_lossy();
        assert!(expanded.contains(&format!(
            "<attached_file path=\"{path}\" tool=\"read_file\" already_read=\"true\" source=\"user_attachment\">"
        )));
        assert!(expanded.contains("Called the read_file tool with the following input:"));
        assert!(expanded.contains("--- Cell 0 [markdown] id=intro ---"));
        assert!(app.app.core.files.has(&path));
    }

    #[test]
    fn idle_process_completion_starts_turn_with_process_status_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app.handle_process_completed("1234".into(), Some(0));

        assert!(app.app.agent_is_running());
        assert_eq!(user_blocks(&app), Vec::<String>::new());
        assert_eq!(
            process_status_blocks(&app),
            vec!["background process 1234 finished successfully"]
        );
    }

    #[test]
    fn process_status_resize_keeps_transcript_cursor_in_bounds() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::ProcessCompleted {
                id: String::new(),
                exit_code: None,
            },
        ));
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
    fn running_agent_process_completion_queues_process_status_append() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(7);

        let before = app
            .app
            .pending_history_appends
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();

        app.app.handle_process_completed("4242".into(), Some(9));

        assert!(process_status_blocks(&app).is_empty());
        let expected = protocol::HistoryItem::note(protocol::HistoryNote::process_status_event(
            protocol::ProcessStatusEvent::background_process_completed("4242", Some(9)),
        ));
        let after = app
            .app
            .pending_history_appends
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();
        assert_eq!(&after[..before.len()], before.as_slice());
        assert_eq!(&after[before.len()..], std::slice::from_ref(&expected));

        let process_status_appends: Vec<_> = app
            .app
            .pending_history_appends
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

        assert!(app.app.agent_is_running());
        assert_eq!(process_status_blocks(&app), vec![text]);
        assert!(user_blocks(&app).is_empty());
    }

    #[test]
    fn process_status_history_update_stays_out_of_lua_conversation() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app.handle_process_completed("751225".into(), Some(1));
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
            protocol::ProcessStatusEvent::background_process_completed("751225", Some(1)),
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
            &app.app.core.session.history[1],
            HistoryItem::Note(protocol::HistoryNote::ProcessStatus { text, event })
                if text == "background process 751225 exited with code 1"
                    && event.as_ref().and_then(protocol::ProcessStatusEvent::process_id) == Some("751225")
                    && event.as_ref().and_then(|event| event.exit_code()) == Some(1)
        ));

        let (count, first_content, contains_marker): (i64, String, bool) = {
            let _guard = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .lua
                .lua
                .load(
                    r#"
                    local rows = smelt.session.conversation()
                    local contains_marker = false
                    for _, row in ipairs(rows) do
                        if string.find(row.content, "%[smelt:process%]") then
                            contains_marker = true
                        end
                    end
                    return #rows, rows[1] and rows[1].content or "", contains_marker
                    "#,
                )
                .eval()
                .expect("conversation query succeeds")
        };

        assert_eq!(count, 1);
        assert_eq!(first_content, "previous user message");
        assert!(!contains_marker);
    }

    #[test]
    fn lua_conversation_limit_returns_recent_rows() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![
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
        ];

        let (count, first, second): (i64, String, String) = {
            let _guard = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .lua
                .lua
                .load(
                    r#"
                    local rows = smelt.session.conversation({ limit = 2 })
                    return #rows, rows[1] and rows[1].content or "", rows[2] and rows[2].content or ""
                    "#,
                )
                .eval()
                .expect("conversation limit query succeeds")
        };

        assert_eq!(count, 2);
        assert_eq!(first, "second user");
        assert_eq!(second, "second assistant");
    }

    #[tokio::test]
    async fn cancelling_turn_does_not_kill_registered_background_process() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let registry = app.app.core.processes.clone();
        let child = smelt_core::process::spawn_shell_child(
            "echo alive; sleep 30",
            &smelt_core::process::ShellSpec::default(),
        )
        .unwrap();
        let id = registry.child_id(&child);
        registry.spawn(
            id.clone(),
            "echo alive; sleep 30",
            child,
            std::time::Instant::now(),
        );
        app.start_turn(1);

        app.cancel();

        assert!(registry.snapshot_output(&id).unwrap().running);
        assert_eq!(registry.running_count(), 1);
        let _ = registry.stop(&id).await;
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
