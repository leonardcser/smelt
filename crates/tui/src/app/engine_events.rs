use crate::app::{PendingTool, SessionControl, TuiApp};
use protocol::EngineEvent;
use smelt_core::transcript_model::{Block, ToolOutput, ToolStatus};
use smelt_core::working::{TurnOutcome, TurnPhase};
use smelt_core::ConfirmRequest;
use std::time::Duration;

fn starts_assistant_or_tool_output(ev: &EngineEvent) -> bool {
    match ev {
        EngineEvent::Thinking { content } | EngineEvent::Text { content } => !content.is_empty(),
        EngineEvent::ThinkingDelta { delta } | EngineEvent::TextDelta { delta } => {
            !delta.is_empty()
        }
        EngineEvent::ToolCallDraftStarted { .. }
        | EngineEvent::ToolCallDraftFinished { .. }
        | EngineEvent::ToolStarted { .. }
        | EngineEvent::ToolOutput { .. }
        | EngineEvent::ToolFinished { .. }
        | EngineEvent::RequestPermission { .. } => true,
        EngineEvent::ToolCallDraftDelta { delta, .. } => !delta.is_empty(),
        EngineEvent::HistoryAppended { items, .. } => items
            .iter()
            .any(|item| matches!(item, protocol::HistoryItem::Assistant(_))),
        EngineEvent::HistoryUpdated { history, .. } => history
            .iter()
            .any(|item| matches!(item, protocol::HistoryItem::Assistant(_))),
        _ => false,
    }
}

impl TuiApp {
    pub(crate) fn publish_visible_token_usage(&mut self, usage: protocol::TokenUsage) {
        self.core
            .signals
            .set_dyn("tokens_used", std::rc::Rc::new(usage));
    }

    pub(crate) fn reset_visible_context_tokens(&mut self) {
        self.publish_visible_token_usage(protocol::TokenUsage {
            context_tokens: Some(0),
            prompt_tokens: Some(0),
            ..Default::default()
        });
    }

    pub(crate) fn record_visible_token_usage(&mut self, usage: protocol::TokenUsage) {
        if !self.session_is_read_only() {
            if let Some(tokens) = usage.context_tokens.or(usage.prompt_tokens) {
                if tokens > 0 {
                    self.core
                        .session
                        .record_context_tokens(tokens, self.active_context_token_identity());
                    self.context_tokens_updated_this_turn = true;
                    if self.active_provider_supports_mid_turn_reasoning_changes() {
                        self.sync_reasoning_effort_applied();
                    }
                }
            }
        }
        self.publish_visible_token_usage(usage);
    }

    fn resolve_tool_summary_for_engine_event(
        &self,
        tool_name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
        summary: protocol::StyledLines,
    ) -> protocol::StyledLines {
        if !summary.is_empty() {
            return summary;
        }
        let summary =
            crate::app::history::ToolSummaryResolver::new(&self.lua).resolve(tool_name, args);
        if summary.is_empty() {
            protocol::StyledLines::from_plain(tool_name)
        } else {
            summary
        }
    }

    fn begin_tool_block_for_engine_event(
        &mut self,
        pending: &mut Vec<PendingTool>,
        call_id: String,
        tool_name: String,
        summary: protocol::StyledLines,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        if pending.iter().any(|p| p.call_id == call_id) {
            return;
        }
        self.flush_streaming_thinking();
        self.flush_streaming_text();
        if !self.promote_tool_draft(
            call_id.clone(),
            tool_name.clone(),
            summary.clone(),
            args.clone(),
        ) {
            self.start_tool(call_id.clone(), tool_name.clone(), summary, args.clone());
        }
        self.core.signals.emit_dyn(
            "tool_start",
            std::rc::Rc::new(smelt_core::signals::ToolStart {
                tool: tool_name.clone(),
                args: args.clone(),
            }),
        );
        self.pump_lua();
        pending.push(PendingTool {
            call_id,
            name: tool_name,
        });
    }

    fn finish_tool_for_engine_event(
        &mut self,
        pending: &mut Vec<PendingTool>,
        call_id: String,
        result: protocol::ToolOutcome,
        elapsed_ms: Option<u64>,
        status: ToolStatus,
    ) {
        let mut finished_tool_name: Option<String> = None;
        let mut finished_is_error = false;
        if let Some(idx) = pending.iter().position(|p| p.call_id == call_id) {
            let removed = pending.remove(idx);
            finished_tool_name = Some(removed.name.clone());
            finished_is_error = result.is_error;
            let output = Some(Box::new(ToolOutput {
                content: result.content,
                is_error: result.is_error,
                metadata: result.metadata,
            }));
            let elapsed = elapsed_ms.map(Duration::from_millis);
            self.finish_tool(&call_id, status, output, elapsed);
        }
        if let Some(tool_name) = finished_tool_name {
            self.core.signals.emit_dyn(
                "tool_end",
                std::rc::Rc::new(smelt_core::signals::ToolEnd {
                    tool: tool_name,
                    is_error: finished_is_error,
                    elapsed_ms,
                }),
            );
            self.pump_lua();
        }
    }

    /// Route an `EngineEvent` through the correct branch of the agent
    /// state machine. When a turn is active, delegates to
    /// `handle_engine_event` + `dispatch_control` and discards the turn on
    /// a stop signal; when idle, falls through to `handle_idle_engine_event`.
    /// Shared by the production main loop, the test harness, and the
    /// scenario replay binary so all three drive identical state.
    pub fn dispatch_engine_event(&mut self, ev: EngineEvent) -> bool {
        let _perf = smelt_perf::perf::begin("tui:dispatch_engine_event");
        // Engine ask callbacks can call UiHost Lua APIs such as transcript updates.
        crate::lua::with_app_ptr(self, |app| app.dispatch_engine_event_inner(ev))
    }

    fn dispatch_engine_event_inner(&mut self, ev: EngineEvent) -> bool {
        if let Some(mut ag) = self.agent.take() {
            if starts_assistant_or_tool_output(&ev) {
                ag.assistant_output_started = true;
            }
            let prev_dispatching_turn_id = self.dispatching_turn_id.replace(ag.turn_id);
            let ctrl = self.handle_engine_event(ev, ag.turn_id, &mut ag.pending);
            let end = self.dispatch_control(ctrl, &mut ag);
            self.dispatching_turn_id = prev_dispatching_turn_id;
            self.agent = Some(ag);
            match end {
                SessionControl::Continue | SessionControl::NeedsConfirm(_) => true,
                SessionControl::Done => {
                    self.discard_turn(crate::app::TurnEnd::Complete);
                    false
                }
                SessionControl::Error { kind, retry_at_ms } => {
                    self.discard_turn(crate::app::TurnEnd::Errored { kind, retry_at_ms });
                    false
                }
            }
        } else {
            self.handle_idle_engine_event(ev);
            true
        }
    }

    pub(crate) fn handle_engine_event(
        &mut self,
        ev: EngineEvent,
        turn_id: u64,
        pending: &mut Vec<PendingTool>,
    ) -> SessionControl {
        match ev {
            EngineEvent::Ready => SessionControl::Continue,
            EngineEvent::TokenUsage {
                usage,
                tokens_per_sec,
                cost_usd,
                background,
            } => {
                if !background {
                    self.record_visible_token_usage(usage.clone());
                    if let Some(tps) = tokens_per_sec {
                        self.working.record_tokens_per_sec(tps);
                    }
                    {
                        self.working.begin(TurnPhase::Working);
                    };
                }
                let cost = cost_usd.unwrap_or(0.0);
                if !self.session_is_read_only() {
                    self.mark_session_dirty();
                    self.core.session.session_cost_usd += cost;
                    self.core.session.session_usage.accumulate(&usage);
                }
                crate::metrics::append(&crate::metrics::MetricsEntry {
                    timestamp_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                    completion_tokens: usage.completion_tokens.unwrap_or(0),
                    model: self.core.config.model.clone(),
                    cost_usd,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_write_tokens: usage.cache_write_tokens,
                    reasoning_tokens: usage.reasoning_tokens,
                });
                SessionControl::Continue
            }
            EngineEvent::ToolOutput { call_id, chunk } => {
                self.append_active_output(&call_id, &chunk);
                SessionControl::Continue
            }
            EngineEvent::Steered { text, count } => {
                self.flush_streaming_thinking();
                self.flush_streaming_text();
                let drained = self.queued_inputs.drain_request_ack(count);
                if !drained.is_empty() {
                    let display = drained
                        .iter()
                        .map(crate::app::QueuedInput::display)
                        .collect::<Vec<_>>()
                        .join("\n");
                    let text = if display.is_empty() { text } else { display };
                    self.push_block(Block::User {
                        text,
                        image_labels: vec![],
                    });
                }
                SessionControl::Continue
            }
            EngineEvent::ThinkingDelta { delta } => {
                let bytes = delta.len();
                self.append_streaming_thinking(&delta);
                self.core.signals.emit_dyn(
                    "stream_delta",
                    std::rc::Rc::new(smelt_core::signals::StreamDelta {
                        kind: "thinking".to_string(),
                        bytes,
                        text: delta,
                        call_id: None,
                        tool_name: None,
                    }),
                );
                SessionControl::Continue
            }
            EngineEvent::Thinking { content } => {
                self.flush_streaming_thinking();
                self.push_block(Block::Thinking { content });
                SessionControl::Continue
            }
            EngineEvent::TextDelta { delta } => {
                let bytes = delta.len();
                self.append_streaming_text(&delta);
                self.core.signals.emit_dyn(
                    "stream_delta",
                    std::rc::Rc::new(smelt_core::signals::StreamDelta {
                        kind: "text".to_string(),
                        bytes,
                        text: delta,
                        call_id: None,
                        tool_name: None,
                    }),
                );
                SessionControl::Continue
            }
            EngineEvent::ToolCallDraftStarted {
                stream_id,
                call_id,
                tool_name,
            } => {
                self.handle_tool_draft_started(stream_id, call_id, tool_name);
                SessionControl::Continue
            }
            EngineEvent::ToolCallDraftDelta {
                stream_id,
                call_id,
                tool_name,
                delta,
            } => {
                self.handle_tool_draft_delta(stream_id, call_id, tool_name, delta);
                SessionControl::Continue
            }
            EngineEvent::ToolCallDraftFinished {
                stream_id,
                call_id,
                tool_name,
                arguments,
            } => {
                self.handle_tool_draft_finished(stream_id, call_id, tool_name, arguments);
                SessionControl::Continue
            }
            EngineEvent::Text { content } => {
                self.flush_streaming_text();
                self.push_block(Block::Text { content });
                SessionControl::Continue
            }
            EngineEvent::ToolStarted {
                call_id,
                tool_name,
                args,
            } => {
                let summary = self.resolve_tool_summary_for_engine_event(
                    &tool_name,
                    &args,
                    protocol::StyledLines::empty(),
                );
                self.begin_tool_block_for_engine_event(pending, call_id, tool_name, summary, args);
                SessionControl::Continue
            }
            EngineEvent::ToolFinished {
                call_id,
                result,
                elapsed_ms,
            } => {
                let status = if result.is_error {
                    ToolStatus::Err
                } else {
                    ToolStatus::Ok
                };
                self.finish_tool_for_engine_event(pending, call_id, result, elapsed_ms, status);
                SessionControl::Continue
            }
            EngineEvent::ToolRejected {
                call_id,
                tool_name,
                args,
                summary,
                result,
                elapsed_ms,
            } => {
                let summary =
                    self.resolve_tool_summary_for_engine_event(&tool_name, &args, summary);
                self.begin_tool_block_for_engine_event(
                    pending,
                    call_id.clone(),
                    tool_name,
                    summary,
                    args,
                );
                let status = if result.is_error {
                    ToolStatus::Err
                } else {
                    ToolStatus::Denied
                };
                self.finish_tool_for_engine_event(pending, call_id, result, elapsed_ms, status);
                SessionControl::Continue
            }
            EngineEvent::RequestPermission {
                request_id,
                call_id,
                tool_name,
                args,
                approval_patterns,
                summary,
            } => {
                let summary =
                    self.resolve_tool_summary_for_engine_event(&tool_name, &args, summary);
                self.begin_tool_block_for_engine_event(
                    pending,
                    call_id.clone(),
                    tool_name.clone(),
                    summary.clone(),
                    args.clone(),
                );
                SessionControl::NeedsConfirm(Box::new(ConfirmRequest {
                    call_id,
                    tool_name,
                    args,
                    approval_candidates: approval_patterns,
                    grant_options: Vec::new(),
                    summary,
                    request_id,
                }))
            }
            EngineEvent::Retrying { delay_ms, attempt } => {
                // The retry restarts the turn from the last committed
                // message - any partial streaming text/thinking captured
                // before the failure is obsolete and must not bleed into
                // the next attempt's stream.
                self.flush_streaming_thinking();
                self.flush_streaming_text();
                self.clear_tool_drafts();
                self.working.begin(TurnPhase::Retrying {
                    delay: Duration::from_millis(delay_ms),
                    attempt,
                });
                SessionControl::Continue
            }
            EngineEvent::RequestAuditError { message } => {
                self.notify_warn(message);
                SessionControl::Continue
            }
            EngineEvent::ProcessCompleted { id, exit_code } => {
                self.handle_process_completed(id, exit_code);
                SessionControl::Continue
            }
            EngineEvent::EngineAskDelta { id, delta } => {
                self.lua.fire_ask_delta_callback(id, &delta);
                SessionControl::Continue
            }
            EngineEvent::EngineAskResponse { id, message, error } => {
                self.lua.fire_ask_callback(id, message.as_ref(), error);
                SessionControl::Continue
            }
            EngineEvent::HistoryAppended {
                turn_id: id,
                first_index,
                items,
            } => {
                if id == turn_id {
                    self.append_engine_history_items(first_index, items);
                    self.save_session();
                }
                SessionControl::Continue
            }
            EngineEvent::HistoryUpdated {
                turn_id: id,
                first_changed_index,
                history,
            } => {
                if id == turn_id {
                    self.set_history_from(history, Some(first_changed_index));
                    self.save_session();
                }
                SessionControl::Continue
            }
            EngineEvent::TurnComplete {
                turn_id: id,
                first_changed_index,
                history,
                meta,
            } => {
                if id != turn_id {
                    return SessionControl::Continue;
                }
                if let Some(history) = history {
                    self.set_history_from(history, Some(first_changed_index));
                }
                let payload = meta.clone().unwrap_or(protocol::TurnMeta {
                    elapsed_ms: 0,
                    avg_tps: None,
                    display_tps: None,
                    interrupted: false,
                    tool_elapsed: std::collections::HashMap::new(),
                });
                self.core
                    .signals
                    .emit_dyn("turn_complete", std::rc::Rc::new(payload));
                self.pending_turn_meta = meta;
                SessionControl::Done
            }
            EngineEvent::TurnError {
                message,
                kind,
                retry_at_ms,
            } => {
                {
                    self.working.finish(TurnOutcome::Interrupted);
                };
                self.core.signals.emit_dyn(
                    "turn_error",
                    std::rc::Rc::new(smelt_core::signals::TurnError {
                        message: message.clone(),
                    }),
                );
                self.notify_error_sticky(message);
                SessionControl::Error { kind, retry_at_ms }
            }
            EngineEvent::Shutdown { .. } => SessionControl::Error {
                kind: None,
                retry_at_ms: None,
            },
            EngineEvent::ToolDispatch {
                request_id,
                call_id,
                tool_name,
                args,
            } => {
                // Plugins open their own confirm dialogs via `smelt.dialog.open` inside `execute`.
                self.handle_tool_call(request_id, call_id, tool_name, args);
                SessionControl::Continue
            }
            EngineEvent::ToolEvaluationRequest {
                request_id,
                call_id: _,
                tool_name,
                args,
                mode: _,
            } => {
                let _guard = crate::lua::install_app_ptr(self);
                let metadata = self.lua.evaluate_tool_metadata(&tool_name, &args);
                drop(_guard);
                let decision = if let Some(err) = metadata.preflight_error.clone() {
                    protocol::Decision::Error(err)
                } else {
                    let permissions = self.active_permissions();
                    let active_mode = self.core.config.mode.clone();
                    let outcome = permissions.evaluate_tool_with_approvals(
                        active_mode,
                        smelt_core::permissions::ToolOrigin::Lua,
                        &tool_name,
                        &args,
                    );
                    outcome.decision
                };
                let evaluation = protocol::ToolEvaluation { decision, metadata };
                self.core
                    .engine
                    .send(protocol::UiCommand::ToolEvaluationResponse {
                        request_id,
                        evaluation,
                    });
                SessionControl::Continue
            }
            EngineEvent::CoreToolResult {
                request_id,
                content,
                is_error,
                metadata,
            } => {
                self.lua
                    .resolve_core_tool_call(request_id, content, is_error, metadata);
                SessionControl::Continue
            }
        }
    }
    /// Handle engine events that arrive when no turn is active.
    pub(crate) fn handle_idle_engine_event(&mut self, ev: EngineEvent) {
        match ev {
            // Stale history snapshots from cancelled/completed turns would overwrite a freshly cleared history.
            EngineEvent::HistoryUpdated { .. } => {}
            EngineEvent::TurnComplete {
                history: Some(history),
                first_changed_index,
                ..
            } if !history.is_empty() => {
                // Persist final messages from a cancelled turn without rebuilding the screen.
                self.set_history_from(history, Some(first_changed_index));
                self.save_session();
            }
            EngineEvent::EngineAskDelta { id, delta } => {
                self.lua.fire_ask_delta_callback(id, &delta);
            }
            EngineEvent::EngineAskResponse { id, message, error } => {
                self.lua.fire_ask_callback(id, message.as_ref(), error);
            }
            EngineEvent::ProcessCompleted { id, exit_code } => {
                self.handle_process_completed(id, exit_code);
            }
            EngineEvent::TurnError { message, .. } => {
                self.working.finish(TurnOutcome::Interrupted);
                self.notify_error_sticky(message);
            }
            EngineEvent::RequestAuditError { message } => {
                self.notify_warn(message);
            }
            _ => {}
        }
    }
}
