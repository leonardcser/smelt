use super::*;

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
        let _ = app.app.bring_up_lua("launch");
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
            smelt.keymap.set("n", "<C-g>", function() end)
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
        .any(|(_, c)| c == "<C-g>"));
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
            .any(|(_, c)| c == "<C-g>"),
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
        plan,
        smelt_core::permissions::ToolOrigin::Lua,
        "smelt_reload",
        &std::collections::HashMap::new(),
    );
    assert_eq!(outcome.decision, protocol::Decision::Deny);
}

#[test]
fn plan_mode_reload_registers_exit_tool_when_already_in_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "require(\"smelt.plugins.plan_mode\")\n").unwrap();
    let plan = AgentMode::parse("plan").unwrap();

    let app = TestApp::builder()
        .with_init_lua(&init)
        .with_mode(plan.clone())
        .build();
    let tools = app.app.lua.tool_defs(plan);
    assert!(
        tools.iter().any(|t| t.name == "exit_plan_mode"),
        "exit_plan_mode should be present after reload while already in plan"
    );
}
