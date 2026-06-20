use super::*;

#[test]
fn signal_api_reads_sets_and_subscribes_to_values() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        _G.initial_work_state = smelt.signal("work_state"):get()
        smelt.signal("work_state"):subscribe(function(value, previous)
            _G.signal_transition = previous .. "->" .. value
        end)
        smelt.signal("work_state"):set("testing")
        "#
    ));
    app.app.drain_signals_pending();

    let globals = app.app.lua.lua.globals();
    assert_eq!(
        globals.get::<String>("initial_work_state").ok().as_deref(),
        Some("idle")
    );
    assert_eq!(
        globals.get::<String>("signal_transition").ok().as_deref(),
        Some("idle->testing")
    );
}

#[test]
fn events_on_subscribes_to_event_shaped_signals() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        smelt.events.on("turn_start", function(payload)
            _G.turn_start_seen = payload.kind
        end)
        smelt.events.emit("turn_start", { kind = "manual" })
        "#
    ));
    app.app.drain_signals_pending();

    let globals = app.app.lua.lua.globals();
    assert_eq!(
        globals.get::<String>("turn_start_seen").ok().as_deref(),
        Some("manual")
    );
}

#[test]
fn custom_events_declare_and_emit_through_events_api() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        smelt.events.on("plugin:ready", function(payload)
            _G.custom_event_seen = payload.answer
        end)
        smelt.events.emit("plugin:ready", { answer = 42 })
        "#
    ));
    app.app.drain_signals_pending();

    let globals = app.app.lua.lua.globals();
    assert_eq!(globals.get::<i64>("custom_event_seen").ok(), Some(42));
}

#[test]
fn events_emit_does_not_replace_signal_value() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        smelt.events.new("plugin:tick")
        _G.before_tick = smelt.signal("plugin:tick"):get()
        smelt.events.on("plugin:tick", function(payload)
            _G.tick_payload = payload.value
        end)
        smelt.events.emit("plugin:tick", { value = 7 })
        _G.after_tick = smelt.signal("plugin:tick"):get()
        "#
    ));
    app.app.drain_signals_pending();

    let globals = app.app.lua.lua.globals();
    assert!(globals
        .get::<mlua::Value>("before_tick")
        .is_ok_and(|v| v.is_nil()));
    assert!(globals
        .get::<mlua::Value>("after_tick")
        .is_ok_and(|v| v.is_nil()));
    assert_eq!(globals.get::<i64>("tick_payload").ok(), Some(7));
}

#[test]
fn display_only_resume_sets_resume_hint_state() {
    let mut app = TestApp::builder().build();
    let mut session =
        smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
    session.id = "display-session".into();
    let mut transcript = smelt_core::content::transcript::Transcript::new();
    transcript.push(smelt_core::transcript_model::Block::Text {
        content: "restored transcript".into(),
    });

    app.app.load_session_display_only(
        session,
        crate::app::transcript::LoadedTranscript::full(transcript),
        crate::app::DeferredSessionLoad {
            id: "full-session".into(),
            history_len: 0,
            checkpoint: None,
        },
    );

    assert!(app.app.core.session.history.is_empty());
    assert_eq!(
        app.app
            .deferred_session_load
            .as_ref()
            .map(|deferred| deferred.id.as_str()),
        Some("full-session")
    );
    assert!(app.app.has_resume_hint_messages());
    let shutdown = app.app.shutdown_context();
    assert_eq!(shutdown.session_id, "display-session");
    assert!(shutdown.has_messages);
    let state = app
        .app
        .shared_session
        .lock()
        .unwrap()
        .clone()
        .expect("shared session state");
    assert_eq!(state.id, "display-session");
    assert!(state.has_messages);
}

#[test]
fn shared_session_state_uses_resume_hint_message_state() {
    let mut app = TestApp::builder().build();
    app.app.deferred_session_load = Some(crate::app::DeferredSessionLoad {
        id: "saved-session".into(),
        history_len: 0,
        checkpoint: None,
    });

    app.app.publish_shared_session_state();

    let state = app
        .app
        .shared_session
        .lock()
        .unwrap()
        .clone()
        .expect("shared session state");
    assert_eq!(state.id, app.app.core.session.id);
    assert!(state.has_messages);
}

#[test]
fn assembled_system_prompt_uses_engine_template() {
    let app = TestApp::builder().build();

    let prompt = app.app.assemble_system_prompt();

    assert!(prompt.contains("# Managed worktrees"));
    assert!(!prompt.contains("Working directory:"));
}

#[test]
fn system_prompt_override_replaces_tui_prompt() {
    let mut app = TestApp::builder().build();
    app.app.prompt_inputs.system_prompt_override = Some("custom prompt".into());
    app.app.prompt_inputs.instructions = Some("ignored instructions".into());
    app.app.prompt_inputs.skill_section = Some("# Skills\nignored".into());

    assert_eq!(app.app.assemble_system_prompt(), "custom prompt");
}

#[test]
fn system_prompt_omits_tool_guidance_when_tool_calling_disabled() {
    let mut app = TestApp::builder().build();
    app.app.core.config.model_config.tool_calling = Some(false);

    let prompt = app.app.assemble_system_prompt();

    assert!(!prompt.contains("# Tools"));
    assert!(!prompt.contains("read_file"));
    assert!(prompt.contains("# Code"));
}

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
    assert_eq!(
        app.app.document_action_at(crate::app::TRANSCRIPT_WIN, pos),
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
    assert_eq!(
        app.app.document_action_at(crate::app::TRANSCRIPT_WIN, pos),
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
fn transcript_line_end_uses_absolute_document_row() {
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
fn transcript_pinned_bottom_resize_click_selects_clicked_row_without_tail_snap() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = row_document_transcript_app(180, false);
    app.app.transcript_win_mut().follow_tail();
    app.render_silent();
    assert!(app.app.transcript_win().is_following_tail());

    app.set_terminal_size(80, 12);
    app.render_silent();
    assert!(app.app.transcript_win().is_following_tail());

    let vp = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport after resize");
    let click_row = vp.rect.top.saturating_add(2);
    let click_col = vp
        .rect
        .left
        .saturating_add(vp.gutter_width)
        .saturating_add(3);
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: click_row,
        column: click_col,
        modifiers: KeyModifiers::empty(),
    };
    let expected = app
        .app
        .document_view_position_at_mouse_for_win(crate::app::TRANSCRIPT_WIN, mouse)
        .expect("clicked transcript row");
    let scroll_before = app.app.transcript_win().scroll_top();

    app.feed_one(SourceEvent::Term(Event::Mouse(mouse)));

    assert!(
        !app.app.transcript_win().is_following_tail(),
        "click selection must break tail-follow before the next projection"
    );
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        scroll_before,
        "mouse down should not move the transcript before projection"
    );
    assert_eq!(transcript_row_cursor_row(&app), expected.row);

    app.render_silent();

    assert!(
        !app.app.transcript_win().is_following_tail(),
        "selection must stay pinned after projection"
    );
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        scroll_before,
        "selection projection snapped away from the clicked viewport"
    );
    assert_eq!(transcript_row_cursor_row(&app), expected.row);
}

#[test]
fn resumed_heterogeneous_sparse_wheel_scroll_up_keeps_visible_records_monotonic() {
    let count = 260;
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(count, 78, 18);
    let loaded = app.app.transcript.history().descriptor_records().len();
    assert!(
        loaded < count / 2,
        "resume test must stay sparse, loaded={loaded}, count={count}"
    );

    let mut previous = first_visible_record_index(&app).expect("initial visible record marker");
    let mut saw_earlier_record = false;
    for step in 0..140 {
        let before_scroll = app.app.transcript_win().scroll_top();
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
        let after_scroll = app.app.transcript_win().scroll_top();
        let current = first_visible_record_index(&app).unwrap_or_else(|| {
            panic!(
                "step {step} rendered no visible record marker: scroll={after_scroll}, lines={:?}",
                transcript_viewport_lines(&app)
            )
        });

        assert!(
            after_scroll <= before_scroll,
            "step {step} scrolled back down: before={before_scroll}, after={after_scroll}, lines={:?}",
            transcript_viewport_lines(&app)
        );
        assert!(
            current <= previous,
            "step {step} remapped visible content downward: previous record={previous}, current record={current}, scroll={after_scroll}, lines={:?}",
            transcript_viewport_lines(&app)
        );
        saw_earlier_record |= current < previous;
        previous = current;
        if after_scroll == 0 {
            break;
        }
    }

    assert!(
        saw_earlier_record,
        "wheel scroll never reached an earlier record"
    );
}

#[test]
fn streaming_tool_and_compaction_updates_do_not_snap_scrolled_transcript_to_tail() {
    let mut app = row_document_transcript_app(180, false);
    app.app.transcript_win_mut().follow_tail();
    app.render_silent();
    assert!(app.app.transcript_win().is_following_tail());

    for _ in 0..12 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
    }
    let pinned_scroll = app.app.transcript_win().scroll_top();
    assert!(pinned_scroll > 0, "test must be scrolled away from top");
    assert!(
        !app.app.transcript_win().is_following_tail(),
        "wheel scroll must pin transcript before streaming updates"
    );

    app.app.start_tool(
        "stream-write-file".into(),
        "write_file".into(),
        protocol::StyledLines::from_plain("STREAMING_WRITE_FILE should stay hidden"),
        std::collections::HashMap::new(),
    );
    app.render_silent();
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        pinned_scroll,
        "streaming tool start snapped the pinned transcript"
    );
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .all(|line| !line.contains("STREAMING_WRITE_FILE")),
        "streaming tool tail became visible: {:?}",
        transcript_viewport_lines(&app)
    );

    app.push_compaction_preview("STREAMING_COMPACTION should stay hidden");
    app.render_silent();
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        pinned_scroll,
        "streaming compaction preview snapped the pinned transcript"
    );
    assert!(
        !app.app.transcript_win().is_following_tail(),
        "streaming updates must not re-enable tail-follow"
    );
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .all(|line| !line.contains("STREAMING_COMPACTION")),
        "streaming compaction tail became visible: {:?}",
        transcript_viewport_lines(&app)
    );
}

fn resumed_heterogeneous_transcript_app(
    count: usize,
    width: u16,
    height: u16,
) -> (TestApp, tempfile::TempDir) {
    let records = heterogeneous_resume_records(count);
    let dir = tempfile::tempdir().expect("session dir");
    crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records)
        .expect("write transcript descriptors");
    let loaded = crate::app::transcript::LoadedTranscript::tail_from_sqlite_dir(
        dir.path().to_path_buf(),
        width,
        height,
    )
    .expect("tail transcript");
    let mut app = TestApp::builder().build();
    app.set_terminal_size(width, height);
    app.app.transcript = crate::app::transcript::TranscriptDocument::from_loaded_transcript(loaded);
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    app.app.transcript_win_mut().follow_tail();
    app.render_silent();
    (app, dir)
}

fn heterogeneous_resume_records(count: usize) -> Vec<smelt_core::TranscriptBlockRecord> {
    use smelt_core::transcript_model::Block;

    let mut source = smelt_core::content::transcript::Transcript::new();
    for idx in 0..count {
        let marker = format!("record-{idx:04}");
        match idx % 10 {
            0 => source.push(Block::User {
                text: format!(
                    "{marker} user prompt with image labels and wrapped text {}",
                    "u ".repeat(12)
                ),
                image_labels: vec![format!("image-{idx}")],
            }),
            1 => source.push(Block::Text {
                content: format!(
                    "{marker} assistant paragraph\n\n```diff\n- old {idx}\n+ new {idx}\n```\n{}",
                    "markdown wrap ".repeat(20)
                ),
            }),
            2 => source.push(Block::Thinking {
                content: format!("{marker} thinking trace {}", "reasoning ".repeat(28)),
            }),
            3 => source.push(Block::CodeLine {
                content: format!("{marker} let value_{idx} = compute({idx});"),
                lang: "rust".into(),
            }),
            4 => source.push(Block::Exec {
                command: format!("echo {marker}"),
                output: format!("{marker} stdout line\n{}", "exec output ".repeat(18)),
            }),
            5 => source.push(Block::Compacted {
                summary: format!("{marker} compacted summary {}", "summary ".repeat(10)),
            }),
            6 => source.push(Block::CompactionPreview {
                summary: format!("{marker} streaming preview {}", "preview ".repeat(15)),
            }),
            7 => source.push(Block::ToolCall {
                call_id: format!("read-file-{idx}"),
                name: "read_file".into(),
                summary: protocol::StyledLines::from_plain(format!(
                    "{marker} read_file src/{idx}.rs"
                )),
                args: std::collections::HashMap::from([(
                    "file_path".to_string(),
                    serde_json::json!(format!("src/{idx}.rs")),
                )]),
            }),
            8 => source.push(Block::ToolCall {
                call_id: format!("grep-{idx}"),
                name: "grep".into(),
                summary: protocol::StyledLines::from_plain(format!("{marker} grep needle")),
                args: std::collections::HashMap::from([(
                    "pattern".to_string(),
                    serde_json::json!(marker),
                )]),
            }),
            _ => source.push(Block::ProcessStatus {
                text: format!("{marker} background process finished"),
                event: None,
            }),
        };
    }
    source.history.descriptor_records()
}

fn wheel_transcript(app: &mut TestApp, kind: crossterm::event::MouseEventKind) {
    let vp = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport");
    app.feed_one(SourceEvent::Term(Event::Mouse(
        crossterm::event::MouseEvent {
            kind,
            row: vp.rect.top.saturating_add(1),
            column: vp
                .rect
                .left
                .saturating_add(vp.gutter_width)
                .saturating_add(1),
            modifiers: KeyModifiers::empty(),
        },
    )));
}

fn first_visible_record_index(app: &TestApp) -> Option<usize> {
    transcript_viewport_lines(app)
        .into_iter()
        .find_map(|line| parse_record_index(&line))
}

fn parse_record_index(line: &str) -> Option<usize> {
    let start = line.find("record-")? + "record-".len();
    let digits: String = line[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    digits.parse().ok()
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
