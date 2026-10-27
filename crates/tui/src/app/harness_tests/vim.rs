use super::*;

fn user_blocks(app: &mut TestApp) -> Vec<(String, Vec<String>)> {
    let ids = app
        .conversation_probe()
        .transcript()
        .history()
        .order
        .clone();
    let inspect = |history: &smelt_core::transcript_model::BlockHistory| {
        history
            .order
            .iter()
            .filter_map(|id| history.block(*id))
            .filter_map(|block| match block {
                smelt_core::transcript_model::Block::User {
                    text, image_labels, ..
                } => Some((text.clone(), image_labels.clone())),
                _ => None,
            })
            .collect()
    };
    if let Some(blocks) = app.with_pinned_transcript_blocks(&ids, inspect) {
        return blocks;
    }
    app.render_silent();
    app.with_pinned_transcript_blocks(&ids, inspect)
        .expect("test inspection should hydrate canonical transcript blocks")
}

fn insert_image(app: &mut TestApp, label: &str, data_url: &str) {
    app.insert_image_attachment(label.to_string(), data_url.to_string());
}

#[test]
fn vim_insert_double_esc_is_quiet_without_rewindable_turns() {
    let mut app = TestApp::builder().with_vim(true).build();

    app.press(KeyCode::Esc);
    assert_eq!(app.state().vim_mode, VimMode::Normal);

    app.press(KeyCode::Esc);
    drive_lua_tasks(&mut app);

    let state = app.state();
    assert!(state.active_modal.is_none());
    assert!(
        state.notification.is_none(),
        "Esc-Esc should do nothing when there are no turns to rewind"
    );
}

#[test]
fn vim_insert_double_esc_opens_rewind_dialog_when_idle() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.push_user_block("write the parser");
    assert_eq!(app.state().vim_mode, VimMode::Insert);

    app.press(KeyCode::Esc);
    let after_first = app.state();
    assert_eq!(after_first.vim_mode, VimMode::Normal);
    assert!(
        after_first.active_modal.is_none(),
        "first Esc is only the local Vim action"
    );

    app.press(KeyCode::Esc);
    drive_lua_tasks(&mut app);

    assert!(
        app.state().active_modal.is_some(),
        "second Esc should complete the idle Esc-Esc rewind chord"
    );
}

#[test]
fn double_esc_can_rewind_after_cancelling_quota_pause() {
    for vim in [false, true] {
        let mut app = TestApp::builder().with_vim(vim).build();
        app.start_submitted_turn("retry this request");
        app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
            message: "quota exceeded".into(),
            kind: Some(protocol::EngineAskErrorKind::Quota),
            retry_at_ms: Some(123_000),
        }));

        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);
        app.wait_for_turn_persistence();
        drive_lua_tasks(&mut app);
        assert!(
            app.state().active_modal.is_none(),
            "first Esc-Esc cancels quota recovery"
        );
        assert!(app
            .eval_lua::<bool>(
                r#"
                local state = smelt.engine.continuation_state()
                return state.paused and state.error_kind == "quota" and state.token == nil
            "#,
            )
            .unwrap());

        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);
        drive_lua_tasks(&mut app);

        assert!(
            app.state().active_modal.is_some(),
            "Esc-Esc should open rewind after quota recovery was cancelled (vim={vim})"
        );
    }
}

#[test]
fn vim_insert_double_esc_cancels_running_agent_on_second_press() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.start_turn(1);
    assert_eq!(app.state().vim_mode, VimMode::Insert);
    assert!(app.agent_running());

    app.press(KeyCode::Esc);
    assert!(app.agent_running(), "first Esc is the local Vim action");
    assert_eq!(app.state().vim_mode, VimMode::Normal);

    app.press(KeyCode::Esc);
    assert!(!app.agent_running(), "second Esc cancels the agent");
}

#[test]
fn vim_insert_double_esc_rewinds_active_user_turn_before_output() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.start_submitted_turn("wrong prompt");

    app.press(KeyCode::Esc);
    assert!(app.agent_running(), "first Esc is the local Vim action");
    assert_eq!(app.state().vim_mode, VimMode::Normal);

    app.press(KeyCode::Esc);
    app.render_silent();
    let after_second = app.state();
    assert!(
        !after_second.agent_running,
        "second Esc rewinds the active turn"
    );
    assert_eq!(after_second.prompt_text, "wrong prompt");
    assert!(user_blocks(&mut app).is_empty());
}

#[test]
fn vim_insert_double_esc_rewinds_active_command_by_history_index() {
    let mut app = TestApp::builder().with_vim(true).build();
    let context = protocol::HistoryItem::note(protocol::HistoryNote::context(
        "Current working directory: /workspace.",
    ));
    app.app.conversation.append_history_item(context.clone());
    assert!(app.start_command_request_turn(
        "/review".into(),
        "expanded review prompt".into(),
        smelt_core::custom_commands::CommandOverrides::default(),
        crate::app::CommandTurnStart::Fresh,
    ));

    app.press(KeyCode::Esc);
    app.press(KeyCode::Esc);
    app.render_silent();

    let after_second = app.state();
    assert!(!after_second.agent_running);
    assert_eq!(after_second.prompt_text, "/review");
    let history = &app.app.conversation.session().history;
    assert_eq!(history.first(), Some(&context));
    assert!(history
        .iter()
        .all(|item| matches!(item, protocol::HistoryItem::Note(_))));
    assert!(user_blocks(&mut app).is_empty());
}

#[test]
fn empty_reasoning_does_not_count_as_assistant_output() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.start_submitted_turn("wrong prompt");
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::ReasoningPartFinished {
            id: "summary".into(),
            kind: protocol::ReasoningKind::Summary,
            title: Some("  ".into()),
            content: "\n\t".into(),
        },
    ));

    app.press(KeyCode::Esc);
    app.press(KeyCode::Esc);
    app.render_silent();

    let after_second = app.state();
    assert!(!after_second.agent_running);
    assert_eq!(after_second.prompt_text, "wrong prompt");
    assert!(user_blocks(&mut app).is_empty());
}

#[test]
fn vim_insert_double_esc_only_cancels_after_assistant_output() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.start_submitted_turn("keep prompt");
    app.dispatch_engine_event(protocol::EngineEvent::TextDelta {
        delta: "started".into(),
    });

    app.press(KeyCode::Esc);
    assert!(app.agent_running(), "first Esc is the local Vim action");
    assert_eq!(app.state().vim_mode, VimMode::Normal);

    app.press(KeyCode::Esc);
    let after_second = app.state();
    assert!(!after_second.agent_running, "second Esc cancels the agent");
    assert_eq!(after_second.prompt_text, "");
    assert_eq!(
        user_blocks(&mut app),
        vec![("keep prompt".to_string(), vec![])]
    );
}

#[test]
fn vim_insert_double_esc_unqueues_messages_on_second_press() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.start_turn(1);
    app.push_queued_message("queued".to_string());

    app.press(KeyCode::Esc);
    let after_first = app.state();
    assert!(after_first.agent_running);
    assert_eq!(after_first.vim_mode, VimMode::Normal);
    assert_eq!(after_first.queued_inputs, vec!["queued".to_string()]);
    assert_eq!(after_first.prompt_text, "");

    app.press(KeyCode::Esc);
    let after_second = app.state();
    assert!(
        after_second.agent_running,
        "unqueue does not cancel the turn"
    );
    assert!(after_second.queued_inputs.is_empty());
    assert_eq!(after_second.prompt_text, "queued");
}

#[test]
fn vim_yank_in_overlay_viewer_writes_system_clipboard() {
    let mut app = TestApp::builder().with_vim(true).build();
    let leaf = app.open_readonly_overlay_fixture(vec!["alpha beta".into(), "gamma".into()], None);
    app.render_silent();

    app.type_char('v');
    app.type_char('e');
    app.type_char('y');

    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "alpha");
    assert_eq!(
        app.core_probe().clipboard.kill_ring.last_clipboard_write(),
        Some("alpha")
    );
    assert_eq!(
        app.ui_probe().win(leaf).expect("overlay window").vim_mode(),
        VimMode::Normal
    );
    assert!(
        app.ui_probe()
            .win(leaf)
            .expect("overlay window")
            .byte_yank_flash_until()
            .is_some(),
        "byte-backed overlay yank should record a source-local flash"
    );

    app.render_silent();

    assert!(
        !app.ui_probe()
            .win(leaf)
            .expect("overlay window")
            .range_layer(crate::smelt_edit::RangeLayer::YankFlash)
            .is_empty(),
        "yanked overlay selection should flash in the focused dialog viewer"
    );

    app.clock.advance(
        smelt_buffer::kill_ring::YANK_FLASH_DURATION + std::time::Duration::from_millis(1),
    );
    app.render_silent();

    assert!(
        app.ui_probe()
            .win(leaf)
            .expect("overlay window")
            .range_layer(crate::smelt_edit::RangeLayer::YankFlash)
            .is_empty(),
        "expired overlay yank flash should clear like transcript flashes"
    );
    assert!(
        app.ui_probe()
            .win(leaf)
            .expect("overlay window")
            .byte_yank_flash_until()
            .is_none(),
        "expired overlay yank flash state should be cleared"
    );
}

#[test]
fn keyboard_yank_in_readonly_viewer_preserves_grapheme_clusters() {
    let mut app = TestApp::builder().with_vim(true).build();
    let expected = "besta\u{308}tigt 日本 👩\u{200d}💻 9\u{fe0f} 🇨🇦";
    app.open_readonly_overlay_fixture(vec![expected.into()], None);
    app.render_silent();

    app.type_char('v');
    app.type_char('$');
    app.type_char('y');

    assert_eq!(app.core_probe().clipboard.kill_ring.current(), expected);
    assert_eq!(
        app.core_probe().clipboard.kill_ring.last_clipboard_write(),
        Some(expected)
    );
}

#[test]
fn vim_i_enters_insert_from_normal() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    assert_eq!(app.state().vim_mode, VimMode::Normal);

    app.type_char('i');
    assert_eq!(app.state().vim_mode, VimMode::Insert);
}

#[test]
fn vim_a_enters_insert_after_cursor() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char('a');
    assert_eq!(app.state().vim_mode, VimMode::Insert);
}

#[test]
fn vim_esc_returns_insert_to_normal() {
    let mut app = TestApp::builder().with_vim(true).build();
    // Prompt starts in Insert; type directly.
    app.type_text("hello");
    assert_eq!(app.state().vim_mode, VimMode::Insert);
    assert_eq!(app.state().prompt_text, "hello");

    app.press(KeyCode::Esc);
    assert_eq!(app.state().vim_mode, VimMode::Normal);
}

#[test]
fn vim_v_enters_visual_from_normal() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("abc");
    app.press(KeyCode::Esc);
    assert_eq!(app.state().vim_mode, VimMode::Normal);

    app.type_char('v');
    assert_eq!(app.state().vim_mode, VimMode::Visual);
}

#[test]
fn vim_shift_v_enters_visual_line() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("abc");
    app.press(KeyCode::Esc);

    app.press_mod(KeyCode::Char('V'), KeyModifiers::SHIFT);
    assert_eq!(app.state().vim_mode, VimMode::VisualLine);
}

#[test]
fn vim_terminal_paste_exits_visual_mode() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("abc");
    app.press(KeyCode::Esc);
    app.type_char('v');
    assert_eq!(app.state().vim_mode, VimMode::Visual);

    app.feed_one(SourceEvent::Term(Event::Paste("X".into())));

    assert_eq!(app.state().prompt_text, "abX");
    assert_eq!(app.state().vim_mode, VimMode::Normal);
}

#[test]
fn vim_terminal_paste_exits_visual_line_mode() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("one\ntwo\nthree");
    app.press(KeyCode::Esc);
    app.press_mod(KeyCode::Char('V'), KeyModifiers::SHIFT);
    assert_eq!(app.state().vim_mode, VimMode::VisualLine);

    app.feed_one(SourceEvent::Term(Event::Paste("THREE".into())));

    assert_eq!(app.state().prompt_text, "one\ntwo\nTHREE");
    assert_eq!(app.state().vim_mode, VimMode::Normal);
}

#[test]
fn vim_esc_from_visual_returns_to_normal() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("abc");
    app.press(KeyCode::Esc);
    app.type_char('v');
    assert_eq!(app.state().vim_mode, VimMode::Visual);

    app.press(KeyCode::Esc);
    assert_eq!(app.state().vim_mode, VimMode::Normal);
}

#[test]
fn ctrl_r_searches_history_outside_vim_normal_mode() {
    for vim in [false, true] {
        let mut app = TestApp::builder().with_vim(vim).build();
        app.push_history_entry("current draft from history".into());
        app.type_text("current draft");
        app.render_silent();
        app.press_mod(KeyCode::Char('r'), KeyModifiers::CONTROL);
        app.feed_one(SourceEvent::LuaWakeup);
        let frame = app.render_to_frame();
        assert!(
            frame.text().contains("current draft from history"),
            "vim={vim}, frame:\n{}",
            frame.text()
        );
        app.press(KeyCode::Esc);
        app.feed_one(SourceEvent::LuaWakeup);
        assert_eq!(app.state().prompt_text, "current draft");
        assert!(!app
            .render_to_frame()
            .text()
            .contains("current draft from history"));
        app.assert_invariants();
    }
}

#[test]
fn vim_visual_enter_submits_selection_only() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("alpha beta gamma");
    app.press(KeyCode::Esc);
    app.type_char('0');
    app.type_char('w');
    app.type_char('v');
    app.type_char('e');

    app.press(KeyCode::Enter);

    let blocks = user_blocks(&mut app);
    assert_eq!(blocks, vec![("beta".to_string(), vec![])]);
    assert_eq!(app.state().prompt_text, "alpha  gamma");
    assert_eq!(app.state().vim_mode, VimMode::Normal);
}

#[test]
fn vim_visual_line_enter_submits_selected_lines_only() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("one\ntwo\nthree");
    app.press(KeyCode::Esc);
    app.type_char('k');
    app.type_char('0');
    app.press_mod(KeyCode::Char('V'), KeyModifiers::SHIFT);

    app.press(KeyCode::Enter);

    let blocks = user_blocks(&mut app);
    assert_eq!(blocks, vec![("two".to_string(), vec![])]);
    assert_eq!(app.state().prompt_text, "one\nthree");
    assert_eq!(app.state().vim_mode, VimMode::Normal);
}

#[test]
fn vim_visual_line_enter_carries_only_selected_attachments() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("keep ");
    insert_image(&mut app, "keep.png", "data:image/png;base64,KEEP");
    app.type_text("\nsend ");
    insert_image(&mut app, "send.png", "data:image/png;base64,SEND");
    app.press(KeyCode::Esc);
    app.press_mod(KeyCode::Char('V'), KeyModifiers::SHIFT);

    app.press(KeyCode::Enter);

    let blocks = user_blocks(&mut app);
    assert_eq!(
        blocks,
        vec![(
            "send [send.png]".to_string(),
            vec!["[send.png]".to_string()]
        )]
    );
    assert_eq!(
        app.state().prompt_text,
        format!("keep {}", crate::input::ATTACHMENT_MARKER)
    );
    assert_eq!(app.prompt_attachment_count(), 1);
}

#[test]
fn vim_visual_ctrl_q_steers_selection_only() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.start_turn(1);
    app.drain_engine_sends();
    app.type_text("alpha beta gamma");
    app.press(KeyCode::Esc);
    app.type_char('0');
    app.type_char('w');
    app.type_char('v');
    app.type_char('e');
    app.clear_actions();

    app.press_mod(KeyCode::Char('q'), KeyModifiers::CONTROL);

    assert_eq!(app.queued_message_count(), 1);
    assert_eq!(app.state().prompt_text, "alpha  gamma");
    assert!(app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input }
                    if input.provider_content().text_content() == "beta")
    )));
}

#[test]
fn vim_visual_ctrl_q_with_image_steers_and_restores_selection() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.start_turn(1);
    app.drain_engine_sends();
    app.type_text("keep ");
    insert_image(&mut app, "keep.png", "data:image/png;base64,KEEP");
    app.type_text("\nsend ");
    insert_image(&mut app, "send.png", "data:image/png;base64,SEND");
    app.press(KeyCode::Esc);
    app.press_mod(KeyCode::Char('V'), KeyModifiers::SHIFT);
    app.clear_actions();

    app.press_mod(KeyCode::Char('q'), KeyModifiers::CONTROL);

    assert_eq!(app.queued_message_count(), 1);
    assert_eq!(
        app.state().prompt_text,
        format!("keep {}", crate::input::ATTACHMENT_MARKER)
    );
    assert_eq!(app.prompt_attachment_count(), 1);
    assert!(app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input }
            if input.provider_content().image_count() == 1)
    )));

    app.press(KeyCode::Esc);
    app.press(KeyCode::Esc);
    assert_eq!(app.queued_message_count(), 0);
    assert_eq!(
        app.state().prompt_text,
        format!("send {0}\nkeep {0}", crate::input::ATTACHMENT_MARKER)
    );
    assert_eq!(app.prompt_attachment_count(), 2);
}

#[test]
fn vim_full_cycle_normal_insert_normal_visual_normal() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    assert_eq!(app.state().vim_mode, VimMode::Normal);

    app.type_char('i');
    assert_eq!(app.state().vim_mode, VimMode::Insert);

    app.type_text("foo");
    app.press(KeyCode::Esc);
    assert_eq!(app.state().vim_mode, VimMode::Normal);

    app.type_char('v');
    assert_eq!(app.state().vim_mode, VimMode::Visual);

    app.press(KeyCode::Esc);
    assert_eq!(app.state().vim_mode, VimMode::Normal);
}

#[test]
fn vim_typing_in_normal_mode_does_not_append_to_buffer() {
    let mut app = TestApp::builder().with_vim(true).build();
    // Normal-mode 'h' / 'l' are motions, not characters - should not
    // land in the prompt buffer.
    app.press(KeyCode::Esc);
    app.type_text("hl");
    assert_eq!(app.state().prompt_text, "");
    assert_eq!(app.state().vim_mode, VimMode::Normal);
}

#[test]
fn vim_typing_in_insert_mode_appends_to_buffer() {
    let mut app = TestApp::builder().with_vim(true).build();
    // Prompt starts in Insert.
    app.type_text("hello world");
    assert_eq!(app.state().prompt_text, "hello world");
}

#[test]
fn vim_dd_in_normal_deletes_line() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("line one");
    app.press(KeyCode::Esc);
    assert_eq!(app.state().prompt_text, "line one");

    // `dd` deletes the current line.
    app.type_char('d');
    app.type_char('d');
    assert_eq!(app.state().prompt_text, "");
}

#[test]
fn vim_pending_replace_owns_colon_before_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("abc");
    app.press(KeyCode::Esc);

    app.type_char('r');
    app.type_char(':');

    let state = app.state();
    assert_eq!(state.prompt_text, "ab:");
    assert!(!state.cmdline_open);
    assert_eq!(state.vim_mode, VimMode::Normal);
}

#[test]
fn vim_pending_replace_owns_slash_before_search() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("abc");
    app.press(KeyCode::Esc);

    app.type_char('r');
    app.type_char('/');

    let state = app.state();
    assert_eq!(state.prompt_text, "ab/");
    assert!(!state.cmdline_open);
    assert_eq!(state.vim_mode, VimMode::Normal);
}

#[test]
fn vim_pending_find_owns_slash_before_search() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("a/b");
    app.press(KeyCode::Esc);
    app.type_char('0');

    app.type_char('f');
    app.type_char('/');

    let state = app.state();
    assert_eq!(state.prompt_text, "a/b");
    assert!(!state.cmdline_open);
    assert_eq!(state.vim_mode, VimMode::Normal);
}

#[test]
fn vim_pending_replace_owns_lua_keymap() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(r#"smelt.keymap.set("", "?", function() end)"#));
    app.type_text("abc");
    app.press(KeyCode::Esc);

    app.type_char('r');
    app.type_char('?');

    let state = app.state();
    assert_eq!(state.prompt_text, "ab?");
    assert_eq!(state.vim_mode, VimMode::Normal);
}

#[test]
fn vim_yy_yank_flash_expires_after_tick() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_char('i');
    app.type_text("hello");
    app.press(KeyCode::Esc);
    app.type_char('y');
    app.type_char('y');

    let now = app.core_probe().clock.instant_now();
    let flash = app.core_probe().clipboard.kill_ring.yank_flash_range(now);
    assert!(
        flash.is_some(),
        "yank flash range should be active right after yy"
    );

    // Advance past the 150ms flash window. If the clock chain is wired
    // correctly, the flash deadline now sits in the virtual past.
    app.feed_one(SourceEvent::Tick(300));
    let now = app.core_probe().clock.instant_now();
    let flash = app.core_probe().clipboard.kill_ring.yank_flash_range(now);
    assert!(
        flash.is_none(),
        "flash should expire after Tick past the window"
    );
}

#[test]
fn vim_paragraph_motions_move_the_prompt_cursor_between_blank_lines() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("one\ntwo\n\nthree");
    app.press(KeyCode::Esc);

    app.type_text("{");
    assert_eq!(app.prompt_cpos(), 8);
    app.type_text("{");
    assert_eq!(app.prompt_cpos(), 0);
    app.type_text("}");
    assert_eq!(app.prompt_cpos(), 8);
    app.type_text("}");
    assert_eq!(app.prompt_cpos(), "one\ntwo\n\nthree".len() - 1);
}

#[test]
fn vim_delete_to_paragraph_end_takes_whole_lines() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("one\ntwo\n\nthree");
    app.press(KeyCode::Esc);
    app.type_text("{{d}");
    assert_eq!(app.state().prompt_text, "\nthree");
}

#[test]
fn vim_yiw_in_viewer_flashes_and_keeps_the_cursor_at_the_word_start() {
    let mut app = TestApp::builder().with_vim(true).build();
    let leaf = app.open_readonly_overlay_fixture(vec!["alpha beta gamma".into()], None);
    app.render_silent();

    app.type_text("wl");
    app.type_text("yiw");

    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "beta");
    let win = app.ui_probe().win(leaf).expect("overlay window");
    assert_eq!(win.cpos(), 6, "yiw leaves the cursor at the word start");
    assert!(win.byte_yank_flash_until().is_some());
}

#[test]
fn transcript_yiw_flashes_and_keeps_the_cursor_at_the_word_start() {
    let mut app = row_document_transcript_app(100, true);
    app.type_text("gg");
    app.render_silent();
    let lines = transcript_buffer_lines(&app, 400);
    let row = lines
        .iter()
        .position(|line| line.contains("row 003 alpha beta"))
        .expect("row 003");
    let word_start = lines[row].find("alpha").unwrap();
    move_transcript_cursor_to_row(&mut app, row as crate::smelt_edit::RowIndex);

    app.type_text("0wwl");
    app.type_text("yiw");
    app.render_silent();

    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "alpha");
    let cursor = app.transcript_window().row_cursor.unwrap();
    assert_eq!(cursor.row as usize, row);
    assert_eq!(cursor.byte_col, word_start);
    let flash = app
        .transcript_window()
        .document_view
        .yank_flash
        .expect("yank flash");
    assert_eq!(flash.range.start.byte_col, word_start);
    assert_eq!(flash.range.end.byte_col, word_start + "alpha".len());
}

#[test]
fn transcript_paragraph_motions_land_on_blank_rows() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 16);
    for i in 0..40 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("para {i} one\nstill {i}\n\npara {i} two").into(),
        });
    }
    app.render_silent();
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
    app.type_text("gg");
    app.render_silent();
    let lines = transcript_buffer_lines(&app, 400);

    app.type_text("}");
    let first = app.transcript_window().row_cursor.unwrap().row as usize;
    assert!(
        lines[first].trim().is_empty(),
        "closing brace lands on a blank row"
    );
    assert!(first > 0);

    app.type_text("}");
    let second = app.transcript_window().row_cursor.unwrap().row as usize;
    assert!(second > first && lines[second].trim().is_empty());

    app.type_text("{");
    assert_eq!(
        app.transcript_window().row_cursor.unwrap().row as usize,
        first
    );
}

#[test]
fn transcript_cursor_stays_visible_on_the_timestamp_row_at_the_top() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(100, 24);
    app.push_transcript_block(smelt_core::transcript_model::Block::User {
        text: "give me a tour".into(),
        image_labels: Vec::new(),
        command: false,
        sent_at_ms: Some(1_700_000_000_000),
    });
    for i in 0..6 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("para {i} one\n\npara {i} two line").into(),
        });
    }
    app.render_silent();
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);

    app.type_text("gg");
    let frame = app.render_to_frame();
    let cursor_rows: Vec<usize> = frame
        .styles_text()
        .lines()
        .filter(|line| line.contains("bg=White"))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect();
    assert_eq!(cursor_rows, vec![0], "gg draws the cursor on the top row");
}

fn transcript_cursor(app: &TestApp) -> (usize, usize) {
    let cursor = app.transcript_window().row_cursor.unwrap();
    (cursor.row as usize, cursor.byte_col)
}

fn transcript_row_of(app: &TestApp, needle: &str) -> usize {
    transcript_buffer_lines(app, 400)
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no transcript row contains {needle:?}"))
}

fn transcript_at(rows: usize, needle: &str) -> (TestApp, usize) {
    let mut app = row_document_transcript_app(rows, true);
    app.type_text("gg");
    app.render_silent();
    let row = transcript_row_of(&app, needle);
    move_transcript_cursor_to_row(&mut app, row as crate::smelt_edit::RowIndex);
    (app, row)
}

#[test]
fn transcript_big_word_motions_and_word_end_backward() {
    let (mut app, row) = transcript_at(100, "row 003 alpha beta");
    app.type_text("0");
    app.type_text("2W");
    assert_eq!(transcript_cursor(&app), (row, "row 003 ".len()));
    app.type_text("ge");
    assert_eq!(transcript_cursor(&app), (row, "row 00".len()));
    app.type_text("gE");
    assert_eq!(transcript_cursor(&app), (row, "ro".len()));
    app.type_text("E");
    assert_eq!(transcript_cursor(&app), (row, "row 00".len()));
    app.type_text("$B");
    assert_eq!(transcript_cursor(&app), (row, "row 003 alpha ".len()));
}

#[test]
fn transcript_ge_continues_onto_the_previous_row() {
    let (mut app, row) = transcript_at(100, "row 003 alpha beta");
    app.type_text("0ge");
    let (prev_row, col) = transcript_cursor(&app);
    assert!(prev_row < row, "ge skips the blank rows between blocks");
    assert!(transcript_buffer_lines(&app, 400)[prev_row].contains("row 002 alpha beta"));
    assert_eq!(col, "row 002 alpha bet".len());
}

#[test]
fn transcript_find_motions_repeat_with_semicolon_and_comma() {
    let (mut app, row) = transcript_at(100, "row 003 alpha beta");
    app.type_text("0fa");
    assert_eq!(transcript_cursor(&app), (row, "row 003 ".len()));
    app.type_text(";");
    assert_eq!(transcript_cursor(&app), (row, "row 003 alph".len()));
    app.type_text(",");
    assert_eq!(transcript_cursor(&app), (row, "row 003 ".len()));
    app.type_text("0tb");
    assert_eq!(transcript_cursor(&app), (row, "row 003 alpha ".len() - 1));
    app.type_text("$Fl");
    assert_eq!(transcript_cursor(&app), (row, "row 003 a".len()));
    app.type_text("$Tl");
    assert_eq!(transcript_cursor(&app), (row, "row 003 al".len()));
}

#[test]
fn transcript_yank_to_a_found_character() {
    let (mut app, row) = transcript_at(100, "row 003 alpha beta");
    app.type_text("0yfa");
    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "row 003 a");
    assert_eq!(transcript_cursor(&app), (row, 0));
    app.type_text("yta");
    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "row 003 ");
    app.type_text("$yFl");
    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "lpha bet");
}

#[test]
fn transcript_percent_jumps_between_brackets_across_rows() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 16);
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: "call(one, [two])\nfn open {\n    body\n}\n".into(),
    });
    app.render_silent();
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
    app.type_text("gg");
    app.render_silent();
    let call = transcript_row_of(&app, "call(one");
    move_transcript_cursor_to_row(&mut app, call as crate::smelt_edit::RowIndex);

    app.type_text("0%");
    assert_eq!(transcript_cursor(&app), (call, "call(one, [two]".len()));
    app.type_text("%");
    assert_eq!(transcript_cursor(&app), (call, "call".len()));

    app.type_text("0f(y%");
    assert_eq!(
        app.core_probe().clipboard.kill_ring.current(),
        "(one, [two])"
    );

    let open = transcript_row_of(&app, "fn open");
    move_transcript_cursor_to_row(&mut app, open as crate::smelt_edit::RowIndex);
    app.type_text("$%");
    let close = transcript_row_of(&app, "}");
    assert_eq!(transcript_cursor(&app).0, close);
}

#[test]
fn transcript_screen_motions_pick_visible_rows() {
    let mut app = row_document_transcript_app(100, true);
    move_transcript_cursor_to_row(&mut app, 60);
    app.type_text("zt");
    app.render_silent();
    let top = app.transcript_window().scroll_top() as usize;
    let height = app.transcript_window().viewport.unwrap().rect.height as usize;

    app.type_text("L");
    assert_eq!(transcript_cursor(&app).0, top + height - 1);
    app.type_text("H");
    assert_eq!(transcript_cursor(&app).0, top);
    app.type_text("M");
    assert_eq!(transcript_cursor(&app).0, top + (height - 1) / 2);
    app.type_text("2L");
    assert_eq!(transcript_cursor(&app).0, top + height - 2);
    app.type_text("3H");
    assert_eq!(transcript_cursor(&app).0, top + 2);
}

#[test]
fn transcript_zt_and_zb_place_the_cursor_row_at_the_viewport_edges() {
    let mut app = row_document_transcript_app(100, true);
    let row = 60;
    move_transcript_cursor_to_row(&mut app, row as crate::smelt_edit::RowIndex);
    app.type_text("zt");
    app.render_silent();
    assert_eq!(app.transcript_window().scroll_top() as usize, row);
    let height = app.transcript_window().viewport.unwrap().rect.height as usize;
    app.type_text("zb");
    app.render_silent();
    assert_eq!(
        app.transcript_window().scroll_top() as usize,
        row + 1 - height
    );
    eprintln!(
        "DBG before zz cursor {:?} scroll {}",
        transcript_cursor(&app),
        app.transcript_window().scroll_top()
    );
    app.type_text("zz");
    eprintln!(
        "DBG after zz cursor {:?} scroll {}",
        transcript_cursor(&app),
        app.transcript_window().scroll_top()
    );
    app.render_silent();
    eprintln!(
        "DBG rendered zz cursor {:?} scroll {}",
        transcript_cursor(&app),
        app.transcript_window().scroll_top()
    );
    assert_eq!(
        app.transcript_window().scroll_top() as usize,
        row - height / 2
    );
}

#[test]
fn vim_word_end_backward_moves_the_prompt_cursor_and_takes_operators() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("alpha beta  gamma");
    app.press(KeyCode::Esc);

    app.type_text("ge");
    assert_eq!(app.prompt_cpos(), "alpha beta  ".len() - 3);
    app.type_text("ge");
    assert_eq!(app.prompt_cpos(), "alph".len());
    app.type_text("ge");
    assert_eq!(app.prompt_cpos(), 0);
    app.type_text("$gE");
    assert_eq!(app.prompt_cpos(), "alpha beta".len() - 1);

    app.type_text("$dge");
    assert_eq!(app.state().prompt_text, "alpha bet");
}

#[test]
fn vim_gj_and_gk_follow_wrapped_prompt_rows() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(20, 12);
    let text = "aaaa bbbb cccc dddd eeee ffff gggg hhhh";
    app.type_text(text);
    app.press(KeyCode::Esc);
    app.render_silent();
    app.type_text("0");

    app.type_text("gj");
    let first = app.prompt_cpos();
    assert!(
        first > 0 && first < text.len(),
        "gj lands on the next row: {first}"
    );
    app.type_text("gj");
    assert!(app.prompt_cpos() > first);
    app.type_text("gk");
    assert_eq!(app.prompt_cpos(), first);
    app.type_text("gkgk");
    assert_eq!(app.prompt_cpos(), 0);
}

#[test]
fn prompt_gv_reselects_the_last_visual_selection() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("alpha beta gamma");
    app.press(KeyCode::Esc);
    app.type_text("0viw");
    app.press(KeyCode::Esc);
    app.type_text("$gvy");
    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "alpha");
    assert_eq!(app.prompt_cpos(), 0);
}

#[test]
fn transcript_gv_reselects_the_last_visual_selection() {
    let mut app = row_document_transcript_app(100, true);
    app.type_text("gg");
    app.render_silent();
    let lines = transcript_buffer_lines(&app, 400);
    let row = lines
        .iter()
        .position(|line| line.contains("row 003 alpha beta"))
        .expect("row 003");
    move_transcript_cursor_to_row(&mut app, row as crate::smelt_edit::RowIndex);

    app.type_text("0wwviw");
    app.press(KeyCode::Esc);
    app.type_text("jgvy");
    app.render_silent();

    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "alpha");
}

#[test]
fn viewer_gv_reselects_the_last_visual_selection() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.open_readonly_overlay_fixture(vec!["alpha beta gamma".into()], None);
    app.render_silent();

    app.type_text("wviw");
    app.press(KeyCode::Esc);
    app.type_text("$gvy");

    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "beta");
}

fn transcript_with_code_block() -> (TestApp, usize) {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 24);
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: "intro\n\n```rust\nfn main() {}\nlet x = 1;\n```\n\noutro\n".into(),
    });
    app.render_silent();
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
    app.type_text("gg");
    app.render_silent();
    let row = transcript_row_of(&app, "fn main");
    move_transcript_cursor_to_row(&mut app, row as crate::smelt_edit::RowIndex);
    (app, row)
}

#[test]
fn transcript_yank_in_code_block_copies_the_code() {
    let (mut app, _) = transcript_with_code_block();
    app.type_text("yic");
    assert_eq!(
        app.core_probe().clipboard.kill_ring.current(),
        "fn main() {}\nlet x = 1;"
    );
}

#[test]
fn transcript_yank_around_code_block_copies_fenced_markdown() {
    let (mut app, _) = transcript_with_code_block();
    app.type_text("yac");
    let copied = app.core_probe().clipboard.kill_ring.current().to_string();
    assert!(
        copied.starts_with("```rust\nfn main() {}\nlet x = 1;\n```"),
        "{copied:?}"
    );
}

#[test]
fn transcript_visual_code_block_selects_only_the_code() {
    let (mut app, row) = transcript_with_code_block();
    app.type_text("vic");
    assert_eq!(app.transcript_window().vim_mode, VimMode::Visual);
    app.type_text("y");
    assert_eq!(
        app.core_probe().clipboard.kill_ring.current(),
        "fn main() {}\nlet x = 1;"
    );
    assert_eq!(transcript_cursor(&app).0, row);
}

#[test]
fn transcript_code_block_object_ignores_prose() {
    let (mut app, _) = transcript_with_code_block();
    let outro = transcript_row_of(&app, "outro");
    move_transcript_cursor_to_row(&mut app, outro as crate::smelt_edit::RowIndex);
    app.type_text("yic");
    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "");
}
