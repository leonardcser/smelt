use super::*;

#[test]
fn builds_a_fresh_test_app() {
    let app = TestApp::builder().build();
    let s = app.state();
    assert!(!s.cmdline_open);
    assert!(!s.quit_requested);
    assert!(!s.agent_running);
    assert_eq!(s.app_focus, AppFocus::Prompt);
    assert!(s.queued_inputs.is_empty());
}

#[test]
fn no_model_state_is_explicit_and_blocks_dispatch() {
    let mut app = TestApp::builder().with_vim(false).without_model().build();
    let initial_history_len = app.app.core.session.history.len();

    assert!(app.run_lua(
        r#"
            assert(smelt.model.current() == nil)
            local status = smelt.model.status()
            assert(status.current == nil)
            assert(status.requested == nil)
            assert(status.availability == "none")
            assert(smelt.signal.get("model") == nil)
            assert(smelt.config.provider_type() == nil)
            assert(smelt.config.api_base() == nil)
            assert(smelt.config.api_key_env() == nil)
            assert(smelt.config.model_config() == nil)
            assert(smelt.model.transport() == nil)
            assert(smelt.model.capabilities() == nil)
        "#,
    ));

    app.press(KeyCode::F(3));
    assert!(app.render_to_frame().text().contains("debug"));
    app.press(KeyCode::Esc);

    app.type_text("hello without a model");
    app.press(KeyCode::Enter);

    assert_eq!(app.state().prompt_text, "hello without a model");
    assert!(!app.state().agent_running);
    assert_eq!(app.app.core.session.history.len(), initial_history_len);
    assert!(app.app.notification_win().is_some());
    assert!(app.actions().iter().all(|action| {
        !matches!(
            action,
            Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )
    }));
}

#[test]
fn queued_input_is_retained_until_a_model_is_usable() {
    let mut app = TestApp::builder().without_model().build();
    app.push_queued_message("wait for a model".into());

    assert!(app.app.start_next_queued_input_if_idle());

    assert_eq!(app.state().queued_inputs, vec!["wait for a model"]);
    assert!(!app.state().agent_running);
    assert!(app.actions().iter().all(|action| {
        !matches!(
            action,
            Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )
    }));
}

#[test]
fn unavailable_model_is_reported_and_blocks_dispatch() {
    let mut app = TestApp::builder().build();
    app.app.core.config.active_model_mut().unwrap().availability =
        smelt_core::ModelAvailability::Unavailable {
            reason: smelt_core::ModelUnavailableReason::MissingCredentials,
        };

    assert!(app.run_lua(
        r#"
            assert(smelt.model.current() == "test/test-model")
            local status = smelt.model.status()
            assert(status.current == "test/test-model")
            assert(status.requested == "test/test-model")
            assert(status.availability == "unavailable")
            assert(status.reason == "missing_credentials")
        "#,
    ));

    app.type_text("do not dispatch");
    app.press(KeyCode::Enter);

    assert_eq!(app.state().prompt_text, "do not dispatch");
    assert!(!app.state().agent_running);
    assert!(app.actions().iter().all(|action| {
        !matches!(
            action,
            Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )
    }));
}

#[test]
fn model_switch_marks_missing_credentials_unavailable() {
    let mut app = TestApp::builder().build();
    app.app
        .core
        .config
        .available_models
        .push(smelt_core::config::ResolvedModel {
            key: "missing/credentials".into(),
            provider_name: "missing".into(),
            model_name: "credentials".into(),
            api_base: "https://example.invalid/v1".into(),
            api_key_env: "SMELT_TEST_INTENTIONALLY_MISSING_MODEL_KEY_7F31A9".into(),
            provider_type: "openai-compatible".into(),
            config: protocol::ModelConfig::default(),
        });

    app.app.apply_model("missing/credentials", true);

    assert!(matches!(
        app.app
            .core
            .config
            .active_model()
            .map(|model| &model.availability),
        Some(smelt_core::ModelAvailability::Unavailable {
            reason: smelt_core::ModelUnavailableReason::MissingCredentials,
        })
    ));
    assert!(app.run_lua(
        r#"
        local status = smelt.model.status()
        assert(status.current == "missing/credentials")
        assert(status.availability == "unavailable")
        assert(status.reason == "missing_credentials")
        "#,
    ));
}

#[test]
fn api_base_endpoint_warning_is_persistent_and_deduped() {
    let mut app = TestApp::builder().build();
    let active = app.app.core.config.active_model_mut().unwrap();
    active.provider_type = "openai-compatible".into();
    active.api_base = "https://api.cerebras.ai/v1/chat/completions".into();

    app.app.warn_if_api_base_normalized();
    app.app.warn_if_api_base_normalized();

    let messages = app.app.lua.core_shared().messages.lock().unwrap();
    let entries: Vec<_> = messages
        .entries()
        .iter()
        .filter(|entry| entry.full.contains("api_base includes /chat/completions"))
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, smelt_core::messages::MessageKind::Warning);
    assert_eq!(entries[0].source, "config");
    assert!(entries[0]
        .full
        .contains("using https://api.cerebras.ai/v1 instead"));
}

#[test]
fn feed_one_records_alloc_delta_with_tick_near_zero() {
    let mut app = TestApp::builder().build();
    assert!(app.last_alloc_delta().is_none());
    app.feed_one(SourceEvent::Tick(10));
    let delta = app.last_alloc_delta().expect("delta after first event");
    assert!(
        delta.allocs < 32,
        "Tick should allocate near zero, got {} allocs / {} bytes",
        delta.allocs,
        delta.bytes_grown
    );
}

#[test]
fn keystroke_stays_within_default_alloc_budget() {
    let mut app = TestApp::builder().build();
    // Warm caches with a discarded first keystroke so this test
    // measures steady-state cost, not first-event init.
    app.type_char('a');
    app.feed_one_within_budget(
        SourceEvent::Term(Event::Key(KeyEvent {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })),
        AllocBudget::DEFAULT,
    );
    let delta = app.last_alloc_delta().expect("delta recorded");
    // Print observed steady-state cost so it's visible in `cargo test
    // -- --nocapture` runs and during budget-tuning sweeps.
    eprintln!(
        "steady-state keystroke delta: {} allocs / {} bytes",
        delta.allocs, delta.bytes_grown
    );
}

#[test]
fn skill_backed_commands_submit_skill_body_and_focus() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.app.core.skills = Some(std::sync::Arc::new(engine::SkillLoader::load(&[])));
    app.type_text("/reflect focus area");
    app.press(KeyCode::Enter);
    let payload = app
        .actions()
        .iter()
        .find_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::StartTurn(payload) => Some((**payload).clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("/reflect should start a turn");
    let content = payload.input.provider_content();
    let text = content.text_content();
    assert!(text.contains("# Reflect"));
    assert!(text.contains("## Additional Focus\n\nfocus area"));
    assert!(text.contains("<skill name=\"reflect\" included_by=\"smelt\""));
}

#[test]
fn custom_command_with_non_scalar_description_still_registers() {
    let dir = tempfile::tempdir().unwrap();
    let command_path = dir.path().join("security-audit.md");
    std::fs::write(
        &command_path,
        "---\ndescription:\n  audit the repository for malicious code\n  and data exfiltration\ntools:\n  allow: [read_file, glob, grep]\n---\n\nAudit the repository",
    )
    .unwrap();

    let mut app = TestApp::builder().with_vim(false).build();
    let dir_lua = format!("{:?}", dir.path().display().to_string());
    assert!(app.run_lua(&format!(
        "require('smelt.commands.custom_commands').register_dir({dir_lua})"
    )));
    let command_registered = app
        .app
        .lua
        .shared()
        .commands
        .lock()
        .unwrap()
        .contains_key("security-audit");
    assert!(command_registered, "security-audit command should register");
}

#[tokio::test(flavor = "current_thread")]
async fn custom_command_shell_output_is_marked_as_smelt_context() {
    let dir = tempfile::tempdir().unwrap();
    let command_path = dir.path().join("probe.md");
    std::fs::write(
        &command_path,
        "---\ndescription: probe\n---\n\nBefore\n\n```!\nprintf smelt-provenance\n```\n\nAfter",
    )
    .unwrap();

    let mut app = TestApp::builder().with_vim(false).build();
    let dir_lua = format!("{:?}", dir.path().display().to_string());
    assert!(app.run_lua(
        "smelt.process.run = function() return { stdout = 'smelt-provenance', stderr = '', exit_code = 0 } end"
    ));
    assert!(app.run_lua(&format!(
        "require('smelt.commands.custom_commands').register_dir({dir_lua})"
    )));
    app.type_text("/probe");
    app.press(KeyCode::Enter);
    for _ in 0..10 {
        tokio::task::yield_now().await;
        app.feed_one(SourceEvent::LuaWakeup);
        if app
            .actions()
            .iter()
            .any(|action| matches!(action, Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(_))))
        {
            break;
        }
    }

    let payload = app
        .actions()
        .iter()
        .find_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::StartTurn(payload) => Some((**payload).clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("/probe should start a turn");
    let content = payload.input.provider_content();
    let text = content.text_content();
    assert!(text.contains("<command_output "));
    assert!(text.contains("executed_by=\"smelt\""));
    assert!(text.contains("source=\"custom_command\""));
    assert!(text.contains("command=\"printf smelt-provenance\""));
    assert!(text.contains("smelt-provenance\n</command_output>"));
}

#[test]
fn explicit_model_switch_sends_complete_target_only_for_an_active_turn() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.app
        .core
        .config
        .available_models
        .push(smelt_core::config::ResolvedModel {
            key: "other/switched".into(),
            provider_name: "other".into(),
            model_name: "switched".into(),
            api_base: "https://switch.example/v1".into(),
            api_key_env: String::new(),
            provider_type: "anthropic".into(),
            config: protocol::ModelConfig {
                context_window: Some(200_000),
                tool_calling: Some(false),
                ..Default::default()
            },
        });
    let _ = app.drain_engine_sends();

    app.app.apply_model("other/switched", false);
    assert!(app
        .drain_engine_sends()
        .into_iter()
        .all(|cmd| !matches!(cmd, protocol::UiCommand::SetTurnModel { .. })));

    app.app.apply_model("test/test-model", false);
    let _ = app.drain_engine_sends();
    app.start_turn(1);
    app.app.apply_model("other/switched", false);
    let command = app
        .drain_engine_sends()
        .into_iter()
        .find(|cmd| matches!(cmd, protocol::UiCommand::SetTurnModel { .. }))
        .expect("active turn model switch should notify the engine");
    match command {
        protocol::UiCommand::SetTurnModel { target } => {
            assert_eq!(target.model, "switched");
            assert_eq!(target.api_base, "https://switch.example/v1");
            assert_eq!(target.provider_type, "anthropic");
            assert_eq!(target.config.context_window, Some(200_000));
            assert_eq!(target.config.tool_calling, Some(false));
        }
        _ => unreachable!(),
    }
}

#[test]
fn custom_command_override_dispatches_its_complete_target_and_request_config() {
    let mut app = TestApp::builder().with_vim(false).build();
    let active = app.app.core.config.active_model_mut().unwrap();
    active.provider_type = "openai".into();
    active.config = protocol::ModelConfig {
        input_cost: Some(99.0),
        ..Default::default()
    };
    app.app.core.config.settings.redact_secrets = true;
    app.app.core.config.settings.cache_ttl_long = true;
    app.app.core.config.available_models = vec![smelt_core::config::ResolvedModel {
        key: "other/target-model".into(),
        provider_name: "other".into(),
        model_name: "target-model".into(),
        api_base: "https://other.example/v1".into(),
        api_key_env: String::new(),
        provider_type: "anthropic-compatible".into(),
        config: protocol::ModelConfig {
            temperature: Some(0.1),
            output_cost: Some(3.0),
            tool_calling: Some(false),
            ..Default::default()
        },
    }];
    let _ = app.drain_engine_sends();

    let turn = app
        .app
        .begin_command_request_turn(
            "custom".into(),
            "body".into(),
            smelt_core::custom_commands::CommandOverrides {
                model: Some("other/target-model".into()),
                temperature: Some(0.8),
                ..Default::default()
            },
            crate::app::CommandTurnStart::Fresh,
        )
        .expect("custom command target resolves");
    app.app.agent = Some(turn);

    let payload = app
        .drain_engine_sends()
        .into_iter()
        .find_map(|cmd| match cmd {
            protocol::UiCommand::StartTurn(payload) => Some(*payload),
            _ => None,
        })
        .expect("custom command should send StartTurn");
    assert_eq!(payload.model_target.model, "target-model");
    assert_eq!(payload.model_target.api_base, "https://other.example/v1");
    assert_eq!(payload.model_target.provider_type, "anthropic-compatible");
    assert_eq!(payload.model_target.config.temperature, Some(0.8));
    assert_eq!(payload.model_target.config.output_cost, Some(3.0));
    assert_eq!(payload.model_target.config.input_cost, None);
    assert_eq!(payload.model_target.config.tool_calling, Some(false));
    assert!(payload.request_config.redact_secrets);
    assert!(payload.request_config.cache_ttl_long);
}

#[test]
fn custom_command_turn_includes_registered_lua_tools() {
    let mut app = TestApp::builder().with_vim(false).build();
    let payload = app
        .start_custom_command_with_lua_tool(0)
        .expect("custom command should send StartTurn");
    assert!(
        payload.tools.iter().any(|t| t.name == "fuzz_custom_tool_0"),
        "registered Lua tool missing from custom command payload: {:?}",
        payload.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
}

#[test]
fn engine_ask_probe_dispatches_complete_target_and_request_config() {
    let mut app = TestApp::builder().with_vim(false).build();
    let active = app.app.core.config.active_model_mut().unwrap();
    active.model_name = "ask-model".into();
    active.api_base = "https://ask.example/v1".into();
    active.provider_type = "openai-compatible".into();
    active.config = protocol::ModelConfig {
        max_tokens: Some(1234),
        supports_reasoning: Some(true),
        ..Default::default()
    };
    app.app.core.config.settings.cache_ttl_long = true;
    app.app.core.config.settings.request_audit = "off".into();
    app.app.core.config.request_audit = protocol::RequestAuditMode::Full;
    let _ = app.drain_engine_sends();

    app.start_engine_ask_probe("summarize this");
    assert!(app.pending_ask_id().is_some());
    let command = app
        .actions()
        .iter()
        .rev()
        .find_map(|action| match action {
            Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::EngineAsk { .. }) =>
            {
                Some(command.as_ref().clone())
            }
            _ => None,
        })
        .expect("probe should send EngineAsk");
    match command {
        protocol::UiCommand::EngineAsk {
            target,
            request_config,
            ..
        } => {
            assert_eq!(target.model, "ask-model");
            assert_eq!(target.api_base, "https://ask.example/v1");
            assert_eq!(target.provider_type, "openai-compatible");
            assert_eq!(target.config.max_tokens, Some(1234));
            assert_eq!(target.config.supports_reasoning, Some(true));
            assert!(request_config.cache_ttl_long);
            assert_eq!(
                request_config.request_audit,
                protocol::RequestAuditMode::Full
            );
        }
        _ => unreachable!(),
    }
}

#[test]
fn ctrl_w_pane_chord_expires_after_tick_past_window() {
    let mut app = TestApp::builder().build();
    app.press_mod(KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert!(
        app.app.timers.pending_pane_chord.is_some(),
        "Ctrl-W arms the pane chord"
    );

    // 1000ms > PANE_CHORD_WINDOW (750ms).
    app.feed_one(SourceEvent::Tick(1000));
    // Follow-up key after expiry: handler drops the pending chord and
    // returns None so the key falls through to normal dispatch.
    app.press(KeyCode::Char('j'));
    assert!(
        app.app.timers.pending_pane_chord.is_none(),
        "expired pane chord should be cleared on the next key"
    );
}
