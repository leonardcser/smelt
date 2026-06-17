use super::*;

#[test]
fn stale_title_response_after_reset_is_ignored() {
    let mut app = TestApp::builder().build();
    let original_session_id = app.app.core.session.id.clone();

    publish_input_submit(&mut app, "Fix flaky integration tests");
    let ask_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(ask_ids.len(), 1, "title should issue one background ask");
    let title_id = ask_ids[0];

    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reset_session();
    }
    assert_ne!(app.app.core.session.id, original_session_id);

    respond_ask_with_text(
        &mut app,
        title_id,
        r#"{"title":"Wrong session title","slug":"wrong-session"}"#,
    );

    assert_eq!(app.app.core.session.title, None);
    assert_eq!(app.app.core.session.slug, None);
}

#[test]
fn title_response_after_rewind_is_ignored() {
    let mut app = TestApp::builder().build();

    publish_input_submit(&mut app, "Add caching to parser");
    let ask_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(ask_ids.len(), 1, "title should issue one background ask");
    let title_id = ask_ids[0];

    publish_history_delta(&mut app, "rewound");
    respond_ask_with_text(
        &mut app,
        title_id,
        r#"{"title":"Stale parser cache","slug":"stale-parser-cache"}"#,
    );

    assert_eq!(app.app.core.session.title, None);
    assert_eq!(app.app.core.session.slug, None);
}

#[test]
fn cancelled_title_request_does_not_fallback_to_submitted_text() {
    let mut app = TestApp::builder().build();

    publish_input_submit(&mut app, "Temporary request to cancel");
    let ask_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(ask_ids.len(), 1, "title should issue one background ask");

    respond_ask_with_error(
        &mut app,
        ask_ids[0],
        protocol::EngineAskErrorKind::Cancelled,
    );

    assert_eq!(app.app.core.session.title, None);
    assert_eq!(app.app.core.session.slug, None);
}

#[test]
fn rewind_restores_session_title_snapshot() {
    let mut app = TestApp::builder().build();
    app.app.core.session.history = vec![
        protocol::HistoryItem::user(protocol::Content::text("First task")),
        protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("done")),
            None,
            Vec::new(),
        )),
        protocol::HistoryItem::user(protocol::Content::text("Second task")),
        protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("done")),
            None,
            Vec::new(),
        )),
    ];
    app.app.restore_screen();

    app.app
        .set_session_title("First task".into(), "first-task".into(), Some(1));
    app.app
        .set_session_title("Second task".into(), "second-task".into(), Some(3));

    let restored = app.app.rewind_to(2).expect("second user turn");

    assert_eq!(restored.0, "Second task");
    assert_eq!(app.app.core.session.history.len(), 2);
    assert_eq!(app.app.core.session.title.as_deref(), Some("First task"));
    assert_eq!(app.app.core.session.slug.as_deref(), Some("first-task"));
    assert_eq!(app.app.task_label.as_deref(), Some("first-task"));
}

#[test]
fn rewind_to_start_clears_session_title_snapshot() {
    let mut app = TestApp::builder().build();
    app.app.core.session.history = vec![protocol::HistoryItem::user(protocol::Content::text(
        "First task",
    ))];
    app.app
        .set_session_title("First task".into(), "first-task".into(), Some(1));

    app.app.rewind_to_start();

    assert_eq!(app.app.core.session.title, None);
    assert_eq!(app.app.core.session.slug, None);
    assert_eq!(app.app.task_label, None);
}

#[test]
fn second_title_request_supersedes_inflight_response() {
    let mut app = TestApp::builder().build();

    publish_input_submit(&mut app, "Investigate parser panic");
    let first_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(first_ids.len(), 1);
    let first_id = first_ids[0];

    publish_input_submit(&mut app, "Fix renderer panic instead");
    let second_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(second_ids.len(), 1);
    let second_id = second_ids[0];
    assert_ne!(first_id, second_id);

    respond_ask_with_text(
        &mut app,
        first_id,
        r#"{"title":"Old parser panic","slug":"old-parser"}"#,
    );
    assert_eq!(app.app.core.session.title, None);

    respond_ask_with_text(
        &mut app,
        second_id,
        r#"{"title":"Fix renderer panic","slug":"fix-renderer"}"#,
    );
    assert_eq!(
        app.app.core.session.title.as_deref(),
        Some("Fix renderer panic")
    );
    assert_eq!(app.app.core.session.slug.as_deref(), Some("fix-renderer"));
}

#[test]
fn slow_double_escape_is_two_single_escapes() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);

    app.press(KeyCode::Esc);
    app.feed_one(SourceEvent::Tick(crate::app::ESC_CHORD_TIMEOUT_MS + 1));
    app.press(KeyCode::Esc);

    assert!(app.agent_running(), "expired Esc prefix must not cancel");
}

#[test]
fn non_escape_key_breaks_pending_escape_sequence() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);

    app.press(KeyCode::Esc);
    app.type_char('x');
    app.press(KeyCode::Esc);

    assert!(
        app.agent_running(),
        "Esc, other key, Esc is not a double Esc"
    );
}

#[test]
fn timed_notification_expires_after_ttl() {
    let mut app = TestApp::builder().build();

    app.app.notify_error("transient error".into());
    assert!(app.state().notification.is_some());

    app.feed_one(SourceEvent::Tick(crate::app::NOTIFICATION_TTL_MS + 1));

    assert!(app.state().notification.is_none());
}

#[test]
fn sticky_notification_waits_for_escape() {
    let mut app = TestApp::builder().with_vim(false).build();

    app.app.notify_error_sticky("quota reached".into());
    let notification = app.state().notification;
    assert!(notification.is_some());

    app.feed_one(SourceEvent::Tick(crate::app::NOTIFICATION_TTL_MS + 1));
    assert_eq!(app.state().notification, notification);

    app.type_char('x');
    assert_eq!(app.state().notification, notification);

    app.press(KeyCode::Esc);
    assert!(app.state().notification.is_none());
}

fn first_transcript_action_position(
    app: &mut TestApp,
) -> (
    crate::smelt_edit::DocPosition,
    smelt_core::buffer::SpanAction,
) {
    let total_rows = app.app.transcript_total_rows();
    for row in 0..total_rows {
        let display_rows = app.app.transcript_rows_and_breaks_range(row, 1);
        let Some(display_row) = display_rows.rows.into_iter().next() else {
            continue;
        };
        let Some(action) = display_row.actions.into_iter().next() else {
            continue;
        };
        let byte_col = crate::smelt_edit::text::cell_to_byte(&display_row.text, action.cell_start);
        return (
            crate::smelt_edit::DocPosition { row, byte_col },
            action.action,
        );
    }
    panic!("transcript did not expose any display action");
}

#[test]
fn short_transcript_display_document_surfaces_actions() {
    let url = "https://example.test/short";
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(80, 16);
    app.app
        .push_block(smelt_core::transcript_model::Block::Text {
            content: format!("short [link]({url})"),
        });
    app.render_silent();
    assert!(
        !app.app.transcript_win().has_materialized_rows(),
        "short transcript should exercise the normal-buffer document path"
    );

    let (pos, action) = first_transcript_action_position(&mut app);
    assert_eq!(
        action,
        smelt_core::buffer::SpanAction::OpenUrl(url.to_string())
    );
    let mut doc =
        crate::smelt_edit::HostDisplayDocument::new(&mut app.app, crate::app::TRANSCRIPT_WIN);
    assert_eq!(
        crate::smelt_edit::DisplayDocument::action_at(&mut doc, pos),
        Some(smelt_core::buffer::SpanAction::OpenUrl(url.to_string()))
    );
}

#[test]
fn row_backed_transcript_display_document_surfaces_actions() {
    let url = "https://example.test/row-backed";
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(80, 16);
    app.app
        .push_block(smelt_core::transcript_model::Block::Text {
            content: format!("head [link]({url})"),
        });
    for i in 0..120 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("tail {i}"),
            });
    }
    app.render_silent();
    assert!(
        app.app.transcript_win().has_materialized_rows(),
        "large transcript should exercise the row-backed document path"
    );

    let (pos, action) = first_transcript_action_position(&mut app);
    assert_eq!(
        action,
        smelt_core::buffer::SpanAction::OpenUrl(url.to_string())
    );
    let mut doc =
        crate::smelt_edit::HostDisplayDocument::new(&mut app.app, crate::app::TRANSCRIPT_WIN);
    assert_eq!(
        crate::smelt_edit::DisplayDocument::action_at(&mut doc, pos),
        Some(smelt_core::buffer::SpanAction::OpenUrl(url.to_string()))
    );
}

#[test]
fn transcript_vim_gg_g_and_count_g_use_row_document() {
    let mut app = row_document_transcript_app(100, true);
    let total_rows = transcript_total_rows(&app);
    assert!(total_rows > 40, "test transcript should be row-backed");

    app.type_char('g');
    app.type_char('g');
    assert_eq!(transcript_row_cursor_row(&app), 0);

    app.type_char('G');
    assert_eq!(transcript_row_cursor_row(&app), total_rows - 1);

    app.type_char('2');
    app.type_char('5');
    app.type_char('G');
    assert_eq!(transcript_row_cursor_row(&app), 24);
}

#[test]
fn transcript_h_and_l_move_row_cursor_horizontally() {
    let mut app = row_document_transcript_app(100, true);
    let materialized_head = transcript_buffer_lines(&app, 1).pop().unwrap_or_default();

    app.type_char('g');
    app.type_char('g');

    let start = app.app.transcript_win().row_cursor().unwrap();
    assert_eq!(start.row, 0);
    let row = app
        .app
        .transcript_rows_and_breaks_range(start.row, 1)
        .into_text_rows()
        .pop()
        .expect("absolute transcript row");
    assert_ne!(row, materialized_head, "row 0 must be off-materialized");
    assert!(row.len() > 1, "row must allow horizontal motion: {row:?}");

    app.type_char('l');
    assert_eq!(
        app.app.transcript_win().row_cursor().unwrap().byte_col,
        crate::smelt_edit::text::next_char_boundary(&row, start.byte_col)
    );

    app.type_char('h');
    assert_eq!(
        app.app.transcript_win().row_cursor().unwrap().byte_col,
        start.byte_col
    );
}

#[test]
fn transcript_line_end_uses_absolute_row_text() {
    let mut app = row_document_transcript_app(100, true);
    let materialized_head = transcript_buffer_lines(&app, 1).pop().unwrap_or_default();

    app.type_char('g');
    app.type_char('g');

    let start = app.app.transcript_win().row_cursor().unwrap();
    assert_eq!(start.row, 0);
    let row = app
        .app
        .transcript_rows_and_breaks_range(start.row, 1)
        .into_text_rows()
        .pop()
        .expect("absolute transcript row");
    assert_ne!(row, materialized_head, "row 0 must be off-materialized");
    assert!(!row.is_empty(), "row must have an end: {row:?}");

    app.type_char('$');

    assert_eq!(
        app.app.transcript_win().row_cursor().unwrap().byte_col,
        crate::smelt_edit::text::prev_char_boundary(&row, row.len())
    );
}

#[test]
fn transcript_user_resize_keeps_viewport_top_content_stable() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(56, 20);
    let before = "before wrapping content ".repeat(6);
    app.app
        .push_block(smelt_core::transcript_model::Block::User {
            text: format!("{before}\nANCHOR stay at viewport top\nafter"),
            image_labels: vec![],
        });
    for i in 0..120 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("tail {i}"),
            });
    }
    app.render_silent();
    pin_transcript_top_to_line_containing(&mut app, "ANCHOR");

    app.set_terminal_size(96, 20);
    app.render_silent();

    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "viewport top moved to {:?}",
        transcript_viewport_top_line(&app)
    );

    app.set_terminal_size(56, 20);
    app.render_silent();

    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "viewport top moved to {:?}",
        transcript_viewport_top_line(&app)
    );
}

#[test]
fn transcript_resize_keeps_wrapped_line_anchor_visible() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(56, 20);
    let before = "before wrapping content ".repeat(10);
    let after = " after wrapping content".repeat(10);
    app.app
        .push_block(smelt_core::transcript_model::Block::Text {
            content: format!("{before} ANCHOR stay at viewport top {after}"),
        });
    for i in 0..120 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("tail {i}"),
            });
    }
    app.render_silent();
    pin_transcript_top_to_line_containing(&mut app, "ANCHOR");

    app.set_terminal_size(96, 20);
    app.render_silent();

    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("ANCHOR")),
        "viewport moved to {:?}",
        transcript_viewport_lines(&app)
    );

    app.set_terminal_size(40, 20);
    app.render_silent();

    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("ANCHOR")),
        "viewport moved to {:?}",
        transcript_viewport_lines(&app)
    );
}

#[test]
fn transcript_resize_keeps_markdown_anchor_visible() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(58, 20);
    app.app
        .push_block(smelt_core::transcript_model::Block::Text {
            content: format!(
                "# Heading\n\n{} ANCHOR markdown paragraph {}\n\n- {}\n- tail item",
                "paragraph with `inline code`, **bold text**, and wrap pressure".repeat(5),
                "after anchor content that continues wrapping".repeat(4),
                "list item with enough words to wrap around the viewport".repeat(5),
            ),
        });
    for i in 0..120 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("tail {i}"),
            });
    }
    app.render_silent();
    pin_transcript_viewport_to_line_containing(&mut app, "ANCHOR");

    app.set_terminal_size(96, 20);
    app.render_silent();
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("ANCHOR")),
        "viewport moved to {:?}",
        transcript_viewport_lines(&app)
    );

    app.set_terminal_size(42, 20);
    app.render_silent();
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("ANCHOR")),
        "viewport moved to {:?}",
        transcript_viewport_lines(&app)
    );
}

#[test]
fn transcript_height_resize_keeps_top_when_cursor_is_lower_in_viewport() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(64, 28);
    app.app
        .push_block(smelt_core::transcript_model::Block::Text {
            content: "ANCHOR pinned top before height resize".into(),
        });
    for i in 0..160 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("tail {i}"),
            });
    }
    app.render_silent();
    let anchor_scroll = pin_transcript_top_to_line_containing(&mut app, "ANCHOR");
    move_transcript_cursor_to_row(&mut app, anchor_scroll + 14);
    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "initial viewport top moved to {:?}; lines: {:?}",
        transcript_viewport_top_line(&app),
        transcript_viewport_lines(&app)
    );

    app.set_terminal_size(64, 12);
    app.render_silent();

    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "height resize moved viewport top to {:?}; lines: {:?}",
        transcript_viewport_top_line(&app),
        transcript_viewport_lines(&app)
    );
}

#[test]
fn transcript_width_resize_keeps_top_when_cursor_is_lower_in_viewport() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 28);
    app.app
        .push_block(smelt_core::transcript_model::Block::Text {
            content: format!(
                "{} ANCHOR pinned top before width resize {}",
                "leading wrapped text".repeat(4),
                "trailing wrapped text".repeat(8),
            ),
        });
    for i in 0..160 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("tail {i}"),
            });
    }
    app.render_silent();
    let anchor_scroll = pin_transcript_top_to_line_containing(&mut app, "ANCHOR");
    move_transcript_cursor_to_row(&mut app, anchor_scroll + 16);
    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "initial viewport top moved to {:?}; lines: {:?}",
        transcript_viewport_top_line(&app),
        transcript_viewport_lines(&app)
    );

    app.set_terminal_size(44, 28);
    app.render_silent();

    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "width resize moved viewport top to {:?}; lines: {:?}",
        transcript_viewport_top_line(&app),
        transcript_viewport_lines(&app)
    );
}

#[test]
fn transcript_tail_follow_keeps_cursor_fixed_relative_to_viewport() {
    let mut app = row_document_transcript_app(100, true);

    app.type_char('G');
    app.render_silent();

    let win = app.app.transcript_win();
    let viewport_rows = win
        .viewport
        .map(|v| v.rect.height)
        .expect("transcript viewport after render");
    let total_rows = transcript_total_rows(&app);
    assert_eq!(transcript_row_cursor_row(&app), total_rows - 1);

    // Put the transcript cursor on a visible row above the bottom, then
    // re-enable sticky-bottom scrolling. New transcript rows should move
    // the cursor to the row now under the same screen cell.
    for _ in 0..3 {
        app.type_char('k');
    }
    app.render_silent();
    let screen_row_before = app
        .app
        .transcript_win()
        .cursor_screen_row(viewport_rows)
        .expect("cursor should be visible before append");
    let cursor_before = transcript_row_cursor_row(&app);
    assert!(cursor_before < total_rows - 1);
    assert!(!app.app.transcript_win().is_following_tail());

    app.app.transcript_win_mut().follow_tail();
    app.render_silent();
    assert!(app.app.transcript_win().is_following_tail());
    assert_eq!(
        app.app.transcript_win().cursor_screen_row(viewport_rows),
        Some(screen_row_before)
    );

    for i in 0..10 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("new row {i:03} alpha beta"),
            });
    }
    app.render_silent();

    let win = app.app.transcript_win();
    assert!(
        transcript_total_rows(&app) > total_rows,
        "content was appended"
    );
    assert!(
        win.is_following_tail(),
        "should still be following tail after append"
    );
    assert_eq!(
        win.cursor_screen_row(viewport_rows),
        Some(screen_row_before),
        "cursor should stay fixed relative to the viewport"
    );
}

#[test]
fn transcript_vim_visual_yank_copies_document_range() {
    let mut app = row_document_transcript_app(100, true);

    app.type_char('g');
    app.type_char('g');
    app.type_char('V');
    app.type_char('6');
    app.type_char('0');
    app.type_char('G');
    app.type_char('y');

    let yank = app.app.core.clipboard.kill_ring.current();
    assert!(yank.contains("row 000 alpha beta"), "yank was {yank:?}");
    assert!(yank.contains("row 029 alpha beta"), "yank was {yank:?}");
    let now = app.app.core.clock.instant_now();
    let win = app.app.transcript_win();
    let buf = app.app.ui.buf(win.buf).unwrap();
    assert!(win.row_selection_range(buf, now).is_some());
    app.feed_one(SourceEvent::Tick(300));
    let now = app.app.core.clock.instant_now();
    let win = app.app.transcript_win();
    let buf = app.app.ui.buf(win.buf).unwrap();
    assert!(win.row_selection_range(buf, now).is_none());
}

#[test]
fn transcript_vim_visual_char_starts_at_cursor() {
    let mut app = row_document_transcript_app(100, true);

    // Move to row 003 (1-indexed in vim, so 0-indexed row 2)
    app.type_char('3');
    app.type_char('G');
    assert_eq!(transcript_row_cursor_row(&app), 2);

    // Enter visual mode
    app.type_char('v');

    // Render and inspect selection highlights
    app.render_silent();
    let win = app.app.transcript_win();
    let scroll_top = win.scroll_top();
    let row_base = 0;
    let highlights = app
        .app
        .transcript_selection_highlights(scroll_top, row_base, 16);
    // Visual mode includes the character under the cursor, matching nvim.
    let rows: Vec<usize> = highlights.iter().map(|(line, _, _)| *line).collect();
    assert!(
        rows.contains(&2),
        "visual selection should include cursor row 2, got {highlights:?}"
    );

    // Move down one row
    app.type_char('j');
    app.render_silent();
    let highlights = app
        .app
        .transcript_selection_highlights(scroll_top, row_base, 16);
    // Should highlight rows 2
    let rows: Vec<usize> = highlights.iter().map(|(line, _, _)| *line).collect();
    assert!(
        rows.contains(&2),
        "visual selection should include row 2, got rows {rows:?}"
    );
    assert!(
        !rows.contains(&0),
        "visual selection should not include row 0, got rows {rows:?}"
    );

    // Now simulate mouse down while in visual mode.
    // This clears selection_anchor but NOT vim_mode, which used to trigger
    // the fallback path and select the whole buffer.
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: 5,
        column: 10,
        modifiers: crossterm::event::KeyModifiers::empty(),
    })));
    app.render_silent();
    let highlights = app
        .app
        .transcript_selection_highlights(scroll_top, row_base, 16);
    assert!(
        highlights.is_empty(),
        "mouse down after visual should clear selection, got {highlights:?}"
    );
}

#[test]
fn transcript_jump_to_bottom_preserves_cursor_screen_row() {
    let mut app = row_document_transcript_app(100, true);
    let win = app.app.transcript_win();
    let viewport_rows = win
        .viewport
        .map(|v| v.rect.height)
        .expect("transcript viewport after render");
    let buf_id = win.buf;

    // Scroll up so the viewport is no longer at the tail. Wheel-style panning
    // keeps the cursor on the same screen row.
    {
        let (w, buf) = app
            .app
            .ui
            .win_and_buf_mut(crate::app::TRANSCRIPT_WIN, buf_id);
        let w = w.expect("transcript window");
        let buf = buf.expect("transcript buffer");
        w.pan_by_lines(buf, -(viewport_rows as isize * 3), viewport_rows);
    }

    app.render_silent();
    let win = app.app.transcript_win();
    assert!(
        !win.is_following_tail(),
        "scrolled up should break tail-follow"
    );
    let screen_row_before = win
        .cursor_screen_row(viewport_rows)
        .expect("cursor should be visible");

    // Jump to bottom via the same API the bottom pill uses.
    {
        let (w, buf) = app
            .app
            .ui
            .win_and_buf_mut(crate::app::TRANSCRIPT_WIN, buf_id);
        let w = w.expect("transcript window");
        let buf = buf.expect("transcript buffer");
        w.jump_to_bottom(buf);
    }
    app.render_silent();

    let win = app.app.transcript_win();
    assert!(win.is_following_tail(), "jump_to_bottom should engage tail");
    assert_eq!(
        win.cursor_screen_row(viewport_rows),
        Some(screen_row_before),
        "cursor should stay on the same screen row after jumping to bottom"
    );
}
