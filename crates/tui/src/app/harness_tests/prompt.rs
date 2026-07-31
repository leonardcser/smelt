use super::*;

#[test]
fn prompt_prediction_tab_accepts_after_non_path_typing() {
    let mut app = TestApp::builder().build();

    // This first edit used to leave the manual path-completer Tab binding
    // installed, which swallowed the later prediction accept.
    app.type_text("ordinary prompt text");
    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    app.install_prompt_placeholder(
        "predicted follow-up".to_string(),
        vec![crate::smelt_edit::KeyBind::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )],
        Vec::new(),
    );

    app.press(KeyCode::Tab);

    assert_eq!(app.state().prompt_text, "predicted follow-up");
}

#[test]
fn slash_completion_tab_accepts_command_name() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"smelt.cmd.register("tab-accept-regression", function() end, { desc = "tab regression" })"#
    ));

    app.type_text("/tab-accept-regression");
    app.press(KeyCode::Tab);

    assert_eq!(app.state().prompt_text, "/tab-accept-regression ");
}

#[test]
fn partial_model_command_tab_then_enter_opens_model_picker() {
    let mut app = TestApp::builder().build();
    app.set_available_models(vec![smelt_core::config::ResolvedModel {
        key: "anthropic/claude-sonnet-4".into(),
        provider_name: "anthropic".into(),
        model_name: "claude-sonnet-4".into(),
        display_name: None,
        api_base: "https://api.anthropic.com".into(),
        api_key_env: "ANTHROPIC_API_KEY".into(),
        provider_type: "anthropic".into(),
        config: protocol::ModelConfig::default(),
    }]);

    app.type_text("/mode");
    app.press(KeyCode::Tab);
    assert_eq!(app.state().prompt_text, "/model ");

    app.press(KeyCode::Enter);

    let state = app.state();
    assert!(
        state.notification.is_none(),
        "opening the model picker should not report an error: {:?}",
        state.notification
    );
    assert!(state.picker_count > 0, "Enter should open the model picker");
    assert_eq!(state.prompt_text, "");
    let frame = app.render_to_frame().text();
    assert!(
        !frame.contains("[test/test-model]"),
        "the submitted command's argument placeholder remained visible:\n{frame}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn fast_slash_command_updates_session_state_for_supported_model() {
    let mut app = TestApp::builder().build();
    app.configure_active_model_fast_mode("codex", true);

    app.type_text("/fast on");
    app.press(KeyCode::Enter);

    assert_eq!(app.session_snapshot().fast_mode, Some(true));
    assert!(!app.core_probe().config.settings.fast_mode);
    assert!(app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(cmd)
            if matches!(cmd.as_ref(), protocol::UiCommand::SetFastMode { enabled: true })
    )));
    app.press(KeyCode::F(3));
    assert!(app
        .render_to_frame()
        .text()
        .contains("active=true supported=true"));
    app.press(KeyCode::Esc);

    app.type_text("/fast off");
    app.press(KeyCode::Enter);
    assert_eq!(app.session_snapshot().fast_mode, Some(false));

    app.type_text("/fast toggle");
    app.press(KeyCode::Enter);
    assert_eq!(app.session_snapshot().fast_mode, Some(true));

    app.type_text("/fast");
    app.press(KeyCode::Enter);
    assert_eq!(app.session_snapshot().fast_mode, Some(false));
}

#[test]
fn fast_slash_command_enables_the_next_turn_payload() {
    let mut app = TestApp::builder().build();
    app.configure_active_model_fast_mode("codex", true);

    app.type_text("/fast on");
    app.press(KeyCode::Enter);
    app.type_text("verify fast request");
    app.press(KeyCode::Enter);

    let fast_mode = app
        .actions()
        .iter()
        .find_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::StartTurn(payload) => Some(payload.fast_mode),
                _ => None,
            },
            _ => None,
        })
        .expect("prompt submission should start a turn");
    assert!(fast_mode);
}

#[test]
fn fast_slash_command_rejects_unsupported_model() {
    let mut app = TestApp::builder().build();
    app.configure_active_model_fast_mode("codex", false);

    app.type_text("/fast");
    app.press(KeyCode::Enter);

    assert_eq!(app.session_snapshot().fast_mode, Some(false));
    assert!(app.state().notification.is_some());
    assert!(!app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(cmd)
            if matches!(cmd.as_ref(), protocol::UiCommand::SetFastMode { .. })
    )));
}

#[test]
fn fast_slash_command_rejects_non_codex_provider() {
    let mut app = TestApp::builder().build();
    app.configure_active_model_fast_mode("openai", true);

    app.type_text("/fast on");
    app.press(KeyCode::Enter);

    assert_eq!(app.session_snapshot().fast_mode, Some(false));
    assert!(app.state().notification.is_some());
    assert!(!app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(cmd)
            if matches!(cmd.as_ref(), protocol::UiCommand::SetFastMode { .. })
    )));
}

#[test]
fn fast_mode_is_restored_per_loaded_session() {
    let mut app = TestApp::builder().build();
    app.set_active_model_fast_mode_support(true);

    let mut enabled =
        smelt_core::session::Session::new(app.core_probe().env.pid(), app.core_probe().env.cwd());
    enabled.id = "fast-enabled".into();
    enabled.fast_mode = Some(true);
    app.load_store_backed_session(
        crate::app::session_document::StoreBackedSessionDocument::new(
            enabled,
            crate::app::transcript::LoadedTranscript::full(
                smelt_core::content::transcript::Transcript::new(),
            ),
            crate::app::history::live_session_for_test("fast-enabled".into(), 0, None),
            smelt_store::StoreHead::default(),
        ),
    );
    assert!(app.fast_mode());

    let mut disabled =
        smelt_core::session::Session::new(app.core_probe().env.pid(), app.core_probe().env.cwd());
    disabled.id = "fast-disabled".into();
    disabled.fast_mode = Some(false);
    app.load_store_backed_session(
        crate::app::session_document::StoreBackedSessionDocument::new(
            disabled,
            crate::app::transcript::LoadedTranscript::full(
                smelt_core::content::transcript::Transcript::new(),
            ),
            crate::app::history::live_session_for_test("fast-disabled".into(), 0, None),
            smelt_store::StoreHead::default(),
        ),
    );
    assert!(!app.fast_mode());
    assert!(!app.core_probe().config.settings.fast_mode);
}

#[test]
fn path_completion_tab_still_opens_for_path_tokens() {
    let mut app = TestApp::builder().build();
    let src = app.workspace_probe().cwd_path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn lib() {}\n").unwrap();

    app.type_text("src/");
    app.press(KeyCode::Tab);

    assert!(
        app.state().picker_count > 0,
        "Tab on a path token should open the path completion picker"
    );
}

#[test]
fn unknown_slash_prompt_submits_as_user_message() {
    let mut app = TestApp::builder().build();

    app.type_text("/not-a-command please answer normally");
    app.press(KeyCode::Enter);

    assert!(app.agent_running());
    assert!(
        app.state().notification.is_none(),
        "unknown slash text should not raise a command error"
    );
    assert_eq!(app.state().prompt_text, "");
    let sent = app.actions().iter().any(|action| match action {
        Action::EngineSend(cmd) => matches!(
            cmd.as_ref(),
            protocol::UiCommand::StartTurn(payload)
                if matches!(
                    &payload.input,
                    protocol::StartTurnInput::User { content, .. }
                        if content.text_content() == "/not-a-command please answer normally"
                )
        ),
        _ => false,
    });
    assert!(
        sent,
        "unknown slash text should reach the engine as a user message"
    );
}

#[test]
fn question_keymap_after_prompt_attachment_is_not_plain_insertion() {
    let mut app = TestApp::builder()
        .with_vim(true)
        .with_mode(AgentMode::parse("yolo").expect("valid mode"))
        .build();
    assert!(app.run_lua(r#"smelt.keymap.set("", "?", function() end)"#));
    app.insert_attachment(String::new());
    app.render_silent();

    assert!(app.prompt_plain_insert_ready());
    assert!(app.prompt_plain_char_has_lua_keymap('?'));
    app.feed_one(SourceEvent::Term(Event::Key(KeyEvent {
        code: KeyCode::Char('?'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })));

    assert_eq!(
        app.state().prompt_text,
        crate::input::ATTACHMENT_MARKER.to_string()
    );
    assert_eq!(
        app.prompt_cpos(),
        crate::input::ATTACHMENT_MARKER.len_utf8()
    );
}

#[test]
fn queued_turn_preserves_work_elapsed() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.feed_one(SourceEvent::Tick(3_000));
    app.push_queued_message("follow up".to_string());

    let before = app.working_probe().elapsed().expect("live turn elapsed");
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: None,
        meta: None,
    }));
    let after = app.working_probe().elapsed().expect("queued turn elapsed");

    let saved_elapsed_ms = app
        .conversation_probe()
        .session()
        .turn_metas
        .last()
        .map(|(_, meta)| meta.elapsed_ms)
        .expect("completed turn meta");

    assert!(app.agent_running(), "queued turn should start immediately");
    assert_eq!(app.queued_message_count(), 0);
    assert_eq!(app.state().prompt_text, "");
    assert!(
        saved_elapsed_ms >= 3_000,
        "completed turn meta reset elapsed time: {saved_elapsed_ms}ms"
    );
    assert!(
        after >= before && after >= Duration::from_secs(3),
        "queued turn reset elapsed time: before {before:?}, after {after:?}"
    );
}

#[test]
fn engine_history_replacement_preserves_work_elapsed() {
    let mut app = TestApp::builder().build();
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("old")));
    app.start_turn(1);
    app.feed_one(SourceEvent::Tick(750));

    app.feed_one(SourceEvent::engine(EngineEvent::HistoryUpdated {
        turn_id: 1,
        update: protocol::CanonicalHistoryDelta::new(
            0,
            vec![protocol::HistoryItem::user(protocol::Content::text("new"))],
        ),
    }));

    assert_eq!(
        app.working_probe().elapsed(),
        Some(Duration::from_millis(750))
    );
}

#[test]
fn request_queue_bindings_steer_running_turn() {
    for code in [KeyCode::Enter, KeyCode::Char('q')] {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.drain_engine_sends();

        app.type_text("steer this turn");
        app.clear_actions();
        app.press_mod(code, KeyModifiers::CONTROL);

        assert_eq!(app.queued_message_count(), 1);
        assert_eq!(app.state().prompt_text, "");
        let steered = app.actions().iter().any(|action| match action {
            Action::EngineSend(cmd) => matches!(
                cmd.as_ref(),
                protocol::UiCommand::Steer { input }
                    if input.provider_content().text_content() == "steer this turn"
            ),
            _ => false,
        });
        assert!(steered);
    }
}

#[test]
fn queued_request_stays_out_of_transcript_until_all_tools_finish() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    let tool_a = app.tool_started(
        "tool-a",
        "read_file",
        std::collections::HashMap::from([("file_path".to_string(), serde_json::json!("a.rs"))]),
    );
    let tool_b = app.tool_started(
        "tool-b",
        "read_file",
        std::collections::HashMap::from([("file_path".to_string(), serde_json::json!("b.rs"))]),
    );

    app.type_text("queued follow-up");
    app.press_mod(KeyCode::Enter, KeyModifiers::CONTROL);

    assert_eq!(app.queued_message_count(), 1);
    assert!(app.render_to_frame().text().contains("queued follow-up"));
    assert!(!app.transcript_buffer_text().contains("queued follow-up"));

    let finished = || protocol::ToolOutcome {
        content: "done".into(),
        is_error: false,
        metadata: None,
    };
    app.tool_finished(tool_a, "tool-a", finished(), Some(10));

    assert_eq!(app.pending_tool_invocation_ids(), [tool_b]);
    assert_eq!(app.queued_message_count(), 1);
    assert!(app.render_to_frame().text().contains("queued follow-up"));
    assert!(!app.transcript_buffer_text().contains("queued follow-up"));

    app.tool_finished(tool_b, "tool-b", finished(), Some(20));

    assert!(app.pending_tool_invocation_ids().is_empty());
    assert_eq!(app.queued_message_count(), 1);
    assert!(app.render_to_frame().text().contains("queued follow-up"));
    assert!(!app.transcript_buffer_text().contains("queued follow-up"));

    let transcript_blocks_before_ack = app.transcript_block_count();
    app.feed_one(SourceEvent::engine(EngineEvent::Steered {
        text: "queued follow-up".into(),
        count: 1,
    }));

    assert_eq!(app.queued_message_count(), 0);
    assert_eq!(app.state().prompt_text, "");
    assert_eq!(
        app.transcript_block_count(),
        transcript_blocks_before_ack + 1
    );
    assert!(app.render_to_frame().text().contains("queued follow-up"));
}

#[test]
fn stale_prompt_prediction_response_after_submit_is_ignored() {
    let mut app = TestApp::builder().build();
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text(
        "How should I debug this failing test?",
    )));

    publish_turn_end(&mut app);
    let ask_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(
        ask_ids.len(),
        1,
        "prediction should issue one background ask"
    );
    let prediction_id = ask_ids[0];

    publish_input_submit(&mut app, "Run the focused test first");
    respond_ask_with_text(&mut app, prediction_id, "Run cargo test");

    let prompt = app.well_known_probe().prompt;
    assert_eq!(app.placeholder_text(prompt), None);
}

#[test]
fn queued_messages_collapse_to_keep_transcript_visible() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 10);
    app.start_turn(1);

    for i in 0..12 {
        app.push_queued_message(format!("follow-up {i}"));
    }
    app.render();

    let transcript_rect = app
        .ui_probe()
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .expect("transcript has a rect");
    assert!(
        transcript_rect.height >= 2,
        "transcript should keep at least 2 rows, got {}",
        transcript_rect.height
    );

    let top_win = app
        .ui_probe()
        .named_win("smelt.prompt_bar.top")
        .expect("top bar exists");
    let top_rect = app
        .ui_probe()
        .split_rect(top_win)
        .expect("top bar has a rect");
    assert!(
        top_rect.height <= 5,
        "top bar should be capped on a short terminal, got {}",
        top_rect.height
    );
}

#[test]
fn stale_prompt_prediction_response_after_custom_command_is_ignored() {
    let mut app = TestApp::builder().build();
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text(
        "How should I debug this failing test?",
    )));

    publish_turn_end(&mut app);
    let ask_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(
        ask_ids.len(),
        1,
        "prediction should issue one background ask"
    );
    let prediction_id = ask_ids[0];

    let cmd = smelt_core::custom_commands::CustomCommand {
        name: "fuzz-custom".to_string(),
        display: "fuzz-custom".to_string(),
        body: "Run the focused test first".to_string(),
        overrides: smelt_core::custom_commands::CommandOverrides::default(),
    };
    assert!(
        app.start_custom_command_turn(cmd),
        "test app has a usable model"
    );

    respond_ask_with_text(&mut app, prediction_id, "Run cargo test");

    let prompt = app.well_known_probe().prompt;
    assert_eq!(app.placeholder_text(prompt), None);
}

#[test]
fn lua_prompt_text_strips_attachment_markers() {
    // Inserting an attachment seeds the prompt with U+FFFC + a backing id.
    // `smelt.prompt.text()` is the Lua-side accessor that history search,
    // pickers, and similar plugins use to snapshot the input - those
    // callers can't carry attachment ids, so leaking the marker byte
    // lets a marker round-trip back through `set_text` orphan an id.
    let mut app = TestApp::builder().build();
    app.insert_attachment("screenshot.png".into());
    assert!(app
        .prompt_source()
        .contains(smelt_buffer::ATTACHMENT_MARKER));
    let s: String = app
        .eval_lua("return smelt.prompt.text()")
        .expect("smelt.prompt.text");
    assert!(
        !s.contains(smelt_buffer::ATTACHMENT_MARKER),
        "prompt.text leaked marker byte: {s:?}"
    );
}

#[test]
fn idle_placeholder_dismissal_does_not_swallow_second_escape_rewind() {
    let mut app = TestApp::builder().build();
    app.push_user_block("write the parser");
    app.install_prompt_placeholder(
        "ghost".to_string(),
        Vec::new(),
        vec![crate::smelt_edit::KeyBind::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )],
    );

    app.press(KeyCode::Esc);
    assert!(
        app.state().active_modal.is_none(),
        "first Esc only dismisses the placeholder"
    );

    app.press(KeyCode::Esc);
    drive_lua_tasks(&mut app);

    assert!(
        app.state().active_modal.is_some(),
        "second Esc should still complete the idle Esc-Esc rewind chord"
    );
}

#[test]
fn placeholder_dismissal_does_not_swallow_second_escape_cancel() {
    let mut app = TestApp::builder().build();
    app.install_prompt_placeholder(
        "ghost".to_string(),
        Vec::new(),
        vec![crate::smelt_edit::KeyBind::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )],
    );
    app.start_turn(1);

    app.press(KeyCode::Esc);
    assert!(
        app.agent_running(),
        "first Esc only dismisses the placeholder"
    );

    app.press(KeyCode::Esc);
    assert!(!app.agent_running(), "second Esc still reaches hard cancel");
}

#[test]
fn ctrl_c_on_empty_buffer_when_idle_quits() {
    let mut app = TestApp::builder().build();
    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_requested());
}

#[test]
fn ctrl_c_with_text_in_buffer_clears_buffer_without_quitting() {
    let mut app = TestApp::builder().build();
    app.type_text("hello");
    assert_eq!(app.state().prompt_text, "hello");

    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(app.state().prompt_text, "");
    assert!(!app.quit_requested());
}

#[test]
fn ctrl_c_twice_clears_then_quits() {
    let mut app = TestApp::builder().build();
    app.type_text("hi");
    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!app.quit_requested(), "first Ctrl-C clears, doesn't quit");

    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit_requested(), "second Ctrl-C on empty buffer quits");
}

#[test]
fn fresh_vim_prompt_starts_in_insert_mode() {
    // Chat input ergonomics: even with vim enabled, the prompt starts
    // in Insert so the first keystroke types instead of navigating.
    let app = TestApp::builder().with_vim(true).build();
    assert_eq!(app.state().vim_mode, VimMode::Insert);
}

#[test]
fn typing_into_cmdline_appends_to_payload() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    app.type_text("help");
    assert_eq!(app.state().cmdline_text, "help");
}

#[test]
fn slash_in_editable_prompt_does_not_open_search() {
    let mut app = TestApp::builder().build();
    app.type_char('/');
    let s = app.state();
    assert!(!s.cmdline_open);
    assert_eq!(s.prompt_text, "/");
}

#[test]
fn prompt_docked_picker_does_not_get_tail_clobbered_on_first_render() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            smelt.cmd.picker("pick", {
              items = (function()
                local out = {}
                for i = 1, 12 do out[i] = { label = "item" .. i } end
                return out
              end)(),
              apply = function() end,
            })
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    app.type_text("/pick");
    app.render();

    app.press(KeyCode::Enter);
    app.feed_one(SourceEvent::LuaWakeup);
    app.render();

    // Locate the prompt-docked picker overlay (the slash completer's
    // own picker is closed on Enter).
    let leaf = (1u32..50)
        .map(crate::smelt_edit::OverlayId)
        .filter_map(|id| app.ui_probe().overlay(id))
        .filter_map(|ov| ov.layout.leaves_in_order().into_iter().next())
        .map(|p| WinId(p.0))
        .find(|&win| app.overlays_probe().has_picker(win))
        .expect("a prompt-docked picker overlay should be open after /pick");

    let win = app.ui_probe().win(leaf).expect("picker leaf alive");
    let buf = app.ui_probe().buf(win.buf).expect("picker buf alive");
    let viewport_rows = win
        .viewport
        .map(|v| v.rect.height)
        .expect("picker leaf must have a viewport after render_normal");
    let total_rows = buf.line_count() as crate::smelt_edit::RowIndex;
    let max_scroll = total_rows.saturating_sub(viewport_rows as crate::smelt_edit::RowIndex);
    assert!(
        win.scroll_top() <= max_scroll,
        "picker scroll_top must stay within bounds on first render \
             (scroll_top={}, max_scroll={}, total_rows={}, viewport_rows={})",
        win.scroll_top(),
        max_scroll,
        total_rows,
        viewport_rows,
    );
}

#[test]
fn generic_prompt_buf_source_setter_uses_prompt_install_path() {
    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(r#"smelt.prompt.win():buf():source("hel")"#));
    app.type_text("lo");
    assert_eq!(app.state().prompt_text, "hello");
}

#[test]
fn generic_prompt_buf_lines_setter_uses_prompt_install_path() {
    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(r#"smelt.prompt.win():buf():lines({ "hel" })"#));
    app.type_text("lo");
    assert_eq!(app.state().prompt_text, "hello");
}

#[test]
fn prompt_top_bar_chrome_click_focuses_prompt_without_selecting() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    app.push_transcript_block(smelt_core::Block::Text {
        content: "transcript".into(),
    });
    app.render_silent();
    app.focus_transcript();

    let top_bar = app
        .ui_probe()
        .named_win("smelt.prompt_bar.top")
        .expect("prompt top bar window");
    let vp = app
        .ui_probe()
        .win(top_bar)
        .and_then(|w| w.viewport)
        .expect("prompt top bar viewport");
    let bar_row = vp.rect.top.saturating_add(vp.rect.height.saturating_sub(1));
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: bar_row,
        column: vp.rect.left,
        modifiers: KeyModifiers::empty(),
    })));

    assert_eq!(app.state().app_focus, AppFocus::Prompt);
    assert_eq!(app.ui_probe().focus(), Some(crate::app::PROMPT_WIN));
    assert!(!app.ui_probe().any_drag_active());
}

#[test]
fn prompt_triple_click_event_pipeline_yanks_clicked_source_line() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(r#"smelt.prompt.set_text("first line\nsecond line\nthird line")"#));
    let (top, column) = prompt_content_cell(&mut app);
    let row = top + 1;
    let column = column + 2;

    for _ in 0..3 {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::empty(),
        })));
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::empty(),
        })));
    }

    assert_eq!(
        app.core_probe().clipboard.kill_ring.current(),
        "second line"
    );
}

#[test]
fn keyboard_input_cancels_stale_prompt_mouse_endpoint() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(false).build();
    let (row, column) = prompt_content_cell(&mut app);

    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row,
        column,
        modifiers: KeyModifiers::empty(),
    })));
    assert!(
        app.ui_probe().any_drag_active(),
        "mouse down staged a drag endpoint"
    );

    app.type_text("Hello");

    assert_eq!(app.state().prompt_text, "Hello");
    assert_eq!(app.prompt_endpoint(), 5);
    assert_eq!(app.ui_probe().capture(), None);
    assert!(!app.ui_probe().any_drag_active());
}

#[test]
fn typing_after_turn_complete_keeps_prompt_cursor_coherent() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: None,
        meta: None,
    }));
    app.render_silent();

    for (idx, ch) in "Hello".chars().enumerate() {
        app.type_char(ch);
        assert_eq!(app.prompt_cpos(), idx + 1);
    }

    app.press(KeyCode::Left);
    app.type_char('!');

    assert_eq!(app.state().prompt_text, "Hell!o");
    assert_eq!(app.prompt_cpos(), 5);
}

#[test]
fn text_changed_callbacks_do_not_repark_prompt_cursor() {
    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(
        r#"
            smelt.prompt.win():on("text_changed", function()
                smelt.prompt.cursor(0)
            end)
            "#,
    ));

    app.type_text("Hello");

    assert_eq!(app.state().prompt_text, "Hello");
    assert_eq!(app.prompt_cpos(), 5);
}

#[test]
fn typing_after_unfinished_prompt_click_uses_clicked_caret() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(r#"smelt.prompt.set_text("abcd")"#));
    let (row, column) = prompt_content_cell(&mut app);

    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row,
        column: column + 1,
        modifiers: KeyModifiers::empty(),
    })));
    assert_eq!(app.prompt_endpoint(), 1);

    app.type_text("X");

    assert_eq!(app.state().prompt_text, "aXbcd");
    assert_eq!(app.prompt_cpos(), 2);
    assert_eq!(app.prompt_endpoint(), 2);
    assert_eq!(app.ui_probe().capture(), None);
    assert!(!app.ui_probe().any_drag_active());
}

#[test]
fn focus_lost_cancels_stale_prompt_mouse_endpoint() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(false).build();
    let (row, column) = prompt_content_cell(&mut app);

    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row,
        column,
        modifiers: KeyModifiers::empty(),
    })));
    assert!(
        app.ui_probe().any_drag_active(),
        "mouse down staged a drag endpoint"
    );

    app.feed_one(SourceEvent::Term(Event::FocusLost));

    assert_eq!(app.ui_probe().capture(), None);
    assert!(!app.ui_probe().any_drag_active());
    assert_eq!(app.prompt_endpoint(), 0);
}

#[test]
fn lua_reports_terminal_focus_state() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(r#"assert(smelt.terminal.is_focused() == true)"#));
    app.feed_one(SourceEvent::Term(Event::FocusLost));
    assert!(app.run_lua(r#"assert(smelt.terminal.is_focused() == false)"#));
    app.feed_one(SourceEvent::Term(Event::FocusGained));
    assert!(app.run_lua(r#"assert(smelt.terminal.is_focused() == true)"#));
}

#[test]
fn prompt_tips_do_not_rotate_while_terminal_is_unfocused() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        local tips = require("smelt.tips")
        tips.register({ id = "test.focus.a", text = "test focus tip a" })
        tips.register({ id = "test.focus.b", text = "test focus tip b" })
        _G.smelt_tip_now = 0
        smelt.time.now_ms = function() return _G.smelt_tip_now * 1000 end

        local id = tips.prompt_tip().id
        for _ = 1, #tips.list() + 2 do
          if id == "test.focus.a" then break end
          _G.smelt_tip_now = _G.smelt_tip_now + 12
          id = tips.prompt_tip().id
        end
        assert(id == "test.focus.a")

        _G.smelt_tip_now = _G.smelt_tip_now + 11
        assert(tips.prompt_tip().id == "test.focus.a")
        "#
    ));

    app.feed_one(SourceEvent::Term(Event::FocusLost));
    assert!(app.run_lua(
        r#"
        local tips = require("smelt.tips")
        _G.smelt_tip_now = _G.smelt_tip_now + 100
        assert(tips.prompt_tip().id == "test.focus.a")
        "#
    ));

    app.feed_one(SourceEvent::Term(Event::FocusGained));
    assert!(app.run_lua(
        r#"
        local tips = require("smelt.tips")
        assert(tips.prompt_tip().id == "test.focus.a")
        _G.smelt_tip_now = _G.smelt_tip_now + 1
        assert(tips.prompt_tip().id == "test.focus.b")
        "#
    ));
}

#[test]
fn prompt_window_wraps_parser_output() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.feed_one(SourceEvent::Term(crossterm::event::Event::Paste(
        "x".repeat(200),
    )));
    app.render_silent();
    app.assert_ui_invariants();
}

#[test]
fn pasted_missing_image_path_stays_text() {
    let mut app = TestApp::builder().with_vim(false).build();
    let dir = tempfile::tempdir().unwrap();
    let text = dir.path().join("missing.png").to_string_lossy().to_string();

    app.feed_one(SourceEvent::Term(crossterm::event::Event::Paste(
        text.clone(),
    )));
    app.render_silent();

    assert_eq!(app.state().prompt_text, text);
    let prompt = app.ui_probe().buf(crate::app::PROMPT_EDIT_BUF).unwrap();
    assert!(prompt.attachment_ids.is_empty());
}

#[test]
fn pasted_http_image_url_stays_text() {
    let mut app = TestApp::builder().with_vim(false).build();
    let url = "https://example.com/image.png".to_string();

    app.feed_one(SourceEvent::Term(crossterm::event::Event::Paste(
        url.clone(),
    )));
    app.render_silent();

    assert_eq!(app.state().prompt_text, url);
    let prompt = app.ui_probe().buf(crate::app::PROMPT_EDIT_BUF).unwrap();
    assert!(prompt.attachment_ids.is_empty());
}

#[test]
fn prompt_cursor_probe_catches_stuck_insert_after_turn() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.probe_prompt_cursor_after_turn(1);
}
