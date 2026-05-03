use crate::app::{PendingTool, SessionControl, TuiApp};
use protocol::EngineEvent;
use smelt_core::transcript_model::{Block, ToolOutput, ToolStatus};
use smelt_core::working::{TurnOutcome, TurnPhase};
use smelt_core::ConfirmRequest;
use std::time::Duration;

impl TuiApp {
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
                    if let Some(tokens) = usage.prompt_tokens {
                        if tokens > 0 {
                            self.core.session.context_tokens = Some(tokens);
                        }
                    }
                    if let Some(tps) = tokens_per_sec {
                        self.working.record_tokens_per_sec(tps);
                    }
                    {
                        self.working.begin(TurnPhase::Working);
                    };
                }
                let cost = cost_usd.unwrap_or(0.0);
                self.core.session.session_cost_usd += cost;
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
                // Auxiliary requests (title, compaction, btw, predict)
                // are excluded so a `tokens_used` subscriber sees only
                // the user-visible context flow.
                // doesn't touch this cell.
                if !background {
                    self.core
                        .cells
                        .set_dyn("tokens_used", std::rc::Rc::new(usage));
                }
                SessionControl::Continue
            }
            EngineEvent::ToolOutput { call_id, chunk } => {
                self.append_active_output(&call_id, &chunk);
                SessionControl::Continue
            }
            EngineEvent::Steered { text, count } => {
                self.flush_streaming_thinking();
                self.flush_streaming_text();
                let drain_n = count.min(self.queued_messages.len());
                self.queued_messages.drain(..drain_n);
                if drain_n > 0 {
                    self.push_block(Block::User {
                        text,
                        image_labels: vec![],
                    });
                }
                SessionControl::Continue
            }
            EngineEvent::ThinkingDelta { delta } => {
                self.append_streaming_thinking(&delta);
                SessionControl::Continue
            }
            EngineEvent::Thinking { content } => {
                self.push_block(Block::Thinking { content });
                SessionControl::Continue
            }
            EngineEvent::TextDelta { delta } => {
                self.append_streaming_text(&delta);
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
                self.flush_streaming_thinking();
                self.flush_streaming_text();
                let summary = self.lua.tool_summary(&tool_name, &args);
                self.start_tool(call_id.clone(), tool_name.clone(), summary, args.clone());
                self.core.cells.set_dyn(
                    "tool_start",
                    std::rc::Rc::new(smelt_core::cells::ToolStart {
                        tool: tool_name.clone(),
                        args: args.clone(),
                    }),
                );
                self.drain_cells_pending();
                self.flush_lua_callbacks();
                pending.push(PendingTool {
                    call_id,
                    name: tool_name,
                    args,
                });
                SessionControl::Continue
            }
            EngineEvent::ToolFinished {
                call_id,
                result,
                elapsed_ms,
            } => {
                let mut finished_tool_name: Option<String> = None;
                let mut finished_is_error = false;
                if let Some(idx) = pending.iter().position(|p| p.call_id == call_id) {
                    let removed = pending.remove(idx);
                    {
                        finished_tool_name = Some(removed.name.clone());
                        finished_is_error = result.is_error;
                        let status = if result.is_error {
                            ToolStatus::Err
                        } else {
                            ToolStatus::Ok
                        };
                        let render_cache =
                            smelt_core::transcript_cache::build_tool_output_render_cache(
                                &removed.name,
                                &removed.args,
                                &result.content,
                                result.is_error,
                                result.metadata.as_ref(),
                            );
                        let output = Some(Box::new(ToolOutput {
                            content: result.content,
                            is_error: result.is_error,
                            metadata: result.metadata,
                            render_cache,
                        }));
                        let elapsed = elapsed_ms.map(Duration::from_millis);
                        self.finish_tool(&call_id, status, output, elapsed);
                    }
                }
                if let Some(tool_name) = finished_tool_name {
                    self.core.cells.set_dyn(
                        "tool_end",
                        std::rc::Rc::new(smelt_core::cells::ToolEnd {
                            tool: tool_name,
                            is_error: finished_is_error,
                            elapsed_ms,
                        }),
                    );
                    self.drain_cells_pending();
                    self.flush_lua_callbacks();
                }
                SessionControl::Continue
            }
            EngineEvent::RequestPermission {
                request_id,
                call_id,
                tool_name,
                args,
                confirm_message,
                approval_patterns,
                summary,
            } => SessionControl::NeedsConfirm(Box::new(ConfirmRequest {
                call_id,
                tool_name,
                desc: confirm_message,
                args,
                approval_patterns,
                outside_dir: None,
                summary,
                request_id,
            })),
            EngineEvent::Retrying { delay_ms, attempt } => {
                self.working.begin(TurnPhase::Retrying {
                    delay: Duration::from_millis(delay_ms),
                    attempt,
                });
                SessionControl::Continue
            }
            EngineEvent::ProcessCompleted { id, exit_code } => {
                self.handle_process_completed(id, exit_code);
                SessionControl::Continue
            }
            EngineEvent::CompactionComplete { messages } => {
                if self.pending_compact_epoch != self.compact_epoch {
                    {
                        self.working.finish(TurnOutcome::Done);
                    };
                    return SessionControl::Continue;
                }
                self.apply_compaction(messages);
                SessionControl::Continue
            }
            EngineEvent::TitleGenerated { title, slug } => {
                self.handle_title_generated(title, slug);
                SessionControl::Continue
            }
            EngineEvent::BtwResponse { content } => {
                let _ = content;
                SessionControl::Continue
            }
            EngineEvent::InputPrediction { text, generation } => {
                if generation == self.predict_generation {
                    self.handle_input_prediction(text);
                }
                SessionControl::Continue
            }
            EngineEvent::EngineAskResponse { id, content } => {
                self.lua.fire_callback(id, &content);
                SessionControl::Continue
            }
            EngineEvent::Messages {
                turn_id: id,
                messages,
            } => {
                if id == turn_id {
                    self.set_history(messages);
                }
                SessionControl::Continue
            }
            EngineEvent::TurnComplete {
                turn_id: id,
                messages,
                meta,
            } => {
                if id != turn_id {
                    return SessionControl::Continue;
                }
                self.set_history(messages);
                let payload = meta.clone().unwrap_or(protocol::TurnMeta {
                    elapsed_ms: 0,
                    avg_tps: None,
                    interrupted: false,
                    tool_elapsed: std::collections::HashMap::new(),
                });
                self.core
                    .cells
                    .set_dyn("turn_complete", std::rc::Rc::new(payload));
                self.pending_turn_meta = meta;
                SessionControl::Done
            }
            EngineEvent::TurnError { message } => {
                {
                    self.working.finish(TurnOutcome::Done);
                };
                self.core.cells.set_dyn(
                    "turn_error",
                    std::rc::Rc::new(smelt_core::cells::TurnError {
                        message: message.clone(),
                    }),
                );
                self.notify_error(message);
                SessionControl::Done
            }
            EngineEvent::Shutdown { .. } => SessionControl::Done,
            EngineEvent::ToolDispatch {
                request_id,
                call_id,
                tool_name,
                args,
            } => {
                // Plugins open their own confirm dialogs via
                // `smelt.ui.dialog.open` from inside `execute`. The
                // core no longer special-cases plugin tools here.
                self.handle_tool_call(request_id, call_id, tool_name, args);
                SessionControl::Continue
            }
            EngineEvent::ToolHooksRequest {
                request_id,
                call_id: _,
                tool_name,
                args,
                mode,
            } => {
                let _guard = crate::lua::install_app_ptr(self);
                let mut hooks = self.lua.evaluate_hooks(&tool_name, &args);
                drop(_guard);
                // Apply permission policy on the TUI side.
                if !matches!(hooks.decision, protocol::Decision::Error(_)) {
                    let decision = self.permissions.decide(mode, &tool_name, &args, false);
                    let mut decision = decision;
                    if decision == protocol::Decision::Ask {
                        let desc = hooks
                            .confirm_message
                            .clone()
                            .unwrap_or_else(|| tool_name.clone());
                        let rt = self.runtime_approvals.read().unwrap();
                        if rt.is_auto_approved(&self.permissions, mode, &tool_name, &args, &desc) {
                            decision = protocol::Decision::Allow;
                        }
                    }
                    hooks.decision = decision;
                }
                self.core
                    .engine
                    .send(protocol::UiCommand::ToolHooksResponse { request_id, hooks });
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
        // Ignore stale Messages snapshots from cancelled/completed turns.
        // These would overwrite a freshly cleared history (e.g. after /clear).
        EngineEvent::Messages { .. } => {}
        EngineEvent::TurnComplete { messages, .. }
            // Accept final messages from a just-cancelled turn so that
            // partial assistant content and tool results are persisted.
            // Don't rebuild the screen — the displayed blocks already
            // reflect what the user saw at cancel time.
            if !messages.is_empty() =>
        {
            self.set_history(messages);
            self.save_session();
        }
        EngineEvent::CompactionComplete { messages } => {
            if self.pending_compact_epoch != self.compact_epoch {
                self.working.finish(TurnOutcome::Done);
                return;
            }
            self.apply_compaction(messages);
        }
        EngineEvent::TitleGenerated { title, slug } => {
            self.handle_title_generated(title, slug);
        }
        EngineEvent::BtwResponse { content } => {
            let _ = content;
        }
        EngineEvent::InputPrediction { text, generation }
            if generation == self.predict_generation =>
        {
            self.handle_input_prediction(text);
        }
        EngineEvent::EngineAskResponse { id, content } => {
            self.lua.fire_callback(id, &content);
        }
        EngineEvent::ProcessCompleted { id, exit_code } => {
            self.handle_process_completed(id, exit_code);
        }
        EngineEvent::TurnError { message } => {
            self.working.finish(TurnOutcome::Done);
            self.notify_error(message);
        }
        _ => {}
    }
    }
}
