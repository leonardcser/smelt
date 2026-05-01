//! Headless frontend: `Core` + `HeadlessSink`. Used for
//! `smelt --headless` (one-shot CLI) and `smelt --subagent`
//! (persistent worker over a Unix socket). Constructed in
//! `src/main.rs` directly — no `Ui`, no buffers, no compositor.
//!
//! The methods on `HeadlessApp` mirror the surface today's `TuiApp`
//! offered for these two flows: `run_oneshot` drains a single turn
//! and prints summaries; `run_subagent` enters a multi-turn loop
//! receiving messages over a parent socket, forwarding LLM events
//! to stdout as JSON.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

use protocol::{Content, EngineEvent, Mode, UiCommand};

use super::core::Core;
use super::headless::{HeadlessSink, OutputFormat};

pub struct HeadlessApp {
    pub core: Core,
    pub(crate) sink: HeadlessSink,
    pub(crate) next_turn_id: u64,
}

impl HeadlessApp {
    pub fn new(core: Core, sink: HeadlessSink) -> Self {
        Self {
            core,
            sink,
            next_turn_id: 1,
        }
    }

    fn api_key(&self) -> String {
        std::env::var(&self.core.config.api_key_env).unwrap_or_default()
    }

    /// One-shot: send the user's message, drain engine events,
    /// print the final assistant text + token / cost summary, exit.
    /// Aborts cleanly on Ctrl-C (cancellation flips the parent's
    /// `Notify`, the engine receives `UiCommand::Cancel`, and the
    /// loop breaks before the summary prints).
    pub async fn run_oneshot(&mut self, message: String, cancel: Arc<tokio::sync::Notify>) {
        use std::io::Write;

        let trimmed = message.trim();

        // Shell escape: execute and print output.
        if let Some(cmd) = trimmed.strip_prefix('!') {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                let output = std::process::Command::new("sh").arg("-c").arg(cmd).output();
                match output {
                    Ok(o) => {
                        let _ = io::stdout().write_all(&o.stdout);
                        let _ = io::stderr().write_all(&o.stderr);
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            return;
        }

        // Commands require interactive mode.
        if trimmed.starts_with('/') && crate::completer::Completer::is_command(trimmed) {
            eprintln!("\"{}\" requires interactive mode", trimmed);
            std::process::exit(1);
        }

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.core.engine.send(UiCommand::StartTurn {
            turn_id,
            content: Content::text(message),
            mode: self.core.config.mode,
            model: self.core.config.model.clone(),
            reasoning_effort: self.core.config.reasoning_effort,
            history: self.core.session.messages.clone(),
            api_base: Some(self.core.config.api_base.clone()),
            api_key: Some(self.api_key()),
            session_id: self.core.session.id.clone(),
            session_dir: crate::session::dir_for(&self.core.session),
            model_config_overrides: None,
            permission_overrides: None,
            system_prompt: None,
            plugin_tools: vec![],
        });

        // In text mode, buffer assistant text and only print to stdout at the end.
        let mut final_message = String::new();
        let mut total_usage = protocol::TokenUsage::default();
        let mut last_tps: Option<f64> = None;
        let mut total_cost = 0.0_f64;
        let mut pending_tools: HashMap<String, (String, String, String)> = HashMap::new();

        // Drain events. Break on cancellation (Ctrl+C) so the summary still prints.
        let mut interrupted = false;
        loop {
            let ev = tokio::select! {
                ev = self.core.engine.recv() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
                _ = cancel.notified() => {
                    self.core.engine.send(protocol::UiCommand::Cancel);
                    interrupted = true;
                    break;
                }
            };
            match self.sink.format {
                OutputFormat::Json => {
                    // Forward every event as JSONL.
                    self.sink.emit_json(&ev);

                    // Still need to handle side-effect events.
                    match ev {
                        EngineEvent::RequestPermission { request_id, .. } => {
                            let approved = self.core.config.mode == Mode::Yolo;
                            self.core.engine.send(UiCommand::PermissionDecision {
                                request_id,
                                approved,
                                message: None,
                            });
                        }
                        EngineEvent::TurnError { .. } | EngineEvent::TurnComplete { .. } => {
                            break;
                        }
                        _ => {}
                    }
                }
                OutputFormat::Text => match ev {
                    EngineEvent::ThinkingDelta { .. } => {}
                    EngineEvent::Thinking { content } => {
                        self.sink.log_thinking(&content);
                    }
                    EngineEvent::TextDelta { delta } => {
                        final_message.push_str(&delta);
                    }
                    EngineEvent::Text { content } => {
                        // Full text block replaces any accumulated deltas.
                        final_message = content;
                    }
                    EngineEvent::ToolStarted {
                        call_id,
                        tool_name,
                        summary,
                        ..
                    } => {
                        pending_tools.insert(call_id, (tool_name, summary, String::new()));
                    }
                    EngineEvent::ToolOutput { call_id, chunk } if self.sink.verbose => {
                        if let Some((_, _, output)) = pending_tools.get_mut(&call_id) {
                            output.push_str(&chunk);
                        }
                    }
                    EngineEvent::ToolFinished {
                        call_id,
                        result,
                        elapsed_ms,
                    } => {
                        let (name, summary, output) =
                            pending_tools.remove(&call_id).unwrap_or_default();
                        let display_output = if !self.sink.verbose {
                            String::new()
                        } else if result.is_error {
                            result.content.clone()
                        } else {
                            output
                        };
                        self.sink.log_tool(
                            &name,
                            &summary,
                            &display_output,
                            result.is_error,
                            elapsed_ms,
                        );
                    }
                    EngineEvent::TokenUsage {
                        usage,
                        tokens_per_sec,
                        cost_usd,
                        ..
                    } => {
                        total_cost += cost_usd.unwrap_or(0.0);
                        total_usage.accumulate(&usage);
                        last_tps = tokens_per_sec.or(last_tps);
                    }
                    EngineEvent::Retrying { delay_ms, attempt } => {
                        self.sink.log_retry(attempt, delay_ms);
                    }
                    EngineEvent::RequestPermission { request_id, .. } => {
                        let approved = self.core.config.mode == Mode::Yolo;
                        self.core.engine.send(UiCommand::PermissionDecision {
                            request_id,
                            approved,
                            message: None,
                        });
                    }
                    EngineEvent::Messages { .. } => {}
                    EngineEvent::TurnError { message } => {
                        self.sink.log_error(&message);
                        break;
                    }
                    EngineEvent::TurnComplete { .. } => {
                        break;
                    }
                    _ => {}
                },
            }
        }

        // Print accumulated token/cost summary.
        if self.sink.format == OutputFormat::Text {
            self.sink
                .log_token_usage(&total_usage, last_tps, total_cost);
        }

        // Text mode: write the final message to stdout (only when piped).
        // `final_message` is model-generated and passes through unredacted.
        if self.sink.format == OutputFormat::Text && !final_message.is_empty() {
            use std::io::IsTerminal;
            let stdout_is_tty = std::io::stdout().is_terminal();
            let stderr_is_tty = std::io::stderr().is_terminal();

            if stdout_is_tty && stderr_is_tty {
                // Interactive: print to stderr so the answer appears in
                // chronological order after tool output, not on a separate stream.
                eprintln!();
                eprint!("{final_message}");
                if !final_message.ends_with('\n') {
                    eprintln!();
                }
            } else {
                // At least one stream is piped — stdout gets the clean answer.
                print!("{final_message}");
                if !final_message.ends_with('\n') {
                    println!();
                }
                let _ = io::stdout().flush();
            }
        }

        if interrupted {
            let _ = io::stderr().flush();
            std::process::exit(130);
        }
    }

    // ── Subagent mode ────────────────────────────────────────────────────

    fn shutdown_subagent(&self, parent_pid: u32) {
        eprintln!("[subagent] parent {parent_pid} is dead, exiting");
        engine::registry::cleanup_self(std::process::id());
    }

    /// Forward an inter-agent message: emit to stdout and inject into engine.
    fn forward_agent_message(&self, from_id: &str, from_slug: &str, message: &str) {
        self.sink.emit_json(&EngineEvent::AgentMessage {
            from_id: from_id.to_string(),
            from_slug: from_slug.to_string(),
            message: message.to_string(),
        });
        self.core.engine.send(UiCommand::AgentMessage {
            from_id: from_id.to_string(),
            from_slug: from_slug.to_string(),
            message: message.to_string(),
        });
    }

    /// Send a Btw query to the engine on behalf of a querying peer.
    fn send_btw_query(&self, question: String) {
        self.core.engine.send(UiCommand::Btw {
            question,
            history: self.core.session.messages.clone(),
            reasoning_effort: self.core.config.reasoning_effort,
        });
    }

    /// Run as a persistent subagent. Each `EngineEvent` is written to
    /// stdout as a JSON line so the parent can parse and render it.
    /// Processes the initial message, then loops: go idle → wait for
    /// messages → run next turn → repeat.
    pub async fn run_subagent(
        &mut self,
        initial_message: String,
        parent_pid: u32,
        mut socket_rx: tokio::sync::mpsc::UnboundedReceiver<engine::socket::IncomingMessage>,
    ) {
        let parent_socket = engine::registry::read_entry(parent_pid)
            .ok()
            .map(|e| std::path::PathBuf::from(&e.socket_path));
        let my_pid = std::process::id();
        let my_agent_id = engine::registry::read_entry(my_pid)
            .ok()
            .map(|e| e.agent_id)
            .unwrap_or_default();

        // Run the initial turn.
        self.run_subagent_turn(
            Content::text(initial_message),
            &mut socket_rx,
            parent_pid,
            parent_socket.as_deref(),
            &my_agent_id,
        )
        .await;

        // Persistent loop: wait for incoming messages or parent death.
        loop {
            let parent_check = tokio::time::sleep(std::time::Duration::from_secs(5));
            tokio::pin!(parent_check);

            tokio::select! {
                Some(incoming) = socket_rx.recv() => {
                    match incoming {
                        engine::socket::IncomingMessage::Message { from_id, from_slug, message } => {
                            self.forward_agent_message(&from_id, &from_slug, &message);
                            self.core.session.messages
                                .push(protocol::Message::agent(&from_id, &from_slug, &message));
                            self.run_subagent_turn(
                                Content::text(""),
                                &mut socket_rx,
                                parent_pid,
                                parent_socket.as_deref(),
                                &my_agent_id,
                            )
                            .await;
                        }
                        engine::socket::IncomingMessage::Query { from_id: _, question, reply_tx } => {
                            self.send_btw_query(question);
                            while let Some(ev) = self.core.engine.recv().await {
                                self.sink.emit_json(&ev);
                                if let EngineEvent::BtwResponse { content } = ev {
                                    let _ = reply_tx.send(content);
                                    break;
                                }
                            }
                        }
                        engine::socket::IncomingMessage::PermissionCheck {
                            from_id, tool_name, args, confirm_message,
                            approval_patterns, summary, reply_tx,
                        } => {
                            let (approved, message) = relay_permission(
                                parent_socket.as_deref(), &from_id, &tool_name,
                                &args, &confirm_message, &approval_patterns, summary.as_deref(),
                            ).await;
                            let _ = reply_tx.send(engine::socket::PermissionReply { approved, message });
                        }
                    }
                }
                _ = &mut parent_check => {
                    if !engine::registry::is_pid_alive(parent_pid) {
                        self.shutdown_subagent(parent_pid);
                        return;
                    }
                }
            }
        }
    }

    async fn run_subagent_turn(
        &mut self,
        content: Content,
        socket_rx: &mut tokio::sync::mpsc::UnboundedReceiver<engine::socket::IncomingMessage>,
        parent_pid: u32,
        parent_socket: Option<&Path>,
        my_agent_id: &str,
    ) {
        let my_pid = std::process::id();
        engine::registry::update_status(my_pid, engine::registry::AgentStatus::Working);

        // Generate title/slug for the subagent.
        let text = content.text_content();
        if self.core.session.slug.is_none() && !text.is_empty() {
            self.core.engine.send(UiCommand::GenerateTitle {
                last_user_message: text,
                assistant_tail: String::new(),
            });
        }

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.core.engine.send(UiCommand::StartTurn {
            turn_id,
            content,
            mode: self.core.config.mode,
            model: self.core.config.model.clone(),
            reasoning_effort: self.core.config.reasoning_effort,
            history: self.core.session.messages.clone(),
            api_base: Some(self.core.config.api_base.clone()),
            api_key: Some(self.api_key()),
            session_id: self.core.session.id.clone(),
            session_dir: crate::session::dir_for(&self.core.session),
            model_config_overrides: None,
            permission_overrides: None,
            system_prompt: None,
            plugin_tools: vec![],
        });

        let mut pending_query_tx: Option<tokio::sync::oneshot::Sender<String>> = None;

        loop {
            let parent_check = tokio::time::sleep(std::time::Duration::from_secs(5));
            tokio::pin!(parent_check);

            tokio::select! {
                Some(incoming) = socket_rx.recv() => {
                    match incoming {
                        engine::socket::IncomingMessage::Message { from_id, from_slug, message } => {
                            self.forward_agent_message(&from_id, &from_slug, &message);
                        }
                        engine::socket::IncomingMessage::Query { from_id: _, question, reply_tx } => {
                            self.send_btw_query(question);
                            pending_query_tx = Some(reply_tx);
                        }
                        engine::socket::IncomingMessage::PermissionCheck {
                            from_id, tool_name, args, confirm_message,
                            approval_patterns, summary, reply_tx,
                        } => {
                            let (approved, message) = relay_permission(
                                parent_socket, &from_id, &tool_name,
                                &args, &confirm_message, &approval_patterns, summary.as_deref(),
                            ).await;
                            let _ = reply_tx.send(engine::socket::PermissionReply { approved, message });
                        }
                    }
                }
                _ = &mut parent_check => {
                    if !engine::registry::is_pid_alive(parent_pid) {
                        self.shutdown_subagent(parent_pid);
                        return;
                    }
                }
                maybe_ev = self.core.engine.recv() => {
                    let Some(ev) = maybe_ev else {
                        break;
                    };

                    // Forward every event to stdout as JSON.
                    self.sink.emit_json(&ev);

                    // Handle side effects for events that need them.
                    match ev {
                        EngineEvent::RequestPermission {
                            request_id, tool_name, args, confirm_message,
                            approval_patterns, summary, ..
                        } => {
                            let (approved, message) = relay_permission(
                                parent_socket, my_agent_id, &tool_name,
                                &args, &confirm_message, &approval_patterns, summary.as_deref(),
                            ).await;
                            self.core.engine.send(UiCommand::PermissionDecision {
                                request_id, approved, message,
                            });
                        }
                        EngineEvent::Messages { messages, .. } => {
                            self.core.session.messages = messages;
                        }
                        EngineEvent::BtwResponse { content } => {
                            if let Some(tx) = pending_query_tx.take() {
                                let _ = tx.send(content);
                            }
                        }
                        EngineEvent::TitleGenerated { title, slug } => {
                            self.core.session.title = Some(title);
                            self.core.session.slug = Some(slug.clone());
                            engine::registry::update_slug(my_pid, &slug);
                        }
                        EngineEvent::TurnError { .. } => {
                            break;
                        }
                        EngineEvent::TurnComplete { messages, .. } => {
                            self.core.session.messages = messages;

                            // Auto-return last assistant message to parent.
                            if let Some(socket) = parent_socket {
                                if let Some(last_asst) = self.core.session.messages.iter().rev().find(|m| m.role == protocol::Role::Assistant) {
                                    let text = last_asst.content.as_ref().map(|c| c.text_content()).unwrap_or_default();
                                    if !text.is_empty() {
                                        let slug = self.core.session.slug.as_deref().unwrap_or("");
                                        let _ = engine::socket::send_message(socket, my_agent_id, slug, &text).await;
                                    }
                                }
                            }

                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
        engine::registry::update_status(my_pid, engine::registry::AgentStatus::Idle);
    }
}

/// Relay a permission check through the parent's socket and return
/// the parent's decision. Falls open if no parent socket is available.
async fn relay_permission(
    parent_socket: Option<&Path>,
    from_id: &str,
    tool_name: &str,
    args: &HashMap<String, serde_json::Value>,
    confirm_message: &str,
    approval_patterns: &[String],
    summary: Option<&str>,
) -> (bool, Option<String>) {
    let Some(socket) = parent_socket else {
        return (false, Some("no parent socket available".into()));
    };
    let req = engine::socket::PermissionCheckRequest {
        from_id,
        tool_name,
        args,
        confirm_message,
        approval_patterns,
        summary,
    };
    match engine::socket::send_permission_check(socket, &req).await {
        Ok(reply) => (reply.approved, reply.message),
        Err(e) => (false, Some(format!("permission relay failed: {e}"))),
    }
}
