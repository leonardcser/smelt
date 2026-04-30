use super::*;

pub enum ExecEvent {
    Output(String),
    Done(Option<i32>),
}

/// Stable command outcome exposed through `api::cmd::run`. Lua
/// command bodies do their own side effects via `with_app`; the
/// outcome only carries a shell-escape `Exec` channel pair when
/// the line started with `!`.
pub type CommandOutcome = CommandAction;

/// Public command runner used by `crate::api::cmd::run`. Accepts raw
/// command lines (`/quit`, `:q`, `/compact foo`, `! ls`, ...).
/// Normalises a leading `:` to `/` so `/quit` and `:quit` dispatch
/// identically. `!` lines spawn a shell escape; everything else
/// dispatches by name to a Lua-registered handler (or no-ops if
/// nothing matches).
pub fn run_command(app: &mut TuiApp, line: &str) -> CommandOutcome {
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
    if !name.is_empty() && app.core.lua.has_command(name) {
        app.core.lua.run_command(name, arg);
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
    /// running a command, opening a dialog, etc.). Returns `true` if the app
    /// should quit. Centralizes the dispatch previously duplicated across the
    /// Submit path, queued-message fallback, and auto-start-from-queued loop.
    pub(super) fn apply_input_outcome(
        &mut self,
        outcome: InputOutcome,
        content: Content,
        display: &str,
    ) -> bool {
        match outcome {
            InputOutcome::StartAgent => {
                let turn = self.begin_agent_turn(display, content);
                self.agent = Some(turn);
            }
            InputOutcome::CustomCommand(cmd) => {
                let turn = self.begin_custom_command_turn(*cmd);
                self.agent = Some(turn);
            }
            InputOutcome::Exec(rx, kill) => {
                self.exec_rx = Some(rx);
                self.exec_kill = Some(kill);
            }
            InputOutcome::Continue => {}
        }
        false
    }

    /// Execute a command while the agent is running.
    /// Returns the `EventOutcome` to use, or `None` to queue as a message.
    pub(super) fn try_command_while_running(&mut self, input: &str) -> Option<EventOutcome> {
        // Not a command — will be queued as a user message.
        // Skip shell escape check for pasted content
        let is_from_paste = self.input.skip_shell_escape();
        if !input.starts_with('/')
            && (!input.starts_with('!') || is_from_paste)
            && !matches!(input, ":q" | ":qa" | ":wq" | ":wqa")
        {
            return None;
        }
        if input.starts_with('/') && !crate::completer::Completer::is_command(input) {
            return None;
        }

        // Custom commands need their own agent turn — queue them like regular
        // messages so they run after the current turn finishes.
        if input.starts_with('/') && crate::custom_commands::is_custom_command(input) {
            return None;
        }

        // Access control: a command opts out of mid-turn execution
        // by registering with `{ while_busy = false }` (e.g. /compact,
        // /fork, /resume). Shell escapes (`!cmd`) and the vim quit
        // aliases (`:q`, `:qa`, `:wq`, `:wqa`) bypass this lookup —
        // they aren't `/`-commands.
        if let Some(rest) = input.strip_prefix('/') {
            let name = rest
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches('/');
            if !name.is_empty() && self.core.lua.command_blocks_while_busy(name) == Some(true) {
                self.notify_error(format!("cannot run /{name} while agent is working"));
                return Some(EventOutcome::Noop);
            }
        }

        // Delegate to the unified handler. Lua command bodies do their
        // own side effects via `with_app` (`smelt.quit`, `smelt.session.*`,
        // ...); the only `CommandAction` left to forward up is `Exec` for
        // shell escapes (`! cmd`).
        match run_command(self, input) {
            CommandAction::Exec(rx, kill) => Some(EventOutcome::Exec(rx, kill)),
            CommandAction::Continue => Some(EventOutcome::Noop),
        }
    }

    /// Spawn a shell command asynchronously. Returns a receiver for output
    /// lines and the child process handle (for killing on Ctrl+C).
    pub(super) fn start_shell_escape(
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

    /// Mutate resolved settings in place, then persist + propagate to
    /// input/screen. Centralizes the pattern that used to be scattered across
    /// the command handlers.
    pub(super) fn update_settings<F: FnOnce(&mut state::ResolvedSettings)>(&mut self, f: F) {
        f(&mut self.core.config.settings);
        self.input.set_vim_enabled(self.core.config.settings.vim);
        self.transcript_window
            .set_vim_enabled(self.core.config.settings.vim);
        state::save_settings(&self.core.config.settings);
    }

    /// Replace all resolved settings at once (from a settings dialog result),
    /// persisting + propagating to input/screen.
    pub(crate) fn set_settings(&mut self, new: state::ResolvedSettings) {
        self.update_settings(|slot| *slot = new);
    }

    /// Set the agent mode, persist it, and notify the engine. Marks the
    /// screen dirty so the mode indicator refreshes.
    pub(crate) fn set_mode(&mut self, mode: Mode) {
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
        let plugin_tools = self.core.lua.plugin_tool_defs(self.core.config.mode);
        self.core.engine.send(UiCommand::SetMode {
            mode: self.core.config.mode,
            system_prompt: Some(system_prompt),
            plugin_tools: Some(plugin_tools),
        });
    }

    pub(crate) fn toggle_mode(&mut self) {
        let next = self
            .core
            .config
            .mode
            .cycle_within(&self.core.config.mode_cycle);
        self.set_mode(next);
    }

    pub(crate) fn cycle_reasoning(&mut self) {
        let next = self
            .core
            .config
            .reasoning_effort
            .cycle_within(&self.core.config.reasoning_cycle);
        self.set_reasoning_effort(next);
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

/// Copy text to the system clipboard using platform commands.
///
/// Reached only through `SystemSink::write` — every clipboard write
/// in the runtime flows through `app.core.clipboard.write()` so vim,
/// emacs, transcript yank, and Lua `smelt.clipboard` share one path.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let (cmd, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else if std::env::var("WAYLAND_DISPLAY").is_ok() {
        ("wl-copy", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{cmd}: {e}"))?;

    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())?;

    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} exited with {status}"))
    }
}

/// Read text from the system clipboard using platform commands.
/// Returns `None` when the platform helper fails or the clipboard is
/// empty / holds non-text data — callers should fall back to the kill
/// ring in that case.
///
/// Reached only through `SystemSink::read` — every clipboard read in
/// the runtime flows through `app.core.clipboard.read()`.
fn paste_from_clipboard() -> Option<String> {
    use std::process::{Command, Stdio};

    let (cmd, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbpaste", &[])
    } else if std::env::var("WAYLAND_DISPLAY").is_ok() {
        ("wl-paste", &["--no-newline"])
    } else {
        ("xclip", &["-selection", "clipboard", "-o"])
    };

    let output = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// `ui::Sink` impl backed by the platform subprocess helpers. Owned
/// by the TuiApp-level `ui::Clipboard` so vim yank / paste sites push
/// through the same path the prompt and transcript already use.
pub(crate) struct SystemSink;

impl ui::Sink for SystemSink {
    fn read(&mut self) -> Option<String> {
        paste_from_clipboard()
    }
    fn write(&mut self, text: &str) -> Result<(), String> {
        copy_to_clipboard(text)
    }
}
