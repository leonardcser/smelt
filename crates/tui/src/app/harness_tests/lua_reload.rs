use super::*;
use crate::test_support::ProcessCwdGuard;

#[test]
fn lua_config_auto_reload_success_notifies_for_real_edits() {
    let mut app = TestApp::builder().build();
    assert!(app.state().notification.is_none());

    crate::lua::with_app_ptr(&mut app.app, |app| {
        app.reload_lua_config();
    });

    assert!(app.state().notification.is_some());
}

#[test]
fn manual_lua_reload_success_notifies() {
    let mut app = TestApp::builder().build();

    crate::lua::with_app_ptr(&mut app.app, |app| {
        app.reload_lua();
    });

    assert!(app.state().notification.is_some());
}

#[test]
fn lua_config_session_and_transcript_contracts_are_available() {
    let mut app = TestApp::builder().build();
    app.push_user_block("hello from lua api test");
    app.app.push_block(smelt_core::Block::Text {
        content: "assistant line".into(),
    });
    app.app.render_normal_to(&mut std::io::sink());

    assert!(app.run_lua(
        r#"
            assert(type(smelt.config.provider_type()) == "string")
            assert(type(smelt.config.api_base()) == "string")
            assert(type(smelt.config.api_key_env()) == "string")
            assert(type(smelt.config.model_config()) == "table")

            assert(smelt.session.title() == nil)
            smelt.session.title("Lua Contract Title", "lua-contract-title")
            assert(smelt.session.title() == "Lua Contract Title")
            assert(smelt.session.slug() == "lua-contract-title")
            assert(type(smelt.session.cwd()) == "string")
            assert(type(smelt.session.tokens()) == "table")
            local info = smelt.session.info()
            assert(type(info.id) == "string")
            assert(info.id == smelt.session.id())
            assert(info.title == "Lua Contract Title")
            assert(type(info.dir) == "string")
            assert(type(info.cwd) == "string")
            assert(type(info.tokens) == "table")
            assert(type(info.worktree) == "table")

            assert(smelt.transcript.is_empty() == false)
            local text = smelt.transcript.text()
            assert(text:find("hello from lua api test", 1, true))
            local blocks = smelt.transcript.blocks()
            assert(#blocks >= 1)
            assert(type(blocks[1].idx) == "number")
            assert(type(blocks[1].role) == "string")
            assert(type(smelt.transcript.rows(0, 2)) == "table")

            local defaults = require("smelt.transcript.defaults")
            assert(defaults.display_count_text({ output = { metadata = { display_count = { value = 0, unit = "file" } } } }) == "0 files")
            assert(defaults.display_count_text({ output = { metadata = { display_count = { value = 1, unit = "match" } } } }) == "1 match")
            assert(defaults.display_count_text({ output = { metadata = { display_count = { value = 2, unit = "output line" } } } }) == "2 output lines")
            assert(defaults.display_count_text({}, { unit = "file" }) == "0 files")
        "#,
    ));
}

#[test]
fn lua_context_note_updates_named_history_notes_independently() {
    let mut app = TestApp::builder().build();
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::User {
            content: protocol::Content::text("hello"),
            display: None,
        });

    assert!(app.run_lua(
        r#"
            smelt.session.context_note("goal", "first goal")
            smelt.session.context_note("cwd", "custom cwd")
            smelt.session.context_note("goal", "second goal")
        "#,
    ));

    let history = &app.app.core.session.history;
    assert!(history.iter().any(|item| matches!(
        item,
        protocol::HistoryItem::Note(note)
            if note.context_name() == Some("goal") && note.text() == "second goal"
    )));
    assert!(history.iter().any(|item| matches!(
        item,
        protocol::HistoryItem::Note(note)
            if note.context_name() == Some("cwd") && note.text() == "custom cwd"
    )));
    assert_eq!(
        history
            .iter()
            .filter_map(protocol::HistoryItem::as_note)
            .filter(|note| note.kind() == protocol::HistoryNoteKind::Context)
            .count(),
        2
    );

    assert!(app.run_lua(r#"smelt.session.context_note("goal", nil)"#));
    let history = &app.app.core.session.history;
    assert!(!history.iter().any(|item| matches!(
        item,
        protocol::HistoryItem::Note(note) if note.context_name() == Some("goal")
    )));
    assert!(history.iter().any(|item| matches!(
        item,
        protocol::HistoryItem::Note(note)
            if note.context_name() == Some("cwd") && note.text() == "custom cwd"
    )));
}

#[test]
fn lua_goal_module_persists_and_updates_session_goal() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            local created = assert(goal.create("finish named context notes", { auto_continue = false }))
            assert(created.id ~= nil and created.id ~= "")
            assert(type(created.created_at_ms) == "number")
            assert(goal.current().objective == "finish named context notes")
            assert(goal.current().state == "active")
            assert(goal.update_status({ summary = "Context notes", progress = "Phase 1/2" }))
            assert(goal.current().summary == "Context notes")
            assert(goal.current().progress.label == "Phase 1/2")
            assert(goal.describe():find("Auto%-continue: off"))
            assert(goal.describe():find("State: active", 1, true))
            assert(goal.describe():find("Summary: Context notes", 1, true))
            assert(goal.describe():find("Progress: Phase 1/2", 1, true))
            assert(goal.describe():find("ID:"))
            assert(goal.pause())
            assert(goal.current().state == "paused")
            assert(goal.status_text():find("goal paused", 1, true))
            assert(goal.block("waiting for input"))
            assert(goal.current().state == "blocked")
            assert(goal.current().auto_continue == false)
            assert(goal.current().reason == "waiting for input")
            assert(goal.status_text():find("goal blocked", 1, true))
            assert(goal.complete())
            assert(type(goal.current().completed_at_ms) == "number")
            assert(goal.status_text() == nil)
            goal.clear()
            assert(goal.current() == nil)
        "#,
    ));
}

#[test]
fn lua_goal_tools_limit_model_updates_to_done_or_blocked() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(r#"require("smelt.goal").setup()"#));
    let tools = app.app.lua.tool_defs(
        protocol::AgentMode::normal(),
        smelt_core::lua::ToolVisibility::Interactive,
    );
    let create = tools
        .iter()
        .find(|tool| tool.name == "create_goal")
        .expect("create_goal should be registered");
    assert!(create
        .description
        .contains("system/developer instructions; do not infer goals from ordinary tasks"));

    let update = tools
        .iter()
        .find(|tool| tool.name == "update_goal")
        .expect("update_goal should be registered");
    assert!(update
        .description
        .contains("cannot pause, resume, clear, or rewrite"));
    assert_eq!(
        update.parameters["properties"]["state"]["enum"],
        serde_json::json!(["done", "blocked"])
    );
    assert_eq!(update.parameters["required"], serde_json::json!(["state"]));

    let status = tools
        .iter()
        .find(|tool| tool.name == "update_goal_progress")
        .expect("update_goal_progress should be registered");
    assert!(status.description.contains("durable goal progress"));
    assert!(status.parameters["properties"].get("activity").is_none());
    assert!(status.parameters["properties"].get("summary").is_none());
    assert!(status.parameters["properties"]["progress"]
        .get("properties")
        .is_some());
    assert_eq!(
        status.parameters["required"],
        serde_json::json!(["progress"])
    );
}

#[test]
fn lua_goal_auto_continue_scheduled_during_turn_starts_when_idle() {
    let mut app = TestApp::builder().build();
    let _ = app.drain_engine_sends();

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            smelt.engine.is_running = function() return _G.__goal_running == true end
            smelt.engine.submit_command = function(name, body, _overrides, display)
                _G.__goal_submit = { name = name, body = body, display = display }
            end
            _G.__goal_running = true
            assert(goal.create("finish <the> & goal", { auto_continue = true }))
            goal.schedule_auto_continue()
            _G.__goal_running = false
        "#,
    ));

    app.feed_one(SourceEvent::Tick(1300));
    app.app.tick_timers();
    assert!(app.run_lua(
        r##"
            assert(_G.__goal_submit.name == "goal")
            assert(_G.__goal_submit.display == "goal continue")
            assert(_G.__goal_submit.body:find("# Continue goal", 1, true))
            assert(_G.__goal_submit.body:find("finish &lt;the&gt; &amp; goal", 1, true))
            assert(_G.__goal_submit.body:find("Update goal progress only when entering", 1, true))
            assert(_G.__goal_submit.body:find("Progress labels may include a brief stage description", 1, true))
            assert(_G.__goal_submit.body:find("moment-to-moment activity", 1, true))
            assert(_G.__goal_submit.body:find("routine substeps", 1, true))
            assert(_G.__goal_submit.body:find("state=\"done\"", 1, true))
            assert(_G.__goal_submit.body:find("state=\"blocked\"", 1, true))
        "##,
    ));
}

#[test]
fn lua_switch_cwd_updates_runtime_state_and_engine_cwd() {
    let home_guard = test_home_guard();
    let target_dir = tempfile::TempDir::new().expect("create switch cwd tempdir");
    let _cwd_guard = ProcessCwdGuard::capture();
    let mut app = TestApp::builder().build_with_test_home_guard(&home_guard);
    let target = std::fs::canonicalize(target_dir.path()).expect("canonical target cwd");
    let expected = target.to_string_lossy().into_owned();

    app.app
        .lua
        .lua
        .globals()
        .set("__switch_cwd_target", expected.clone())
        .expect("set Lua target cwd");
    let _ = app.drain_engine_sends();

    assert!(app.run_lua(
        r#"
            local out = smelt.session.switch_cwd(_G.__switch_cwd_target)
            assert(out.cwd == _G.__switch_cwd_target)
            assert(smelt.session.cwd() == _G.__switch_cwd_target)
        "#,
    ));

    assert_eq!(app.app.cwd, expected);
    assert_eq!(app.app.core.session.cwd.as_deref(), Some(expected.as_str()));
    assert_eq!(app.app.core.env.cwd(), target);
    assert_eq!(std::env::current_dir().expect("process cwd"), target);
    assert_eq!(std::env::var_os("PWD").as_deref(), Some(target.as_os_str()));
    assert_eq!(
        app.app.core.signals.get::<String>("cwd").as_deref(),
        Some(expected.as_str())
    );
    assert!(app
        .app
        .pending_history_appends
        .iter()
        .any(|pending| matches!(
            pending.history_item(),
            protocol::HistoryItem::Note(protocol::HistoryNote::Context { ref text, .. })
                if text == &format!("Current working directory: {expected}.")
        )));
    assert!(app.drain_engine_sends().into_iter().any(|cmd| matches!(
        cmd,
        protocol::UiCommand::SetCwd { cwd } if cwd == expected
    )));
}

#[test]
fn loading_session_restores_persisted_cwd() {
    let home_guard = test_home_guard();
    let target_dir = tempfile::TempDir::new().expect("create resumed cwd tempdir");
    let _cwd_guard = ProcessCwdGuard::capture();
    let mut app = TestApp::builder().build_with_test_home_guard(&home_guard);
    let target = std::fs::canonicalize(target_dir.path()).expect("canonical target cwd");
    let expected = target.to_string_lossy().into_owned();
    let _ = app.drain_engine_sends();

    let mut session = smelt_core::session::Session::new(app.app.core.env.pid(), target.clone());
    session.id = "resumed-cwd-session".into();
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            "hello",
        )));

    app.app.load_session(session);

    assert_eq!(app.app.cwd, expected);
    assert_eq!(app.app.core.session.cwd.as_deref(), Some(expected.as_str()));
    assert_eq!(app.app.core.env.cwd(), target);
    assert_eq!(std::env::current_dir().expect("process cwd"), target);
    assert_eq!(std::env::var_os("PWD").as_deref(), Some(target.as_os_str()));
    assert_eq!(
        app.app.core.signals.get::<String>("cwd").as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(app.app.deferred_session_load, None);
    assert!(app.drain_engine_sends().into_iter().any(|cmd| matches!(
        cmd,
        protocol::UiCommand::SetCwd { cwd } if cwd == expected
    )));

    let display_target_dir = tempfile::TempDir::new().expect("create display-only cwd tempdir");
    let display_target =
        std::fs::canonicalize(display_target_dir.path()).expect("canonical display-only cwd");
    let display_expected = display_target.to_string_lossy().into_owned();
    let mut display_session =
        smelt_core::session::Session::new(app.app.core.env.pid(), display_target.clone());
    display_session.id = "display-only-cwd-session".into();
    let transcript = smelt_core::content::transcript::Transcript::new();

    app.app.load_session_display_only(
        display_session,
        transcript,
        "full-display-only-cwd-session".into(),
    );

    assert_eq!(app.app.cwd, display_expected);
    assert_eq!(
        app.app.core.session.cwd.as_deref(),
        Some(display_expected.as_str())
    );
    assert_eq!(app.app.core.env.cwd(), display_target);
    assert_eq!(
        app.app.deferred_session_load.as_deref(),
        Some("full-display-only-cwd-session")
    );
    assert!(app.drain_engine_sends().into_iter().any(|cmd| matches!(
        cmd,
        protocol::UiCommand::SetCwd { cwd } if cwd == display_expected
    )));

    let missing_dir = tempfile::TempDir::new().expect("create missing cwd tempdir");
    let missing_path = missing_dir.path().join("gone");
    std::fs::create_dir(&missing_path).expect("create missing cwd before deletion");
    let missing_path = std::fs::canonicalize(&missing_path).expect("canonical missing cwd");
    drop(missing_dir);
    let fallback = app.app.cwd.clone();
    let fallback_path = app.app.core.env.cwd();
    let mut missing_session =
        smelt_core::session::Session::new(app.app.core.env.pid(), missing_path.clone());
    missing_session.id = "missing-cwd-session".into();

    app.app.load_session(missing_session);

    assert_eq!(app.app.cwd, fallback);
    assert_eq!(app.app.core.session.cwd.as_deref(), Some(fallback.as_str()));
    assert_eq!(app.app.core.env.cwd(), fallback_path);
    assert!(app
        .app
        .notification
        .as_ref()
        .is_some_and(|n| n.summary.contains("session cwd unavailable")));
}

#[test]
fn lua_session_context_tokens_stays_visible_while_turn_history_is_ahead_of_baseline() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua("assert(smelt.session.context_tokens() == nil)"));

    let completed_history = vec![
        protocol::HistoryItem::user(protocol::Content::text("u1")),
        protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("a1")),
            None,
            vec![],
        )),
    ];

    app.start_turn(1);
    app.feed_one(SourceEvent::Engine(EngineEvent::TokenUsage {
        usage: protocol::TokenUsage {
            context_tokens: Some(123),
            ..Default::default()
        },
        tokens_per_sec: None,
        cost_usd: None,
        background: false,
    }));
    app.feed_one(SourceEvent::Engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: completed_history.clone(),
        meta: None,
    }));
    assert_eq!(app.app.core.session.current_context_tokens(), Some(123));
    assert!(app.run_lua("assert(smelt.session.context_tokens() == 123)"));

    let mut in_flight_history = completed_history;
    in_flight_history.push(protocol::HistoryItem::user(protocol::Content::text("u2")));
    app.start_turn(2);
    app.feed_one(SourceEvent::Engine(EngineEvent::HistoryUpdated {
        turn_id: 2,
        history: in_flight_history,
    }));

    assert_eq!(app.app.core.session.current_context_tokens(), None);
    assert_eq!(app.app.core.session.display_context_tokens(), Some(123));
    assert!(app.run_lua("assert(smelt.session.context_tokens() == 123)"));
}

#[test]
fn lua_history_entries_and_search_return_sequences() {
    let mut app = TestApp::builder().build();
    app.app.input_history.push("first prompt".into());
    app.app.input_history.push("older bun prompt".into());
    app.app.input_history.push("newer bun prompt".into());
    app.app.input_history.push("second search target".into());

    assert!(app.run_lua(
        r#"
            local entries = smelt.history.entries()
            assert(#entries >= 2)
            assert(entries[#entries] == "second search target")
            local matches = smelt.history.search("target")
            assert(#matches >= 1)
            assert(type(matches[1].index) == "number")
            assert(type(matches[1].score) == "number")
            local bun_matches = smelt.history.search("bun")
            assert(#bun_matches >= 2)
            assert(entries[bun_matches[1].index] == "newer bun prompt")
        "#,
    ));
}

#[test]
fn lua_model_settings_metrics_and_render_contracts_are_available() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r##"
            local model = smelt.model()
            assert(type(model) == "string")
            local models = smelt.model.list()
            assert(type(models) == "table")
            if #models > 0 then
                assert(type(models[1].key) == "string")
                assert(type(models[1].name) == "string")
                assert(type(models[1].provider_type) == "string")
            end
            local pricing = smelt.model.pricing()
            assert(type(pricing.input) == "number")
            assert(type(pricing.output) == "number")
            assert(type(pricing.cache_read) == "number")
            assert(type(pricing.cache_write) == "number")
            assert(type(pricing.source) == "string")
            local max_tokens = smelt.model.max_tokens()
            assert(max_tokens == nil or type(max_tokens) == "number")

            local original = smelt.settings.show_slug
            assert(type(original) == "boolean")
            smelt.settings.show_slug = not original
            assert(smelt.settings.show_slug == not original)
            smelt.settings.show_slug = original
            local schema = smelt.settings.schema()
            assert(#schema > 0)
            assert(type(schema[1].key) == "string")
            assert(type(schema[1].kind) == "string")
            assert(not pcall(function() return smelt.settings.not_a_real_setting end))
            assert(not pcall(function() smelt.settings.show_slug = "bad" end))

            smelt.metrics.perf.clear()
            smelt.metrics.perf.set_enabled(true)
            local perf = smelt.metrics.perf.snapshot()
            assert(perf.enabled == true)
            assert(type(perf.durations) == "table")
            assert(type(perf.values) == "table")
            smelt.metrics.perf.set_enabled(false)
            assert(smelt.metrics.perf.snapshot().enabled == false)
            assert(type(smelt.metrics.entries()) == "table")

            local b = smelt.buf.new({ name = "coverage.render.text" })
            smelt.render.text(b, "alpha\nbeta", { width = 20, hl_group = "Normal" })
            assert(b:line(1) == "alpha")
            assert(b:line(2) == "beta")

            local md = smelt.buf.new({ name = "coverage.render.markdown" })
            smelt.render.markdown(md, "# Title")
            assert(type(md:line(1)) == "string")

            local code = smelt.buf.new({ name = "coverage.render.syntax" })
            smelt.render.syntax(code, { content = "fn main() {}", lang = "rust" })
            assert(type(code:line(1)) == "string")

            local left = smelt.buf.new({ name = "coverage.render.diff.left" })
            local right = smelt.buf.new({ name = "coverage.render.diff.right" })
            smelt.render.diff_split(left, right, { old = "one\ntwo", new = "one\nthree", lang = "text" })
            assert(left:line(1) ~= nil)
            assert(right:line(1) ~= nil)
        "##,
    ));
}

#[test]
fn lua_buf_win_overlay_contracts_are_available() {
    let mut app = TestApp::builder().build();

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .lua
            .lua
            .load(
                r##"
            local ns = smelt.ns("coverage.buf_win_overlay")
            assert(type(ns) == "number", "ns returns number")

            local buf = smelt.buf.new({ name = "coverage.contract.buf", readonly = true, editable = true })
            assert(tostring(buf):find("Buf#", 1, true) == 1, "buf tostring")
            assert(buf:readonly() == true, "initial readonly")
            assert(buf:readonly(false) == buf, "readonly setter chain")
            assert(buf:readonly() == false, "readonly setter value")
            assert(buf:source("alpha\nβeta") == buf, "source setter chain")
            assert(buf:source() == "alpha\nβeta", "source getter")
            assert(buf:lines({ "one", "two" }) == buf, "lines setter chain")
            local lines = buf:lines()
            assert(#lines == 2, "line count")
            assert(lines[1] == "one", "first line")
            assert(lines[2] == "two", "second line")
            assert(buf:line(1) == "one", "line getter")
            assert(buf:line(0) == nil, "line zero nil")
            local mark_id = buf:mark(ns, 1, 1, { end_row = 1, end_col = 3, hl_group = "Normal" })
            assert(type(mark_id) == "number", "mark id")
            assert(buf:clear_ns(ns) == buf, "clear ns chain")
            assert(buf:styled({
                { { text = "styled", style = { bold = true, fg = { 255, 0, 0 } } } },
            }) == buf, "styled chain")
            assert(type(buf:line(1)) == "string", "styled line")

            local win = smelt.win.new(buf, {
                name = "coverage.contract.win",
                surface = "selectable_text",
                wrap = false,
                scrollbar = false,
            })
            assert(tostring(win):find("Win#", 1, true) == 1)
            assert(win:buf():source() == buf:source())
            assert(win:cursor(1) == win)
            assert(type(win:cursor()) == "number")
            assert(win:move_cursor(-1) == win)
            assert(win:placeholder("ghost", { accept_keys = { "tab" }, dismiss_keys = { "esc" } }) == win)
            assert(win:placeholder_text() == "ghost")
            win:clear_placeholder()
            assert(win:placeholder_text() == nil)
            assert(type(win:scroll()) == "table")
            assert(win:scroll(0) == win)
            assert(win:scroll("tail") == win)
            local win_reg = win:key("x", function() end)
            assert(win_reg:remove() == true)

            local measure = smelt.ui.layout.measure(12, 3)
            local mw, mh = measure:get()
            assert(mw == 12 and mh == 3)
            measure:set(14, 4)
            mw, mh = measure:get()
            assert(mw == 14 and mh == 4)

            local layout = smelt.ui.layout.vbox({
                { smelt.ui.layout.leaf(win, { measure = measure, title = "Leaf" }), height = "fill" },
            }, { gap = 0, title = "VBox" })
            local overlay = smelt.overlay.new({
                name = "coverage.contract.overlay",
                title = "Coverage Overlay",
                anchor = "screen_at",
                corner = "nw",
                row = 0, col = 0,
                width = 30, height = 8,
                layout = layout,
            })
            assert(tostring(overlay):find("Overlay#", 1, true) == 1)
            local overlay_reg = overlay:key("y", function() end)
            assert(overlay_reg:remove() == true)
        "##,
            )
            .exec()
            .expect("lua buf/win/overlay contracts");
    }

    assert!(app.app.ui.named_buf("coverage.contract.buf").is_some());
    assert!(app.app.ui.named_win("coverage.contract.win").is_some());
    assert!(app
        .app
        .ui
        .named_overlay("coverage.contract.overlay")
        .is_some());
}

#[test]
fn lua_prompt_text_theme_work_keymap_and_vim_contracts_are_available() {
    let mut app = TestApp::builder().with_vim(true).build();

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .lua
            .lua
            .load(
                r##"
            smelt.prompt.set_text("aéz")
            assert(smelt.prompt.text() == "aéz", "prompt text roundtrip")
            assert(smelt.prompt.cursor(999) == #"aéz", "prompt cursor clamps")
            assert(smelt.prompt.replace_range(1, 3, "X") == 2, "prompt replace cursor")
            assert(smelt.prompt.text() == "aXz", "prompt replace snaps utf8 range")
            assert(type(smelt.prompt.win()) == "userdata", "prompt win")
            assert(type(smelt.prompt.queued()) == "table", "prompt queued")
            assert(smelt.prompt.has_stash() == false, "prompt stash")

            assert(smelt.text.width("界") == 2, "wide char width")
            assert(smelt.text.line_count("a\nb") == 2, "line count")
            assert(smelt.text.slugify("Hello, Smelt!") == "hello-smelt", "slugify")
            assert(smelt.text.truncate("éx", 2) == "é", "truncate keeps utf8 boundary")
            assert(smelt.text.truncate("abcdef", 3, { keep = "tail", prefix = "…" }) == "…def", "tail truncate")
            assert(smelt.text.fit("x", 4, { align = "right", fill = "." }) == "...x", "fit right")
            assert(smelt.text.format_duration(65) == "1m 5s", "duration")
            assert(smelt.text.format_tokens(1200) == "1.2k", "tokens")
            assert(smelt.text.format_cost(1.25) == "$1.25", "cost")

            smelt.theme.set("CoverageContract", { fg = { ansi = 42 }, bold = true })
            local style = smelt.theme.get("CoverageContract")
            assert(style.fg.ansi == 42, "theme fg")
            assert(style.bold == true, "theme bold")
            assert(type(smelt.theme.snapshot().CoverageContract) == "table", "theme snapshot")
            assert(type(smelt.theme.is_light()) == "boolean", "theme light")

            assert(smelt.work.is_busy() == false, "work initially idle")
            local guard = smelt.work.guard()
            assert(smelt.work.guard_current(guard) == true, "work guard current")
            local busy = smelt.work.busy("coverage")
            assert(smelt.work.is_busy() == true, "work busy")
            assert(busy:remove() == true, "work remove")
            assert(smelt.work.is_busy() == false, "work idle after remove")

            local old_leader = smelt.keymap.leader()
            smelt.keymap.set_leader("<space>")
            assert(smelt.keymap.leader() == "<space>", "leader set")
            local reg = smelt.keymap.set("n", "<leader>t", function() end)
            local found = false
            for _, row in ipairs(smelt.keymap.list()) do
                if row.mode == "n" and row.chord == "<space>t" then found = true end
            end
            assert(found, "keymap listed")
            assert(reg:remove() == true, "keymap reg remove")
            assert(smelt.keymap.unset("n", "<leader>t") == false, "keymap already removed")
            smelt.keymap.set_leader(old_leader)
            assert(type(smelt.keymap.help_sections()) == "table", "help sections")

            assert(smelt.vim.mode() == "insert", "initial vim mode")
            smelt.vim.set_mode("normal")
            assert(smelt.vim.mode() == "normal", "vim mode set")
            smelt.vim.set_mode("insert")
        "##,
            )
            .exec()
            .expect("lua prompt/text/theme/work/keymap/vim contracts");
    }

    assert_eq!(app.state().prompt_text, "aXz");
}

#[test]
fn lua_picker_permissions_notify_engine_and_ui_contracts_are_available() {
    let mut app = TestApp::builder().build();

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .lua
            .lua
            .load(
                r#"
            local size = smelt.ui.size()
            assert(type(size.width) == "number" and type(size.height) == "number", "ui size")
            assert(type(smelt.win.transcript()) == "userdata", "transcript win")
            assert(type(smelt.win.TRANSCRIPT) == "userdata", "transcript constant")
            assert(type(smelt.win.PROMPT) == "userdata", "prompt constant")

            local picker = smelt.picker.new({
                title = "Coverage Picker",
                items = {
                    "one",
                    { label = "two", description = "second" },
                },
            })
            assert(tostring(picker):find("Picker#", 1, true) == 1, "picker tostring")
            assert(type(picker:win()) == "userdata", "picker win")
            assert(picker:selected() == 0, "picker initial selected")
            assert(picker:move(1) == picker, "picker move chain")
            assert(picker:selected() == 1, "picker moved")
            assert(picker:items({ "replacement" }, 0) == picker, "picker items chain")
            assert(picker:selected() == 0, "picker selected reset")
            picker:close()

            local perms = smelt.permissions.list()
            assert(type(perms.session) == "table", "permission session list")
            assert(type(perms.workspace) == "table", "permission workspace list")
            local tool_decision = smelt.permissions.check_tool("default", "bash")
            local subcommand_decision = smelt.permissions.check("default", "bash", "git status")
            assert(type(tool_decision) == "string" and #tool_decision > 0, "permission tool decision")
            assert(type(subcommand_decision) == "string" and #subcommand_decision > 0, "permission subcommand decision")
            smelt.permissions.set_rules({ default = { tools = { ask = { "*" } } } })

            smelt.notify("hello from lua contract", "coverage")
            smelt.notify.warn("careful from lua contract", "coverage")
            smelt.notify.error("error from lua contract", "coverage")

            assert(smelt.engine.is_running() == false, "engine running")
            assert(type(smelt.engine.summary_prefix()) == "string", "summary prefix")
            local prep = smelt.engine.on_prepare_request(function(req, reply) reply(nil) end)
            assert(prep:remove() == true, "prepare hook remove")
            local limit = smelt.engine.on_context_limit(function(messages, reply) reply(nil) end)
            assert(limit:remove() == true, "context hook remove")
            assert(not pcall(function() smelt.engine.ask({ system = "" }) end), "ask validates system")
        "#,
            )
            .exec()
            .expect("lua picker/permissions/notify/engine/ui contracts");
    }
}

#[test]
fn lua_layout_is_applied_before_first_render() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
            local top = require("smelt.prompt_bar").top_win:rect()
            local transcript = smelt.win.transcript():rect()
            assert(top ~= nil, "prompt top bar has layout rect before first render")
            assert(transcript ~= nil, "transcript has layout rect before first render")
            assert(top.row > transcript.row, "top bar is below transcript")
        "#,
    ));
}

#[test]
fn empty_banner_returns_to_startup_position_after_resize_round_trip() {
    fn banner_label_rect(app: &TestApp) -> crate::smelt_edit::Rect {
        let win = app
            .app
            .ui
            .named_win("smelt.banner.label.win")
            .expect("banner label window");
        app.app
            .ui
            .win(win)
            .and_then(|win| win.viewport.map(|vp| vp.rect))
            .expect("banner label viewport")
    }

    fn paint_and_emit_resize(app: &mut TestApp) {
        app.render_silent();
        crate::lua::with_app_ptr(&mut app.app, |app| {
            app.dispatch_ui_window_events(false);
        });
    }

    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    crate::lua::with_app_ptr(&mut app.app, |app| {
        let err = app.bring_up_lua("launch", true);
        assert_eq!(err, None);
    });

    let startup = banner_label_rect(&app);

    app.set_terminal_size(100, 30);
    paint_and_emit_resize(&mut app);
    app.set_terminal_size(80, 24);
    paint_and_emit_resize(&mut app);

    assert_eq!(banner_label_rect(&app), startup);
}

#[test]
fn empty_banner_stays_inside_transcript_when_prompt_grows() {
    fn rects_intersect(a: crate::smelt_edit::Rect, b: crate::smelt_edit::Rect) -> bool {
        a.left < b.right() && a.right() > b.left && a.top < b.bottom() && a.bottom() > b.top
    }

    fn banner_label_rect(app: &TestApp) -> crate::smelt_edit::Rect {
        let win = app
            .app
            .ui
            .named_win("smelt.banner.label.win")
            .expect("banner label window");
        app.app
            .ui
            .win(win)
            .and_then(|win| win.viewport.map(|vp| vp.rect))
            .expect("banner label viewport")
    }

    fn paint_and_emit_resize(app: &mut TestApp) {
        app.render_silent();
        crate::lua::with_app_ptr(&mut app.app, |app| {
            app.dispatch_ui_window_events(false);
        });
    }

    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    crate::lua::with_app_ptr(&mut app.app, |app| {
        let err = app.bring_up_lua("launch", true);
        assert_eq!(err, None);
    });

    app.type_text("one\ntwo\nthree\nfour\nfive\nsix");
    paint_and_emit_resize(&mut app);

    let label = banner_label_rect(&app);
    let transcript = app
        .app
        .ui
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .expect("transcript rect");
    let prompt = app
        .app
        .ui
        .split_rect(crate::app::PROMPT_WIN)
        .expect("prompt rect");

    assert!(
        label.top >= transcript.top && label.bottom() <= transcript.bottom(),
        "banner label should stay inside transcript: label={label:?} transcript={transcript:?}"
    );
    assert!(
        !rects_intersect(label, prompt),
        "banner label must not overlap prompt: label={label:?} prompt={prompt:?}"
    );
}

#[test]
fn win_rect_prefers_current_layout_after_resize_before_paint() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.render_silent();

    app.set_terminal_size(100, 30);
    let expected = app
        .app
        .ui
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .expect("transcript split rect after resize");

    let lua = format!(
        r#"
            local r = smelt.win.transcript():rect()
            assert(r ~= nil, "transcript rect")
            assert(r.row == {row}, "row " .. tostring(r.row))
            assert(r.col == {col}, "col " .. tostring(r.col))
            assert(r.width == {width}, "width " .. tostring(r.width))
            assert(r.height == {height}, "height " .. tostring(r.height))
        "#,
        row = expected.top,
        col = expected.left,
        width = expected.width,
        height = expected.height,
    );
    assert!(app.run_lua(&lua));
}

#[test]
fn reload_clears_surviving_prompt_keymaps() {
    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(
        r#"
            smelt.prompt.win():key("left", function() end)
            "#,
    ));

    app.reload_lua();
    app.type_text("ab");
    app.press(KeyCode::Left);
    app.type_char('X');

    assert_eq!(app.state().prompt_text, "aXb");
    assert_eq!(app.app.prompt_win().cpos(), 2);
}

#[test]
fn reload_recompiles_transcript_renderer_extensions_and_rejects_stale_ir() {
    fn write_renderer(path: &std::path::Path, marker: Option<&str>) {
        let source = marker.map_or_else(
            || "-- default transcript renderer only\n".to_string(),
            |marker| {
                format!(
                    r#"
                    smelt.transcript.extend_renderer("reload-tool-marker", function(next, block, ctx)
                      if block.kind == "tool" then
                        return smelt.layout.text("{marker} " .. (block.name or "tool"))
                      end
                      return next(block, ctx)
                    end, {{ cache_key = "reload-tool-marker:{marker}" }})
                    "#,
                )
            },
        );
        std::fs::write(path, source).expect("write init.lua");
    }

    fn transcript_rows(app: &mut TestApp) -> Vec<String> {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .full_transcript_display_text()
            .iter()
            .cloned()
            .collect()
    }

    let tmp = tempfile::tempdir().expect("temp init dir");
    let init = tmp.path().join("init.lua");
    write_renderer(&init, Some("reload-marker-v1"));

    let mut app = TestApp::builder().with_init_lua(&init).build();
    app.app.handle_resize(100, 30);
    app.app.start_tool(
        "reload-call-1".into(),
        "bash".into(),
        protocol::StyledLines::from_plain("echo reload"),
        std::collections::HashMap::new(),
    );
    app.app.finish_tool(
        "reload-call-1",
        smelt_core::transcript_model::ToolStatus::Ok,
        Some(Box::new(smelt_core::transcript_model::ToolOutput {
            content: "reload output".into(),
            is_error: false,
            metadata: None,
        })),
        Some(Duration::from_millis(1200)),
    );

    let first = transcript_rows(&mut app).join("\n");
    assert!(first.contains("reload-marker-v1 bash"), "{first}");
    assert!(!first.contains("reload-marker-v2"), "{first}");

    write_renderer(&init, Some("reload-marker-v2"));
    app.reload_lua();
    let second = transcript_rows(&mut app).join("\n");
    assert!(second.contains("reload-marker-v2 bash"), "{second}");
    assert!(!second.contains("reload-marker-v1"), "{second}");

    write_renderer(&init, None);
    app.reload_lua();
    let default = transcript_rows(&mut app).join("\n");
    assert!(default.contains("* bash echo reload"), "{default}");
    assert!(!default.contains("reload-marker-v2"), "{default}");
}

#[test]
fn named_overlay_open_refreshes_title_in_place() {
    let mut app = TestApp::builder().build();
    let _guard = crate::lua::install_app_ptr(&mut app.app);

    let lua = &app.app.lua.lua;
    lua.load(
        r#"
            local buf = smelt.buf.new({ name = "perf_panel.buf" })
            local win = smelt.win.new(buf, { name = "perf_panel.win", surface = "inert" })
            smelt.overlay.new({
                name = "perf_panel",
                title = "old title",
                anchor = "screen_at",
                corner = "ne",
                row = 0, col = 0,
                width = 44, height = 14,
                layout = smelt.ui.layout.leaf(win),
            })
            "#,
    )
    .exec()
    .expect("first open");

    let id1 = app.app.ui.named_overlay("perf_panel").expect("named id");
    let title1 = app
        .app
        .ui
        .overlay(id1)
        .and_then(|ov| {
            ov.layout
                .chrome()
                .title
                .as_ref()
                .map(|l| l.spans.iter().map(|s| s.text.as_ref()).collect::<String>())
        })
        .unwrap_or_default();
    assert_eq!(title1, "old title");

    lua.load(
        r#"
            local buf = smelt.buf.new({ name = "perf_panel.buf" })
            local win = smelt.win.new(buf, { name = "perf_panel.win", surface = "inert" })
            smelt.overlay.new({
                name = "perf_panel",
                title = "new title",
                anchor = "screen_at",
                corner = "ne",
                row = 0, col = 0,
                width = 44, height = 14,
                layout = smelt.ui.layout.leaf(win),
            })
            "#,
    )
    .exec()
    .expect("second open");

    let id2 = app
        .app
        .ui
        .named_overlay("perf_panel")
        .expect("named id after refresh");
    assert_eq!(id1, id2, "same OverlayId across refresh");
    let title2 = app
        .app
        .ui
        .overlay(id2)
        .and_then(|ov| {
            ov.layout
                .chrome()
                .title
                .as_ref()
                .map(|l| l.spans.iter().map(|s| s.text.as_ref()).collect::<String>())
        })
        .unwrap_or_default();
    assert_eq!(title2, "new title", "title should refresh in place");
}

#[test]
fn named_win_refresh_preserves_wrap_when_omitted() {
    let mut app = TestApp::builder().build();
    let _guard = crate::lua::install_app_ptr(&mut app.app);
    let lua = &app.app.lua.lua;

    lua.load(
        r#"
            local buf = smelt.buf.new({ name = "w.buf" })
            smelt.win.new(buf, { name = "w.win", wrap = false })
            "#,
    )
    .exec()
    .expect("first open");

    let wid = app.app.ui.named_win("w.win").expect("named win");
    assert!(
        !app.app.ui.win(wid).unwrap().wrap,
        "wrap should be false after explicit open"
    );

    // Re-open with the same name but no `wrap` key → wrap should stay false.
    lua.load(
        r#"
            local buf = smelt.buf.new({ name = "w.buf" })
            smelt.win.new(buf, { name = "w.win" })
            "#,
    )
    .exec()
    .expect("refresh");

    assert!(
        !app.app.ui.win(wid).unwrap().wrap,
        "wrap must be preserved across named refresh (regression)"
    );
}

#[test]
fn named_buf_and_win_survive_across_open_calls() {
    let mut app = TestApp::builder().build();
    let _guard = crate::lua::install_app_ptr(&mut app.app);
    let lua = &app.app.lua.lua;

    lua.load(
        r#"
            local buf = smelt.buf.new({ name = "x.buf" })
            smelt.win.new(buf, { name = "x.win" })
            "#,
    )
    .exec()
    .expect("first");
    let first_buf = app.app.ui.named_buf("x.buf").expect("buf 1");
    let first_win = app.app.ui.named_win("x.win").expect("win 1");

    lua.load(
        r#"
            local buf = smelt.buf.new({ name = "x.buf" })
            smelt.win.new(buf, { name = "x.win" })
            "#,
    )
    .exec()
    .expect("second");
    let second_buf = app.app.ui.named_buf("x.buf").expect("buf 2");
    let second_win = app.app.ui.named_win("x.win").expect("win 2");

    assert_eq!(
        first_buf, second_buf,
        "named buf id stable across re-create"
    );
    assert_eq!(first_win, second_win, "named win id stable across re-open");
}

#[test]
fn named_overlay_refresh_replaces_layout_structure() {
    let mut app = TestApp::builder().build();
    let _guard = crate::lua::install_app_ptr(&mut app.app);
    let lua = &app.app.lua.lua;

    lua.load(
        r#"
            local buf = smelt.buf.new({ name = "a.buf" })
            local win = smelt.win.new(buf, { name = "a.win" })
            smelt.overlay.new({
                name = "ov",
                anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 40, height = 10,
                layout = smelt.ui.layout.leaf(win),
            })
            "#,
    )
    .exec()
    .expect("first open");

    let id = app.app.ui.named_overlay("ov").expect("named");
    let leaves_before = app
        .app
        .ui
        .overlay(id)
        .map(|ov| ov.layout.leaves_in_order().len())
        .unwrap_or(0);
    assert_eq!(leaves_before, 1);

    lua.load(
        r#"
            local b1 = smelt.buf.new({ name = "a.buf" })
            local b2 = smelt.buf.new({ name = "b.buf" })
            local w1 = smelt.win.new(b1, { name = "a.win" })
            local w2 = smelt.win.new(b2, { name = "b.win" })
            smelt.overlay.new({
                name = "ov",
                anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 40, height = 10,
                layout = smelt.ui.layout.vbox({
                    { smelt.ui.layout.leaf(w1), height = "fill" },
                    { smelt.ui.layout.leaf(w2), height = "fill" },
                }),
            })
            "#,
    )
    .exec()
    .expect("structural refresh");

    let leaves_after = app
        .app
        .ui
        .overlay(id)
        .map(|ov| ov.layout.leaves_in_order().len())
        .unwrap_or(0);
    assert_eq!(leaves_after, 2, "layout should be swapped to 2-leaf vbox");
}

#[test]
fn sweep_state_prunes_untouched_entries() {
    let rt = crate::lua::LuaRuntime::new();
    rt.lua
        .load(
            r#"
                local s1 = smelt.state("alive")
                s1.open = true
                local s2 = smelt.state("dead")
                s2.open = true
                "#,
        )
        .exec()
        .expect("seed");

    // Mimic what `reload()` does: reset the touched table, simulate one
    // plugin re-touching its state, then sweep.
    rt.lua
        .load(
            r#"
                __smelt_state_touched__ = {}
                smelt.state("alive")
                smelt.__sweep_state()
                "#,
        )
        .exec()
        .expect("sweep");

    let alive: bool = rt
        .lua
        .load("return __smelt_state__.alive ~= nil")
        .eval()
        .unwrap();
    let dead: bool = rt
        .lua
        .load("return __smelt_state__.dead ~= nil")
        .eval()
        .unwrap();
    assert!(alive, "touched entry survives");
    assert!(!dead, "untouched entry is swept");
}

#[test]
fn sweep_state_prunes_clean_untouched_persistent_entries() {
    let rt = crate::lua::LuaRuntime::new();
    rt.lua
        .load(
            r#"
                __smelt_persistent_state__.alive = { dirty = false }
                __smelt_persistent_state__.dead = { dirty = false }
                __smelt_persistent_state__.dirty = { dirty = true }
                __smelt_persistent_state_touched__ = { alive = true }
                smelt.__sweep_state()
                "#,
        )
        .exec()
        .expect("sweep");

    let alive: bool = rt
        .lua
        .load("return __smelt_persistent_state__.alive ~= nil")
        .eval()
        .unwrap();
    let dead: bool = rt
        .lua
        .load("return __smelt_persistent_state__.dead ~= nil")
        .eval()
        .unwrap();
    let dirty: bool = rt
        .lua
        .load("return __smelt_persistent_state__.dirty ~= nil")
        .eval()
        .unwrap();
    assert!(alive, "touched persistent entry survives");
    assert!(!dead, "clean untouched persistent entry is swept");
    assert!(
        dirty,
        "dirty untouched persistent entry is kept for flushing"
    );
}

#[test]
fn reload_lua_refreshes_overlay_title_from_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");

    let body = |title: &str| {
        format!(
            r#"
                local state = smelt.state("plug")
                local function attach()
                    local buf = smelt.buf.new({{ name = "plug.buf" }})
                    local win = smelt.win.new(buf, {{ name = "plug.win" }})
                    smelt.overlay.new({{
                        name = "plug",
                        title = "{title}",
                        anchor = "screen_at", corner = "nw",
                        row = 0, col = 0, width = 40, height = 10,
                        layout = smelt.ui.layout.leaf(win),
                    }})
                end
                state.open = true
                attach()
                "#
        )
    };
    std::fs::write(&init, body("v1")).unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        assert_eq!(read_overlay_title(&app, "plug").as_deref(), Some("v1"));
    }
    let id1 = app.app.ui.named_overlay("plug").unwrap();

    std::fs::write(&init, body("v2")).unwrap();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reload_lua();
    }
    let id2 = app.app.ui.named_overlay("plug").expect("overlay survives");
    assert_eq!(id1, id2, "OverlayId preserved across reload");
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        assert_eq!(read_overlay_title(&app, "plug").as_deref(), Some("v2"));
    }
}

#[test]
fn reload_lua_preserves_nested_state_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            local s = smelt.state("nested")
            s.cfg = s.cfg or { panel = { width = 80, history = { 1, 2, 3 } } }
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reload_lua();
        app.app.reload_lua();
    }
    let width: u64 = app
        .app
        .lua
        .lua
        .load("return __smelt_state__.nested.cfg.panel.width")
        .eval()
        .unwrap();
    let last: u64 = app
        .app
        .lua
        .lua
        .load("return __smelt_state__.nested.cfg.panel.history[3]")
        .eval()
        .unwrap();
    assert_eq!(width, 80);
    assert_eq!(last, 3);
}

#[test]
fn reload_lua_flushes_pending_persistent_state_before_clearing_timers() {
    let _home_guard = test_home_guard();
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            local s = smelt.state.persistent("flush_reload", { debounce_ms = 100000 })
            s.value = "before-reload"
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder()
        .with_init_lua(&init)
        .build_with_test_home_guard(&_home_guard);
    let state_path = smelt_core::config::state_dir()
        .join("plugins")
        .join("flush_reload.json");
    assert!(
        !state_path.exists(),
        "debounced save should not have reached disk before reload"
    );
    let dirty_before: bool = app
        .app
        .lua
        .lua
        .load("return __smelt_persistent_state__.flush_reload.dirty == true")
        .eval()
        .unwrap();
    let pending_before: bool = app
        .app
        .lua
        .lua
        .load("return __smelt_persistent_state__.flush_reload.pending ~= nil")
        .eval()
        .unwrap();
    assert!(
        dirty_before,
        "persistent write should be dirty before reload"
    );
    assert!(pending_before, "debounced save should still be pending");

    std::fs::write(&init, "-- no persistent write on reload\n").unwrap();
    app.reload_lua();

    let raw = std::fs::read_to_string(&state_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", state_path.display()));
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["value"], "before-reload");

    let entry_swept_after: bool = app
        .app
        .lua
        .lua
        .load("return __smelt_persistent_state__.flush_reload == nil")
        .eval()
        .unwrap();
    assert!(
        entry_swept_after,
        "clean persistent state not touched by the new config should be swept"
    );
}

#[test]
fn direct_reload_clears_pending_scheduled_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "_G.reload_count = (_G.reload_count or 0) + 1\n").unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    assert_eq!(app.lua_int_global("reload_count"), Some(1));

    assert!(app.schedule_lua_reload());
    app.reload_lua();

    assert!(!app.pending_lua_reload());
    assert_eq!(app.lua_int_global("reload_count"), Some(2));
}

#[test]
fn reload_lua_does_not_double_wrap_tools_register() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "").unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        for _ in 0..5 {
            app.app.reload_lua();
        }
        // Register a tool with no `summary`; the bootstrap wrap should
        // populate it once. If the wrap had compounded across reloads
        // the call would still succeed - but every reload would add a
        // closure frame on top. The functional check: registration
        // works and the registered summary handler runs.
        app.app
            .lua
            .lua
            .load(
                r#"
                    smelt.tools.register({
                        name = "t",
                        description = "",
                        parameters = { type = "object", properties = {} },
                        execute = function() return "" end,
                    })
                    "#,
            )
            .exec()
            .expect("register after many reloads");
    }
    let summary = app
        .app
        .lua
        .tool_summary("t", &std::collections::HashMap::new());
    // `default_summary` returns "" when args have no recognised keys.
    assert!(
        summary.is_empty(),
        "summary should be empty for no-arg tool"
    );
}

#[test]
fn reload_lua_reaps_anonymous_overlay_keeps_named() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    // First version opens both a named overlay and a plain
    // anonymous overlay. init.lua doesn't call `smelt.plugin(...)`,
    // so its loader frame is unnamed and anonymous resources stay
    // anonymous - they get reaped on /reload.
    std::fs::write(
        &init,
        r#"
            local state = smelt.state("mix")
            local function attach()
                local b1 = smelt.buf.new({ name = "mix.buf" })
                local w1 = smelt.win.new(b1, { name = "mix.win" })
                smelt.overlay.new({
                    name = "mix",
                    anchor = "screen_at", corner = "nw",
                    row = 0, col = 0, width = 30, height = 8,
                    layout = smelt.ui.layout.leaf(w1),
                })
            end
            state.open = true
            attach()

            -- Anonymous overlay: init.lua's frame is unnamed (no
            -- `smelt.plugin(...)` call), so this gets reaped on /reload.
            local b2 = smelt.buf.new()
            local w2 = smelt.win.new(b2, {})
            smelt.overlay.new({
                anchor = "screen_at", corner = "se",
                row = 0, col = 0, width = 20, height = 5,
                layout = smelt.ui.layout.leaf(w2),
            })
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();

    // Capture the anonymous overlay's id - we'll assert it's gone
    // after reload while the named one survives. (Total overlay
    // count is noisy: reload_lua emits a `notify(...)` toast which
    // adds its own short-lived overlay.)
    let named_id = app.app.ui.named_overlay("mix").expect("named");
    let anon_id = (1u32..)
        .map(crate::smelt_edit::OverlayId)
        .find(|id| *id != named_id && app.app.ui.overlay(*id).is_some())
        .expect("anonymous overlay present");

    // Second version drops the anonymous overlay; named one stays.
    std::fs::write(
        &init,
        r#"
            local state = smelt.state("mix")
            local function attach()
                local b1 = smelt.buf.new({ name = "mix.buf" })
                local w1 = smelt.win.new(b1, { name = "mix.win" })
                smelt.overlay.new({
                    name = "mix",
                    anchor = "screen_at", corner = "nw",
                    row = 0, col = 0, width = 30, height = 8,
                    layout = smelt.ui.layout.leaf(w1),
                })
            end
            if state.open then attach() end
            "#,
    )
    .unwrap();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reload_lua();
    }
    assert!(
        app.app.ui.named_overlay("mix").is_some(),
        "named overlay survives reload"
    );
    assert!(
        app.app.ui.overlay(anon_id).is_none(),
        "anonymous overlay {} should be reaped",
        anon_id.0
    );
}

#[test]
fn reload_lua_preserves_named_paint_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    // Module-body code: capture the paint id in a state slot so we
    // can read it back from Rust after the reload cycle.
    std::fs::write(
        &init,
        r#"
            local state = smelt.state("paint_id_probe")
            local function painter(_slice, _ctx) end
            -- No `smelt.plugin(...)` call → init.lua's loader frame
            -- stays unnamed, so the unnamed register call below is
            -- anonymous and gets reaped on /reload. The explicit
            -- name = "probe.named" slot survives.
            smelt.paint.register(painter, { name = "probe.named" })
            smelt.paint.register(painter)
            state.dummy = true
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();

    let pre_named = app
        .app
        .paint_registry
        .id_by_name("probe.named")
        .expect("named pre id");
    // The anonymous slot has no name binding; locate it as the only
    // un-named PaintId currently registered.
    let pre_anon = find_anon_paint(&app.app);

    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reload_lua();
    }

    let post_named = app
        .app
        .paint_registry
        .id_by_name("probe.named")
        .expect("named post id");
    let post_anon = find_anon_paint(&app.app);
    assert_eq!(
        pre_named, post_named,
        "named paint slot must keep stable PaintId across reload"
    );
    assert_ne!(
        pre_anon, post_anon,
        "anonymous paint slot must allocate a fresh id on reload"
    );
    assert!(
        !app.app.paint_registry.contains(pre_anon),
        "old anonymous PaintId must be reaped"
    );
    assert!(app.app.paint_registry.contains(post_named));
    assert!(app.app.paint_registry.contains(post_anon));
}

#[test]
fn reload_lua_drains_ready_hooks_with_kind_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            local state = smelt.state("ready_kind_probe")
            state.fires = (state.fires or 0)
            state.last_kind = nil
            smelt.lifecycle.on_ready(function(ctx)
                state.fires = state.fires + 1
                state.last_kind = ctx and ctx.kind or "<nil>"
            end)
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    // Cold-start `TestApp` skips the `on_ready` drain (storybook
    // tests don't want interactive decoration like the splash
    // banner). Fire it manually here since this test specifically
    // covers the `kind = "launch"` drain.
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        let _ = app.app.bring_up_lua("launch", true);
    }

    let read = |rt: &crate::lua::LuaRuntime, k: &str| -> String {
        rt.lua
            .load(format!(
                "return tostring(__smelt_state__['ready_kind_probe'].{k})"
            ))
            .eval::<String>()
            .unwrap()
    };
    assert_eq!(read(&app.app.lua, "fires"), "1");
    assert_eq!(read(&app.app.lua, "last_kind"), "launch");

    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reload_lua();
    }
    assert_eq!(read(&app.app.lua, "fires"), "2");
    assert_eq!(read(&app.app.lua, "last_kind"), "reload");
}

#[test]
fn reload_lua_sweeps_state_for_deleted_plugins() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            local a = smelt.state("kept")
            a.flag = true
            local b = smelt.state("dropped")
            b.flag = true
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    let exists = |rt: &crate::lua::LuaRuntime, k: &str| -> bool {
        rt.lua
            .load(format!("return __smelt_state__['{k}'] ~= nil"))
            .eval::<bool>()
            .unwrap()
    };
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        assert!(exists(&app.app.lua, "kept"));
        assert!(exists(&app.app.lua, "dropped"));
    }

    // Edit: only the "kept" plugin remains.
    std::fs::write(
        &init,
        r#"
            local a = smelt.state("kept")
            "#,
    )
    .unwrap();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reload_lua();
        assert!(exists(&app.app.lua, "kept"));
        assert!(
            !exists(&app.app.lua, "dropped"),
            "dropped plugin's state should be swept"
        );
    }
}

#[test]
fn reload_clears_every_lua_surface() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    // Populate every observable surface from user init.lua so the
    // reload-with-empty-init test below can assert each is empty.
    std::fs::write(
        &init,
        r#"
            -- LuaShared registries
            smelt.cmd.register("seed_cmd", function() end)
            smelt.keymap.set("n", "<C-o>", function() end)
            smelt.tools.register({
                name = "seed_tool",
                description = "",
                parameters = { type = "object", properties = {} },
                permission_defaults = { normal = "deny" },
                effect = "config_reload",
                default_allow = { "seed" },
                subpattern_parser = "bash",
                execute = function() return "" end,
            })
            smelt.permissions.set_rules({ normal = { tools = { deny = { "seed_tool" } } } })
            smelt.process.set_default_shell({ program = "/bin/zsh", args = { "-fc" } })
            smelt.provider.register("seed_provider", {
                type = "openai",
                api_base = "http://seed.invalid",
                models = { "seed-model" },
            })
            smelt.tools.middleware("", { before = function() end })
            smelt.provider.middleware({ on_response = function() end })

            -- core::timers (Lua-side)
            smelt.timer.every(100000, function() end)

            -- in-flight task (cancel_and_clear path)
            smelt.spawn(function()
                smelt.sleep(100000)
            end)

            -- Anonymous + named UI resources
            local b1 = smelt.buf.new({ name = "seed.buf" })
            local w1 = smelt.win.new(b1, { name = "seed.win" })
            smelt.overlay.new({
                name = "seed.ov",
                anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 30, height = 8,
                layout = smelt.ui.layout.leaf(w1),
            })
            -- Anonymous overlay (init.lua frame unnamed): must be reaped.
            local b2 = smelt.buf.new()
            local w2 = smelt.win.new(b2, {})
            smelt.overlay.new({
                anchor = "screen_at", corner = "se",
                row = 0, col = 0, width = 20, height = 5,
                layout = smelt.ui.layout.leaf(w2),
            })

            -- smelt.state slot
            local s = smelt.state("seed_plugin")
            s.open = true
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    let shared = app.app.lua.shared().core.clone();

    // Pre-reload: every surface has at least the seeded entry.
    assert!(shared.commands.lock().unwrap().contains_key("seed_cmd"));
    assert!(shared
        .keymaps
        .lock()
        .unwrap()
        .keys()
        .any(|(_, c)| c == "<C-o>"));
    assert!(shared.tools.lock().unwrap().contains_key("seed_tool"));
    assert!(shared
        .tool_defaults
        .lock()
        .unwrap()
        .tool_decisions
        .contains_key("seed_tool"));
    assert!(shared.permission_rules.lock().unwrap().is_some());
    assert!(shared.default_shell.lock().unwrap().is_some());
    assert!(shared
        .providers
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.name.as_deref() == Some("seed_provider")));
    assert!(!shared.hooks.tool_before.is_empty());
    assert!(!shared.hooks.provider_response.is_empty());
    assert!(!app.app.core.timers.is_empty());
    assert!(!shared.tasks.lock().unwrap().is_empty());
    let anon_overlay = (1u32..)
        .map(crate::smelt_edit::OverlayId)
        .find(|id| {
            Some(*id) != app.app.ui.named_overlay("seed.ov") && app.app.ui.overlay(*id).is_some()
        })
        .expect("anonymous overlay present");

    // Edit init.lua to empty + drop the "seed_plugin" state slot.
    std::fs::write(&init, "").unwrap();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reload_lua();
    }

    // Post-reload: every "user-registered" surface is empty; named UI
    // resources survive; anonymous ones are reaped; state slot for
    // the dropped plugin is swept.
    assert!(
        !shared.commands.lock().unwrap().contains_key("seed_cmd"),
        "user command cleared"
    );
    assert!(
        !shared
            .keymaps
            .lock()
            .unwrap()
            .keys()
            .any(|(_, c)| c == "<C-o>"),
        "user keymap cleared"
    );
    assert!(
        !shared.tools.lock().unwrap().contains_key("seed_tool"),
        "user tool cleared"
    );
    let defaults = shared.tool_defaults.lock().unwrap();
    assert!(
        !defaults.tool_decisions.contains_key("seed_tool")
            && !defaults.tool_effects.contains_key("seed_tool")
            && !defaults.subcommand_allow.contains_key("seed_tool")
            && !defaults.subpattern_parsers.contains_key("seed_tool"),
        "tool defaults cleared"
    );
    drop(defaults);
    assert!(
        shared.permission_rules.lock().unwrap().is_none(),
        "permission rules cleared"
    );
    assert!(
        shared.default_shell.lock().unwrap().is_none(),
        "default shell cleared"
    );
    assert!(
        !shared
            .providers
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.name.as_deref() == Some("seed_provider")),
        "provider registry cleared"
    );
    assert!(
        shared.hooks.tool_before.is_empty(),
        "tool middleware cleared"
    );
    assert!(
        shared.hooks.provider_response.is_empty(),
        "provider middleware cleared"
    );
    assert!(app.app.core.timers.is_empty(), "timers cleared");
    assert!(shared.tasks.lock().unwrap().is_empty(), "tasks cleared");
    assert!(
        shared.task_inbox.lock().unwrap().is_empty(),
        "task_inbox drained"
    );
    assert!(
        shared.json_inbox.lock().unwrap().is_empty(),
        "json_inbox drained"
    );
    assert!(
        app.app.ui.named_overlay("seed.ov").is_some(),
        "named overlay survives"
    );
    assert!(
        app.app.ui.overlay(anon_overlay).is_none(),
        "anonymous overlay reaped"
    );
    let dropped_state: bool = app
        .app
        .lua
        .lua
        .load("return __smelt_state__.seed_plugin ~= nil")
        .eval()
        .unwrap();
    assert!(!dropped_state, "dropped-plugin state slot swept");
}

#[test]
fn reload_lua_cancels_in_flight_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            _G.__task_completed__ = false
            smelt.spawn(function()
                smelt.sleep(10_000)  -- long sleep so the task is still parked
                _G.__task_completed__ = true
            end)
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    // Sanity: task is parked but not complete.
    let completed: bool = app
        .app
        .lua
        .lua
        .load("return _G.__task_completed__")
        .eval()
        .unwrap();
    assert!(!completed, "task shouldn't have completed yet");

    // Edit init.lua so reload doesn't re-spawn the task.
    std::fs::write(&init, "_G.__task_completed__ = false").unwrap();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reload_lua();
    }
    // Drive: cancelled tasks should be a no-op since we cleared them.
    let outs = app.app.lua.drive_tasks(app.app.core.clock.instant_now());
    assert!(
        outs.is_empty(),
        "no task outputs after reload cancellation (saw {} entries)",
        outs.len()
    );
    let completed: bool = app
        .app
        .lua
        .lua
        .load("return _G.__task_completed__")
        .eval()
        .unwrap();
    assert!(!completed, "cancelled task must not have run to completion");
}

#[test]
fn reload_lua_via_engine_dismisses_open_modal() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            smelt.cmd.register("open_modal", function()
                smelt.spawn(function()
                    local leaf = smelt.dialog.content({ text = "hello" })
                    smelt.dialog.open({
                        title = "test",
                        max_height = "50%",
                        panels = { { leaf = leaf } },
                    })
                end)
            end)
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.apply_lua_command("open_modal");
        app.app.drive_lua_tasks();
    }
    assert!(
        app.app.ui.active_modal().is_some(),
        "modal should be open after /open_modal"
    );

    // Drive the reload through the Lua binding (the gate lives there,
    // not in `TuiApp::reload_lua`). The binding should dismiss the
    // modal and call through to `reload_lua` instead of bailing out.
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .lua
            .lua
            .load("smelt.engine.reload()")
            .exec()
            .expect("reload succeeds even with modal open");
    }
    assert!(
        app.app.ui.active_modal().is_none(),
        "modal must be dismissed after reload"
    );

    // Reload should have re-registered the command - reopen works.
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.apply_lua_command("open_modal");
        app.app.drive_lua_tasks();
    }
    assert!(
        app.app.ui.active_modal().is_some(),
        "command survived reload and reopens modal"
    );
}

#[test]
fn reload_lua_preserves_user_size_override() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            local state = smelt.state("res")
            local function attach()
                local b = smelt.buf.new({ name = "res.buf" })
                local w = smelt.win.new(b, { name = "res.win" })
                smelt.overlay.new({
                    name = "res",
                    anchor = "screen_at", corner = "nw",
                    row = 0, col = 0, width = 30, height = 10,
                    resizable = true,
                    layout = smelt.ui.layout.leaf(w),
                })
            end
            state.open = true
            attach()
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    let id = app.app.ui.named_overlay("res").unwrap();
    // Simulate a user resize gesture.
    if let Some(ov) = app.app.ui.overlay_mut(id) {
        ov.size_override = Some((50, 18));
    }

    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reload_lua();
    }
    let id2 = app.app.ui.named_overlay("res").expect("survives");
    assert_eq!(id, id2);
    let ov = app.app.ui.overlay(id2).unwrap();
    assert_eq!(
        ov.size_override,
        Some((50, 18)),
        "user resize preserved across reload"
    );
}

#[test]
fn scheduled_reload_runs_after_turn_is_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "_G.reload_count = (_G.reload_count or 0) + 1\n").unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    assert_eq!(app.lua_int_global("reload_count"), Some(1));

    app.start_turn(1);
    assert!(app.run_lua("return smelt.engine.reload_when_idle()"));
    assert!(app.app.pending_lua_reload);
    assert_eq!(app.lua_int_global("reload_count"), Some(1));

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: vec![],
        meta: None,
    }));

    assert!(!app.app.pending_lua_reload);
    assert_eq!(app.lua_int_global("reload_count"), Some(2));
}

#[test]
fn hot_reload_reconciles_plan_mode_cycle_and_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "-- initially empty\n").unwrap();

    let mut app = TestApp::builder()
        .with_init_lua(&init)
        .with_mode_cycle(vec![
            AgentMode::normal(),
            AgentMode::parse("apply").unwrap(),
            AgentMode::parse("yolo").unwrap(),
        ])
        .build();
    let plan = AgentMode::parse("plan").unwrap();
    assert!(!app.app.core.config.mode_cycle.contains(&plan));

    std::fs::write(&init, "require(\"smelt.plugins.plan_mode\")\n").unwrap();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.reload_lua();
    }

    assert!(app.app.core.config.mode_cycle.contains(&plan));
    let outcome = app.app.core.permissions.evaluate_tool(
        plan.clone(),
        smelt_core::permissions::ToolOrigin::Lua,
        "smelt_reload",
        &std::collections::HashMap::new(),
    );
    assert_eq!(outcome.decision, protocol::Decision::Deny);
    let outcome = app.app.core.permissions.evaluate_tool(
        plan,
        smelt_core::permissions::ToolOrigin::Lua,
        "present_plan",
        &std::collections::HashMap::new(),
    );
    assert_eq!(outcome.decision, protocol::Decision::Allow);
}

#[test]
fn plan_mode_reload_registers_present_plan_when_already_in_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "require(\"smelt.plugins.plan_mode\")\n").unwrap();
    let plan = AgentMode::parse("plan").unwrap();

    let app = TestApp::builder()
        .with_init_lua(&init)
        .with_mode(plan.clone())
        .build();
    let tools = app
        .app
        .lua
        .tool_defs(plan, smelt_core::lua::ToolVisibility::Interactive);
    assert!(
        tools.iter().any(|t| t.name == "present_plan"),
        "present_plan should be present after reload while already in plan"
    );
}
