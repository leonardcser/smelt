use crate::app::{PendingTool, SessionControl, TuiApp};
use protocol::EngineEvent;
use smelt_core::content::stream_parser::ToolStart;
use smelt_core::transcript_model::{Block, ToolOutput, ToolStatus};
use smelt_core::working::{TurnOutcome, TurnPhase};
use smelt_core::ConfirmRequest;
use std::time::Duration;

struct EngineEventResult {
    control: SessionControl,
    assistant_output_started: bool,
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
            let identity = self.active_context_token_identity();
            if self
                .conversation
                .record_context_tokens(usage.clone(), identity)
            {
                self.conversation.mark_context_tokens_updated();
                if self.active_provider_supports_mid_turn_reasoning_changes() {
                    self.sync_reasoning_effort_applied();
                }
            }
        }
        self.publish_visible_token_usage(usage);
    }

    fn resolve_tool_summary_for_engine_event(
        &mut self,
        tool_name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
        summary: protocol::StyledLines,
    ) -> protocol::StyledLines {
        if !summary.is_empty() {
            return summary;
        }
        let lua = self.lua.execution();
        let summary = crate::lua::scope_app(self, || {
            lua.tool_summary_with_context(tool_name, args, true)
        });
        if summary.is_empty() && !lua.has_tool(tool_name) {
            let summary = smelt_core::mcp::args_summary(args);
            if !summary.is_empty() {
                return summary;
            }
        }
        if summary.is_empty() {
            protocol::StyledLines::from_plain(tool_name)
        } else {
            summary
        }
    }

    fn begin_tool_block_for_engine_event(
        &mut self,
        pending: &mut Vec<PendingTool>,
        start: ToolStart,
    ) {
        let ToolStart {
            invocation_id,
            call_id,
            name: tool_name,
            summary,
            args,
            called_at_ms,
            ..
        } = start;
        if pending
            .iter()
            .any(|pending| pending.invocation_id == invocation_id)
        {
            return;
        }
        self.flush_streaming_thinking();
        self.flush_streaming_text();
        if !self.promote_tool_draft(
            invocation_id,
            call_id.clone(),
            tool_name.clone(),
            summary.clone(),
            args.clone(),
            called_at_ms,
        ) {
            self.start_tool_at(
                invocation_id,
                call_id.clone(),
                tool_name.clone(),
                summary,
                args.clone(),
                called_at_ms,
            );
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
            invocation_id,
            name: tool_name,
        });
    }

    fn finish_tool_for_engine_event(
        &mut self,
        pending: &mut Vec<PendingTool>,
        invocation_id: protocol::InvocationId,
        result: protocol::ToolOutcome,
        elapsed_ms: Option<u64>,
        status: ToolStatus,
    ) {
        let mut finished_tool_name: Option<String> = None;
        let mut finished_is_error = false;
        if let Some(idx) = pending
            .iter()
            .position(|pending| pending.invocation_id == invocation_id)
        {
            let removed = pending.remove(idx);
            finished_tool_name = Some(removed.name.clone());
            finished_is_error = result.is_error;
            let output = Some(Box::new(ToolOutput {
                content: result.content,
                is_error: result.is_error,
                metadata: result.metadata,
            }));
            let elapsed = elapsed_ms.map(Duration::from_millis);
            self.finish_tool(invocation_id, status, output, elapsed);
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

    fn complete_reasoning_summary_part(&mut self, title: Option<String>, content: String) -> bool {
        let title = title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty());
        let content = content.trim().to_string();
        if title.is_none() && content.is_empty() {
            return false;
        }

        self.flush_streaming_thinking();
        let Some(previous) = self.conversation.promote_last_reasoning_summary() else {
            self.push_block(Block::Thinking {
                summary_titles: title.iter().cloned().collect(),
                title,
                content,
                kind: protocol::ReasoningKind::Summary,
            });
            return true;
        };
        let id = previous.id;
        let previous_title = previous.title;
        let mut summary_titles = previous.summary_titles;
        let previous_content = previous.content;

        if let Some(title) = title.as_ref() {
            if summary_titles.last() != Some(title) {
                summary_titles.push(title.clone());
            }
        }
        let content = match (previous_content.is_empty(), content.is_empty()) {
            (_, true) => previous_content,
            (true, false) => content,
            (false, false) => format!("{previous_content}\n{content}"),
        };
        self.conversation.rewrite_block(
            id,
            Block::Thinking {
                title: title.or(previous_title),
                summary_titles,
                content,
                kind: protocol::ReasoningKind::Summary,
            },
        );
        true
    }

    /// Route an `EngineEvent` through the correct branch of the agent
    /// state machine. When a turn is active, delegates to
    /// `handle_engine_event` + `dispatch_control` and discards the turn on
    /// a stop signal; when idle, falls through to `handle_idle_engine_event`.
    /// Shared by the production main loop, the test harness, and the
    /// scenario replay binary so all three drive identical state.
    pub fn dispatch_engine_event(&mut self, ev: EngineEvent) -> bool {
        let _perf = smelt_perf::perf::begin("tui:dispatch_engine_event");
        self.dispatch_engine_event_inner(ev)
    }

    fn dispatch_engine_event_inner(&mut self, ev: EngineEvent) -> bool {
        if !self.conversation.is_active() {
            self.handle_idle_engine_event(ev);
            return true;
        }

        let end = self
            .with_dispatched_turn(move |app, turn| {
                let result = app.handle_engine_event(ev, turn.turn_id, &mut turn.pending);
                turn.assistant_output_started |= result.assistant_output_started;
                app.dispatch_control(result.control, turn)
            })
            .expect("active turn enters engine event dispatch");
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
    }

    fn handle_engine_event(
        &mut self,
        ev: EngineEvent,
        turn_id: u64,
        pending: &mut Vec<PendingTool>,
    ) -> EngineEventResult {
        let mut assistant_output_started = false;
        let control = match ev {
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
                    self.conversation.accumulate_usage(usage.clone(), cost);
                }
                crate::metrics::append(
                    self.core.sessions.state_root(),
                    &crate::metrics::MetricsEntry {
                        timestamp_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                        prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                        completion_tokens: usage.completion_tokens.unwrap_or(0),
                        model: self
                            .core
                            .config
                            .active_model()
                            .map_or_else(|| "unknown".into(), |model| model.model_name.clone()),
                        cost_usd,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_write_tokens: usage.cache_write_tokens,
                        reasoning_tokens: usage.reasoning_tokens,
                    },
                );
                SessionControl::Continue
            }
            EngineEvent::ToolOutput {
                invocation_id,
                chunk,
                ..
            } => {
                assistant_output_started = true;
                self.append_active_output(invocation_id, &chunk);
                SessionControl::Continue
            }
            EngineEvent::Steered { text, count } => {
                self.flush_streaming_thinking();
                self.flush_streaming_text();
                let drained = self.prompt.acknowledge_requests(count);
                if !drained.is_empty() {
                    let display = drained
                        .iter()
                        .map(crate::app::QueuedInput::display)
                        .collect::<Vec<_>>()
                        .join("\n");
                    let text = if display.is_empty() { text } else { display };
                    let command = drained
                        .first()
                        .is_some_and(crate::app::QueuedInput::is_command);
                    self.push_block(Block::User {
                        text,
                        image_labels: vec![],
                        command,
                    });
                    for line in drained
                        .iter()
                        .filter_map(crate::app::QueuedInput::command_line)
                    {
                        self.run_queued_command_line(line);
                    }
                }
                SessionControl::Continue
            }
            EngineEvent::ReasoningPartStarted { kind, .. } => {
                if kind == protocol::ReasoningKind::Raw {
                    self.flush_streaming_thinking();
                }
                SessionControl::Continue
            }
            EngineEvent::ReasoningPartDelta { kind, delta, .. } => {
                assistant_output_started = !delta.is_empty();
                let bytes = delta.len();
                if kind == protocol::ReasoningKind::Raw {
                    self.append_streaming_thinking(&delta);
                }
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
            EngineEvent::ReasoningPartFinished {
                kind,
                title,
                content,
                ..
            } => {
                if kind == protocol::ReasoningKind::Raw {
                    self.flush_streaming_thinking();
                } else {
                    assistant_output_started = self.complete_reasoning_summary_part(title, content);
                }
                SessionControl::Continue
            }
            EngineEvent::Reasoning {
                kind,
                title,
                content,
            } => {
                if kind == protocol::ReasoningKind::Summary {
                    assistant_output_started = self.complete_reasoning_summary_part(title, content);
                } else {
                    self.flush_streaming_thinking();
                    assistant_output_started = self.try_push_block(Block::Thinking {
                        title,
                        summary_titles: Vec::new(),
                        content,
                        kind,
                    });
                }
                SessionControl::Continue
            }
            EngineEvent::TextDelta { delta } => {
                assistant_output_started = !delta.is_empty();
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
                assistant_output_started = true;
                self.handle_tool_draft_started(stream_id, call_id, tool_name);
                SessionControl::Continue
            }
            EngineEvent::ToolCallDraftDelta {
                stream_id,
                call_id,
                tool_name,
                delta,
            } => {
                assistant_output_started = !delta.is_empty();
                self.handle_tool_draft_delta(stream_id, call_id, tool_name, delta);
                SessionControl::Continue
            }
            EngineEvent::ToolCallDraftFinished {
                stream_id,
                call_id,
                tool_name,
                arguments,
            } => {
                assistant_output_started = true;
                self.handle_tool_draft_finished(stream_id, call_id, tool_name, arguments);
                SessionControl::Continue
            }
            EngineEvent::Text { content } => {
                self.flush_streaming_text();
                assistant_output_started = self.try_push_block(Block::Text { content });
                SessionControl::Continue
            }
            EngineEvent::ToolStarted {
                invocation_id,
                call_id,
                tool_name,
                args,
                called_at_ms,
            } => {
                assistant_output_started = true;
                let summary = self.resolve_tool_summary_for_engine_event(
                    &tool_name,
                    &args,
                    protocol::StyledLines::empty(),
                );
                self.begin_tool_block_for_engine_event(
                    pending,
                    ToolStart {
                        invocation_id,
                        call_id,
                        name: tool_name,
                        summary,
                        args,
                        preview_output: None,
                        called_at_ms,
                    },
                );
                SessionControl::Continue
            }
            EngineEvent::ToolFinished {
                invocation_id,
                result,
                elapsed_ms,
                ..
            } => {
                assistant_output_started = true;
                let status = if result.is_error {
                    ToolStatus::Err
                } else {
                    ToolStatus::Ok
                };
                self.finish_tool_for_engine_event(
                    pending,
                    invocation_id,
                    result,
                    elapsed_ms,
                    status,
                );
                SessionControl::Continue
            }
            EngineEvent::ToolRejected {
                invocation_id,
                call_id,
                tool_name,
                args,
                summary,
                result,
                elapsed_ms,
                called_at_ms,
            } => {
                assistant_output_started = true;
                let summary =
                    self.resolve_tool_summary_for_engine_event(&tool_name, &args, summary);
                self.begin_tool_block_for_engine_event(
                    pending,
                    ToolStart {
                        invocation_id,
                        call_id: call_id.clone(),
                        name: tool_name,
                        summary,
                        args,
                        preview_output: None,
                        called_at_ms,
                    },
                );
                let status = if result.is_error {
                    ToolStatus::Err
                } else {
                    ToolStatus::Denied
                };
                self.finish_tool_for_engine_event(
                    pending,
                    invocation_id,
                    result,
                    elapsed_ms,
                    status,
                );
                SessionControl::Continue
            }
            EngineEvent::RequestPermission {
                request_id,
                invocation_id,
                call_id,
                tool_name,
                args,
                approval_patterns,
                called_at_ms,
                summary,
            } => {
                assistant_output_started = true;
                let summary =
                    self.resolve_tool_summary_for_engine_event(&tool_name, &args, summary);
                let lua = self.lua.execution();
                let tool_paths =
                    crate::lua::scope_app(self, || lua.tool_paths_for_workspace(&tool_name, &args));
                self.begin_tool_block_for_engine_event(
                    pending,
                    ToolStart {
                        invocation_id,
                        call_id: call_id.clone(),
                        name: tool_name.clone(),
                        summary: summary.clone(),
                        args: args.clone(),
                        preview_output: None,
                        called_at_ms,
                    },
                );
                SessionControl::NeedsConfirm(Box::new(ConfirmRequest {
                    invocation_id,
                    call_id,
                    tool_name,
                    args,
                    tool_paths,
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
                let lua = self.lua.execution();
                crate::lua::scope_app(self, || lua.fire_ask_delta_callback(id, &delta));
                SessionControl::Continue
            }
            EngineEvent::EngineAskResponse { id, message, error } => {
                let lua = self.lua.execution();
                crate::lua::scope_app(self, || lua.fire_ask_callback(id, message.as_ref(), error));
                SessionControl::Continue
            }
            EngineEvent::HistoryAppended { turn_id: id, delta } => {
                if id == turn_id {
                    assistant_output_started = delta
                        .items
                        .iter()
                        .any(|item| matches!(item, protocol::HistoryItem::Assistant(_)));
                    self.append_engine_history_items(delta.first_index.get(), delta.items);
                    self.save_session();
                }
                SessionControl::Continue
            }
            EngineEvent::HistoryUpdated {
                turn_id: id,
                update,
            } => {
                if id == turn_id {
                    assistant_output_started = update
                        .items
                        .iter()
                        .any(|item| matches!(item, protocol::HistoryItem::Assistant(_)));
                    self.set_history_from(update.first_index.get(), update.items);
                    self.save_session();
                }
                SessionControl::Continue
            }
            EngineEvent::TurnComplete {
                turn_id: id,
                history,
                meta,
            } => {
                if id != turn_id {
                    SessionControl::Continue
                } else {
                    if let Some(history) = history {
                        self.set_history_from(history.first_index.get(), history.items);
                    }
                    let payload = meta.clone().unwrap_or(protocol::TurnMeta {
                        elapsed_ms: 0,
                        avg_tps: None,
                        display_tps: None,
                        interrupted: false,
                    });
                    self.core
                        .signals
                        .emit_dyn("turn_complete", std::rc::Rc::new(payload));
                    self.conversation.set_pending_meta(meta);
                    SessionControl::Done
                }
            }
            EngineEvent::TurnError {
                message,
                kind,
                retry_at_ms,
            } => {
                {
                    self.working.finish(TurnOutcome::Errored);
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
                invocation_id,
                call_id,
                tool_name,
                args,
            } => {
                // Plugins open their own confirm dialogs via `smelt.dialog.open` inside `execute`.
                self.handle_tool_call(request_id, invocation_id, call_id, tool_name, args);
                SessionControl::Continue
            }
            EngineEvent::ToolEvaluationRequest {
                request_id,
                invocation_id: _,
                call_id: _,
                tool_name,
                args,
                mode,
            } => {
                let lua = self.lua.execution();
                let metadata =
                    crate::lua::scope_app(self, || lua.evaluate_tool_metadata(&tool_name, &args));
                let decision = if let Some(err) = metadata.preflight_error.clone() {
                    protocol::Decision::Error(err)
                } else {
                    let lua = self.lua.execution();
                    let tool_paths = crate::lua::scope_app(self, || {
                        lua.tool_paths_for_workspace(&tool_name, &args)
                    });
                    let permissions = self.active_permissions();
                    let outcome = permissions.evaluate_tool_with_paths_and_approvals(
                        mode,
                        smelt_core::permissions::ToolOrigin::Lua,
                        &tool_name,
                        &args,
                        &tool_paths,
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
                let lua = self.lua.execution();
                crate::lua::scope_app(self, move || {
                    lua.resolve_core_tool_call(request_id, content, is_error, metadata)
                });
                SessionControl::Continue
            }
        };
        EngineEventResult {
            control,
            assistant_output_started,
        }
    }
    /// Handle engine events that arrive when no turn is active.
    pub(crate) fn handle_idle_engine_event(&mut self, ev: EngineEvent) {
        match ev {
            // Stale history snapshots from cancelled/completed turns would overwrite a freshly cleared history.
            EngineEvent::HistoryUpdated { .. } => {}
            EngineEvent::TurnComplete {
                history: Some(history),
                ..
            } if !history.items.is_empty() => {
                // Persist final messages from a cancelled turn without rebuilding the screen.
                self.set_history_from(history.first_index.get(), history.items);
                self.save_session();
            }
            EngineEvent::EngineAskDelta { id, delta } => {
                let lua = self.lua.execution();
                crate::lua::scope_app(self, || lua.fire_ask_delta_callback(id, &delta));
            }
            EngineEvent::EngineAskResponse { id, message, error } => {
                let lua = self.lua.execution();
                crate::lua::scope_app(self, || lua.fire_ask_callback(id, message.as_ref(), error));
            }
            EngineEvent::ProcessCompleted { id, exit_code } => {
                self.handle_process_completed(id, exit_code);
            }
            EngineEvent::TurnError { message, .. } => {
                self.working.finish(TurnOutcome::Errored);
                self.notify_error_sticky(message);
            }
            EngineEvent::RequestAuditError { message } => {
                self.notify_warn(message);
            }
            _ => {}
        }
    }
}
