use super::*;

#[test]
fn transcript_search_opens_status_input_and_repeats_matches() {
    let mut app = row_document_transcript_app(20, true);
    app.type_char('g');
    app.type_char('g');

    app.type_char('/');
    assert!(app.state().cmdline_open);
    app.type_text("alpha");
    app.press(KeyCode::Enter);

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
    assert_eq!(transcript_row_cursor_row(&app), first_match.start.row);
}

#[test]
fn transcript_search_reverse_repeat_wraps_from_first_match() {
    let mut app = row_document_transcript_app(20, true);
    app.type_char('g');
    app.type_char('g');

    app.type_char('/');
    app.type_text("alpha");
    app.press(KeyCode::Enter);
    let first_row = transcript_row_cursor_row(&app);

    app.type_char('N');
    assert!(transcript_row_cursor_row(&app) > first_row);
    app.type_char('n');
    assert_eq!(transcript_row_cursor_row(&app), first_row);
}

#[test]
fn transcript_search_jump_keeps_match_below_top_overlay() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('G');

    app.type_char('/');
    app.type_text("row 010");
    app.press(KeyCode::Enter);

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
fn transcript_search_paints_visible_matches_and_esc_clears() {
    let mut app = row_document_transcript_app(20, true);
    app.type_char('g');
    app.type_char('g');
    app.type_char('/');
    app.type_text("row 000");
    app.press(KeyCode::Enter);
    app.render_silent();

    let win = app.app.transcript_win();
    let buf = app.app.ui.buf(win.buf).expect("transcript buffer");
    assert!(
        !buf.range_layer(crate::smelt_edit::RangeLayer::Search)
            .is_empty(),
        "submitted search should paint visible matches"
    );

    app.press(KeyCode::Esc);
    app.render_silent();
    let win = app.app.transcript_win();
    let buf = app.app.ui.buf(win.buf).expect("transcript buffer");
    assert!(buf
        .range_layer(crate::smelt_edit::RangeLayer::Search)
        .is_empty());
    assert!(app.app.search.session.is_none());
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
    assert!(transcript_row_cursor_row(&app) < current_row);
    app.type_char('N');
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

    app.type_char('/');
    app.type_text("real");
    app.press(KeyCode::Enter);
    let session = app.app.search.session.as_ref().unwrap();
    assert_eq!(session.full_matches().len(), 1);
    assert_eq!(
        session.full_matches()[0].rows().unwrap().start.byte_col,
        "chrome ".len()
    );
}
