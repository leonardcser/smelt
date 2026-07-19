use super::*;

fn searchable_transcript_app() -> TestApp {
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(80, 16);
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    app
}

fn sparse_display_only_search_app(guard: &std::sync::MutexGuard<'static, ()>) -> TestApp {
    let mut app = TestApp::builder()
        .with_vim(true)
        .build_with_test_home_guard(guard);
    app.app.handle_resize(80, 16);
    let session_id = app.app.core.session.id.clone();
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut session =
        smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
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
            test_descriptor_record(idx, &content)
        })
        .collect::<Vec<_>>();
    db.apply_transcript_descriptor_fixture(&records).unwrap();
    drop(db);

    let loaded = crate::app::history::load_transcript_tail_from_sqlite_dir(session_dir, 80, 16)
        .expect("display-only transcript tail");
    session.history.clear();
    app.app.load_store_backed_session(
        crate::app::session_document::StoreBackedSessionDocument::new(
            session,
            loaded,
            crate::app::history::live_session_for_test(session_id, 200, None),
            receipt.current,
        ),
    );
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    app.render_silent();
    app
}

fn test_descriptor_record(
    block_idx: u64,
    content: &str,
) -> smelt_store::TranscriptDescriptorRecord {
    smelt_store::TranscriptDescriptorRecord {
        block_idx,
        history_idx: None,
        kind: "text".to_string(),
        tool_call_id: None,
        tool_name: None,
        content_hash: "0".to_string(),
        estimated_text_bytes: content.len() as u64,
        preview_text: content.to_string(),
        indexed_text: content.to_string(),
        descriptor_json: serde_json::to_string(
            &smelt_core::transcript_model::TranscriptBlockDescriptor::Text {
                content: content.to_string(),
            },
        )
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
        .app
        .search
        .session
        .as_ref()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert_eq!(transcript_row_cursor_row(&app), first_match.start.row);

    app.type_char('n');
    app.render_silent();
    let next_match = app
        .app
        .search
        .session
        .as_ref()
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
    let guard = test_home_guard();
    let mut app = sparse_display_only_search_app(&guard);
    app.type_char('G');

    app.type_char('/');
    app.type_text("needle");
    app.press(KeyCode::Enter);
    app.render_silent();

    let tail_match = app
        .app
        .search
        .session
        .as_ref()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    let tail_row = app
        .app
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
        .app
        .search
        .session
        .as_ref()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert!(early_match.start.row < tail_match.start.row);
    assert_eq!(transcript_row_cursor_row(&app), early_match.start.row);
    let early_row = app
        .app
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
    let guard = test_home_guard();
    let mut app = sparse_display_only_search_app(&guard);
    app.type_char('G');

    app.type_char('/');
    app.type_text("needle");
    app.press(KeyCode::Enter);
    app.render_silent();
    let tail_match = app
        .app
        .search
        .session
        .as_ref()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();

    app.type_char('N');
    app.render_silent();
    let early_match = app
        .app
        .search
        .session
        .as_ref()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert!(early_match.start.row < tail_match.start.row);
    assert_eq!(transcript_row_cursor_row(&app), early_match.start.row);
    let early_row = app
        .app
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
    let guard = test_home_guard();
    let mut app = sparse_display_only_search_app(&guard);
    app.type_char('g');
    app.type_char('g');

    app.type_char('/');
    app.type_text("needle");
    app.press(KeyCode::Enter);
    app.render_silent();
    let early_match = app
        .app
        .search
        .session
        .as_ref()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    let early_row = app
        .app
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
        .app
        .search
        .session
        .as_ref()
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
        .app
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
        .app
        .search
        .session
        .as_ref()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap()
        .start
        .row;
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(app.app.transcript_win().scroll_top(), match_row - 1);

    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(app.app.transcript_win().scroll_top(), match_row - 1);
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
        .app
        .search
        .session
        .as_ref()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap()
        .start
        .row;
    let viewport_rows = app
        .app
        .transcript_win()
        .viewport
        .map(|v| v.rect.height as crate::smelt_edit::RowIndex)
        .unwrap_or(1);
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        match_row.saturating_sub(viewport_rows.saturating_sub(2))
    );

    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        match_row.saturating_sub(viewport_rows.saturating_sub(2))
    );
}

#[test]
fn transcript_reveal_api_keeps_cursor_below_top_padding() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('G');

    assert!(app.run_lua("smelt.win.transcript():reveal(10, { top_padding = 1 })"));

    assert_eq!(transcript_row_cursor_row(&app), 10);
    assert_eq!(app.app.transcript_win().scroll_top(), 9);
}

#[test]
fn transcript_reveal_api_keeps_cursor_above_bottom_padding() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('g');
    app.type_char('g');

    assert!(app.run_lua("smelt.win.transcript():reveal(90, { bottom_padding = 1 })"));

    let viewport_rows = app
        .app
        .transcript_win()
        .viewport
        .map(|v| v.rect.height as crate::smelt_edit::RowIndex)
        .unwrap_or(1);
    assert_eq!(transcript_row_cursor_row(&app), 90);
    assert_eq!(
        app.app.transcript_win().scroll_top(),
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

        let win = app.app.transcript_win();
        assert!(
            win.range_layer(crate::smelt_edit::RangeLayer::Search)
                .is_empty(),
            "{command} should clear search highlights"
        );
        assert!(app.app.search.session.is_none());
    }
}

#[test]
fn transcript_search_paints_visible_matches_and_esc_clears() {
    let mut app = row_document_transcript_app(20, true);
    start_visible_transcript_search(&mut app);

    app.press(KeyCode::Esc);
    app.render_silent();
    let win = app.app.transcript_win();
    assert!(win
        .range_layer(crate::smelt_edit::RangeLayer::Search)
        .is_empty());
    assert!(app.app.search.session.is_none());
}

fn start_visible_transcript_search(app: &mut TestApp) {
    app.type_char('g');
    app.type_char('g');
    app.type_char('/');
    app.type_text("row 000");
    app.press(KeyCode::Enter);
    app.render_silent();

    let win = app.app.transcript_win();
    assert!(
        !win.range_layer(crate::smelt_edit::RangeLayer::Search)
            .is_empty(),
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
    app.app
        .push_block(smelt_core::transcript_model::Block::Compacted {
            summary: "archived earlier turns".into(),
        });
    app.render_silent();

    app.type_char('/');
    app.type_text("compacted");
    app.press(KeyCode::Enter);

    let range = app
        .app
        .search
        .session
        .as_ref()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("compacted divider search match");
    let row = app
        .app
        .transcript_rows_and_breaks_range(range.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(row.contains("compacted"), "matched row: {row:?}");
}

#[test]
fn transcript_search_includes_selectable_compacted_separator_chrome() {
    let mut app = searchable_transcript_app();
    app.app
        .push_block(smelt_core::transcript_model::Block::Compacted {
            summary: "archived earlier turns".into(),
        });
    app.render_silent();

    let row = app
        .app
        .transcript_rows_and_breaks_range(0, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(row.contains('─'), "expected separator chrome: {row:?}");

    app.type_char('/');
    app.type_text("─");
    app.press(KeyCode::Enter);

    assert!(app
        .app
        .search
        .session
        .as_ref()
        .and_then(|session| session.current_range())
        .is_some());
}

#[test]
fn transcript_search_finds_lua_rendered_collapsed_tool_detail() {
    let mut app = searchable_transcript_app();
    let mut args = std::collections::HashMap::new();
    args.insert("pattern".to_string(), serde_json::json!("needle"));
    app.app.start_tool(
        "glob-call-1".into(),
        "glob".into(),
        protocol::StyledLines::from_plain("**/*.rs"),
        args,
    );
    app.app.finish_tool(
        "glob-call-1",
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
        .app
        .search
        .session
        .as_ref()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("collapsed tool detail search match");
    let row = app
        .app
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
        .app
        .search
        .session
        .as_ref()
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
    let buf = app
        .app
        .ui
        .buf_create(crate::smelt_edit::BufCreateOpts::default());
    {
        let buf = app.app.ui.buf_mut(buf).expect("overlay buffer");
        buf.readonly = true;
        buf.set_all_lines(vec!["alpha beta".into(), "gamma delta".into()]);
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

    app.type_char('/');
    assert!(app.state().cmdline_open);
    app.type_text("gamma");
    app.press(KeyCode::Enter);

    let win = app.app.ui.win(leaf).expect("overlay window");
    let buf = app.app.ui.buf(win.buf).expect("overlay buffer");
    assert_eq!(buf.display_byte_pos(win.cpos()), (1, 0));
    assert_eq!(app.app.search.session.as_ref().unwrap().target, leaf);

    app.render_silent();
    let win = app.app.ui.win(leaf).expect("overlay window");
    let buf_id = win.buf;
    assert!(
        win.range_layer(crate::smelt_edit::RangeLayer::Search)
            .iter()
            .any(|range| range.line == 1),
        "focused overlay search should paint visible matches"
    );

    app.app
        .ui
        .buf_mut(buf_id)
        .expect("overlay buffer")
        .set_all_lines(vec!["gamma moved".into(), "no match".into()]);
    app.render_silent();
    let win = app.app.ui.win(leaf).expect("overlay window");
    let ranges = win.range_layer(crate::smelt_edit::RangeLayer::Search);
    assert!(
        ranges.iter().any(|range| range.line == 0),
        "search paint should follow live buffer contents"
    );
    assert!(
        !ranges.iter().any(|range| range.line == 1),
        "stale search ranges should not survive buffer rewrites"
    );

    app.app
        .ui
        .buf_mut(buf_id)
        .expect("overlay buffer")
        .set_all_lines(vec!["gamma first".into(), "gamma second".into()]);
    app.app
        .ui
        .win_mut(leaf)
        .expect("overlay window")
        .set_cpos(0);
    app.type_char('n');
    let win = app.app.ui.win(leaf).expect("overlay window");
    let buf = app.app.ui.buf(buf_id).expect("overlay buffer");
    assert_eq!(buf.display_byte_pos(win.cpos()), (0, 0));
}

#[test]
fn viewer_search_ignores_non_selectable_spans() {
    let mut app = TestApp::builder().with_vim(true).build();
    let buf = app
        .app
        .ui
        .buf_create(crate::smelt_edit::BufCreateOpts::default());
    {
        let buf = app.app.ui.buf_mut(buf).expect("overlay buffer");
        buf.readonly = true;
        buf.set_all_lines(vec!["chrome real".into()]);
        buf.add_highlight_group_with_meta(
            0,
            0,
            6,
            smelt_buffer::theme::intern("Normal"),
            crate::smelt_edit::SpanMeta::unselectable(),
        );
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

    app.type_char('/');
    app.type_text("chrome");
    app.press(KeyCode::Enter);
    assert!(app
        .app
        .search
        .session
        .as_ref()
        .unwrap()
        .full_matches()
        .is_empty());
    app.render_silent();
    let win = app.app.ui.win(leaf).expect("overlay window");
    assert!(win
        .range_layer(crate::smelt_edit::RangeLayer::Search)
        .is_empty());

    app.type_char('/');
    app.type_text("real");
    app.press(KeyCode::Enter);
    let session = app.app.search.session.as_ref().unwrap();
    assert_eq!(session.full_matches().len(), 1);
    assert_eq!(
        session.full_matches()[0].rows().unwrap().start.byte_col,
        "chrome ".len()
    );
    app.render_silent();
    let win = app.app.ui.win(leaf).expect("overlay window");
    assert!(
        !win.range_layer(crate::smelt_edit::RangeLayer::Search)
            .is_empty(),
        "selectable match should paint"
    );
}
