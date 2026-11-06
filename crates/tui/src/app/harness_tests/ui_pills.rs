use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

fn conversation() -> TestApp {
    let mut app = TestApp::builder()
        .with_ephemeral(true)
        .with_vim(true)
        .build();
    app.set_terminal_size(80, 24);
    for i in 0..20 {
        app.push_transcript_block(smelt_core::transcript_model::Block::User {
            text: format!("user target {i:02}"),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: if i == 19 {
                (0..50)
                    .map(|n| format!("assistant line {n}"))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into()
            } else {
                format!("answer {i}").into()
            },
        });
    }
    app.render_silent();
    app.focus_transcript();
    app.type_char('G');
    assert!(app.run_lua("smelt.prompt.win:focus()"));
    app.render_silent();
    app
}

fn click(app: &mut TestApp, expression: &str) {
    let (x, y) = app
        .eval_lua::<(u16, u16)>(&format!("local r = {expression}:rect(); return r.x, r.y"))
        .unwrap();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })));
    }
    app.render_silent();
}

#[test]
fn custom_bar_press_handlers_exist_only_while_enabled() {
    let mut app = TestApp::builder().build();
    app.render_silent();
    let callbacks = app.tui.lua.shared().callbacks.lock().unwrap().len();
    app.exec_lua_entry(
        r#"
      smelt.statusline:set_view(function()
        local l = smelt.ui.layout
        __press_win = smelt.win.new(smelt.buf.new(), { surface = 'inert', scrollbar = false })
        __press_paint = smelt.paint.register(function(s) s:put_str(0, 0, 'click') end)
        __leaf_presses, __bar_presses = 0, 0
        __press_win:on('press', function() __leaf_presses = __leaf_presses + 1 end)
        return l.hbox({
          { l.leaf(__press_win, { measure = { 10, 1 } }) },
          { l.leaf(__press_paint, { measure = { 10, 1 } }) },
        })
      end)
    "#,
    )
    .expect("mount custom bar without bar-level press handlers");
    app.render_silent();
    assert_eq!(
        app.tui.lua.shared().callbacks.lock().unwrap().len(),
        callbacks + 2
    );
    assert!(app.run_lua("smelt.statusline:configure({ on_press = function() __bar_presses = __bar_presses + 1 end })"));
    app.render_silent();
    assert_eq!(
        app.tui.lua.shared().callbacks.lock().unwrap().len(),
        callbacks + 4
    );
    click(&mut app, "__press_win");
    click(&mut app, "__press_paint");
    assert_eq!(app.eval_lua::<u32>("return __bar_presses").unwrap(), 2);
    assert!(app.run_lua("smelt.statusline:configure({ on_press = false })"));
    app.render_silent();
    assert_eq!(
        app.tui.lua.shared().callbacks.lock().unwrap().len(),
        callbacks + 2
    );
    click(&mut app, "__press_win");
    assert_eq!(app.eval_lua::<u32>("return __leaf_presses").unwrap(), 2);
    assert_eq!(app.eval_lua::<u32>("return __bar_presses").unwrap(), 2);
    assert!(app.run_lua("smelt.statusline:set_view(nil)"));
    app.render_silent();
    assert_eq!(
        app.tui.lua.shared().callbacks.lock().unwrap().len(),
        callbacks
    );
}

#[test]
fn failed_pill_remount_preserves_active_presentation() {
    let mut app = conversation();
    assert!(app.run_lua("local w = smelt.transcript.win; w:scroll(w:scroll().top - 1)"));
    app.render_silent();
    let buffers = app.tui.ui.iter_bufs().count();
    let renderers = app.tui.lua.shared().win_renderers.lock().unwrap().len();
    app.exec_lua_entry(
        r#"
      local bar, l = smelt.transcript.pills.bottom, smelt.ui.layout
      local old = bar.win
      local factory = function()
        __candidate_pill = smelt.paint.register(function(s) s:put_str(0, 0, 'active candidate') end)
        return l.leaf(__candidate_pill, { measure = { 20, 1 } })
      end
      bar.host_opts.width = 'invalid'
      assert(not pcall(bar.set_view, bar, factory))
      assert(bar.win == old and old:buf() ~= nil and not __candidate_pill:invalidate())
      bar.host_opts.width = nil
      bar:set_view(factory)
      assert(old:buf() == nil and bar.win == nil)
      bar.host_opts.width = 'invalid'
      assert(not pcall(bar.set_view, bar, nil))
      assert(bar.view_factory == factory and bar.win == nil and __candidate_pill:invalidate())
      bar.host_opts.width = nil
    "#,
    )
    .expect("keep old presentation until replacement mount succeeds");
    assert_eq!(app.tui.ui.iter_bufs().count(), buffers);
    assert_eq!(
        app.tui.lua.shared().win_renderers.lock().unwrap().len(),
        renderers - 1
    );
    assert!(app.render_to_frame().text().contains("active candidate"));
    click(&mut app, "__candidate_pill");
    assert!(app.transcript_window().following_tail);
}

#[test]
fn custom_pill_subtrees_keep_navigation_and_release_owned_paints() {
    let mut app = conversation();
    app.exec_lua_entry(
        r#"
      local l = smelt.ui.layout
      __builds = 0
      smelt.transcript.pills.top:set_view(function(ctx)
        __builds = __builds + 1
        __top_paint = smelt.paint.register(function(s)
          local state = ctx.state
          s:put_str(0, 0, state and state.label or "")
        end)
        local buf = smelt.buf.new()
        buf:lines({ "previous message" })
        __top_button = smelt.win.new(buf, { surface = "inert", scrollbar = false })
        return l.vbox({
          { height = 1, l.leaf(__top_paint, { measure = { 24, 1 } }) },
          { height = 1, l.leaf(__top_button, { measure = { 24, 1 } }) },
        })
      end)
      smelt.transcript.pills.bottom:set_view(function()
        __bottom_paint = smelt.paint.register(function(s) s:put_str(0, 0, "go to tail") end)
        return l.leaf(__bottom_paint, { measure = { 12, 1 } })
      end)
    "#,
    )
    .expect("customize pill views");
    let frame = app.render_to_frame();
    assert!(
        frame.text().contains("previous message"),
        "{}",
        frame.text()
    );
    let tail = app.transcript_window().scroll_top;
    click(&mut app, "__top_button");
    assert!(app.transcript_window().scroll_top < tail);
    assert!(app.render_to_frame().text().contains("go to tail"));
    click(&mut app, "__bottom_paint");
    assert!(app.transcript_window().following_tail);
    assert!(!app
        .eval_lua::<bool>("return __bottom_paint:invalidate()")
        .unwrap());

    assert!(app.run_lua(
        r#"
      __old = __top_paint
      smelt.transcript.pills.top:configure({ visible = false })
      assert(not __old:invalidate())
      smelt.transcript.pills.top:configure({ visible = true })
    "#
    ));
    let frame = app.render_to_frame();
    assert!(
        frame.text().contains("previous message"),
        "{}",
        frame.text()
    );
    assert!(app.eval_lua::<u32>("return __builds").unwrap() >= 2);
    app.set_terminal_size(36, 16);
    assert!(app.render_to_frame().text().contains("previous message"));
}

#[test]
fn growing_custom_pill_keeps_navigation_below_current_geometry() {
    let mut app = conversation();
    app.exec_lua_entry(
        r#"
      local l = smelt.ui.layout
      smelt.transcript.pills.top:set_view(function(ctx)
        __pill_measure = l.measure(24, 1)
        __growing_pill = smelt.paint.register(function(s)
          s:put_str(0, 0, "previous message")
        end)
        return l.leaf(__growing_pill, { measure = __pill_measure })
      end)
    "#,
    )
    .expect("mount growing pill");
    app.render_silent();
    assert!(app.run_lua("__pill_measure:set(24, 3)"));
    app.render_silent();
    let (y, height) = app
        .eval_lua::<(usize, usize)>("local r = __growing_pill:rect(); return r.y, r.height")
        .unwrap();
    assert_eq!(height, 3);
    click(&mut app, "__growing_pill");
    let frame = app.render_to_frame();
    assert!(
        frame
            .rows
            .iter()
            .skip(y + height)
            .any(|row| row.contains("user target 19")),
        "{}",
        frame.text()
    );
}

#[test]
fn rejected_pill_mount_releases_owned_leaves() {
    let mut app = conversation();
    let buffers = app.tui.ui.iter_bufs().count();
    app.exec_lua_entry(r#"
      local l = smelt.ui.layout
      for _, stop in ipairs({ true, false }) do
        local ok, err = pcall(function()
          smelt.transcript.pills.top:set_view(function(ctx)
            __rejected_paint = smelt.paint.register(function() end)
            __rejected_window = smelt.win.new(smelt.buf.new(), { surface = "inert" })
            if stop then error("factory stopped") end
            local split = l.split("horizontal")
            local node = split:layout(l.leaf(__rejected_window), l.leaf(__rejected_paint, { measure = { 10, 1 } }))
            return l.vbox({ { node }, { node } })
          end)
        end)
        assert(not ok, "invalid pill mount accepted")
        assert(tostring(err):find(stop and "factory stopped" or "split"), tostring(err))
        assert(not __rejected_paint:invalidate(), "rejected Paint leaked")
        assert(__rejected_window:buf() == nil, "rejected window leaked")
        smelt.transcript.pills.top:set_view(nil)
      end
    "#).expect("roll back rejected pill mount");
    assert!(app.paint_registry_probe().all_ids().next().is_none());
    assert_eq!(app.tui.ui.iter_bufs().count(), buffers);
}

#[test]
fn unchanged_pill_labels_do_not_rewrite_buffers_on_scroll() {
    let mut app = conversation();
    let scroll = "local w = smelt.transcript.win; w:scroll(w:scroll().top - 1)";
    assert!(app.run_lua(scroll));
    app.render_silent();
    let ids = ["top", "bottom"].map(|kind| {
        app.ui_probe()
            .named_buf(&format!("smelt.scroll_pills.{kind}.buf"))
            .unwrap()
    });
    let ticks = ids.map(|id| app.tui.ui.buf(id).unwrap().changedtick());
    assert!(app.run_lua(scroll));
    app.render_silent();
    assert_eq!(
        ids.map(|id| app.tui.ui.buf(id).unwrap().changedtick()),
        ticks
    );
}

#[test]
fn fitted_pill_sources_settle_height_for_their_actual_width() {
    let mut app = conversation();
    app.exec_lua_entry(
        r#"
      local bar = smelt.transcript.pills.bottom
      bar:clear()
      bar:add('responsive', function(ctx)
        if ctx.width < 20 then
          return { { text = 'narrow', row = 1 }, { text = 'second', row = 2 } }
        end
        return { text = 'narrow' }
      end)
    "#,
    )
    .expect("install responsive fitted source");
    assert!(app.run_lua("local w = smelt.transcript.win; w:scroll(w:scroll().top - 1)"));
    let frame = app.render_to_frame();
    assert!(
        frame.text().contains("narrow") && frame.text().contains("second"),
        "{}",
        frame.text()
    );
    let size = app
        .eval_lua::<(u16, u16)>(
            "local r = smelt.transcript.pills.bottom.win:rect(); return r.width, r.height",
        )
        .unwrap();
    assert_eq!(size, (6, 2));
}

#[test]
fn pill_sources_receive_new_view_state_when_label_is_unchanged() {
    let mut app = conversation();
    app.exec_lua_entry(
        r#"
      smelt.transcript.pills.top:add('label', function(ctx)
        __source_revision = ctx.state.view.revision
        __source_label = ctx.state.label
        return { text = 'view revision ' .. __source_revision }
      end)
    "#,
    )
    .expect("replace label with state-dependent source");
    app.render_silent();
    let before = app.eval_lua::<u64>("return __source_revision").unwrap();
    let label = app.eval_lua::<String>("return __source_label").unwrap();
    assert!(app.run_lua("local w = smelt.transcript.win; w:scroll(w:scroll().top - 1)"));
    app.render_silent();
    assert_eq!(
        app.eval_lua::<String>("return __source_label").unwrap(),
        label
    );
    assert!(app.eval_lua::<u64>("return __source_revision").unwrap() > before);
}

#[test]
fn pill_label_style_and_visibility_are_independent() {
    let mut app = conversation();
    app.exec_lua_entry(
        r##"
      smelt.transcript.pills.top:add("label", function(ctx)
        return { text = "previous: " .. ctx.state.label }
      end)
      smelt.transcript.pills.top:configure({
        style = { fg = "#abcdef", bg = "#123456", underline = true },
      })
    "##,
    )
    .expect("configure top label");
    let frame = app.render_to_frame();
    let (y, row) = frame
        .rows
        .iter()
        .enumerate()
        .find(|(_, row)| row.contains("previous: user target"))
        .unwrap_or_else(|| panic!("{}", frame.text()));
    let x = row.find("previous:").unwrap();
    assert!(frame.styles[y][x].underline);
    assert_eq!(
        frame.styles[y][x].fg,
        Some(smelt_core::style::Color::Rgb {
            r: 0xab,
            g: 0xcd,
            b: 0xef
        })
    );
    assert!(app.run_lua("smelt.transcript.pills.top:configure({ visible = false })"));
    assert!(!app.render_to_frame().text().contains("previous:"));
    assert!(app.run_lua("smelt.transcript.pills.top:reset(); smelt.transcript.pills.top:add('label', smelt.transcript.pills.top:builtin('label'))"));
    let restored = app.render_to_frame();
    assert!(!restored.text().contains("previous:"));
    assert!(
        restored.text().contains("user target 19"),
        "{}",
        restored.text()
    );
}
