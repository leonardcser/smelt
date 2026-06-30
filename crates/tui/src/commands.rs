use crate::app::{
    CommandAction, ContextWindowUpdate, EventOutcome, InputOutcome, QueueStage, TuiApp,
};
use crate::state;
use protocol::{AgentMode, Content, ReasoningEffort, UiCommand};

mod parse;

pub(crate) use parse::{parse_command_line, ParsedCommand};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandSource {
    Prompt,
    Cmdline,
    Lua,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellSink {
    Transcript,
    Overlay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommandContext {
    pub(crate) source: CommandSource,
    pub(crate) queue_target: QueueStage,
}

impl CommandContext {
    pub(crate) fn prompt() -> Self {
        Self {
            source: CommandSource::Prompt,
            queue_target: QueueStage::Turn,
        }
    }

    pub(crate) fn cmdline() -> Self {
        Self {
            source: CommandSource::Cmdline,
            queue_target: QueueStage::Turn,
        }
    }

    pub(crate) fn lua() -> Self {
        Self {
            source: CommandSource::Lua,
            queue_target: QueueStage::Turn,
        }
    }

    pub(crate) fn with_queue_target(mut self, queue_target: QueueStage) -> Self {
        self.queue_target = queue_target;
        self
    }
}

pub(crate) enum ExecEvent {
    Output(String),
    Done(Option<i32>),
}

/// Live shell-escape child. Streams stdout/stderr lines and a final exit
/// status; `kill` cancels the process group on Ctrl-C.
pub(crate) struct ExecHandle {
    pub rx: tokio::sync::mpsc::UnboundedReceiver<ExecEvent>,
    pub kill: std::sync::Arc<tokio::sync::Notify>,
    pub sink: ShellSink,
}

pub(crate) enum CommandEffect {
    Continue,
    QuitApp,
    CloseFocusedContext,
    QuitFocusedContextOrApp,
    RunCommand {
        name: String,
        arg: Option<String>,
        ctx: CommandContext,
    },
    StartShell {
        script: String,
        sink: ShellSink,
    },
}

pub(crate) fn prompt_quit_alias(line: &str) -> bool {
    let ParsedCommand::Ex { name, bang: _, arg } = parse_command_line(line) else {
        return false;
    };
    arg.is_none() && matches!(name, "q" | "quit" | "qa" | "qall" | "quitall" | "wq" | "x")
}

fn parse_for_context<'a>(line: &'a str, ctx: CommandContext) -> ParsedCommand<'a> {
    if ctx.source == CommandSource::Prompt
        && line.trim_start().starts_with(':')
        && !prompt_quit_alias(line)
    {
        return ParsedCommand::Bare { text: line.trim() };
    }
    parse_command_line(line)
}

impl CommandEffect {
    fn from_parsed(parsed: ParsedCommand<'_>, ctx: CommandContext) -> Self {
        match parsed {
            ParsedCommand::Shell { script, sink } => {
                let sink = match (ctx.source, sink) {
                    (CommandSource::Prompt, ShellSink::Overlay) => ShellSink::Transcript,
                    _ => sink,
                };
                Self::StartShell {
                    script: script.to_string(),
                    sink,
                }
            }
            ParsedCommand::Slash { name, arg } => Self::RunCommand {
                name: name.to_string(),
                arg: arg.map(str::to_string),
                ctx,
            },
            ParsedCommand::Ex { name, bang, arg } => ex_command_effect(name, bang, arg, ctx),
            ParsedCommand::Empty | ParsedCommand::Bare { .. } => Self::Continue,
        }
    }
}

fn ex_command_effect(
    name: &str,
    bang: bool,
    arg: Option<&str>,
    ctx: CommandContext,
) -> CommandEffect {
    let name = name.trim();
    match name {
        "q" | "quit" => {
            if bang {
                CommandEffect::QuitApp
            } else {
                CommandEffect::QuitFocusedContextOrApp
            }
        }
        "qa" | "qall" | "quitall" => CommandEffect::QuitApp,
        "close" => CommandEffect::CloseFocusedContext,
        "wq" | "x" => CommandEffect::QuitApp,
        _ => CommandEffect::RunCommand {
            name: name.to_string(),
            arg: arg.map(str::to_string),
            ctx,
        },
    }
}

/// Dispatch a raw command line with prompt semantics.
pub(crate) fn run_command(app: &mut TuiApp, line: &str) -> CommandAction {
    run_command_with_context(app, line, CommandContext::prompt())
}

pub(crate) fn run_command_with_context(
    app: &mut TuiApp,
    line: &str,
    ctx: CommandContext,
) -> CommandAction {
    let _perf = smelt_perf::perf::begin("cmd:dispatch");
    let effect = CommandEffect::from_parsed(parse_for_context(line, ctx), ctx);
    app.apply_command_effect(effect)
}

enum CloseTarget {
    Cmdline,
    ShellPanel,
    Overlay(crate::smelt_edit::OverlayId),
}

enum ModeNotePolicy {
    Append,
    Suppress,
}

impl TuiApp {
    fn focused_close_target(&self) -> Option<CloseTarget> {
        if self.cmdline_is_focused() {
            return Some(CloseTarget::Cmdline);
        }
        let overlay = self.ui.focused_overlay()?;
        if self
            .shell_panel
            .is_some_and(|panel| panel.overlay == overlay)
        {
            return Some(CloseTarget::ShellPanel);
        }
        Some(CloseTarget::Overlay(overlay))
    }

    pub(crate) fn apply_command_effect(&mut self, effect: CommandEffect) -> CommandAction {
        match effect {
            CommandEffect::Continue => CommandAction::Continue,
            CommandEffect::QuitApp => {
                self.pending_quit = true;
                CommandAction::Continue
            }
            CommandEffect::CloseFocusedContext => {
                if !self.close_focused_context() {
                    self.notify_error("no focused window to close".into());
                }
                CommandAction::Continue
            }
            CommandEffect::QuitFocusedContextOrApp => {
                self.quit_focused_context_or_app();
                CommandAction::Continue
            }
            CommandEffect::RunCommand { name, arg, ctx } => {
                self.run_command_by_name(&name, arg.as_deref(), ctx)
            }
            CommandEffect::StartShell { script, sink } => {
                if self.input.skip_shell_escape() {
                    return CommandAction::Continue;
                }
                match self.start_shell_escape_with_sink(&script, sink) {
                    Some(handle) => CommandAction::Exec(handle),
                    None => CommandAction::Continue,
                }
            }
        }
    }

    pub(crate) fn has_command_name(&self, name: &str) -> bool {
        !name.is_empty() && self.lua.has_command(name)
    }

    pub(crate) fn run_command_by_name(
        &mut self,
        name: &str,
        arg: Option<&str>,
        ctx: CommandContext,
    ) -> CommandAction {
        let name = name.to_string();
        let arg = arg.map(str::to_string);
        let next_turn_id = self.next_turn_id;
        self.core
            .signals
            .emit_dyn("cmd_pre", std::rc::Rc::new(name.clone()));
        self.drain_signals_pending();
        if self.has_command_name(&name) {
            self.lua
                .run_command_with_queue_target(&name, arg, ctx.queue_target.into());
        } else {
            let prefix = match ctx.source {
                CommandSource::Cmdline => ':',
                _ => '/',
            };
            self.notify_error(format!("unknown command: {prefix}{name}"));
        }
        self.core
            .signals
            .emit_dyn("cmd_post", std::rc::Rc::new(name));
        self.drain_signals_pending();
        self.flush_lua_callbacks();
        if self.next_turn_id == next_turn_id {
            self.invalidate_prompt_prediction();
        }
        CommandAction::Continue
    }

    pub(crate) fn quit_focused_context_or_app(&mut self) {
        if !self.close_focused_context() {
            self.pending_quit = true;
        }
    }

    pub(crate) fn close_focused_context(&mut self) -> bool {
        match self.focused_close_target() {
            Some(CloseTarget::Cmdline) => {
                self.close_cmdline();
                true
            }
            Some(CloseTarget::ShellPanel) => self.close_shell_panel_and_stop_job(),
            Some(CloseTarget::Overlay(overlay)) => {
                self.close_overlay(overlay);
                true
            }
            None => false,
        }
    }

    /// Apply a resolved `InputOutcome` to app state. Command handlers update
    /// `pending_quit` directly; this covers start-agent, exec, and continue cases.
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

    fn run_command_with_queue_target(
        &mut self,
        input: &str,
        queue_target: QueueStage,
    ) -> CommandAction {
        run_command_with_context(
            self,
            input,
            CommandContext::prompt().with_queue_target(queue_target),
        )
    }

    /// Attempt to execute a command mid-run. Returns the outcome, or `None`
    /// to queue the input as a regular user message.
    pub(crate) fn try_command_while_running(
        &mut self,
        input: &str,
        queue_target: QueueStage,
    ) -> Option<EventOutcome> {
        let is_from_paste = self.input.skip_shell_escape();

        if input.starts_with('!') && !is_from_paste {
            return match self.run_command_with_queue_target(input, queue_target) {
                CommandAction::Exec(handle) => Some(EventOutcome::Exec(handle)),
                CommandAction::Continue => Some(EventOutcome::Noop),
            };
        }

        if prompt_quit_alias(input) {
            return Some(EventOutcome::Quit);
        }

        let parsed = parse_command_line(input);
        let (name, normalized) = match parsed {
            ParsedCommand::Slash { name, .. } if self.has_command_name(name) => {
                (name.to_string(), input.to_string())
            }
            _ => return None,
        };
        if self.lua.command_queues_when_busy(&name) {
            return match self.run_command_with_queue_target(&normalized, queue_target) {
                CommandAction::Exec(handle) => Some(EventOutcome::Exec(handle)),
                CommandAction::Continue => Some(EventOutcome::Noop),
            };
        }
        if self.lua.command_blocks_while_busy(&name) == Some(true) {
            self.notify_error(format!("cannot run /{name} while agent is working"));
            return Some(EventOutcome::Noop);
        }

        match self.run_command_with_queue_target(&normalized, queue_target) {
            CommandAction::Exec(handle) => Some(EventOutcome::Exec(handle)),
            CommandAction::Continue => Some(EventOutcome::Noop),
        }
    }

    /// Spawn a shell command. Returns a handle for streaming output and
    /// killing the process on Ctrl+C.
    pub(crate) fn start_shell_escape(&mut self, raw: &str) -> Option<ExecHandle> {
        self.start_shell_escape_with_sink(raw, ShellSink::Transcript)
    }

    pub(crate) fn start_shell_escape_with_sink(
        &mut self,
        raw: &str,
        sink: ShellSink,
    ) -> Option<ExecHandle> {
        let cmd = raw.trim();
        if cmd.is_empty() {
            return None;
        }
        if self.exec.is_some() {
            self.notify_error("a shell command is already running".into());
            return None;
        }
        match sink {
            ShellSink::Transcript => {
                self.start_exec(cmd.to_string());
                self.publish_input_submit(format!("!{cmd}"));
            }
            ShellSink::Overlay => self.open_shell_panel(cmd),
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let kill = std::sync::Arc::new(tokio::sync::Notify::new());
        let kill2 = kill.clone();
        let cmd = cmd.to_string();
        let cwd = std::path::PathBuf::from(&self.cwd);
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .current_dir(&cwd)
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

        Some(ExecHandle { rx, kill, sink })
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
        if record && self.block_read_only_mutation("change read-only session settings") {
            return;
        }
        let old = self.core.config.model.clone();
        self.core.config.model = resolved.model_name.clone();
        self.core.config.api_base = resolved.api_base.clone();
        self.core.config.api_key_env = resolved.api_key_env.clone();
        self.core.config.provider_type = resolved.provider_type.clone();
        self.core.config.model_config = (&resolved.config).into();
        if record {
            self.update_session_persist_metadata();
        }
        let api_key = self.resolve_api_key().unwrap_or_default();
        if record && self.core.config.remember.model {
            state::set_selected_model(resolved.key.clone());
        }
        self.warn_if_api_base_normalized();
        self.core.engine.send(UiCommand::SetModel {
            model: self.core.config.model.clone(),
            api_base: self.core.config.api_base.clone(),
            api_key,
            provider_type: self.core.config.provider_type.clone(),
        });
        if old != self.core.config.model {
            self.core
                .signals
                .set_dyn("model", std::rc::Rc::new(self.core.config.model.clone()));
        }
        let identity = self.active_context_token_identity();
        if record {
            self.apply_session_document_mutation(
                crate::app::session_document::SessionMutation::ClearContextTokensBaselineIfMismatched {
                    identity,
                },
            );
        } else {
            let _ = self.session_document.apply(
                &mut self.core.session,
                &mut self.parser,
                false,
                crate::app::session_document::SessionMutation::ClearContextTokensBaselineIfMismatched {
                    identity,
                },
            );
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
        let model_config = self.core.config.model_config.clone();
        let update_api_base = api_base.clone();
        let clock = std::sync::Arc::clone(&self.core.clock);
        tokio::spawn(async move {
            let provider = engine::Provider::new(api_base, api_key, &provider_type, client, clock)
                .with_model_config(model_config);
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
        let old_file_icon_options = (
            self.core.config.settings.file_icons,
            self.core.config.settings.file_icon_colors,
        );
        let old_system_clipboard = self.core.config.settings.system_clipboard;
        f(&mut self.core.config.settings);
        let file_icons = self.core.config.settings.file_icons;
        let file_icon_colors = self.core.config.settings.file_icon_colors;
        let system_clipboard = self.core.config.settings.system_clipboard;
        if system_clipboard != old_system_clipboard {
            self.core.set_system_clipboard_enabled(system_clipboard);
        }
        let vim = self.core.config.settings.vim;
        let prompt_win = self
            .ui
            .win_mut(crate::app::PROMPT_WIN)
            .expect("prompt window");
        self.input.set_vim_enabled(prompt_win, vim);
        self.transcript_win_mut().set_vim_enabled(vim);
        if old_file_icon_options != (file_icons, file_icon_colors) {
            self.sync_inline_options();
            self.sync_transcript_renderer_generation();
        }
    }

    /// Replace all resolved settings at once, propagating to input/screen.
    pub(crate) fn set_settings(&mut self, new: smelt_core::config::ResolvedSettings) {
        self.update_settings(|slot| *slot = new);
    }

    /// `record=false` skips the `recent.json` write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn set_mode(&mut self, mode: AgentMode, record: bool) {
        self.apply_mode(mode, record, ModeNotePolicy::Append);
    }

    pub(crate) fn restore_mode_after_rewind(&mut self, mode: AgentMode) {
        self.apply_mode(mode, false, ModeNotePolicy::Suppress);
    }

    fn apply_mode(&mut self, mode: AgentMode, record: bool, note_policy: ModeNotePolicy) {
        if record && self.block_read_only_mutation("change read-only session mode") {
            return;
        }
        let old = self.core.config.mode.clone();
        self.core.config.mode = mode.clone();
        if record && self.core.config.remember.mode {
            state::set_mode(self.core.config.mode.clone());
        }
        // Publish new mode before Lua/tool snapshots for future requests.
        if old != mode {
            if record {
                self.update_session_persist_metadata();
            }
            self.core
                .signals
                .set_dyn("agent_mode", std::rc::Rc::new(mode.as_str().to_string()));
            self.drain_signals_pending();
            self.core
                .engine
                .send(UiCommand::SetMode { mode: mode.clone() });
            if matches!(note_policy, ModeNotePolicy::Append) {
                // Queue an internal mode-change note so the next LLM request learns about
                // the new mode without regenerating the cached prompt prefix. If a
                // turn is active, the engine applies the same note when it reaches
                // its next request boundary; otherwise we apply it locally before
                // the next turn starts.
                let note_text = self.lua.mode_note(self.core.config.mode.as_str());
                self.queue_history_append(crate::app::PendingHistoryAppend::mode_change(
                    self.core.config.mode.as_str().to_string(),
                    note_text,
                ));
            }
        }
    }

    /// `record=false` skips the `recent.json` write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn set_reasoning_effort(&mut self, effort: ReasoningEffort, record: bool) {
        if record && self.block_read_only_mutation("change read-only session reasoning effort") {
            return;
        }
        self.core.config.reasoning_effort = effort;
        if record {
            self.update_session_persist_metadata();
        }
        if record && self.core.config.remember.reasoning_effort {
            state::set_reasoning_effort(effort);
        }
        self.core
            .signals
            .set_dyn("reasoning", std::rc::Rc::new(effort.label().to_string()));
        if self.agent.is_none() {
            self.sync_reasoning_effort_applied();
        }
        self.core
            .engine
            .send(UiCommand::SetReasoningEffort { effort });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::ProcessCwdGuard;
    use smelt_core::transcript_model::Block;

    fn slash<'a>(name: &'a str, arg: Option<&'a str>) -> ParsedCommand<'a> {
        ParsedCommand::Slash { name, arg }
    }

    fn ex<'a>(name: &'a str, bang: bool, arg: Option<&'a str>) -> ParsedCommand<'a> {
        ParsedCommand::Ex { name, bang, arg }
    }

    fn shell(script: &str, sink: ShellSink) -> ParsedCommand<'_> {
        ParsedCommand::Shell { script, sink }
    }

    fn mode_blocks(app: &crate::app::TuiApp) -> Vec<&str> {
        let history = app.session_document.transcript.history();
        history
            .order
            .iter()
            .filter_map(|id| match history.block(*id) {
                Some(Block::Mode { text, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn pending_mode_appends(app: &crate::app::TuiApp) -> Vec<&crate::app::PendingHistoryAppend> {
        app.pending_history_appends
            .iter()
            .filter(|append| {
                append.replacement_note_kind() == Some(protocol::HistoryNoteKind::ModeChange)
            })
            .collect()
    }

    fn normal_app() -> crate::app::test_harness::TestApp {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.config.mode = AgentMode::parse("normal").unwrap();
        app.app.core.session.mode = Some("normal".into());
        app
    }

    fn user(text: &str) -> protocol::HistoryItem {
        protocol::HistoryItem::user(Content::text(text))
    }

    fn assistant(text: &str) -> protocol::HistoryItem {
        protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(Content::text(text)),
            None,
            Vec::new(),
        ))
    }

    fn set_history(
        app: &mut crate::app::test_harness::TestApp,
        history: Vec<protocol::HistoryItem>,
    ) {
        app.app.core.session.history = history;
        app.app.restore_screen();
    }

    #[test]
    fn mode_change_during_turn_commits_when_history_reaches_next_request() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);

        let note = app.app.lua.mode_note("apply");
        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        assert!(mode_blocks(&app.app).is_empty());

        app.feed_one(crate::event_source::SourceEvent::Engine(
            protocol::EngineEvent::HistoryUpdated {
                turn_id: 1,
                first_changed_index: 0,
                history: vec![protocol::HistoryItem::note(
                    protocol::HistoryNote::mode_change(note),
                )],
            },
        ));

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
    }

    #[test]
    fn multiple_mode_changes_during_turn_commit_only_the_last_request_note() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        let note = app.app.lua.mode_note("yolo");
        app.app.set_mode(AgentMode::parse("yolo").unwrap(), false);

        app.feed_one(crate::event_source::SourceEvent::Engine(
            protocol::EngineEvent::HistoryUpdated {
                turn_id: 1,
                first_changed_index: 0,
                history: vec![protocol::HistoryItem::note(
                    protocol::HistoryNote::mode_change(note),
                )],
            },
        ));

        assert_eq!(mode_blocks(&app.app), vec!["now in yolo mode"]);
    }

    #[test]
    fn mode_pending_clears_when_cycling_back_to_applied_mode() {
        let mut app = normal_app();
        app.app.core.session.history = vec![user("hello")];
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        assert!(app.app.mode_pending());

        app.app.set_mode(AgentMode::parse("normal").unwrap(), false);
        assert!(!app.app.mode_pending());
    }

    #[test]
    fn reasoning_pending_clears_when_cycling_back_to_applied_effort() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.config.reasoning_effort = protocol::ReasoningEffort::Off;
        app.app.sync_reasoning_effort_applied();
        app.start_turn(1);

        app.app
            .set_reasoning_effort(protocol::ReasoningEffort::High, false);
        assert!(app.app.reasoning_effort_pending());

        app.app
            .set_reasoning_effort(protocol::ReasoningEffort::Off, false);
        assert!(!app.app.reasoning_effort_pending());
    }

    #[test]
    fn switching_back_to_context_token_identity_clears_stale_marker() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.config.model = "old-model".into();
        app.app.core.config.api_base = "https://old.example".into();
        app.app.core.config.api_key_env = "OLD_KEY".into();
        app.app.core.config.provider_type = "openai".into();
        app.app.core.config.available_models = vec![
            smelt_core::config::ResolvedModel {
                key: "new/new-model".into(),
                provider_name: "new".into(),
                model_name: "new-model".into(),
                api_base: "https://new.example".into(),
                api_key_env: "NEW_KEY".into(),
                provider_type: "openai".into(),
                config: smelt_core::config::ModelConfig::default(),
            },
            smelt_core::config::ResolvedModel {
                key: "old/old-model".into(),
                provider_name: "old".into(),
                model_name: "old-model".into(),
                api_base: "https://old.example".into(),
                api_key_env: "OLD_KEY".into(),
                provider_type: "openai".into(),
                config: smelt_core::config::ModelConfig::default(),
            },
        ];
        let old_identity = app.app.active_context_token_identity();
        app.app.core.session.history = vec![user("hello")];
        app.app
            .core
            .session
            .record_context_tokens(100, old_identity);

        app.app.apply_model("new/new-model", false);
        assert!(app
            .app
            .core
            .session
            .display_context_tokens_stale(&app.app.active_context_token_identity()));

        app.app.apply_model("old/old-model", false);
        assert!(!app
            .app
            .core
            .session
            .display_context_tokens_stale(&app.app.active_context_token_identity()));
    }

    #[test]
    fn apply_model_clears_context_baseline_when_provider_identity_changes() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.config.model = "same-model".into();
        app.app.core.config.api_base = "https://old.example".into();
        app.app.core.config.api_key_env = "OLD_KEY".into();
        app.app.core.config.provider_type = "openai".into();
        app.app.core.config.available_models = vec![smelt_core::config::ResolvedModel {
            key: "new/same-model".into(),
            provider_name: "new".into(),
            model_name: "same-model".into(),
            api_base: "https://new.example".into(),
            api_key_env: "NEW_KEY".into(),
            provider_type: "openai".into(),
            config: smelt_core::config::ModelConfig::default(),
        }];
        let old_identity = app.app.active_context_token_identity();
        app.app.core.session.history = vec![user("hello")];
        app.app
            .core
            .session
            .record_context_tokens(100, old_identity);

        app.app.apply_model("new/same-model", false);

        assert!(app.app.core.session.context_tokens.is_none());
        assert_eq!(app.app.core.session.display_context_tokens(), Some(100));
        assert!(app
            .app
            .core
            .session
            .display_context_tokens_stale(&app.app.active_context_token_identity()));
    }

    #[test]
    fn returning_to_history_mode_removes_mode_change_note() {
        let mut app = normal_app();
        app.app.core.session.history = vec![user("hello")];

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);

        app.app.set_mode(AgentMode::parse("normal").unwrap(), false);

        assert!(mode_blocks(&app.app).is_empty());
        assert!(app
            .app
            .core
            .session
            .history
            .iter()
            .all(|item| item.note_kind() != Some(protocol::HistoryNoteKind::ModeChange)));
    }

    #[test]
    fn returning_to_history_mode_clears_pending_mode_change_during_turn() {
        let mut app = normal_app();
        app.app.core.session.history = vec![user("hello")];
        app.start_turn(1);

        let before = app
            .app
            .pending_history_appends
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        let after_apply = app
            .app
            .pending_history_appends
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();
        let expected_apply =
            protocol::HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                "normal",
                "apply",
                app.app.lua.mode_note("apply"),
            ));
        assert_eq!(&after_apply[..before.len()], before.as_slice());
        assert_eq!(&after_apply[before.len()..], [expected_apply]);
        assert_eq!(pending_mode_appends(&app.app).len(), 1);

        app.app.set_mode(AgentMode::parse("normal").unwrap(), false);

        let after_normal = app
            .app
            .pending_history_appends
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();
        assert_eq!(after_normal, before);
        assert!(pending_mode_appends(&app.app).is_empty());
        assert!(mode_blocks(&app.app).is_empty());
    }

    #[test]
    fn mode_change_without_another_turn_request_commits_at_turn_end() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        app.app.discard_turn(crate::app::TurnEnd::Complete);

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
    }

    #[test]
    fn cancelled_turn_preserves_deferred_mode_change_for_replacement_turn() {
        let mut app = normal_app();
        set_history(&mut app, vec![user("hello")]);
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        assert_eq!(app.app.pending_history_appends.len(), 1);

        app.app.discard_turn(crate::app::TurnEnd::Cancelled);
        assert_eq!(app.app.pending_history_appends.len(), 1);
        assert_eq!(app.app.pending_history_appends[0].mode(), Some("apply"));

        app.app.apply_pending_history_appends_for_request();
        assert_eq!(
            app.app
                .core
                .session
                .history
                .last()
                .and_then(protocol::HistoryItem::as_note)
                .and_then(protocol::HistoryNote::mode),
            Some("apply")
        );
        app.app.restore_screen();

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
        assert_eq!(
            app.app
                .core
                .session
                .history
                .last()
                .and_then(protocol::HistoryItem::as_note)
                .and_then(protocol::HistoryNote::mode),
            Some("apply")
        );
    }

    #[test]
    fn mode_change_before_first_user_message_does_not_push_mode_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);

        assert!(app.app.core.session.history.is_empty());
        assert!(mode_blocks(&app.app).is_empty());
    }

    #[test]
    fn mode_change_after_rewinding_to_first_turn_does_not_push_mode_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        set_history(
            &mut app,
            vec![
                protocol::HistoryItem::note(protocol::HistoryNote::context(
                    "Current working directory: /tmp.",
                )),
                user("first"),
                assistant("reply"),
            ],
        );
        let first_turn_block = app.app.user_turns()[0].0;

        app.app.rewind_to(first_turn_block);
        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);

        assert!(mode_blocks(&app.app).is_empty());
        assert_eq!(
            app.app.core.session.history,
            vec![protocol::HistoryItem::note(protocol::HistoryNote::context(
                "Current working directory: /tmp.",
            ))]
        );
    }

    #[test]
    fn rewind_to_message_before_mode_change_restores_boundary_mode() {
        let mut app = normal_app();
        set_history(
            &mut app,
            vec![
                user("first"),
                assistant("first reply"),
                user("second"),
                assistant("second reply"),
            ],
        );
        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        app.app.core.session.history.push(user("third"));
        app.app.core.session.history.push(assistant("third reply"));
        app.app.sync_session_snapshot();
        app.app.restore_screen();
        let second_turn_block = app.app.user_turns()[1].0;

        let restored = app.app.rewind_to(second_turn_block).expect("second turn");

        assert_eq!(restored.0, "second");
        assert_eq!(app.app.core.config.mode.as_str(), "normal");

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
    }

    #[test]
    fn returning_to_mode_note_base_after_rewind_removes_mode_block() {
        let mut app = normal_app();
        set_history(&mut app, vec![user("first"), assistant("first reply")]);
        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        app.app.core.session.history.push(user("second"));
        app.app.core.session.history.push(assistant("second reply"));
        app.app.sync_session_snapshot();
        app.app.restore_screen();
        let second_turn_block = app.app.user_turns()[1].0;

        app.app.rewind_to(second_turn_block);
        app.app.set_mode(AgentMode::parse("normal").unwrap(), false);

        assert!(mode_blocks(&app.app).is_empty());
        assert!(app
            .app
            .core
            .session
            .history
            .iter()
            .all(|item| item.note_kind() != Some(protocol::HistoryNoteKind::ModeChange)));
    }

    #[tokio::test]
    async fn shell_escape_uses_cwd_captured_at_submission() {
        let _guard = ProcessCwdGuard::capture();
        let shell_cwd = tempfile::TempDir::new().expect("shell cwd");
        let later_cwd = tempfile::TempDir::new().expect("later cwd");
        let shell_cwd = std::fs::canonicalize(shell_cwd.path()).expect("canonical shell cwd");
        let later_cwd = std::fs::canonicalize(later_cwd.path()).expect("canonical later cwd");

        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.cwd = shell_cwd.to_string_lossy().into_owned();
        let mut handle = app
            .app
            .start_shell_escape("pwd")
            .expect("shell escape starts");
        std::env::set_current_dir(&later_cwd).expect("move process cwd after submission");

        let mut output = Vec::new();
        while let Some(event) = handle.rx.recv().await {
            match event {
                ExecEvent::Output(line) => output.push(line),
                ExecEvent::Done(code) => {
                    assert_eq!(code, Some(0));
                    break;
                }
            }
        }
        assert_eq!(output, vec![shell_cwd.to_string_lossy().into_owned()]);
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
            shell("echo hi", ShellSink::Transcript)
        );
    }

    #[test]
    fn shell_escape_trims_leading_whitespace_after_bang() {
        // `! echo hi` and `!echo hi` produce the same script.
        assert_eq!(
            parse_command_line("!   echo hi"),
            shell("echo hi", ShellSink::Transcript)
        );
    }

    #[test]
    fn shell_escape_keeps_an_empty_script_for_bare_bang() {
        assert_eq!(parse_command_line("!"), shell("", ShellSink::Transcript));
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
    fn colon_parses_as_ex_command() {
        assert_eq!(parse_command_line(":quit"), ex("quit", false, None));
        assert_eq!(parse_command_line(":q!"), ex("q", true, None));
        assert_eq!(
            parse_command_line(":model x"),
            ex("model", false, Some("x"))
        );
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
            shell("/foo bar", ShellSink::Transcript)
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
            shell("echo hi", ShellSink::Transcript)
        );
    }

    #[test]
    fn ex_bang_parses_as_overlay_shell() {
        assert_eq!(
            parse_command_line(":!echo hi"),
            shell("echo hi", ShellSink::Overlay)
        );
    }

    #[test]
    fn prompt_colon_commands_are_only_quit_aliases() {
        assert_eq!(
            parse_for_context(":help", CommandContext::prompt()),
            ParsedCommand::Bare { text: ":help" }
        );
        assert_eq!(
            parse_for_context(":q", CommandContext::prompt()),
            ex("q", false, None)
        );
        assert_eq!(
            parse_for_context(":wq", CommandContext::prompt()),
            ex("wq", false, None)
        );
    }

    #[test]
    fn cmdline_colon_commands_use_command_namespace() {
        assert_eq!(
            parse_for_context(":help", CommandContext::cmdline()),
            ex("help", false, None)
        );
    }

    #[test]
    fn slash_commands_remain_prompt_commands() {
        assert_eq!(
            parse_for_context("/help", CommandContext::prompt()),
            slash("help", None)
        );
    }
}
