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
    let (dir, mut app) = fixture_app();
    std::fs::write(dir.path().join("beta.txt"), "beta preview\n".repeat(318)).unwrap();
    let source_column = |frame: &str, marker: &str| {
        frame
            .lines()
            .find_map(|row| {
                row.split_once(marker)
                    .map(|(prefix, _)| prefix.chars().count())
            })
            .unwrap()
    };
    app.run_lua_result(
        r#"
        smelt.terminal.open_editor = function(path) opened_file = path; return true end
        smelt.cmd.run("files")
        "#,
    )
    .unwrap();
    let frame = wait_for_frame(&mut app, "alpha_preview");
    let alpha_column = source_column(&frame, "fn alpha_preview");
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
    let frame = wait_for_frame(&mut app, "beta preview");
    assert_eq!(
        source_column(&frame, "beta preview"),
        alpha_column,
        "{frame}"
    );
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
fn file_finder_empty_and_binary_previews() {
    let (dir, mut app) = fixture_app();
    std::fs::write(dir.path().join("binary.dat"), b"hello\0world").unwrap();
    app.run_lua_result("smelt.cmd.run('files')").unwrap();
    app.type_text("binary");
    wait_for_frame(&mut app, "(binary file)");
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

/// A workspace of real source files: every `smelt-core` source, each tagged
/// with a first line naming it so a frame tells which file is previewed.
fn source_tree_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/src");
    copy_tree(&source, dir.path(), |relative, text| {
        format!("// bench {}\n{text}", relative.display())
    });
    dir
}

/// Copies every file under `from` into `to`, passing each through `edit`
/// with its relative path. Returns the bytes written.
fn copy_tree(
    from: &std::path::Path,
    to: &std::path::Path,
    edit: impl Fn(&std::path::Path, String) -> String,
) -> usize {
    let mut bytes = 0;
    let mut stack = vec![from.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path.strip_prefix(from).unwrap();
            let target = to.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            let text = edit(relative, std::fs::read_to_string(&path).unwrap());
            bytes += text.len();
            std::fs::write(&target, text).unwrap();
        }
    }
    bytes
}

/// The file the preview shows, from its `// bench <path>` marker.
fn previewed(frame: &str) -> Option<String> {
    let (_, rest) = frame.split_once("// bench ")?;
    Some(rest.split_whitespace().next()?.to_string())
}

/// What users do in the file finder: hold an arrow key and wait for the
/// preview. Frame cost is one terminal-loop iteration; preview latency adds
/// timer delays on a virtual clock, so it counts debounce waits too.
#[test]
#[ignore = "timed benchmark: cargo xtask bench-files"]
fn file_finder_benchmark() {
    let dir = source_tree_fixture();
    let mut app = TestApp::builder().with_cwd(dir.path()).build();
    app.set_terminal_size(200, 55);
    app.animate_with_virtual_clock();
    app.run_lua_result("smelt.cmd.run('files')").unwrap();
    wait_for_frame(&mut app, "// bench ");
    let summarize = |label: &str, mut samples: Vec<Duration>| {
        samples.sort();
        let at = |q: usize| samples[(samples.len() - 1) * q / 100];
        println!(
            "  {label:<28} n={:<4} p50={:>9.2?} p95={:>9.2?} max={:>9.2?}",
            samples.len(),
            at(50),
            at(95),
            samples[samples.len() - 1]
        );
    };

    // Held key: a frame per repeat, 30 ms apart. A frame is stale when the
    // preview still changes once timers settle.
    let mut frames = Vec::new();
    let mut stale = 0;
    for i in 0..300 {
        let start = std::time::Instant::now();
        app.press(if i % 100 < 50 {
            KeyCode::Up
        } else {
            KeyCode::Down
        });
        let shown = previewed(&app.advance_animation(30).text());
        frames.push(start.elapsed());
        if shown != previewed(&app.advance_animation(500).text()) {
            stale += 1;
        }
    }
    summarize("held key frame", frames);
    println!("  held key frames showing a stale preview: {stale} / 300");

    // One key press, then a frame every 1 ms until the preview changes. The
    // latency is the timer wait plus the wall time of the press and of the
    // frame that painted the new preview.
    let mut latencies = Vec::new();
    let mut preview_frames = Vec::new();
    for i in 0..100 {
        let before = previewed(&app.advance_animation(500).text());
        let start = std::time::Instant::now();
        app.press(if i % 20 < 10 {
            KeyCode::Up
        } else {
            KeyCode::Down
        });
        let press = start.elapsed();
        let mut waited = 0;
        let painted = loop {
            let start = std::time::Instant::now();
            let frame = app.advance_animation(1).text();
            let elapsed = start.elapsed();
            if previewed(&frame) != before || waited >= 1000 {
                break elapsed;
            }
            waited += 1;
        };
        preview_frames.push(painted);
        latencies.push(press + painted + Duration::from_millis(waited));
    }
    summarize("preview frame", preview_frames);
    summarize("key to preview", latencies);
    app.press(KeyCode::Esc);
}

/// Typing searches over 8 copies of the `smelt-core` sources, then browsing
/// the results. Search time is wall time until the counter stops showing
/// progress; frames run every 1 ms meanwhile.
#[test]
#[ignore = "timed benchmark: cargo xtask bench-files"]
fn code_search_benchmark() {
    let dir = tempfile::tempdir().unwrap();
    let source = source_tree_fixture();
    let bytes: usize = (0..8)
        .map(|copy| {
            copy_tree(
                source.path(),
                &dir.path().join(format!("copy{copy}")),
                |_, text| text,
            )
        })
        .sum();
    let mut app = TestApp::builder().with_cwd(dir.path()).build();
    app.set_terminal_size(200, 55);
    app.animate_with_virtual_clock();
    // Warm the file index the way a session does before the first search.
    app.run_lua_result("smelt.files.search('', { cwd = smelt.session.cwd() })")
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while app
        .run_lua_result(
            "assert(smelt.files.search('', { cwd = smelt.session.cwd() }).status == 'ready')",
        )
        .is_err()
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    println!("  corpus: {} MiB", bytes >> 20);
    // The last query stays open for browsing below.
    for query in ["fn ", "unwrap()", "LineDecoration", "zzzz_no_match", "self"] {
        // The input box's text, without the preview beside it.
        let input_row = |frame: &str| {
            let prefix = format!("│ {query}");
            frame
                .lines()
                .map(|row| row.trim_start())
                .find(|row| row.starts_with(&prefix))
                .and_then(|row| row.split("││").next())
                .unwrap_or_default()
                .to_string()
        };
        app.run_lua_result("smelt.cmd.run('search')").unwrap();
        wait_for_frame(&mut app, "(type to search)");
        let start = std::time::Instant::now();
        let mut first = None;
        app.type_text(query);
        let frame = loop {
            app.feed_one(SourceEvent::LuaWakeup);
            let frame = app.advance_animation(1).text();
            if first.is_none() && frame.contains(".rs:") {
                first = Some(start.elapsed());
            }
            let counter = input_row(&frame);
            if counter.contains("match") && !counter.contains('%') {
                break frame;
            }
            assert!(start.elapsed() < Duration::from_secs(30), "{frame}");
        };
        let counter = input_row(&frame);
        println!(
            "  search {query:<16} first row {:>9}  done {:>9.2?}  {}",
            first.map_or("-".to_string(), |first| format!("{first:.2?}")),
            start.elapsed(),
            counter
                .trim_end_matches(['│', ' '])
                .rsplit("  ")
                .next()
                .unwrap_or_default()
        );
        if query != "self" {
            app.press(KeyCode::Esc);
        }
    }

    // Browse the `self` results: one frame per held-key repeat, and the frame
    // that paints each new preview.
    let summarize = |label: &str, mut samples: Vec<Duration>| {
        samples.sort();
        let at = |q: usize| samples[(samples.len() - 1) * q / 100];
        println!(
            "  {label:<28} n={:<4} p50={:>9.2?} p95={:>9.2?} max={:>9.2?}",
            samples.len(),
            at(50),
            at(95),
            samples[samples.len() - 1]
        );
    };
    let mut frames = Vec::new();
    for i in 0..300 {
        let start = std::time::Instant::now();
        app.press(if i % 100 < 50 {
            KeyCode::Down
        } else {
            KeyCode::Up
        });
        app.advance_animation(30);
        frames.push(start.elapsed());
    }
    summarize("held key frame", frames);
    app.press(KeyCode::Esc);
}

#[test]
fn file_finder_drops_preview_when_terminal_narrows() {
    let (_dir, mut app) = fixture_app();
    app.run_lua_result("smelt.cmd.run('files')").unwrap();
    wait_for_frame(&mut app, " preview ");
    app.set_terminal_size(70, 30);
    app.render_silent();
    app.dispatch_ui_window_events(false);
    let frame = wait_for_frame(&mut app, "alpha.rs");
    assert!(!frame.contains(" preview "), "{frame}");
    app.set_terminal_size(120, 30);
    app.render_silent();
    app.dispatch_ui_window_events(false);
    let frame = wait_for_frame(&mut app, "alpha_preview");
    assert!(frame.contains(" preview "), "{frame}");
}

fn search_fixture() -> (tempfile::TempDir, TestApp) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let body: String = (1..=60)
        .map(|i| format!("    let filler_{i} = {i};\n"))
        .collect();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        format!("fn spawn() {{\n    let task = async_task();\n{body}    run(task);\n}}\n"),
    )
    .unwrap();
    std::fs::write(dir.path().join("notes.txt"), "no match here\nTask list\n").unwrap();
    let mut app = TestApp::builder().with_cwd(dir.path()).build();
    app.set_terminal_size(120, 30);
    (dir, app)
}

#[test]
fn search_lists_matching_lines_and_previews_each_at_its_line() {
    let (_dir, mut app) = search_fixture();
    app.run_lua_result(
        r#"
        smelt.settings.file_icons = true
        smelt.terminal.open_editor = function(path, opts) opened = { path, opts and opts.line }; return true end
        smelt.cmd.run("search")
        "#,
    )
    .unwrap();
    wait_for_frame(&mut app, "(type to search)");
    app.type_text("task");
    let frame = wait_for_frame(&mut app, "3 matches");
    // Matches read upward from the input in path order, with file icons,
    // and the first one is previewed.
    let rows: Vec<&str> = frame.lines().collect();
    let row = |needle: &str| rows.iter().position(|row| row.contains(needle)).unwrap();
    let first = row("notes.txt:2:1: Task list");
    assert_eq!(
        row("src/lib.rs:2:9: let task = async"),
        first - 1,
        "{frame}"
    );
    assert_eq!(row("src/lib.rs:63:9: run(task);"), first - 2, "{frame}");
    assert!(rows[first + 1].contains('├'), "{frame}");
    assert!(rows[first - 1].contains("\u{e68b} src/lib.rs"), "{frame}");
    assert!(frame.contains("  2 Task list"), "{frame}");
    app.press(KeyCode::Up);
    app.press(KeyCode::Up);
    wait_for_frame(&mut app, "  63     run(task);");
    let frame = app.render_to_frame();
    let cell = |needle: &str, offset: usize| {
        let row = frame
            .rows
            .iter()
            .position(|row| row.contains(needle))
            .unwrap();
        let byte = frame.rows[row].find(needle).unwrap();
        let col = frame.rows[row][..byte].chars().count() + offset;
        frame.styles[row][col]
    };
    let list_match = cell("src/lib.rs:63:9: run(task);", 21);
    let preview_match = cell("  63     run(task);", 13);
    let preview_plain = cell("  63     run(task);", 9);
    let other_line = cell("  62     let filler", 9);
    // Matches share the search highlight in both panes; the focused line
    // and the selected row get the cursor-line background.
    assert_eq!(list_match, preview_match);
    assert_ne!(preview_match.bg, preview_plain.bg);
    assert!(preview_plain.bg.is_some());
    assert_eq!(preview_plain.fg, other_line.fg);
    assert_eq!(other_line.bg, None);
    assert_eq!(cell("src/lib.rs:63:9", 0).bg, preview_plain.bg);
    assert_eq!(cell("src/lib.rs:2:9", 0).bg, None);
    // A taller list grows fill rows above; the selection stays on its match.
    app.set_terminal_size(120, 40);
    let frame = app.advance_animation(50);
    let row = |needle: &str| {
        frame
            .rows
            .iter()
            .position(|row| row.contains(needle))
            .unwrap()
    };
    let first = row("notes.txt:2:1");
    assert!(frame.rows[first + 1].contains('├'), "{}", frame.text());
    let selected = row("src/lib.rs:63:9");
    let col = frame.rows[selected].find("src").unwrap();
    let col = frame.rows[selected][..col].chars().count();
    assert!(frame.styles[selected][col].bg.is_some(), "{}", frame.text());
    app.press(KeyCode::Enter);
    app.run_lua_result(
        "assert(opened[1]:match('/src/lib.rs$') and opened[2] == 63, tostring(opened[1]))",
    )
    .unwrap();
}

#[test]
fn search_git_covers_tracked_files_and_reports_no_matches() {
    let (dir, mut app) = search_fixture();
    for args in [vec!["init", "-q"], vec!["add", "src/lib.rs"]] {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
    }
    app.run_lua_result("smelt.cmd.run('search bogus')").unwrap();
    assert!(app
        .render_to_frame()
        .text()
        .contains("usage: /search [all|git]"));
    app.run_lua_result("smelt.cmd.run('search git')").unwrap();
    wait_for_frame(&mut app, "(no file selected)");
    app.type_text("task");
    let frame = wait_for_frame(&mut app, "2 matches");
    assert!(!frame.contains("notes.txt"), "{frame}");
    app.type_text("_nowhere");
    let frame = wait_for_frame(&mut app, "(no matches)");
    assert!(frame.contains("0 matches"), "{frame}");
    assert!(frame.contains("(no file selected)"), "{frame}");
    app.press(KeyCode::Esc);
    assert!(!app.render_to_frame().text().contains(" preview "));
}

/// The list box's highlighted row and the row just above its bottom edge.
fn search_selection(frame: &crate::smelt_edit::SnapshotFrame) -> (Option<usize>, usize) {
    let top = frame.rows.iter().position(|row| row.contains('┌')).unwrap();
    let left = frame.rows[top][..frame.rows[top].find('┌').unwrap()]
        .chars()
        .count();
    let bottom = (top + 1..frame.rows.len())
        .find(|&row| frame.rows[row].chars().nth(left) == Some('├'))
        .unwrap();
    let selected = (top + 1..bottom).find(|&row| frame.styles[row][left + 2].bg.is_some());
    (selected, bottom - 1)
}

#[test]
fn search_selection_stays_on_the_bottom_row_while_typing() {
    let dir = tempfile::tempdir().unwrap();
    // Enough files that matches arrive over several publishes.
    for file in 0..2000 {
        let body: String = (0..5)
            .map(|i| format!("let task_{i} = {file};\n"))
            .collect();
        std::fs::write(dir.path().join(format!("f{file:04}.rs")), body).unwrap();
    }
    let mut app = TestApp::builder().with_cwd(dir.path()).build();
    app.set_terminal_size(120, 30);
    app.run_lua_result("smelt.cmd.run('search')").unwrap();
    let frame = wait_for_frame(&mut app, "(type to search)");
    let (selected, bottom) = search_selection(&app.render_to_frame());
    assert_eq!(selected, Some(bottom), "{frame}");
    for ch in "task_3".chars() {
        app.type_text(&ch.to_string());
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            app.feed_one(SourceEvent::LuaWakeup);
            let frame = app.advance_animation(1);
            let text = frame.text();
            if text.contains("f0000.rs:") {
                let (selected, bottom) = search_selection(&frame);
                assert_eq!(selected, Some(bottom), "{text}");
                assert!(frame.rows[bottom].contains("f0000.rs:"), "{text}");
                let counter = text.lines().find(|row| row.contains(" matches")).unwrap();
                if !counter.contains('%') {
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline, "{text}");
        }
    }
    app.type_text("zz");
    let frame = wait_for_frame(&mut app, "(no matches)");
    let (selected, bottom) = search_selection(&app.render_to_frame());
    assert_eq!(selected, Some(bottom), "{frame}");
}

#[test]
fn file_finder_divider_drag_resizes_results_and_preview() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let (_dir, mut app) = fixture_app();
    app.run_lua_result("smelt.cmd.run('files')").unwrap();
    wait_for_frame(&mut app, " preview ");
    let preview_rect = |app: &TestApp| {
        let win = app.ui_probe().named_win("smelt.files.preview").unwrap();
        app.ui_probe().win(win).unwrap().viewport.unwrap().rect
    };
    let before = preview_rect(&app);
    for (kind, delta) in [
        (MouseEventKind::Down(MouseButton::Left), 0),
        (MouseEventKind::Drag(MouseButton::Left), -10),
        (MouseEventKind::Up(MouseButton::Left), -10),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: before.y + 4,
            column: (before.x - 1).saturating_add_signed(delta),
            modifiers: KeyModifiers::NONE,
        })));
        app.render_silent();
    }
    let after = preview_rect(&app);
    assert_eq!(
        after.width,
        before.width + 10,
        "{}",
        app.render_to_frame().text()
    );
}
