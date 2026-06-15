use super::*;

#[test]
fn ask_user_question_multiple_questions_wakes_between_dialogs() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert(
        "questions".into(),
        serde_json::json!([
            {
                "header": "First",
                "question": "Pick first?",
                "options": [
                    { "label": "One", "description": "first option" },
                    { "label": "Two", "description": "second option" }
                ],
                "multiSelect": false
            },
            {
                "header": "Second",
                "question": "Pick second?",
                "options": [
                    { "label": "Three", "description": "third option" },
                    { "label": "Four", "description": "fourth option" }
                ],
                "multiSelect": false
            }
        ]),
    );

    app.feed_one(SourceEvent::Engine(EngineEvent::ToolDispatch {
        request_id: 77,
        call_id: "aq-questions".into(),
        tool_name: "ask_user_question".into(),
        args,
    }));

    let first = app
        .state()
        .focused_overlay
        .expect("first question dialog should open");

    app.press(KeyCode::Enter);
    assert!(
        app.app.lua_wakeup_rx.try_recv().is_ok(),
        "resolving the first dialog should wake the Lua task runtime"
    );
    app.feed_one(SourceEvent::LuaWakeup);

    let second = app
        .state()
        .focused_overlay
        .expect("second question dialog should open after first answer");
    assert_ne!(first, second);

    app.press(KeyCode::Char('2'));
    assert!(
        app.app.lua_wakeup_rx.try_recv().is_ok(),
        "resolving the final dialog should wake the Lua task runtime"
    );
    app.feed_one(SourceEvent::LuaWakeup);

    assert!(app.state().focused_overlay.is_none());
    let result = app
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::ToolResult {
                    request_id,
                    call_id,
                    content,
                    is_error,
                    ..
                } => Some((*request_id, call_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            },
            _ => None,
        })
        .next_back()
        .expect("ask_user_question should send a tool result");

    assert_eq!(result.0, 77);
    assert_eq!(result.1, "aq-questions");
    assert_eq!(
        result.2,
        "Q: Pick first?\nA: One\n\nQ: Pick second?\nA: Four"
    );
    assert!(!result.3);
}

#[test]
fn colon_in_non_vim_mode_does_not_open_cmdline() {
    let mut app = TestApp::builder().build();
    app.type_char(':');
    let s = app.state();
    assert!(!s.cmdline_open);
    assert_eq!(s.prompt_text, ":");
}

#[test]
fn colon_in_vim_insert_mode_does_not_open_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    // Fresh prompt is already in Insert.
    assert_eq!(app.state().vim_mode, VimMode::Insert);

    app.type_char(':');
    let s = app.state();
    assert!(!s.cmdline_open);
    assert_eq!(s.prompt_text, ":");
}

#[test]
fn colon_in_vim_normal_mode_opens_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    // Drop to Normal first since the prompt starts in Insert.
    app.press(KeyCode::Esc);
    app.type_char(':');
    let s = app.state();
    assert!(s.cmdline_open);
    assert_eq!(s.cmdline_text, "");
}

#[test]
fn esc_closes_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    assert!(app.state().cmdline_open);

    app.press(KeyCode::Esc);
    assert!(!app.state().cmdline_open);
}

#[test]
fn cmdline_quit_command_requests_quit() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    app.type_text("quit");
    app.press(KeyCode::Enter);
    assert!(app.state().pending_quit);
}

#[test]
fn cmdline_paste_inserts_single_line_payload() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');

    app.feed_one(SourceEvent::Term(Event::Paste("hello\nworld".into())));

    let s = app.state();
    assert!(s.cmdline_open);
    assert_eq!(s.cmdline_text, "hello world");
}

#[test]
fn cmdline_unicode_editing_uses_byte_safe_cursor() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    app.feed_one(SourceEvent::Term(Event::Paste("a日本b".into())));
    app.press(KeyCode::Left);
    app.press(KeyCode::Backspace);

    let s = app.state();
    assert_eq!(s.cmdline_text, "a日b");
    app.assert_invariants();
}

#[test]
fn cmdline_typed_payload_does_not_trip_cursor_invariant() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    // Type more than any single-line buffer could ever encode in
    // source byte arithmetic from cell position alone.
    app.type_text("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    app.assert_invariants();
}

#[test]
fn btw_command_preserves_model_history_prefix_and_appends_question() {
    let mut app = TestApp::builder().build();
    stub_btw_ui(&mut app);
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u2")));

    let expected_prefix = protocol::history_to_messages(&app.app.model_history());

    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.apply_lua_command("btw what changed?");
        app.app.drive_lua_tasks();
    }

    let asks = ask_messages(app.drain_engine_sends());
    assert_eq!(asks.len(), 1, "/btw should issue one inherited ask");
    let (system, messages) = &asks[0];
    assert_eq!(system, &app.app.assemble_system_prompt());
    assert_eq!(
        &messages[..expected_prefix.len()],
        expected_prefix.as_slice(),
        "/btw must preserve the exact model-visible prefix"
    );
    let last_text = messages
        .last()
        .and_then(|m| m.content.as_ref())
        .map(|c| c.text_content())
        .expect("/btw question");
    assert!(last_text.contains("Under no circumstances use tools"));
    assert!(last_text.contains("Question: what changed?"));

    respond_pending_ask_with_text(&mut app, "done");
    app.app.core.timers.clear();
}

#[test]
fn btw_command_denies_tool_calls_then_retries_same_request_shape() {
    let mut app = TestApp::builder().build();
    stub_btw_ui(&mut app);
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    let expected_prefix = protocol::history_to_messages(&app.app.model_history());

    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.apply_lua_command("btw quick summary");
        app.app.drive_lua_tasks();
    }

    let first = ask_messages(app.drain_engine_sends());
    assert_eq!(first.len(), 1);
    let first_messages = first[0].1.clone();

    respond_pending_ask_with_tool_call(&mut app, "call-1", "grep");

    let second = ask_messages(app.drain_engine_sends());
    assert_eq!(second.len(), 1);
    let second_messages = &second[0].1;
    assert_eq!(
        &second_messages[..first_messages.len()],
        first_messages.as_slice(),
        "/btw tool denial retry must keep the same request prefix"
    );
    assert_eq!(
        &second_messages[..expected_prefix.len()],
        expected_prefix.as_slice(),
        "/btw tool denial retry must keep the same inherited conversation prefix"
    );
    assert_eq!(
        second_messages[first_messages.len()].role,
        protocol::Role::Assistant
    );
    assert_eq!(
        second_messages[first_messages.len() + 1].role,
        protocol::Role::Tool
    );
    assert!(second_messages[first_messages.len() + 1].is_error);

    respond_pending_ask_with_text(&mut app, "done");
    app.app.core.timers.clear();
}
