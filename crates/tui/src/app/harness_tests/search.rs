use super::*;

fn searchable_transcript_app() -> TestApp {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 16);
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
    app
}

fn sparse_display_only_search_app() -> TestApp {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 16);
    let session_id = app.session_snapshot().id.clone();
    let session_dir = app.core_probe().sessions.dir_for_id(&session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut session =
        smelt_core::session::Session::new(app.core_probe().env.pid(), app.core_probe().env.cwd());
    session.id = session_id.clone();
    session.history = (0..200)
        .map(|idx| protocol::HistoryItem::user(protocol::Content::text(format!("item {idx}"))))
        .collect();
    let commit = smelt_core::session::initial_store_commit_from_session(&session).unwrap();
    let mut db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
    let receipt = db.apply_session_commit(&commit).unwrap();
    let records = (0..200)
        .map(|idx| {
            let content = match idx {
                20 => "early needle".to_string(),
                199 => "tail needle".to_string(),
                _ => format!("block {idx}"),
            };
            test_block_record(idx, &content)
        })
        .collect::<Vec<_>>();
    db.apply_transcript_record_fixture(&records).unwrap();
    drop(db);

    let loaded = crate::app::history::load_transcript_tail_from_sqlite_dir(session_dir, 80, 16)
        .expect("display-only transcript tail");
    session.history.clear();
    app.load_store_backed_session(
        crate::app::session_document::StoreBackedSessionDocument::new(
            session,
            loaded,
            crate::app::history::live_session_for_test(session_id, 200, None),
            receipt.current,
        ),
    );
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
    app.render_silent();
    app
}

fn test_block_record(block_idx: u64, content: &str) -> smelt_store::StoredTranscriptBlock {
    smelt_store::StoredTranscriptBlock {
        block_idx,
        history_idx: None,
        kind: "text".to_string(),
        tool_call_id: None,
        tool_name: None,
        content_hash: "0".to_string(),
        estimated_text_bytes: content.len() as u64,
        preview_text: content.to_string(),
        indexed_text: content.to_string(),
        block_json: serde_json::to_string(&smelt_core::Block::Text {
            content: content.to_string(),
        })
        .unwrap(),
        origin_json: Some(
            serde_json::to_string(&smelt_core::BlockOrigin::History(block_idx as usize)).unwrap(),
        ),
        tool_state_json: None,
    }
}

#[test]
fn transcript_search_opens_status_input_and_repeats_matches() {
    let mut app = row_document_transcript_app(20, true);
    app.type_char('g');
    app.type_char('g');

    app.type_char('/');
    assert!(app.state().cmdline_open);
    app.type_text("alpha");
    app.press(KeyCode::Enter);
    app.render_silent();

    let first_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert_eq!(transcript_row_cursor_row(&app), first_match.start.row);

    app.type_char('n');
    app.render_silent();
    let next_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert_eq!(transcript_row_cursor_row(&app), next_match.start.row);
    assert!(next_match.start.row > first_match.start.row);
    app.type_char('N');
    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), first_match.start.row);
}

#[test]
fn transcript_search_repeat_reaches_unloaded_sparse_matches() {
    let mut app = sparse_display_only_search_app();
    app.type_char('G');

    app.type_char('/');
    app.type_text("needle");
    app.press(KeyCode::Enter);
    app.render_silent();

    let tail_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    let tail_row = app
        .transcript_rows_and_breaks_range(tail_match.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        tail_row.contains("tail needle"),
        "matched row: {tail_row:?}"
    );

    app.type_char('n');
    app.render_silent();
    let early_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert!(early_match.start.row < tail_match.start.row);
    assert_eq!(transcript_row_cursor_row(&app), early_match.start.row);
    let early_row = app
        .transcript_rows_and_breaks_range(early_match.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        early_row.contains("early needle"),
        "matched row after repeat: {early_row:?}"
    );
}

#[test]
fn transcript_search_reverse_repeat_reaches_unloaded_sparse_matches() {
    let mut app = sparse_display_only_search_app();
    app.type_char('G');

    app.type_char('/');
    app.type_text("needle");
    app.press(KeyCode::Enter);
    app.render_silent();
    let tail_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();

    app.type_char('N');
    app.render_silent();
    let early_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert!(early_match.start.row < tail_match.start.row);
    assert_eq!(transcript_row_cursor_row(&app), early_match.start.row);
    let early_row = app
        .transcript_rows_and_breaks_range(early_match.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        early_row.contains("early needle"),
        "matched row after reverse repeat: {early_row:?}"
    );
}

#[test]
fn transcript_search_reverse_repeat_returns_to_cached_sparse_match() {
    let mut app = sparse_display_only_search_app();
    app.type_char('g');
    app.type_char('g');

    app.type_char('/');
    app.type_text("needle");
    app.press(KeyCode::Enter);
    app.render_silent();
    let early_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    let early_row = app
        .transcript_rows_and_breaks_range(early_match.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        early_row.contains("early needle"),
        "matched row: {early_row:?}"
    );

    app.type_char('n');
    app.render_silent();
    let tail_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert!(tail_match.start.row > early_match.start.row);
    assert_eq!(transcript_row_cursor_row(&app), tail_match.start.row);

    app.type_char('N');
    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), early_match.start.row);
    let row = app
        .transcript_rows_and_breaks_range(early_match.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        row.contains("early needle"),
        "cached reverse repeat did not reveal early match: {row:?}"
    );
}

#[test]
fn transcript_search_reverse_repeat_wraps_from_first_match() {
    let mut app = row_document_transcript_app(20, true);
    app.type_char('g');
    app.type_char('g');

    app.type_char('/');
    app.type_text("alpha");
    app.press(KeyCode::Enter);
    app.render_silent();
    let first_row = transcript_row_cursor_row(&app);

    app.type_char('N');
    app.render_silent();
    assert!(transcript_row_cursor_row(&app) > first_row);
    app.type_char('n');
    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), first_row);
}

#[test]
fn transcript_search_jump_keeps_match_below_top_overlay() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('G');

    app.type_char('/');
    app.type_text("row 010");
    app.press(KeyCode::Enter);
    app.render_silent();

    let match_row = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap()
        .start
        .row;
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(app.transcript_window().scroll_top, match_row - 1);

    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(app.transcript_window().scroll_top, match_row - 1);
}

#[test]
fn transcript_search_jump_keeps_match_above_bottom_overlay() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('g');
    app.type_char('g');

    app.type_char('/');
    app.type_text("row 090");
    app.press(KeyCode::Enter);
    app.render_silent();

    let match_row = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap()
        .start
        .row;
    let viewport_rows = app
        .transcript_window()
        .viewport
        .map(|v| v.rect.height as crate::smelt_edit::RowIndex)
        .unwrap_or(1);
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(
        app.transcript_window().scroll_top,
        match_row.saturating_sub(viewport_rows.saturating_sub(2))
    );

    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(
        app.transcript_window().scroll_top,
        match_row.saturating_sub(viewport_rows.saturating_sub(2))
    );
}

#[test]
fn transcript_reveal_api_keeps_cursor_below_top_padding() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('G');

    assert!(app.run_lua("smelt.win.transcript():reveal(10, { top_padding = 1 })"));

    assert_eq!(transcript_row_cursor_row(&app), 10);
    assert_eq!(app.transcript_window().scroll_top, 9);
}

#[test]
fn transcript_reveal_api_keeps_cursor_above_bottom_padding() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('g');
    app.type_char('g');

    assert!(app.run_lua("smelt.win.transcript():reveal(90, { bottom_padding = 1 })"));

    let viewport_rows = app
        .transcript_window()
        .viewport
        .map(|v| v.rect.height as crate::smelt_edit::RowIndex)
        .unwrap_or(1);
    assert_eq!(transcript_row_cursor_row(&app), 90);
    assert_eq!(
        app.transcript_window().scroll_top,
        90u64.saturating_sub(viewport_rows.saturating_sub(2))
    );
}

#[test]
fn transcript_cursor_api_reads_absolute_row_after_scroll() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('g');
    app.type_char('g');

    assert!(app.run_lua(
        r#"
        smelt.win.transcript():reveal(90)
        _G.transcript_cursor_row = smelt.win.transcript():cursor()
        "#
    ));

    assert_eq!(transcript_row_cursor_row(&app), 90);
    assert_eq!(app.lua_int_global("transcript_cursor_row"), Some(90));
}

#[test]
fn transcript_search_paints_visible_matches_and_nohl_aliases_clear() {
    for command in ["nohl", "nohlsearch", "noh"] {
        let mut app = row_document_transcript_app(20, true);
        start_visible_transcript_search(&mut app);

        run_cmdline(&mut app, command);
        app.render_silent();

        let win = app.transcript_window();
        assert!(
            win.search_ranges.is_empty(),
            "{command} should clear search highlights"
        );
        assert!(app.overlays_probe().search_session().is_none());
    }
}

#[test]
fn transcript_search_paints_visible_matches_and_esc_clears() {
    let mut app = row_document_transcript_app(20, true);
    start_visible_transcript_search(&mut app);

    app.press(KeyCode::Esc);
    app.render_silent();
    let win = app.transcript_window();
    assert!(win.search_ranges.is_empty());
    assert!(app.overlays_probe().search_session().is_none());
}

fn start_visible_transcript_search(app: &mut TestApp) {
    app.type_char('g');
    app.type_char('g');
    app.type_char('/');
    app.type_text("row 000");
    app.press(KeyCode::Enter);
    app.render_silent();

    let win = app.transcript_window();
    assert!(
        !win.search_ranges.is_empty(),
        "submitted search should paint visible matches"
    );
}

fn run_cmdline(app: &mut TestApp, command: &str) {
    app.press(KeyCode::Char(':'));
    app.type_text(command);
    app.press(KeyCode::Enter);
}

#[test]
fn transcript_search_finds_compacted_divider_label() {
    let mut app = searchable_transcript_app();
    app.push_transcript_block(smelt_core::transcript_model::Block::Compacted {
        summary: "archived earlier turns".into(),
    });
    app.render_silent();

    app.type_char('/');
    app.type_text("compacted");
    app.press(KeyCode::Enter);

    let range = app
        .overlays_probe()
        .search_session()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("compacted divider search match");
    let row = app
        .transcript_rows_and_breaks_range(range.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(row.contains("compacted"), "matched row: {row:?}");
}

#[test]
fn transcript_search_includes_selectable_compacted_separator_chrome() {
    let mut app = searchable_transcript_app();
    app.push_transcript_block(smelt_core::transcript_model::Block::Compacted {
        summary: "archived earlier turns".into(),
    });
    app.render_silent();

    let row = app
        .transcript_rows_and_breaks_range(0, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(row.contains('─'), "expected separator chrome: {row:?}");

    app.type_char('/');
    app.type_text("─");
    app.press(KeyCode::Enter);

    assert!(app
        .overlays_probe()
        .search_session()
        .and_then(|session| session.current_range())
        .is_some());
}

#[test]
fn transcript_search_finds_lua_rendered_collapsed_tool_detail() {
    let mut app = searchable_transcript_app();
    let mut args = std::collections::HashMap::new();
    args.insert("pattern".to_string(), serde_json::json!("needle"));
    let invocation_id = app.start_tool(
        "glob-call-1".into(),
        "glob".into(),
        protocol::StyledLines::from_plain("**/*.rs"),
        args,
    );
    app.finish_tool(
        invocation_id,
        smelt_core::transcript_model::ToolStatus::Ok,
        Some(Box::new(smelt_core::transcript_model::ToolOutput {
            content: "src/a.rs\nsrc/b.rs".into(),
            is_error: false,
            metadata: Some(serde_json::json!({
                "display_count": { "value": 12, "unit": "file" }
            })),
        })),
        None,
    );
    app.render_silent();

    app.type_char('/');
    app.type_text("12 files");
    app.press(KeyCode::Enter);

    let range = app
        .overlays_probe()
        .search_session()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("collapsed tool detail search match");
    let row = app
        .transcript_rows_and_breaks_range(range.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(row.contains("12 files"), "matched row: {row:?}");
}

#[test]
fn backward_search_starts_from_previous_match() {
    let mut app = row_document_transcript_app(20, true);
    app.type_char('5');
    app.type_char('G');
    assert_eq!(transcript_row_cursor_row(&app), 4);

    app.press(KeyCode::Char('?'));
    assert!(app.state().cmdline_open);
    app.type_text("alpha");
    app.press(KeyCode::Enter);
    app.render_silent();
    let current_row = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap()
        .start
        .row;
    assert_eq!(transcript_row_cursor_row(&app), current_row);

    app.type_char('n');
    app.render_silent();
    assert!(transcript_row_cursor_row(&app) < current_row);
    app.type_char('N');
    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), current_row);
}

#[test]
fn overlay_viewer_search_targets_focused_overlay() {
    let mut app = TestApp::builder().with_vim(true).build();
    let leaf =
        app.open_readonly_overlay_fixture(vec!["alpha beta".into(), "gamma delta".into()], None);
    app.render_silent();

    app.type_char('/');
    assert!(app.state().cmdline_open);
    app.type_text("gamma");
    app.press(KeyCode::Enter);

    let win = app.ui_probe().win(leaf).expect("overlay window");
    let buf = app.ui_probe().buf(win.buf).expect("overlay buffer");
    assert_eq!(buf.display_byte_pos(win.cpos()), (1, 0));
    assert_eq!(app.overlays_probe().search_session().unwrap().target, leaf);

    app.render_silent();
    let win = app.ui_probe().win(leaf).expect("overlay window");
    let buf_id = win.buf;
    assert!(
        win.range_layer(crate::smelt_edit::RangeLayer::Search)
            .iter()
            .any(|range| range.line == 1),
        "focused overlay search should paint visible matches"
    );

    app.set_window_lines(leaf, vec!["gamma moved".into(), "no match".into()]);
    app.render_silent();
    let win = app.ui_probe().win(leaf).expect("overlay window");
    let ranges = win.range_layer(crate::smelt_edit::RangeLayer::Search);
    assert!(
        ranges.iter().any(|range| range.line == 0),
        "search paint should follow live buffer contents"
    );
    assert!(
        !ranges.iter().any(|range| range.line == 1),
        "stale search ranges should not survive buffer rewrites"
    );

    app.set_window_lines(leaf, vec!["gamma first".into(), "gamma second".into()]);
    app.set_window_cursor(leaf, 0);
    app.type_char('n');
    let win = app.ui_probe().win(leaf).expect("overlay window");
    let buf = app.ui_probe().buf(buf_id).expect("overlay buffer");
    assert_eq!(buf.display_byte_pos(win.cpos()), (0, 0));
}

#[test]
fn viewer_search_ignores_non_selectable_spans() {
    let mut app = TestApp::builder().with_vim(true).build();
    let leaf = app.open_readonly_overlay_fixture(vec!["chrome real".into()], Some(6));
    app.render_silent();

    app.type_char('/');
    app.type_text("chrome");
    app.press(KeyCode::Enter);
    assert!(app
        .overlays_probe()
        .search_session()
        .unwrap()
        .full_matches()
        .is_empty());
    app.render_silent();
    let win = app.ui_probe().win(leaf).expect("overlay window");
    assert!(win
        .range_layer(crate::smelt_edit::RangeLayer::Search)
        .is_empty());

    app.type_char('/');
    app.type_text("real");
    app.press(KeyCode::Enter);
    let session = app.overlays_probe().search_session().unwrap();
    assert_eq!(session.full_matches().len(), 1);
    assert_eq!(
        session.full_matches()[0].rows().unwrap().start.byte_col,
        "chrome ".len()
    );
    app.render_silent();
    let win = app.ui_probe().win(leaf).expect("overlay window");
    assert!(
        !win.range_layer(crate::smelt_edit::RangeLayer::Search)
            .is_empty(),
        "selectable match should paint"
    );
}
