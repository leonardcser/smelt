//! `EngineBridge` — owns the `EngineHandle` and is the single
//! Rust-side surface tui uses to talk to the engine.
//!
//! Two responsibilities:
//!
//! 1. The `EngineBridge` struct delegates `send` / `recv` /
//!    `try_recv` / `processes` / `drain_spawned` onto the
//!    underlying `EngineHandle`.
//! 2. Free functions `handle_event` and `handle_idle_event`
//!    translate `EngineEvent`s into TuiApp mutations. The
//!    full target shape (Buffer::append + on_block fan-out
//!    via Buffer::attach) is gated on P1.a tail; today these
//!    drive today's transcript model directly.

use crate::app::transcript_model::{Block, ToolOutput, ToolStatus};
use crate::app::working::{TurnOutcome, TurnPhase};
use crate::app::{ConfirmRequest, PendingTool, SessionControl, TuiApp};
use engine::{tools, EngineHandle};
use protocol::{EngineEvent, UiCommand};
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) struct EngineBridge {
    handle: EngineHandle,
}

impl EngineBridge {
    pub(crate) fn new(handle: EngineHandle) -> Self {
        Self { handle }
    }

    pub(crate) fn send(&self, cmd: UiCommand) {
        self.handle.send(cmd);
    }

    pub(crate) async fn recv(&mut self) -> Option<EngineEvent> {
        self.handle.recv().await
    }

    pub(crate) fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        self.handle.try_recv()
    }

    pub(crate) fn drain_spawned(&mut self) -> Vec<tools::SpawnedChild> {
        self.handle.drain_spawned()
    }

    pub(crate) fn processes(&self) -> &tools::ProcessRegistry {
        &self.handle.processes
    }
}

pub(crate) fn handle_event(
    app: &mut TuiApp,
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
                        app.core.session.context_tokens = Some(tokens);
                    }
                }
                if let Some(tps) = tokens_per_sec {
                    app.working.record_tokens_per_sec(tps);
                }
                {
                    app.working.begin(TurnPhase::Working);
                };
            }
            let cost = cost_usd.unwrap_or(0.0);
            app.core.session.session_cost_usd += cost;
            crate::metrics::append(&crate::metrics::MetricsEntry {
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                completion_tokens: usage.completion_tokens.unwrap_or(0),
                model: app.core.config.model.clone(),
                cost_usd,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            });
            // Auxiliary requests (title, compaction, btw, predict)
            // are excluded so a `tokens_used` subscriber sees only
            // the user-visible context flow. Sub-agent token usage
            // routes through `agent.context_tokens` separately and
            // doesn't touch this cell.
            if !background {
                app.core
                    .cells
                    .set_dyn("tokens_used", std::rc::Rc::new(usage));
            }
            SessionControl::Continue
        }
        EngineEvent::ToolOutput { call_id, chunk } => {
            app.append_active_output(&call_id, &chunk);
            SessionControl::Continue
        }
        EngineEvent::Steered { text, count } => {
            app.flush_streaming_thinking();
            app.flush_streaming_text();
            let drain_n = count.min(app.queued_messages.len());
            app.queued_messages.drain(..drain_n);
            if drain_n > 0 {
                app.push_block(Block::User {
                    text,
                    image_labels: vec![],
                });
            }
            SessionControl::Continue
        }
        EngineEvent::ThinkingDelta { delta } => {
            app.append_streaming_thinking(&delta);
            SessionControl::Continue
        }
        EngineEvent::Thinking { content } => {
            app.push_block(Block::Thinking { content });
            SessionControl::Continue
        }
        EngineEvent::TextDelta { delta } => {
            app.append_streaming_text(&delta);
            SessionControl::Continue
        }
        EngineEvent::Text { content } => {
            app.flush_streaming_text();
            app.push_block(Block::Text { content });
            SessionControl::Continue
        }
        EngineEvent::ToolStarted {
            call_id,
            tool_name,
            args,
            summary,
        } => {
            app.flush_streaming_thinking();
            app.flush_streaming_text();
            if tool_name != "spawn_agent" {
                app.start_tool(
                    call_id.clone(),
                    tool_name.clone(),
                    summary.clone(),
                    args.clone(),
                );
            }
            app.core.cells.set_dyn(
                "tool_start",
                std::rc::Rc::new(crate::app::cells::ToolStart {
                    tool: tool_name.clone(),
                    args: args.clone(),
                }),
            );
            app.drain_cells_pending();
            app.flush_lua_callbacks();
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
                if removed.name == "spawn_agent" {
                    let agent_id = result
                        .content
                        .strip_prefix("agent ")
                        .and_then(|s| s.split_whitespace().next())
                        .unwrap_or("")
                        .to_string();
                    app.finish_active_agent(&agent_id);
                    if let Some(idx) = app
                        .agents
                        .iter()
                        .position(|a| a.agent_id == agent_id && a.blocking)
                    {
                        let agent = &app.agents[idx];
                        app.pending_agent_blocks.push((
                            agent.agent_id.clone(),
                            protocol::AgentBlockData {
                                slug: agent.slug.clone(),
                                tool_calls: agent
                                    .tool_calls
                                    .iter()
                                    .map(|tc| protocol::AgentToolData {
                                        tool_name: tc.tool_name.clone(),
                                        summary: tc.summary.clone(),
                                        elapsed_ms: tc.elapsed.map(|d| d.as_millis() as u64),
                                        is_error: matches!(tc.status, ToolStatus::Err),
                                    })
                                    .collect(),
                            },
                        ));
                        let pid = agent.pid;
                        engine::registry::kill_agent(pid);
                        app.agents.remove(idx);
                        app.sync_agent_snapshots();
                    }
                } else {
                    finished_tool_name = Some(removed.name.clone());
                    finished_is_error = result.is_error;
                    let status = if result.is_error {
                        ToolStatus::Err
                    } else {
                        ToolStatus::Ok
                    };
                    let render_cache = crate::app::transcript_cache::build_tool_output_render_cache(
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
                    app.finish_tool(&call_id, status, output, elapsed);
                }
            }
            if let Some(tool_name) = finished_tool_name {
                app.core.cells.set_dyn(
                    "tool_end",
                    std::rc::Rc::new(crate::app::cells::ToolEnd {
                        tool: tool_name,
                        is_error: finished_is_error,
                        elapsed_ms,
                    }),
                );
                app.drain_cells_pending();
                app.flush_lua_callbacks();
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
            app.working.begin(TurnPhase::Retrying {
                delay: Duration::from_millis(delay_ms),
                attempt,
            });
            SessionControl::Continue
        }
        EngineEvent::ProcessCompleted { id, exit_code } => {
            app.handle_process_completed(id, exit_code);
            SessionControl::Continue
        }
        EngineEvent::CompactionComplete { messages } => {
            if app.pending_compact_epoch != app.compact_epoch {
                {
                    app.working.finish(TurnOutcome::Done);
                };
                return SessionControl::Continue;
            }
            app.apply_compaction(messages);
            SessionControl::Continue
        }
        EngineEvent::TitleGenerated { title, slug } => {
            app.handle_title_generated(title, slug);
            SessionControl::Continue
        }
        EngineEvent::BtwResponse { content } => {
            let _ = content;
            SessionControl::Continue
        }
        EngineEvent::InputPrediction { text, generation } => {
            if generation == app.predict_generation {
                app.handle_input_prediction(text);
            }
            SessionControl::Continue
        }
        EngineEvent::EngineAskResponse { id, content } => {
            app.core.lua.fire_callback(id, &content);
            SessionControl::Continue
        }
        EngineEvent::Messages {
            turn_id: id,
            messages,
        } => {
            if id == turn_id {
                app.set_history(messages);
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
            app.set_history(messages);
            let payload = meta.clone().unwrap_or(protocol::TurnMeta {
                elapsed_ms: 0,
                avg_tps: None,
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
                agent_blocks: std::collections::HashMap::new(),
            });
            app.core
                .cells
                .set_dyn("turn_complete", std::rc::Rc::new(payload));
            app.pending_turn_meta = meta;
            SessionControl::Done
        }
        EngineEvent::TurnError { message } => {
            {
                app.working.finish(TurnOutcome::Done);
            };
            app.core.cells.set_dyn(
                "turn_error",
                std::rc::Rc::new(crate::app::cells::TurnError {
                    message: message.clone(),
                }),
            );
            app.notify_error(message);
            SessionControl::Done
        }
        EngineEvent::Shutdown { .. } => SessionControl::Done,
        EngineEvent::AgentExited {
            agent_id,
            exit_code,
        } => {
            app.handle_agent_exited(&agent_id, exit_code);
            SessionControl::Continue
        }
        EngineEvent::AgentMessage {
            from_id,
            from_slug,
            message,
        } => {
            // Suppress AgentMessage rendering for blocking agents — their
            // result flows through the spawn_agent tool output instead.
            let is_blocking = app
                .agents
                .iter()
                .any(|a| a.agent_id == from_id && a.blocking);
            if !is_blocking {
                app.push_block(Block::AgentMessage {
                    from_id: from_id.clone(),
                    from_slug: from_slug.clone(),
                    content: message.clone(),
                });
            }
            // Forward to engine so it enters the conversation history
            // (deferred until current tool batch completes).
            app.core.engine.send(protocol::UiCommand::AgentMessage {
                from_id,
                from_slug,
                message,
            });
            SessionControl::Continue
        }
        EngineEvent::ToolDispatch {
            request_id,
            call_id,
            tool_name,
            args,
        } => {
            // Plugins open their own confirm dialogs via
            // `smelt.ui.dialog.open` from inside `execute`. The
            // core no longer special-cases plugin tools here.
            app.handle_plugin_tool(request_id, call_id, tool_name, args);
            SessionControl::Continue
        }
        EngineEvent::ToolHooksRequest {
            request_id,
            tool_name,
            args,
            ..
        } => {
            let _guard = crate::lua::install_app_ptr(app);
            let hooks = app.core.lua.evaluate_plugin_hooks(&tool_name, &args);
            drop(_guard);
            app.core
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
            app.core
                .lua
                .resolve_core_tool_call(request_id, content, is_error, metadata);
            SessionControl::Continue
        }
    }
}
/// Handle engine events that arrive when no agent turn is active.
pub(crate) fn handle_idle_event(app: &mut TuiApp, ev: EngineEvent) {
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
            app.set_history(messages);
            app.save_session();
        }
        EngineEvent::CompactionComplete { messages } => {
            if app.pending_compact_epoch != app.compact_epoch {
                app.working.finish(TurnOutcome::Done);
                return;
            }
            app.apply_compaction(messages);
        }
        EngineEvent::TitleGenerated { title, slug } => {
            app.handle_title_generated(title, slug);
        }
        EngineEvent::BtwResponse { content } => {
            let _ = content;
        }
        EngineEvent::InputPrediction { text, generation }
            if generation == app.predict_generation =>
        {
            app.handle_input_prediction(text);
        }
        EngineEvent::EngineAskResponse { id, content } => {
            app.core.lua.fire_callback(id, &content);
        }
        EngineEvent::ProcessCompleted { id, exit_code } => {
            app.handle_process_completed(id, exit_code);
        }
        EngineEvent::TurnError { message } => {
            app.working.finish(TurnOutcome::Done);
            app.notify_error(message);
        }
        EngineEvent::AgentExited {
            agent_id,
            exit_code,
        } => {
            app.handle_agent_exited(&agent_id, exit_code);
        }
        EngineEvent::AgentMessage {
            from_id,
            from_slug,
            message,
        } => {
            let is_blocking = app
                .agents
                .iter()
                .any(|a| a.agent_id == from_id && a.blocking);
            if !is_blocking {
                app.push_block(Block::AgentMessage {
                    from_id: from_id.clone(),
                    from_slug: from_slug.clone(),
                    content: message.clone(),
                });
                // Queue as an Agent role message to trigger a turn without
                // rendering a duplicate User block.
                app.pending_agent_messages
                    .push(protocol::Message::agent(&from_id, &from_slug, &message));
            }
        }
        _ => {}
    }
}
