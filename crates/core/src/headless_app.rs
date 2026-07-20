//! Headless frontend (`smelt --headless`). No `Ui`, no buffers, no compositor.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use protocol::{Content, Decision, EngineEvent, UiCommand};

use super::headless::{HeadlessSink, OutputFormat};
use super::runtime::Core;

pub struct HeadlessApp {
    pub core: Core,
    pub(crate) sink: HeadlessSink,
    pub(crate) next_turn_id: u64,
    system_prompt: String,
    capabilities: engine::SystemPromptCapabilities,
    lua: Option<crate::lua::LuaRuntime>,
    lua_wakeup_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
}

impl HeadlessApp {
    pub fn new(
        core: Core,
        sink: HeadlessSink,
        system_prompt: String,
        capabilities: engine::SystemPromptCapabilities,
        lua: Option<crate::lua::LuaRuntime>,
    ) -> Self {
        let (lua, lua_wakeup_rx) = if let Some(lua) = lua {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            lua.set_wakeup_sender(tx);
            (Some(lua), Some(rx))
        } else {
            (None, None)
        };
        Self {
            core,
            sink,
            next_turn_id: 1,
            system_prompt,
            capabilities,
            lua,
            lua_wakeup_rx,
        }
    }

    fn model_target(&self) -> Option<protocol::ModelTarget> {
        let active = self.core.config.active_model()?;
        let api_key = if active.api_key_env.is_empty() {
            String::new()
        } else {
            std::env::var(&active.api_key_env).unwrap_or_default()
        };
        Some(active.target(api_key))
    }

    fn approves_permission(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> bool {
        let mode = self.core.config.mode.clone();
        let permissions = self.core.permissions.snapshot();
        permissions
            .evaluate_tool(
                mode.clone(),
                crate::permissions::ToolOrigin::Lua,
                tool_name,
                args,
            )
            .decision
            == Decision::Allow
            || permissions
                .evaluate_tool(mode, crate::permissions::ToolOrigin::Mcp, tool_name, args)
                .decision
                == Decision::Allow
    }

    fn tool_defs(&self) -> Vec<protocol::ToolDef> {
        if !self.capabilities.tool_calling {
            return Vec::new();
        }
        self.lua
            .as_ref()
            .map(|lua| {
                lua.tool_defs(
                    self.core.config.mode.clone(),
                    crate::lua::ToolVisibility::Headless,
                )
            })
            .unwrap_or_default()
    }

    fn handle_tool_evaluation_request(
        &mut self,
        request_id: u64,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
    ) {
        let Some(lua) = self.lua.as_ref() else {
            self.core.engine.send(UiCommand::ToolEvaluationResponse {
                request_id,
                evaluation: protocol::ToolEvaluation {
                    decision: protocol::Decision::Error(format!("tool not found: {tool_name}")),
                    metadata: protocol::ToolMetadata::default(),
                },
            });
            return;
        };
        if !lua.tool_available_for(&tool_name, crate::lua::ToolVisibility::Headless) {
            self.core.engine.send(UiCommand::ToolEvaluationResponse {
                request_id,
                evaluation: protocol::ToolEvaluation {
                    decision: protocol::Decision::Deny,
                    metadata: protocol::ToolMetadata::default(),
                },
            });
            return;
        }
        let metadata = lua.evaluate_tool_metadata(&tool_name, &args);
        let decision = if let Some(err) = metadata.preflight_error.clone() {
            protocol::Decision::Error(err)
        } else {
            self.core
                .permissions
                .snapshot()
                .evaluate_tool_with_approvals(
                    self.core.config.mode.clone(),
                    crate::permissions::ToolOrigin::Lua,
                    &tool_name,
                    &args,
                )
                .decision
        };
        self.core.engine.send(UiCommand::ToolEvaluationResponse {
            request_id,
            evaluation: protocol::ToolEvaluation { decision, metadata },
        });
    }

    fn handle_tool_dispatch(
        &mut self,
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
    ) {
        let Some(lua) = self.lua.as_ref() else {
            self.core.engine.send(UiCommand::ToolResult {
                request_id,
                call_id,
                content: format!("tool not found: {tool_name}"),
                is_error: true,
                metadata: None,
            });
            return;
        };
        if !lua.tool_available_for(&tool_name, crate::lua::ToolVisibility::Headless) {
            self.core.engine.send(UiCommand::ToolResult {
                request_id,
                call_id,
                content: format!("tool not available in headless mode: {tool_name}"),
                is_error: true,
                metadata: None,
            });
            return;
        }
        let mode = self.core.config.mode.clone();
        let session_id = self.core.session.id.clone();
        let session_dir = crate::session::dir_for(&self.core.session);
        match lua.execute_tool(
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
                self.core.engine.send(UiCommand::ToolResult {
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

    fn drive_lua_tasks(&mut self) {
        let Some(lua) = self.lua.as_ref() else {
            return;
        };
        lua.pump_task_events();
        for out in lua.drive_tasks(self.core.clock.instant_now()) {
            if let crate::lua::TaskDriveOutput::ToolComplete {
                invocation,
                call_id,
                content,
                is_error,
                metadata,
            } = out
            {
                self.core.engine.send(UiCommand::ToolResult {
                    request_id: invocation.request_id,
                    call_id,
                    content,
                    is_error,
                    metadata,
                });
            }
        }
    }

    fn next_lua_wakeup(&self) -> Option<std::time::Instant> {
        self.lua
            .as_ref()
            .and_then(|lua| lua.next_task_wakeup(self.core.clock.instant_now()))
    }

    fn handle_control_event(&mut self, ev: &EngineEvent) -> bool {
        match ev {
            EngineEvent::RequestPermission {
                request_id,
                tool_name,
                args,
                ..
            } => {
                let approved = self.approves_permission(tool_name, args);
                self.core.engine.send(UiCommand::PermissionDecision {
                    request_id: *request_id,
                    approved,
                    message: None,
                });
                true
            }
            EngineEvent::ToolEvaluationRequest {
                request_id,
                tool_name,
                args,
                ..
            } => {
                self.handle_tool_evaluation_request(*request_id, tool_name.clone(), args.clone());
                true
            }
            EngineEvent::ToolDispatch {
                request_id,
                call_id,
                tool_name,
                args,
            } => {
                self.handle_tool_dispatch(
                    *request_id,
                    call_id.clone(),
                    tool_name.clone(),
                    args.clone(),
                );
                true
            }
            EngineEvent::CoreToolResult {
                request_id,
                content,
                is_error,
                metadata,
            } => {
                if let Some(lua) = self.lua.as_ref() {
                    lua.resolve_core_tool_call(
                        *request_id,
                        content.clone(),
                        *is_error,
                        metadata.clone(),
                    );
                }
                true
            }
            _ => false,
        }
    }

    async fn wait_for_lua_wakeup(
        rx: Option<&mut tokio::sync::mpsc::UnboundedReceiver<()>>,
        wakeup: Option<std::time::Instant>,
    ) {
        match (rx, wakeup) {
            (Some(rx), Some(at)) => {
                tokio::select! {
                    _ = rx.recv() => {}
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(at)) => {}
                }
            }
            (Some(rx), None) => {
                let _ = rx.recv().await;
            }
            (None, Some(at)) => {
                tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
            }
            (None, None) => std::future::pending::<()>().await,
        }
    }

    /// Send `message`, drain engine events, print assistant text + token/cost
    /// summary, then exit. Ctrl-C sends `UiCommand::Cancel` and exits 130.
    pub async fn run_oneshot(&mut self, message: String, cancel: Arc<tokio::sync::Notify>) {
        use std::io::Write;

        let trimmed = message.trim();

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

        if crate::commands::command_name(trimmed).is_some() {
            eprintln!("\"{}\" requires interactive mode", trimmed);
            std::process::exit(1);
        }

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        let cwd_path = self.core.env.cwd();
        let cwd = cwd_path.to_string_lossy().into_owned();
        let content = crate::file_ref::expand_at_file_refs(&message, &cwd, &self.core.files);
        let mut history = self.core.session.history.clone();
        history.push(protocol::HistoryItem::note(protocol::HistoryNote::context(
            crate::context_notes::cwd_note(
                &cwd_path,
                std::path::Path::new(&self.core.config.settings.worktree_root),
            ),
        )));

        let tools = self.tool_defs();
        let Some(model_target) = self.model_target() else {
            eprintln!("error: no model is available for headless dispatch");
            return;
        };
        let fast_mode = model_target.provider_type == "codex"
            && model_target.config.supports_fast_mode == Some(true)
            && self
                .core
                .session
                .fast_mode
                .unwrap_or(self.core.config.settings.fast_mode);

        self.core
            .engine
            .send(UiCommand::StartTurn(Box::new(protocol::StartTurnPayload {
                turn_id,
                input: protocol::StartTurnInput::user(Content::text(content)),
                mode: self.core.config.mode.clone(),
                model_target,
                request_config: self.core.config.request_runtime_config(),
                reasoning_effort: self.core.config.reasoning_effort,
                fast_mode,
                history: protocol::ModelHistorySource::items(history),
                session_id: self.core.session.id.clone(),
                session_dir: crate::session::dir_for(&self.core.session),
                persistence: protocol::PersistenceScope::default(),
                permission_overrides: None,
                system_prompt: Some(self.system_prompt.clone()),
                tools,
            })));

        let mut final_message = String::new();
        let mut total_usage = protocol::TokenUsage::default();
        let mut last_tps: Option<f64> = None;
        let mut total_cost = 0.0_f64;
        let mut pending_tools: HashMap<String, (String, String, String)> = HashMap::new();

        let mut interrupted = false;
        loop {
            self.drive_lua_tasks();
            let wakeup = self.next_lua_wakeup();
            let ev = tokio::select! {
                biased;
                _ = cancel.notified() => {
                    self.core.engine.send(protocol::UiCommand::Cancel);
                    interrupted = true;
                    break;
                }
                ev = self.core.engine.recv() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
                _ = Self::wait_for_lua_wakeup(self.lua_wakeup_rx.as_mut(), wakeup), if self.lua.is_some() => {
                    continue;
                }
            };
            if self.sink.format == OutputFormat::Json {
                self.sink.emit_json(&ev);
            }

            if self.handle_control_event(&ev) {
                continue;
            }

            match &ev {
                EngineEvent::ReasoningPartStarted { .. }
                | EngineEvent::ReasoningPartDelta { .. }
                | EngineEvent::ReasoningPartFinished { .. } => {}
                EngineEvent::Reasoning { content, .. }
                    if self.sink.format == OutputFormat::Text =>
                {
                    self.sink.log_thinking(content);
                }
                EngineEvent::TextDelta { delta } => {
                    final_message.push_str(delta);
                }
                EngineEvent::Text { content } => {
                    final_message = content.clone();
                }
                EngineEvent::ToolStarted {
                    call_id,
                    tool_name,
                    args,
                } => {
                    let mut arg_keys: Vec<String> = args.keys().cloned().collect();
                    arg_keys.sort();
                    let summary = format!("{tool_name}({})", arg_keys.join(", "));
                    pending_tools
                        .insert(call_id.clone(), (tool_name.clone(), summary, String::new()));
                }
                EngineEvent::ToolOutput { call_id, chunk }
                    if self.sink.format == OutputFormat::Text && self.sink.verbose =>
                {
                    if let Some((_, _, output)) = pending_tools.get_mut(call_id) {
                        output.push_str(chunk);
                    }
                }
                EngineEvent::ToolFinished {
                    call_id,
                    result,
                    elapsed_ms,
                } => {
                    let (name, summary, output) = pending_tools.remove(call_id).unwrap_or_default();
                    if self.sink.format == OutputFormat::Text {
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
                            *elapsed_ms,
                        );
                    }
                }
                EngineEvent::TokenUsage {
                    usage,
                    tokens_per_sec,
                    cost_usd,
                    ..
                } => {
                    total_cost += cost_usd.unwrap_or(0.0);
                    total_usage.accumulate(usage);
                    last_tps = tokens_per_sec.or(last_tps);
                }
                EngineEvent::Retrying { delay_ms, attempt }
                    if self.sink.format == OutputFormat::Text =>
                {
                    self.sink.log_retry(*attempt, *delay_ms);
                }
                EngineEvent::HistoryUpdated { .. } => {}
                EngineEvent::RequestAuditError { message }
                    if self.sink.format == OutputFormat::Text =>
                {
                    self.sink.log_error(message);
                }
                EngineEvent::TurnError { message, .. } => {
                    if self.sink.format == OutputFormat::Text {
                        self.sink.log_error(message);
                    }
                    break;
                }
                EngineEvent::TurnComplete { .. } => break,
                _ => {}
            }
        }

        if self.sink.format == OutputFormat::Text {
            self.sink
                .log_token_usage(&total_usage, last_tps, total_cost);
        }

        if self.sink.format == OutputFormat::Text && !final_message.is_empty() {
            use std::io::IsTerminal;
            let stdout_is_tty = std::io::stdout().is_terminal();
            let stderr_is_tty = std::io::stderr().is_terminal();

            if stdout_is_tty && stderr_is_tty {
                // Both TTY: print to stderr so the answer appears after tool output.
                eprintln!();
                eprint!("{final_message}");
                if !final_message.ends_with('\n') {
                    eprintln!();
                }
            } else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config, permissions, FrontendKind, RuntimeState, StartupOverrides};

    fn runtime_state(tool_calling: bool) -> RuntimeState {
        let config = config::Config {
            providers: vec![config::ProviderConfig {
                name: Some("test".into()),
                provider_type: Some("openai".into()),
                api_base: Some("http://example.invalid".into()),
                api_key_env: Some("SMELT_TEST_KEY".into()),
                models: vec![protocol::ModelConfig {
                    name: Some("test-model".into()),
                    tool_calling: Some(tool_calling),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        let models = config.resolve_models();
        crate::resolve_runtime(crate::RuntimeInputs {
            config: &config,
            startup: &StartupOverrides::default(),
            available_models: &models,
            registered_modes: &[],
            selections: &crate::RuntimeSelections::default(),
            previous: None,
            headless: true,
        })
        .unwrap()
    }

    fn lua_with_probe_tools() -> crate::lua::LuaRuntime {
        let lua = crate::lua::LuaRuntime::new();
        lua.lua
            .load(
                r#"
                smelt.tools.register({
                    name = "headless_probe",
                    description = "test tool",
                    parameters = { type = "object", properties = {} },
                    execute = function(args) return "ok" end,
                })
                smelt.tools.register({
                    name = "ui_only_probe",
                    description = "ui only",
                    parameters = { type = "object", properties = {} },
                    headless = false,
                    execute = function(args) return "ok" end,
                })
                "#,
            )
            .exec()
            .unwrap();
        lua
    }

    fn headless_app(tool_calling: bool) -> HeadlessApp {
        headless_app_with_cmd_rx(tool_calling).0
    }

    fn headless_app_with_cmd_rx(
        tool_calling: bool,
    ) -> (HeadlessApp, tokio::sync::mpsc::UnboundedReceiver<UiCommand>) {
        let (engine, cmd_rx, _event_tx) = engine::EngineHandle::for_test();
        let clock: Arc<dyn engine::clock::Clock> = Arc::new(engine::clock::RealClock);
        let env = Arc::new(engine::env::RuntimeEnv::snapshot());
        let core = Core::new(
            runtime_state(tool_calling),
            StartupOverrides::default(),
            engine,
            FrontendKind::Headless,
            permissions::PermissionsHandle::new(permissions::Permissions::load()),
            clock,
            env,
        );
        let capabilities = engine::SystemPromptCapabilities::from_tool_calling(tool_calling);
        let lua = tool_calling.then(lua_with_probe_tools);
        (
            HeadlessApp::new(
                core,
                HeadlessSink::new(OutputFormat::Json, crate::ColorMode::Never, false),
                "system".into(),
                capabilities,
                lua,
            ),
            cmd_rx,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn headless_startup_dispatches_complete_target_and_request_config() {
        let (engine, mut cmd_rx, event_tx) = engine::EngineHandle::for_test();
        let clock: Arc<dyn engine::clock::Clock> = Arc::new(engine::clock::RealClock);
        let env = Arc::new(engine::env::RuntimeEnv::snapshot());
        let mut config = runtime_state(false);
        let active = config.active_model_mut().unwrap();
        active.api_key_env.clear();
        active.config.max_tokens = Some(3210);
        config.settings.redact_secrets = true;
        config.settings.cache_ttl_long = true;
        let core = Core::new(
            config,
            StartupOverrides::default(),
            engine,
            FrontendKind::Headless,
            permissions::PermissionsHandle::new(permissions::Permissions::load()),
            Arc::clone(&clock),
            env,
        );
        let mut app = HeadlessApp::new(
            core,
            HeadlessSink::new(OutputFormat::Text, crate::ColorMode::Never, false),
            "system".into(),
            engine::SystemPromptCapabilities::from_tool_calling(false),
            None,
        );
        let cancel = Arc::new(tokio::sync::Notify::new());

        let assert_request = async move {
            let command = cmd_rx.recv().await.expect("headless StartTurn");
            let UiCommand::StartTurn(payload) = command else {
                panic!("expected StartTurn");
            };
            assert_eq!(payload.model_target.model, "test-model");
            assert_eq!(payload.model_target.api_base, "http://example.invalid");
            assert_eq!(payload.model_target.provider_type, "openai");
            assert_eq!(payload.model_target.config.max_tokens, Some(3210));
            assert_eq!(payload.model_target.config.tool_calling, Some(false));
            assert!(payload.request_config.redact_secrets);
            assert!(payload.request_config.cache_ttl_long);
            event_tx
                .send(EngineEvent::TurnComplete {
                    turn_id: 1,
                    history: None,
                    meta: None,
                })
                .unwrap();
        };

        tokio::join!(app.run_oneshot("hello".into(), cancel), assert_request);
    }

    #[test]
    fn headless_tool_defs_follow_tool_calling_capability() {
        let enabled = headless_app(true);
        let disabled = headless_app(false);

        let enabled_names: Vec<_> = enabled.tool_defs().iter().map(|t| t.name.clone()).collect();
        assert!(enabled_names.contains(&"headless_probe".to_string()));
        assert!(!enabled_names.contains(&"ui_only_probe".to_string()));
        assert!(disabled.tool_defs().is_empty());
    }

    #[test]
    fn headless_denies_ui_only_tool_evaluation() {
        let (mut app, mut cmd_rx) = headless_app_with_cmd_rx(true);
        app.handle_tool_evaluation_request(7, "ui_only_probe".into(), HashMap::new());

        match cmd_rx.try_recv().unwrap() {
            UiCommand::ToolEvaluationResponse {
                request_id,
                evaluation,
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(evaluation.decision, protocol::Decision::Deny);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn headless_rejects_ui_only_tool_dispatch() {
        let (mut app, mut cmd_rx) = headless_app_with_cmd_rx(true);
        app.handle_tool_dispatch(7, "call-1".into(), "ui_only_probe".into(), HashMap::new());

        match cmd_rx.try_recv().unwrap() {
            UiCommand::ToolResult {
                request_id,
                call_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(call_id, "call-1");
                assert!(is_error);
                assert!(content.contains("not available in headless mode"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
