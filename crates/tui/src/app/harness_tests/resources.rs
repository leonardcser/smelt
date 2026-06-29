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
fn api_base_endpoint_warning_is_persistent_and_deduped() {
    let mut app = TestApp::builder().build();
    app.app.core.config.provider_type = "openai-compatible".into();
    app.app.core.config.api_base = "https://api.cerebras.ai/v1/chat/completions".into();

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
fn engine_ask_probe_registers_pending_callback() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_engine_ask_probe("summarize this");
    assert!(app.pending_ask_id().is_some());
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
