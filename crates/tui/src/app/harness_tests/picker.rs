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
fn subagent_viewer_formats_token_totals_on_refresh() {
    let root = tempfile::tempdir().unwrap();
    let init = root.path().join("init.lua");
    std::fs::write(&init, "require('smelt.plugins.subagents')").unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    assert!(app.run_lua(
        r#"
        _G.viewer_usage = {}
        smelt.agent.runs = function()
            return { { id = 1, group = 1, session_id = 'unavailable', task = 'Review',
                status = 'running', cost_usd = 0.0123, usage = _G.viewer_usage } }
        end
        smelt.cmd.run('subagents')
    "#
    ));
    for width in [120, 45] {
        app.set_terminal_size(width, 24);
        app.render_silent();
        for (tokens, formatted) in [(0, "0"), (999, "999"), (1200, "1.2k"), (3_400_000, "3.4m")] {
            assert!(app.run_lua(&format!("_G.viewer_usage.prompt_tokens = {tokens}")));
            app.feed_one(SourceEvent::Tick(300));
            app.app.tick_timers();
            app.settle_lua();
            let text = app.render_to_frame().text();
            assert!(text.contains(&format!("{formatted} tokens")), "{text}");
            assert!(text.contains("$0.0123"), "{text}");
        }
    }
}

#[test]
fn subagent_sidebar_vim_motions_skip_headers_and_preserve_pane_focus() {
    let root = tempfile::tempdir().unwrap();
    let init = root.path().join("init.lua");
    std::fs::write(&init, "require('smelt.plugins.subagents')").unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    app.set_terminal_size(120, 32);
    assert!(app.run_lua(
        r#"
        _G.viewer_runs = {}
        for id = 1, 41 do
            _G.viewer_runs[id] = { id = id, group = math.floor(id / 2) + 1,
                session_id = 'unavailable', task = 'Review', status = 'queued', cost_usd = 0 }
        end
        smelt.agent.runs = function() return _G.viewer_runs end
        smelt.cmd.run('subagents')
    "#
    ));
    app.settle_lua();
    app.render_silent();
    let sidebar = app.ui_probe().named_win("smelt.subagents.runs").unwrap();
    let preview = app
        .ui_probe()
        .named_win("smelt.subagents.transcript")
        .unwrap();
    let selected_row = |app: &TestApp| app.ui_probe().win(sidebar).unwrap().cursor_abs_row();
    for (keys, id) in [
        ("j", 2),
        ("k", 1),
        ("2j", 3),
        ("j", 4),
        ("2k", 2),
        ("k", 1),
        ("99k", 1),
        ("99j", 41),
        ("gg", 1),
        ("G", 41),
        ("gg10j", 11),
    ] {
        app.type_text(keys);
        app.settle_lua();
        let text = app.render_to_frame().text();
        assert!(
            text.contains(&format!("agent {id} - queued")),
            "{keys}: {text}"
        );
        assert_eq!(selected_row(&app), id - 1 + id / 2, "{keys}");
        assert_eq!(app.ui_probe().focus(), Some(sidebar));
    }
    app.type_text("2");
    app.feed_one(SourceEvent::Tick(300));
    app.app.tick_timers();
    app.settle_lua();
    app.type_text("k");
    assert_eq!(selected_row(&app), 12, "counts survive a live refresh");
    app.press(KeyCode::Tab);
    app.dispatch_ui_window_events(false);
    app.type_text("j");
    assert_eq!(app.ui_probe().focus(), Some(preview));
    assert_eq!(
        selected_row(&app),
        12,
        "preview motions do not move the sidebar"
    );
    app.press(KeyCode::Tab);
    app.dispatch_ui_window_events(false);
    app.press(KeyCode::Home);
    assert_eq!(selected_row(&app), 0);
    app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL);
    let half_page = selected_row(&app);
    assert!(half_page > 0);
    app.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(selected_row(&app), 0);
    for (down, up, modifiers) in [
        (
            KeyCode::Char('f'),
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        ),
        (KeyCode::PageDown, KeyCode::PageUp, KeyModifiers::empty()),
    ] {
        app.press_mod(down, modifiers);
        assert!(selected_row(&app) > half_page);
        app.press_mod(up, modifiers);
        assert_eq!(selected_row(&app), 0);
    }
    app.set_terminal_size(45, 18);
    app.render_silent();
    app.press(KeyCode::End);
    assert_eq!(selected_row(&app), 60);
    app.type_text("gg2j");
    assert_eq!(selected_row(&app), 3);
    app.press_mod(KeyCode::Char('k'), KeyModifiers::CONTROL);
    assert_eq!(selected_row(&app), 2);
    app.press_mod(KeyCode::Char('j'), KeyModifiers::CONTROL);
    assert_eq!(selected_row(&app), 3);
    app.press(KeyCode::Up);
    assert_eq!(selected_row(&app), 2);
    app.press(KeyCode::Down);
    assert_eq!(selected_row(&app), 3);
    assert!(app.run_lua("_G.viewer_runs = {}"));
    app.feed_one(SourceEvent::Tick(300));
    app.app.tick_timers();
    app.settle_lua();
    app.type_text("99jggGk");
    let text = app.render_to_frame().text();
    assert!(text.contains("no subagents"), "{text}");
    assert!(app.run_lua("assert(smelt.prompt.text() == '')"));
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

#[tokio::test(flavor = "current_thread")]
async fn subagents_inherit_session_approvals_at_spawn_including_queued_children() {
    use super::compaction::read_json_request;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;

    let root = tempfile::tempdir().unwrap();
    let init = root.path().join("init.lua");
    std::fs::write(&init, r#"
        require('smelt.plugins.subagents').setup({ max_concurrent = 1 })
        for _, name in ipairs({ 'inherited_probe', 'late_probe', 'approval_probe' }) do
            local decision = name == 'approval_probe' and 'allow' or 'ask'
            smelt.tools.register({
                name = name, description = 'Test child session approval isolation.', effect = 'read',
                permission_defaults = { normal = decision, plan = decision, apply = decision },
                parameters = { type = 'object', properties = {} },
                execute = function()
                    if name == 'approval_probe' then return smelt.json.encode(smelt.permissions.list()) end
                    return 'executed ' .. name
                end,
            })
        end
    "#).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let engine = engine::start(
        engine::EngineConfig::new(root.path().into(), Arc::new(engine::clock::RealClock)),
        Box::new(engine::tools::EmptyDispatcher),
    );
    let mut app = TestApp::builder()
        .with_cwd(root.path())
        .with_init_lua(&init)
        .with_engine(engine)
        .build();
    app.use_model(smelt_core::config::ResolvedModel {
        key: "mock/approvals".into(),
        provider_name: "mock".into(),
        model_name: "approvals".into(),
        display_name: None,
        api_base: format!("http://{address}"),
        api_key_env: String::new(),
        provider_type: "anthropic-compatible".into(),
        config: Default::default(),
        catalog: Default::default(),
    });
    assert!(app.run_lua(&format!(r#"
        smelt.permissions.sync({{
            session = {{ {{ tool = 'inherited_probe', pattern = '*' }}, {{ tool = 'directory', pattern = {path:?} }} }},
            path_grants = {{
                {{ kind = 'path', tool = 'read_file', access = 'read', path_prefix = {path:?} }},
                {{ kind = 'path', mode = 'plan', tool = 'edit_file', access = 'write', path_prefix = {path:?} }},
            }},
        }})
    "#, path = root.path().to_str().unwrap())));
    app.start_submitted_turn("Delegate independent tasks");
    let server = tokio::spawn(async move {
        let mut results = Vec::new();
        while results.len() < 2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_json_request(&mut stream).await;
            let parts: Vec<_> = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|message| message["content"].as_array())
                .flatten()
                .collect();
            let child = parts.iter().any(|part| {
                part["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("You are a subagent."))
            });
            let has_results = parts.iter().any(|part| part["type"] == "tool_result");
            let calls = if has_results {
                if child {
                    results.push(request);
                }
                Vec::new()
            } else if child {
                vec![
                    ("inherited_probe", json!({})),
                    ("late_probe", json!({})),
                    ("approval_probe", json!({})),
                ]
            } else {
                vec![("swarm", json!({"prompt":"Test inherited approvals", "n":2}))]
            };
            let tools = !calls.is_empty();
            let mut events = vec![json!({"type":"message_start","message":{
                "id":"test","type":"message","role":"assistant","content":[],"model":"approvals",
                "usage":{"input_tokens":10,"output_tokens":1}
            }})];
            if tools {
                for (index, (name, args)) in calls.into_iter().enumerate() {
                    events.push(
                        json!({"type":"content_block_start","index":index,"content_block":{
                            "type":"tool_use","id":name,"name":name,"input":{}
                        }}),
                    );
                    events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":args.to_string()}}));
                    events.push(json!({"type":"content_block_stop","index":index}));
                }
            } else {
                events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}));
                events.push(json!({"type":"content_block_stop","index":0}));
            }
            events.push(json!({"type":"message_delta","delta":{"stop_reason":if tools {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":10}}));
            events.push(json!({"type":"message_stop"}));
            let body: String = events
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect();
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
            ).as_bytes()).await.unwrap();
        }
        results
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while app.app.core.agents.children.len() < 2 {
            let output = app
                .app
                .core
                .engine
                .recv_output()
                .await
                .expect("engine output");
            app.app
                .dispatch_selected_engine_output_in_render_loop_to(output, &mut std::io::sink());
            app.settle_lua();
        }
    })
    .await
    .expect("parent model spawns the swarm");
    app.run_lua_result(r#"
        local runs = smelt.agent.runs()
        assert(runs[1].status == 'running' and runs[2].status == 'queued')
        smelt.permissions.sync({ session = { { tool = 'late_probe', pattern = '*' } }, path_grants = {} })
    "#).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while app
            .app
            .core
            .agents
            .children
            .values()
            .any(|child| child.info.status != "completed")
        {
            let output = app
                .app
                .core
                .engine
                .recv_output()
                .await
                .expect("engine output");
            app.app
                .dispatch_selected_engine_output_in_render_loop_to(output, &mut std::io::sink());
            app.settle_lua();
        }
    })
    .await
    .expect("both running and queued children complete");
    let requests = server.await.unwrap();
    app.app.core.engine.send(protocol::UiCommand::Cancel);
    assert_eq!(requests.len(), 2);
    for request in requests {
        let results: Vec<_> = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter(|part| part["type"] == "tool_result")
            .collect();
        let result = |name: &str| {
            *results
                .iter()
                .find(|result| result["tool_use_id"] == name)
                .unwrap()
        };
        let inherited = result("inherited_probe");
        assert_ne!(
            inherited["is_error"], true,
            "spawn-time approvals survive parent revocation: {inherited}"
        );
        assert!(
            inherited["content"]
                .to_string()
                .contains("executed inherited_probe"),
            "spawn-time approvals survive parent revocation: {inherited}"
        );
        let late = result("late_probe");
        assert!(
            late["content"]
                .to_string()
                .contains("subagent requires explicit approval"),
            "later parent grants must not authorize existing children: {late}"
        );
        let probe = result("approval_probe");
        let approvals: serde_json::Value =
            serde_json::from_str(probe["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            approvals["session"].as_array().unwrap().len(),
            2,
            "{approvals}"
        );
        assert!(approvals["session"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["tool"] == "directory"));
        let paths = approvals["path_grants"].as_array().unwrap();
        assert_eq!(paths.len(), 2, "{approvals}");
        assert!(paths
            .iter()
            .any(|grant| grant["tool"] == "read_file" && grant["access"] == "read"));
        assert!(paths
            .iter()
            .any(|grant| grant["tool"] == "edit_file" && grant["mode"] == "plan"));
    }
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
