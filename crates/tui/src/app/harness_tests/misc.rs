use super::*;
use crate::app::search::SearchDirection;
use crate::app::transcript_scroll_trace::{
    TranscriptDescriptorTraceRange, TranscriptProjectionTargetTrace, TranscriptScrollIntent,
    TranscriptScrollTraceFrame, TranscriptTraceAnchor,
};
use crate::content::render_plan::RenderNodeId;
use crate::smelt_edit::RowIndex;

#[test]
fn signal_api_reads_sets_and_subscribes_to_values() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        _G.initial_work_state = smelt.signal.get("work_state")
        smelt.signal.subscribe("work_state", function(value, previous)
            _G.signal_transition = previous .. "->" .. value
        end)
        smelt.signal.set("work_state", "testing")
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
        _G.before_tick = smelt.signal.get("plugin:tick")
        smelt.events.on("plugin:tick", function(payload)
            _G.tick_payload = payload.value
        end)
        smelt.events.emit("plugin:tick", { value = 7 })
        _G.after_tick = smelt.signal.get("plugin:tick")
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

    app.app.load_store_backed_session(
        session,
        crate::app::transcript::LoadedTranscript::full(transcript),
        crate::app::history::live_session_for_test("full-session".into(), 0, None),
    );

    assert!(app.app.core.session.history.is_empty());
    assert_eq!(
        app.app.live_session.as_ref().map(|live| live.id()),
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
    app.app.live_session = Some(crate::app::history::live_session_for_test(
        "saved-session".into(),
        0,
        None,
    ));

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
fn transcript_scroll_state_does_not_request_tail_repin_when_pinned_at_bottom() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = row_document_transcript_app(180, false);
    app.app.transcript_win_mut().follow_tail();
    app.render_silent();
    assert!(app.app.transcript_win().is_following_tail());

    let vp = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport");
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: vp.rect.bottom().saturating_sub(2),
        column: vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(3),
        modifiers: KeyModifiers::empty(),
    })));
    assert!(
        !app.app.transcript_win().is_following_tail(),
        "selection click should pin the transcript even at bottom"
    );

    assert!(app.run_lua(
        r#"
        local scroll = assert(smelt.win.transcript():scroll())
        _G.transcript_scroll_at_bottom = scroll.at_bottom
        _G.transcript_scroll_needs_tail_repin = scroll.needs_tail_repin
        _G.transcript_scroll_follow = scroll.follow
        "#
    ));
    let globals = app.app.lua.lua.globals();
    assert!(
        globals
            .get::<bool>("transcript_scroll_at_bottom")
            .expect("transcript at_bottom global"),
        "clicked bottom-pinned transcript should still report at_bottom"
    );
    assert!(
        !globals
            .get::<bool>("transcript_scroll_follow")
            .expect("transcript follow global"),
        "test setup should exercise pinned-not-following state"
    );
    assert!(
        !globals
            .get::<bool>("transcript_scroll_needs_tail_repin")
            .expect("transcript needs_tail_repin global"),
        "bottom pill state should stay false when the viewport is already at bottom"
    );
}

#[test]
fn transcript_interaction_trace_records_click_and_projection_events() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = row_document_transcript_app(80, false);
    app.app.transcript.set_scroll_trace_enabled(true);
    app.app.transcript.take_scroll_trace_interaction_events();
    app.render_silent();
    app.app.transcript.take_scroll_trace_interaction_events();

    let vp = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport");
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: vp.rect.top.saturating_add(2),
        column: vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(3),
        modifiers: KeyModifiers::empty(),
    })));
    app.render_silent();

    let events = app.app.transcript.take_scroll_trace_interaction_events();
    let kinds = events
        .iter()
        .map(|event| event.kind.as_str())
        .collect::<Vec<_>>();
    assert!(
        kinds.contains(&"document_mouse_before"),
        "missing click pre-state trace in {kinds:?}"
    );
    assert!(
        kinds.contains(&"document_mouse_after"),
        "missing click post-state trace in {kinds:?}"
    );
    assert!(
        kinds.contains(&"projection_frame"),
        "missing projection frame trace in {kinds:?}"
    );
}

#[test]
fn transcript_fast_scroll_jump_bottom_then_click_preserves_bottom_viewport() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (mut app, _dir) = resumed_heterogeneous_transcript_app(320, 78, 18);
    app.app.transcript.set_scroll_trace_enabled(true);
    app.app.transcript.take_scroll_trace_interaction_events();

    for _ in 0..180 {
        wheel_transcript(&mut app, MouseEventKind::ScrollUp);
        app.render_silent();
    }

    app.type_char('G');
    app.render_silent();
    let bottom_scroll = app.app.transcript_win().scroll_top();
    let vp = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport");
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: vp.rect.bottom().saturating_sub(2),
        column: vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(3),
        modifiers: KeyModifiers::empty(),
    };
    let expected = app
        .app
        .document_view_position_at_mouse_for_win(crate::app::TRANSCRIPT_WIN, mouse)
        .expect("clicked bottom transcript row");

    app.feed_one(SourceEvent::Term(Event::Mouse(mouse)));
    app.render_silent();

    let after_scroll = app.app.transcript_win().scroll_top();
    let after_cursor = transcript_row_cursor_row(&app);
    assert_eq!(
        after_cursor,
        expected.row,
        "bottom click cursor resolved to wrong row after fast sparse scroll; trace={:#?}",
        app.app.transcript.scroll_trace_interaction_events()
    );
    assert_eq!(
        after_scroll,
        bottom_scroll,
        "bottom click teleported transcript after fast sparse scroll; trace={:#?}",
        app.app.transcript.scroll_trace_interaction_events()
    );
}

#[test]
fn resumed_sparse_bottom_click_keeps_clicked_row_and_viewport() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (mut app, _dir) = resumed_heterogeneous_transcript_app(120, 60, 14);
    app.app.transcript_win_mut().set_vim_enabled(true);
    app.app
        .transcript_win_mut()
        .set_vim_mode(crate::smelt_edit::VimMode::Normal);
    app.app.transcript.set_scroll_trace_enabled(true);

    app.type_char('G');
    app.render_silent();
    app.app.transcript.take_scroll_trace_interaction_events();

    let vp = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport");
    let click_row = vp.rect.bottom().saturating_sub(1);
    let click_col = vp
        .rect
        .left
        .saturating_add(vp.gutter_width)
        .saturating_add(3);
    let rel_row = click_row.saturating_sub(vp.rect.top) as crate::smelt_edit::RowIndex;
    let before_scroll = app.app.transcript_win().scroll_top();
    let total_rows = app
        .app
        .transcript_win()
        .materialized_rows()
        .expect("resumed transcript should be row-materialized")
        .total_rows;
    let expected_row = before_scroll
        .saturating_add(rel_row)
        .min(total_rows.saturating_sub(1));

    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: click_row,
        column: click_col,
        modifiers: KeyModifiers::empty(),
    })));

    assert_eq!(
        app.app.transcript_win().scroll_top(),
        before_scroll,
        "mouse down should not scroll the resumed transcript; trace={:#?}",
        app.app.transcript.scroll_trace_interaction_events()
    );
    assert_eq!(
        transcript_row_cursor_row(&app),
        expected_row,
        "mouse down should put the cursor on the clicked transcript row; trace={:#?}",
        app.app.transcript.scroll_trace_interaction_events()
    );
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
fn resumed_sparse_scroll_up_reports_tail_repin_needed() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);
    let bottom_scroll = app.app.transcript_win().scroll_top();
    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    assert!(app.run_lua(
        r#"
        local scroll = assert(smelt.win.transcript():scroll())
        _G.transcript_scroll_at_bottom = scroll.at_bottom
        _G.transcript_scroll_needs_tail_repin = scroll.needs_tail_repin
        _G.transcript_scroll_follow = scroll.follow
        _G.transcript_scroll_top = scroll.top
        _G.transcript_scroll_max = scroll.max
        "#
    ));
    let globals = app.app.lua.lua.globals();
    assert!(
        !globals
            .get::<bool>("transcript_scroll_follow")
            .expect("transcript follow global"),
        "wheel up should leave tail-follow mode"
    );
    assert!(
        !globals
            .get::<bool>("transcript_scroll_at_bottom")
            .expect("transcript at_bottom global"),
        "wheel up should report off-bottom: bottom={bottom_scroll}, top={:?}, max={:?}, needs_tail_repin={:?}",
        globals.get::<u64>("transcript_scroll_top"),
        globals.get::<u64>("transcript_scroll_max"),
        globals.get::<bool>("transcript_scroll_needs_tail_repin")
    );
    assert!(
        globals
            .get::<bool>("transcript_scroll_needs_tail_repin")
            .expect("transcript needs_tail_repin global"),
        "wheel up should show the jump-to-bottom pill: top={:?}, max={:?}",
        globals.get::<u64>("transcript_scroll_top"),
        globals.get::<u64>("transcript_scroll_max")
    );
}

#[test]
fn resumed_sparse_jump_to_bottom_after_scroll_up_renders_tail() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);

    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    let before_tail_command = app.app.transcript_win().scroll_top();
    assert!(
        app.run_lua(r#"assert(smelt.win.transcript()):scroll("tail")"#),
        "jump-to-bottom lua command should succeed"
    );
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        before_tail_command,
        "transcript tail command should wait for projection instead of jumping to a sparse estimated row"
    );
    app.render_silent();

    let lines = transcript_viewport_lines(&app);
    assert!(
        lines.iter().any(|line| line.contains("record-025")),
        "jump-to-bottom should render tail records instead of an empty transcript: scroll={}, lines={lines:?}",
        app.app.transcript_win().scroll_top()
    );
    assert!(
        app.app.transcript_win().is_following_tail(),
        "jump-to-bottom should restore tail-follow"
    );
}

#[test]
fn resumed_sparse_scroll_down_to_tail_hides_jump_to_bottom() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);

    for _ in 0..140 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
    }
    assert!(
        !app.app.transcript_win().is_following_tail(),
        "test setup should leave tail-follow"
    );

    for _ in 0..200 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
        app.render_silent();
        if app.app.transcript_win().is_following_tail() {
            break;
        }
    }
    assert!(
        app.app.transcript_win().is_following_tail(),
        "wheel down should eventually reach semantic tail"
    );
    assert!(app.run_lua(
        r#"
        local scroll = assert(smelt.win.transcript():scroll())
        _G.transcript_scroll_at_bottom = scroll.at_bottom
        _G.transcript_scroll_needs_tail_repin = scroll.needs_tail_repin
        "#
    ));
    let globals = app.app.lua.lua.globals();
    assert!(
        globals
            .get::<bool>("transcript_scroll_at_bottom")
            .expect("transcript at_bottom global"),
        "tail should report at_bottom"
    );
    assert!(
        !globals
            .get::<bool>("transcript_scroll_needs_tail_repin")
            .expect("transcript needs_tail_repin global"),
        "jump-to-bottom pill should hide at semantic tail"
    );
}

fn previous_user_descriptor_index(app: &TestApp) -> usize {
    app.app
        .transcript
        .previous_navigation_block(Some("user"))
        .expect("previous user target")
        .descriptor_index
}

fn previous_user_record_index(app: &TestApp) -> usize {
    let target = app
        .app
        .transcript
        .previous_navigation_block(Some("user"))
        .expect("previous user target");
    parse_record_index(&target.first_line).unwrap_or_else(|| {
        panic!(
            "previous user target did not include a record marker: {:?}",
            target.first_line
        )
    })
}

fn previous_heterogeneous_user_before(index: usize) -> usize {
    index.saturating_sub(1) / 10 * 10
}

fn previous_tail_consecutive_user_before(index: usize, count: usize) -> usize {
    let first_tail_user = count.saturating_sub(2);
    if index > first_tail_user {
        first_tail_user
    } else {
        previous_heterogeneous_user_before(index)
    }
}

fn wheel_and_render(app: &mut TestApp, kind: crossterm::event::MouseEventKind) {
    wheel_transcript(app, kind);
    app.render_silent();
}

#[test]
fn resumed_sparse_jump_to_bottom_anchors_previous_user_to_visible_tail() {
    let (mut app, _dir) =
        resumed_transcript_app_from_records(tail_consecutive_user_records(260), 78, 18);

    for _ in 0..8 {
        wheel_and_render(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    }
    assert!(
        !app.app.transcript_win().is_following_tail(),
        "test setup should leave tail-follow mode"
    );

    assert!(
        app.run_lua(r#"assert(smelt.win.transcript()):scroll("tail")"#),
        "jump-to-bottom lua command should succeed"
    );
    app.render_silent();
    assert!(
        app.app.transcript_win().is_following_tail(),
        "jump-to-bottom should restore tail-follow"
    );

    let first_visible = first_visible_record_index(&app).expect("visible tail record");
    assert!(
        first_visible < 258,
        "test must keep consecutive tail users below the top visible anchor; first_visible={first_visible}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    let expected = previous_tail_consecutive_user_before(first_visible, 260);
    let target_after_jump = previous_user_record_index(&app);
    assert_eq!(
        target_after_jump,
        expected,
        "jump-to-bottom should anchor previous-user navigation to the visible tail, not the loaded sparse window start; first_visible={first_visible}, lines={:?}",
        transcript_viewport_lines(&app)
    );

    wheel_and_render(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    let first_visible_after_scroll =
        first_visible_record_index(&app).expect("visible record after scroll");
    let expected_after_scroll =
        previous_tail_consecutive_user_before(first_visible_after_scroll, 260);
    assert_eq!(
        previous_user_record_index(&app),
        expected_after_scroll,
        "a small scroll should not be required to repair the previous-user target"
    );
}

#[test]
fn resumed_sparse_tail_scroll_down_keeps_previous_user_target_stable() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);
    assert!(
        app.app.transcript_win().is_following_tail(),
        "resumed transcript test starts pinned to tail"
    );
    let bottom_scroll = app.app.transcript_win().scroll_top();
    let initial_target = previous_user_descriptor_index(&app);

    for _ in 0..3 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
        app.render_silent();
        assert!(
            app.app.transcript_win().is_following_tail(),
            "scrolling down at tail should remain tail-following"
        );
        assert_eq!(
            app.app.transcript_win().scroll_top(),
            bottom_scroll,
            "scrolling down at tail should not move the viewport"
        );
        assert_eq!(
            previous_user_descriptor_index(&app),
            initial_target,
            "top pill target changed after wheel-down input at tail"
        );

        app.render_silent();
        assert_eq!(
            previous_user_descriptor_index(&app),
            initial_target,
            "top pill target changed on the idle render after wheel-down at tail"
        );
    }
}

#[test]
fn resumed_sparse_near_tail_scroll_down_stays_incremental() {
    let count = 260;
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(count, 78, 18);
    let loaded = app.app.transcript.history().descriptor_records().len();
    assert!(
        loaded < count / 2,
        "resume test must stay sparse, loaded={loaded}, count={count}"
    );

    assert!(
        app.app.transcript_win().is_following_tail(),
        "resumed transcript test starts pinned to tail"
    );
    let bottom_scroll = app.app.transcript_win().scroll_top();

    for _ in 0..3 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
    }
    let after_up = app.app.transcript_win().scroll_top();
    assert!(
        after_up.saturating_add(3) < bottom_scroll,
        "test setup should remain near but off tail after one down tick: bottom={bottom_scroll}, after_up={after_up}"
    );
    assert!(
        !app.app.transcript_win().is_following_tail(),
        "wheel up should leave tail-follow mode"
    );

    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
    app.render_silent();
    let after_down = app.app.transcript_win().scroll_top();
    assert_eq!(
        after_down,
        after_up.saturating_add(3),
        "near-tail wheel down should move by one wheel tick, not snap to tail; bottom={bottom_scroll}, after_up={after_up}, after_down={after_down}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    assert!(
        after_down < bottom_scroll,
        "near-tail wheel down should remain off semantic tail: bottom={bottom_scroll}, after_down={after_down}"
    );
    assert!(
        !app.app.transcript_win().is_following_tail(),
        "near-tail wheel down should not re-enter tail-follow"
    );
}

#[test]
fn resumed_sparse_scroll_down_after_scroll_up_does_not_snap_to_tail() {
    let count = 260;
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(count, 78, 18);
    let loaded = app.app.transcript.history().descriptor_records().len();
    assert!(
        loaded < count / 2,
        "resume test must stay sparse, loaded={loaded}, count={count}"
    );

    assert!(
        app.app.transcript_win().is_following_tail(),
        "resumed transcript test starts pinned to tail"
    );
    let bottom_scroll = app.app.transcript_win().scroll_top();

    for _ in 0..140 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
    }
    let after_up = app.app.transcript_win().scroll_top();
    assert!(
        after_up < bottom_scroll,
        "wheel up should move away from tail: bottom={bottom_scroll}, after_up={after_up}"
    );
    assert!(
        !app.app.transcript_win().is_following_tail(),
        "wheel up should leave tail-follow mode"
    );

    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
    app.render_silent();
    let after_down = app.app.transcript_win().scroll_top();
    assert_eq!(
        after_down,
        after_up.saturating_add(3),
        "first wheel down should move by one wheel tick, not snap to tail; bottom={bottom_scroll}, after_up={after_up}, after_down={after_down}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    assert!(
        after_down < bottom_scroll,
        "first wheel down should remain off tail: bottom={bottom_scroll}, after_down={after_down}"
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

#[derive(Clone, Copy, Debug)]
enum TranscriptReplayStep {
    WheelUp { ticks: usize },
    WheelDown { ticks: usize },
    CoalescedWheel { rows: isize },
    DragAutoscrollTop { ticks: usize },
    DragAutoscrollBottom { ticks: usize },
    Resize { width: u16, height: u16 },
    StreamingAppend,
    ScrollbarFarSeek { rel_row: u16 },
    WheelUpUntilDescriptorRangeChanges { max_ticks: usize },
}

#[derive(Default)]
struct TranscriptScrollReplayReport {
    frames: Vec<TranscriptScrollTraceFrame>,
    descriptor_range_changed: bool,
}

struct TranscriptScrollReplay {
    steps: Vec<TranscriptReplayStep>,
}

impl TranscriptScrollReplay {
    fn new(steps: Vec<TranscriptReplayStep>) -> Self {
        Self { steps }
    }

    fn run(self, app: &mut TestApp) -> TranscriptScrollReplayReport {
        app.app.transcript.set_scroll_trace_timings_enabled(true);
        app.app.transcript.take_scroll_trace_frames();
        let mut report = TranscriptScrollReplayReport::default();
        for step in self.steps {
            match step {
                TranscriptReplayStep::WheelUp { ticks } => {
                    for tick in 0..ticks {
                        let before = app.app.transcript_win().scroll_top();
                        wheel_transcript(app, crossterm::event::MouseEventKind::ScrollUp);
                        set_replay_trace_input(
                            app,
                            format!("wheel_up:{tick}"),
                            TranscriptScrollIntent::UserDelta { rows: -3 },
                            before,
                        );
                        render_replay_frame(app, &mut report);
                    }
                }
                TranscriptReplayStep::WheelDown { ticks } => {
                    for tick in 0..ticks {
                        let before = app.app.transcript_win().scroll_top();
                        wheel_transcript(app, crossterm::event::MouseEventKind::ScrollDown);
                        set_replay_trace_input(
                            app,
                            format!("wheel_down:{tick}"),
                            TranscriptScrollIntent::UserDelta { rows: 3 },
                            before,
                        );
                        render_replay_frame(app, &mut report);
                    }
                }
                TranscriptReplayStep::CoalescedWheel { rows } => {
                    let before = app.app.transcript_win().scroll_top();
                    let (row, col) = transcript_content_point(app, 1);
                    assert!(
                        app.app.scroll_at_with_transcript_intent(
                            row,
                            col,
                            rows,
                            format!("coalesced_wheel:{rows}"),
                        ),
                        "coalesced wheel replay should pan the transcript"
                    );
                    set_replay_trace_input(
                        app,
                        format!("coalesced_wheel:{rows}"),
                        TranscriptScrollIntent::UserDelta { rows },
                        before,
                    );
                    render_replay_frame(app, &mut report);
                }
                TranscriptReplayStep::DragAutoscrollTop { ticks } => {
                    start_transcript_edge_drag(app, TranscriptDragEdge::Top);
                    for tick in 0..ticks {
                        let before = app.app.transcript_win().scroll_top();
                        if app.app.tick_drag_autoscroll_with_transcript_intent() {
                            set_replay_trace_input(
                                app,
                                format!("drag_autoscroll_top:{tick}"),
                                TranscriptScrollIntent::UserDelta { rows: -1 },
                                before,
                            );
                            render_replay_frame(app, &mut report);
                        }
                    }
                    finish_transcript_drag(app);
                }
                TranscriptReplayStep::DragAutoscrollBottom { ticks } => {
                    start_transcript_edge_drag(app, TranscriptDragEdge::Bottom);
                    for tick in 0..ticks {
                        let before = app.app.transcript_win().scroll_top();
                        if app.app.tick_drag_autoscroll_with_transcript_intent() {
                            set_replay_trace_input(
                                app,
                                format!("drag_autoscroll_bottom:{tick}"),
                                TranscriptScrollIntent::UserDelta { rows: 1 },
                                before,
                            );
                            render_replay_frame(app, &mut report);
                        }
                    }
                    finish_transcript_drag(app);
                }
                TranscriptReplayStep::Resize { width, height } => {
                    let before = app.app.transcript_win().scroll_top();
                    let previous_width = app
                        .app
                        .transcript_win()
                        .viewport
                        .map(|viewport| viewport.content_width)
                        .unwrap_or(width);
                    app.set_terminal_size(width, height);
                    set_replay_trace_input(
                        app,
                        format!("resize:{width}x{height}"),
                        TranscriptScrollIntent::ResizeReflow { previous_width },
                        before,
                    );
                    render_replay_frame(app, &mut report);
                }
                TranscriptReplayStep::StreamingAppend => {
                    let before = app.app.transcript_win().scroll_top();
                    app.app
                        .push_block(smelt_core::transcript_model::Block::Text {
                            content: "replay streaming append should not move pinned viewport"
                                .into(),
                        });
                    set_replay_trace_input(
                        app,
                        "streaming_append".to_string(),
                        TranscriptScrollIntent::PreserveViewport,
                        before,
                    );
                    render_replay_frame(app, &mut report);
                }
                TranscriptReplayStep::ScrollbarFarSeek { rel_row } => {
                    let before = app.app.transcript_win().scroll_top();
                    let (row, col, numerator, denominator) =
                        transcript_scrollbar_point(app, rel_row);
                    app.feed_one(SourceEvent::Term(Event::Mouse(
                        crossterm::event::MouseEvent {
                            kind: crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Left,
                            ),
                            row,
                            column: col,
                            modifiers: KeyModifiers::empty(),
                        },
                    )));
                    set_replay_trace_input(
                        app,
                        format!("scrollbar_far_seek:{rel_row}"),
                        TranscriptScrollIntent::ScrollbarFraction {
                            numerator,
                            denominator,
                        },
                        before,
                    );
                    render_replay_frame(app, &mut report);
                    app.feed_one(SourceEvent::Term(Event::Mouse(
                        crossterm::event::MouseEvent {
                            kind: crossterm::event::MouseEventKind::Up(
                                crossterm::event::MouseButton::Left,
                            ),
                            row,
                            column: col,
                            modifiers: KeyModifiers::empty(),
                        },
                    )));
                }
                TranscriptReplayStep::WheelUpUntilDescriptorRangeChanges { max_ticks } => {
                    for tick in 0..max_ticks {
                        if report.descriptor_range_changed {
                            break;
                        }
                        let before = app.app.transcript_win().scroll_top();
                        wheel_transcript(app, crossterm::event::MouseEventKind::ScrollUp);
                        set_replay_trace_input(
                            app,
                            format!("wheel_up_window_probe:{tick}"),
                            TranscriptScrollIntent::UserDelta { rows: -3 },
                            before,
                        );
                        render_replay_frame(app, &mut report);
                    }
                }
            }
        }
        report
    }
}

#[derive(Clone, Copy)]
enum TranscriptDragEdge {
    Top,
    Bottom,
}

fn render_replay_frame(app: &mut TestApp, report: &mut TranscriptScrollReplayReport) {
    app.render_silent();
    let frames = app.app.transcript.take_scroll_trace_frames();
    report.descriptor_range_changed |= frames
        .iter()
        .any(|frame| frame.active_descriptor_range_before != frame.active_descriptor_range_after);
    report.frames.extend(frames);
}

fn set_replay_trace_input(
    app: &mut TestApp,
    input_event_or_tick: String,
    scroll_intent: TranscriptScrollIntent,
    window_scroll_before: crate::smelt_edit::RowIndex,
) {
    let window_scroll_after_input = app.app.transcript_win().scroll_top();
    app.app.transcript.set_next_scroll_trace_input(
        crate::app::transcript_scroll_trace::TranscriptScrollTraceRenderInput {
            input_event_or_tick,
            scroll_intent,
            window_scroll_before,
            window_scroll_after_input,
        },
    );
}

fn transcript_content_point(app: &TestApp, rel_row: u16) -> (u16, u16) {
    let vp = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport");
    (
        vp.rect
            .top
            .saturating_add(rel_row.min(vp.rect.height.saturating_sub(1))),
        vp.rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(1),
    )
}

fn transcript_scrollbar_point(app: &TestApp, rel_row: u16) -> (u16, u16, u64, u64) {
    let vp = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport");
    let scrollbar = vp.scrollbar.expect("transcript scrollbar");
    let denominator = u64::from(vp.rect.height.saturating_sub(1).max(1));
    let numerator = u64::from(rel_row.min(vp.rect.height.saturating_sub(1)));
    (
        vp.rect
            .top
            .saturating_add(rel_row.min(vp.rect.height.saturating_sub(1))),
        scrollbar.col,
        numerator,
        denominator,
    )
}

fn start_transcript_edge_drag(app: &mut TestApp, edge: TranscriptDragEdge) {
    let vp = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport");
    let col = vp
        .rect
        .left
        .saturating_add(vp.gutter_width)
        .saturating_add(1);
    let down_row = vp.rect.top.saturating_add(vp.rect.height / 2);
    let edge_row = match edge {
        TranscriptDragEdge::Top => vp.rect.top,
        TranscriptDragEdge::Bottom => vp.rect.bottom().saturating_sub(1),
    };
    app.feed_one(SourceEvent::Term(Event::Mouse(
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            row: down_row,
            column: col,
            modifiers: KeyModifiers::empty(),
        },
    )));
    app.feed_one(SourceEvent::Term(Event::Mouse(
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            row: edge_row,
            column: col,
            modifiers: KeyModifiers::empty(),
        },
    )));
}

fn finish_transcript_drag(app: &mut TestApp) {
    let (row, col) = transcript_content_point(app, 1);
    app.feed_one(SourceEvent::Term(Event::Mouse(
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            row,
            column: col,
            modifiers: KeyModifiers::empty(),
        },
    )));
}

#[derive(Clone, Debug)]
struct RevealedUserBlock {
    descriptor_index: usize,
    block_id: u64,
    first_line: String,
}

fn reveal_user_block_via_lua(app: &mut TestApp, direction: &str) -> RevealedUserBlock {
    let snippet = match direction {
        "previous" => {
            r#"
            local block = assert(smelt.transcript.previous_block({ role = "user" }))
            assert(block.role == "user")
            assert(smelt.transcript.reveal_block(block.descriptor_index, { top_padding = 1, cursor = true }))
            _G.transcript_revealed_descriptor_index = block.descriptor_index
            _G.transcript_revealed_block_id = block.block_id
            _G.transcript_revealed_first_line = block.first_line
            "#
        }
        "next" => {
            r#"
            local block = assert(smelt.transcript.next_block({ role = "user" }))
            assert(block.role == "user")
            assert(smelt.transcript.reveal_block(block.descriptor_index, { top_padding = 1, cursor = true }))
            _G.transcript_revealed_descriptor_index = block.descriptor_index
            _G.transcript_revealed_block_id = block.block_id
            _G.transcript_revealed_first_line = block.first_line
            "#
        }
        other => panic!("unsupported transcript reveal direction {other}"),
    };
    assert!(app.run_lua(snippet));
    let globals = app.app.lua.lua.globals();
    RevealedUserBlock {
        descriptor_index: globals
            .get::<usize>("transcript_revealed_descriptor_index")
            .expect("revealed descriptor index"),
        block_id: globals
            .get::<u64>("transcript_revealed_block_id")
            .expect("revealed block id"),
        first_line: globals
            .get::<String>("transcript_revealed_first_line")
            .expect("revealed first line"),
    }
}

fn assert_reveal_block_frame(frame: &TranscriptScrollTraceFrame, block: &RevealedUserBlock) {
    assert_eq!(frame.input_event_or_tick, "reveal_block");
    assert!(
        !frame.placeholder_rows_visible,
        "semantic block reveal must not expose sparse placeholders: {frame:?}"
    );
    assert!(
        frame.first_visible_content_anchor.is_some(),
        "semantic block reveal must resolve an exact content anchor: {frame:?}"
    );
    match frame.scroll_intent {
        TranscriptScrollIntent::RevealBlock {
            descriptor_index,
            block_id,
            row_offset,
            screen_padding_top,
        } => {
            assert_eq!(descriptor_index, block.descriptor_index);
            assert_eq!(
                block_id,
                smelt_core::transcript_model::BlockId::new(block.block_id)
            );
            assert_eq!(row_offset, 0);
            assert_eq!(screen_padding_top, 1);
        }
        ref intent => panic!("semantic user navigation collapsed to wrong intent: {intent:?}"),
    }
}

#[test]
fn transcript_previous_and_next_user_reveals_are_full_frame_semantic() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(340, 78, 18);
    app.app.transcript.set_scroll_trace_enabled(true);
    app.app.transcript.take_scroll_trace_frames();

    let previous = reveal_user_block_via_lua(&mut app, "previous");
    app.render_silent();
    let frames = app.app.transcript.take_scroll_trace_frames();
    let frame = frames.first().expect("previous user reveal frame");
    assert_reveal_block_frame(frame, &previous);
    let lines = transcript_viewport_lines(&app);
    let previous_marker = previous
        .first_line
        .split_whitespace()
        .next()
        .expect("previous user marker");
    assert!(
        lines.iter().any(|line| line.contains(previous_marker)),
        "previous user target was not revealed in the viewport: target={:?}, lines={lines:?}",
        previous.first_line
    );

    let older = reveal_user_block_via_lua(&mut app, "previous");
    app.render_silent();
    let frames = app.app.transcript.take_scroll_trace_frames();
    let frame = frames.first().expect("older previous user reveal frame");
    assert_reveal_block_frame(frame, &older);
    let lines = transcript_viewport_lines(&app);
    let older_marker = older
        .first_line
        .split_whitespace()
        .next()
        .expect("older previous user marker");
    assert!(
        lines.iter().any(|line| line.contains(older_marker)),
        "second previous user target was not revealed in the viewport: target={:?}, lines={lines:?}",
        older.first_line
    );
    assert!(
        older.descriptor_index < previous.descriptor_index,
        "repeated previous-user reveal should walk backward by descriptor identity: previous={previous:?}, older={older:?}"
    );

    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        SearchDirection::Forward,
        "record-0291".to_string(),
    );
    app.render_silent();
    app.app.transcript.take_scroll_trace_frames();

    let next = reveal_user_block_via_lua(&mut app, "next");
    app.render_silent();
    let frames = app.app.transcript.take_scroll_trace_frames();
    let frame = frames.first().expect("next user reveal frame");
    assert_reveal_block_frame(frame, &next);
    let lines = transcript_viewport_lines(&app);
    let next_marker = next
        .first_line
        .split_whitespace()
        .next()
        .expect("next user marker");
    assert!(
        lines.iter().any(|line| line.contains(next_marker)),
        "next user target was not revealed in the viewport: target={:?}, lines={lines:?}",
        next.first_line
    );
    assert!(
        next.descriptor_index > previous.descriptor_index,
        "next user reveal should move forward by descriptor identity: previous={previous:?}, next={next:?}"
    );
}

fn visible_anchor_order(frame: &TranscriptScrollTraceFrame) -> Option<(u8, u64, u64)> {
    let anchor = frame.first_visible_content_anchor?;
    match anchor.node_id {
        RenderNodeId::Block(id) => Some((0, id.get(), anchor.row_offset)),
        RenderNodeId::Group(id) => Some((1, id, anchor.row_offset)),
    }
}

fn assert_monotonic_visible_anchors(frames: &[TranscriptScrollTraceFrame], upward: bool) {
    let mut previous = None;
    let mut compared = 0;
    for frame in frames {
        let Some(current) = visible_anchor_order(frame) else {
            continue;
        };
        if let Some(previous) = previous {
            if upward {
                assert!(
                    current <= previous,
                    "visible content anchor moved down during upward replay: previous={previous:?}, current={current:?}, frame={frame:?}"
                );
            } else {
                assert!(
                    current >= previous,
                    "visible content anchor moved up during downward replay: previous={previous:?}, current={current:?}, frame={frame:?}"
                );
            }
            compared += 1;
        }
        previous = Some(current);
    }
    assert!(
        compared > 0,
        "replay did not produce comparable content anchors"
    );
}

fn assert_local_scroll_frames_are_exact_and_fast(frames: &[TranscriptScrollTraceFrame]) {
    assert!(!frames.is_empty(), "expected local scroll frames");
    for frame in frames {
        assert!(
            !frame.placeholder_rows_visible,
            "local scroll should not land in sparse placeholders: {frame:?}"
        );
        let ms = frame
            .render_or_projection_ms
            .expect("replay should enable projection timings");
        assert!(
            ms <= 250,
            "projection frame exceeded latency budget: {ms}ms in {frame:?}"
        );
        assert!(
            frame.first_visible_content_anchor.is_some(),
            "local scroll should resolve an exact visible content anchor: {frame:?}"
        );
    }
}

fn assert_user_delta_targets_exact_rows(frames: &[TranscriptScrollTraceFrame]) {
    assert!(!frames.is_empty(), "expected user delta frames");
    for frame in frames {
        let TranscriptScrollIntent::UserDelta { .. } = frame.scroll_intent else {
            continue;
        };
        assert!(
            matches!(
                frame.projection_target,
                TranscriptProjectionTargetTrace::ExactRow(_)
            ),
            "user delta should project through an exact local row: {frame:?}"
        );
    }
}

fn assert_user_delta_inputs_do_not_pre_scroll(frames: &[TranscriptScrollTraceFrame]) {
    assert!(!frames.is_empty(), "expected user delta frames");
    assert!(
        frames
            .iter()
            .filter(|frame| matches!(
                frame.scroll_intent,
                TranscriptScrollIntent::UserDelta { .. }
            ))
            .all(|frame| frame.window_scroll_after_input == frame.window_scroll_before),
        "local user delta mutated Window::scroll_top before transcript projection: {frames:?}"
    );
}

#[derive(Clone, Copy, Debug)]
enum TranscriptBurstKey {
    CtrlD,
    CtrlU,
    Down,
    Up,
}

impl TranscriptBurstKey {
    fn press(self, app: &mut TestApp) {
        match self {
            Self::CtrlD => app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Self::CtrlU => app.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Self::Down => app.press(KeyCode::Down),
            Self::Up => app.press(KeyCode::Up),
        }
    }

    fn moves_down(self) -> bool {
        matches!(self, Self::CtrlD | Self::Down)
    }

    fn max_rows_per_event(self, viewport_rows: u16) -> RowIndex {
        match self {
            Self::CtrlD | Self::CtrlU => RowIndex::from(viewport_rows.max(1)),
            Self::Down | Self::Up => 1,
        }
    }
}

fn assert_burst_projection_delta_bounded(
    label: &str,
    frames: &[TranscriptScrollTraceFrame],
    before_scroll: RowIndex,
    max_delta: RowIndex,
) {
    let mut checked = 0;
    for frame in frames {
        let TranscriptScrollIntent::UserDelta { .. } = frame.scroll_intent else {
            continue;
        };
        let TranscriptProjectionTargetTrace::ExactRow(target) = frame.projection_target else {
            panic!("{label} projected user delta through a non-exact target: {frame:?}");
        };
        let delta = target.abs_diff(before_scroll);
        assert!(
            delta <= max_delta,
            "{label} over-accumulated a no-render key burst: before_scroll={before_scroll}, target={target}, delta={delta}, max_delta={max_delta}, frame={frame:?}"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "{label} produced no user-delta projection frames: {frames:?}"
    );
}

fn run_transcript_key_burst(app: &mut TestApp, label: &str, key: TranscriptBurstKey) {
    const BURST_EVENTS: usize = 80;

    let viewport_rows = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport")
        .rect
        .height
        .max(1);
    let before_scroll = app.app.transcript_win().scroll_top();
    app.app.transcript.set_scroll_trace_timings_enabled(true);
    app.app.transcript.take_scroll_trace_frames();

    for _ in 0..BURST_EVENTS {
        key.press(app);
    }

    let scroll_before_render = app.app.transcript_win().scroll_top();
    assert_eq!(
        scroll_before_render, before_scroll,
        "{label} mutated Window::scroll_top before projection"
    );

    app.render_silent();
    let frames = app.app.transcript.take_scroll_trace_frames();
    assert_local_scroll_frames_are_exact_and_fast(&frames);
    assert_user_delta_targets_exact_rows(&frames);
    assert_user_delta_inputs_do_not_pre_scroll(&frames);
    let max_delta = key
        .max_rows_per_event(viewport_rows)
        .saturating_mul(BURST_EVENTS as RowIndex)
        .saturating_add(RowIndex::from(viewport_rows).saturating_mul(2));
    assert_burst_projection_delta_bounded(label, &frames, before_scroll, max_delta);

    let after_scroll = app.app.transcript_win().scroll_top();
    if key.moves_down() {
        assert!(
            after_scroll >= before_scroll,
            "{label} moved upward after a downward burst: before_scroll={before_scroll}, after_scroll={after_scroll}, frames={frames:?}"
        );
    } else {
        assert!(
            after_scroll <= before_scroll,
            "{label} moved downward after an upward burst: before_scroll={before_scroll}, after_scroll={after_scroll}, frames={frames:?}"
        );
    }
}

fn prepare_transcript_burst_app(app: &mut TestApp) {
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
}

fn jump_transcript_top(app: &mut TestApp) {
    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        0,
        "gg should reach the top before a key-repeat burst"
    );
}

fn jump_transcript_bottom(app: &mut TestApp) {
    app.type_char('G');
    app.render_silent();
    assert!(
        app.app.transcript_win().scroll_top() > 0,
        "G should reach a non-top viewport before an upward key-repeat burst"
    );
}

fn jump_transcript_middle(app: &mut TestApp) {
    let descriptor = app
        .app
        .transcript
        .descriptor_total_count()
        .expect("sparse descriptor count")
        / 2;
    assert!(
        app.app
            .reveal_transcript_descriptor_block(descriptor, 1, true),
        "middle descriptor reveal failed for descriptor {descriptor}"
    );
    app.render_silent();
    let scroll = app.app.transcript_win().scroll_top();
    assert!(
        scroll > 0,
        "middle reveal should leave the top: scroll={scroll}"
    );
}

fn assert_user_delta_descriptor_coverage_moves_contiguously(frames: &[TranscriptScrollTraceFrame]) {
    let mut previous: Option<TranscriptDescriptorTraceRange> = None;
    for frame in frames {
        let TranscriptScrollIntent::UserDelta { .. } = frame.scroll_intent else {
            continue;
        };
        let Some(current) = frame.active_descriptor_range_after else {
            continue;
        };
        if let Some(previous) = previous {
            assert!(
                current.start <= previous.end && previous.start <= current.end,
                "local user delta jumped to disjoint descriptor coverage: previous={previous:?}, current={current:?}, frame={frame:?}"
            );
        }
        previous = Some(current);
    }
}

fn assert_preserve_frames_keep_semantic_anchor(frames: &[TranscriptScrollTraceFrame]) {
    let preserve_frames: Vec<_> = frames
        .iter()
        .filter(|frame| {
            matches!(
                frame.scroll_intent,
                TranscriptScrollIntent::PreserveViewport
                    | TranscriptScrollIntent::ResizeReflow { .. }
            )
        })
        .collect();
    assert!(
        !preserve_frames.is_empty(),
        "replay did not include preserve or resize frames"
    );
    let mut compared = 0;
    for frame in preserve_frames {
        let Some(TranscriptTraceAnchor::Content {
            descriptor_index: before_descriptor,
            block_id: before_block,
            ..
        }) = frame.viewport_anchor_before
        else {
            continue;
        };
        let Some(TranscriptTraceAnchor::Content {
            descriptor_index: after_descriptor,
            block_id: after_block,
            ..
        }) = frame.viewport_anchor_after
        else {
            panic!("preserve frame lost its semantic viewport anchor: {frame:?}");
        };
        assert_eq!(
            (after_descriptor, after_block),
            (before_descriptor, before_block),
            "preserve/resize frame moved to different visible block identity: {frame:?}"
        );
        compared += 1;
        let ms = frame
            .render_or_projection_ms
            .expect("replay should enable projection timings");
        assert!(
            ms <= 250,
            "preserve/resize projection exceeded latency budget: {ms}ms in {frame:?}"
        );
    }
    assert!(
        compared > 0,
        "replay preserve/resize frames did not expose comparable semantic anchors"
    );
}

#[test]
fn transcript_scroll_replay_covers_velocity_latency_and_sparse_scenarios() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(320, 78, 18);
    let replay = TranscriptScrollReplay::new(vec![
        TranscriptReplayStep::WheelUp { ticks: 8 },
        TranscriptReplayStep::CoalescedWheel { rows: -9 },
        TranscriptReplayStep::WheelDown { ticks: 4 },
        TranscriptReplayStep::DragAutoscrollTop { ticks: 3 },
        TranscriptReplayStep::WheelUpUntilDescriptorRangeChanges { max_ticks: 80 },
        TranscriptReplayStep::Resize {
            width: 70,
            height: 20,
        },
        TranscriptReplayStep::StreamingAppend,
        TranscriptReplayStep::DragAutoscrollBottom { ticks: 3 },
        TranscriptReplayStep::ScrollbarFarSeek { rel_row: 3 },
    ]);

    let report = replay.run(&mut app);
    assert!(!report.frames.is_empty(), "replay produced no trace frames");
    assert!(
        report.descriptor_range_changed,
        "replay did not cover descriptor-window replacement: {:?}",
        report.frames
    );
    assert!(
        report.frames.iter().any(|frame| matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ResizeReflow { .. }
        )),
        "replay did not cover resize/reflow"
    );
    assert!(
        report.frames.iter().any(|frame| matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ScrollbarFraction { .. }
        )),
        "replay did not cover scrollbar far seek"
    );
    for frame in report
        .frames
        .iter()
        .filter(|frame| frame.placeholder_rows_visible)
    {
        assert!(
            matches!(
                frame.scroll_intent,
                TranscriptScrollIntent::ScrollbarFraction { .. }
            ),
            "only scrollbar far seek should be allowed to expose sparse placeholders: {frame:?}"
        );
    }

    let wheel_up_frames: Vec<_> = report
        .frames
        .iter()
        .filter(|frame| {
            frame.input_event_or_tick.starts_with("wheel_up:")
                || frame.input_event_or_tick.starts_with("coalesced_wheel")
        })
        .cloned()
        .collect();
    let wheel_probe_frames: Vec<_> = report
        .frames
        .iter()
        .filter(|frame| {
            frame
                .input_event_or_tick
                .starts_with("wheel_up_window_probe")
        })
        .cloned()
        .collect();
    let drag_top_frames: Vec<_> = report
        .frames
        .iter()
        .filter(|frame| frame.input_event_or_tick.starts_with("drag_autoscroll_top"))
        .cloned()
        .collect();
    let wheel_down_frames: Vec<_> = report
        .frames
        .iter()
        .filter(|frame| frame.input_event_or_tick.starts_with("wheel_down"))
        .cloned()
        .collect();
    let drag_bottom_frames: Vec<_> = report
        .frames
        .iter()
        .filter(|frame| {
            frame
                .input_event_or_tick
                .starts_with("drag_autoscroll_bottom")
        })
        .cloned()
        .collect();
    assert!(
        !wheel_up_frames.is_empty()
            && !drag_top_frames.is_empty()
            && !wheel_down_frames.is_empty()
            && !drag_bottom_frames.is_empty(),
        "replay missing local scroll frame groups: wheel_up={}, probe={}, drag_top={}, wheel_down={}, drag_bottom={}",
        wheel_up_frames.len(),
        wheel_probe_frames.len(),
        drag_top_frames.len(),
        wheel_down_frames.len(),
        drag_bottom_frames.len()
    );
    assert_local_scroll_frames_are_exact_and_fast(&wheel_up_frames);
    if !wheel_probe_frames.is_empty() {
        assert_local_scroll_frames_are_exact_and_fast(&wheel_probe_frames);
    }
    assert_local_scroll_frames_are_exact_and_fast(&drag_top_frames);
    assert_user_delta_descriptor_coverage_moves_contiguously(&drag_top_frames);
    assert_local_scroll_frames_are_exact_and_fast(&wheel_down_frames);
    assert_local_scroll_frames_are_exact_and_fast(&drag_bottom_frames);
    assert_user_delta_descriptor_coverage_moves_contiguously(&drag_bottom_frames);
    assert_user_delta_targets_exact_rows(&wheel_up_frames);
    assert_user_delta_descriptor_coverage_moves_contiguously(&wheel_up_frames);
    if !wheel_probe_frames.is_empty() {
        assert_user_delta_targets_exact_rows(&wheel_probe_frames);
        assert_user_delta_descriptor_coverage_moves_contiguously(&wheel_probe_frames);
    }
    assert_user_delta_targets_exact_rows(&wheel_down_frames);
    assert_user_delta_descriptor_coverage_moves_contiguously(&wheel_down_frames);
    assert_preserve_frames_keep_semantic_anchor(&report.frames);
    assert_monotonic_visible_anchors(&wheel_up_frames, true);
    if !wheel_probe_frames.is_empty() {
        assert_monotonic_visible_anchors(&wheel_probe_frames, true);
    }
    assert_monotonic_visible_anchors(&drag_top_frames, true);
    assert_monotonic_visible_anchors(&wheel_down_frames, false);
    assert_monotonic_visible_anchors(&drag_bottom_frames, false);
}

#[test]
fn transcript_key_repeat_bursts_do_not_overaccumulate_sparse_local_motion() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    prepare_transcript_burst_app(&mut app);

    jump_transcript_top(&mut app);
    run_transcript_key_burst(&mut app, "top Ctrl-D burst", TranscriptBurstKey::CtrlD);
    jump_transcript_top(&mut app);
    run_transcript_key_burst(&mut app, "top Down-arrow burst", TranscriptBurstKey::Down);

    jump_transcript_bottom(&mut app);
    run_transcript_key_burst(&mut app, "bottom Ctrl-U burst", TranscriptBurstKey::CtrlU);
    jump_transcript_bottom(&mut app);
    run_transcript_key_burst(&mut app, "bottom Up-arrow burst", TranscriptBurstKey::Up);

    jump_transcript_middle(&mut app);
    run_transcript_key_burst(&mut app, "middle Ctrl-D burst", TranscriptBurstKey::CtrlD);
    jump_transcript_middle(&mut app);
    run_transcript_key_burst(
        &mut app,
        "middle Down-arrow burst",
        TranscriptBurstKey::Down,
    );
    jump_transcript_middle(&mut app);
    run_transcript_key_burst(&mut app, "middle Ctrl-U burst", TranscriptBurstKey::CtrlU);
    jump_transcript_middle(&mut app);
    run_transcript_key_burst(&mut app, "middle Up-arrow burst", TranscriptBurstKey::Up);
}

#[test]
fn transcript_gg_then_key_repeat_burst_without_intermediate_render_uses_top_base() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    prepare_transcript_burst_app(&mut app);
    jump_transcript_bottom(&mut app);

    app.app.transcript.set_scroll_trace_timings_enabled(true);
    app.app.transcript.take_scroll_trace_frames();
    app.type_char('g');
    app.type_char('g');
    for _ in 0..80 {
        TranscriptBurstKey::CtrlD.press(&mut app);
    }
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        0,
        "gg should update the document command base before the held Ctrl-D burst renders"
    );

    app.render_silent();
    let frames = app.app.transcript.take_scroll_trace_frames();
    assert_local_scroll_frames_are_exact_and_fast(&frames);
    assert_user_delta_targets_exact_rows(&frames);
    assert_user_delta_inputs_do_not_pre_scroll(&frames);
    let viewport_rows = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport")
        .rect
        .height
        .max(1);
    let max_delta = RowIndex::from(viewport_rows)
        .saturating_mul(80)
        .saturating_add(RowIndex::from(viewport_rows).saturating_mul(2));
    assert_burst_projection_delta_bounded("gg then Ctrl-D burst", &frames, 0, max_delta);
}

#[test]
fn transcript_drag_autoscroll_top_crosses_sparse_windows_without_teleport() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    app.app.transcript.set_scroll_trace_timings_enabled(true);
    app.app.transcript.take_scroll_trace_frames();

    let initial_record = first_visible_record_index(&app).expect("initial visible record");
    start_transcript_edge_drag(&mut app, TranscriptDragEdge::Top);
    let mut frames = Vec::new();
    for _ in 0..900 {
        if app.app.tick_drag_autoscroll_with_transcript_intent() {
            app.render_silent();
            frames.extend(app.app.transcript.take_scroll_trace_frames());
        }
    }
    finish_transcript_drag(&mut app);

    let final_record = first_visible_record_index(&app).expect("final visible record");
    assert!(
        final_record.saturating_add(100) < initial_record,
        "drag autoscroll did not move through older sparse content: initial_record={initial_record}, final_record={final_record}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    assert_local_scroll_frames_are_exact_and_fast(&frames);
    assert_monotonic_visible_anchors(&frames, true);
}

#[test]
fn transcript_drag_autoscroll_bottom_crosses_sparse_windows_without_locking() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    assert!(app.run_lua(
        r#"assert(smelt.transcript.reveal_block(120, { top_padding = 1, cursor = true }))"#
    ));
    app.render_silent();
    app.app.transcript.set_scroll_trace_timings_enabled(true);
    app.app.transcript.take_scroll_trace_frames();

    let initial_record = first_visible_record_index(&app).expect("initial visible record");
    assert!(
        initial_record < 200,
        "test must start high in sparse content, got record {initial_record}: {:?}",
        transcript_viewport_lines(&app)
    );
    start_transcript_edge_drag(&mut app, TranscriptDragEdge::Bottom);
    let mut frames = Vec::new();
    let mut latest = initial_record;
    for tick in 0..320 {
        assert!(
            app.app.tick_drag_autoscroll_with_transcript_intent(),
            "bottom-edge drag tick {tick} stopped before reaching newer content: first={:?}, last={:?}, scroll_top={}, lines={:?}",
            first_visible_record_index(&app),
            last_visible_record_index(&app),
            app.app.transcript_win().scroll_top(),
            transcript_viewport_lines(&app)
        );
        app.render_silent();
        frames.extend(app.app.transcript.take_scroll_trace_frames());
        let current = first_visible_record_index(&app).unwrap_or_else(|| {
            panic!(
                "bottom-edge drag tick {tick} rendered no visible record marker: lines={:?}",
                transcript_viewport_lines(&app)
            )
        });
        assert!(
            current >= latest,
            "bottom-edge drag moved backward: latest={latest}, current={current}, lines={:?}",
            transcript_viewport_lines(&app)
        );
        latest = current;
    }
    finish_transcript_drag(&mut app);

    let final_record = first_visible_record_index(&app).expect("final visible record");
    assert!(
        final_record > initial_record.saturating_add(40),
        "bottom-edge drag autoscroll did not advance through newer sparse content: initial_record={initial_record}, final_record={final_record}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    assert!(
        frames.len() >= 300,
        "bottom-edge drag produced too few frames before locking: {} frames",
        frames.len()
    );
    assert_local_scroll_frames_are_exact_and_fast(&frames);
    assert_monotonic_visible_anchors(&frames, false);
    assert_user_delta_targets_exact_rows(&frames);
    assert_user_delta_inputs_do_not_pre_scroll(&frames);
    assert_user_delta_descriptor_coverage_moves_contiguously(&frames);
}

#[test]
fn transcript_drag_autoscroll_bottom_stops_at_real_bottom() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    prepare_transcript_burst_app(&mut app);
    jump_transcript_bottom(&mut app);
    let viewport_rows = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport")
        .rect
        .height
        .max(1);
    let state = app.app.transcript_win().document_view_state();
    let max_scroll = state
        .materialized
        .total_rows
        .saturating_sub(RowIndex::from(viewport_rows));
    assert!(
        app.app.transcript_win().scroll_top() >= max_scroll,
        "G should reach the real transcript bottom before testing drag boundary"
    );

    start_transcript_edge_drag(&mut app, TranscriptDragEdge::Bottom);
    assert!(
        !app.app.tick_drag_autoscroll_with_transcript_intent(),
        "bottom-edge drag autoscroll should stop when the resolved viewport is already at the real bottom"
    );
    finish_transcript_drag(&mut app);
}

#[test]
fn transcript_drag_autoscroll_bottom_no_input_renders_do_not_undo_ticks() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    assert!(app.run_lua(
        r#"assert(smelt.transcript.reveal_block(120, { top_padding = 1, cursor = true }))"#
    ));
    app.render_silent();
    app.app.transcript.set_scroll_trace_timings_enabled(true);
    app.app.transcript.take_scroll_trace_frames();

    start_transcript_edge_drag(&mut app, TranscriptDragEdge::Bottom);
    let mut frames = Vec::new();
    for tick in 0..80 {
        let before_scroll = app.app.transcript_win().scroll_top();
        assert!(
            app.app.tick_drag_autoscroll_with_transcript_intent(),
            "bottom-edge drag tick {tick} stopped before a real boundary"
        );
        app.render_silent();
        let after_tick_scroll = app.app.transcript_win().scroll_top();
        let after_tick_lines = transcript_viewport_lines(&app);
        frames.extend(app.app.transcript.take_scroll_trace_frames());

        app.render_silent();
        let after_idle_scroll = app.app.transcript_win().scroll_top();
        let after_idle_lines = transcript_viewport_lines(&app);
        frames.extend(app.app.transcript.take_scroll_trace_frames());
        assert_eq!(
            after_idle_scroll, after_tick_scroll,
            "no-input render after bottom-edge drag tick {tick} changed the resolved scroll row: before_scroll={before_scroll}, after_tick_scroll={after_tick_scroll}, after_idle_scroll={after_idle_scroll}, frames={frames:?}"
        );
        assert_eq!(
            after_idle_lines, after_tick_lines,
            "no-input render after bottom-edge drag tick {tick} changed visible content: before_scroll={before_scroll}, after_tick_scroll={after_tick_scroll}, after_idle_scroll={after_idle_scroll}, frames={frames:?}"
        );
    }
    finish_transcript_drag(&mut app);

    assert_local_scroll_frames_are_exact_and_fast(&frames);
    assert_user_delta_inputs_do_not_pre_scroll(&frames);
}

#[test]
fn transcript_cursor_down_inside_viewport_moves_one_row_without_scrolling() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    assert!(app.run_lua(
        r#"assert(smelt.transcript.reveal_block(120, { top_padding = 1, cursor = true }))"#
    ));
    app.render_silent();
    app.app.transcript_win_mut().set_vim_enabled(true);
    app.app
        .transcript_win_mut()
        .set_vim_mode(crate::smelt_edit::VimMode::Normal);

    let viewport_rows = app
        .app
        .transcript_win()
        .viewport
        .map(|viewport| viewport.rect.height)
        .expect("transcript viewport");
    let (click_row, click_col) = transcript_content_point(&app, 2);
    let mouse = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        row: click_row,
        column: click_col,
        modifiers: KeyModifiers::empty(),
    };
    let pos = app
        .app
        .document_view_position_at_mouse_for_win(crate::app::TRANSCRIPT_WIN, mouse)
        .expect("transcript cursor position");
    let mut state = app.app.transcript_win().document_view_state();
    state.cursor = pos;
    state.drag_endpoint = None;
    state.selection_anchor = None;
    app.app.transcript_win_mut().set_document_view_state(state);
    app.render_silent();

    for step in 0..4 {
        let before_lines = transcript_viewport_lines(&app);
        let before = app.app.transcript_win().document_view_state();
        let before_scroll = app.app.transcript_win().scroll_top();
        let now = app.app.core.clock.instant_now();
        app.app.execute_document_view_command_for_win(
            crate::app::TRANSCRIPT_WIN,
            crate::smelt_edit::DocumentCommand::MoveRows(1),
            viewport_rows,
            now,
        );
        app.render_silent();
        let after_lines = transcript_viewport_lines(&app);
        let after = app.app.transcript_win().document_view_state();
        let after_scroll = app.app.transcript_win().scroll_top();
        assert_eq!(
            after_scroll, before_scroll,
            "cursor-down step {step} scrolled before the cursor reached the edge: before_lines={before_lines:?}, after_lines={after_lines:?}"
        );
        assert_eq!(
            after.cursor.row,
            before.cursor.row.saturating_add(1),
            "cursor-down step {step} did not move exactly one row inside the viewport: before={:?}, after={:?}, lines={after_lines:?}",
            before.cursor,
            after.cursor
        );
        assert_eq!(
            after_lines, before_lines,
            "cursor-down step {step} changed visible content before the cursor reached the edge"
        );
    }
}

#[test]
fn transcript_cursor_down_at_lower_edge_moves_one_visible_row_per_step() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    assert!(app.run_lua(
        r#"assert(smelt.transcript.reveal_block(120, { top_padding = 1, cursor = true }))"#
    ));
    app.render_silent();
    app.app.transcript_win_mut().set_vim_enabled(true);
    app.app
        .transcript_win_mut()
        .set_vim_mode(crate::smelt_edit::VimMode::Normal);

    let viewport_rows = app
        .app
        .transcript_win()
        .viewport
        .map(|viewport| viewport.rect.height)
        .expect("transcript viewport");
    let lower_edge = viewport_rows.saturating_sub(1);
    let (click_row, click_col) = transcript_content_point(&app, lower_edge);
    let mouse = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        row: click_row,
        column: click_col,
        modifiers: KeyModifiers::empty(),
    };
    let pos = app
        .app
        .document_view_position_at_mouse_for_win(crate::app::TRANSCRIPT_WIN, mouse)
        .expect("transcript cursor position");
    let mut state = app.app.transcript_win().document_view_state();
    state.cursor = pos;
    state.drag_endpoint = None;
    state.selection_anchor = None;
    app.app.transcript_win_mut().set_document_view_state(state);
    app.render_silent();
    app.app.transcript.set_scroll_trace_timings_enabled(true);
    app.app.transcript.take_scroll_trace_frames();

    let mut frames = Vec::new();
    for step in 0..160 {
        let before_lines = transcript_viewport_lines(&app);
        let before = app.app.transcript_win().document_view_state();
        let before_scroll = app.app.transcript_win().scroll_top();
        let before_screen_row = before.cursor.row.saturating_sub(before_scroll);
        assert_eq!(
            before_screen_row, lower_edge as u64,
            "cursor must stay parked at the lower edge before step {step}: cursor={:?}, scroll_top={before_scroll}, lines={before_lines:?}",
            before.cursor
        );

        let now = app.app.core.clock.instant_now();
        app.app.execute_document_view_command_for_win(
            crate::app::TRANSCRIPT_WIN,
            crate::smelt_edit::DocumentCommand::MoveRows(1),
            viewport_rows,
            now,
        );
        app.render_silent();
        frames.extend(app.app.transcript.take_scroll_trace_frames());

        let after_lines = transcript_viewport_lines(&app);
        let after = app.app.transcript_win().document_view_state();
        let after_scroll = app.app.transcript_win().scroll_top();
        let after_screen_row = after.cursor.row.saturating_sub(after_scroll);
        assert_eq!(
            after_screen_row, before_screen_row,
            "cursor-down step {step} moved the cursor away from the lower edge: before_cursor={:?}, after_cursor={:?}, before_scroll={before_scroll}, after_scroll={after_scroll}, after_lines={after_lines:?}",
            before.cursor, after.cursor
        );
        assert!(
            after.cursor.row >= after_scroll
                && after.cursor.row < after_scroll.saturating_add(u64::from(viewport_rows)),
            "cursor-down step {step} left the cursor outside the resolved viewport: after_cursor={:?}, after_scroll={after_scroll}, after_lines={after_lines:?}",
            after.cursor
        );
        assert_viewport_shifted_down_one_row(step, &before_lines, &after_lines);
    }

    assert_local_scroll_frames_are_exact_and_fast(&frames);
    assert_user_delta_inputs_do_not_pre_scroll(&frames);
    assert!(
        frames.iter().all(|frame| !matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ExactContentAnchor(TranscriptTraceAnchor::EstimatedRow(_))
        )),
        "cursor-down lower-edge movement fell back to estimated exact-row anchors: {frames:?}"
    );
    assert_user_delta_descriptor_coverage_moves_contiguously(&frames);
}

#[test]
fn transcript_cursor_down_scroll_crosses_sparse_windows_without_locking() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    app.app.transcript_win_mut().set_vim_enabled(true);
    app.app
        .transcript_win_mut()
        .set_vim_mode(crate::smelt_edit::VimMode::Normal);
    app.app.transcript.set_scroll_trace_enabled(true);
    for _ in 0..180 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
        app.app.transcript.take_scroll_trace_frames();
    }
    let start = first_visible_record_index(&app).expect("initial visible record");
    let start_row = app.app.transcript_win().scroll_top();
    let (click_row, click_col) = transcript_content_point(&app, 1);
    let mouse = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        row: click_row,
        column: click_col,
        modifiers: KeyModifiers::empty(),
    };
    let pos = app
        .app
        .document_view_position_at_mouse_for_win(crate::app::TRANSCRIPT_WIN, mouse)
        .expect("transcript cursor position");
    let mut state = app.app.transcript_win().document_view_state();
    state.cursor = pos;
    state.drag_endpoint = None;
    state.selection_anchor = None;
    app.app.transcript_win_mut().set_document_view_state(state);
    app.render_silent();
    app.app.transcript.take_scroll_trace_frames();
    let mut latest = start;
    let mut frames = Vec::new();

    for step in 0..220 {
        let viewport_rows = app
            .app
            .transcript_win()
            .viewport
            .map(|viewport| viewport.rect.height)
            .expect("transcript viewport");
        let now = app.app.core.clock.instant_now();
        app.app.execute_document_view_command_for_win(
            crate::app::TRANSCRIPT_WIN,
            crate::smelt_edit::DocumentCommand::MoveRows(1),
            viewport_rows,
            now,
        );
        app.render_silent();
        frames.extend(app.app.transcript.take_scroll_trace_frames());
        let current = first_visible_record_index(&app).unwrap_or_else(|| {
            panic!(
                "cursor-down step {step} rendered no visible record marker: lines={:?}",
                transcript_viewport_lines(&app)
            )
        });
        assert!(
            current >= latest,
            "cursor-down scroll moved backward: latest={latest}, current={current}, lines={:?}",
            transcript_viewport_lines(&app)
        );
        latest = current;
    }

    let final_scroll = app.app.transcript_win().scroll_top();
    let final_cursor = app.app.transcript_win().document_view_state().cursor;
    assert!(
        final_scroll > start_row.saturating_add(40),
        "cursor-down scrolling locked before advancing the viewport: start_row={start_row}, final_scroll={final_scroll}, final_cursor={final_cursor:?}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    assert!(
        frames.iter().any(|frame| matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::UserDelta { rows } if rows > 0
        )),
        "cursor-down document scrolling did not produce semantic UserDelta frames: {frames:?}"
    );
    assert!(
        frames.iter().all(|frame| !matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ExactContentAnchor(TranscriptTraceAnchor::EstimatedRow(_))
        )),
        "cursor-down document scrolling fell back to estimated exact-row anchors: {frames:?}"
    );
    assert_monotonic_visible_anchors(&frames, false);
    assert_user_delta_descriptor_coverage_moves_contiguously(&frames);
}

#[test]
fn transcript_scrollbar_click_preserves_fraction_intent() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(160, 78, 18);
    app.app.transcript.set_scroll_trace_enabled(true);
    app.app.transcript.take_scroll_trace_frames();

    let (row, column, _, _) = transcript_scrollbar_point(&app, 3);
    app.feed_one(SourceEvent::Term(Event::Mouse(
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::empty(),
        },
    )));
    app.render_silent();
    let frames = app.app.transcript.take_scroll_trace_frames();
    let frame = frames.first().expect("scrollbar render frame");

    assert!(
        matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ScrollbarFraction { .. }
        ),
        "scrollbar clicks must preserve a fraction intent instead of collapsing to {:?}",
        frame.scroll_intent
    );
}

#[test]
fn transcript_scroll_search_jump_preserves_semantic_scroll_intent() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(160, 78, 18);
    app.app.transcript.set_scroll_trace_enabled(true);
    app.app.transcript.take_scroll_trace_frames();

    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        SearchDirection::Forward,
        "record-0150".to_string(),
    );
    app.render_silent();
    let frames = app.app.transcript.take_scroll_trace_frames();
    let frame = frames.first().expect("search jump render frame");

    assert!(
        matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::SearchJump {
                anchor: crate::app::transcript::TranscriptSearchAnchor::Content { .. },
                ..
            }
        ),
        "search jumps must preserve a semantic content anchor instead of collapsing to {:?}",
        frame.scroll_intent
    );
    assert!(
        !frame.placeholder_rows_visible,
        "search reveal must not expose sparse placeholders: {frame:?}"
    );
    assert!(
        frame.first_visible_content_anchor.is_some(),
        "search reveal must resolve exact visible content: {frame:?}"
    );
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("record-0150")),
        "search reveal did not place the matched record in the viewport: {:?}",
        transcript_viewport_lines(&app)
    );
}

#[test]
fn transcript_search_jump_keeps_cursor_on_heterogeneous_match_start_after_render() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(180, 78, 18);

    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        SearchDirection::Forward,
        "assistant paragraph".to_string(),
    );
    app.render_silent();

    let range = app
        .app
        .search
        .session
        .as_ref()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("heterogeneous search match");
    let cursor = app
        .app
        .transcript_win()
        .row_cursor()
        .expect("transcript cursor");
    let cursor_row = app
        .app
        .transcript_rows_and_breaks_range(cursor.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert_eq!(
        cursor,
        range.start,
        "cursor_row={cursor_row:?}, viewport={:?}",
        transcript_viewport_lines(&app)
    );
    assert_eq!(
        smelt_buffer::text::slice(&cursor_row, cursor.byte_col..range.end.byte_col),
        "assistant paragraph"
    );
}

#[test]
fn transcript_wheel_preserves_user_delta_intent() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(160, 78, 18);
    app.app.transcript.set_scroll_trace_enabled(true);
    app.app.transcript.take_scroll_trace_frames();

    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    let frames = app.app.transcript.take_scroll_trace_frames();
    let frame = frames.first().expect("wheel render frame");

    assert_eq!(
        frame.scroll_intent,
        TranscriptScrollIntent::UserDelta { rows: -3 },
        "wheel replay must preserve the semantic user delta instead of collapsing to {:?}",
        frame.scroll_intent
    );
    assert_eq!(
        frame.window_scroll_after_input, frame.window_scroll_before,
        "transcript wheel input must not mutate Window::scroll_top before projection: {frame:?}"
    );
}

#[test]
fn transcript_drag_autoscroll_preserves_user_delta_intent() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(160, 78, 18);
    app.app.transcript.set_scroll_trace_enabled(true);
    app.app.transcript.take_scroll_trace_frames();

    start_transcript_edge_drag(&mut app, TranscriptDragEdge::Top);
    assert!(
        app.app.tick_drag_autoscroll_with_transcript_intent(),
        "transcript edge drag should request a semantic scroll tick"
    );
    app.render_silent();
    let frames = app.app.transcript.take_scroll_trace_frames();
    let frame = frames.first().expect("drag autoscroll render frame");
    let state = app.app.transcript_win().document_view_state();

    assert_eq!(
        frame.scroll_intent,
        TranscriptScrollIntent::UserDelta { rows: -1 },
        "drag autoscroll must preserve the semantic user delta instead of collapsing to {:?}",
        frame.scroll_intent
    );
    assert_eq!(
        frame.window_scroll_after_input, frame.window_scroll_before,
        "transcript drag autoscroll must not mutate Window::scroll_top before projection: {frame:?}"
    );
    assert_eq!(
        state.drag_endpoint.map(|pos| pos.row),
        Some(frame.resolved_scroll_top),
        "top-edge drag endpoint should advance to the projected leading row"
    );
}

fn resumed_heterogeneous_transcript_app(
    count: usize,
    width: u16,
    height: u16,
) -> (TestApp, tempfile::TempDir) {
    resumed_transcript_app_from_records(heterogeneous_resume_records(count), width, height)
}

fn resumed_transcript_app_from_records(
    records: Vec<smelt_core::TranscriptBlockRecord>,
    width: u16,
    height: u16,
) -> (TestApp, tempfile::TempDir) {
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

fn tail_consecutive_user_records(count: usize) -> Vec<smelt_core::TranscriptBlockRecord> {
    use smelt_core::transcript_model::Block;

    let mut source = smelt_core::content::transcript::Transcript::new();
    for idx in 0..count {
        let marker = format!("record-{idx:04}");
        if idx % 10 == 0 || idx >= count.saturating_sub(2) {
            source.push(Block::User {
                text: format!("{marker} user prompt {}", "u ".repeat(8)),
                image_labels: Vec::new(),
            });
        } else {
            source.push(Block::Text {
                content: format!("{marker} assistant output\n{}", "tail context ".repeat(18)),
            });
        }
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

fn assert_viewport_shifted_down_one_row(step: usize, before: &[String], after: &[String]) {
    assert_eq!(
        before.len(),
        after.len(),
        "cursor-down step {step} changed viewport line count: before={before:?}, after={after:?}"
    );
    let overlap = before.len().saturating_sub(1);
    assert!(
        overlap > 0,
        "cursor-down step {step} needs a non-empty viewport: before={before:?}, after={after:?}"
    );
    assert_eq!(
        &after[..overlap],
        &before[1..],
        "cursor-down step {step} did not shift the viewport by exactly one visible row"
    );
}

fn first_visible_record_index(app: &TestApp) -> Option<usize> {
    transcript_viewport_lines(app)
        .into_iter()
        .find_map(|line| parse_record_index(&line))
}

fn last_visible_record_index(app: &TestApp) -> Option<usize> {
    transcript_viewport_lines(app)
        .into_iter()
        .filter_map(|line| parse_record_index(&line))
        .next_back()
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
