use crate::app::{
    DeferredDialog, PendingTool, SessionControl, TuiApp, TurnState, CONFIRM_DEFER_MS,
};
use protocol::{Content, Decision, HistoryItem, UiCommand};
use smelt_core::working::{TurnOutcome, TurnPhase};
use smelt_core::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

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

    pub(crate) fn begin_agent_turn(&mut self, display: &str, content: Content) -> TurnState {
        let _perf = smelt_perf::perf::begin("agent:begin_turn");
        self.sleep_inhibit.acquire();
        self.clear_placeholder(self.well_known.prompt);
        self.begin_turn();
        self.show_user_message(display, content.image_labels());
        let text = content.text_content();
        if self.core.session.first_user_message.is_none() {
            self.core.session.first_user_message = Some(text.clone().into_owned());
        }
        if !content.is_empty() {
            self.core
                .session
                .history
                .push(HistoryItem::user(content.clone()));
            self.sync_session_snapshot();
            self.core.session.history.pop();
        }
        self.dispatch_turn(content)
    }

    fn dispatch_turn(&mut self, content: Content) -> TurnState {
        let Some(api_key) = self.resolve_api_key() else {
            {
                self.working.finish(TurnOutcome::Done);
            };
            return TurnState {
                turn_id: 0,
                pending: Vec::new(),
                _perf: smelt_perf::perf::begin("agent:turn"),
            };
        };

        {
            self.working.begin(TurnPhase::Working);
        };

        self.core
            .cells
            .set_dyn("turn_start", std::rc::Rc::new(smelt_core::cells::EventStub));
        self.pump_lua();

        let system_prompt = {
            let _perf = smelt_perf::perf::begin("agent:rebuild_prompt");
            self.rebuild_system_prompt()
        };
        let tools = {
            let _perf = smelt_perf::perf::begin("agent:tool_defs");
            self.lua.tool_defs(self.core.config.mode)
        };

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.core
            .engine
            .send(UiCommand::StartTurn(Box::new(protocol::StartTurnPayload {
                turn_id,
                content,
                mode: self.core.config.mode,
                model: self.core.config.model.clone(),
                reasoning_effort: self.core.config.reasoning_effort,
                history: self.model_history(),
                api_base: Some(self.core.config.api_base.clone()),
                api_key: Some(api_key),
                session_id: self.core.session.id.clone(),
                session_dir: smelt_core::session::dir_for(&self.core.session),
                model_config_overrides: None,
                permission_overrides: None,
                system_prompt: Some(system_prompt),
                tools,
            })));

        TurnState {
            turn_id,
            pending: Vec::new(),
            _perf: smelt_perf::perf::begin("agent:turn"),
        }
    }

    pub(crate) fn begin_custom_command_turn(
        &mut self,
        cmd: smelt_core::custom_commands::CustomCommand,
    ) -> TurnState {
        let evaluated = if self.core.config.settings.redact_secrets {
            engine::redact::redact(&cmd.body)
        } else {
            cmd.body.clone()
        };
        let display = format!("/{}", cmd.display);

        if !evaluated.is_empty() {
            self.core
                .session
                .history
                .push(HistoryItem::user(Content::text(evaluated.clone())));
            self.sync_session_snapshot();
            self.core.session.history.pop();
        }

        let (model, api_base, api_key) = {
            let target_model = cmd.overrides.model.as_deref();
            let target_provider = cmd.overrides.provider.as_deref();
            let resolved = match (target_model, target_provider) {
                (Some(reference), provider) => {
                    match smelt_core::config::resolve_model_ref_with_provider(
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
                    match smelt_core::config::resolve_provider_ref(
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
                    max_tokens: None,
                    thinking_budgets: None,
                })
            } else {
                None
            }
        };

        let permission_overrides = {
            let o = &cmd.overrides;
            if o.tools.is_some() || !o.subcommands.is_empty() {
                let to_override =
                    |r: &smelt_core::custom_commands::RuleOverride| protocol::RuleSetOverride {
                        allow: r.allow.clone(),
                        ask: r.ask.clone(),
                        deny: r.deny.clone(),
                    };
                Some(protocol::PermissionOverrides {
                    tools: o.tools.as_ref().map(to_override),
                    subcommands: o
                        .subcommands
                        .iter()
                        .map(|(k, v)| (k.clone(), to_override(v)))
                        .collect(),
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
        {
            self.working.begin(TurnPhase::Working);
        };

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.core
            .engine
            .send(UiCommand::StartTurn(Box::new(protocol::StartTurnPayload {
                turn_id,
                content: Content::text(evaluated),
                mode: self.core.config.mode,
                model,
                reasoning_effort: reasoning,
                history: self.model_history(),
                api_base: Some(api_base),
                api_key: Some(api_key),
                session_id: self.core.session.id.clone(),
                session_dir: smelt_core::session::dir_for(&self.core.session),
                model_config_overrides,
                permission_overrides,
                system_prompt: None,
                tools: vec![],
            })));

        TurnState {
            turn_id,
            pending: Vec::new(),
            _perf: smelt_perf::perf::begin("agent:turn"),
        }
    }

    /// Stop the engine turn without saving session or triggering auto-compact; used before rewind/clear.
    pub(crate) fn cancel_agent(&mut self) {
        self.sleep_inhibit.release();
        self.core.engine.send(UiCommand::Cancel);
        self.lua.cancel_tasks();
        self.cancel_generation = self.cancel_generation.wrapping_add(1);
        self.busy_stack.clear();
        // A turn is ending without going through `finish_turn`. Commit any
        // in-flight streaming buffers so the post-cancel state honors the
        // "no agent ⇒ no active stream" invariant (an empty thinking delta
        // arrived right before this can leave an empty `active_thinking`
        // sentinel that lingers past `agent = None`).
        self.flush_streaming_thinking();
        self.flush_streaming_text();
        {
            self.working.finish(TurnOutcome::Interrupted);
        };
        self.queued_inputs.clear();
    }

    pub(crate) fn discard_turn(&mut self, cancelled: bool) {
        if self.agent.is_some() {
            self.finish_turn(cancelled);
            self.agent = None;
        } else if cancelled {
            // No active turn but user requested cancel — still notify the
            // engine and kill any running Lua tasks (background tool calls,
            // bash executions, etc.).
            self.core.engine.send(UiCommand::Cancel);
            self.lua.cancel_tasks();
            self.cancel_generation = self.cancel_generation.wrapping_add(1);
            self.busy_stack.clear();
            // Archive an interrupted outcome so the prompt bar shows
            // "interrupted" rather than falling back to idle/done.
            self.working.finish(TurnOutcome::Interrupted);
        }
    }

    pub(crate) fn finish_turn(&mut self, cancelled: bool) {
        self.sleep_inhibit.release();
        if cancelled {
            self.core.engine.send(UiCommand::Cancel);
            self.lua.cancel_tasks();
            self.cancel_generation = self.cancel_generation.wrapping_add(1);
            self.busy_stack.clear();
        }
        self.core.cells.set_dyn(
            "turn_end",
            std::rc::Rc::new(smelt_core::cells::TurnEnd { cancelled }),
        );
        self.pump_lua();
        self.flush_streaming_thinking();
        self.flush_streaming_text();
        self.finish_transcript_turn();
        if cancelled {
            {
                self.working.finish(TurnOutcome::Interrupted);
            };
            let leftover = std::mem::take(&mut self.queued_inputs);
            if !leftover.is_empty() {
                let mut ctx = crate::input::prompt_ctx_mut(&mut self.ui);
                let mut prefix = leftover
                    .iter()
                    .map(crate::app::QueuedInput::prompt_replay_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !ctx.buf.source().is_empty() {
                    prefix.push('\n');
                }
                self.input.prepend_text(&mut ctx, prefix);
            }
        } else {
            self.working.finish(TurnOutcome::Done);
            self.clear_placeholder(self.well_known.prompt);
        }
        let meta = self
            .pending_turn_meta
            .take()
            .or_else(|| self.working.turn_meta());
        if let Some(meta) = meta {
            self.core
                .session
                .turn_metas
                .push((self.core.session.history.len(), meta));
        }
        self.snapshot_tokens();
        self.save_session();
    }

    /// Invokes the Lua handler for a plugin-defined tool; synchronous handlers resolve immediately, async ones park until `drive_tasks` completes them.
    pub(crate) fn handle_tool_call(
        &mut self,
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        let mode = self.core.config.mode;
        let session_id = self.core.session.id.clone();
        let session_dir = smelt_core::session::dir_for(&self.core.session);
        match self.lua.execute_tool(
            &tool_name,
            &args,
            request_id,
            &call_id,
            crate::lua::ToolEnv {
                mode,
                session_id: &session_id,
                session_dir: &session_dir,
            },
            self.core.clock.instant_now(),
        ) {
            crate::lua::ToolExecResult::Immediate {
                content,
                is_error,
                metadata,
            } => {
                self.core.engine.send(protocol::UiCommand::ToolResult {
                    request_id,
                    call_id,
                    content,
                    is_error,
                    metadata,
                });
            }
            crate::lua::ToolExecResult::Pending => {}
        }
    }

    pub(crate) fn resolve_api_key(&mut self) -> Option<String> {
        let key_env = self.core.config.api_key_env.clone();
        self.resolve_api_key_for_env(&key_env)
    }

    pub(crate) fn resolve_api_key_for_env(&mut self, key_env: &str) -> Option<String> {
        match lookup_api_key(key_env, |v| std::env::var(v)) {
            Ok(key) => Some(key),
            Err(err) => {
                self.notify_error(err.message());
                None
            }
        }
    }

    pub(crate) fn handle_process_completed(&mut self, id: String, exit_code: Option<i32>) {
        let msg = match exit_code {
            Some(0) => format!("Background process {id} has finished."),
            Some(c) => format!("Background process {id} exited with code {c}."),
            None => format!("Background process {id} exited."),
        };
        self.push_block(Block::Text { content: msg });
    }

    pub(crate) fn session_permission_entries(&self) -> Vec<PermissionEntry> {
        let rt = self.core.permissions.approvals.read().unwrap();
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
        workspace_rules: Vec<smelt_core::permissions::store::Rule>,
    ) {
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

        smelt_core::permissions::store::save(&self.cwd, &workspace_rules);
        let (ws_tools, ws_dirs) = smelt_core::permissions::store::into_approvals(&workspace_rules);
        let mut rt = self.core.permissions.approvals.write().unwrap();
        rt.set_session(session_tools, session_dirs);
        rt.load_workspace(ws_tools, ws_dirs);
    }

    fn reload_workspace_permissions(&mut self) {
        let rules = smelt_core::permissions::store::load(&self.cwd);
        let (ws_tools, ws_dirs) = smelt_core::permissions::store::into_approvals(&rules);
        self.core
            .permissions
            .approvals
            .write()
            .unwrap()
            .load_workspace(ws_tools, ws_dirs);
    }

    pub(crate) fn reset_session_permissions(&mut self) {
        self.core
            .permissions
            .approvals
            .write()
            .unwrap()
            .clear_session();
    }

    /// Resolves a confirm dialog choice; returns `true` if the agent should be cancelled.
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
                        self.core
                            .permissions
                            .approvals
                            .write()
                            .unwrap()
                            .add_session_tool(tool_name, vec![]);
                    }
                    ApprovalScope::Workspace => {
                        smelt_core::permissions::store::add_tool(&self.cwd, tool_name, vec![]);
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
                        self.core
                            .permissions
                            .approvals
                            .write()
                            .unwrap()
                            .add_session_tool(tool_name, compiled);
                    }
                    ApprovalScope::Workspace => {
                        smelt_core::permissions::store::add_tool(
                            &self.cwd,
                            tool_name,
                            patterns.clone(),
                        );
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
                        self.core
                            .permissions
                            .approvals
                            .write()
                            .unwrap()
                            .add_session_dir(std::path::PathBuf::from(dir));
                    }
                    ApprovalScope::Workspace => {
                        smelt_core::permissions::store::add_dir(&self.cwd, dir);
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

    /// Dispatches one engine-event control signal; returns `true` to continue draining, `false` on turn end.
    pub(crate) fn dispatch_control(
        &mut self,
        ctrl: SessionControl,
        pending: &[PendingTool],
    ) -> bool {
        let should_queue = self
            .timers
            .last_keypress
            .is_some_and(|t| t.elapsed() < Duration::from_millis(CONFIRM_DEFER_MS))
            && !self.prompt_buf().source().is_empty();

        match ctrl {
            SessionControl::Continue => true,
            SessionControl::Done => false,
            SessionControl::NeedsConfirm(mut req) => {
                if req.tool_name.is_empty() {
                    req.tool_name = pending.last().map(|p| p.name.clone()).unwrap_or_default();
                }

                let summary_plain = req.summary.as_plain_text();
                let auto_approved = {
                    let rt = self.core.permissions.approvals.read().unwrap();
                    rt.is_auto_approved(
                        &self.core.permissions,
                        self.core.config.mode,
                        &req.tool_name,
                        &req.args,
                        &summary_plain,
                    )
                };
                if auto_approved {
                    self.send_permission_decision(req.request_id, true, None);
                    return true;
                }

                if self.core.permissions.decide(
                    self.core.config.mode,
                    &req.tool_name,
                    &req.args,
                    false,
                ) == Decision::Allow
                {
                    self.send_permission_decision(req.request_id, true, None);
                    return true;
                }

                let outside_paths = self
                    .core
                    .permissions
                    .outside_workspace_paths(&req.tool_name, &req.args);

                if should_queue {
                    self.set_active_status(&req.call_id, ToolStatus::Confirm);
                    self.pending_dialog = true;
                    self.pending_dialogs.push_back(DeferredDialog::Confirm(req));
                    return true;
                }

                let downgraded = self.core.permissions.was_downgraded(
                    self.core.config.mode,
                    &req.tool_name,
                    &req.args,
                );
                req.outside_dir = if downgraded && !outside_paths.is_empty() {
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
                    let rt = self.core.permissions.approvals.read().unwrap();
                    req.approval_patterns
                        .retain(|p| !rt.has_pattern(&req.tool_name, p));
                }

                self.close_focused_non_blocking_overlay();
                self.set_active_status(&req.call_id, ToolStatus::Confirm);

                let snapshot = smelt_core::cells::ConfirmRequested {
                    handle_id: 0,
                    tool_name: req.tool_name.clone(),
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
                    std::rc::Rc::new(smelt_core::cells::ConfirmRequested {
                        handle_id,
                        ..snapshot
                    }),
                );
                self.lua.fire_confirm_open(handle_id);
                true
            }
        }
    }
}

/// Reason an API-key env lookup failed; carries enough context for the
/// caller to surface a user-facing error.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApiKeyError {
    /// The env var is not set in the process environment.
    NotSet { var: String },
    /// The env var exists but its bytes are not valid Unicode.
    NotUnicode { var: String },
}

impl ApiKeyError {
    pub(crate) fn message(&self) -> String {
        match self {
            ApiKeyError::NotSet { var } => format!(
                "environment variable '{var}' is not set but is required for API authentication"
            ),
            ApiKeyError::NotUnicode { var } => format!(
                "environment variable '{var}' contains non-Unicode data and cannot be used as an API key"
            ),
        }
    }
}

/// Look up an API key value via the injected `get_env` resolver.
///
/// An empty `key_env` is treated as "no auth required" and returns an empty
/// key. Otherwise the resolver is queried; `VarError::NotPresent` and
/// `NotUnicode` map to typed errors so the dispatcher can format a stable
/// user-facing message.
///
/// The resolver indirection keeps tests off the process-global env.
pub(crate) fn lookup_api_key(
    key_env: &str,
    get_env: impl FnOnce(&str) -> Result<String, std::env::VarError>,
) -> Result<String, ApiKeyError> {
    if key_env.is_empty() {
        return Ok(String::new());
    }
    match get_env(key_env) {
        Ok(key) => Ok(key),
        Err(std::env::VarError::NotPresent) => Err(ApiKeyError::NotSet {
            var: key_env.to_string(),
        }),
        Err(std::env::VarError::NotUnicode(_)) => Err(ApiKeyError::NotUnicode {
            var: key_env.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_key_env_returns_empty_string_without_consulting_environment() {
        // Callers configured with no API key (e.g. local-only models) hit
        // this path; the resolver must never be invoked.
        let mut called = false;
        let out = lookup_api_key("", |_| {
            called = true;
            Ok("nope".into())
        });
        assert_eq!(out, Ok(String::new()));
        assert!(!called, "resolver should be skipped for empty key_env");
    }

    #[test]
    fn nonempty_key_env_returns_resolved_value() {
        let out = lookup_api_key("MY_KEY", |var| {
            assert_eq!(var, "MY_KEY");
            Ok("secret".into())
        });
        assert_eq!(out, Ok("secret".into()));
    }

    #[test]
    fn missing_env_var_maps_to_not_set_error_with_var_name() {
        let out = lookup_api_key("MY_KEY", |_| Err(std::env::VarError::NotPresent));
        assert_eq!(
            out,
            Err(ApiKeyError::NotSet {
                var: "MY_KEY".into()
            })
        );
    }

    #[test]
    fn non_unicode_env_var_maps_to_not_unicode_error() {
        // `std::env::VarError::NotUnicode` carries an `OsString`; we don't
        // care about its payload — only the variant matters for our message.
        let out = lookup_api_key("MY_KEY", |_| {
            Err(std::env::VarError::NotUnicode(std::ffi::OsString::new()))
        });
        assert_eq!(
            out,
            Err(ApiKeyError::NotUnicode {
                var: "MY_KEY".into()
            })
        );
    }

    #[test]
    fn not_set_error_message_names_the_missing_var() {
        let msg = ApiKeyError::NotSet {
            var: "OPENAI_API_KEY".into(),
        }
        .message();
        assert!(msg.contains("OPENAI_API_KEY"));
        assert!(msg.contains("not set"));
    }

    #[test]
    fn not_unicode_error_message_names_the_var() {
        let msg = ApiKeyError::NotUnicode {
            var: "WEIRD_KEY".into(),
        }
        .message();
        assert!(msg.contains("WEIRD_KEY"));
        assert!(msg.contains("non-Unicode"));
    }
}
