use super::*;

fn fixture_app() -> (tempfile::TempDir, TestApp) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alpha.rs"), "fn alpha_preview() {}\n").unwrap();
    std::fs::write(dir.path().join("beta.txt"), "beta preview\n").unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let mut app = TestApp::builder().with_cwd(dir.path()).build();
    app.set_terminal_size(120, 30);
    app.run_lua_result("smelt.settings.file_icons = true")
        .unwrap();
    (dir, app)
}

fn wait_for_frame(app: &mut TestApp, text: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        app.feed_one(SourceEvent::LuaWakeup);
        app.advance_animation(100);
        let frame = app.render_to_frame().text();
        if frame.contains(text) {
            return frame;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "missing {text}:\n{frame}"
        );
        std::thread::yield_now();
    }
}

#[test]
fn file_finder_search_preview_accept_and_dismiss() {
    let (_dir, mut app) = fixture_app();
    app.run_lua_result(
        r#"
        smelt.terminal.open_editor = function(path) opened_file = path; return true end
        smelt.cmd.run("files")
        "#,
    )
    .unwrap();
    let frame = wait_for_frame(&mut app, "alpha_preview");
    let alpha_row = frame
        .lines()
        .position(|row| row.contains("alpha.rs"))
        .unwrap();
    let beta_row = frame
        .lines()
        .position(|row| row.contains("beta.txt"))
        .unwrap();
    assert!(beta_row < alpha_row, "{frame}");
    app.press(KeyCode::Up);
    wait_for_frame(&mut app, "beta preview");
    app.advance_animation(500);
    assert!(app.render_to_frame().text().contains("beta preview"));
    app.press(KeyCode::Down);
    wait_for_frame(&mut app, "alpha_preview");
    app.type_text("beta");
    let frame = wait_for_frame(&mut app, "beta preview");
    assert!(!frame.contains("alpha.rs"), "{frame}");
    assert!(!frame.contains("alpha_preview"), "{frame}");
    app.press(KeyCode::Enter);
    app.run_lua_result("assert(opened_file == 'beta.txt' or opened_file:match('/beta.txt$'), tostring(opened_file))")
        .unwrap();
    assert!(!app.render_to_frame().text().contains(" preview "));
    app.run_lua_result("smelt.cmd.run('files')").unwrap();
    let frame = wait_for_frame(&mut app, "beta preview");
    let alpha_row = frame
        .lines()
        .position(|row| row.contains("alpha.rs"))
        .unwrap();
    let beta_row = frame
        .lines()
        .position(|row| row.contains("beta.txt"))
        .unwrap();
    assert!(alpha_row < beta_row, "{frame}");
    app.press(KeyCode::Esc);
    assert!(!app.render_to_frame().text().contains(" preview "));
}

#[test]
fn file_finder_has_no_default_bindings_and_validates_source() {
    let (_dir, mut app) = fixture_app();
    app.drain_launch_ready_hooks();
    app.run_lua_result(
        r#"
        assert(#smelt.keymap.prefixes('<leader>f', 'n') == 0)
        smelt.cmd.run('files invalid')
    "#,
    )
    .unwrap();
    assert!(!app.render_to_frame().text().contains(" preview "));
    assert!(app
        .render_to_frame()
        .text()
        .contains("usage: /files [all|git]"));
    app.run_lua_result("smelt.cmd.run('files all')").unwrap();
    wait_for_frame(&mut app, "alpha_preview");
}

#[test]
fn file_finder_counter_uses_total_matches_without_overlapping_query() {
    let (_dir, mut app) = fixture_app();
    app.run_lua_result(r#"
        smelt.files.search = function(query)
            if query == '' then
                return { items = { { label = 'alpha.rs', path = 'alpha.rs' } }, total_matched = 2777, total_files = 2777 }
            end
            return { items = {}, total_matched = 0, total_files = 2777 }
        end
        smelt.cmd.run('files')
    "#).unwrap();
    let frame = wait_for_frame(&mut app, "alpha_preview");
    assert!(frame.contains("2777 / 2777"), "{frame}");
    app.type_text(&"query".repeat(30));
    let frame = wait_for_frame(&mut app, "0 / 2777");
    let row = frame.lines().find(|row| row.contains("0 / 2777")).unwrap();
    assert!(row.contains("query"), "{frame}");
    assert!(frame.contains("no matching files"), "{frame}");
}

fn mock_pending_file_search(app: &mut TestApp) {
    app.run_lua_result(r#"
        search_ready = false
        local items = {
            { label = 'alpha.rs', path = 'alpha.rs', kind = 'file' },
            { label = 'beta.txt', path = 'beta.txt', kind = 'file' },
        }
        smelt.files.search = function(query)
            if query == '' then return { items = items, total_matched = 2, total_files = 2 } end
            if not search_ready then return { items = {}, searching = true, message = 'searching files…' } end
            if query == 'beta' then return { items = { items[2] }, total_matched = 1, total_files = 2 } end
            return { items = {}, total_matched = 0, total_files = 2, message = 'no matching files' }
        end
    "#).unwrap();
}

#[test]
fn file_finder_keeps_stale_results_until_search_completes() {
    for command in ["files", "files git"] {
        let (_dir, mut app) = fixture_app();
        mock_pending_file_search(&mut app);
        app.run_lua_result(&format!("smelt.cmd.run('{command}')"))
            .unwrap();
        wait_for_frame(&mut app, "alpha_preview");
        app.type_text("beta");
        let frame = app.render_to_frame().text();
        assert!(
            frame.contains("alpha.rs") && frame.contains("alpha_preview"),
            "{frame}"
        );
        assert!(!frame.contains("no matching files"), "{frame}");
        assert!(frame.contains("2 / 2"), "{frame}");
        app.run_lua_result("search_ready = true").unwrap();
        let frame = wait_for_frame(&mut app, "beta preview");
        assert!(!frame.contains("alpha.rs"), "{frame}");
        assert!(frame.contains("1 / 2"), "{frame}");
        app.type_text("!");
        let frame = wait_for_frame(&mut app, "no matching files");
        assert!(frame.contains("0 / 2"), "{frame}");
    }
}

#[test]
fn file_finder_at_completion_keeps_stale_results() {
    let (_dir, mut app) = fixture_app();
    mock_pending_file_search(&mut app);
    app.type_text("@");
    wait_for_frame(&mut app, "alpha.rs");
    app.type_text("beta");
    let frame = app.render_to_frame().text();
    assert!(
        frame.contains("alpha.rs") && frame.contains("beta.txt"),
        "{frame}"
    );
    assert!(!frame.contains("searching files"), "{frame}");
    app.run_lua_result("search_ready = true").unwrap();
    app.advance_animation(200);
    let frame = wait_for_frame(&mut app, "beta.txt");
    assert!(!frame.contains("alpha.rs"), "{frame}");
    app.type_text("!");
    wait_for_frame(&mut app, "no matching files");
}

#[test]
fn file_finder_icon_settings_and_palette() {
    for (enabled, colors, light) in [
        (false, true, false),
        (true, false, false),
        (true, true, false),
        (true, true, true),
    ] {
        let (_dir, mut app) = fixture_app();
        app.run_lua_result(&format!(
            r#"
            smelt.settings.file_icons = {enabled}
            smelt.settings.file_icon_colors = {colors}
            local spec = smelt.theme.spec('default')
            spec.light = {light}
            smelt.theme.apply(spec)
        "#
        ))
        .unwrap();
        app.run_lua_result("smelt.cmd.run('files')").unwrap();
        wait_for_frame(&mut app, "alpha.rs");
        let frame = app.render_to_frame();
        let row = frame
            .rows
            .iter()
            .position(|row| row.contains("alpha.rs"))
            .unwrap();
        let icon_col = frame.rows[row].chars().position(|ch| ch == '\u{e68b}');
        assert_eq!(icon_col.is_some(), enabled, "{}", frame.text());
        if let Some(col) = icon_col {
            let label_col = col + 2;
            if colors {
                let (r, g, b) = if light {
                    (0x6f, 0x52, 0x42)
                } else {
                    (0xde, 0xa5, 0x84)
                };
                assert_eq!(
                    frame.styles[row][col].fg,
                    Some(smelt_core::style::Color::Rgb { r, g, b })
                );
                assert_ne!(frame.styles[row][col].fg, frame.styles[row][label_col].fg);
            } else {
                assert_eq!(frame.styles[row][col].fg, frame.styles[row][label_col].fg);
            }
        }
    }
}

#[test]
fn file_finder_empty_binary_and_truncated_previews() {
    let (dir, mut app) = fixture_app();
    std::fs::write(dir.path().join("binary.dat"), b"hello\0world").unwrap();
    std::fs::write(dir.path().join("huge.txt"), "x".repeat(80000)).unwrap();
    app.run_lua_result("smelt.cmd.run('files')").unwrap();
    app.type_text("binary");
    wait_for_frame(&mut app, "(binary file)");
    app.press(KeyCode::Esc);
    app.run_lua_result("smelt.cmd.run('files')").unwrap();
    app.type_text("huge");
    wait_for_frame(&mut app, "[preview truncated]");
    app.press(KeyCode::Esc);
    app.run_lua_result("smelt.cmd.run('files')").unwrap();
    app.type_text("no_such_file");
    wait_for_frame(&mut app, "(no matching files)");
    app.press(KeyCode::Enter);
    assert!(app.render_to_frame().text().contains(" preview "));
    app.press(KeyCode::Esc);
    app.set_terminal_size(70, 18);
    app.run_lua_result("smelt.cmd.run('files')").unwrap();
    let frame = wait_for_frame(&mut app, "alpha.rs");
    assert!(!frame.contains(" preview "), "{frame}");
}

#[test]
fn file_finder_git_excludes_untracked_files() {
    let (dir, mut app) = fixture_app();
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "tracked despite ignore\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "alpha.rs"],
        vec!["add", "-f", "ignored.txt"],
    ] {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
    }
    app.run_lua_result("smelt.cmd.run('files git')").unwrap();
    let frame = wait_for_frame(&mut app, "alpha_preview");
    assert!(frame.contains("alpha.rs"), "{frame}");
    assert!(!frame.contains("beta.txt"), "{frame}");
    assert!(frame.contains("ignored.txt"), "{frame}");
    assert!(frame.contains("2 / 2"), "{frame}");
    app.type_text("ignored");
    let frame = wait_for_frame(&mut app, "tracked despite ignore");
    assert!(frame.contains("1 / 2"), "{frame}");
    app.press(KeyCode::Esc);
}

#[test]
fn file_finder_uses_search_root_from_repository_subdirectory() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("nested");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(dir.path().join("root.rs"), "fn root_preview() {}\n").unwrap();
    for args in [["init", "-q"], ["add", "root.rs"]] {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
    }
    for source in ["all", "git"] {
        let mut app = TestApp::builder().with_cwd(&sub).build();
        app.set_terminal_size(120, 30);
        app.run_lua_result(&format!(
            r#"
            smelt.terminal.open_editor = function(path) opened_file = path; return true end
            smelt.cmd.run('files {source}')
        "#
        ))
        .unwrap();
        wait_for_frame(&mut app, "root_preview");
        app.press(KeyCode::Enter);
        let expected = std::fs::canonicalize(dir.path()).unwrap().join("root.rs");
        app.run_lua_result(&format!(
            "assert(opened_file == {:?}, opened_file)",
            expected.to_str().unwrap()
        ))
        .unwrap();
    }
}

#[test]
fn file_finder_search_errors_close_overlay_without_poll_leak() {
    let (_dir, mut app) = fixture_app();
    app.run_lua_result(
        r#"
        for _, source in ipairs({ 'bogus', false, 42, {} }) do
            local ok, err = pcall(smelt.files.search, '', { source = source })
            assert(not ok and tostring(err):find('source must be', 1, true))
        end
        calls = 0
        smelt.files.search = function(_, opts)
            assert(opts.source == 'git' and opts.limit == 500 and not opts.include_dirs)
            calls = calls + 1
            return { items = {}, status = 'error', message = 'fixture Git error' }
        end
        smelt.cmd.run('files git')
    "#,
    )
    .unwrap();
    assert!(!app.render_to_frame().text().contains(" preview "));
    app.advance_animation(1000);
    app.run_lua_result("assert(calls == 1)").unwrap();
    app.run_lua_result("smelt.cmd.run('files git')").unwrap();
    drive_lua_tasks(&mut app);
    app.run_lua_result("assert(calls == 2)").unwrap();
}

#[test]
fn file_finder_editor_reports_missing_file_or_terminal() {
    let (_dir, mut app) = fixture_app();
    app.run_lua_result(
        r#"
        local ok, err = smelt.terminal.open_editor('missing.rs')
        assert(not ok and err:find('not a file', 1, true), err)
        ok, err = smelt.terminal.open_editor('alpha.rs')
        assert(not ok and err:find('without an attached terminal', 1, true), err)
    "#,
    )
    .unwrap();
}
