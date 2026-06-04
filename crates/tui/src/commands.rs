use crate::app::{CommandAction, ContextWindowUpdate, EventOutcome, InputOutcome, TuiApp};
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
/// The dispatcher's call site treats `Shell` specially (spawn a child) and
/// `Slash` as a registered command invocation. `Bare` is plain text that the
/// user typed without a sigil - it is NOT dispatched as a command, even if
/// its first word matches a registered name; the caller hands it to the
/// agent as a normal user message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParsedCommand<'a> {
    /// Empty (or whitespace-only) line.
    Empty,
    /// `! <script>` shell escape. `script` has its leading whitespace trimmed.
    Shell { script: &'a str },
    /// Slash command (`/name [arg…]` or `:name [arg…]`). Requires a sigil.
    Slash { name: &'a str, arg: Option<&'a str> },
    /// Plain text with no sigil. Never dispatched as a command.
    Bare { text: &'a str },
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
    let Some(body) = line.strip_prefix(':').or_else(|| line.strip_prefix('/')) else {
        return ParsedCommand::Bare { text: line };
    };
    // `splitn` steps past one whole whitespace char (may be multi-byte, e.g.
    // U+2000 EN QUAD) instead of slicing at `idx + 1` mid-codepoint.
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

/// Dispatch a raw command line. `!` lines spawn a shell escape; `/` and `:`
/// dispatch to a Lua-registered handler. Bare text (no sigil) is never
/// dispatched - it is left for the caller to forward to the agent.
pub(crate) fn run_command(app: &mut TuiApp, line: &str) -> CommandAction {
    let _perf = smelt_perf::perf::begin("cmd:dispatch");
    let (name, arg) = match parse_command_line(line) {
        ParsedCommand::Shell { script } => {
            if app.input.skip_shell_escape() {
                return CommandAction::Continue;
            }
            return match app.start_shell_escape(script) {
                Some(handle) => CommandAction::Exec(handle),
                None => CommandAction::Continue,
            };
        }
        ParsedCommand::Slash { name, arg } => (name.to_string(), arg.map(str::to_string)),
        ParsedCommand::Empty | ParsedCommand::Bare { .. } => return CommandAction::Continue,
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

        // Shell escape - `! cmd` (skipped while pasting).
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

        let token = smelt_core::commands::registered_command_token(&normalized)?;
        let name = &token[1..];
        // Commands that opt into `queue_when_busy` get one synchronous pass so
        // handlers that build a custom-command turn can capture their evaluated
        // body and enqueue it via `smelt.engine.submit_command`.
        if self.lua.command_queues_when_busy(name) {
            return match run_command(self, &normalized) {
                CommandAction::Exec(handle) => Some(EventOutcome::Exec(handle)),
                CommandAction::Continue => Some(EventOutcome::Noop),
            };
        }
        // Commands registered with `{ while_busy = false }` are blocked mid-turn.
        if self.lua.command_blocks_while_busy(name) == Some(true) {
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
        self.bump_epoch("input_epoch");
        self.core
            .cells
            .set_dyn("input_submit", std::rc::Rc::new(format!("!{cmd}")));
        self.pump_lua();

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
    /// `record=false` skips the `recent.json` write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn apply_model(&mut self, key: &str, record: bool) {
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
        if record && self.core.config.remember.model {
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
        self.context_window_request_id = self.context_window_request_id.wrapping_add(1);
        let request_id = self.context_window_request_id;
        let api_base = self.core.config.api_base.clone();
        let api_key = self.resolve_api_key().unwrap_or_default();
        let provider_type = self.core.config.provider_type.clone();
        let model = self.core.config.model.clone();
        let update_api_base = api_base.clone();
        let clock = std::sync::Arc::clone(&self.core.clock);
        tokio::spawn(async move {
            let provider = engine::Provider::new(api_base, api_key, &provider_type, client, clock);
            let value = provider.fetch_context_window(&model).await;
            let _ = tx.send(ContextWindowUpdate {
                request_id,
                model,
                api_base: update_api_base,
                provider_type,
                value,
            });
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

    /// `record=false` skips the `recent.json` write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn set_mode(&mut self, mode: AgentMode, record: bool) {
        let old = self.core.config.mode.clone();
        self.core.config.mode = mode.clone();
        if record && self.core.config.remember.mode {
            state::set_mode(self.core.config.mode.clone());
        }
        // Publish new mode before Lua/tool snapshots for future requests.
        if old != mode {
            self.core
                .cells
                .set_dyn("agent_mode", std::rc::Rc::new(mode.as_str().to_string()));
            self.drain_cells_pending();
            self.core
                .engine
                .send(UiCommand::SetMode { mode: mode.clone() });
            // Queue a synthetic user note so the next LLM request learns about
            // the new mode without regenerating the cached prompt prefix. If a
            // turn is active, the engine applies the same note when it reaches
            // its next request boundary; otherwise we apply it locally before
            // the next turn starts.
            let note_text = self.lua.mode_note(self.core.config.mode.as_str());
            let note = protocol::mode_change_note(&note_text);
            let mode_block = self
                .lua
                .mode_block(Some(self.core.config.mode.as_str()), &note_text);
            self.queue_history_append(crate::app::PendingHistoryAppend::ModeChange {
                note,
                block: mode_block,
            });
        }
    }

    /// `record=false` skips the `recent.json` write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn set_reasoning_effort(&mut self, effort: ReasoningEffort, record: bool) {
        self.core.config.reasoning_effort = effort;
        if record && self.core.config.remember.reasoning_effort {
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

    use smelt_core::transcript_model::Block;

    fn slash<'a>(name: &'a str, arg: Option<&'a str>) -> ParsedCommand<'a> {
        ParsedCommand::Slash { name, arg }
    }

    fn mode_blocks(app: &crate::app::TuiApp) -> Vec<&str> {
        let history = app.transcript.history();
        history
            .order
            .iter()
            .filter_map(|id| match history.blocks.get(id) {
                Some(Block::Mode { text, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn mode_change_during_turn_commits_when_history_reaches_next_request() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);

        let note = protocol::mode_change_note(&app.app.lua.mode_note("apply"));
        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        assert!(mode_blocks(&app.app).is_empty());

        app.feed_one(crate::event_source::SourceEvent::Engine(
            protocol::EngineEvent::HistoryUpdated {
                turn_id: 1,
                history: vec![protocol::HistoryItem::user(protocol::Content::text(note))],
            },
        ));

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
    }

    #[test]
    fn multiple_mode_changes_during_turn_commit_only_the_last_request_note() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        let note = protocol::mode_change_note(&app.app.lua.mode_note("yolo"));
        app.app.set_mode(AgentMode::parse("yolo").unwrap(), false);

        app.feed_one(crate::event_source::SourceEvent::Engine(
            protocol::EngineEvent::HistoryUpdated {
                turn_id: 1,
                history: vec![protocol::HistoryItem::user(protocol::Content::text(note))],
            },
        ));

        assert_eq!(mode_blocks(&app.app), vec!["now in yolo mode"]);
    }

    #[test]
    fn mode_change_without_another_turn_request_commits_at_turn_end() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        app.app.discard_turn(false);

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
    }

    #[test]
    fn mode_change_before_first_user_message_does_not_push_mode_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);

        assert!(app.app.core.session.history.is_empty());
        assert!(mode_blocks(&app.app).is_empty());
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
    fn slash_with_multiline_argument_preserves_body() {
        assert_eq!(
            parse_command_line("/btw first line\nsecond line"),
            slash("btw", Some("first line\nsecond line"))
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
    fn bare_name_without_a_sigil_parses_as_bare_text() {
        // Without a leading `/` or `:`, the line is plain text headed for
        // the agent - even if its first word matches a registered command.
        assert_eq!(
            parse_command_line("foo bar"),
            ParsedCommand::Bare { text: "foo bar" }
        );
        assert_eq!(
            parse_command_line("foo"),
            ParsedCommand::Bare { text: "foo" }
        );
        assert_eq!(
            parse_command_line("  btw what's up  "),
            ParsedCommand::Bare {
                text: "btw what's up"
            }
        );
    }

    #[test]
    fn slash_with_only_trailing_whitespace_has_no_arg() {
        // `"/foo   "` is the same as `"/foo"` once trimmed.
        assert_eq!(parse_command_line("/foo   "), slash("foo", None));
    }

    #[test]
    fn shell_takes_priority_over_slash_when_both_prefixes_present() {
        // The leading char wins - `!/foo` is a shell escape for `/foo`.
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
