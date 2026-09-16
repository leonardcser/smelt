use super::*;

#[test]
fn subagent_picker_opens_during_parent_turn_and_escape_only_closes_viewer() {
    let root = tempfile::tempdir().unwrap();
    let init = root.path().join("init.lua");
    std::fs::write(&init, "require('smelt.plugins.subagents')").unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    app.start_turn(1);
    assert!(app.run_lua("smelt.cmd.run('subagents')"));
    drive_lua_tasks(&mut app);
    for (width, height) in [(80, 24), (40, 16), (120, 40)] {
        app.set_terminal_size(width, height);
        let frame = app.render_to_frame();
        assert!(frame.text().contains("no subagents"), "{}", frame.text());
    }
    app.press(KeyCode::Esc);
    drive_lua_tasks(&mut app);
    assert!(!app.render_to_frame().text().contains("no subagents"));
    assert!(
        app.state().agent_running,
        "Escape must not cancel the parent"
    );
    assert!(!app.quit_requested());
}

#[test]
fn subagent_viewer_displays_selected_native_transcript() {
    let root = tempfile::tempdir().unwrap();
    let init = root.path().join("init.lua");
    std::fs::write(&init, "require('smelt.plugins.subagents')").unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    app.set_terminal_size(120, 32);
    let mut session = smelt_core::session::Session::new(1, root.path().into());
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            format!(
                "subagent transcript fixture\npreview first line\n{}\nsubagent transcript fixture",
                (0..150)
                    .map(|i| format!("preview line {i}\n"))
                    .collect::<String>()
            ),
        )));
    let storage = app.app.conversation.sessions();
    storage.save_result(&session).unwrap();
    let resolved = storage
        .resolve_session_for_read_result(&session.id)
        .unwrap();
    let address = crate::app::transcript::TranscriptStoreAddress::new(
        resolved.sessions_root,
        resolved.id,
        resolved.lineage_id,
    );
    let transcript = crate::app::history::build_transcript_from_session(&app.app.lua, &session);
    crate::persist::write_transcript_record_suffix(
        &address,
        0,
        &transcript.history.block_records(),
    )
    .unwrap();
    assert!(app.run_lua(&format!("_G.subagent_fixture_id = {:?}", session.id)));
    assert!(app.run_lua(r#"
        local id = _G.subagent_fixture_id
        _G.viewer_runs = {
            { id = 1, group = 1, session_id = id, task = 'Review parser', status = 'running', cost_usd = 0,
              usage = { prompt_tokens = 100, completion_tokens = 20, cache_read_tokens = 30, cache_write_tokens = 40, reasoning_tokens = 5, context_tokens = 999 } },
            { id = 2, group = 1, session_id = id, task = 'Review parser', status = 'queued', cost_usd = 0,
              usage = { prompt_tokens = 10, completion_tokens = 5 } },
        }
        smelt.agent.runs = function() return _G.viewer_runs end
        smelt.agent.stop = function(id) _G.stopped_agent = id end
        smelt.cmd.run('subagents')
    "#));
    app.settle_lua();
    app.render_silent();
    app.feed_one(SourceEvent::Tick(300));
    app.app.tick_timers();
    app.settle_lua();
    let frame = app.render_to_frame();
    assert!(
        frame.text().contains("subagent transcript fixture"),
        "{}",
        frame.text()
    );
    assert!(
        frame.text().contains("Swarm 1: Review parser"),
        "{}",
        frame.text()
    );
    assert!(
        frame.text().contains("1 running / 1 queued"),
        "{}",
        frame.text()
    );
    let text = frame.text();
    assert!(text.contains("205 tokens"), "{text}");
    assert!(!text.contains("$0.0000"), "{text}");
    assert!(!text.contains("Tab: panes"), "{text}");
    assert!(
        text.find("1 running").unwrap() < text.find("Swarm 1").unwrap(),
        "{text}"
    );
    let row = frame
        .rows
        .iter()
        .position(|line| line.contains("#2"))
        .unwrap();
    assert_eq!(
        frame.rows[row].matches('│').count(),
        3,
        "one separator between panes: {text}"
    );
    let col =
        smelt_buffer::text::byte_to_cell(&frame.rows[row], frame.rows[row].find("#2").unwrap());
    let pending = app
        .ui_probe()
        .theme()
        .resolve(smelt_core::theme::intern("SmeltToolPending"))
        .fg;
    assert_eq!(frame.styles[row][col].fg, pending);
    app.start_turn(1);
    app.press(KeyCode::Down);
    app.settle_lua();
    assert!(app.render_to_frame().text().contains("agent 2 - queued"));
    assert!(app.run_lua("_G.viewer_runs[1].status = 'completed'; _G.viewer_runs[1].cost_usd = 0.001; _G.viewer_runs[2].cost_usd = 0.002"));
    app.feed_one(SourceEvent::Tick(300));
    app.app.tick_timers();
    app.settle_lua();
    assert!(
        app.render_to_frame().text().contains("agent 2 - queued"),
        "refresh preserves selection"
    );
    let frame = app.render_to_frame();
    assert!(frame.text().contains("$0.0030"), "{}", frame.text());
    assert!(frame.text().contains("$0.0010"), "{}", frame.text());
    let row = frame
        .rows
        .iter()
        .position(|line| line.contains("#1"))
        .unwrap();
    let col =
        smelt_buffer::text::byte_to_cell(&frame.rows[row], frame.rows[row].find("#1").unwrap());
    assert_ne!(
        frame.styles[row][col].fg, pending,
        "completed runs are no longer dimmed"
    );
    app.press_mod(KeyCode::Char('s'), KeyModifiers::ALT);
    app.settle_lua();
    assert!(app.run_lua("assert(_G.stopped_agent == 2)"));
    app.press(KeyCode::Tab);
    app.type_text("ggV");
    app.feed_one(SourceEvent::Tick(300));
    app.app.tick_timers();
    app.settle_lua();
    app.type_text("G");
    let preview_win = app
        .ui_probe()
        .named_win("smelt.subagents.transcript")
        .unwrap();
    let preview_state = app
        .ui_probe()
        .win(preview_win)
        .unwrap()
        .document_view_state();
    assert!(
        preview_state.cursor.row >= 150,
        "preview cursor after G: {preview_state:?}"
    );
    assert!(
        app.app.session_preview_is_attached_to(preview_win),
        "preview binding after G: {preview_state:?}"
    );
    app.render_silent();
    let preview_state = app
        .ui_probe()
        .win(preview_win)
        .unwrap()
        .document_view_state();
    assert!(
        app.app.session_preview_is_attached_to(preview_win),
        "preview binding after render: {preview_state:?}"
    );
    assert!(
        preview_state.cursor.row >= 150,
        "preview cursor after render: {preview_state:?}"
    );
    app.type_text("y");
    let copied = app.core_probe().clipboard.kill_ring.current().to_owned();
    assert!(
        copied.contains("preview first line"),
        "keyboard yank must copy off-screen rows: {copied:?}"
    );
    assert!(
        copied.contains("preview line 149"),
        "keyboard yank must include the end of the preview: {copied:?}"
    );
    assert!(
        copied.contains("subagent transcript fixture"),
        "keyboard yank must copy the preview transcript: {copied:?}"
    );
    let frame = app.render_to_frame();
    let row = frame
        .rows
        .iter()
        .position(|line| line.contains("subagent transcript fixture"))
        .unwrap_or_else(|| panic!("preview after copying:\n{}", frame.text()));
    let col = smelt_buffer::text::byte_to_cell(
        &frame.rows[row],
        frame.rows[row].find("subagent transcript fixture").unwrap(),
    );
    app.press(KeyCode::Tab);
    app.dispatch_ui_window_events(false);
    app.render_silent();
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), col),
        (
            MouseEventKind::Drag(MouseButton::Left),
            col + "subagent transcript fixture".len(),
        ),
        (
            MouseEventKind::Up(MouseButton::Left),
            col + "subagent transcript fixture".len(),
        ),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: row as u16,
            column: column as u16,
            modifiers: KeyModifiers::empty(),
        })));
    }
    assert_eq!(
        app.core_probe().clipboard.kill_ring.current().trim(),
        "subagent transcript fixture"
    );
    app.dispatch_ui_window_events(false);
    app.render_silent();
    app.settle_lua();
    assert_eq!(app.ui_probe().focus(), Some(preview_win));
    app.press(KeyCode::Tab);
    app.settle_lua();
    app.press(KeyCode::Up);
    app.settle_lua();
    let frame = app.render_to_frame();
    assert!(
        frame.text().contains("agent 1 - completed"),
        "Tab follows mouse focus back to the run list:\n{}",
        frame.text()
    );
    app.press(KeyCode::Tab);
    app.type_text("must not edit");
    assert!(app.run_lua("assert(smelt.prompt.text() == '')"));
    app.set_terminal_size(45, 18);
    app.render_silent();
    app.press(KeyCode::Enter);
    app.settle_lua();
    app.render_silent();
    app.feed_one(SourceEvent::Tick(300));
    app.app.tick_timers();
    app.settle_lua();
    let frame = app.render_to_frame();
    assert!(
        frame.text().contains("preview line")
            || frame.text().contains("subagent transcript fixture"),
        "{}",
        frame.text()
    );
    assert!(frame.text().contains("205 tokens"), "{}", frame.text());
    assert!(frame.text().contains("$0.0030"), "{}", frame.text());
    app.press(KeyCode::Esc);
    app.settle_lua();
    assert!(app.render_to_frame().text().contains("Review parser"));
    app.press(KeyCode::Esc);
    app.settle_lua();
    assert!(app.state().agent_running);
    assert!(app.state().active_modal.is_none());
    app.press_mod(KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert!(app.run_lua("assert(smelt.prompt.text():find('subagent transcript fixture', 1, true))"));
}

#[test]
fn subagent_viewer_shows_preview_errors() {
    let root = tempfile::tempdir().unwrap();
    let init = root.path().join("init.lua");
    std::fs::write(&init, "require('smelt.plugins.subagents')").unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    app.set_terminal_size(120, 32);
    assert!(app.run_lua(r#"
        smelt.agent.runs = function()
            return { { id = 1, group = 1, session_id = 'unavailable', task = 'Review', status = 'running', cost_usd = 0 } }
        end
        smelt.session.render_preview_into = function() error('preview fixture failure') end
        smelt.cmd.run('subagents')
    "#));
    app.settle_lua();
    app.render_silent();
    app.feed_one(SourceEvent::Tick(300));
    app.app.tick_timers();
    app.settle_lua();
    let frame = app.render_to_frame();
    assert!(
        frame.text().contains("Transcript unavailable:"),
        "{}",
        frame.text()
    );
    assert!(
        frame.text().contains("preview fixture failure"),
        "{}",
        frame.text()
    );
    app.press(KeyCode::Esc);
    app.settle_lua();
    assert!(app.state().active_modal.is_none());
}

#[test]
fn picker_open_focuses_overlay() {
    let mut app = TestApp::builder().build();
    let leaf = open_test_picker(&mut app, &["one", "two", "three"], 0);
    let s = app.state();
    assert!(s.focused_overlay.is_some());
    assert_eq!(app.ui_probe().focus(), Some(leaf));
}

#[test]
fn prompt_picker_ctrl_c_dismisses_before_idle_quit() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
            _G.prompt_picker_dismissed = false
            _G.prompt_picker_on_dismiss = 0
            smelt.spawn(function()
                local result = smelt.picker.open({
                    placement = "prompt_docked",
                    items = {
                        { label = "alpha" },
                        { label = "beta" },
                    },
                    on_dismiss = function()
                        _G.prompt_picker_on_dismiss = _G.prompt_picker_on_dismiss + 1
                    end,
                })
                _G.prompt_picker_dismissed = result == nil
            end)
        "#,
    ));
    drive_lua_tasks(&mut app);
    assert!(
        app.overlays_probe().has_pickers(),
        "prompt picker should open"
    );

    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    drive_lua_tasks(&mut app);

    assert!(!app.quit_requested(), "first Ctrl-C should not quit");
    assert!(
        !app.overlays_probe().has_pickers(),
        "first Ctrl-C should dismiss picker"
    );
    assert!(app.run_lua(r#"assert(_G.prompt_picker_dismissed == true)"#));
    assert!(app.run_lua(r#"assert(_G.prompt_picker_on_dismiss == 1)"#));

    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(
        app.quit_requested(),
        "second Ctrl-C after dismissal should quit"
    );
}

#[test]
fn prompt_picker_esc_fires_on_dismiss() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
            _G.prompt_picker_esc_dismissed = false
            _G.prompt_picker_esc_on_dismiss = 0
            smelt.spawn(function()
                local result = smelt.picker.open({
                    placement = "prompt_docked",
                    items = {
                        { label = "alpha" },
                        { label = "beta" },
                    },
                    on_dismiss = function()
                        _G.prompt_picker_esc_on_dismiss = _G.prompt_picker_esc_on_dismiss + 1
                    end,
                })
                _G.prompt_picker_esc_dismissed = result == nil
            end)
        "#,
    ));
    drive_lua_tasks(&mut app);
    assert!(
        app.overlays_probe().has_pickers(),
        "prompt picker should open"
    );

    app.press(KeyCode::Esc);
    drive_lua_tasks(&mut app);

    assert!(
        !app.overlays_probe().has_pickers(),
        "Esc should dismiss picker"
    );
    assert!(app.run_lua(r#"assert(_G.prompt_picker_esc_dismissed == true)"#));
    assert!(app.run_lua(r#"assert(_G.prompt_picker_esc_on_dismiss == 1)"#));
}

#[test]
fn floating_picker_ctrl_c_dismisses_before_idle_quit() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
            _G.floating_picker_dismissed = false
            _G.floating_picker_on_dismiss = 0
            smelt.spawn(function()
                local result = smelt.picker.open({
                    items = {
                        { label = "alpha" },
                        { label = "beta" },
                    },
                    placement = "center",
                    on_dismiss = function()
                        _G.floating_picker_on_dismiss = _G.floating_picker_on_dismiss + 1
                    end,
                })
                _G.floating_picker_dismissed = result == nil
            end)
        "#,
    ));
    drive_lua_tasks(&mut app);
    assert!(
        app.overlays_probe().has_pickers(),
        "floating picker should open"
    );

    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    drive_lua_tasks(&mut app);

    assert!(!app.quit_requested(), "first Ctrl-C should not quit");
    assert!(
        !app.overlays_probe().has_pickers(),
        "first Ctrl-C should dismiss picker"
    );
    assert!(app.run_lua(r#"assert(_G.floating_picker_dismissed == true)"#));
    assert!(app.run_lua(r#"assert(_G.floating_picker_on_dismiss == 1)"#));

    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(
        app.quit_requested(),
        "second Ctrl-C after dismissal should quit"
    );
}

#[test]
fn floating_picker_accepts_original_string_item_and_reports_selection() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
            _G.floating_picker_selected = {}
            _G.floating_picker_result = nil
            smelt.spawn(function()
                _G.floating_picker_result = smelt.picker.open({
                    items = { "alpha", "beta" },
                    placement = "center",
                    on_select = function(item)
                        table.insert(_G.floating_picker_selected, item)
                    end,
                })
            end)
        "#,
    ));
    drive_lua_tasks(&mut app);
    assert!(app.run_lua(r#"assert(_G.floating_picker_selected[1] == "alpha")"#));

    app.press(KeyCode::Down);
    drive_lua_tasks(&mut app);
    assert!(app.run_lua(r#"assert(_G.floating_picker_selected[2] == "beta")"#));

    app.press(KeyCode::Enter);
    drive_lua_tasks(&mut app);
    assert!(app.run_lua(
        r#"
            assert(_G.floating_picker_result.index == 2)
            assert(_G.floating_picker_result.item == "beta")
            assert(_G.floating_picker_result.action == "enter")
        "#,
    ));
}

#[test]
fn picker_open_renders_items_into_buffer() {
    let mut app = TestApp::builder().build();
    let leaf = open_test_picker(&mut app, &["alpha", "beta", "gamma"], 0);
    let lines = picker_buffer_lines(&app, leaf);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("alpha"));
    assert!(lines[1].contains("beta"));
    assert!(lines[2].contains("gamma"));
}

#[test]
fn picker_set_items_replaces_buffer_contents() {
    let mut app = TestApp::builder().build();
    let leaf = open_test_picker(&mut app, &["foo", "bar"], 0);
    let new_items: Vec<_> = ["x", "y", "z"]
        .iter()
        .map(|s| crate::picker::PickerItem::new(*s))
        .collect();
    app.set_picker_items(leaf, new_items, 0);
    let lines = picker_buffer_lines(&app, leaf);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("x"));
    assert!(lines[2].contains("z"));
}

#[test]
fn picker_set_selected_moves_cursor() {
    let mut app = TestApp::builder().build();
    let leaf = open_test_picker(&mut app, &["a", "b", "c", "d"], 0);
    let initial_cpos = app.ui_probe().win(leaf).map(|w| w.cpos()).unwrap_or(0);

    app.set_picker_selected(leaf, 2);
    let new_cpos = app.ui_probe().win(leaf).map(|w| w.cpos()).unwrap_or(0);
    assert_ne!(initial_cpos, new_cpos, "cursor moved with selection");
}

#[test]
fn picker_wheel_pans_viewport_when_unfocused() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut app = TestApp::builder().build();
    let items: Vec<crate::picker::PickerItem> = (0..40)
        .map(|i| crate::picker::PickerItem::new(format!("item {i}")))
        .collect();
    let leaf = app
        .open_picker(
            items,
            0,
            crate::picker::PickerPlacement::ScreenCenter,
            false, // non-focusable: focus stays on prompt
            false,
            10,
        )
        .expect("picker leaf created");

    // Render to populate the viewport.
    app.render();
    assert_eq!(app.ui_probe().win(leaf).map(|w| w.scroll_top()), Some(0));

    let leaf_rect = app
        .paint_rect(crate::smelt_edit::PaintId::from(leaf))
        .expect("picker leaf has a rect after render");
    // Pick a cell inside the picker rect.
    let row = leaf_rect.top + 1;
    let col = leaf_rect.left + 1;

    let scroll = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        row,
        column: col,
        modifiers: crossterm::event::KeyModifiers::empty(),
    };
    let _ = scroll; // silence unused-warning if path below ignores it
    let _ = MouseButton::Left;

    let pre_scroll = app.ui_probe().win(leaf).unwrap().scroll_top();
    let _ = app.scroll_at(row, col, 3);
    let post_scroll = app.ui_probe().win(leaf).unwrap().scroll_top();
    assert!(
        post_scroll > pre_scroll,
        "wheel over unfocused picker must pan scroll_top (pre={pre_scroll}, post={post_scroll})",
    );
}

#[test]
fn picker_forget_drops_state() {
    let mut app = TestApp::builder().build();
    let leaf = open_test_picker(&mut app, &["a", "b"], 0);
    assert!(app.overlays_probe().has_picker(leaf));

    app.forget_picker(leaf);
    assert!(!app.overlays_probe().has_picker(leaf));
}

#[test]
fn picker_filter_workflow_via_set_items() {
    let mut app = TestApp::builder().build();
    let leaf = open_test_picker(&mut app, &["apple", "apricot", "banana", "cherry"], 0);
    assert_eq!(picker_buffer_lines(&app, leaf).len(), 4);

    // Simulate "filter as user types": narrow set_items, then narrow again.
    let filtered: Vec<_> = ["apple", "apricot"]
        .iter()
        .map(|s| crate::picker::PickerItem::new(*s))
        .collect();
    app.set_picker_items(leaf, filtered, 0);
    assert_eq!(picker_buffer_lines(&app, leaf).len(), 2);

    let single: Vec<_> = ["apple"]
        .iter()
        .map(|s| crate::picker::PickerItem::new(*s))
        .collect();
    app.set_picker_items(leaf, single, 0);
    let lines = picker_buffer_lines(&app, leaf);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("apple"));
}

#[test]
fn prompt_docked_picker_clamps_height_to_headroom() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 6);

    let items: Vec<crate::picker::PickerItem> = (0..40)
        .map(|i| crate::picker::PickerItem::new(format!("item {i}")))
        .collect();
    let leaf = app
        .open_picker(
            items,
            0,
            crate::picker::PickerPlacement::PromptDocked { max_rows: 8 },
            false,
            false,
            30,
        )
        .expect("picker leaf created");

    app.render();

    let picker_rect = app
        .paint_rect(crate::smelt_edit::PaintId::from(leaf))
        .expect("picker has a rect");
    let prompt_rect = app
        .split_rect(crate::app::PROMPT_WIN)
        .expect("prompt has a rect");

    assert!(
        picker_rect.top + picker_rect.height <= prompt_rect.top,
        "picker at {picker_rect:?} overlaps prompt at {prompt_rect:?}"
    );

    // The picker should be clamped below its 8-row desired cap when the
    // terminal is short; the exact height depends on chrome, so the real
    // invariant is the non-overlap check above.
    assert!(
        picker_rect.height <= 8,
        "picker height {} should not exceed the requested cap",
        picker_rect.height
    );
}

#[test]
fn prompt_docked_picker_relayouts_on_resize() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);

    let items: Vec<crate::picker::PickerItem> = (0..40)
        .map(|i| crate::picker::PickerItem::new(format!("item {i}")))
        .collect();
    let leaf = app
        .open_picker(
            items,
            0,
            crate::picker::PickerPlacement::PromptDocked { max_rows: 8 },
            false,
            false,
            30,
        )
        .expect("picker leaf created");

    app.render();
    let tall_rect = app
        .paint_rect(crate::smelt_edit::PaintId::from(leaf))
        .expect("picker has a rect");
    assert_eq!(tall_rect.height, 8);

    app.set_terminal_size(80, 6);
    app.render();
    let short_rect = app
        .paint_rect(crate::smelt_edit::PaintId::from(leaf))
        .expect("picker has a rect");

    assert!(
        short_rect.height < tall_rect.height,
        "picker should shrink after resize: tall={tall_rect:?}, short={short_rect:?}"
    );
    let prompt_rect = app
        .split_rect(crate::app::PROMPT_WIN)
        .expect("prompt has a rect");
    assert!(
        short_rect.top + short_rect.height <= prompt_rect.top,
        "shrunk picker at {short_rect:?} overlaps prompt at {prompt_rect:?}"
    );
}

#[test]
fn theme_picker_confirms_selection_without_callback_error() {
    let mut app = TestApp::builder().build();

    app.type_text("/theme");
    app.press(KeyCode::Enter);
    drive_lua_tasks(&mut app);
    assert!(
        app.overlays_probe().has_pickers(),
        "theme picker should open"
    );

    app.press(KeyCode::Enter);
    drive_lua_tasks(&mut app);

    assert!(app.lua_messages_contain("theme preview selected for this session:"));
    assert!(!app.lua_messages_contain("cmd.register_picker on_enter:"));
}

#[test]
fn prompt_picker_custom_rank_uses_returned_indices() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
            _G.prompt_picker_rank_result = nil
            _G.prompt_picker_rank_calls = 0
            smelt.spawn(function()
                local result = smelt.picker.open({
                    placement = "prompt_docked",
                    items = {
                        { label = "alpha" },
                        { label = "beta" },
                        { label = "gamma" },
                    },
                    rank = function(items, query, original)
                        _G.prompt_picker_rank_calls = _G.prompt_picker_rank_calls + 1
                        assert(#items == 3)
                        assert(query == "")
                        assert(original[1].label == "alpha")
                        return { 3, {}, 1, 99 }
                    end,
                })
                if result then
                    _G.prompt_picker_rank_result = result.item.label .. ":" .. tostring(result.index)
                end
            end)
        "#,
    ));

    drive_lua_tasks(&mut app);
    app.press(KeyCode::Enter);
    drive_lua_tasks(&mut app);

    assert!(app.run_lua(
        r#"
            assert(_G.prompt_picker_rank_calls >= 1)
            assert(_G.prompt_picker_rank_result == "gamma:3")
        "#,
    ));
}
