use super::*;

#[test]
fn moved_bar_source_updates_without_original_bar() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(80, 20);
    assert!(app.run_lua(
        r#"
        smelt.prompt.bottom:configure({ layout = "strip" })
        smelt.prompt.bottom:add("cursor", smelt.statusline:builtin("cursor"))
        smelt.statusline:clear()
        smelt.statusline:configure({ visible = false })
        "#,
    ));
    app.render_silent();
    app.type_text("abc");
    let frame = app.render_to_frame();
    assert!(frame.text().contains("1:4"), "{}", frame.text());
    app.type_text("def");
    let frame = app.render_to_frame();
    assert!(frame.text().contains("1:7"), "{}", frame.text());
}

#[test]
fn shared_source_signals_invalidate_each_bar_once() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(
        r#"
      smelt.signal.new('bar.shared', 0)
      local bar = smelt.statusline
      bar:clear()
      __shared_a = bar:add('a', {
        signals = { { 'bar.shared', function(v) return v // 10 end } },
        render = function() return { text = 'a' } end,
      })
      __shared_b = bar:add('b', {
        signals = { { 'bar.shared', function(v) return v // 100 end } },
        render = function() return { text = 'b' } end,
      })
      local invalidate = bar.invalidate
      __invalidations = 0
      bar.invalidate = function(self)
        __invalidations = __invalidations + 1
        return invalidate(self)
      end
    "#,
    )
    .expect("attach sources sharing a signal");
    app.render_silent();
    for (value, expected) in [(1, 0), (10, 1), (100, 1)] {
        assert!(app.run_lua(&format!(
            "__invalidations = 0; smelt.signal.set('bar.shared', {value})"
        )));
        app.render_silent();
        assert_eq!(
            app.eval_lua::<u32>("return __invalidations").unwrap(),
            expected
        );
    }
    assert!(app
        .run_lua("__shared_a:remove(); __invalidations = 0; smelt.signal.set('bar.shared', 110)"));
    app.render_silent();
    assert_eq!(app.eval_lua::<u32>("return __invalidations").unwrap(), 0);
    assert!(app
        .run_lua("__shared_b:remove(); __invalidations = 0; smelt.signal.set('bar.shared', 200)"));
    app.render_silent();
    assert_eq!(app.eval_lua::<u32>("return __invalidations").unwrap(), 0);
}

#[test]
fn failing_signal_projection_does_not_block_other_sources() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(r#"
      local bar = smelt.statusline
      bar:clear()
      smelt.signal.new('bar.projection.failure', 0)
      bar:add('broken', {
        signals = { { 'bar.projection.failure', function() error('bad projection') end } },
        render = function() return { text = 'broken' } end,
      })
      bar:add('live', {
        signals = { { 'bar.projection.failure', function(v) return v end } },
        render = function() return { text = 'live ' .. smelt.signal.get('bar.projection.failure') } end,
      })
    "#).expect("attach independent projections to shared signal");
    assert!(app.render_to_frame().text().contains("live 0"));
    assert!(app.run_lua("smelt.signal.set('bar.projection.failure', 1)"));
    assert!(app.render_to_frame().text().contains("live 1"));
}

#[test]
fn shared_source_subscriptions_outlive_their_first_factory() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(r#"
      local bar = smelt.statusline
      bar:clear()
      smelt.signal.new('bar.scoped.shared', 0)
      bar:set_view(function()
        bar:add('scoped', { signals = { 'bar.scoped.shared' }, render = function() return { text = 'temporary' } end })
        return smelt.ui.layout.vbox({})
      end)
      bar:add('persistent', {
        signals = { 'bar.scoped.shared' },
        render = function() return { text = 'persistent ' .. smelt.signal.get('bar.scoped.shared') } end,
      })
      bar:set_view(nil)
      assert(#bar.sources == 1 and bar.sources[1].name == 'persistent')
    "#).expect("share subscription across factory and persistent sources");
    assert!(app.render_to_frame().text().contains("persistent 0"));
    assert!(app.run_lua("smelt.signal.set('bar.scoped.shared', 1)"));
    assert!(app.render_to_frame().text().contains("persistent 1"));
}

#[test]
fn owned_layout_cleanup_preserves_existing_resources() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(
        r#"
      __existing = smelt.win.new(smelt.buf.new(), { surface = "inert" })
      __existing_paint = smelt.paint.register(function() end)
    "#,
    )
    .expect("create unrelated resources");
    let buffers = app.app.ui.iter_bufs().count();
    app.exec_lua_entry(
        r#"
      local l = smelt.ui.layout
      smelt.statusline:set_view(function(ctx)
        assert(ctx.state ~= nil)
        __owned = smelt.win.new(smelt.buf.new(), { surface = "inert" })
        __owned_paint = smelt.paint.register(function() end)
        return l.hbox({ { l.leaf(__owned) }, { l.leaf(__owned_paint) } })
      end)
      assert(#l.leaves(smelt.statusline:node()) == 2)
      smelt.statusline:set_view(nil)
      assert(__owned:buf() == nil and not __owned_paint:invalidate())
      assert(__existing:buf() ~= nil and __existing_paint:invalidate())
    "#,
    )
    .expect("release only owned resources");
    assert_eq!(app.app.ui.iter_bufs().count(), buffers);
}

#[test]
fn layout_owners_do_not_retain_existing_registry_capacity() {
    let mut app = TestApp::builder().with_ephemeral(true).build();
    app.render_silent();
    app.exec_lua_entry(
        r#"
      for i = 1, 4096 do smelt.buf.new({ name = "capacity.probe." .. i }) end
      __empty_bars = {}
      for i = 1, 32 do __empty_bars[i] = smelt.bar.new({ name = "capacity.bar." .. i }) end
    "#,
    )
    .expect("create unrelated buffers");
    assert!(app.app.ui.iter_bufs().count() >= 4096);
    app.app.lua.lua().gc_collect().unwrap();
    let before = smelt_perf::alloc::snapshot().current_bytes;
    app.exec_lua_entry(
        r#"
      for _, bar in ipairs(__empty_bars) do
        bar:set_view(function() return smelt.ui.layout.vbox({}) end)
      end
    "#,
    )
    .expect("build empty owned layouts");
    app.app.lua.lua().gc_collect().unwrap();
    let retained = smelt_perf::alloc::snapshot()
        .current_bytes
        .saturating_sub(before);
    assert!(
        retained < 256 * 1024,
        "empty layout owners retained {retained} bytes"
    );
    assert!(app.run_lua("for _, bar in ipairs(__empty_bars) do bar:set_view(nil) end"));
}

#[test]
fn window_configuration_is_atomic_and_rejects_typos() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local win = smelt.prompt.win
        local function rejects(opts)
          local ok, err = pcall(function() win:configure(opts) end)
          assert(not ok, "invalid window configuration accepted")
          assert(tostring(err):find("win"), tostring(err))
        end
        rejects({ pad_left = 4, prefix = { first = 42 } })
        rejects({ pad_lefft = 4 })
        rejects({ surface = "edtiable_text" })
        rejects({ wrap = "yes" })
        rejects({ resize = "prompt" })
        "#,
    ));
    assert_eq!(
        app.app
            .ui
            .win(crate::app::PROMPT_WIN)
            .unwrap()
            .config
            .gutters
            .pad_left,
        1
    );
}

#[test]
fn window_options_are_read_once_before_host_mutation() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(
        r#"
      local reads = 0
      smelt.prompt.win:configure(setmetatable({}, { __index = function(_, key)
        if key == "wrap" then
          reads = reads + 1
          return reads ~= 1
        end
      end }))
      assert(reads == 1, "window option read " .. reads .. " times")
    "#,
    )
    .expect("parse window options once");
    assert!(!app.app.ui.win(crate::app::PROMPT_WIN).unwrap().wrap);
}

#[test]
fn source_subscriptions_follow_registration_lifetime() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
      smelt.signal.new("test:source", 0)
      smelt.signal.new("test:unused", 0)
      smelt.statusline:clear()
      __calls = 0
      __old = smelt.statusline:add("value", {
        signals = { "test:unused" }, render = function() error("replaced source ran") end,
      })
      __current = smelt.statusline:add("value", {
        signals = { { "test:source", function(v) return v // 10 end } },
        render = function() __calls = __calls + 1; return { text = "live source" } end,
      })
      __old:remove()
      for _, invalid in ipairs({ 42, false }) do
        assert(not pcall(function()
          smelt.statusline:add("value", {
            signals = { "test:unused", { "test:source", invalid } },
            render = function() error("rejected source ran") end,
          })
        end))
      end
    "#
    ));
    app.render_silent();
    let calls = app.eval_lua::<u32>("return __calls").unwrap();
    assert!(calls > 0);
    assert!(
        app.run_lua(r#"smelt.signal.set("test:source", 1); smelt.signal.set("test:unused", 1)"#)
    );
    app.render_silent();
    assert_eq!(app.eval_lua::<u32>("return __calls").unwrap(), calls);
    assert!(app.run_lua(r#"smelt.signal.set("test:source", 10)"#));
    app.render_silent();
    assert!(app.eval_lua::<u32>("return __calls").unwrap() > calls);
    assert!(app.run_lua(r#"__current:remove(); smelt.signal.set("test:source", 20)"#));
    assert!(!app.render_to_frame().text().contains("live source"));
}

#[test]
fn shared_styles_work_across_ui_surfaces() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(r##"
      local style = { hl_group = "Comment", fg = "#123456", bold = true, underline = true }
      local buf = smelt.buf.new()
      buf:lines({ "mark" })
      buf:mark(1, 1, 0, { end_col = 4, style = style })
      buf:styled({ { { text = "styled", style = style } } })
      local win = smelt.win.new(buf, { scrollbar = false, style = style, prefix = { first = "> ", style = style } })
      smelt.statusline:clear()
      smelt.statusline:add("style", function() return { text = "bar", style = style } end)
      local p = smelt.paint.register(function(s)
        s:put_str(0, 0, "paint", style)
        s:map(function() return { style = { italic = true } } end)
      end)
      local layout = smelt.ui.layout
      smelt.overlay.new({ anchor = "center", modal = false, width = 20, height = 5,
        title = { { text = " title ", style = style } },
        layout = layout.vbox({ { height = 1, layout.leaf(win) }, { height = 1, layout.leaf(p) } }),
      })
      for _, bad in ipairs({ { boldd = true }, { bold = "yes" }, { hl_group = "MissingStyle" } }) do
        assert(not pcall(smelt.style.resolve, bad))
      end
      assert(not pcall(smelt.theme.rgb, "#🦀ab"))
    "##).expect("shared style script");
    let frame = app.render_to_frame();
    for text in ["styled", "paint", "bar", "title"] {
        let (y, row) = frame
            .rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.contains(text))
            .unwrap_or_else(|| panic!("missing {text}: {}", frame.text()));
        let x = row[..row.find(text).unwrap()].chars().count();
        let style = frame.styles[y][x];
        assert_eq!(
            style.fg,
            Some(smelt_core::style::Color::Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56
            })
        );
        assert!(style.bold && style.underline, "{text}: {style:?}");
        if text == "paint" {
            assert!(style.italic);
        }
    }
}

#[test]
fn expired_paint_slice_cannot_alias_another_callback() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
      smelt.paint.frame(function(s) __expired = s end)
      smelt.paint.frame(function(s)
        __expired_ok = pcall(function() __expired:set(0, 0, "X") end)
        s:put_str(0, 0, "live")
      end)
    "#
    ));
    let frame = app.render_to_frame();
    assert!(frame.rows[0].starts_with("live"));
    assert!(!app.eval_lua::<bool>("return __expired_ok").unwrap());
}

#[test]
fn replacement_bar_view_measures_its_own_chrome() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    assert!(app.run_lua(
        r#"
      local l = smelt.ui.layout
      smelt.statusline:set_view(function()
        local p = smelt.paint.register(function(s) s:put_str(0, 0, "measured view") end)
        return l.frame(l.leaf(p, { measure = { 20, 2 } }), { border = "single", padding = 1 })
      end)
      assert(smelt.statusline:rows() == 6)
    "#
    ));
    assert!(app.render_to_frame().text().contains("measured view"));
    assert!(app.run_lua("smelt.statusline:set_view(nil)"));
    app.render_silent();
    assert_eq!(
        app.eval_lua::<u16>("return smelt.statusline:rows()")
            .unwrap(),
        1
    );
}

#[test]
fn source_presentation_does_not_mutate_reused_items() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
      __item = { text = "placed", align_right = true, row = -1 }
      smelt.statusline:clear()
      smelt.statusline:add("placed", function() return __item end, { align_right = false, row = 1 })
    "#
    ));
    let frame = app.render_to_frame();
    assert!(frame.rows.last().unwrap().starts_with("placed"));
    assert!(app
        .eval_lua::<bool>("return __item.align_right and __item.row == -1")
        .unwrap());
}

#[test]
fn width_dependent_bar_height_is_correct_on_the_first_frame() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    assert!(app.run_lua(
        r#"
      smelt.statusline:clear()
      smelt.statusline:add("responsive", function(ctx)
        if ctx.width < 60 then
          return { { text = "responsive first", row = 1 }, { text = "responsive second", row = 2 } }
        end
        return { text = "wide bar" }
      end)
    "#
    ));
    assert!(app.render_to_frame().text().contains("wide bar"));
    assert!(app.run_lua(
        r#"
      local l = smelt.ui.layout
      local default = require("smelt.layout").compose
      local side = smelt.win.new(smelt.buf.new(), { surface = "inert" })
      __views = {}
      smelt.transcript.watch_view(function(view) __views[#__views + 1] = view.viewport.height end)
      l.set(function(state)
        return l.hbox({ { width = "fill", default(state) }, { width = 30, l.leaf(side) } })
      end)
    "#
    ));
    let frame = app.render_to_frame();
    assert!(
        frame.rows[22].contains("responsive first"),
        "{}",
        frame.text()
    );
    assert!(
        frame.rows[23].contains("responsive second"),
        "{}",
        frame.text()
    );
    assert_eq!(app.eval_lua::<Vec<u16>>("return __views").unwrap().len(), 1);
    app.set_terminal_size(100, 24);
    let frame = app.render_to_frame();
    assert!(frame.rows[23].contains("wide bar"), "{}", frame.text());
    assert!(!frame.text().contains("responsive second"));
}

#[test]
fn non_converging_layout_is_bounded_and_reported() {
    let mut app = TestApp::builder().build();
    app.render_silent();
    app.exec_lua_entry(
        r#"
      __passes, __view_calls = 0, 0
      smelt.transcript.watch_view(function() __view_calls = __view_calls + 1 end)
      local default = require("smelt.layout").compose
      smelt.ui.layout.set(function(state)
        __passes = __passes + 1
        smelt.ui.layout.invalidate()
        return default(state)
      end)
    "#,
    )
    .expect("install invalidating composer");
    app.render_silent();
    let passes = app.eval_lua::<u32>("return __passes").unwrap();
    assert!(
        (2..=16).contains(&passes),
        "unbounded layout passes: {passes}"
    );
    assert_eq!(app.eval_lua::<u32>("return __view_calls").unwrap(), 1);
    assert!(app
        .eval_lua::<bool>(
            r#"
      for _, message in ipairs(smelt.messages.list()) do
        if message.full:find("layout did not settle") then return true end
      end
      return false
    "#
        )
        .unwrap());
    assert!(!app.app.lua.shared().layout_refresh_pending());
}

#[test]
fn view_observer_layout_changes_wait_for_the_next_frame() {
    let mut app = TestApp::builder().build();
    app.render_silent();
    app.exec_lua_entry(
        r#"
      __view_calls = 0
      smelt.transcript.watch_view(function(view)
        __view_calls = __view_calls + 1
        __view_height = view.viewport.height
        if __view_calls == 1 then
          smelt.statusline:add("late", function() return { text = "observer row", row = 2 } end)
        end
      end)
    "#,
    )
    .expect("install layout observer");
    app.render_silent();
    let first = app.eval_lua::<u16>("return __view_height").unwrap();
    assert_eq!(app.app.layout.transcript.height, first);
    assert_eq!(app.eval_lua::<u32>("return __view_calls").unwrap(), 1);
    assert!(app.app.lua.shared().layout_refresh_pending());
    let frame = app.render_to_frame();
    assert!(frame.text().contains("observer row"), "{}", frame.text());
    assert_eq!(app.app.layout.transcript.height, first - 1);
    assert_eq!(app.eval_lua::<u32>("return __view_calls").unwrap(), 2);
}

#[test]
fn labels_and_sources_do_not_enable_paint_or_frame_tracking() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
      smelt.statusline:add("plain", function() return { text = "plain" } end)
      smelt.transcript.pills.bottom:add("label", function() return { text = "latest" } end)
      smelt.transcript.pills.bottom:configure({ style = "Comment" })
    "#
    ));
    app.render_silent();
    assert!(app.paint_registry_probe().all_ids().next().is_none());
    assert!(!app.paint_registry_probe().tracks_pointer());
    assert!(app.app.scheduled_frame_delay().is_none());
}

#[test]
fn restored_bar_view_renders_current_source_content() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
      smelt.statusline:clear()
      __label = "before replacement"
      smelt.statusline:add("label", function() return { text = __label } end)
    "#
    ));
    assert!(app.render_to_frame().text().contains("before replacement"));
    assert!(app.run_lua(
        r#"
      local l = smelt.ui.layout
      smelt.statusline:set_view(function()
        local p = smelt.paint.register(function(s) s:put_str(0, 0, "custom view") end)
        return l.leaf(p, { measure = { 0, 1 } })
      end)
      __label = "after replacement"
    "#
    ));
    assert!(app.render_to_frame().text().contains("custom view"));
    assert!(app.run_lua("smelt.statusline:set_view(nil)"));
    let frame = app.render_to_frame();
    assert!(
        frame.text().contains("after replacement"),
        "{}",
        frame.text()
    );
}

#[test]
fn input_constructor_preserves_caller_options() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(
        r#"
      local opts = { name = "preserved.input", text = "hello", pad_left = 2 }
      local input = smelt.input.new(opts)
      assert(input:text() == "hello")
      assert(opts.name == "preserved.input" and opts.text == "hello" and opts.pad_left == 2)
      assert(opts.kind == nil and opts.surface == nil and opts.wrap == nil)
      assert(opts.scrollbar == nil and opts.placeholder == nil and opts.buf_name == nil)
    "#,
    )
    .expect("preserve caller options");
}

#[test]
fn rejected_input_options_do_not_allocate_resources() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(
        r#"
      __input = smelt.input.new({ name = "stable.input", text = "original" })
    "#,
    )
    .expect("create input");
    let before = app.named_resource_counts();
    app.exec_lua_entry(
        r#"
      for i, opts in ipairs({
        { pad_lefft = 2 }, { wrap = "yes" },
        { prefix = { first = 42 } }, { style = { boldd = true } },
      }) do
        opts.name = "invalid.input." .. i
        assert(not pcall(smelt.input.new, opts))
      end
      assert(not pcall(smelt.input.new, {
        name = "stable.input", text = "changed", surface = "typo",
      }))
      assert(__input:text() == "original")
    "#,
    )
    .expect("reject bad inputs");
    assert_eq!(app.named_resource_counts(), before);
}

#[test]
fn view_factories_own_nested_resources_and_registrations() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(r#"
      local l = smelt.ui.layout
      __external_buf = smelt.buf.new({ name = 'view.shared' })
      __external_buf:lines({ 'keep me' })
      __external_win = smelt.win.new(__external_buf, { name = 'view.shared' })
      __external_paint = smelt.paint.register(function() end, { name = 'view.shared' })
      smelt.signal.new('view.scope.test', 0)
      local function factory()
        __scoped_buf = smelt.buf.new({ name = 'view.shared' })
        __scoped_buf:lines({ 'temporary' })
        assert(__scoped_buf ~= __external_buf)
        assert(smelt.buf.new({ name = 'view.shared' }) == __scoped_buf)
        __scoped_win = smelt.win.new(__external_buf, { name = 'view.shared', surface = 'inert' })
        assert(__scoped_win ~= __external_win)
        __timer = smelt.timer.set(60000, function() error('expired view timer fired') end)
        __signal = smelt.signal.subscribe('view.scope.test', function() error('expired view subscription fired') end)
        local nested = smelt.bar.new({ name = 'nested.view' })
        nested:set_view(function()
          __scoped_paint = smelt.paint.register(function(s) s:put_str(0, 0, 'nested view') end, { name = 'view.shared' })
          return l.leaf(__scoped_paint, { measure = { 20, 1 } })
        end)
        return l.hbox({ { nested:node() }, { l.leaf(__scoped_win) } })
      end
      smelt.statusline:set_view(factory)
    "#).expect("construct nested owned view");
    assert!(app.render_to_frame().text().contains("nested view"));
    app.exec_lua_entry(
        r#"
      smelt.statusline:set_view(nil)
      assert(__scoped_win:buf() == nil and not __scoped_paint:invalidate())
      assert(not __timer:remove() and not __signal:remove())
      assert(__external_buf:line(1) == 'keep me')
      assert(__external_win:buf() == __external_buf and __external_paint:invalidate())
    "#,
    )
    .expect("release view without touching external resources");
}

#[test]
fn rejected_view_factory_preserves_active_view_and_cancels_work() {
    let mut app = TestApp::builder().build();
    let renderers = app.app.lua.shared().win_renderers.lock().unwrap().len();
    app.exec_lua_entry(r#"
      local l = smelt.ui.layout
      smelt.statusline:set_view(function()
        __active_view = smelt.paint.register(function(s) s:put_str(0, 0, 'active view') end)
        return l.leaf(__active_view, { measure = { 20, 1 } })
      end)
      local function rejected()
        __failed_timer = smelt.timer.set(60000, function() end)
        __failed_paint = smelt.paint.register(function() end)
        local win = smelt.win.new(smelt.buf.new())
        win:set_renderer(function() end)
        error('reject view')
      end
      assert(not pcall(smelt.statusline.set_view, smelt.statusline, rejected))
      assert(not __failed_timer:remove() and not __failed_paint:invalidate())
      assert(__active_view:invalidate())
      assert(not pcall(smelt.statusline.set_view, smelt.statusline, function() return l.leaf(smelt.prompt.win) end))
      assert(smelt.prompt.win:buf() ~= nil)
    "#).expect("reject construction atomically");
    assert_eq!(
        app.app.lua.shared().win_renderers.lock().unwrap().len(),
        renderers
    );
    assert!(app.render_to_frame().text().contains("active view"));
}

#[test]
fn overlay_reservations_combine_geometry_with_numeric_minimum() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.render_silent();
    app.exec_lua_entry(r#"
      local l = smelt.ui.layout
      local function reserve(height, offset, target)
        local win = smelt.win.new(smelt.buf.new(), { surface = 'inert', scrollbar = false })
        return smelt.overlay.new({ anchor = 'win', target = target or smelt.transcript.win,
          attach = 'nw', row_offset = offset, width = 20, height = height,
          reserve = true, modal = false, border = 'none', layout = l.leaf(win) })
      end
      smelt.transcript.top_inset(2)
      __reserve_a = reserve(2, 0)
      __reserve_b = reserve(3, 1)
      __reserve_other = reserve(8, 0, smelt.prompt.win)
      assert(not pcall(smelt.overlay.new, { anchor = 'center', reserve = true, layout = l.vbox({}) }))
    "#).expect("mount overlapping reservations");
    app.render_silent();
    assert_eq!(app.app.transcript_top_inset(), 4);
    assert!(app.run_lua("__reserve_b:close()"));
    assert_eq!(app.app.transcript_top_inset(), 2);
    assert!(app.run_lua("__reserve_a:close(); smelt.transcript.top_inset(0)"));
    assert_eq!(app.app.transcript_top_inset(), 0);
}

#[test]
fn closed_view_renderers_do_not_accumulate_across_frames() {
    let mut app = TestApp::builder().build();
    app.render_silent();
    let before = app.app.lua.shared().win_renderers.lock().unwrap().len();
    for _ in 0..16 {
        app.exec_lua_entry(
            r#"
          smelt.statusline:set_view(function()
            local win = smelt.win.new(smelt.buf.new(), { surface = 'inert', scrollbar = false })
            win:set_renderer(function() win:buf():lines({ 'temporary renderer' }) end)
            return smelt.ui.layout.leaf(win, { measure = { 20, 1 } })
          end)
        "#,
        )
        .expect("mount retained view renderer");
        assert!(app.render_to_frame().text().contains("temporary renderer"));
        assert!(app.run_lua("smelt.statusline:set_view(nil)"));
        app.render_silent();
    }
    assert_eq!(
        app.app.lua.shared().win_renderers.lock().unwrap().len(),
        before
    );
}

#[test]
fn closing_view_during_render_skips_remaining_closed_renderers() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(
        r#"
      __renderer_calls = 0
      smelt.statusline:set_view(function()
        local l = smelt.ui.layout
        local children = {}
        for i = 1, 2 do
          local win = smelt.win.new(smelt.buf.new(), { surface = 'inert', scrollbar = false })
          win:set_renderer(function()
            __renderer_calls = __renderer_calls + 1
            smelt.statusline:set_view(nil)
          end)
          children[i] = { l.leaf(win, { measure = { 10, 1 } }) }
        end
        return l.hbox(children)
      end)
    "#,
    )
    .expect("close view inside retained renderer");
    app.render_silent();
    assert_eq!(app.eval_lua::<u64>("return __renderer_calls").unwrap(), 1);
}

#[test]
fn window_delayed_invalidation_uses_cancellable_timer() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(
        r#"
      __window_renders = 0
      smelt.statusline:set_view(function()
        __delayed_win = smelt.win.new(smelt.buf.new(), { surface = 'inert', scrollbar = false })
        __delayed_win:set_renderer(function()
          __window_renders = __window_renders + 1
          __delayed_win:buf():lines({ 'timer renderer' })
        end)
        return smelt.ui.layout.leaf(__delayed_win, { measure = { 20, 1 } })
      end)
    "#,
    )
    .expect("mount delayed window renderer");
    app.render_silent();
    let before = app.eval_lua::<u64>("return __window_renders").unwrap();
    assert!(app.run_lua("assert(__delayed_win:invalidate_after(0):remove())"));
    app.tick_timers();
    app.settle_lua();
    app.render_silent();
    assert_eq!(
        app.eval_lua::<u64>("return __window_renders").unwrap(),
        before
    );
    assert!(app.run_lua("__delayed_win:invalidate_after(0)"));
    app.tick_timers();
    app.settle_lua();
    app.render_silent();
    assert_eq!(
        app.eval_lua::<u64>("return __window_renders").unwrap(),
        before + 1
    );
}

#[test]
fn replacement_views_can_reuse_their_retained_split() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(
        r#"
      local l = smelt.ui.layout
      __retained_split = l.split('horizontal')
      __split_view = function()
        local a, b = smelt.buf.new(), smelt.buf.new()
        a:lines({ 'retained split' }); b:lines({ 'second pane' })
        __split_a = smelt.win.new(a, { surface = 'inert', scrollbar = false })
        __split_b = smelt.win.new(b, { surface = 'inert', scrollbar = false })
        return __retained_split:layout(l.leaf(__split_a), l.leaf(__split_b))
      end
      smelt.statusline:set_view(__split_view)
      __unrelated_split = l.split('horizontal')
      local default = require('smelt.layout').compose
      l.set(function(state)
        return __unrelated_split:layout(default(state), l.vbox({}))
      end)
    "#,
    )
    .expect("mount retained split view");
    app.render_silent();
    app.exec_lua_entry(r#"
      local old = __split_a
      smelt.statusline:set_view(__split_view)
      assert(old:buf() == nil and __split_a:buf() ~= nil)
      local l, active = smelt.ui.layout, __split_a
      local function leaf()
        return l.leaf(smelt.win.new(smelt.buf.new(), { surface = 'inert' }))
      end
      local function rejects(factory)
        local ok, err = pcall(function() smelt.statusline:set_view(factory) end)
        assert(not ok and tostring(err):find('split handle can be mounted only once'), tostring(err))
        assert(active:buf() ~= nil)
      end
      rejects(function() return __unrelated_split:layout(leaf(), leaf()) end)
      rejects(function()
        return l.hbox({
          { __retained_split:layout(leaf(), leaf()) },
          { __retained_split:layout(leaf(), leaf()) },
        })
      end)
    "#).expect("replace only owned splits without allowing duplicate mounts");
    assert!(app.render_to_frame().text().contains("retained split"));
}

#[test]
fn rejected_view_factory_removes_decorations_on_external_windows() {
    let mut app = TestApp::builder().build();
    app.render_silent();
    let trees = app.app.ui.layout_trees().count();
    app.exec_lua_entry(
        r#"
      local ok = pcall(function()
        smelt.statusline:set_view(function()
          local paint = smelt.paint.register(function() end)
          smelt.prompt.win:decorate({ layout = smelt.ui.layout.leaf(paint) })
          error('reject decorated view')
        end)
      end)
      assert(not ok and smelt.prompt.win:buf() ~= nil)
    "#,
    )
    .expect("reject view after attaching decoration");
    assert_eq!(app.app.ui.layout_trees().count(), trees);
    app.exec_lua_entry(
        r#"
      smelt.statusline:set_view(function()
        local l = smelt.ui.layout
        local paint = smelt.paint.register(function() end)
        smelt.prompt.win:decorate({ layout = l.leaf(paint) })
        return l.vbox({})
      end)
    "#,
    )
    .expect("mount view with scoped decoration");
    assert_eq!(app.app.ui.layout_trees().count(), trees + 1);
    assert!(app.run_lua("smelt.statusline:set_view(nil)"));
    assert_eq!(app.app.ui.layout_trees().count(), trees);
}

#[test]
fn view_factories_cannot_open_dialogs() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(
        r#"
      local ok, err = pcall(function()
        smelt.statusline:set_view(function()
          local panel = smelt.dialog.content({ text = 'unexpected dialog' })
          smelt.dialog.new({ layout = smelt.ui.layout.leaf(panel) })
          error('reject dialog view')
        end)
      end)
      assert(not ok and tostring(err):find('open dialogs outside construction'), tostring(err))
      assert(smelt.dialog.current() == nil, 'dialog escaped rejected view construction')
      local panel = smelt.dialog.content({ text = 'normal dialog' })
      local dialog = smelt.dialog.new({ layout = smelt.ui.layout.leaf(panel) })
      assert(smelt.dialog.current() ~= nil)
      dialog:close()
      assert(smelt.dialog.current() == nil)
    "#,
    )
    .expect("keep dialog hosting outside view construction");
    assert!(app.app.ui.active_docked_surface().is_none());
}

#[test]
fn style_merge_preserves_references_false_values_and_inputs() {
    let mut app = TestApp::builder().build();
    app.exec_lua_entry(
        r#"
      local base = { hl_group = 'Comment', fg = 'SmeltAccent', bold = true }
      local patch = { bg = false, bold = false, underline = true }
      local merged = smelt.style.merge(base, patch)
      assert(merged.hl_group == 'Comment' and merged.fg == 'SmeltAccent')
      assert(merged.bg == false and merged.bold == false and merged.underline)
      assert(base.bg == nil and base.bold == true and patch.hl_group == nil)
      assert(smelt.style.merge('Comment', { dim = true }).hl_group == 'Comment')
      assert(smelt.style.resolve(merged).bg == false)
      assert(not pcall(smelt.style.merge, base, { boldd = true }))
    "#,
    )
    .expect("compose styles without freezing theme references");
}
