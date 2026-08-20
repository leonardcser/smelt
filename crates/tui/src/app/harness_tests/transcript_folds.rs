use super::*;
use smelt_core::transcript_model::ViewState;

fn finish_tool(app: &mut TestApp, call_id: &str, name: &str, content: &str) {
    let invocation_id = app.tool_started(call_id, name, std::collections::HashMap::new());
    app.tool_finished(
        invocation_id,
        call_id,
        protocol::ToolOutcome::new(content.into(), false, None),
        Some(1),
    );
}

fn focus_transcript_in_normal_mode(app: &mut TestApp) {
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
    app.type_char('g');
    app.type_char('g');
    app.render_silent();
}

#[tokio::test(flavor = "current_thread")]
async fn collapsing_group_while_compacting_keeps_cursor_on_group() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 30);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.start_turn(1);
    finish_tool(&mut app, "read-1", "read_file", "first\nsecond\nthird");
    finish_tool(&mut app, "grep-1", "grep", "one.rs\ntwo.rs");
    assert!(app.finish_turn());
    app.set_context_token_baseline_for_harness(Some(500));
    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
    let ask_id = app
        .pending_ask_id()
        .expect("/compact registered ask callback");
    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "# Goal\nkeep the active task".into(),
    });
    app.render_silent();
    focus_transcript_in_normal_mode(&mut app);

    let collapsed = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("collapsed group at cursor");
    assert_eq!(collapsed.view_state, ViewState::Collapsed);

    app.press(KeyCode::Enter);
    app.render_silent();
    let expanded = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("expanded group at cursor");
    assert_eq!(expanded.id, collapsed.id);
    assert_eq!(expanded.view_state, ViewState::Expanded);
    assert!(expanded.rows > collapsed.rows);

    for _ in 1..expanded.rows {
        app.type_char('j');
    }
    app.render_silent();
    assert_eq!(
        transcript_row_cursor_row(&app),
        expanded.first_row + expanded.rows - 1
    );

    app.press(KeyCode::Enter);
    app.render_silent();

    let cursor_row = transcript_row_cursor_row(&app);
    let current = app
        .app
        .transcript_node_at_row(cursor_row)
        .expect("node at cursor after collapse");
    assert_eq!(current.id, collapsed.id);
    assert_eq!(current.view_state, ViewState::Collapsed);
    assert!(cursor_row >= current.first_row);
    assert!(cursor_row < current.first_row + current.rows);

    app.type_char('z');
    app.type_char('a');
    app.render_silent();
    let reopened = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("reopened group at cursor");
    assert_eq!(reopened.id, collapsed.id);
    assert_eq!(reopened.view_state, ViewState::Expanded);

    app.type_char('z');
    app.type_char('M');
    app.render_silent();
    let closed_all_row = transcript_row_cursor_row(&app);
    let closed_all_total_rows = transcript_total_rows(&app);
    let closed_all = app
        .app
        .transcript_node_at_row(closed_all_row)
        .unwrap_or_else(|| {
            panic!(
                "group at cursor after closing all folds: row {closed_all_row} of {closed_all_total_rows}"
            )
        });
    assert_eq!(closed_all.id, collapsed.id);
    assert_eq!(closed_all.view_state, ViewState::Collapsed);

    app.type_char('z');
    app.type_char('R');
    app.render_silent();
    let opened_all = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("group at cursor after opening all folds");
    assert_eq!(opened_all.id, collapsed.id);
    assert_eq!(opened_all.view_state, ViewState::Expanded);
}

#[test]
fn expanding_bottom_pinned_preview_while_streaming_restores_tail_follow() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 12);
    assert!(app.run_lua(
        r#"
        smelt.keymap.set("n", "<space>", function()
            local transcript = smelt.win.transcript()
            smelt.transcript.fold_at_row(transcript:cursor(), "toggle")
        end)
        "#
    ));
    for i in 0..20 {
        app.session_append_history(protocol::HistoryItem::user(protocol::Content::text(
            format!("history {i}"),
        )));
    }
    app.set_context_token_baseline_for_harness(Some(500));
    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
    let ask_id = app
        .pending_ask_id()
        .expect("/compact registered ask callback");
    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "# Goal\nkeep the active task\n\n# Progress\none\ntwo\nthree\nfour\nfive".into(),
    });
    app.follow_transcript_tail();
    app.render_silent();

    let total_before = transcript_total_rows(&app);
    let preview = app
        .app
        .transcript_node_at_row(total_before.saturating_sub(1))
        .expect("streaming compaction preview at transcript tail");
    assert_eq!(preview.view_state, ViewState::Peek);
    let window = app.transcript_window();
    assert!(window.following_tail);
    let viewport = window.viewport.expect("transcript viewport");
    let click_row = viewport.rect.top.saturating_add(
        preview
            .first_row
            .saturating_sub(window.scroll_top)
            .min(viewport.rect.height.saturating_sub(1).into()) as u16,
    );
    let click_col = viewport
        .rect
        .left
        .saturating_add(viewport.gutter_width)
        .saturating_add(2);
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: click_row,
            column: click_col,
            modifiers: KeyModifiers::empty(),
        })));
    }
    assert!(
        !app.transcript_window().following_tail,
        "click should pin the viewport before the fold"
    );

    app.press(KeyCode::Char(' '));
    app.render_silent();

    let expanded = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("expanded compaction preview at cursor");
    assert_eq!(expanded.id, preview.id);
    assert_eq!(expanded.view_state, ViewState::Expanded);
    assert!(app.transcript_window().following_tail);
    let total_after = transcript_total_rows(&app);
    let window = app.transcript_window();
    assert_eq!(
        window.scroll_top,
        total_after.saturating_sub(viewport.rect.height.into()),
        "expanded streaming preview should remain pinned to the transcript tail"
    );
}

#[test]
fn fold_keys_work_while_compaction_preview_is_streaming() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 24);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.set_context_token_baseline_for_harness(Some(500));
    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
    let ask_id = app
        .pending_ask_id()
        .expect("/compact registered ask callback");
    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "# Goal\nkeep the active task\n\n# Progress\none\ntwo\nthree\nfour\nfive".into(),
    });
    app.render_silent();

    focus_transcript_in_normal_mode(&mut app);
    app.type_char('G');
    app.render_silent();
    let preview = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("compaction preview at cursor");
    assert_eq!(preview.view_state, ViewState::Peek);

    app.press(KeyCode::Enter);
    app.render_silent();
    let expanded = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("expanded compaction preview at cursor");
    assert_eq!(expanded.id, preview.id);
    assert_eq!(expanded.view_state, ViewState::Expanded);

    app.type_char('z');
    app.type_char('c');
    app.render_silent();
    let collapsed = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("collapsed compaction preview at cursor");
    assert_eq!(collapsed.id, preview.id);
    assert_eq!(collapsed.view_state, ViewState::Collapsed);

    app.type_char('z');
    app.type_char('o');
    app.render_silent();
    let reopened = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("reopened compaction preview at cursor");
    assert_eq!(reopened.id, preview.id);
    assert_eq!(reopened.view_state, ViewState::Expanded);
}
