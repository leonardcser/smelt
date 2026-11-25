use super::*;

#[test]
fn upgrade_status_view_shows_shared_download_progress() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    app.run_lua_result(r#"
        smelt.settings.autoupgrade = "off"
        smelt.process.run = function(_, args)
          assert(args[2] == "status", "status view must not start an installation")
          return { exit_code = 0, stdout = '{"current":"v0.6.2","status":{"operation_id":"shared-worker","channel":"stable","target":"v0.7.0","stage":"downloading","running":true,"downloaded_bytes":18874368,"total_bytes":33554432,"executable":"/opt/smelt/bin/smelt"}}' }
        end
        smelt.cmd.run("upgrade status")
    "#).unwrap();
    drive_lua_tasks(&mut app);
    insta::assert_snapshot!("upgrade_status_download", upgrade_status_dialog(&mut app));
    app.set_terminal_size(60, 24);
    insta::assert_snapshot!(
        "upgrade_status_download_narrow",
        upgrade_status_dialog(&mut app)
    );
    app.type_char('q');
    assert!(app.run_lua("assert(smelt.state.get('upgrade').snapshot.status.running)"));
}

#[test]
fn upgrade_status_view_updates_guidance_without_reopening() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 24);
    assert!(app.run_lua(
        r#"
        smelt.settings.autoupgrade = "off"
        smelt.process.run = function() return upgrade_reply end
        "#
    ));
    for (name, reply) in [
        (
            "upgrade_status_empty",
            r#"{ exit_code = 0, stdout = '{"current":"v0.6.2"}' }"#,
        ),
        (
            "upgrade_status_failed",
            r#"{ exit_code = 0, stdout = '{"current":"v0.6.2","status":{"stage":"failed","error":"Checksum mismatch","running":false}}' }"#,
        ),
        (
            "upgrade_status_installed",
            r#"{ exit_code = 0, stdout = '{"current":"v0.7.0","status":{"stage":"installed","target":"v0.7.0","running":false}}' }"#,
        ),
        (
            "upgrade_status_unavailable",
            r#"{ exit_code = 1, stderr = 'error: status file unavailable' }"#,
        ),
    ] {
        assert!(app.run_lua(&format!(
            "upgrade_reply = {reply}; smelt.cmd.run('upgrade status')"
        )));
        drive_lua_tasks(&mut app);
        insta::assert_snapshot!(name, upgrade_status_dialog(&mut app));
    }
}

#[test]
fn startup_banner_shows_compact_upgrade_status() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 24);
    assert!(app.run_lua(
        r#"
        smelt.settings.autoupgrade = "off"
        smelt.build.display = "v0.6.2"
        smelt.process.run = function() return upgrade_reply end
        "#
    ));
    for (name, reply) in [
        (
            "upgrade_banner_downloading",
            r#"{ exit_code = 0, stdout = '{"current":"v0.6.2","status":{"operation_id":"w","channel":"stable","target":"v0.7.0","stage":"downloading","running":true,"downloaded_bytes":18874368,"total_bytes":33554432}}' }"#,
        ),
        (
            "upgrade_banner_downloading_unstable",
            r#"{ exit_code = 0, stdout = '{"current":"v0.6.2","status":{"operation_id":"w","channel":"unstable","target":"main@abc1234","stage":"downloading","running":true,"downloaded_bytes":1048576,"total_bytes":2097152}}' }"#,
        ),
        (
            "upgrade_banner_failed",
            r#"{ exit_code = 0, stdout = '{"current":"v0.6.2","status":{"stage":"failed","error":"Checksum mismatch","running":false}}' }"#,
        ),
        (
            "upgrade_banner_installed",
            r#"{ exit_code = 0, stdout = '{"current":"v0.7.0","status":{"stage":"installed","target":"v0.7.0","running":false}}' }"#,
        ),
    ] {
        assert!(app.run_lua(&format!(
            "upgrade_reply = {reply}; smelt.cmd.run('upgrade status')"
        )));
        drive_lua_tasks(&mut app);
        app.press(KeyCode::Esc);
        insta::assert_snapshot!(name, upgrade_banner_frame(&mut app));
    }
}

fn upgrade_banner_frame(app: &mut TestApp) -> String {
    app.render_to_frame().text()
}

fn upgrade_status_dialog(app: &mut TestApp) -> String {
    app.render_to_frame()
        .text()
        .lines()
        .skip_while(|line| !line.starts_with("─ upgrade status "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn open_root_test_dialog(app: &mut TestApp) {
    assert!(app.run_lua(
        r#"
        local leaf = smelt.dialog.content({ text = "Review this action" })
        smelt.dialog.new({
          panels = { { leaf = leaf, height = "fit" } },
          blocks_agent = true,
        })
        "#
    ));
}

fn assert_safe_dialog_fallback(app: &TestApp) {
    assert!(app.ui_probe().split_rect(crate::app::PROMPT_WIN).is_none());
    let statusline = app
        .ui_probe()
        .named_win("smelt.statusline")
        .expect("statusline window");
    let status_rect = app
        .ui_probe()
        .split_rect(statusline)
        .expect("fallback keeps statusline mounted");
    assert_eq!(
        status_rect.y + status_rect.height,
        app.ui_probe().terminal_size().1
    );
}

#[test]
fn wrapped_menu_uses_its_layout_width_before_first_paint() {
    for overlay in [false, true] {
        let mut app = TestApp::builder().build();
        app.set_terminal_size(100, 24);
        app.run_lua_result(&format!("use_overlay = {overlay}"))
            .unwrap();
        app.run_lua_result(
            r#"
            local layout = smelt.ui.layout
            menu, menu_ctrl = smelt.dialog.menu({
                "write the parser and handle invalid input with helpful error messages",
                "(current)",
            }, { wrap = true, selected = 2 })
            if use_overlay then
                smelt.overlay.new({layout = layout.leaf(menu), width = "50%", height = 12})
                menu:focus()
            else
                local side = smelt.win.new(smelt.buf.new())
                smelt.dialog.new({title = "menu", panels = {{leaf = menu}}})
                smelt.ui.layout.set(function(state)
                    return layout.hsplit(layout.leaf(side), state.dialog,
                        {size = "50%", min_first = 0, min_second = 0})
                end)
            end
        "#,
        )
        .unwrap();

        for width in [100, 60, 120] {
            app.set_terminal_size(width, 24);
            let frame = app.render_to_frame().text();
            let win = app.ui_probe().win(app.ui_probe().focus().unwrap()).unwrap();
            let buf = app.ui_probe().buf(win.buf).unwrap();
            let content_width = usize::from(win.viewport.unwrap().content_width);
            assert!(content_width < usize::from(width) / 2);
            assert!(buf.line_count() > 2, "{frame}");
            for row in 0..buf.line_count() {
                assert!(
                    smelt_buffer::cell_width::text_width(buf.get_line(row).unwrap())
                        <= content_width,
                    "{frame}"
                );
            }
            assert_eq!(
                buf.get_line(win.cursor_row() as usize),
                Some(" 2. (current)")
            );
            assert!(frame.contains("2. (current)"), "{frame}");
        }
    }
}

#[test]
fn menu_row_mapping_handles_newlines_clipping_and_narrow_widths() {
    for wrap in ["nil", "true", "false"] {
        let mut app = TestApp::builder().build();
        app.set_terminal_size(40, 20);
        app.run_lua_result(&format!(r#"
            menu, ctrl = smelt.dialog.menu({{
                {{label = "First long label that wraps\n\nLast line", description = "First description\n\nLast description line"}},
                {{label = "Second 界 é 👩‍💻", description = "A long description that needs continuation rows"}},
                {{label = "Disabled", disabled = true}},
                {{label = "Fourth"}},
            }}, {{wrap = {wrap}, selected = 2, shortcuts = "select"}})
            smelt.dialog.new({{panels = {{{{leaf = menu}}}}}})
        "#)).unwrap();
        for width in [40, 6, 52] {
            app.set_terminal_size(width, 20);
            let frame = app.render_to_frame();
            let row = frame
                .rows
                .iter()
                .position(|row| row.contains("2."))
                .unwrap_or_else(|| panic!("{}", frame.text()));
            assert_eq!(
                frame.styles[row][0].bg,
                app.ui_probe().theme().get("CursorLine").bg,
                "wrap={wrap}: {}",
                frame.text()
            );
            app.run_lua_result("assert(ctrl:cursor() == 2)").unwrap();
        }
        app.press(KeyCode::Down);
        app.run_lua_result("assert(ctrl:cursor() == 4)").unwrap();
        app.press(KeyCode::Up);
        app.run_lua_result("assert(ctrl:cursor() == 2)").unwrap();
        for (key, index) in [(KeyCode::End, 4), (KeyCode::Home, 1)] {
            app.press(key);
            let frame = app.render_to_frame();
            let prefix = format!(" {index}. ");
            let row = frame
                .rows
                .iter()
                .position(|row| row.contains(&prefix))
                .unwrap();
            assert_eq!(
                frame.styles[row][0].bg,
                app.ui_probe().theme().get("CursorLine").bg,
                "{}",
                frame.text()
            );
        }
    }
}

#[test]
fn menu_external_cursor_changes_keep_highlight_and_submission_in_sync() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 20);
    app.run_lua_result(r#"
        menu, ctrl = smelt.dialog.menu({
            {label = "First", description = "A description long enough to wrap across several rows"},
            {label = "Second", description = "Select this item from its description row"},
            {label = "Third"},
        }, {on_submit = function(ctx) picked = ctx.index end})
        menu:key("x", function()
            ctrl:cursor(3)
            ctrl:cursor(1)
            for row, line in ipairs(menu:buf():lines()) do
                if line:match("^ 2%. ") then menu:cursor(row); break end
            end
        end)
        smelt.dialog.new({panels = {{leaf = menu}}})
    "#).unwrap();
    app.render_silent();
    app.press(KeyCode::Char('x'));
    for width in [40, 24, 60] {
        app.set_terminal_size(width, 20);
        let frame = app.render_to_frame();
        let row = frame
            .rows
            .iter()
            .position(|row| row.contains(" 2. "))
            .unwrap();
        assert_eq!(
            frame.styles[row][0].bg,
            app.ui_probe().theme().get("CursorLine").bg,
            "{}",
            frame.text()
        );
        app.run_lua_result("assert(ctrl:cursor() == 2)").unwrap();
    }
    app.press(KeyCode::Enter);
    app.run_lua_result("assert(picked == 2)").unwrap();
}

#[test]
fn retained_renderer_tracks_layout_size_without_resize_callbacks() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(50, 24);
    app.run_lua_result(
        r#"
        local header, header_buf = smelt.dialog.content({text = "Header"})
        local buf = smelt.buf.new()
        local leaf = smelt.dialog.list(buf)
        render_calls = 0
        leaf:set_renderer(function()
            render_calls = render_calls + 1
            buf:lines({"available " .. leaf:rect().height .. " rows"})
        end)
        leaf:key("x", function() header_buf:lines({"Header", "More", "Context", "Here"}) end)
        smelt.dialog.new({height = 12, panels = {
            {leaf = header, height = "fit"}, {leaf = leaf, height = "fill"},
        }})
    "#,
    )
    .unwrap();
    for resized in [false, true] {
        if resized {
            app.press(KeyCode::Char('x'));
        }
        let frame = app.render_to_frame();
        let win = app.ui_probe().win(app.ui_probe().focus().unwrap()).unwrap();
        let expected = format!("available {} rows", win.viewport.unwrap().rect.height);
        assert!(frame.text().contains(&expected), "{}", frame.text());
        let calls = app.lua_int_global("render_calls").unwrap();
        assert_eq!(calls, if resized { 2 } else { 1 });
        app.dispatch_ui_window_events(false);
        app.render_silent();
        assert_eq!(app.lua_int_global("render_calls"), Some(calls));
    }
}

#[test]
fn retained_overlay_renderer_sees_resolved_size_before_paint() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    app.run_lua_result(
        r#"
        local buf = smelt.buf.new()
        local leaf = smelt.dialog.list(buf)
        render_calls = 0
        leaf:set_renderer(function()
            render_calls = render_calls + 1
            local rect = leaf:rect()
            buf:lines({rect.width .. " x " .. rect.height .. " : " .. leaf:content_width()})
        end)
        smelt.overlay.new({layout = smelt.ui.layout.leaf(leaf), width = "50%", height = "50%"})
        leaf:focus()
    "#,
    )
    .unwrap();
    for (step, (width, height)) in [(100, 24), (80, 20), (60, 28)].into_iter().enumerate() {
        app.set_terminal_size(width, height);
        let frame = app.render_to_frame();
        let win = app.ui_probe().win(app.ui_probe().focus().unwrap()).unwrap();
        let viewport = win.viewport.unwrap();
        let expected = format!(
            "{} x {} : {}",
            viewport.rect.width, viewport.rect.height, viewport.content_width
        );
        assert!(frame.text().contains(&expected), "{}", frame.text());
        assert_eq!(app.lua_int_global("render_calls"), Some(step as i64 + 1));
        app.dispatch_ui_window_events(false);
        app.render_silent();
        assert_eq!(app.lua_int_global("render_calls"), Some(step as i64 + 1));
    }
}

#[test]
fn structured_list_does_not_rebuild_after_painting_a_resize() {
    let mut app = TestApp::builder().build();
    app.run_lua_result(
        r#"
        local buf = smelt.buf.new()
        local leaf = smelt.dialog.list(buf)
        render_calls = 0
        smelt.list.new({leaf = leaf, buf = buf, items = {"One", "Two"},
            render = function(item)
                render_calls = render_calls + 1
                return {text = item}
            end,
        })
        smelt.dialog.new({height = 10, panels = {{leaf = leaf}}})
    "#,
    )
    .unwrap();
    for width in [40, 24, 60] {
        app.set_terminal_size(width, 20);
        app.render_silent();
        let calls = app.lua_int_global("render_calls").unwrap();
        app.dispatch_ui_window_events(false);
        app.render_silent();
        assert_eq!(app.lua_int_global("render_calls"), Some(calls));
    }
}

#[test]
fn wrapped_menu_preserves_item_index_when_replacing_rows() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 20);
    app.run_lua_result(
        r#"
        menu, ctrl = smelt.dialog.menu({"First", "Second", "Third"}, {
            wrap = true, selected = 2, shortcuts = "select",
        })
        smelt.dialog.new({panels = {{leaf = menu}}})
    "#,
    )
    .unwrap();
    app.render_silent();
    app.run_lua_result(r#"
        ctrl:set_items({
            {label = "First label with enough text to wrap across several rows", description = "Description"},
            {label = "Second", description = "Still selected"},
            {label = "Third", description = "Not selected"},
        })
        assert(ctrl:cursor() == 2, "replacing rows must preserve the selected item")
    "#).unwrap();
}

#[test]
fn structured_list_fits_rows_before_first_paint_and_resize() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 20);
    app.run_lua_result(r#"
        local buf = smelt.buf.new()
        local leaf = smelt.dialog.list(buf)
        local list = smelt.list.new({
            leaf = leaf, buf = buf,
            items = {"A long item that must fit without wrapping onto the next option", "Selected item", "Last item"},
            render = function(item) return {text = item} end,
        })
        list:set_cursor(1)
        smelt.dialog.new({panels = {{leaf = leaf}}})
    "#).unwrap();
    for width in [40, 24, 60] {
        app.set_terminal_size(width, 20);
        let frame = app.render_to_frame();
        let row = frame
            .rows
            .iter()
            .position(|row| row.contains("Selected item"))
            .unwrap_or_else(|| panic!("{}", frame.text()));
        let col = frame.rows[row].find("Selected item").unwrap();
        assert_eq!(
            frame.styles[row][col + 3].bg,
            app.ui_probe().theme().get("CursorLine").bg,
            "{}",
            frame.text()
        );
        let win = app.ui_probe().win(app.ui_probe().focus().unwrap()).unwrap();
        assert_eq!(win.cursor_row(), 1);
        assert_eq!(
            win.cursor_col(),
            0,
            "refitting must re-anchor the cursor byte position"
        );
        let buf = app.ui_probe().buf(win.buf).unwrap();
        let width = usize::from(win.viewport.unwrap().content_width);
        for row in 0..buf.line_count() {
            assert!(smelt_buffer::cell_width::text_width(buf.get_line(row).unwrap()) <= width);
        }
    }
}

#[test]
fn rewind_wrapped_messages_keep_selection_visible_across_navigation_and_resize() {
    fn assert_selection(app: &mut TestApp, index: usize) {
        for _ in 0..3 {
            app.render_silent();
            app.dispatch_ui_window_events(false);
        }
        let frame = app.render_to_frame().text();
        let dialog = frame.split("─ rewind ").nth(1).expect("rewind dialog");
        let win = app.ui_probe().win(app.ui_probe().focus().unwrap()).unwrap();
        let buf = app.ui_probe().buf(win.buf).unwrap();
        let prefix = format!(" {index}. ");
        assert!(buf
            .get_line(win.cursor_row() as usize)
            .unwrap()
            .starts_with(&prefix));
        assert!(dialog.contains(&prefix), "{frame}");
        let width = usize::from(win.viewport.unwrap().content_width);
        for row in 0..buf.line_count() {
            let line = buf.get_line(row).unwrap();
            assert!(
                smelt_buffer::cell_width::text_width(line) <= width,
                "{line:?}"
            );
        }
    }

    for vim in [false, true] {
        let mut app = TestApp::builder().with_vim(vim).build();
        app.set_terminal_size(60, 14);
        let messages: Vec<_> = (1..=12)
            .map(|index| {
                format!(
                    "message {index}: {}end-{index}\nkeep the full original input",
                    "界 e\u{301} 👩\u{200d}💻 render this message correctly ".repeat(4)
                )
            })
            .collect();
        for message in &messages {
            app.push_user_block(message);
        }
        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);
        drive_lua_tasks(&mut app);
        assert_selection(&mut app, 13);

        app.press(KeyCode::Up);
        assert_selection(&mut app, 12);
        app.press(KeyCode::Up);
        assert_selection(&mut app, 11);
        app.press(KeyCode::Down);
        assert_selection(&mut app, 12);

        // At the narrowest size a single message is taller than the viewport.
        for (width, height) in [(24, 8), (96, 24), (40, 12)] {
            app.set_terminal_size(width, height);
            assert_selection(&mut app, 12);
        }

        app.type_char('2');
        assert_selection(&mut app, 2);
        assert_eq!(app.session_history().len(), messages.len());
        app.press(KeyCode::Down);
        assert_selection(&mut app, 3);
        app.press(KeyCode::Enter);
        drive_lua_tasks(&mut app);
        assert!(app.state().active_modal.is_none());
        assert_eq!(app.state().prompt_text, messages[2]);
        assert_eq!(app.session_history().len(), 2);
    }
}

#[test]
fn splash_paint_stays_below_global_overlays() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.drain_launch_ready_hooks();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ name = "test.paint_order.buf" })
        local lines = {}
        for _ = 1, 12 do lines[#lines + 1] = string.rep("X", 30) end
        buf:lines(lines)
        local win = smelt.win.new(buf, {
          name = "test.paint_order.win",
          scrollbar = false,
          wrap = false,
        })
        smelt.overlay.new({
          name = "test.paint_order.overlay",
          layout = smelt.ui.layout.leaf(win),
          anchor = "center",
          width = 30,
          height = 12,
          z = 100,
        })
        "#,
    ));

    let frame = app.render_to_frame();
    let overlay_win = app
        .ui_probe()
        .named_win("test.paint_order.win")
        .expect("overlay window");
    let overlay_rect = app
        .ui_probe()
        .win(overlay_win)
        .and_then(|win| win.viewport.map(|viewport| viewport.rect))
        .expect("overlay viewport");

    for row in overlay_rect.y..overlay_rect.bottom() {
        let cells: Vec<char> = frame.rows[row as usize].chars().collect();
        assert!(
            cells[overlay_rect.x as usize..overlay_rect.right() as usize]
                .iter()
                .all(|cell| *cell == 'X'),
            "splash paint escaped its transcript layer into the overlay:\n{}",
            frame.text()
        );
    }
}

#[test]
fn splash_shows_the_theme_background_around_the_logo() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    assert!(app.run_lua(r#"smelt.theme.set("Normal", { bg = { 22, 10, 30 } })"#));
    app.drain_launch_ready_hooks();

    let frame = app.render_to_frame();
    let row = frame
        .rows
        .iter()
        .position(|row| row.contains(['▀', '▄']))
        .expect("splash logo on screen");
    let styles = &frame.styles[row];
    let theme_bg = styles[0].bg;
    assert!(theme_bg.is_some(), "the theme paints a background");
    for (x, ch) in frame.rows[row].chars().enumerate() {
        if ch == ' ' {
            assert_eq!(
                styles[x].bg,
                theme_bg,
                "blank splash cell at column {x} covers the theme background:\n{}",
                frame.text()
            );
        }
    }
}

#[test]
fn root_dialog_replaces_composer_and_restores_prompt_state() {
    let mut app = TestApp::builder().build();
    app.type_text("draft response");
    assert_eq!(app.state().prompt_text, "draft response");

    open_root_test_dialog(&mut app);
    app.render_silent();

    assert!(app.state().active_modal.is_some());
    assert!(app.ui_probe().split_rect(crate::app::PROMPT_WIN).is_none());
    assert_eq!(app.state().prompt_text, "draft response");
    assert!(!app.render_to_frame().text().contains("draft response"));

    assert!(app.run_lua("smelt.dialog.current().close()"));
    app.render_silent();

    assert!(app.state().active_modal.is_none());
    assert!(app.ui_probe().split_rect(crate::app::PROMPT_WIN).is_some());
    assert_eq!(app.state().prompt_text, "draft response");
    assert!(app.render_to_frame().text().contains("draft response"));
}

#[test]
fn dialog_input_edits_and_renders_unicode_by_grapheme() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local leaf, buf = smelt.dialog.input("custom answer", { wrap = true })
        _G.__unicode_answer_buf = buf
        smelt.dialog.new({
          title = "question",
          focus = leaf,
          panels = { { leaf = leaf, height = "fit" } },
        })
        "#
    ));
    let input = "e\u{301}👩\u{200d}💻9\u{fe0f}🇨🇦";

    app.type_text(input);
    let frame = app.render_to_frame().text();
    for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
        assert!(frame.contains(grapheme), "frame: {frame}");
    }

    app.press(KeyCode::Left);
    app.press(KeyCode::Backspace);
    app.press(KeyCode::Delete);

    let edited = app
        .eval_lua::<String>("return _G.__unicode_answer_buf:line(1)")
        .unwrap();
    assert_eq!(edited, "e\u{301}👩\u{200d}💻");
    let frame = app.render_to_frame().text();
    assert!(frame.contains("e\u{301}"), "frame: {frame}");
    assert!(frame.contains("👩\u{200d}💻"), "frame: {frame}");
}

#[test]
fn dialogs_dismiss_from_the_keyboard_without_mouse_focus() {
    let mut app = TestApp::builder().build();

    open_root_test_dialog(&mut app);
    assert!(
        app.ui_probe().focused_modal().is_some(),
        "opening a dialog should move keyboard focus into it"
    );
    app.press(KeyCode::Esc);
    assert!(
        app.state().active_modal.is_none(),
        "Esc should close a dialog without a mouse click"
    );

    assert!(app.run_lua(r#"smelt.dialog.viewer({ title = "viewer", text = "read only" })"#));
    app.type_char('q');
    assert!(
        app.state().active_modal.is_none(),
        "q should close a read-only dialog without a mouse click"
    );
}

#[test]
fn usage_dialog_opened_during_active_turn_stays_dismissible() {
    for (code, modifiers, label) in [
        (KeyCode::Char('q'), KeyModifiers::NONE, "q"),
        (KeyCode::Esc, KeyModifiers::NONE, "Esc"),
        (KeyCode::Char('c'), KeyModifiers::CONTROL, "Ctrl-C"),
    ] {
        let mut app = TestApp::builder().build();
        app.type_text("start working");
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        assert!(
            app.agent_running(),
            "submitting a prompt should start a turn"
        );

        app.type_text("/usage");
        assert!(
            app.state().picker_count > 0,
            "slash completer should be open"
        );
        app.press(KeyCode::Enter);
        assert_eq!(
            app.state().picker_count,
            0,
            "accepting /usage should close the completer"
        );
        assert!(
            app.state().active_modal.is_some(),
            "/usage should open a dialog"
        );
        assert!(
            app.ui_probe().focused_modal().is_some(),
            "/usage should own keyboard focus after prompt submission"
        );
        assert!(app.run_lua(
            r#"
            smelt.spawn(function()
              smelt.sleep(10)
              _G.__usage_focus_callback_ran = true
              smelt.transcript.win:focus()
            end)
            "#,
        ));
        app.settle_lua();

        app.engine_event(protocol::EngineEvent::TextDelta {
            delta: "working".into(),
        });
        app.feed_one(SourceEvent::Tick(10));
        app.tick_timers();
        app.settle_lua();
        app.render_silent();
        assert!(
            app.eval_lua::<bool>("return __usage_focus_callback_ran == true")
                .unwrap(),
            "background callback should run"
        );
        assert!(
            app.ui_probe().focused_modal().is_some(),
            "background callbacks must not move focus outside /usage"
        );
        assert_eq!(
            app.state().app_focus,
            AppFocus::Prompt,
            "a rejected focus request must not change the active app pane"
        );

        app.press_mod(code, modifiers);
        assert!(
            app.state().active_modal.is_none(),
            "{label} should close /usage while the active turn continues"
        );
        app.render_silent();
        assert_eq!(app.state().app_focus, AppFocus::Prompt);
        assert_eq!(app.ui_probe().focus(), Some(crate::app::PROMPT_WIN));
        assert!(app.agent_running());
    }
}

#[test]
fn deferred_lua_focus_commits_app_pane_only_after_layout_accepts_it() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function()
          return smelt.ui.layout.leaf(smelt.prompt.win)
        end)
        "#,
    ));
    app.render_silent();
    assert!(app
        .ui_probe()
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .is_none());
    assert_eq!(app.state().app_focus, AppFocus::Prompt);

    assert!(app
        .exec_lua_entry(
            r#"
            smelt.ui.layout.set(function()
              return smelt.ui.layout.leaf(smelt.transcript.win)
            end)
            smelt.transcript.win:focus()
            "#,
        )
        .is_ok());
    assert_eq!(
        app.state().app_focus,
        AppFocus::Prompt,
        "deferred focus must not commit before its target is mounted"
    );

    app.render_silent();
    assert!(app
        .ui_probe()
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .is_some());
    assert_eq!(app.ui_probe().focus(), Some(crate::app::TRANSCRIPT_WIN));
    assert_eq!(app.state().app_focus, AppFocus::Content);
}

#[test]
fn failed_root_dialog_open_does_not_pollute_dialog_stack() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        local leaf = smelt.dialog.content({ text = "Review this action" })
        local ok = pcall(function()
          smelt.dialog.new({
            panels = { { leaf = leaf, height = "fit" } },
            min_height = "invalid",
          })
        end)
        assert(not ok, "invalid dialog constraint should fail")
        assert(smelt.dialog.current() == nil, "failed dialog leaked into stack")
        "#
    ));
    assert!(app.active_docked_dialog().is_none());
}

#[test]
fn root_dialog_can_disable_top_edge_resize() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local leaf = smelt.dialog.content({ text = "Fixed dialog" })
        smelt.dialog.new({
          panels = { { leaf = leaf, height = "fit" } },
          resizable = false,
        })
        "#
    ));

    let dialog = app.active_docked_dialog().expect("active dialog");
    assert!(
        !app.ui_probe()
            .docked_surface(dialog)
            .expect("docked surface")
            .resize_config()
            .top
    );
}

#[test]
fn root_dialog_uses_safe_fallback_when_custom_composer_omits_it() {
    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function()
          return smelt.ui.layout.leaf(smelt.prompt.win)
        end)
        "#
    ));

    app.render_silent();

    assert_safe_dialog_fallback(&app);
    let dialog = app.active_docked_dialog().expect("active dialog");
    let root = app
        .ui_probe()
        .modal_leaves(
            app.ui_probe()
                .docked_surface(dialog)
                .expect("docked surface")
                .modal(),
        )
        .and_then(|leaves| leaves.first())
        .copied()
        .expect("dialog root");
    assert!(app.ui_probe().split_rect(root).is_some());
}

#[test]
fn root_dialog_uses_safe_fallback_when_custom_composer_duplicates_it() {
    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function(state)
          if not state.dialog then
            return smelt.ui.layout.leaf(smelt.prompt.win)
          end
          return smelt.ui.layout.vbox({
            { state.dialog, height = "fill" },
            { state.dialog, height = "fill" },
          })
        end)
        "#
    ));

    app.render_silent();

    assert_safe_dialog_fallback(&app);
}

#[test]
fn root_dialog_rejects_retained_stage_from_nested_dialog() {
    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        local retained
        smelt.ui.layout.set(function(state)
          if not state.dialog then
            return smelt.ui.layout.leaf(smelt.prompt.win)
          end
          if not retained then
            retained = state.dialog
            return state.dialog
          end
          return smelt.ui.layout.vbox({
            { retained, height = "fill" },
            { state.dialog, height = "fill" },
          })
        end)
        "#
    ));

    open_root_test_dialog(&mut app);
    app.render_silent();

    assert_safe_dialog_fallback(&app);
}

#[test]
fn root_dialog_hides_and_restores_queued_input_chrome() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.type_text("queued follow-up");
    app.press(KeyCode::Enter);
    assert_eq!(app.state().queued_inputs, vec!["queued follow-up"]);
    assert!(app.render_to_frame().text().contains("queued follow-up"));

    open_root_test_dialog(&mut app);

    assert_eq!(app.state().queued_inputs, vec!["queued follow-up"]);
    assert!(!app.render_to_frame().text().contains("queued follow-up"));

    assert!(app.run_lua("smelt.dialog.current().close()"));

    assert_eq!(app.state().queued_inputs, vec!["queued follow-up"]);
    assert!(app.render_to_frame().text().contains("queued follow-up"));
}

#[test]
fn expanding_root_dialog_preserves_focused_panel() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local first = smelt.dialog.content({ text = "First panel", interactive = true })
        local second = smelt.dialog.content({ text = "Second panel", interactive = true })
        smelt.dialog.new({
          panels = {
            { leaf = first, height = "fit" },
            { leaf = second, height = "fit" },
          },
        })
        second:focus()
        "#
    ));
    app.render_silent();
    let focused = app.ui_probe().focus().expect("second dialog panel focused");

    app.press_mod(KeyCode::Char('o'), KeyModifiers::CONTROL);

    assert_eq!(app.ui_probe().focus(), Some(focused));
    assert!(app
        .active_docked_dialog()
        .and_then(|dialog| app.ui_probe().docked_surface(dialog))
        .is_some_and(|dialog| dialog.expanded()));
}

#[test]
fn replaced_and_dismissed_notifications_free_their_buffers() {
    let mut app = TestApp::builder().build();
    let shown_buf = |app: &TestApp| {
        let win = app.notification_win().expect("notification shown");
        app.ui_probe().win(win).expect("notification window").buf
    };

    app.notify_error("first".into());
    let first = shown_buf(&app);
    app.notify_error("second".into());
    let second = shown_buf(&app);
    assert!(app.ui_probe().buf(first).is_none());

    app.clock.advance(std::time::Duration::from_millis(
        crate::app::NOTIFICATION_TTL_MS * 2,
    ));
    assert!(app.dismiss_expired_notification());
    assert!(app.ui_probe().buf(second).is_none());
}

#[test]
fn root_dialog_pauses_notification_visibility_and_ttl() {
    let mut app = TestApp::builder().build();
    app.notify_error("deferred notification".into());
    assert!(app.notification_win().is_some());

    open_root_test_dialog(&mut app);

    assert!(app.notification_win().is_none());
    assert!(app.overlays_probe().suspended_notification().is_some());
    app.clock.advance(std::time::Duration::from_millis(
        crate::app::NOTIFICATION_TTL_MS * 2,
    ));
    assert!(!app.dismiss_expired_notification());

    assert!(app.run_lua("smelt.dialog.current().close()"));

    assert!(app.overlays_probe().suspended_notification().is_none());
    assert!(app.notification_win().is_some());
}

#[test]
fn notification_tracks_prompt_layout_after_dialog_close() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    app.render_silent();
    open_root_test_dialog(&mut app);
    app.render_silent();
    app.run_lua_result(
        r#"smelt.notify.info("Redeemed reset credit. Reset 1 usage window.", "usage")"#,
    )
    .unwrap();
    app.press(KeyCode::Esc);

    // Switch to the minimal layout and back without replacing the notification.
    for with_aux in [true, false, true] {
        app.run_lua_result(if with_aux {
            "smelt.ui.layout.set(require('smelt.layout').compose)"
        } else {
            "smelt.ui.layout.set(nil)"
        })
        .unwrap();
        let frame = app.render_to_frame();
        let aux = app.ui_probe().named_win("smelt.prompt.aux").unwrap();
        let aux_rect = app.ui_probe().split_rect(aux);
        assert_eq!(aux_rect.is_some(), with_aux);
        let row = if let Some(rect) = aux_rect {
            rect.y
        } else {
            app.ui_probe().split_rect(crate::app::PROMPT_WIN).unwrap().y - 1
        };
        assert!(
            frame.rows[usize::from(row)]
                .contains("info  Redeemed reset credit. Reset 1 usage window."),
            "{}",
            frame.text()
        );
    }
}

#[test]
fn root_dialog_preserves_notification_scope() {
    let mut app = TestApp::builder().build();
    let session_id = app.session_snapshot().id.clone();
    app.notify_session_save_failure(&session_id, "database busy");

    open_root_test_dialog(&mut app);

    assert!(app
        .overlays_probe()
        .suspended_notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Operation(
                    crate::app::NotificationOperation::SessionPersistence(owner_session_id)
                ) if owner_session_id == &session_id
            )
        }));

    assert!(app.run_lua("smelt.dialog.current().close()"));

    assert!(app
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Operation(
                    crate::app::NotificationOperation::SessionPersistence(owner_session_id)
                ) if owner_session_id == &session_id
            )
        }));
}

#[test]
fn reset_dismisses_suspended_session_notification_and_keeps_audit_log() {
    let mut app = TestApp::builder().build();
    app.notify_session_error_sticky("stale session failure".into());
    open_root_test_dialog(&mut app);
    assert!(app.overlays_probe().suspended_notification().is_some());

    app.reset_session();

    assert!(app.overlays_probe().suspended_notification().is_none());
    assert!(app.overlays_probe().notification().is_none());
    assert!(app.lua_messages_contain("stale session failure"));
}

#[test]
fn reset_preserves_suspended_application_notification() {
    let mut app = TestApp::builder().build();
    app.notify_application_error_sticky("application failure".into());
    open_root_test_dialog(&mut app);
    assert!(app
        .overlays_probe()
        .suspended_notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Application
            )
        }));

    app.reset_session();

    assert!(app
        .overlays_probe()
        .suspended_notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Application
            ) && notification.summary == "application failure"
        }));
}

#[test]
fn readonly_dialog_arrows_move_without_vim() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha", "bravo", "charlie" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.ui_probe().focus().expect("dialog leaf focused");
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 0);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 0);

    app.press(KeyCode::Down);
    app.press(KeyCode::Right);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 1);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 1);

    app.press(KeyCode::Up);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 0);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 1);

    app.press(KeyCode::Left);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 0);
}

#[test]
fn vim_readonly_dialog_arrows_and_vim_motions_move_cursor() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha one", "bravo two", "charlie three", "delta four", "echo five", "foxtrot six" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.ui_probe().focus().expect("dialog leaf focused");

    app.press(KeyCode::Down);
    app.press(KeyCode::Right);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 1);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 1);

    app.type_char('j');
    app.type_char('l');
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 2);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 2);

    app.press(KeyCode::Up);
    app.press(KeyCode::Left);
    app.type_char('k');
    app.type_char('h');
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 0);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 0);

    app.type_char('w');
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 6);
    app.type_char('j');
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 1);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 6);
}

#[test]
fn vim_readonly_dialog_visual_modes_copy_character_and_whole_buffer() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma", "delta" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.ui_probe().focus().expect("dialog leaf focused");

    app.type_char('v');
    app.type_char('e');
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::Visual
    );
    app.type_char('y');
    assert_eq!(
        app.core_probe().clipboard.kill_ring.current().text(),
        "alpha"
    );
    assert_eq!(
        app.core_probe().clipboard.kill_ring.last_clipboard_write(),
        Some("alpha")
    );
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::Normal
    );

    app.type_char('g');
    app.type_char('g');
    app.type_char('V');
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::VisualLine
    );
    app.type_char('G');
    app.type_char('y');

    assert_eq!(
        app.core_probe().clipboard.kill_ring.current().text(),
        "alpha beta\ngamma\ndelta"
    );
    assert_eq!(
        app.core_probe().clipboard.kill_ring.last_clipboard_write(),
        Some("alpha beta\ngamma\ndelta")
    );
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::Normal
    );
    assert!(
        app.ui_probe().active_modal().is_some(),
        "copying must keep the dialog open"
    );
}

#[test]
fn vim_session_dialog_visual_yank_beats_copy_all_keymap() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(r#"smelt.cmd.run("session")"#));
    app.render_silent();

    let win_id = app.tui.ui.focus().expect("session dialog focused");
    app.type_char('v');
    app.type_char('e');
    assert_eq!(
        app.tui.ui.win(win_id).expect("session window").vim_mode(),
        VimMode::Visual
    );

    app.type_char('y');

    assert_eq!(app.tui.core.clipboard.kill_ring.current().text(), "session");
    assert_eq!(
        app.tui.ui.win(win_id).expect("session window").vim_mode(),
        VimMode::Normal
    );
    assert!(
        app.tui.ui.active_modal().is_some(),
        "visual yank must not close the session dialog"
    );

    app.type_char('y');
    assert!(
        app.tui
            .ui
            .win(win_id)
            .expect("session window")
            .vim_state()
            .is_idle(),
        "normal-mode y must retain the dialog's copy-all shortcut"
    );
}

#[test]
fn vim_visual_viewer_owned_keys_preempt_dialog_keymaps() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
          keymaps = {
            { key = "l", on_press = function(ctx) ctx.close() end },
            { key = "q", on_press = function(ctx) ctx.close() end },
          },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.tui.ui.focus().expect("dialog leaf focused");
    app.type_char('v');
    app.type_char('l');

    assert!(
        app.tui.ui.active_modal().is_some(),
        "a Vim-owned Visual key must not reach the dialog keymap"
    );
    assert_eq!(app.tui.ui.win(win_id).expect("window").cursor_col(), 1);
    assert_eq!(
        app.tui.ui.win(win_id).expect("window").vim_mode(),
        VimMode::Visual
    );

    app.type_char('q');
    assert!(
        app.tui.ui.active_modal().is_none(),
        "a Visual key Vim passes through must reach the dialog keymap"
    );
}

#[test]
fn vim_visual_global_keymap_overrides_viewer_default() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        smelt.keymap.set("v", "l", function()
          smelt.dialog.current().close()
        end)
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    app.type_char('v');
    app.type_char('l');

    assert!(
        app.tui.ui.active_modal().is_none(),
        "an explicit global Visual mapping must override the viewer default"
    );
}

#[test]
fn vim_pending_viewer_sequence_preempts_dialog_keymap() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        smelt.keymap.set("n", "q", function()
          smelt.dialog.current().close()
        end)
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
          keymaps = {
            { key = "q", on_press = function(ctx) ctx.close() end },
          },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.tui.ui.focus().expect("dialog leaf focused");
    app.type_char('z');
    assert!(
        !app.tui
            .ui
            .win(win_id)
            .expect("window")
            .vim_state()
            .is_idle(),
        "z should start a pending Vim sequence"
    );

    app.type_char('q');

    assert!(
        app.tui.ui.active_modal().is_some(),
        "pending Vim input must not reach the dialog keymap"
    );
    assert!(
        app.tui
            .ui
            .win(win_id)
            .expect("window")
            .vim_state()
            .is_idle(),
        "invalid pending input should cancel the Vim sequence"
    );
}

#[test]
fn vim_readonly_dialog_escape_exits_visual_before_dialog_keymap() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
          keymaps = {
            { key = "<Esc>", on_press = function(ctx) ctx.close() end },
          },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.ui_probe().focus().expect("dialog leaf focused");
    app.type_char('v');
    app.type_char('e');
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::Visual
    );
    assert!(
        app.ui_probe()
            .win(win_id)
            .and_then(|win| app
                .ui_probe()
                .buf(win.buf)
                .and_then(|buf| win.selection_range(buf)))
            .is_some(),
        "visual-mode dialog viewer should have a selection range"
    );

    app.press(KeyCode::Esc);

    assert!(
        app.ui_probe().active_modal().is_some(),
        "Esc from Visual should not close the dialog"
    );
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::Normal
    );
}

#[test]
fn vim_readonly_dialog_visual_selection_is_painted() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.ui_probe().focus().expect("dialog leaf focused");
    app.type_char('v');
    app.type_char('e');
    assert!(
        !app.ui_probe()
            .win(win_id)
            .expect("window")
            .has_materialized_rows(),
        "dialog viewer should stay byte-backed after visual motions"
    );
    let frame = app.render_to_frame();
    let win = app.ui_probe().win(win_id).expect("window");
    let rect = win.viewport.expect("dialog viewport").rect;
    let selection_bg = app.ui_probe().theme().get("Visual").bg;
    let painted = (rect.y..rect.y + rect.height).any(|row| {
        (rect.x..rect.x + rect.width)
            .any(|col| frame.styles[row as usize][col as usize].bg == selection_bg)
    });

    assert!(painted, "visual selection should be painted inside dialog");
}
fn tool_result<'a>(app: &'a TestApp, call_id: &str) -> Option<(&'a str, bool)> {
    app.actions().iter().rev().find_map(|action| match action {
        Action::EngineSend(cmd) => match cmd.as_ref() {
            protocol::UiCommand::ToolResult {
                call_id: id,
                content,
                is_error,
                ..
            } if id == call_id => Some((content.as_str(), *is_error)),
            _ => None,
        },
        _ => None,
    })
}

fn confirm_req(request_id: u64) -> smelt_core::ConfirmRequest {
    smelt_core::ConfirmRequest {
        invocation_id: protocol::InvocationId::new(request_id),
        call_id: format!("call-{request_id}"),
        tool_name: "test_tool".into(),
        provider: None,
        args: std::collections::HashMap::new(),
        summary: protocol::StyledLines::from_plain("test tool"),
        request_id,
        access: protocol::Access::Write,
    }
}

/// Register `test_tool`, a `write` tool, and a `deny` mode that denies
/// everything above `read`. `write` allows the tool and `read` asks.
fn install_confirm_test_permissions(app: &mut TestApp) {
    app.run_lua_result(
        r#"
        smelt.tools.register({
            name = "test_tool",
            description = "permission probe",
            parameters = { type = "object", properties = {} },
            paths = function() return {} end,
            execute = function() return "ok" end,
        })
        smelt.mode.register({ name = "deny", access = "read", above = "deny" })
        "#,
    )
    .expect("install confirm test policy");
}

fn permission_decisions(cmds: Vec<protocol::UiCommand>) -> Vec<(u64, bool)> {
    cmds.into_iter()
        .filter_map(|cmd| match cmd {
            protocol::UiCommand::PermissionDecision {
                request_id,
                approved,
                ..
            } => Some((request_id, approved)),
            _ => None,
        })
        .collect()
}

fn dispatch_confirm_request(
    app: &mut TestApp,
    req: smelt_core::ConfirmRequest,
    pending: &mut Vec<crate::app::PendingTool>,
) -> crate::app::SessionControl {
    app.dispatch_confirm_request(req, pending)
}

fn actions_permission_decisions(actions: &[Action]) -> Vec<(u64, bool)> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::PermissionDecision {
                    request_id,
                    approved,
                    ..
                } => Some((*request_id, *approved)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn bash_permission_args(command: &str) -> std::collections::HashMap<String, serde_json::Value> {
    std::collections::HashMap::from([("command".into(), serde_json::json!(command))])
}

fn request_bash_permission(
    app: &mut TestApp,
    request_id: u64,
    command: &str,
    access: protocol::Access,
) {
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(request_id),
            request_id,
            call_id: format!("call-{request_id}"),
            tool_name: "bash".into(),
            provider: None,
            args: bash_permission_args(command),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain(command.to_string()),
            access,
        },
    ));
}

#[test]
fn native_tool_permission_shows_its_provider_and_matches_rules_by_raw_name() {
    let mut app = TestApp::builder().build();
    app.start_submitted_turn("search");
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(7),
            request_id: 7,
            call_id: "call-7".into(),
            tool_name: "ToolSearch".into(),
            provider: Some("claude-code".into()),
            args: std::collections::HashMap::from([("query".into(), serde_json::json!("x"))]),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain("x"),
            access: protocol::Access::Write,
        },
    ));
    let frame = app.render_to_frame().text();
    assert!(
        frame.contains("claude-code/ToolSearch"),
        "dialog and block show the provider:\n{frame}"
    );
    let handle_id = app.first_pending_confirm().expect("permission dialog");
    let confirm = app.core_probe().confirms.get(handle_id).expect("confirm");
    assert_eq!(confirm.req.tool_name, "ToolSearch");
}

#[test]
fn native_monitor_block_keeps_its_name_and_shows_the_bash_summary() {
    let mut app = TestApp::builder().build();
    app.start_submitted_turn("watch");
    app.feed_one(SourceEvent::engine(protocol::EngineEvent::ToolStarted {
        invocation_id: protocol::InvocationId::new(8),
        call_id: "call-8".into(),
        tool_name: "claude-code/Monitor".into(),
        args: std::collections::HashMap::from([
            (
                "command".into(),
                serde_json::json!("while true; do\n  sleep 120\ndone"),
            ),
            ("description".into(), serde_json::json!("watch runs")),
            ("timeout_ms".into(), serde_json::json!(1_800_000)),
        ]),
        called_at_ms: 0,
    }));
    let frame = app.render_to_frame().text();
    assert!(
        frame.contains("claude-code/Monitor while true; do  background"),
        "{frame}"
    );
    assert!(frame.contains("  sleep 120"), "{frame}");
    assert!(!frame.contains(r#""command""#), "{frame}");
}

#[test]
fn request_permission_auto_allows_when_applied_turn_mode_allows() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.set_applied_mode(protocol::AgentMode::parse("write").unwrap());

    let mut pending = Vec::new();
    let ctrl = dispatch_confirm_request(&mut app, confirm_req(10), &mut pending);

    assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(10, true)]
    );
}

#[test]
fn request_permission_auto_denies_when_applied_turn_mode_denies() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.set_applied_mode(protocol::AgentMode::parse("deny").unwrap());

    let mut pending = vec![crate::app::PendingTool {
        invocation_id: protocol::InvocationId::new(11),
        name: "test_tool".into(),
    }];
    let ctrl = dispatch_confirm_request(&mut app, confirm_req(11), &mut pending);

    assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    assert_eq!(app.pending_confirm_count(), 0);
    assert!(pending.is_empty());
    assert!(app.agent_running());
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(11, false)]
    );
}

#[test]
fn canonical_history_rebuild_waits_for_pending_permission_tool() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_submitted_turn("prompt");
    let turn_id = app.current_turn_id().expect("submitted turn is active");

    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(12),
            request_id: 12,
            call_id: "call-12".into(),
            tool_name: "test_tool".into(),
            provider: None,
            args: std::collections::HashMap::new(),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain("test tool"),
            access: protocol::Access::Write,
        },
    ));
    assert_eq!(
        app.pending_tool_invocation_ids(),
        vec![protocol::InvocationId::new(12)]
    );

    app.feed_one(SourceEvent::engine(protocol::EngineEvent::HistoryUpdated {
        turn_id,
        update: protocol::CanonicalHistoryDelta::new(0, Vec::new()),
    }));

    assert_eq!(
        app.tui
            .conversation
            .pending_transcript_history_rebuild_from(),
        Some(0)
    );
    app.assert_invariants();

    app.resolve_first_confirm(false, Some("skip".into()));
    assert!(app.pending_tool_invocation_ids().is_empty());
    app.feed_one(SourceEvent::engine(protocol::EngineEvent::HistoryUpdated {
        turn_id,
        update: protocol::CanonicalHistoryDelta::new(0, Vec::new()),
    }));

    assert_eq!(
        app.tui
            .conversation
            .pending_transcript_history_rebuild_from(),
        None
    );
    assert_eq!(app.transcript_block_count(), 0);
    app.assert_invariants();
}

#[test]
fn session_approval_survives_cancel_and_rewind() {
    let mut app = TestApp::builder().build();
    let command = "cargo test";
    app.start_submitted_turn("run tests");
    let history_idx = app.session_message_count().saturating_sub(1);

    request_bash_permission(&mut app, 21, command, protocol::Access::Write);
    assert_eq!(app.pending_confirm_count(), 1);
    let before = app.actions().len();
    assert!(app.resolve_first_confirm_for_session());
    assert_eq!(
        actions_permission_decisions(app.actions_since(before)),
        vec![(21, true)]
    );

    app.cancel();
    app.rewind_to_history_index(Some(history_idx), false);

    let before = app.actions().len();
    app.start_submitted_turn("run tests");
    request_bash_permission(&mut app, 22, command, protocol::Access::Write);
    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(
        actions_permission_decisions(app.actions_since(before)),
        vec![(22, true)]
    );

    request_bash_permission(&mut app, 23, command, protocol::Access::Full);
    assert_eq!(
        app.pending_confirm_count(),
        1,
        "a write approval does not cover full access"
    );
}

fn evaluate_tool(
    app: &mut TestApp,
    request_id: u64,
    call_id: &str,
    tool_name: &str,
    args: std::collections::HashMap<String, serde_json::Value>,
    mode: protocol::AgentMode,
) -> protocol::ToolEvaluation {
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::ToolEvaluationRequest {
            invocation_id: protocol::InvocationId::new(request_id),
            request_id,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            args,
            mode,
        },
    ));
    app.wait_for_tool_evaluation(request_id)
}

#[test]
fn tool_evaluation_uses_the_mode_carried_by_the_turn_event() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.set_configured_agent_mode_for_harness(protocol::AgentMode::parse("deny").unwrap());
    let _ = app.drain_engine_sends();

    let decision = evaluate_tool(
        &mut app,
        91,
        "call-91",
        "test_tool",
        std::collections::HashMap::new(),
        protocol::AgentMode::parse("write").unwrap(),
    );
    assert_eq!(decision.decision, protocol::Decision::Allow);
}

#[test]
fn switch_cwd_outside_the_sandbox_needs_full_access() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    let _ = app.drain_engine_sends();
    let write = protocol::AgentMode::parse("write").unwrap();
    let path_args = |path: &str| {
        std::collections::HashMap::from([(
            "path".to_string(),
            serde_json::Value::String(path.to_string()),
        )])
    };

    let inside = evaluate_tool(
        &mut app,
        90,
        "switch-inside",
        "switch_cwd",
        path_args("."),
        write.clone(),
    );
    assert_eq!(inside.decision, protocol::Decision::Allow);
    assert_eq!(inside.metadata.access, protocol::Access::Write);

    let outside = evaluate_tool(
        &mut app,
        91,
        "switch-outside",
        "switch_cwd",
        path_args("/"),
        write,
    );
    assert_eq!(outside.decision, protocol::Decision::Ask);
    assert_eq!(outside.metadata.access, protocol::Access::Full);
}

#[test]
fn lua_tool_paths_set_the_access_the_dialog_rechecks() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.run_lua_result(
        r#"
        path_callback_session_id = nil
        smelt.tools.register({
            name = "scoped_path_probe",
            description = "permission path probe",
            parameters = { type = "object", properties = {} },
            paths = function()
                path_callback_session_id = smelt.session.id()
                return { "/" }
            end,
            execute = function() return "ok" end,
        })
        "#,
    )
    .expect("register path-aware tool");
    let _ = app.drain_engine_sends();

    let evaluation = evaluate_tool(
        &mut app,
        91,
        "call-91",
        "scoped_path_probe",
        std::collections::HashMap::new(),
        protocol::AgentMode::parse("write").unwrap(),
    );
    assert_eq!(evaluation.decision, protocol::Decision::Ask);
    assert_eq!(evaluation.metadata.access, protocol::Access::Full);
    assert_eq!(
        app.eval_lua::<String>("return path_callback_session_id")
            .expect("read callback session id"),
        app.session_snapshot().id
    );

    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(91),
            request_id: 92,
            call_id: "call-91".into(),
            tool_name: "scoped_path_probe".into(),
            provider: None,
            args: std::collections::HashMap::new(),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain("path probe"),
            access: evaluation.metadata.access,
        },
    ));
    let handle_id = app
        .first_pending_confirm()
        .expect("permission dialog opens");
    assert!(!app.resolve_open_confirm_for_current_mode(handle_id));
}

#[test]
fn approving_a_path_for_the_session_covers_later_calls_beneath_it() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.run_lua_result(
        r#"
        smelt.tools.register({
            name = "path_writer",
            description = "writes a path",
            access = "write",
            parameters = { type = "object", properties = { path = { type = "string" } } },
            paths = function(args) return { args.path } end,
            execute = function() return "ok" end,
        })
        "#,
    )
    .expect("register path tool");
    let _ = app.drain_engine_sends();
    // Outside the temp directories, which are always "allow".
    let outside = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target"))
        .expect("create directory outside temp");
    let args = |name: &str| {
        std::collections::HashMap::from([(
            "path".to_string(),
            serde_json::json!(outside.path().join(name).to_string_lossy()),
        )])
    };
    let write = protocol::AgentMode::parse("write").unwrap();

    let evaluation = evaluate_tool(
        &mut app,
        41,
        "call-41",
        "path_writer",
        args("a.txt"),
        write.clone(),
    );
    assert_eq!(evaluation.decision, protocol::Decision::Ask);
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(41),
            request_id: 42,
            call_id: "call-41".into(),
            tool_name: "path_writer".into(),
            provider: None,
            args: args("a.txt"),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain("a.txt"),
            access: evaluation.metadata.access,
        },
    ));
    let options: String = app
        .eval_lua(
            r#"local req = smelt.signal.get("confirm_requested")
            return req.scope .. "|" .. tostring(req.intent)"#,
        )
        .expect("read confirm request");
    assert!(options.ends_with("/|write"), "{options}");
    assert!(app.resolve_first_confirm_for_session_path());

    let evaluation = evaluate_tool(
        &mut app,
        43,
        "call-43",
        "path_writer",
        args("b.txt"),
        write.clone(),
    );
    assert_eq!(evaluation.decision, protocol::Decision::Allow);
    let elsewhere = std::collections::HashMap::from([(
        "path".to_string(),
        serde_json::json!("/outside-every-approval/c.txt"),
    )]);
    let evaluation = evaluate_tool(&mut app, 44, "call-44", "path_writer", elsewhere, write);
    assert_eq!(evaluation.decision, protocol::Decision::Ask);
}

#[test]
fn tool_path_callback_failure_fails_closed() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.run_lua_result(
        r#"
        smelt.tools.register({
            name = "broken_path_callback",
            description = "broken path callback",
            parameters = { type = "object", properties = {} },
            paths = function() error("path callback exploded") end,
            execute = function() return "should not run" end,
        })
        "#,
    )
    .expect("register tool with failing path callback");
    app.approve_tool_for_session("broken_path_callback");
    let _ = app.drain_engine_sends();

    let evaluation = evaluate_tool(
        &mut app,
        95,
        "broken-path-callback",
        "broken_path_callback",
        std::collections::HashMap::new(),
        protocol::AgentMode::parse("yolo").unwrap(),
    );
    assert!(
        matches!(
            evaluation.decision,
            protocol::Decision::Error(ref error) if error.contains("path callback exploded")
        ),
        "unexpected evaluation decision: {:?}",
        evaluation.decision
    );
}

#[test]
fn open_confirm_recheck_keeps_dialog_when_mode_still_asks() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(12), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    let handle_id = app.first_pending_confirm().unwrap();
    assert!(!app.resolve_open_confirm_for_current_mode(handle_id));
    assert_eq!(app.pending_confirm_count(), 1);
    assert!(permission_decisions(app.drain_engine_sends()).is_empty());
}

#[test]
fn open_confirm_needs_attention_without_statusline_pending() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(13), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    assert_eq!(app.pending_confirm_count(), 1);
    assert!(app.state().active_modal.is_some());

    assert_eq!(
        app.public_status_state_reason(),
        (
            smelt_core::public_status::PublicState::NeedsAttention,
            Some(smelt_core::public_status::PublicReason::Permission)
        )
    );
    let text = app.render_to_frame().text();
    let statusline = text.lines().last().expect("rendered statusline");
    assert!(!statusline.contains("permission pending"), "{statusline:?}");
}

#[test]
fn deferred_confirm_dialog_shows_permission_pending_in_statusline() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.type_text("draft response");

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(131), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(app.pending_deferred_dialog_count(), 1);
    assert!(app.state().active_modal.is_none());
    let frame = app.render_to_frame();
    let text = frame.text();
    let statusline = text.lines().last().expect("rendered statusline");
    assert!(statusline.contains("permission pending"), "{statusline:?}");
}

#[test]
fn confirm_down_moves_selection_without_scrolling_options() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(14), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }
    app.render_silent();

    let options = app.ui_probe().focus().expect("confirm options focused");
    let before = app.ui_probe().win(options).expect("confirm options window");
    assert_eq!(before.cursor_abs_row(), 0);
    assert_eq!(before.scroll_top(), 0);
    assert!(
        before
            .viewport
            .expect("confirm options viewport")
            .rect
            .height
            >= 2,
        "both confirm options should be visible"
    );

    app.press(KeyCode::Down);

    let after = app.ui_probe().win(options).expect("confirm options window");
    assert_eq!(after.cursor_abs_row(), 1, "Down should select deny");
    assert_eq!(
        after.scroll_top(),
        0,
        "the options viewport should stay fixed while deny is visible"
    );
}

#[test]
fn confirm_top_border_drag_resizes_dialog() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(15), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }
    app.render_silent();

    let dialog = app.active_docked_dialog().expect("confirm dialog");
    let first_leaf = app
        .ui_probe()
        .modal_leaves(
            app.ui_probe()
                .docked_surface(dialog)
                .expect("docked surface")
                .modal(),
        )
        .and_then(|leaves| leaves.first())
        .copied()
        .expect("confirm dialog leaf");
    let before_top = app
        .ui_probe()
        .split_rect(first_leaf)
        .expect("confirm dialog leaf rect")
        .y
        .saturating_sub(1);
    let target_top = before_top.saturating_sub(3);

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: if matches!(kind, MouseEventKind::Down(_)) {
                before_top
            } else {
                target_top
            },
            column: 10,
            modifiers: KeyModifiers::empty(),
        })));
    }
    app.render_silent();

    let after_top = app
        .ui_probe()
        .split_rect(first_leaf)
        .expect("resized confirm dialog leaf rect")
        .y
        .saturating_sub(1);
    assert_eq!(after_top, target_top);
}

#[test]
fn custom_composer_dialog_chrome_resizes_and_reflows_with_terminal() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function(state)
          if not state.dialog then
            return smelt.ui.layout.leaf(smelt.prompt.win)
          end
          return smelt.ui.layout.vbox({
            { state.dialog, height = "fill" },
          })
        end)
        "#
    ));
    app.render_silent();

    let dialog = app.active_docked_dialog().expect("active dialog");
    let before = app
        .ui_probe()
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("custom-composed dialog rect");
    let target_top = before.y.saturating_sub(2);

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: if matches!(kind, MouseEventKind::Down(_)) {
                before.y
            } else {
                target_top
            },
            column: 10,
            modifiers: KeyModifiers::empty(),
        })));
    }
    app.render_silent();

    let resized = app
        .ui_probe()
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("resized custom-composed dialog rect");
    assert_eq!(resized.y, target_top);
    assert_eq!(resized.height, before.height + 2);

    app.set_terminal_size(80, 3);
    app.render_silent();
    let constrained = app
        .ui_probe()
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("constrained dialog rect");
    assert!(constrained.height < resized.height);
    assert!(constrained.bottom() <= 3);

    app.set_terminal_size(80, 24);
    app.render_silent();
    let restored = app
        .ui_probe()
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("restored dialog rect");
    assert_eq!(restored.height, resized.height);
}

#[test]
fn shift_tab_on_open_confirm_waits_for_turn_mode_boundary_before_allowing() {
    let mut app = TestApp::builder()
        .with_mode_cycle(vec![
            protocol::AgentMode::read(),
            protocol::AgentMode::parse("write").unwrap(),
        ])
        .build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(13), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }
    assert_eq!(app.pending_confirm_count(), 1);
    app.clear_actions();

    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);

    assert_eq!(app.pending_confirm_count(), 1);
    assert_eq!(app.core_probe().config.mode.as_str(), "write");
    assert!(app.mode_pending());
    assert!(actions_permission_decisions(app.actions()).is_empty());

    app.sync_agent_mode_applied();
    let handle_id = app.first_pending_confirm().unwrap();
    assert!(app.resolve_open_confirm_for_current_mode(handle_id));
    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(13, true)]
    );
}

#[test]
fn open_confirm_recheck_auto_denies_without_stopping_turn() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(14), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    let handle_id = app.first_pending_confirm().unwrap();
    app.set_applied_mode(protocol::AgentMode::parse("deny").unwrap());

    assert!(app.resolve_open_confirm_for_current_mode(handle_id));
    assert_eq!(app.pending_confirm_count(), 0);
    assert!(app.agent_running());
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(14, false)]
    );
}

#[test]
fn session_approval_resolves_covered_parallel_calls_but_keeps_higher_access_prompt() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    let mut pending = Vec::new();
    let mut higher_access = confirm_req(15);
    higher_access.access = protocol::Access::Full;
    for req in [higher_access, confirm_req(16), confirm_req(17)] {
        dispatch_confirm_request(&mut app, req, &mut pending);
    }
    assert_eq!(app.pending_confirm_count(), 3);

    app.render_silent();
    app.press(KeyCode::Down);
    app.press(KeyCode::Down);
    app.press(KeyCode::Enter); // Allow this tool for the session at write access.
    assert_eq!(app.pending_confirm_count(), 1);
    let mut decisions = permission_decisions(app.drain_engine_sends());
    decisions.sort_unstable();
    assert_eq!(decisions, vec![(16, true), (17, true)]);

    app.press(KeyCode::Enter); // Allow the remaining full-access call once.
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(15, true)]
    );
    assert_eq!(app.pending_confirm_count(), 0);
    assert!(app.state().active_modal.is_none());
}

#[test]
fn present_plan_save_draft_writes_artifact_and_manifest() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert("title".into(), serde_json::json!("Parser plan"));
    args.insert("slug".into(), serde_json::json!("parser-plan"));
    args.insert(
        "plan".into(),
        serde_json::json!("# Goal\nShip the parser change.\n"),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(91),
        request_id: 91,
        call_id: "plan-draft".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    assert!(app.run_lua(r#"smelt.dialog.current().resolve("draft")"#));
    drive_lua_tasks(&mut app);

    let (content, is_error) = tool_result(&app, "plan-draft").expect("present_plan result");
    assert!(!is_error, "{content}");
    let plan_path = content
        .lines()
        .find_map(|line| line.strip_prefix("wrote plan to "))
        .expect("result includes plan path");
    assert!(plan_path.ends_with("/plan.md"));
    assert_eq!(
        std::fs::read_to_string(plan_path).unwrap(),
        "# Goal\nShip the parser change.\n"
    );

    let artifact_dir = std::path::Path::new(plan_path)
        .parent()
        .unwrap()
        .to_path_buf();
    let plan_path = plan_path.to_owned();
    let plan_edit: String = app
        .eval_lua(&format!(
            r#"return smelt.permissions.path_access({}, "write")"#,
            serde_json::to_string(&plan_path).unwrap()
        ))
        .expect("plan path access");
    assert_eq!(plan_edit, "read", "plan mode may edit its own plan");

    let manifest_path = artifact_dir.join("manifest.json");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["kind"], "smelt.plan");
    assert_eq!(manifest["status"], "draft");
    assert_eq!(manifest["title"], "Parser plan");
    assert_eq!(manifest["slug"], "parser-plan");
}

#[test]
fn present_plan_existing_path_approves_without_overwriting_plan_body() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let session_dir = app
        .core_probe()
        .sessions
        .artifact_dir_for(&app.session_snapshot());
    let artifact_dir = session_dir.join("plans/20260101-000000-parser-plan");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let plan_path = artifact_dir.join("plan.md");
    std::fs::write(&plan_path, "# Revised\nUse the existing draft.\n").unwrap();
    std::fs::write(
        artifact_dir.join("manifest.json"),
        serde_json::json!({
            "version": 1,
            "kind": "smelt.plan",
            "title": "Parser plan",
            "slug": "parser-plan",
            "status": "draft",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "plan_path": "plan.md"
        })
        .to_string(),
    )
    .unwrap();

    let mut args = std::collections::HashMap::new();
    args.insert(
        "plan_path".into(),
        serde_json::json!(plan_path.to_string_lossy().to_string()),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(92),
        request_id: 92,
        call_id: "plan-approve".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    app.press(KeyCode::Char('2'));
    drive_lua_tasks(&mut app);

    let (content, is_error) = tool_result(&app, "plan-approve").expect("present_plan result");
    assert!(!is_error, "{content}");
    assert!(content.contains("wrote plan to "));
    assert_eq!(
        std::fs::read_to_string(&plan_path).unwrap(),
        "# Revised\nUse the existing draft.\n"
    );

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(artifact_dir.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["status"], "approved");
    assert_eq!(manifest["created_at"], "2026-01-01T00:00:00Z");
}

#[test]
fn present_plan_dismiss_does_not_echo_plan_body() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert("title".into(), serde_json::json!("Parser plan"));
    args.insert("slug".into(), serde_json::json!("parser-plan"));
    args.insert(
        "plan".into(),
        serde_json::json!("# Secret draft\nDo not keep this transcript copy.\n"),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(94),
        request_id: 94,
        call_id: "plan-dismiss".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    app.press(KeyCode::Esc);
    drive_lua_tasks(&mut app);

    let (content, is_error) = tool_result(&app, "plan-dismiss").expect("present_plan result");
    assert!(is_error);
    assert!(content.contains("plan dismissed"));
    assert!(!content.contains("Secret draft"));
    assert!(!content.contains("Do not keep this transcript copy"));
}

#[test]
fn present_plan_dialog_tracks_terminal_width_on_resize() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert("title".into(), serde_json::json!("Parser plan"));
    args.insert("slug".into(), serde_json::json!("parser-plan"));
    args.insert(
        "plan".into(),
        serde_json::json!("# Goal\nShip the parser change.\n"),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(93),
        request_id: 93,
        call_id: "plan-resize".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    let before = app.render_to_frame();
    assert!(before
        .text()
        .lines()
        .any(|line| line.starts_with("─ plan ")));

    app.set_terminal_size(100, 24);
    let after = app.render_to_frame();
    let text = after.text();
    let title_row = text
        .lines()
        .find(|line| line.starts_with("─ plan "))
        .expect("dialog title row after resize");
    assert_eq!(title_row.chars().count(), 100);
}

#[test]
fn public_status_open_question_needs_attention() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert(
        "questions".into(),
        serde_json::json!([
            {
                "header": "Choice",
                "question": "Pick one?",
                "options": [
                    { "label": "One", "description": "first option" },
                    { "label": "Two", "description": "second option" }
                ],
                "multiSelect": false
            }
        ]),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(77),
        request_id: 77,
        call_id: "status-question".into(),
        tool_name: "ask_user_question".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    let (state, reason) = app.public_status_state_reason();
    assert_eq!(
        state,
        smelt_core::public_status::PublicState::NeedsAttention
    );
    assert_eq!(
        reason,
        Some(smelt_core::public_status::PublicReason::Question)
    );
}

#[test]
fn ask_user_question_wrapped_options_keep_selection_aligned() {
    fn assert_selection(app: &mut TestApp, index: usize) {
        let frame = app.render_to_frame();
        let prefix = format!(" {index}. ");
        let row = frame
            .rows
            .iter()
            .position(|row| row.contains(&prefix))
            .unwrap_or_else(|| panic!("selected option must be visible: {}", frame.text()));
        let col = frame.rows[row].find(&prefix).unwrap() + prefix.len();
        let theme = app.ui_probe().theme();
        assert_eq!(
            frame.styles[row][col].bg,
            theme.get("CursorLine").bg,
            "selection must highlight the option, not a wrapped description: {}",
            frame.text()
        );
        assert_eq!(frame.styles[row][col].fg, theme.get("SmeltAccent").fg);
        let win = app.ui_probe().win(app.ui_probe().focus().unwrap()).unwrap();
        let buf = app.ui_probe().buf(win.buf).unwrap();
        assert!(buf
            .get_line(win.cursor_row() as usize)
            .unwrap()
            .starts_with(&prefix));
        assert!(
            !win.wrap,
            "menu rows must not be soft-wrapped a second time"
        );
        let next_prefix = format!(" {}. ", index + 1);
        let end = (win.cursor_row() as usize + 1..buf.line_count())
            .find(|&row| buf.get_line(row).unwrap().starts_with(&next_prefix))
            .unwrap_or(buf.line_count());
        let rect = win.viewport.unwrap().rect;
        let visible_rows = (end - win.cursor_row() as usize).min(usize::from(rect.height));
        assert!(
            row + visible_rows <= usize::from(rect.y + rect.height),
            "show as much of the selected description as fits: {}",
            frame.text()
        );
    }

    for vim in [false, true] {
        let mut app = TestApp::builder().with_vim(vim).build();
        app.set_terminal_size(46, 26);
        app.start_turn(1);
        let question =
            "Which authentication method should we use for browser sign-in and automated clients?";
        let labels = [
            "Browser sign-in",
            "API key automation fallback",
            "Anonymous",
        ];
        let mut args = std::collections::HashMap::new();
        args.insert(
            "questions".into(),
            serde_json::json!([{
                "header": "Auth method",
                "question": question,
                "options": [
                    { "label": labels[0], "description": "Redirect to the provider and preserve the original destination when the user returns to the application." },
                    { "label": labels[1], "description": "Use a token for automated clients and support scripts, including 界 and e\u{301} in account names." },
                    { "label": labels[2], "description": "Skip authentication entirely." }
                ],
                "multiSelect": false
            }]),
        );
        app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
            invocation_id: protocol::InvocationId::new(77),
            request_id: 77,
            call_id: "aq-wrapped-options".into(),
            tool_name: "ask_user_question".into(),
            args,
        }));
        assert_selection(&mut app, 1);
        app.press(KeyCode::Down);
        assert_selection(&mut app, 2);
        app.press(KeyCode::Down);
        assert_selection(&mut app, 3);
        app.press(KeyCode::Up);
        assert_selection(&mut app, 2);
        for (width, height) in [(24, 14), (80, 26), (32, 18)] {
            app.set_terminal_size(width, height);
            assert_selection(&mut app, 2);
            app.dispatch_ui_window_events(false);
            assert_selection(&mut app, 2);
        }
        app.press_mod(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_selection(&mut app, 2);
        app.press(KeyCode::Tab);
        app.press(KeyCode::Esc);
        assert_selection(&mut app, 2);
        app.type_char('2');
        drive_lua_tasks(&mut app);
        assert!(app.state().active_modal.is_none());
        let expected = format!("q: {question}\na: {}", labels[1]);
        assert!(
            app.actions().iter().any(|action| matches!(action,
                Action::EngineSend(cmd) if matches!(cmd.as_ref(),
                    protocol::UiCommand::ToolResult { call_id, content, is_error: false, .. }
                        if call_id == "aq-wrapped-options" && content == &expected)
            )),
            "the submitted answer must match the highlighted option"
        );
    }
}

#[test]
fn ask_user_question_custom_input_accepts_question_mark() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);
    app.push_user_block("Earlier transcript text");
    app.focus_transcript();

    let mut args = std::collections::HashMap::new();
    args.insert(
        "questions".into(),
        serde_json::json!([{
            "header": "Choice",
            "question": "Pick one?",
            "options": [
                { "label": "One", "description": "first option" },
                { "label": "Two", "description": "second option" }
            ],
            "multiSelect": false
        }]),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(77),
        request_id: 77,
        call_id: "aq-custom-input".into(),
        tool_name: "ask_user_question".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    app.press(KeyCode::Tab);
    app.settle_lua();
    let input_win = app
        .ui_probe()
        .focus()
        .expect("custom input should be focused");
    let input = app.ui_probe().win(input_win).expect("custom input window");
    assert!(
        !input.surface().is_readonly_text(),
        "Tab should focus the editable custom input"
    );
    let input_buf = input.buf;

    app.type_text("why");
    assert_eq!(
        app.ui_probe()
            .buf(input_buf)
            .expect("custom input buffer")
            .get_line(0),
        Some("why")
    );

    app.press_mod(KeyCode::Char('?'), KeyModifiers::SHIFT);

    assert_eq!(
        app.ui_probe()
            .buf(input_buf)
            .expect("custom input buffer")
            .get_line(0),
        Some("why?")
    );
    assert_eq!(app.ui_probe().focus(), Some(input_win));
    assert!(
        !app.state().cmdline_open,
        "question mark must not open search"
    );
}

#[test]
fn ask_user_question_multiple_questions_wakes_between_dialogs() {
    fn drain_wakeups(app: &mut TestApp) {
        for _ in 0..64 {
            if !app.try_receive_lua_wakeup() {
                return;
            }
            app.feed_one(SourceEvent::LuaWakeup);
        }
        panic!("dialog Lua work did not settle");
    }

    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert(
        "questions".into(),
        serde_json::json!([
            {
                "header": "First",
                "question": "Pick first?",
                "options": [
                    { "label": "One", "description": "first option" },
                    { "label": "Two", "description": "second option" }
                ],
                "multiSelect": false
            },
            {
                "header": "Second",
                "question": "Pick second?",
                "options": [
                    { "label": "Three", "description": "third option" },
                    { "label": "Four", "description": "fourth option" }
                ],
                "multiSelect": false
            }
        ]),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(77),
        request_id: 77,
        call_id: "aq-questions".into(),
        tool_name: "ask_user_question".into(),
        args,
    }));
    drain_wakeups(&mut app);

    let first = app
        .state()
        .active_modal
        .expect("first question dialog should open");

    app.press(KeyCode::Enter);
    assert!(
        app.try_receive_lua_wakeup(),
        "resolving the first dialog should wake the Lua task runtime"
    );
    app.feed_one(SourceEvent::LuaWakeup);
    drain_wakeups(&mut app);

    let second = app
        .state()
        .active_modal
        .expect("second question dialog should open after first answer");
    assert_ne!(first, second);

    app.press(KeyCode::Char('2'));
    assert!(
        app.try_receive_lua_wakeup(),
        "resolving the final dialog should wake the Lua task runtime"
    );
    app.feed_one(SourceEvent::LuaWakeup);
    drain_wakeups(&mut app);

    assert!(app.state().active_modal.is_none());
    let result = app
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::ToolResult {
                    request_id,
                    call_id,
                    content,
                    is_error,
                    ..
                } => Some((*request_id, call_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            },
            _ => None,
        })
        .next_back()
        .expect("ask_user_question should send a tool result");

    assert_eq!(result.0, 77);
    assert_eq!(result.1, "aq-questions");
    assert_eq!(
        result.2,
        "q: Pick first?\na: One\n\nq: Pick second?\na: Four"
    );
    assert!(!result.3);
}

#[test]
fn chained_dialog_opens_before_the_frame_that_follows_the_closing_key() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local function page(title)
          local buf = smelt.buf.new({ readonly = true })
          buf:lines({ title })
          local leaf = smelt.dialog.content({ buf = buf, interactive = true })
          return smelt.dialog.open({
            title = title,
            height = 4,
            panels = { { leaf = leaf, height = "fill" } },
            focus = leaf,
            keymaps = { { key = "<Esc>", on_press = function(ctx) ctx.close() end } },
          })
        end
        smelt.spawn(function()
          page("first")
          page("second")
        end)
        "#
    ));
    app.feed_one(SourceEvent::LuaWakeup);
    let first = app.state().active_modal.expect("first dialog opens");

    app.press(KeyCode::Esc);
    let frame = app.render_to_frame();

    let second = app
        .state()
        .active_modal
        .expect("the next dialog is open in the first frame after the closing key");
    assert_ne!(first, second);
    assert!(frame.text().contains("second"));
}

#[test]
fn colon_opens_cmdline_only_in_vim_normal_mode() {
    for (vim, normal) in [(false, false), (true, false), (true, true)] {
        let mut app = TestApp::builder().with_vim(vim).build();
        if normal {
            app.press(KeyCode::Esc);
        }
        app.type_char(':');
        let s = app.state();
        assert_eq!(s.cmdline_open, normal, "vim={vim} normal={normal}");
        assert_eq!(s.prompt_text, if normal { "" } else { ":" });
        if normal {
            assert_eq!(s.cmdline_text, "");
            app.press(KeyCode::Esc);
            assert!(!app.state().cmdline_open);
        }
    }
}

#[test]
fn cmdline_quit_command_requests_quit() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    app.type_text("quit");
    app.press(KeyCode::Enter);
    assert!(app.state().pending_quit);
}

#[test]
fn cmdline_paste_inserts_single_line_payload() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');

    app.feed_one(SourceEvent::Term(Event::Paste("hello\nworld".into())));

    let s = app.state();
    assert!(s.cmdline_open);
    assert_eq!(s.cmdline_text, "hello world");
}

#[test]
fn cmdline_unicode_editing_uses_byte_safe_cursor() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    app.feed_one(SourceEvent::Term(Event::Paste("a日本b".into())));
    app.press(KeyCode::Left);
    app.press(KeyCode::Backspace);

    let s = app.state();
    assert_eq!(s.cmdline_text, "a日b");
    app.type_text(&"a".repeat(60));
    app.assert_invariants();
}

#[test]
fn btw_command_preserves_model_history_prefix_and_appends_question() {
    let mut app = TestApp::builder().build();
    stub_btw_ui(&mut app);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u2")));

    let expected_prefix = protocol::history_to_messages(&app.model_history());

    {
        app.apply_lua_command("btw what changed?");
        app.drive_lua_tasks();
    }

    let asks = ask_messages(app.drain_engine_sends());
    assert_eq!(asks.len(), 1, "/btw should issue one inherited ask");
    let (system, messages) = &asks[0];
    assert_eq!(system, &app.assemble_system_prompt());
    assert_eq!(
        &messages[..expected_prefix.len()],
        expected_prefix.as_slice(),
        "/btw must preserve the exact model-visible prefix"
    );
    let last_text = messages
        .last()
        .and_then(|m| m.content.as_ref())
        .map(|c| c.text_content())
        .expect("/btw question");
    assert!(last_text.contains("Under no circumstances use tools"));
    assert!(last_text.contains("Question: what changed?"));

    respond_pending_ask_with_text(&mut app, "done");
    app.clear_timers();
}

#[test]
fn btw_dialog_paints_a_selected_delta_before_a_coalesced_final_response() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(70, 22);
    app.push_user_block("how do I render a buffer?");
    app.push_assistant_text("Call `buf:source(text)`.");

    {
        app.apply_lua_command("btw show me a tiny example");
        app.drive_lua_tasks();
    }
    let ask_id = app.pending_ask_id().expect("/btw registered ask callback");
    app.render_to_frame();

    app.inject_engine(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "coalesced live dialog marker".into(),
    })
    .expect("queue /btw delta");
    app.inject_engine(protocol::EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text("coalesced final answer")),
            None,
            None,
        )),
        error: None,
    })
    .expect("queue /btw response");

    let selected_delta = app
        .try_receive_engine_output()
        .expect("select loop should receive the first delta");
    app.dispatch_engine_output_in_render_loop_to(selected_delta, &mut std::io::sink(), |_| {});
    let mut streamed_frame = None;
    app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |frame| {
        streamed_frame = Some(frame.text())
    });

    let streamed_frame = streamed_frame.expect("final response should paint the pending delta");
    assert!(
        streamed_frame.lines().any(|line| line.trim() == "c"),
        "no frame painted the selected delta: {streamed_frame}"
    );
    assert!(
        !streamed_frame.contains("coalesced final answer"),
        "the final callback ran before the streamed frame: {streamed_frame}"
    );

    let final_frame = app.render_to_frame();
    let text = "coalesced final answer";
    let row = final_frame
        .rows
        .iter()
        .position(|row| row.contains(text))
        .unwrap_or_else(|| panic!("final frame: {}", final_frame.text()));
    let col = final_frame.rows[row].find(text).unwrap();
    assert!(
        final_frame.styles[row][col..col + text.len()]
            .iter()
            .all(|style| !style.dim && !style.italic),
        "waiting indicator styles leaked into the answer"
    );
    let final_frame = final_frame.text();
    assert!(
        final_frame.contains("coalesced final answer"),
        "frame: {final_frame}"
    );
    app.clear_timers();
}

#[test]
fn btw_dialog_resets_rejected_draft_and_recovers_without_waiting_styles() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(70, 22);
    app.apply_lua_command("btw explain the retry");
    app.drive_lua_tasks();
    let id = app.pending_ask_id().expect("/btw registered ask callback");
    app.feed_one(SourceEvent::engine(protocol::EngineEvent::EngineAskDelta {
        id,
        delta: "rejected side answer".into(),
    }));
    app.clock.advance(crate::app::stream_reveal::MAX_SPREAD);
    assert!(app
        .render_to_frame()
        .text()
        .contains("rejected side answer"));
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::EngineAskDraftRejected { id },
    ));
    assert!(!app
        .render_to_frame()
        .text()
        .contains("rejected side answer"));
    app.feed_one(SourceEvent::engine(protocol::EngineEvent::EngineAskDelta {
        id,
        delta: "accepted side answer".into(),
    }));
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::EngineAskResponse {
            id,
            message: Some(protocol::Message::assistant(
                Some(protocol::Content::text("accepted side answer")),
                None,
                None,
            )),
            error: None,
        },
    ));
    let frame = app.render_to_frame();
    let row = frame
        .rows
        .iter()
        .position(|row| row.contains("accepted side answer"))
        .expect("recovered side answer");
    let col = frame.rows[row].find("accepted side answer").unwrap();
    assert!(frame.styles[row][col..col + "accepted side answer".len()]
        .iter()
        .all(|style| !style.dim && !style.italic));
    assert!(!frame.text().contains("rejected side answer"));
    app.clear_timers();
    app.assert_invariants();
}

#[test]
fn plain_buffer_source_shows_in_a_dialog() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(70, 22);
    assert!(app.run_lua(
        r#"
        smelt.spawn(function()
          local buf = smelt.buf.new({ readonly = true })
          buf:styled({ { { text = "painted row" } } })
          buf:source("plain source text")
          smelt.dialog.open({
            title = "plain",
            min_height = "30%",
            panels = { { leaf = smelt.dialog.content({ buf = buf }), height = "fill" } },
          })
        end)
        "#
    ));
    app.drive_lua_tasks();
    let frame = app.render_to_frame();
    assert!(
        frame.text().contains("plain source text"),
        "{}",
        frame.text()
    );
    assert!(!frame.text().contains("painted row"), "{}", frame.text());
    app.clear_timers();
    app.assert_invariants();
}

#[test]
fn btw_dialog_shows_a_failed_answer() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(70, 22);
    app.apply_lua_command("btw explain the retry");
    app.drive_lua_tasks();
    let id = app.pending_ask_id().expect("/btw registered ask callback");
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::EngineAskResponse {
            id,
            message: None,
            error: Some(protocol::EngineAskError {
                kind: protocol::EngineAskErrorKind::InvalidResponse,
                message: "Claude Code returned no answer".into(),
            }),
        },
    ));
    let frame = app.render_to_frame();
    assert!(
        frame
            .text()
            .contains("error (invalid_response): Claude Code returned no answer"),
        "{}",
        frame.text()
    );
    app.clear_timers();
    app.assert_invariants();
}

#[test]
fn btw_dialog_streams_before_a_busy_engine_queue_reaches_the_final_response() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(70, 22);
    app.push_user_block("how do I render a buffer?");
    app.push_assistant_text("Call `buf:source(text)`.");

    {
        app.apply_lua_command("btw show me a tiny example");
        app.drive_lua_tasks();
    }
    let ask_id = app.pending_ask_id().expect("/btw registered ask callback");
    app.render_to_frame();

    let queued_deltas = crate::app::READY_QUEUE_DRAIN_MAX_ITEMS_PER_FRAME * 2;
    for index in 0..queued_deltas {
        let delta = if index == 0 {
            "live dialog marker ".to_string()
        } else {
            format!("chunk-{index} ")
        };
        app.inject_engine(protocol::EngineEvent::EngineAskDelta { id: ask_id, delta })
            .expect("queue /btw delta");
    }
    app.inject_engine(protocol::EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text("final answer")),
            None,
            None,
        )),
        error: None,
    })
    .expect("queue /btw response");

    app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |_| {});
    app.clock.advance(crate::app::stream_reveal::MAX_SPREAD);
    let frame = app.render_to_frame().text();
    assert!(frame.contains("live dialog marker"), "frame: {frame}");
    assert!(!frame.contains("final answer"), "frame: {frame}");

    for _ in 0..=queued_deltas {
        if app.pending_ask_id().is_none() {
            break;
        }
        app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |_| {});
    }
    assert!(
        app.pending_ask_id().is_none(),
        "final response was not drained"
    );
    let frame = app.render_to_frame().text();
    assert!(frame.contains("final answer"), "frame: {frame}");
    app.clear_timers();
}

#[test]
fn btw_command_denies_tool_calls_then_retries_same_request_shape() {
    let mut app = TestApp::builder().build();
    stub_btw_ui(&mut app);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    let expected_prefix = protocol::history_to_messages(&app.model_history());

    {
        app.apply_lua_command("btw quick summary");
        app.drive_lua_tasks();
    }

    let first = ask_messages(app.drain_engine_sends());
    assert_eq!(first.len(), 1);
    let first_messages = first[0].1.clone();

    respond_pending_ask_with_tool_call(&mut app, "call-1", "grep");

    let second = ask_messages(app.drain_engine_sends());
    assert_eq!(second.len(), 1);
    let second_messages = &second[0].1;
    assert_eq!(
        &second_messages[..first_messages.len()],
        first_messages.as_slice(),
        "/btw tool denial retry must keep the same request prefix"
    );
    assert_eq!(
        &second_messages[..expected_prefix.len()],
        expected_prefix.as_slice(),
        "/btw tool denial retry must keep the same inherited conversation prefix"
    );
    assert_eq!(
        second_messages[first_messages.len()].role,
        protocol::Role::Assistant
    );
    assert_eq!(
        second_messages[first_messages.len() + 1].role,
        protocol::Role::Tool
    );
    assert!(second_messages[first_messages.len() + 1].is_error);

    respond_pending_ask_with_text(&mut app, "done");
    app.clear_timers();
}

#[test]
fn rejected_dialog_construction_keeps_the_active_dialog() {
    let mut app = TestApp::builder().build();
    app.run_lua_result(
        r#"
        first_leaf = smelt.dialog.content({ text = "first dialog" })
        first = smelt.dialog.new({ panels = { { leaf = first_leaf, height = "fit" } } })
        second_leaf = smelt.dialog.content({ text = "second dialog" })
        "#,
    )
    .unwrap();
    assert!(app.render_to_frame().text().contains("first dialog"));

    let rejected = app.run_lua_result(
        r#"
        smelt.dialog.new({
          panels = { { leaf = second_leaf, height = "fit" } },
          focus = "not a window",
        })
        "#,
    );
    assert!(rejected.is_err(), "construction must raise");

    app.run_lua_result(
        r#"
        assert(smelt.dialog.current() ~= nil, "active dialog lost")
        assert(smelt.dialog.current().win == first.win, "active dialog replaced")
        "#,
    )
    .unwrap();
    let frame = app.render_to_frame();
    assert!(frame.text().contains("first dialog"));
    assert!(!frame.text().contains("second dialog"));

    app.run_lua_result(
        r#"
        second = smelt.dialog.new({ panels = { { leaf = second_leaf, height = "fit" } } })
        assert(smelt.dialog.current().win == second.win)
        "#,
    )
    .expect("a rejected construction leaves the caller's panel usable");
    assert!(app.render_to_frame().text().contains("second dialog"));
}

#[test]
fn dialog_with_unknown_keymap_chord_is_rejected_before_opening() {
    let mut app = TestApp::builder().build();
    let rejected = app.run_lua_result(
        r#"
        leaf = smelt.dialog.content({ text = "never shown" })
        smelt.dialog.new({
          panels = { { leaf = leaf, height = "fit" } },
          keymaps = { { key = "not-a-chord", on_press = function() end } },
        })
        "#,
    );
    let err = rejected.expect_err("unknown chord must raise");
    assert!(err.contains("unknown chord"), "{err}");
    app.run_lua_result("assert(smelt.dialog.current() == nil)")
        .unwrap();
    assert!(!app.render_to_frame().text().contains("never shown"));
}

#[test]
fn transparent_overlay_is_not_modal_and_rejects_mouse_capture() {
    let mut app = TestApp::builder().build();
    const OPEN: &str = r#"
        local paint = smelt.paint.register(function() end)
        return smelt.overlay.new({ layout = smelt.ui.layout.leaf(paint), transparent = true %s })
    "#;
    app.run_lua_result(&OPEN.replace("%s", "")).unwrap();
    app.render();
    assert!(app.tui.ui.active_modal().is_none());

    for extra in [", modal = true", ", draggable = true", ", resizable = true"] {
        let err = app.run_lua_result(&OPEN.replace("%s", extra)).unwrap_err();
        assert!(
            err.contains("cannot be modal, draggable or resizable"),
            "{extra}: {err}"
        );
    }
}

#[test]
fn unprepared_edit_file_permission_previews_the_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("native.rs");
    std::fs::write(&file, "const NATIVE_BEFORE: i32 = 1;\n").unwrap();
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 30);
    app.start_submitted_turn("edit");
    // Claude Code runs its native Edit itself, so smelt never prepared it.
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(7),
            request_id: 7,
            call_id: "call-7".into(),
            tool_name: "edit_file".into(),
            provider: None,
            args: std::collections::HashMap::from([
                (
                    "file_path".into(),
                    serde_json::json!(file.to_string_lossy()),
                ),
                ("old_string".into(), serde_json::json!("NATIVE_BEFORE")),
                ("new_string".into(), serde_json::json!("NATIVE_AFTER")),
            ]),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain("native.rs"),
            access: protocol::Access::Write,
        },
    ));
    let frame = app.render_to_frame().text();
    assert!(
        frame.contains("NATIVE_BEFORE") && frame.contains("NATIVE_AFTER"),
        "permission dialog previews the replacement:\n{frame}"
    );
}
