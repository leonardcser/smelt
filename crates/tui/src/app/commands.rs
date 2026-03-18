use super::*;

impl App {
    // ── Commands ─────────────────────────────────────────────────────────

    pub(super) fn handle_command(&mut self, input: &str) -> CommandAction {
        match input {
            "/exit" | "/quit" | ":q" | ":qa" | ":wq" | ":wqa" => CommandAction::Quit,
            "/clear" | "/new" => CommandAction::CancelAndClear,
            "/compact" => CommandAction::Compact,
            "/resume" => {
                let entries = self.resume_entries();
                if entries.is_empty() {
                    self.screen.notify_error("no saved sessions".into());
                    CommandAction::Continue
                } else {
                    let cwd = std::env::current_dir()
                        .ok()
                        .and_then(|p| p.to_str().map(String::from))
                        .unwrap_or_default();
                    CommandAction::OpenDialog(Box::new(render::ResumeDialog::new(
                        entries,
                        cwd,
                        Some(terminal::size().map(|(_, h)| h / 2).unwrap_or(12)),
                    )))
                }
            }
            "/vim" => {
                let enabled = !self.input.vim_enabled();
                self.input.set_vim_enabled(enabled);
                state::set_vim_enabled(enabled);
                CommandAction::Continue
            }
            "/export" => {
                self.export_to_clipboard();
                CommandAction::Continue
            }
            "/ps" => {
                if self.engine.processes.list().is_empty() {
                    self.screen.notify_error("no background processes".into());
                    CommandAction::Continue
                } else {
                    CommandAction::OpenDialog(Box::new(render::PsDialog::new(
                        self.engine.processes.clone(),
                        None,
                    )))
                }
            }
            "/fork" => {
                self.fork_session();
                CommandAction::Continue
            }
            "/model" => {
                let models: Vec<(String, String, String)> = self
                    .available_models
                    .iter()
                    .map(|m| (m.key.clone(), m.model_name.clone(), m.provider_name.clone()))
                    .collect();
                if !models.is_empty() {
                    self.input.open_model_picker(models);
                    self.screen.mark_dirty();
                }
                CommandAction::Continue
            }
            "/settings" => {
                self.input.open_settings(
                    self.input.vim_enabled(),
                    self.auto_compact,
                    self.show_speed,
                    self.show_prediction,
                    self.show_slug,
                    self.restrict_to_workspace,
                );
                self.screen.mark_dirty();
                CommandAction::Continue
            }
            "/theme" => {
                self.input.open_theme_picker();
                self.screen.mark_dirty();
                CommandAction::Continue
            }
            "/stats" => {
                let entries = crate::metrics::load();
                let lines = crate::metrics::render_stats(&entries);
                self.input.open_stats(lines);
                self.screen.mark_dirty();
                CommandAction::Continue
            }
            _ if input.starts_with("/btw ") => {
                let question = input[5..].trim().to_string();
                if question.is_empty() {
                    self.screen.notify_error("usage: /btw <question>".into());
                } else {
                    self.start_btw(question.clone(), question, vec![]);
                }
                CommandAction::Continue
            }
            _ if input.starts_with('!') && !self.input.skip_shell_escape() => {
                self.run_shell_escape(&input[1..]);
                CommandAction::Continue
            }
            _ => CommandAction::Continue,
        }
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
        if input.starts_with('/') && crate::custom_commands::resolve(input).is_some() {
            return None;
        }

        // Access control: some commands are blocked while running.
        if let Err(reason) = is_allowed_while_running(input) {
            self.screen.notify_error(reason);
            return Some(EventOutcome::Noop);
        }

        // Delegate to the unified handler.
        match self.handle_command(input) {
            CommandAction::Quit => Some(EventOutcome::Quit),
            CommandAction::CancelAndClear => Some(EventOutcome::CancelAndClear),
            CommandAction::OpenDialog(dlg) => Some(EventOutcome::OpenDialog(dlg)),
            CommandAction::Continue => Some(EventOutcome::Noop),
            CommandAction::Compact => unreachable!(), // blocked above
        }
    }

    pub(super) fn run_shell_escape(&mut self, raw: &str) {
        let cmd = raw.trim();
        if cmd.is_empty() {
            return;
        }
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .map(|o| {
                let mut s = String::from_utf8_lossy(&o.stdout).to_string();
                let stderr = String::from_utf8_lossy(&o.stderr);
                if !stderr.is_empty() {
                    if !s.is_empty() {
                        s.push('\n');
                    }
                    s.push_str(&stderr);
                }
                s.truncate(s.trim_end().len());
                s
            })
            .unwrap_or_else(|e| format!("error: {}", e));
        self.screen.push(Block::Exec {
            command: cmd.to_string(),
            output,
        });
        self.screen.flush_blocks();
    }

    pub(super) fn start_btw(
        &mut self,
        question: String,
        display_question: String,
        image_labels: Vec<String>,
    ) {
        self.screen.set_btw(display_question, image_labels);
        self.engine.send(UiCommand::Btw {
            question,
            history: self.history.clone(),
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort,
            api_base: Some(self.api_base.clone()),
            api_key: Some(std::env::var(&self.api_key_env).unwrap_or_default()),
        });
    }

    pub(super) fn toggle_mode(&mut self) {
        self.mode = self.mode.toggle();
        state::set_mode(self.mode);
        self.engine.send(UiCommand::SetMode { mode: self.mode });
        self.screen.mark_dirty();
    }

    pub(super) fn set_reasoning_effort(&mut self, effort: ReasoningEffort) {
        self.reasoning_effort = effort;
        self.screen.set_reasoning_effort(effort);
        state::set_reasoning_effort(effort);
        self.engine.send(UiCommand::SetReasoningEffort { effort });
    }

    pub(super) fn export_to_clipboard(&mut self) {
        let text = self.format_conversation_text();
        if text.is_empty() {
            self.screen.notify_error("nothing to export".into());
            return;
        }
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&text)) {
            Ok(()) => {
                self.screen
                    .notify("conversation copied to clipboard".into());
            }
            Err(e) => {
                self.screen.notify_error(format!("clipboard error: {}", e));
            }
        }
    }

    /// Count queued messages that were actually steered into the engine
    /// (excludes custom commands, which need their own turn).
    pub(super) fn steered_message_count(&self) -> usize {
        self.queued_messages
            .iter()
            .filter(|m| crate::custom_commands::resolve(m.trim()).is_none())
            .count()
    }

    pub(super) fn format_conversation_text(&self) -> String {
        let mut out = String::new();
        for msg in &self.history {
            match msg.role {
                Role::System | Role::Tool => continue,
                Role::User => {
                    if let Some(c) = &msg.content {
                        out.push_str("User: ");
                        out.push_str(c.as_text());
                        out.push_str("\n\n");
                    }
                }
                Role::Assistant => {
                    if let Some(c) = &msg.content {
                        if !c.is_empty() {
                            out.push_str("Assistant: ");
                            out.push_str(c.as_text());
                            out.push_str("\n\n");
                        }
                    }
                    if let Some(calls) = &msg.tool_calls {
                        for tc in calls {
                            out.push_str(&format!("[Tool call: {}]\n\n", tc.function.name));
                        }
                    }
                }
            }
        }
        out.trim_end().to_string()
    }
}
