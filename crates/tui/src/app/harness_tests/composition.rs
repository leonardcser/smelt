use super::*;

#[test]
fn render_part_uses_live_composition_while_get_renderer_keeps_a_snapshot() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 20);
    app.run_lua_result(
        r#"
        local t, l = smelt.transcript, smelt.layout
        captured_user_content = t.get_renderer("user_content")
        t.set_renderer(function(block, ctx)
            return l.row_prefix(t.render_part("user_content", block, ctx), { first = "bubble: " })
        end, { part = "user" })
        "#,
    )
    .unwrap();
    app.push_user_block("hello");
    assert!(app.render_to_frame().text().contains("bubble: hello"));
    app.run_lua_result(
        r#"
        live_extension = smelt.transcript.extend_renderer("test.live", function(next, block, ctx)
            return smelt.layout.row_prefix(next(block, ctx), { first = "late: " })
        end, { part = "user_content" })
        "#,
    )
    .unwrap();
    assert!(app.render_to_frame().text().contains("bubble: late: hello"));
    app.run_lua_result("live_extension:remove()").unwrap();
    assert!(app.render_to_frame().text().contains("bubble: hello"));
    app.run_lua_result(
        r#"
        local t = smelt.transcript
        t.set_renderer(function() return smelt.layout.text("replacement") end, { part = "user_content" })
        "#,
    ).unwrap();
    assert!(app.render_to_frame().text().contains("bubble: replacement"));
    app.run_lua_result("smelt.transcript.set_renderer(captured_user_content, { part = 'user' })")
        .unwrap();
    let text = app.render_to_frame().text();
    assert!(
        text.contains("hello") && !text.contains("replacement"),
        "{text}"
    );
    app.run_lua_result(
        r#"
        local ok, err = pcall(smelt.transcript.render_part, "missing", {}, {})
        assert(not ok and tostring(err):find("render_part: unknown renderer part", 1, true), tostring(err))
        "#,
    ).unwrap();
    app.assert_invariants();
}

#[test]
fn transcript_settings_reject_unknown_keys_after_table_replacement() {
    let mut app = TestApp::builder().build();
    app.run_lua_result(
        r#"
        local settings = smelt.settings
        local function rejected(fn, key)
            local ok, err = pcall(fn)
            assert(not ok and tostring(err):find("smelt.settings.transcript: unknown key `" .. key .. "`", 1, true), tostring(err))
        end
        rejected(function() settings.transcript.tool_marker = "" end, "tool_marker")
        rejected(function() return settings.transcript.typo end, "typo")
        local original = settings.transcript
        rejected(function() settings.transcript = { show_timestamps = false, typo = true } end, "typo")
        assert(settings.transcript == original and settings.transcript.show_timestamps == true)
        for _ = 1, 2 do
            local replacement = { show_timestamps = false, view = {}, limits = {} }
            settings.transcript = replacement
            assert(settings.transcript == replacement)
            settings.transcript.show_timestamps = true
            assert(settings.transcript.show_timestamps == true)
            rejected(function() settings.transcript.tool_marker = "" end, "tool_marker")
            rejected(function() replacement.typo = true end, "typo")
            settings.transcript.view.custom = "plugin-owned"
            settings.transcript.limits.custom_rows = 3
            assert(settings.transcript.view.custom == "plugin-owned")
            local paired
            for key, value in pairs(settings) do
                if key == "transcript" then paired = value end
            end
            assert(paired == replacement)
        end
        settings.transcript = {}
        settings.transcript.show_timestamps = false
        assert(settings.transcript.show_timestamps == false and settings.transcript.view == nil)
        rejected(function() settings.transcript.typo = true end, "typo")
        "#,
    ).unwrap();
}

#[test]
fn unadorned_tool_headers_keep_semantic_names() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(72, 20);
    app.run_lua_result(
        r#"
        local t = smelt.transcript
        t.set_renderer(t.get_renderer("tool_header_content"), { part = "tool_header" })
    "#,
    )
    .unwrap();
    app.start_turn(1);
    let id = app.tool_started(
        "shell",
        "bash",
        std::collections::HashMap::from([("command".into(), serde_json::json!("cargo test"))]),
    );
    app.tool_finished(
        id,
        "shell",
        protocol::ToolOutcome::new("checks passed".into(), false, None),
        Some(1200),
    );
    let text = app.render_to_frame().text();
    assert!(text.contains("bash cargo test"), "{text}");
    assert!(!text.contains("* bash"), "{text}");
    app.assert_invariants();
}

#[test]
fn bubble_view_reuses_user_content_without_default_panel() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 18);
    app.run_lua_result(
        r#"
        local t, l = smelt.transcript, smelt.layout
        t.set_renderer(function(block, ctx)
            return l.row_prefix(t.get_renderer("user_content")(block, ctx), {
                first = "you: ", rest = "     ",
            })
        end, { part = "user" })
    "#,
    )
    .unwrap();
    app.push_user_block("hello from the gallery");
    let text = app.render_to_frame().text();
    assert!(text.contains("you: hello from the gallery"), "{text}");
    app.assert_invariants();
}

#[test]
fn dialog_composition_changes_chrome_without_replacing_behavior() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(60, 20);
    app.run_lua_result(r#"
        local l = smelt.ui.layout
        local compose = assert(smelt.dialog.compose)
        smelt.dialog.compose = function(body, opts)
            return l.frame(body, { border = "rounded", padding = 1, title = "custom " .. (opts.title or "") })
        end
        smelt.spawn(function()
            local win = smelt.dialog.input("reusable body", { pad_left = 0, pad_right = 0 })
            dialog_result = smelt.dialog.open({
                title = "dialog", height = 8, panels = { { leaf = win } },
                on_submit = function(ctx) ctx.resolve("accepted") end,
            })
        end)
    "#).unwrap();
    drive_lua_tasks(&mut app);
    let frame = app.render_to_frame().text();
    assert!(frame.contains("custom dialog"), "{frame}");
    assert!(frame.contains("reusable body"), "{frame}");
    assert_eq!(
        app.eval_lua::<u16>("return smelt.dialog.current().win:rect().height")
            .unwrap(),
        4
    );
    app.press(KeyCode::Enter);
    drive_lua_tasks(&mut app);
    assert_eq!(
        app.eval_lua::<String>("return dialog_result").unwrap(),
        "accepted"
    );
    app.assert_invariants();
}

#[test]
fn dialog_picker_reuses_composition_and_styled_list_rows() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(64, 20);
    app.run_lua_result(r#"
        smelt.dialog.compose = function(body, opts)
            return smelt.ui.layout.frame(body, { border = "rounded", title = "pick a view", padding = 1 })
        end
        smelt.spawn(function()
            picked = smelt.dialog.picker({
                height = 8,
                items = { { id = 7, label = "alpha" }, { id = 9, label = "beta" } },
                render = function(item)
                    return { spans = {
                        { text = "[" .. item.id .. "] ", style = "SmeltAccent" },
                        { text = item.label, style = { bold = true } },
                    } }
                end,
                on_open = function(ctx)
                    ctx.input:configure({ pad_left = 2, prefix = "/ " })
                end,
            })
        end)
    "#).unwrap();
    drive_lua_tasks(&mut app);
    let frame = app.render_to_frame().text();
    assert!(frame.contains("pick a view"), "{frame}");
    assert!(frame.contains("[7] alpha"), "{frame}");
    assert!(frame.contains("[9] beta"), "{frame}");
    app.press(KeyCode::Down);
    app.press(KeyCode::Enter);
    drive_lua_tasks(&mut app);
    assert_eq!(app.eval_lua::<u32>("return picked.id").unwrap(), 9);
    app.assert_invariants();
}

#[test]
fn picker_constructor_override_keeps_navigation_and_original_items() {
    for placement in ["center", "prompt_docked"] {
        let mut app = TestApp::builder().with_vim(false).build();
        app.set_terminal_size(60, 20);
        app.run_lua_result(&format!(r#"
            local new = smelt.picker.new
            smelt.theme.set("CustomPicker", {{ bg = {{ 20, 40, 60 }} }})
            smelt.picker.new = function(opts)
                local picker = new(opts)
                styled_win = picker:win()
                styled_win:configure({{ style = "CustomPicker", pad_left = 2, selection_highlight = false }})
                return picker
            end
            smelt.spawn(function()
                picked = smelt.picker.open({{
                    placement = "{placement}",
                    items = {{ {{ label = "alpha", id = 7 }}, {{ label = "beta", id = 9 }} }},
                }})
            end)
        "#)).unwrap();
        drive_lua_tasks(&mut app);
        let frame = app.render_to_frame();
        let (x, y): (u16, u16) = app
            .eval_lua("local r = styled_win:rect(); return r.x, r.y")
            .unwrap();
        assert_eq!(
            frame.styles[y as usize][x as usize].bg,
            Some(smelt_core::style::Color::Rgb {
                r: 20,
                g: 40,
                b: 60
            })
        );
        app.press(KeyCode::Down);
        app.press(KeyCode::Enter);
        drive_lua_tasks(&mut app);
        assert_eq!(app.eval_lua::<u32>("return picked.item.id").unwrap(), 9);
        assert!(!app.overlays_probe().has_pickers());
        app.assert_invariants();
    }
}

#[test]
fn invalid_dialog_composition_keeps_existing_modal_and_caller_windows() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.run_lua_result(
        r#"
        parent_input = smelt.dialog.input("parent")
        parent = smelt.dialog.new({ panels = { { leaf = parent_input } } })
        local compose = smelt.dialog.compose
        smelt.dialog.compose = function() return smelt.ui.layout.vbox({}) end
        child_input = smelt.dialog.input("child")
        local ok, err = pcall(smelt.dialog.new, { panels = { { leaf = child_input } } })
        assert(not ok and tostring(err):find("retain every body window", 1, true))
        assert(smelt.dialog.current().win == parent_input)
        assert(child_input:buf() ~= nil)
        child_input:close()
        smelt.dialog.compose = compose
    "#,
    )
    .unwrap();
    app.press(KeyCode::Esc);
    assert!(app
        .eval_lua::<bool>("return smelt.dialog.current() == nil")
        .unwrap());
    app.assert_invariants();
}

#[test]
fn invalid_dialog_focus_does_not_mount_partial_modal() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.run_lua_result(
        r#"
        local parent_input = smelt.dialog.input("parent")
        local parent = smelt.dialog.new({ panels = { { leaf = parent_input } } })
        local input = smelt.dialog.input("child")
        local outside = smelt.dialog.input("outside")
        local ok, err = pcall(smelt.dialog.new, { panels = { { leaf = input } }, focus = outside })
        assert(not ok and tostring(err):find("focus", 1, true))
        assert(smelt.dialog.current().win == parent_input)
        assert(input:buf() ~= nil and outside:buf() ~= nil)
        input:close()
        outside:close()
        parent:close()
    "#,
    )
    .unwrap();
    app.assert_invariants();
}

#[test]
fn composed_dialog_owns_extra_windows_without_changing_focus_or_submit() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.run_lua_result(
        r#"
        smelt.dialog.compose = function(body)
            extra = smelt.dialog.content({ text = "extra header", surface = "inert" })
            return smelt.ui.layout.vbox({
                { smelt.ui.layout.leaf(extra), height = 1 },
                { body, height = "fill" },
            })
        end
        input = smelt.dialog.input("body")
        smelt.dialog.new({
            panels = { { leaf = input } }, focus = input, height = 5,
            on_submit = function(ctx) ctx.close() end,
        })
    "#,
    )
    .unwrap();
    app.render_silent();
    app.run_lua_result(
        r#"
        assert(#smelt.dialog.current().panels == 2)
        assert(smelt.dialog.current().focused_leaf == input)
        assert(extra:buf() ~= nil and input:buf() ~= nil)
    "#,
    )
    .unwrap();
    app.press(KeyCode::Enter);
    app.run_lua_result(
        r#"
        assert(smelt.dialog.current() == nil)
        assert(extra:buf() == nil and input:buf() == nil)
    "#,
    )
    .unwrap();
    app.assert_invariants();
}

#[test]
fn centered_modal_reuses_dialog_chrome_and_action_behavior() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(64, 20);
    app.run_lua_result(
        r#"
        smelt.dialog.compose = function(body, opts)
            return smelt.ui.layout.frame(body, { border = "rounded", title = "custom centered" })
        end
        require("smelt.modal").open({
            lines = { "shared modal body" },
            actions = { { label = "accept", value = "yes" } },
            on_submit = function(value) modal_result = value end,
        })
    "#,
    )
    .unwrap();
    let frame = app.render_to_frame().text();
    assert!(frame.contains("custom centered"), "{frame}");
    assert!(frame.contains("shared modal body"), "{frame}");
    app.press(KeyCode::Enter);
    assert_eq!(
        app.eval_lua::<String>("return modal_result").unwrap(),
        "yes"
    );
    app.assert_invariants();
}

#[test]
fn win95_close_button_requests_quit() {
    use crossterm::event::{MouseButton, MouseEventKind};

    fn click(app: &mut TestApp, button: MouseButton, column: u16) {
        app.mouse(MouseEventKind::Down(button), (column, 0));
        app.mouse(MouseEventKind::Up(button), (column, 0));
    }

    for width in [80, 24] {
        for offset in 0..3 {
            let mut app = TestApp::builder().with_vim(false).build();
            app.set_terminal_size(80, 20);
            app.run_lua_result(include_str!("../../../../../docs/shots/gallery/win95.lua"))
                .unwrap();
            app.set_terminal_size(width, 20);
            let frame = app.render_to_frame().text();
            assert!(
                frame.lines().next().unwrap().ends_with("[_][□][×]"),
                "{frame}"
            );

            click(&mut app, MouseButton::Left, width - 4);
            assert!(!app.state().pending_quit, "maximize must not quit");
            click(&mut app, MouseButton::Left, width - 7);
            assert!(!app.state().pending_quit, "minimize must not quit");
            click(&mut app, MouseButton::Right, width - 2);
            assert!(!app.state().pending_quit, "right click must not quit");
            click(&mut app, MouseButton::Left, width - 3 + offset);
            assert!(app.state().pending_quit, "close button must quit");
            app.assert_invariants();
        }
    }
}

#[test]
#[ignore = "set SMELT_GALLERY_DIR to independently maintained gallery sources"]
fn gallery_api_consumers() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("SMELT_GALLERY_DIR").expect("set SMELT_GALLERY_DIR"),
    );
    for name in [
        "panels",
        "timeline",
        "bubbles",
        "mono",
        "huedrift",
        "c64",
        "vaporwave",
        "light",
        "win95",
    ] {
        let mut app = TestApp::builder().with_vim(false).build();
        app.set_terminal_size(80, 30);
        app.animate_with_virtual_clock();
        if matches!(name, "panels" | "timeline") {
            // Supply the marker-free policy through configuration; the gallery source stays unchanged.
            app.run_lua_result(
                r#"
                local t = smelt.transcript
                t.set_renderer(t.get_renderer("tool_header_content"), { part = "tool_header" })
            "#,
            )
            .unwrap();
        }
        let path = dir.join(format!("{name}.lua"));
        let source = std::fs::read_to_string(&path).unwrap();
        app.run_lua_result(&source)
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        app.push_user_block("check gallery composition");
        app.start_turn(1);
        let id = app.tool_started(
            "shell",
            "bash",
            std::collections::HashMap::from([("command".into(), serde_json::json!("cargo test"))]),
        );
        app.tool_finished(
            id,
            "shell",
            protocol::ToolOutcome::new("checks passed".into(), false, None),
            Some(1200),
        );
        for pattern in ["alpha", "beta"] {
            let id = app.tool_started(
                pattern,
                "grep",
                std::collections::HashMap::from([("pattern".into(), serde_json::json!(pattern))]),
            );
            app.tool_finished(
                id,
                pattern,
                protocol::ToolOutcome::new("no matches".into(), false, None),
                Some(10),
            );
        }
        app.advance_animation(240);
        let text = app.render_to_frame().text();
        assert!(text.contains("bash cargo test"), "{name}: {text}");
        if matches!(name, "panels" | "timeline") {
            assert!(!text.contains("* bash"), "{name}: {text}");
            assert!(text.contains("explore"), "{name}: {text}");
        }
        if name == "mono" {
            app.run_lua_result("smelt.cmd.run('mono amber')").unwrap();
            app.render_silent();
        }
        let errors: Vec<_> = app
            .tui
            .lua
            .core_shared()
            .messages
            .lock()
            .unwrap()
            .entries()
            .iter()
            .filter(|entry| entry.kind == smelt_core::messages::MessageKind::Error)
            .map(|entry| entry.full.clone())
            .collect();
        assert!(errors.is_empty(), "{name}: {errors:?}");
        app.assert_invariants();
    }
}

#[test]
fn tool_turns_extend_the_row_index_without_rebuilding_it() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(120, 40);
    smelt_perf::perf::set_enabled(true);
    for turn in 1..=4u64 {
        smelt_perf::perf::clear();
        app.push_user_block(&format!("turn {turn} prompt"));
        app.start_turn(turn);
        for step in 0..3 {
            app.engine_event(protocol::EngineEvent::TextDelta {
                delta: "checking the fixture ".into(),
            });
            app.render_silent();
            let call = format!("call-{turn}-{step}");
            let id = app.tool_started(
                call.clone(),
                "bash",
                std::collections::HashMap::from([("command".into(), serde_json::json!("cat"))]),
            );
            app.render_silent();
            app.tool_finished(
                id,
                call,
                protocol::ToolOutcome::new("fixture output\n".repeat(8), false, None),
                Some(10),
            );
            app.render_silent();
        }
        app.engine_event(protocol::EngineEvent::TextDelta {
            delta: "done".into(),
        });
        app.finish_turn();
        app.render_silent();
        let rebuilds = smelt_perf::perf::snapshot()
            .durations
            .iter()
            .find(|row| row.label == "transcript:prepare_row_index:rebuild_index")
            .map_or(0, |row| row.count);
        if turn > 1 {
            assert_eq!(rebuilds, 0, "turn {turn} rebuilt the whole row index");
        }
    }
    smelt_perf::perf::set_enabled(false);
}

#[test]
fn transcript_cache_budgets_follow_the_viewport_until_pinned() {
    let budgets = |app: &TestApp| {
        let memory = app
            .tui
            .conversation
            .transcript_memory_snapshot_for_harness();
        (
            memory.hydrated_budget_bytes,
            memory.record_budget_bytes,
            memory.rendered_budget_bytes,
        )
    };
    let mut app = TestApp::builder().build();
    app.push_user_block("prompt");
    app.set_terminal_size(80, 24);
    app.render_silent();
    let small = budgets(&app);
    app.set_terminal_size(240, 80);
    app.render_silent();
    let large = budgets(&app);
    assert!(large.0 > small.0 && large.1 > small.1 && large.2 > small.2);
    let default = crate::app::transcript::TranscriptMemoryBudget::default();
    assert!(large.0 < default.hydrated_blocks, "{large:?}");

    let pinned = crate::app::transcript::TranscriptMemoryBudget {
        hydrated_blocks: 3 << 20,
        record_windows: 2 << 20,
        rendered_rows: 5 << 20,
    };
    app.tui
        .conversation
        .set_transcript_memory_budget_for_harness(pinned);
    app.set_terminal_size(100, 30);
    app.render_silent();
    assert_eq!(budgets(&app), (3 << 20, 2 << 20, 5 << 20));
}
