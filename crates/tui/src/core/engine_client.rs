//! `EngineClient` — owns the `EngineHandle` and is the single
//! Rust-side surface app uses to talk to the engine.
//!
//! Two responsibilities:
//!
//! 1. The `EngineClient` struct delegates `send` / `recv` /
//!    `try_recv` / `processes` onto the
//!    underlying `EngineHandle`.
//! 2. Free functions `handle_event` and `handle_idle_event`
//!    translate `EngineEvent`s into TuiApp mutations. The
//!    full target shape (Buffer::append + on_block fan-out
//!    via Buffer::attach) is gated on P1.a tail; today these
//!    drive today's transcript model directly.

use crate::core::transcript_model::{Block, ToolOutput, ToolStatus};
use crate::core::working::{TurnOutcome, TurnPhase};
use crate::core::{ConfirmRequest, TuiApp};
use crate::core::{PendingTool, SessionControl};
use engine::EngineHandle;
use protocol::{EngineEvent, UiCommand};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) struct EngineClient {
    handle: EngineHandle,
    confirms_clear: Arc<AtomicBool>,
}

impl EngineClient {
    pub(crate) fn new(handle: EngineHandle, confirms_clear: Arc<AtomicBool>) -> Self {
        Self {
            handle,
            confirms_clear,
        }
    }

    pub(crate) fn send(&self, cmd: UiCommand) {
        self.handle.send(cmd);
    }

    /// Returns `pending()` when a confirm dialog is open so the
    /// `select!` branch never resolves — the engine pauses until
    /// `Confirms::is_clear()` is true again.
    pub(crate) async fn recv(&mut self) -> Option<EngineEvent> {
        if !self.confirms_clear.load(Ordering::Relaxed) {
            std::future::pending().await
        } else {
            self.handle.recv().await
        }
    }

    /// Returns `Err(Empty)` when a confirm dialog is open so the
    /// drain loop breaks immediately.
    pub(crate) fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        if !self.confirms_clear.load(Ordering::Relaxed) {
            Err(mpsc::error::TryRecvError::Empty)
        } else {
            self.handle.try_recv()
        }
    }

    /// Cloneable injector for cross-thread tasks that need to push
    /// events into the engine's event stream (e.g. streaming bash
    /// emitting `EngineEvent::ToolOutput` per line).
    pub(crate) fn injector(&self) -> engine::EventInjector {
        self.handle.injector()
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
            // the user-visible context flow.
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
        } => {
            app.flush_streaming_thinking();
            app.flush_streaming_text();
            let summary = app.core.lua.tool_summary(&tool_name, &args);
            app.start_tool(call_id.clone(), tool_name.clone(), summary, args.clone());
            app.core.cells.set_dyn(
                "tool_start",
                std::rc::Rc::new(crate::core::cells::ToolStart {
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
                {
                    finished_tool_name = Some(removed.name.clone());
                    finished_is_error = result.is_error;
                    let status = if result.is_error {
                        ToolStatus::Err
                    } else {
                        ToolStatus::Ok
                    };
                    let render_cache =
                        crate::core::transcript_cache::build_tool_output_render_cache(
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
                    std::rc::Rc::new(crate::core::cells::ToolEnd {
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
                std::rc::Rc::new(crate::core::cells::TurnError {
                    message: message.clone(),
                }),
            );
            app.notify_error(message);
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
            app.handle_tool_call(request_id, call_id, tool_name, args);
            SessionControl::Continue
        }
        EngineEvent::ToolHooksRequest {
            request_id,
            call_id: _,
            tool_name,
            args,
            mode,
        } => {
            let _guard = crate::lua::install_app_ptr(app);
            let mut hooks = app.core.lua.evaluate_hooks(&tool_name, &args);
            drop(_guard);
            // Apply permission policy on the TUI side.
            if !matches!(hooks.decision, protocol::Decision::Error(_)) {
                let decision = app.permissions.decide(mode, &tool_name, &args, false);
                let mut decision = decision;
                if decision == protocol::Decision::Ask {
                    let desc = hooks
                        .confirm_message
                        .clone()
                        .unwrap_or_else(|| tool_name.clone());
                    let rt = app.runtime_approvals.read().unwrap();
                    if rt.is_auto_approved(&app.permissions, mode, &tool_name, &args, &desc) {
                        decision = protocol::Decision::Allow;
                    }
                }
                hooks.decision = decision;
            }
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
/// Handle engine events that arrive when no turn is active.
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
        _ => {}
    }
}
