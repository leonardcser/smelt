use crate::app::{CommandAction, EventOutcome, InputOutcome, TuiApp};
use crate::state;
use protocol::{AgentMode, Content, ReasoningEffort, UiCommand};

pub(crate) enum ExecEvent {
    Output(String),
    Done(Option<i32>),
}

/// Public command runner used by `crate::api::cmd::run`. Accepts raw
/// command lines (`/quit`, `:q`, `/compact foo`, `! ls`, ...).
/// Normalises a leading `:` to `/` so `/quit` and `:quit` dispatch
/// identically. `!` lines spawn a shell escape; everything else
/// dispatches by name to a Lua-registered handler (or no-ops if
/// nothing matches).
pub(crate) fn run_command(app: &mut TuiApp, line: &str) -> CommandAction {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('!') {
        if !app.input.skip_shell_escape() {
            return match app.start_shell_escape(rest) {
                Some((rx, kill)) => CommandAction::Exec(rx, kill),
                None => CommandAction::Continue,
            };
        }
    }
    let normalized: String = if let Some(rest) = line.strip_prefix(':') {
        format!("/{rest}")
    } else {
        line.to_string()
    };
    let name_arg = normalized.trim_start_matches('/');
    let (name, arg) = match name_arg.find(char::is_whitespace) {
        Some(idx) => (
            &name_arg[..idx],
            Some(name_arg[idx + 1..].trim().to_string()),
        ),
        None => (name_arg, None),
    };
    app.core
        .cells
        .set_dyn("cmd_pre", std::rc::Rc::new(name.to_string()));
    app.drain_cells_pending();
    if !name.is_empty() && app.lua.has_command(name) {
        app.lua.run_command(name, arg);
    }
    app.core
        .cells
        .set_dyn("cmd_post", std::rc::Rc::new(name.to_string()));
    app.drain_cells_pending();
    app.flush_lua_callbacks();
    CommandAction::Continue
}

impl TuiApp {
    /// Apply the result of `process_input` to app state (starting an agent,
    /// running a command, kicking off a shell escape). Centralizes the
    /// dispatch previously duplicated across the Submit path, queued-message
    /// fallback, and auto-start-from-queued loop. Quit is not an outcome
    /// here — `/quit` and friends set `pending_quit` directly via the Lua
    /// `smelt.quit()` body, and the main loop honors it on the next tick.
    pub(crate) fn apply_input_outcome(
        &mut self,
        outcome: InputOutcome,
        content: Content,
        display: &str,
    ) {
        match outcome {
            InputOutcome::StartAgent => {
                let turn = self.begin_agent_turn(display, content);
                self.agent = Some(turn);
            }
            InputOutcome::Exec(rx, kill) => {
                self.exec_rx = Some(rx);
                self.exec_kill = Some(kill);
            }
            InputOutcome::Continue => {}
        }
    }

    /// Execute a command while the agent is running.
    /// Returns the `EventOutcome` to use, or `None` to queue as a message.
    pub(crate) fn try_command_while_running(&mut self, input: &str) -> Option<EventOutcome> {
        let is_from_paste = self.input.skip_shell_escape();

        // Shell escape — `! cmd` (skipped while pasting). Dispatched
        // unconditionally; `run_command` spawns the subprocess.
        if input.starts_with('!') && !is_from_paste {
            return match run_command(self, input) {
                CommandAction::Exec(rx, kill) => Some(EventOutcome::Exec(rx, kill)),
                CommandAction::Continue => Some(EventOutcome::Noop),
            };
        }

        // Slash- or colon-prefixed command. `:` is a vim-style alias for
        // `/`; both dispatch through the same Lua registry. Anything else
        // is queued as a regular user message.
        let normalized = if let Some(rest) = input.strip_prefix(':') {
            format!("/{rest}")
        } else if input.starts_with('/') {
            input.to_string()
        } else {
            return None;
        };

        if !crate::completer::Completer::is_command(&normalized) {
            return None;
        }

        let name = normalized
            .strip_prefix('/')
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");
        // Commands that spawn their own agent turn (user-defined custom
        // commands) opt into `queue_when_busy` so the dispatcher defers
        // them to after the current turn instead of running mid-turn or
        // erroring.
        if !name.is_empty() && self.lua.command_queues_when_busy(name) {
            return None;
        }
        // Access control: a command opts out of mid-turn execution
        // by registering with `{ while_busy = false }` (e.g. /compact,
        // /fork, /resume).
        if !name.is_empty() && self.lua.command_blocks_while_busy(name) == Some(true) {
            self.notify_error(format!("cannot run /{name} while agent is working"));
            return Some(EventOutcome::Noop);
        }

        match run_command(self, &normalized) {
            CommandAction::Exec(rx, kill) => Some(EventOutcome::Exec(rx, kill)),
            CommandAction::Continue => Some(EventOutcome::Noop),
        }
    }

    /// Spawn a shell command asynchronously. Returns a receiver for output
    /// lines and the child process handle (for killing on Ctrl+C).
    pub(crate) fn start_shell_escape(
        &mut self,
        raw: &str,
    ) -> Option<(
        tokio::sync::mpsc::UnboundedReceiver<ExecEvent>,
        std::sync::Arc<tokio::sync::Notify>,
    )> {
        let cmd = raw.trim();
        if cmd.is_empty() {
            return None;
        }
        self.start_exec(cmd.to_string());

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let kill = std::sync::Arc::new(tokio::sync::Notify::new());
        let kill2 = kill.clone();
        let cmd = cmd.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(ExecEvent::Output(format!("error: {e}")));
                    let _ = tx.send(ExecEvent::Done(None));
                    return;
                }
            };

            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let mut stdout_lines = tokio::io::BufReader::new(stdout).lines();
            let mut stderr_lines = tokio::io::BufReader::new(stderr).lines();
            let mut stdout_done = false;
            let mut stderr_done = false;

            loop {
                tokio::select! {
                    biased;
                    _ = kill2.notified() => {
                        let _ = child.kill().await;
                        let _ = tx.send(ExecEvent::Done(Some(130)));
                        return;
                    }
                    line = stdout_lines.next_line(), if !stdout_done => {
                        match line {
                            Ok(Some(l)) => { let _ = tx.send(ExecEvent::Output(l)); }
                            _ => { stdout_done = true; }
                        }
                    }
                    line = stderr_lines.next_line(), if !stderr_done => {
                        match line {
                            Ok(Some(l)) => { let _ = tx.send(ExecEvent::Output(l)); }
                            _ => { stderr_done = true; }
                        }
                    }
                }
                if stdout_done && stderr_done {
                    break;
                }
            }
            let status = child.wait().await.ok();
            let _ = tx.send(ExecEvent::Done(status.and_then(|s| s.code())));
        });

        Some((rx, kill))
    }

    /// Switch to a model by key, updating all relevant state. Silently does
    /// nothing if the key is not found.
    pub(crate) fn apply_model(&mut self, key: &str) {
        let Some(resolved) = self
            .core
            .config
            .available_models
            .iter()
            .find(|m| m.key == key)
            .cloned()
        else {
            return;
        };
        let old = self.core.config.model.clone();
        self.core.config.model = resolved.model_name.clone();
        self.core.config.api_base = resolved.api_base.clone();
        self.core.config.api_key_env = resolved.api_key_env.clone();
        self.core.config.provider_type = resolved.provider_type.clone();
        self.core.config.model_config = (&resolved.config).into();
        let api_key = self.resolve_api_key().unwrap_or_default();
        state::set_selected_model(resolved.key.clone());
        self.core.engine.send(UiCommand::SetModel {
            model: self.core.config.model.clone(),
            api_base: self.core.config.api_base.clone(),
            api_key,
            provider_type: self.core.config.provider_type.clone(),
        });
        if old != self.core.config.model {
            self.core
                .cells
                .set_dyn("model", std::rc::Rc::new(self.core.config.model.clone()));
        }
    }

    /// Mutate resolved settings in place and propagate to input/screen.
    /// Settings are not persisted: the runtime live-state is the source
    /// of truth and config lives in `init.lua`.
    pub(super) fn update_settings<F: FnOnce(&mut smelt_core::config::ResolvedSettings)>(
        &mut self,
        f: F,
    ) {
        f(&mut self.core.config.settings);
        self.input.set_vim_enabled(self.core.config.settings.vim);
        self.transcript_window
            .set_vim_enabled(self.core.config.settings.vim);
    }

    /// Replace all resolved settings at once (from a settings dialog result),
    /// propagating to input/screen.
    pub(crate) fn set_settings(&mut self, new: smelt_core::config::ResolvedSettings) {
        self.update_settings(|slot| *slot = new);
    }

    /// Set the agent mode, persist it, and notify the engine. Marks the
    /// screen dirty so the mode indicator refreshes.
    pub(crate) fn set_mode(&mut self, mode: AgentMode) {
        let old = self.core.config.mode;
        self.core.config.mode = mode;
        state::set_mode(self.core.config.mode);
        // Publish the new mode first so plugins can (un)register tools
        // and prompt sections for the new mode before we snapshot them
        // for the engine.
        if old != mode {
            self.core
                .cells
                .set_dyn("agent_mode", std::rc::Rc::new(mode.as_str().to_string()));
            self.drain_cells_pending();
        }
        let system_prompt = self.rebuild_system_prompt();
        let tools = self.lua.tool_defs(self.core.config.mode);
        self.core.engine.send(UiCommand::SetAgentMode {
            mode: self.core.config.mode,
            system_prompt: Some(system_prompt),
            tools: Some(tools),
        });
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: ReasoningEffort) {
        self.core.config.reasoning_effort = effort;
        state::set_reasoning_effort(effort);
        self.core
            .cells
            .set_dyn("reasoning", std::rc::Rc::new(effort.label().to_string()));
        self.core
            .engine
            .send(UiCommand::SetReasoningEffort { effort });
    }
}
