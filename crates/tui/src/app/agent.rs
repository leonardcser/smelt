use super::working::{TurnOutcome, TurnPhase};
use super::*;
use protocol::Decision;

impl TuiApp {
    /// Send a permission decision to the local engine.
    pub(crate) fn send_permission_decision(
        &mut self,
        request_id: u64,
        approved: bool,
        message: Option<String>,
    ) {
        self.core.engine.send(UiCommand::PermissionDecision {
            request_id,
            approved,
            message,
        });
    }

    // ── Agent lifecycle ──────────────────────────────────────────────────

    pub(super) fn begin_agent_turn(&mut self, display: &str, content: Content) -> TurnState {
        self.sleep_inhibit.acquire();
        self.clear_prompt_completer();
        self.begin_turn();
        self.show_user_message(display, content.image_labels());
        let text = content.text_content();
        if self.core.session.first_user_message.is_none() {
            self.core.session.first_user_message = Some(text.clone());
        }
        if !content.is_empty() {
            self.core
                .session
                .messages
                .push(Message::user(content.clone()));
            self.sync_session_snapshot();
            self.core.session.messages.pop();
        }
        self.maybe_generate_title(Some(&text));
        self.dispatch_turn(content)
    }

    /// Mark the engine busy, allocate a turn id, and send `StartTurn` with the
    /// current app state. Callers own any history/session prep before this.
    fn dispatch_turn(&mut self, content: Content) -> TurnState {
        let Some(api_key) = self.resolve_api_key() else {
            {
                self.working.finish(TurnOutcome::Done);
            };
            return TurnState {
                turn_id: 0,
                pending: Vec::new(),
                _perf: crate::perf::begin("agent:turn"),
            };
        };

        {
            self.working.begin(TurnPhase::Working);
        };

        self.core
            .cells
            .set_dyn("turn_start", std::rc::Rc::new(crate::app::cells::EventStub));
        self.drain_cells_pending();
        self.flush_lua_callbacks();

        let system_prompt = self.rebuild_system_prompt();
        let tools = self.core.lua.tool_defs(self.core.config.mode);

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
            api_key: Some(api_key),
            session_id: self.core.session.id.clone(),
            session_dir: crate::session::dir_for(&self.core.session),
            model_config_overrides: None,
            permission_overrides: None,
            system_prompt: Some(system_prompt),
            tools,
        });

        TurnState {
            turn_id,
            pending: Vec::new(),
            _perf: crate::perf::begin("agent:turn"),
        }
    }

    pub(crate) fn begin_custom_command_turn(
        &mut self,
        cmd: crate::custom_commands::CustomCommand,
    ) -> TurnState {
        // Body comes pre-rendered from Lua (frontmatter stripped, exec
        // blocks evaluated, extra args appended). Apply redaction
        // before history / engine dispatch.
        let evaluated = if self.core.config.settings.redact_secrets {
            engine::redact::redact(&cmd.body)
        } else {
            cmd.body.clone()
        };
        let display = format!("/{}", cmd.name);

        if !evaluated.is_empty() {
            self.core
                .session
                .messages
                .push(Message::user(Content::text(evaluated.clone())));
            self.sync_session_snapshot();
            self.core.session.messages.pop();
        }

        // Resolve model/provider overrides
        let (model, api_base, api_key) = {
            let target_model = cmd.overrides.model.as_deref();
            let target_provider = cmd.overrides.provider.as_deref();
            let resolved = match (target_model, target_provider) {
                (Some(reference), provider) => {
                    match crate::config::resolve_model_ref_with_provider(
                        &self.core.config.available_models,
                        reference,
                        provider,
                    ) {
                        Ok(model) => Some(model),
                        Err(err) => {
                            self.notify_error(err.to_string());
                            None
                        }
                    }
                }
                (None, Some(provider)) => {
                    match crate::config::resolve_provider_ref(
                        &self.core.config.available_models,
                        provider,
                    ) {
                        Ok(model) => Some(model),
                        Err(err) => {
                            self.notify_error(err.to_string());
                            None
                        }
                    }
                }
                (None, None) => None,
            }
            .map(|resolved| {
                (
                    resolved.model_name.clone(),
                    resolved.api_base.clone(),
                    resolved.api_key_env.clone(),
                )
            });
            match resolved {
                Some((model_name, api_base, api_key_env)) => (
                    model_name,
                    api_base,
                    self.resolve_api_key_for_env(&api_key_env)
                        .unwrap_or_default(),
                ),
                None => (
                    self.core.config.model.clone(),
                    self.core.config.api_base.clone(),
                    self.resolve_api_key().unwrap_or_default(),
                ),
            }
        };

        let reasoning = cmd
            .overrides
            .reasoning_effort
            .as_deref()
            .map(|s| match s.to_lowercase().as_str() {
                "low" => protocol::ReasoningEffort::Low,
                "medium" => protocol::ReasoningEffort::Medium,
                "high" => protocol::ReasoningEffort::High,
                _ => protocol::ReasoningEffort::Off,
            })
            .unwrap_or(self.core.config.reasoning_effort);

        let model_config_overrides = {
            let o = &cmd.overrides;
            if o.temperature.is_some()
                || o.top_p.is_some()
                || o.top_k.is_some()
                || o.min_p.is_some()
                || o.repeat_penalty.is_some()
            {
                Some(protocol::ModelConfigOverrides {
                    temperature: o.temperature,
                    top_p: o.top_p,
                    top_k: o.top_k,
                    min_p: o.min_p,
                    repeat_penalty: o.repeat_penalty,
                })
            } else {
                None
            }
        };

        let permission_overrides = {
            let o = &cmd.overrides;
            if o.tools.is_some() || o.bash.is_some() || o.web_fetch.is_some() {
                Some(protocol::PermissionOverrides {
                    tools: o.tools.as_ref().map(|r| protocol::RuleSetOverride {
                        allow: r.allow.clone(),
                        ask: r.ask.clone(),
                        deny: r.deny.clone(),
                    }),
                    bash: o.bash.as_ref().map(|r| protocol::RuleSetOverride {
                        allow: r.allow.clone(),
                        ask: r.ask.clone(),
                        deny: r.deny.clone(),
                    }),
                    web_fetch: o.web_fetch.as_ref().map(|r| protocol::RuleSetOverride {
                        allow: r.allow.clone(),
                        ask: r.ask.clone(),
                        deny: r.deny.clone(),
                    }),
                })
            } else {
                None
            }
        };

        self.sleep_inhibit.acquire();
        self.begin_turn();
        self.show_user_message(&display, vec![]);
        if self.core.session.first_user_message.is_none() {
            self.core.session.first_user_message = Some(display.clone());
        }
        self.maybe_generate_title(Some(&evaluated));
        {
            self.working.begin(TurnPhase::Working);
        };

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.core.engine.send(UiCommand::StartTurn {
            turn_id,
            content: Content::text(evaluated),
            mode: self.core.config.mode,
            model,
            reasoning_effort: reasoning,
            history: self.core.session.messages.clone(),
            api_base: Some(api_base),
            api_key: Some(api_key),
            session_id: self.core.session.id.clone(),
            session_dir: crate::session::dir_for(&self.core.session),
            model_config_overrides,
            permission_overrides,
            system_prompt: None,
            tools: vec![],
        });

        TurnState {
            turn_id,
            pending: Vec::new(),
            _perf: crate::perf::begin("agent:turn"),
        }
    }

    /// Lightweight cancel: stop the engine turn without saving session,
    /// generating titles, or triggering auto-compact. Used before rewind/clear
    /// where the history will be mutated immediately after.
    pub(crate) fn cancel_agent(&mut self) {
        self.sleep_inhibit.release();
        self.core.engine.send(UiCommand::Cancel);
        {
            self.working.finish(TurnOutcome::Interrupted);
        };
        self.queued_messages.clear();
    }

    /// Finish and drop the active turn (no-op when idle). Combines
    /// the `finish_turn` + `self.agent = None` pair every cancel
    /// site needs.
    pub(super) fn discard_turn(&mut self, cancelled: bool) {
        if self.agent.is_some() {
            self.finish_turn(cancelled);
            self.agent = None;
        }
    }

    pub(crate) fn finish_turn(&mut self, cancelled: bool) {
        self.sleep_inhibit.release();
        if cancelled {
            self.core.engine.send(UiCommand::Cancel);
        }
        self.core.cells.set_dyn(
            "turn_end",
            std::rc::Rc::new(crate::app::cells::TurnEnd { cancelled }),
        );
        self.drain_cells_pending();
        self.flush_lua_callbacks();
        // Flush any in-flight streaming content before committing tools.
        self.flush_streaming_thinking();
        self.flush_streaming_text();
        // Commit active tools to block history but don't render yet —
        // the next draw_frame renders blocks + prompt atomically in one
        // synchronized update, avoiding a flash where the prompt disappears.
        self.finish_transcript_turn();
        if cancelled {
            {
                self.working.finish(TurnOutcome::Interrupted);
            };
            // If a title/slug generation was in-flight, discard it so stale
            // TitleGenerated events don't update the session. But if a slug
            // was already set before this turn, keep it.
            if self.pending_title {
                self.pending_title = false;
                // Only clear the slug if it wasn't already set before this
                // turn's title generation request. If a slug existed before,
                // keep it — we're just discarding the in-flight update.
            }
            let leftover = std::mem::take(&mut self.queued_messages);
            if !leftover.is_empty() {
                let mut combined = leftover.join("\n");
                if !self.input.buf.is_empty() {
                    combined.push('\n');
                    combined.push_str(&self.input.buf);
                }
                let __mode = self.vim_mode;
                crate::api::buf::replace(&mut self.input, combined, None, __mode);
            }
        } else {
            {
                self.working.finish(TurnOutcome::Done);
            };
            self.clear_prompt_completer();
        }
        let meta = self
            .pending_turn_meta
            .take()
            .or_else(|| self.working.turn_meta());
        if let Some(meta) = meta {
            self.core
                .session
                .turn_metas
                .push((self.core.session.messages.len(), meta));
        }
        self.snapshot_tokens();
        self.save_session();
        self.maybe_auto_compact();
    }

    /// Execute a plugin-defined tool by calling the Lua handler registered for
    /// it. If no handler is found, returns an error result to the engine.
    ///
    /// Handlers run as `LuaTask`s. A handler that doesn't yield
    /// completes synchronously and the result is forwarded right away.
    /// A handler that yields (e.g. via `smelt.ui.dialog.open`) parks;
    /// its result arrives later through `drive_tasks()`.
    pub(super) fn handle_tool_call(
        &mut self,
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        let mode = self.core.config.mode;
        let session_id = self.core.session.id.clone();
        let session_dir = crate::session::dir_for(&self.core.session);
        match self.core.lua.execute_tool(
            &tool_name,
            &args,
            request_id,
            &call_id,
            crate::lua::ToolEnv {
                mode,
                session_id: &session_id,
                session_dir: &session_dir,
            },
        ) {
            crate::lua::ToolExecResult::Immediate { content, is_error } => {
                self.core.engine.send(protocol::UiCommand::ToolResult {
                    request_id,
                    call_id,
                    content,
                    is_error,
                });
            }
            crate::lua::ToolExecResult::Pending => {
                // Result will be delivered via drive_tasks.
            }
        }
    }

    pub(super) fn handle_title_generated(&mut self, title: String, slug: String) {
        if !self.pending_title {
            return;
        }
        self.core.session.title = Some(title);
        self.core.session.slug = Some(slug.clone());
        self.set_task_label(slug.clone());
        self.pending_title = false;
        self.save_session();
    }

    pub(super) fn handle_input_prediction(&mut self, text: String) {
        if self.input.buf.is_empty() {
            self.set_prompt_completer(text);
        }
    }

    pub(super) fn resolve_api_key(&mut self) -> Option<String> {
        if self.core.config.api_key_env.is_empty() {
            return Some(String::new());
        }
        match std::env::var(&self.core.config.api_key_env) {
            Ok(key) => Some(key),
            Err(std::env::VarError::NotPresent) => {
                self.notify_error(format!(
                    "environment variable '{}' is not set but is required for API authentication",
                    self.core.config.api_key_env
                ));
                None
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                self.notify_error(format!(
                    "environment variable '{}' contains non-Unicode data and cannot be used as an API key",
                    self.core.config.api_key_env
                ));
                None
            }
        }
    }

    pub(super) fn resolve_api_key_for_env(&mut self, key_env: &str) -> Option<String> {
        if key_env.is_empty() {
            return Some(String::new());
        }
        match std::env::var(key_env) {
            Ok(key) => Some(key),
            Err(std::env::VarError::NotPresent) => {
                self.notify_error(format!(
                    "environment variable '{}' is not set but is required for API authentication",
                    key_env
                ));
                None
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                self.notify_error(format!(
                    "environment variable '{}' contains non-Unicode data and cannot be used as an API key",
                    key_env
                ));
                None
            }
        }
    }

    pub(super) fn handle_process_completed(&mut self, id: String, exit_code: Option<i32>) {
        let msg = match exit_code {
            Some(0) => format!("Background process {id} has finished."),
            Some(c) => format!("Background process {id} exited with code {c}."),
            None => format!("Background process {id} exited."),
        };
        self.push_block(Block::Text { content: msg });
    }

    pub(crate) fn session_permission_entries(&self) -> Vec<PermissionEntry> {
        let rt = self.runtime_approvals.read().unwrap();
        let mut entries = Vec::new();
        for (tool, patterns) in rt.session_tool_entries() {
            if patterns.is_empty() {
                entries.push(PermissionEntry {
                    tool,
                    pattern: "*".into(),
                });
            } else {
                for p in patterns {
                    entries.push(PermissionEntry {
                        tool: tool.clone(),
                        pattern: p,
                    });
                }
            }
        }
        for dir in rt.session_dirs() {
            entries.push(PermissionEntry {
                tool: "directory".into(),
                pattern: dir.display().to_string(),
            });
        }
        entries
    }

    pub(crate) fn sync_permissions(
        &mut self,
        session_entries: Vec<PermissionEntry>,
        workspace_rules: Vec<crate::permissions::store::Rule>,
    ) {
        // Rebuild session approvals from flattened entries.
        let mut session_tools: HashMap<String, Vec<glob::Pattern>> = HashMap::new();
        let mut session_dirs: Vec<PathBuf> = Vec::new();
        for entry in session_entries {
            if entry.tool == "directory" {
                session_dirs.push(std::path::PathBuf::from(&entry.pattern));
            } else if entry.pattern == "*" {
                session_tools.entry(entry.tool).or_default();
            } else if let Ok(pat) = glob::Pattern::new(&entry.pattern) {
                session_tools.entry(entry.tool).or_default().push(pat);
            }
        }

        // Persist and reload workspace rules.
        crate::permissions::store::save(&self.cwd, &workspace_rules);
        let (ws_tools, ws_dirs) = crate::permissions::store::into_approvals(&workspace_rules);
        let mut rt = self.runtime_approvals.write().unwrap();
        rt.set_session(session_tools, session_dirs);
        rt.load_workspace(ws_tools, ws_dirs);
    }

    fn reload_workspace_permissions(&mut self) {
        let rules = crate::permissions::store::load(&self.cwd);
        let (ws_tools, ws_dirs) = crate::permissions::store::into_approvals(&rules);
        self.runtime_approvals
            .write()
            .unwrap()
            .load_workspace(ws_tools, ws_dirs);
    }

    pub(super) fn reset_session_permissions(&mut self) {
        self.runtime_approvals.write().unwrap().clear_session();
    }

    /// Resolve a completed confirm dialog choice.
    /// Returns `true` if the agent should be cancelled.
    pub(crate) fn resolve_confirm(
        &mut self,
        (choice, message): (ConfirmChoice, Option<String>),
        call_id: &str,
        request_id: u64,
        tool_name: &str,
    ) -> bool {
        let label = match &choice {
            ConfirmChoice::Yes => "approved",
            ConfirmChoice::Always(_) => "always",
            ConfirmChoice::AlwaysPatterns(ref pats, _) => {
                pats.first().map(|s| s.as_str()).unwrap_or("pattern")
            }
            ConfirmChoice::AlwaysDir(dir, _) => dir.as_str(),
            ConfirmChoice::No => "denied",
        };
        if let Some(ref msg) = message {
            self.set_active_user_message(call_id, format!("{label}: {msg}"));
        }
        match choice {
            ConfirmChoice::Yes => {
                self.set_active_status(call_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::Always(scope) => {
                match scope {
                    ApprovalScope::Session => {
                        self.runtime_approvals
                            .write()
                            .unwrap()
                            .add_session_tool(tool_name, vec![]);
                    }
                    ApprovalScope::Workspace => {
                        crate::permissions::store::add_tool(&self.cwd, tool_name, vec![]);
                        self.reload_workspace_permissions();
                    }
                }
                self.set_active_status(call_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::AlwaysPatterns(ref patterns, scope) => {
                let compiled: Vec<glob::Pattern> = patterns
                    .iter()
                    .filter_map(|p| glob::Pattern::new(p).ok())
                    .collect();
                match scope {
                    ApprovalScope::Session => {
                        self.runtime_approvals
                            .write()
                            .unwrap()
                            .add_session_tool(tool_name, compiled);
                    }
                    ApprovalScope::Workspace => {
                        crate::permissions::store::add_tool(&self.cwd, tool_name, patterns.clone());
                        self.reload_workspace_permissions();
                    }
                }
                self.set_active_status(call_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::AlwaysDir(ref dir, scope) => {
                match scope {
                    ApprovalScope::Session => {
                        self.runtime_approvals
                            .write()
                            .unwrap()
                            .add_session_dir(std::path::PathBuf::from(dir));
                    }
                    ApprovalScope::Workspace => {
                        crate::permissions::store::add_dir(&self.cwd, dir);
                        self.reload_workspace_permissions();
                    }
                }
                self.set_active_status(call_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::No => {
                let has_message = message.is_some();
                self.send_permission_decision(request_id, false, message);
                self.finish_tool(call_id, ToolStatus::Denied, None, None);
                if has_message {
                    if let Some(ref mut ag) = self.agent {
                        ag.pending.retain(|p| p.call_id != call_id);
                    }
                    false
                } else {
                    engine::log::entry(
                        engine::log::Level::Info,
                        "agent_stop",
                        &serde_json::json!({
                            "reason": "confirm_denied",
                            "tool": tool_name,
                        }),
                    );
                    if let Some(ref mut ag) = self.agent {
                        ag.pending.clear();
                    }
                    true
                }
            }
        }
    }

    // ── Control dispatch ─────────────────────────────────────────────────

    pub(super) fn dispatch_control(
        &mut self,
        ctrl: SessionControl,
        pending: &[PendingTool],
        pending_dialogs: &mut VecDeque<DeferredDialog>,
        last_keypress: Option<Instant>,
    ) -> LoopAction {
        // Queue dialogs when a blocking overlay is open or the user is typing.
        // The queue is drained in the main loop via re-dispatch, so auto-approval
        // checks re-run (handles "always allow" → recheck).
        let should_queue = self.focused_overlay_blocks_agent()
            || (last_keypress
                .is_some_and(|t| t.elapsed() < Duration::from_millis(CONFIRM_DEFER_MS))
                && !self.input.buf.is_empty());

        match ctrl {
            SessionControl::Continue => LoopAction::Continue,
            SessionControl::Done => LoopAction::Done,
            SessionControl::NeedsConfirm(mut req) => {
                if req.tool_name.is_empty() {
                    req.tool_name = pending.last().map(|p| p.name.clone()).unwrap_or_default();
                }

                // Check runtime auto-approvals.
                let auto_approved = {
                    let rt = self.runtime_approvals.read().unwrap();
                    rt.is_auto_approved(
                        &self.permissions,
                        self.core.config.mode,
                        &req.tool_name,
                        &req.args,
                        &req.desc,
                    )
                };
                if auto_approved {
                    self.send_permission_decision(req.request_id, true, None);
                    return LoopAction::Continue;
                }

                // Check mode-based permissions (e.g. Apply mode auto-allows writes).
                if self
                    .permissions
                    .decide(self.core.config.mode, &req.tool_name, &req.args, false)
                    == Decision::Allow
                {
                    self.send_permission_decision(req.request_id, true, None);
                    return LoopAction::Continue;
                }

                let outside_paths = self
                    .permissions
                    .outside_workspace_paths(&req.tool_name, &req.args);

                // Auto-approval didn't match — queue if we can't show a dialog now.
                if should_queue {
                    self.set_active_status(&req.call_id, ToolStatus::Confirm);
                    self.pending_dialog = true;
                    pending_dialogs.push_back(DeferredDialog::Confirm(req));
                    return LoopAction::Continue;
                }

                // Prepare dialog options.
                let downgraded = self.permissions.was_downgraded(
                    self.core.config.mode,
                    &req.tool_name,
                    &req.args,
                );
                req.outside_dir = if downgraded && !outside_paths.is_empty() {
                    // Only offer the dir option when the Ask is specifically
                    // from the workspace restriction (downgraded from Allow).
                    // When the Ask is from the command itself (e.g. `for` loop),
                    // a dir approval won't help — show tool patterns instead.
                    let raw = std::path::Path::new(&outside_paths[0]);
                    let expanded = engine::paths::expand_tilde(raw);
                    let abs_dir = if expanded.is_dir() {
                        expanded
                    } else {
                        expanded.parent().unwrap_or(&expanded).to_path_buf()
                    };
                    Some(engine::paths::collapse_tilde(&abs_dir))
                } else {
                    None
                };

                if !req.approval_patterns.is_empty() {
                    let rt = self.runtime_approvals.read().unwrap();
                    req.approval_patterns
                        .retain(|p| !rt.has_pattern(&req.tool_name, p));
                }

                // Close any non-blocking overlay (e.g. Ps) to make room.
                self.close_focused_non_blocking_overlay();
                self.set_active_status(&req.call_id, ToolStatus::Confirm);

                // Register the request and fire the Lua dialog. The
                // dialog (runtime/lua/smelt/dialogs/confirm.lua) reads
                // the payload from the `confirm_requested` cell, builds
                // its own option labels from `outside_dir` +
                // `approval_patterns`, and resolves through
                // `smelt.confirm._resolve(handle_id, decision, message)`
                // on submit / dismiss.
                let snapshot = crate::app::cells::ConfirmRequested {
                    handle_id: 0,
                    tool_name: req.tool_name.clone(),
                    desc: req.desc.clone(),
                    summary: req.summary.clone(),
                    args: req.args.clone(),
                    outside_dir: req
                        .outside_dir
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    approval_patterns: req.approval_patterns.clone(),
                };
                let handle_id = self.core.confirms.register(*req);
                self.core.cells.set_dyn(
                    "confirm_requested",
                    std::rc::Rc::new(crate::app::cells::ConfirmRequested {
                        handle_id,
                        ..snapshot
                    }),
                );
                self.core.lua.fire_confirm_open(handle_id);
                LoopAction::Continue
            }
        }
    }
}
