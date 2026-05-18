use crate::app::{CommandAction, EventOutcome, InputOutcome, TuiApp};
use crate::state;
use protocol::{AgentMode, Content, ReasoningEffort, UiCommand};

pub(crate) enum ExecEvent {
    Output(String),
    Done(Option<i32>),
}

/// Live shell-escape child. Streams stdout/stderr lines and a final exit
/// status; `kill` cancels the process group on Ctrl-C.
pub(crate) struct ExecHandle {
    pub rx: tokio::sync::mpsc::UnboundedReceiver<ExecEvent>,
    pub kill: std::sync::Arc<tokio::sync::Notify>,
}

/// Parsed shape of a raw command line typed into the prompt or cmdline.
///
/// The dispatcher's call site treats `Shell` specially (spawn a child); every
/// other shape goes through the slash-command pipeline, with leading `:` or
/// `/` stripped. Bare text like `foo bar` parses as `Slash { name: "foo",
/// arg: Some("bar") }` and is filtered out downstream by `has_command`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParsedCommand<'a> {
    /// Empty (or whitespace-only) line.
    Empty,
    /// `! <script>` shell escape. `script` has its leading whitespace trimmed.
    Shell { script: &'a str },
    /// Slash command (`/name [arg…]`, `:name [arg…]`, or bare `name [arg…]`).
    Slash { name: &'a str, arg: Option<&'a str> },
}

/// Classify a raw command line without dispatching it. See [`ParsedCommand`].
pub(crate) fn parse_command_line(line: &str) -> ParsedCommand<'_> {
    let line = line.trim();
    if line.is_empty() {
        return ParsedCommand::Empty;
    }
    if let Some(rest) = line.strip_prefix('!') {
        return ParsedCommand::Shell {
            script: rest.trim_start(),
        };
    }
    // `:` and `/` are both slash-command sigils; everything else falls
    // through as a "slash command" with the literal first word as `name`.
    let body = line
        .strip_prefix(':')
        .or_else(|| line.strip_prefix('/'))
        .unwrap_or(line);
    // `idx` is the byte offset of a whitespace char; the char itself may be
    // multi-byte (e.g. U+2000 EN QUAD is 3 bytes), so split via `splitn` to
    // step past one whole char instead of `idx + 1` slicing mid-codepoint.
    match body.splitn(2, char::is_whitespace).collect::<Vec<_>>()[..] {
        [name, arg] => {
            let arg = arg.trim();
            ParsedCommand::Slash {
                name,
                arg: if arg.is_empty() { None } else { Some(arg) },
            }
        }
        _ => ParsedCommand::Slash {
            name: body,
            arg: None,
        },
    }
}

/// Dispatch a raw command line. Leading `:` normalises to `/`. `!` lines
/// spawn a shell escape; everything else dispatches to a Lua-registered handler.
pub(crate) fn run_command(app: &mut TuiApp, line: &str) -> CommandAction {
    let _perf = smelt_perf::perf::begin("cmd:dispatch");
    let parsed = parse_command_line(line);
    if let ParsedCommand::Shell { script } = parsed {
        if !app.input.skip_shell_escape() {
            return match app.start_shell_escape(script) {
                Some(handle) => CommandAction::Exec(handle),
                None => CommandAction::Continue,
            };
        }
    }
    // For non-shell input (or `!` lines being treated as plain text mid-paste),
    // dispatch through the slash-command pipeline. Mirrors the legacy behaviour
    // of `trim_start_matches('/')` on the trimmed line.
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return CommandAction::Continue;
    }
    let body = trimmed
        .strip_prefix(':')
        .or_else(|| trimmed.strip_prefix('/'))
        .unwrap_or(trimmed);
    let (name, arg) = match body.splitn(2, char::is_whitespace).collect::<Vec<_>>()[..] {
        [n, a] => {
            let a = a.trim().to_string();
            (n.to_string(), if a.is_empty() { None } else { Some(a) })
        }
        _ => (body.to_string(), None),
    };
    app.core
        .cells
        .set_dyn("cmd_pre", std::rc::Rc::new(name.clone()));
    app.drain_cells_pending();
    if !name.is_empty() && app.lua.has_command(&name) {
        app.lua.run_command(&name, arg);
    }
    app.core.cells.set_dyn("cmd_post", std::rc::Rc::new(name));
    app.drain_cells_pending();
    app.flush_lua_callbacks();
    CommandAction::Continue
}

impl TuiApp {
    /// Apply a resolved `InputOutcome` to app state. `/quit` is handled
    /// separately via `pending_quit`; this covers the start-agent, exec,
    /// and continue cases.
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
            InputOutcome::Exec(handle) => {
                self.exec = Some(handle);
            }
            InputOutcome::Continue => {}
        }
    }

    /// Attempt to execute a command mid-run. Returns the outcome, or `None`
    /// to queue the input as a regular user message.
    pub(crate) fn try_command_while_running(&mut self, input: &str) -> Option<EventOutcome> {
        let is_from_paste = self.input.skip_shell_escape();

        // Shell escape — `! cmd` (skipped while pasting).
        if input.starts_with('!') && !is_from_paste {
            return match run_command(self, input) {
                CommandAction::Exec(handle) => Some(EventOutcome::Exec(handle)),
                CommandAction::Continue => Some(EventOutcome::Noop),
            };
        }

        // `:` is a vim-style alias for `/`; non-command input is queued.
        let normalized = if let Some(rest) = input.strip_prefix(':') {
            format!("/{rest}")
        } else if input.starts_with('/') {
            input.to_string()
        } else {
            return None;
        };

        if !smelt_core::commands::is_command(&normalized) {
            return None;
        }

        let name = normalized
            .strip_prefix('/')
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");
        // Commands that opt into `queue_when_busy` are deferred until after
        // the current turn rather than running mid-turn.
        if !name.is_empty() && self.lua.command_queues_when_busy(name) {
            return None;
        }
        // Commands registered with `{ while_busy = false }` are blocked mid-turn.
        if !name.is_empty() && self.lua.command_blocks_while_busy(name) == Some(true) {
            self.notify_error(format!("cannot run /{name} while agent is working"));
            return Some(EventOutcome::Noop);
        }

        match run_command(self, &normalized) {
            CommandAction::Exec(handle) => Some(EventOutcome::Exec(handle)),
            CommandAction::Continue => Some(EventOutcome::Noop),
        }
    }

    /// Spawn a shell command. Returns a handle for streaming output and
    /// killing the process on Ctrl+C.
    pub(crate) fn start_shell_escape(&mut self, raw: &str) -> Option<ExecHandle> {
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

        Some(ExecHandle { rx, kill })
    }

    /// Switch to a model by key. No-op if the key is not found.
    /// `persist=false` skips the cross-session cache write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn apply_model(&mut self, key: &str, persist: bool) {
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
        if persist {
            state::set_selected_model(resolved.key.clone());
        }
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
        self.refresh_context_window();
    }

    /// Kick off a background fetch for the current model's context window
    /// and push the result through `context_window_tx`. No-op when the
    /// channel/client aren't wired (test harness, headless runs).
    pub(crate) fn refresh_context_window(&mut self) {
        let Some(tx) = self.context_window_tx.clone() else {
            return;
        };
        let Some(client) = self.http_client.clone() else {
            return;
        };
        let api_base = self.core.config.api_base.clone();
        let api_key = self.resolve_api_key().unwrap_or_default();
        let provider_type = self.core.config.provider_type.clone();
        let model = self.core.config.model.clone();
        let clock = std::sync::Arc::clone(&self.core.clock);
        tokio::spawn(async move {
            let provider = engine::Provider::new(api_base, api_key, &provider_type, client, clock);
            let _ = tx.send(provider.fetch_context_window(&model).await);
        });
    }

    /// Mutate resolved settings and propagate to input/screen. Live state
    /// is authoritative; persistence lives in `init.lua`.
    pub(super) fn update_settings<F: FnOnce(&mut smelt_core::config::ResolvedSettings)>(
        &mut self,
        f: F,
    ) {
        f(&mut self.core.config.settings);
        let vim = self.core.config.settings.vim;
        let prompt_win = self
            .ui
            .win_mut(crate::app::PROMPT_WIN)
            .expect("prompt window");
        self.input.set_vim_enabled(prompt_win, vim);
        self.transcript_win_mut().set_vim_enabled(vim);
    }

    /// Replace all resolved settings at once, propagating to input/screen.
    pub(crate) fn set_settings(&mut self, new: smelt_core::config::ResolvedSettings) {
        self.update_settings(|slot| *slot = new);
    }

    /// `persist=false` skips the cross-session cache write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn set_mode(&mut self, mode: AgentMode, persist: bool) {
        let old = self.core.config.mode;
        self.core.config.mode = mode;
        if persist {
            state::set_mode(self.core.config.mode);
        }
        // Publish new mode before snapshotting tools/prompt for the engine.
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

    /// `persist=false` skips the cross-session cache write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn set_reasoning_effort(&mut self, effort: ReasoningEffort, persist: bool) {
        self.core.config.reasoning_effort = effort;
        if persist {
            state::set_reasoning_effort(effort);
        }
        self.core
            .cells
            .set_dyn("reasoning", std::rc::Rc::new(effort.label().to_string()));
        self.core
            .engine
            .send(UiCommand::SetReasoningEffort { effort });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slash<'a>(name: &'a str, arg: Option<&'a str>) -> ParsedCommand<'a> {
        ParsedCommand::Slash { name, arg }
    }

    #[test]
    fn empty_or_whitespace_lines_parse_as_empty() {
        assert_eq!(parse_command_line(""), ParsedCommand::Empty);
        assert_eq!(parse_command_line("   "), ParsedCommand::Empty);
        assert_eq!(parse_command_line("\n\t  \n"), ParsedCommand::Empty);
    }

    #[test]
    fn bang_prefix_parses_as_shell_escape() {
        assert_eq!(
            parse_command_line("!echo hi"),
            ParsedCommand::Shell { script: "echo hi" }
        );
    }

    #[test]
    fn shell_escape_trims_leading_whitespace_after_bang() {
        // `! echo hi` and `!echo hi` produce the same script.
        assert_eq!(
            parse_command_line("!   echo hi"),
            ParsedCommand::Shell { script: "echo hi" }
        );
    }

    #[test]
    fn shell_escape_keeps_an_empty_script_for_bare_bang() {
        assert_eq!(parse_command_line("!"), ParsedCommand::Shell { script: "" });
    }

    #[test]
    fn slash_prefix_extracts_name_with_no_arg() {
        assert_eq!(parse_command_line("/quit"), slash("quit", None));
    }

    #[test]
    fn slash_with_argument_splits_on_first_whitespace() {
        assert_eq!(
            parse_command_line("/model claude-haiku"),
            slash("model", Some("claude-haiku"))
        );
    }

    #[test]
    fn slash_collapses_extra_whitespace_in_argument() {
        // Extra interior whitespace inside the arg is preserved; only the
        // boundary between name and arg, plus trailing whitespace, is trimmed.
        assert_eq!(
            parse_command_line("/cmd   a    b  "),
            slash("cmd", Some("a    b"))
        );
    }

    #[test]
    fn colon_is_an_alias_for_slash() {
        assert_eq!(parse_command_line(":quit"), slash("quit", None));
        assert_eq!(parse_command_line(":model x"), slash("model", Some("x")));
    }

    #[test]
    fn bare_name_without_a_sigil_still_parses_as_slash() {
        // Preserves the legacy fall-through: any non-shell input goes
        // through the slash pipeline so `has_command` decides whether to
        // dispatch it.
        assert_eq!(parse_command_line("foo bar"), slash("foo", Some("bar")));
        assert_eq!(parse_command_line("foo"), slash("foo", None));
    }

    #[test]
    fn slash_with_only_trailing_whitespace_has_no_arg() {
        // `"/foo   "` is the same as `"/foo"` once trimmed.
        assert_eq!(parse_command_line("/foo   "), slash("foo", None));
    }

    #[test]
    fn shell_takes_priority_over_slash_when_both_prefixes_present() {
        // The leading char wins — `!/foo` is a shell escape for `/foo`.
        assert_eq!(
            parse_command_line("!/foo bar"),
            ParsedCommand::Shell { script: "/foo bar" }
        );
    }

    #[test]
    fn outer_whitespace_does_not_change_the_parse() {
        assert_eq!(
            parse_command_line("   /foo bar   "),
            slash("foo", Some("bar"))
        );
        assert_eq!(
            parse_command_line("  !echo hi  "),
            ParsedCommand::Shell { script: "echo hi" }
        );
    }

    #[test]
    fn slash_name_can_contain_unicode_word_chars() {
        // `find` uses `char::is_whitespace`, so any non-ws unicode goes in `name`.
        assert_eq!(
            parse_command_line("/日本語 arg"),
            slash("日本語", Some("arg"))
        );
    }
}
