//! Headless frontend: `Core` + `HeadlessSink`. Used for
//! `smelt --headless` (one-shot CLI). Constructed in
//! `src/main.rs` directly — no `Ui`, no buffers, no compositor.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use protocol::{AgentMode, Content, EngineEvent, UiCommand};

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
                            let approved = self.core.config.mode == AgentMode::Yolo;
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
                        let approved = self.core.config.mode == AgentMode::Yolo;
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
}
