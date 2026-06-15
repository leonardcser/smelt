use super::*;

#[test]
fn vim_insert_double_esc_opens_rewind_dialog_when_idle() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.push_user_block("write the parser");
    assert_eq!(app.state().vim_mode, VimMode::Insert);

    app.press(KeyCode::Esc);
    let after_first = app.state();
    assert_eq!(after_first.vim_mode, VimMode::Normal);
    assert!(
        after_first.focused_overlay.is_none(),
        "first Esc is only the local Vim action"
    );

    app.press(KeyCode::Esc);
    drive_lua_tasks(&mut app);

    assert!(
        app.state().focused_overlay.is_some(),
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
    assert!(!app.agent_running(), "second Esc hard-cancels the agent");
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
