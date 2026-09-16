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

#[test]
fn spawn_agent_hides_success_output_but_preserves_errors() {
    let mut app = TestApp::builder().build();
    app.run_lua_result("require('smelt.plugins.subagents')")
        .unwrap();
    app.start_turn(1);
    let invocation_id = app.tool_started(
        "spawn",
        "spawn_agent",
        std::collections::HashMap::from([("prompt".into(), serde_json::json!("Review parser"))]),
    );
    app.tool_finished(
        invocation_id,
        "spawn",
        protocol::ToolOutcome::new(
            r#"[{"id":1,"status":"running","session_id":"child-session"}]"#.into(),
            false,
            None,
        ),
        Some(1),
    );
    let invocation_id = app.tool_started("failed-spawn", "spawn_agent", Default::default());
    app.tool_finished(
        invocation_id,
        "failed-spawn",
        protocol::ToolOutcome::new("subagent queue is full".into(), true, None),
        Some(1),
    );
    for width in [120, 45] {
        app.set_terminal_size(width, 24);
        for state in ["open", "close"] {
            app.run_lua_result(&format!("smelt.transcript.fold_all('{state}')"))
                .unwrap();
            app.follow_transcript_tail();
            let text = app.render_to_frame().text();
            assert!(text.contains("spawn_agent Review parser"), "{text}");
            assert!(!text.contains("session_id"), "{text}");
            assert!(!text.contains("child-session"), "{text}");
            assert!(text.contains("subagent queue is full"), "{text}");
            assert!(!text.contains("spawn_agent spawn_agent"), "{text}");
            let timestamps: Vec<_> = text
                .lines()
                .filter(|line| line.starts_with("* spawn_agent"))
                .map(|line| line.rfind(' ').unwrap())
                .collect();
            assert_eq!(timestamps.len(), 2, "{text}");
            assert_eq!(timestamps[0], timestamps[1], "{text}");
        }
    }
}

#[test]
fn peek_agent_output_is_capped_readable_and_copyable() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.run_lua_result("require('smelt.plugins.subagents')")
        .unwrap();
    app.start_turn(1);
    let invocation_id = app.tool_started(
        "peek",
        "peek_agent",
        std::collections::HashMap::from([("id".into(), serde_json::json!(7))]),
    );
    let output = format!(
        "agent #7 - running\n\n{}",
        (1..=30)
            .map(|line| format!("report {line:02}: vérified"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    app.tool_finished(
        invocation_id,
        "peek",
        protocol::ToolOutcome::new(output, false, None),
        Some(1),
    );
    for width in [120, 45] {
        app.set_terminal_size(width, 32);
        app.follow_transcript_tail();
        let text = app.render_to_frame().text();
        assert!(text.contains("peek_agent #7"), "{text}");
        assert!(text.contains("report 30: vérified"), "{text}");
        assert!(!text.contains("report 01:"), "{text}");
    }
    focus_transcript_in_normal_mode(&mut app);
    app.type_text("ggVGy");
    assert!(app
        .core_probe()
        .clipboard
        .kill_ring
        .current()
        .contains("report 30: vérified"));
}

#[test]
fn wait_agents_header_does_not_repeat_fallback_tool_name() {
    let mut app = TestApp::builder().build();
    app.run_lua_result("require('smelt.plugins.subagents')")
        .unwrap();
    app.start_turn(1);
    app.tool_started("empty-wait", "wait_agents", Default::default());
    app.tool_rejected(
        "unknown-wait",
        "wait_agents",
        std::collections::HashMap::from([("ids".into(), serde_json::json!([99]))]),
        protocol::StyledLines::from_plain("wait_agents"),
        protocol::ToolOutcome::new("unknown subagent: 99".into(), true, None),
        Some(1),
    );
    for width in [120, 45] {
        app.set_terminal_size(width, 24);
        app.follow_transcript_tail();
        let text = app.render_to_frame().text();
        assert!(!text.contains("wait_agents wait_agents"), "{text}");
        assert!(text.contains("wait_agents #99"), "{text}");
        assert!(text.contains("unknown subagent: 99"), "{text}");
    }
}

#[test]
fn wait_agents_preview_shows_readable_final_reports_and_copies_them() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.run_lua_result("require('smelt.plugins.subagents')")
        .unwrap();
    app.start_turn(1);
    let invocation_id = app.tool_started(
        "wait",
        "wait_agents",
        std::collections::HashMap::from([("ids".into(), serde_json::json!([1, 2]))]),
    );
    let reports = "agent #1 - completed\nParser review complete.\nAll tests passed.\n\nagent #2 - failed\nProvider unavailable.";
    app.tool_finished(
        invocation_id, "wait",
        protocol::ToolOutcome::new(serde_json::json!([
            { "id":1, "status":"completed", "result":"Parser review complete.\nAll tests passed." },
            { "id":2, "status":"failed", "error":"Provider unavailable." },
        ]).to_string(), false, None).with_display_content(vec![
            protocol::ToolDisplayContent::new("results", reports.into()),
        ]),
        Some(1250),
    );
    for width in [120, 45] {
        app.set_terminal_size(width, 24);
        app.follow_transcript_tail();
        let text = app.render_to_frame().text();
        for line in reports.lines().filter(|line| !line.is_empty()) {
            assert!(text.contains(line), "{text}");
        }
        assert!(text.contains("wait_agents #1, #2"), "{text}");
        assert!(!text.contains("\"result\""), "{text}");
    }
    focus_transcript_in_normal_mode(&mut app);
    app.type_text("ggVGy");
    let copied = app.core_probe().clipboard.kill_ring.current();
    for line in reports.lines().filter(|line| !line.is_empty()) {
        assert!(copied.contains(line), "{copied}");
    }
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
fn collapsing_bottom_pinned_block_hides_jump_to_bottom_pill() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder()
        .with_ephemeral(true)
        .with_vim(true)
        .build();
    app.set_terminal_size(80, 12);
    for i in 0..20 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("history {i}").into(),
        });
    }
    let output = (0..80)
        .map(|line| format!("streamed output line {line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    finish_tool(&mut app, "bottom-collapse", "bash", &output);
    app.render_silent();
    app.focus_transcript();
    assert!(app.run_lua("smelt.transcript.fold_all('open')"));
    app.type_char('G');
    app.render_silent();

    let before = app.transcript_window();
    let viewport = before.viewport.expect("expanded transcript viewport");
    assert!(before.following_tail);
    assert!(app
        .ui_probe()
        .named_win("smelt.scroll_pills.bottom.win")
        .is_none());

    let row = viewport.rect.bottom().saturating_sub(1);
    let column = viewport
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
            row,
            column,
            modifiers: KeyModifiers::empty(),
        })));
    }
    assert!(
        !app.transcript_window().following_tail,
        "clicking the bottom row should exercise the pinned-at-bottom state"
    );

    app.press(KeyCode::Enter);
    app.render_silent();
    let collapsed = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("collapsed bottom tool at cursor");
    assert_eq!(collapsed.view_state, ViewState::Collapsed);
    app.focus_prompt();
    app.render_silent();

    let after = app.transcript_window();
    let viewport = after.viewport.expect("collapsed transcript viewport");
    let max_scroll = viewport
        .total_rows
        .saturating_sub(crate::smelt_edit::RowIndex::from(viewport.rect.height));
    assert_eq!(
        after.scroll_top, max_scroll,
        "collapsed viewport: {after:#?}"
    );
    assert!(
        app.ui_probe()
            .named_win("smelt.scroll_pills.bottom.win")
            .is_none(),
        "jump-to-bottom pill remained after collapsing the block at the transcript bottom: {after:#?}"
    );
}

#[test]
fn streaming_thinking_honors_peek_and_enter_toggles() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 24);
    app.start_turn(1);
    app.dispatch_engine_event(protocol::EngineEvent::ReasoningPartStarted {
        id: "reasoning-1".into(),
        kind: protocol::ReasoningKind::Raw,
    });
    app.dispatch_engine_event(protocol::EngineEvent::ReasoningPartDelta {
        id: "reasoning-1".into(),
        kind: protocol::ReasoningKind::Raw,
        title: None,
        delta: concat!(
            "first line\nsecond line\nthird line\nfourth line\nfifth line\n",
            "sixth line\nseventh line\neighth line"
        )
        .into(),
    });
    app.render_silent();

    focus_transcript_in_normal_mode(&mut app);
    let initial_peek = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("streaming thinking block at cursor");
    assert_eq!(initial_peek.view_state, ViewState::Peek);
    assert!(
        initial_peek.rows < 8,
        "streaming thinking should render its compact default before user interaction"
    );

    app.press(KeyCode::Enter);
    app.render_silent();
    let expanded = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("expanded streaming thinking block at cursor");
    assert_eq!(expanded.id, initial_peek.id);
    assert_eq!(expanded.view_state, ViewState::Expanded);
    assert!(expanded.rows > initial_peek.rows);

    app.dispatch_engine_event(protocol::EngineEvent::ReasoningPartDelta {
        id: "reasoning-1".into(),
        kind: protocol::ReasoningKind::Raw,
        title: None,
        delta: "\nninth line".into(),
    });
    app.render_silent();
    let updated_expanded = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("updated streaming thinking block at cursor");

    app.press(KeyCode::Enter);
    app.render_silent();
    let restored_peek = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("streaming thinking block restored to peek at cursor");
    assert_eq!(restored_peek.id, initial_peek.id);
    assert_eq!(restored_peek.view_state, ViewState::Peek);
    assert!(
        restored_peek.rows < updated_expanded.rows,
        "returning streaming thinking to peek should reduce its rendered rows: expanded={}, peek={}",
        updated_expanded.rows,
        restored_peek.rows
    );

    app.dispatch_engine_event(protocol::EngineEvent::ReasoningPartDelta {
        id: "reasoning-1".into(),
        kind: protocol::ReasoningKind::Raw,
        title: None,
        delta: "\ntenth line".into(),
    });
    app.render_silent();
    let updated_peek = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("updated streaming thinking block in peek state at cursor");
    assert_eq!(updated_peek.id, initial_peek.id);
    assert_eq!(updated_peek.view_state, ViewState::Peek);
    assert_eq!(updated_peek.rows, restored_peek.rows);

    app.press(KeyCode::Enter);
    app.render_silent();
    let reopened = app
        .app
        .transcript_node_at_row(transcript_row_cursor_row(&app))
        .expect("reopened streaming thinking block at cursor");
    assert_eq!(reopened.id, initial_peek.id);
    assert_eq!(reopened.view_state, ViewState::Expanded);
    assert!(reopened.rows > updated_peek.rows);
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
