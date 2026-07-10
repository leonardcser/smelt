use super::*;

fn user_blocks(app: &TestApp) -> Vec<(String, Vec<String>)> {
    let history = app.app.session_document.transcript.history();
    history
        .order
        .iter()
        .filter_map(|id| history.block(*id))
        .filter_map(|block| match block {
            smelt_core::transcript_model::Block::User { text, image_labels } => {
                Some((text.clone(), image_labels.clone()))
            }
            _ => None,
        })
        .collect()
}

fn insert_image(app: &mut TestApp, label: &str, data_url: &str) {
    let mut ctx = crate::input::prompt_ctx_mut(&mut app.app.ui);
    app.app
        .input
        .insert_image(&mut ctx, label.to_string(), data_url.to_string());
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
    let after_second = app.state();
    assert!(
        !after_second.agent_running,
        "second Esc rewinds the active turn"
    );
    assert_eq!(after_second.prompt_text, "wrong prompt");
    assert!(user_blocks(&app).is_empty());
}

#[test]
fn vim_insert_double_esc_only_cancels_after_assistant_output() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.start_submitted_turn("keep prompt");
    app.app
        .dispatch_engine_event(protocol::EngineEvent::TextDelta {
            delta: "started".into(),
        });

    app.press(KeyCode::Esc);
    assert!(app.agent_running(), "first Esc is the local Vim action");
    assert_eq!(app.state().vim_mode, VimMode::Normal);

    app.press(KeyCode::Esc);
    let after_second = app.state();
    assert!(!after_second.agent_running, "second Esc cancels the agent");
    assert_eq!(after_second.prompt_text, "");
    assert_eq!(user_blocks(&app), vec![("keep prompt".to_string(), vec![])]);
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
    let buf = app
        .app
        .ui
        .buf_create(crate::smelt_edit::BufCreateOpts::default());
    {
        let buf = app.app.ui.buf_mut(buf).expect("overlay buffer");
        buf.readonly = true;
        buf.set_all_lines(vec!["alpha beta".into(), "gamma".into()]);
    }

    let leaf = app
        .app
        .ui
        .win_open_split(
            buf,
            crate::smelt_edit::SplitConfig {
                region: "dialog".into(),
                gutters: Default::default(),
            },
        )
        .expect("overlay leaf");
    if let Some(win) = app.app.ui.win_mut(leaf) {
        win.set_surface(crate::smelt_edit::WindowSurface::readonly_text());
        win.set_vim_enabled(true);
    }
    app.app.ui.overlay_open(
        crate::smelt_edit::Overlay::new(
            crate::smelt_edit::LayoutTree::leaf(leaf),
            crate::smelt_edit::layout::Anchor::ScreenCenter,
        )
        .with_size((40, 5))
        .modal(true),
    );
    app.render_silent();

    app.type_char('v');
    app.type_char('e');
    app.type_char('y');

    assert_eq!(app.app.core.clipboard.kill_ring.current(), "alpha");
    assert_eq!(
        app.app.core.clipboard.kill_ring.last_clipboard_write(),
        Some("alpha")
    );
    assert_eq!(
        app.app.ui.win(leaf).expect("overlay window").vim_mode(),
        VimMode::Normal
    );
    assert!(
        app.app
            .ui
            .win(leaf)
            .expect("overlay window")
            .byte_yank_flash_until()
            .is_some(),
        "byte-backed overlay yank should record a source-local flash"
    );

    app.render_silent();

    assert!(
        !app.app
            .ui
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
        app.app
            .ui
            .win(leaf)
            .expect("overlay window")
            .range_layer(crate::smelt_edit::RangeLayer::YankFlash)
            .is_empty(),
        "expired overlay yank flash should clear like transcript flashes"
    );
    assert!(
        app.app
            .ui
            .win(leaf)
            .expect("overlay window")
            .byte_yank_flash_until()
            .is_none(),
        "expired overlay yank flash state should be cleared"
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
fn vim_visual_enter_submits_selection_only() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.type_text("alpha beta gamma");
    app.press(KeyCode::Esc);
    app.type_char('0');
    app.type_char('w');
    app.type_char('v');
    app.type_char('e');

    app.press(KeyCode::Enter);

    let blocks = user_blocks(&app);
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

    let blocks = user_blocks(&app);
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

    let blocks = user_blocks(&app);
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
    assert_eq!(app.app.prompt_buf().attachment_ids.len(), 1);
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
        Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { text } if text == "beta")
    )));
}

#[test]
fn vim_visual_ctrl_q_with_image_leaves_prompt_unchanged() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.start_turn(1);
    app.drain_engine_sends();
    app.type_text("send ");
    insert_image(&mut app, "send.png", "data:image/png;base64,SEND");
    let prompt = app.state().prompt_text;
    app.press(KeyCode::Esc);
    app.press_mod(KeyCode::Char('V'), KeyModifiers::SHIFT);
    app.clear_actions();

    app.press_mod(KeyCode::Char('q'), KeyModifiers::CONTROL);

    assert_eq!(app.queued_message_count(), 0);
    assert_eq!(app.state().prompt_text, prompt);
    assert_eq!(app.app.prompt_buf().attachment_ids.len(), 1);
    assert_eq!(app.state().vim_mode, VimMode::VisualLine);
    assert!(!app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { .. })
    )));
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

    let now = app.app.core.clock.instant_now();
    let flash = app.app.core.clipboard.kill_ring.yank_flash_range(now);
    assert!(
        flash.is_some(),
        "yank flash range should be active right after yy"
    );

    // Advance past the 200ms flash window. If the clock chain is wired
    // correctly, the flash deadline now sits in the virtual past.
    app.feed_one(SourceEvent::Tick(300));
    let now = app.app.core.clock.instant_now();
    let flash = app.app.core.clipboard.kill_ring.yank_flash_range(now);
    assert!(
        flash.is_none(),
        "flash should expire after Tick past the window"
    );
}
