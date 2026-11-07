use super::*;
use crossterm::event::{MouseEvent, MouseEventKind};

fn moved(x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Moved,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

fn pointer_move(app: &mut TestApp, x: u16, y: u16) {
    app.feed_one(SourceEvent::Term(Event::Mouse(moved(x, y))));
}

/// Whether the event loop paints a frame for this motion event.
fn motion_redraws(app: &mut TestApp, x: u16, y: u16) -> bool {
    app.tui.event_needs_frame(&Event::Mouse(moved(x, y)))
}

fn overlay_paint(app: &mut TestApp, body: &str, opts: &str) {
    assert!(app.run_lua(&format!(
        r#"
        _G.__paint = smelt.paint.register(function(slice, ctx) {body} end, {opts})
        smelt.overlay.new({{
            anchor = "screen_at", corner = "nw", row = 2, col = 4,
            width = 6, height = 2, border = "none", modal = false,
            layout = smelt.ui.layout.leaf(_G.__paint),
        }})
        "#
    )));
}

#[test]
fn frame_paint_recolors_real_transcript_text() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    app.push_transcript_block(smelt_transcript::transcript_model::Block::Text {
        content: "melting text".into(),
    });
    assert!(app.run_lua(
        r#"
        _G.__fx = smelt.paint.frame(function(slice)
            slice:map(function(cell)
                if cell.ch == "m" then
                    return { ch = "M", style = { fg = { 1, 2, 3 } } }
                end
            end)
        end, { name = "test.fx" })
        "#
    ));

    let frame = app.render_to_frame();
    let (y, row) = frame
        .rows
        .iter()
        .enumerate()
        .find(|(_, row)| row.contains("Melting text"))
        .unwrap_or_else(|| panic!("frame paint should rewrite real text:\n{}", frame.text()));
    let x = row.chars().position(|c| c == 'M').unwrap();
    assert_eq!(
        frame.styles[y][x].fg,
        Some(smelt_core::style::Color::Rgb { r: 1, g: 2, b: 3 })
    );

    assert!(app.run_lua("_G.__fx:remove()"));
    let frame = app.render_to_frame();
    assert!(frame.text().contains("melting text"), "{}", frame.text());
}

#[test]
fn frame_paint_sees_the_visible_cursor() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(40, 12);
    app.type_text("abc");
    assert!(app.run_lua(
        r#"
        smelt.paint.frame(function(slice, ctx)
          _G.__cursor = ctx.cursor
        end)
        "#
    ));

    let frame = app.render_to_frame();

    let (x, y) = app
        .eval_lua::<(u16, u16)>("return _G.__cursor.x, _G.__cursor.y")
        .expect("frame paints see the cursor");
    let prompt_row = frame
        .rows
        .iter()
        .position(|row| row.contains("abc"))
        .expect("prompt row");
    assert_eq!(y as usize, prompt_row);
    assert_eq!(
        frame.rows[prompt_row]
            .chars()
            .position(|c| c == 'c')
            .unwrap()
            + 1,
        x as usize,
        "the cursor sits after the typed text"
    );
}

#[test]
fn theme_rgb_resolves_every_color_through_the_palette() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r##"
        local function rgb(c) local t = smelt.theme.rgb(c); return t[1] .. "," .. t[2] .. "," .. t[3] end
        assert(rgb(196) == "255,0,0", rgb(196))
        assert(rgb(244) == "128,128,128", rgb(244))
        assert(rgb("#102030") == "16,32,48")
        assert(rgb("red") == "255,0,0")
        assert(rgb(nil) == "229,229,229", rgb(nil))
        assert(rgb("reset") == rgb(nil))
        assert(table.concat(smelt.theme.rgb(nil, "bg"), ",") == "0,0,0")
        assert(rgb(false) == rgb(nil))
        assert(rgb({ 10.4, -3, 300 }) == "10,0,255")
        smelt.theme.set("TestTint", { fg = { 1, 2, 3 }, bg = 196 })
        assert(rgb("TestTint") == "1,2,3")
        assert(table.concat(smelt.theme.rgb("TestTint", "bg"), ",") == "255,0,0")
        "##
    ));
}

#[test]
fn frame_effect_cannot_be_placed_in_a_layout() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(
        r#"
        _G.__calls = 0
        local fx = smelt.paint.frame(function() _G.__calls = _G.__calls + 1 end)
        local ok = pcall(function() smelt.ui.layout.leaf(fx) end)
        assert(not ok, "a frame effect is not a leaf")
        assert(fx.rect == nil and fx.on == nil)
        "#
    ));

    app.render_silent();

    assert_eq!(app.eval_lua::<u32>("return _G.__calls").unwrap(), 1);
}

#[test]
fn leaf_paint_reads_and_restyles_what_is_under_it() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(
        r#"
        _G.__under = smelt.paint.register(function(slice)
            slice:put_str(0, 0, "abc", { bg = { 9, 9, 9 } })
        end)
        _G.__over = smelt.paint.register(function(slice)
            _G.__seen = slice:get(1, 0).ch
            slice:patch_style(0, 0, 3, 1, { bold = true })
        end)
        for i, paint in ipairs({ _G.__under, _G.__over }) do
            smelt.overlay.new({
                anchor = "screen_at", corner = "nw", row = 0, col = 0,
                width = 3, height = 1, border = "none", modal = false, z = 100 + i,
                transparent = i == 2,
                layout = smelt.ui.layout.leaf(paint),
            })
        end
        "#
    ));

    let frame = app.render_to_frame();

    assert_eq!(app.eval_lua::<String>("return _G.__seen").unwrap(), "b");
    assert!(frame.rows[0].starts_with("abc"), "{}", frame.text());
    for x in 0..3 {
        let style = frame.styles[0][x];
        assert!(style.bold, "cell {x} should be restyled");
        assert_eq!(
            style.bg,
            Some(smelt_core::style::Color::Rgb { r: 9, g: 9, b: 9 }),
            "cell {x} keeps its background"
        );
    }
}

#[test]
fn invalidate_after_animates_while_the_paint_asks() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    app.render_silent();
    assert!(app.tui.scheduled_frame_delay().is_none());

    assert!(app.run_lua(
        r#"_G.__on = true
        _G.__anim = smelt.paint.frame(function()
          if _G.__on then _G.__anim:invalidate_after(20) end
        end, { name = "test.anim" })"#
    ));
    app.render_silent();
    let delay = app
        .tui
        .scheduled_frame_delay()
        .expect("a paint that asks keeps frames coming");
    assert!(delay <= Duration::from_millis(20), "{delay:?}");
    app.render_silent();
    assert!(app.tui.scheduled_frame_delay().is_some());

    assert!(app.run_lua("_G.__on = false"));
    app.render_silent();
    assert!(
        app.tui.scheduled_frame_delay().is_none(),
        "a paint that stops asking goes idle"
    );

    assert!(app.run_lua("_G.__anim:remove()"));
    assert!(!app
        .eval_lua::<bool>("return _G.__anim:invalidate_after(20)")
        .unwrap());
}

#[test]
fn pointer_motion_is_ignored_without_listeners() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    app.render_silent();

    assert!(
        !motion_redraws(&mut app, 5, 3),
        "pointer motion must not redraw when nothing listens"
    );

    assert!(app.run_lua(r#"_G.__reg = smelt.transcript.win:on("enter", function() end)"#));
    assert!(motion_redraws(&mut app, 5, 3), "a hover listener opts in");
    assert!(app.run_lua("_G.__reg:remove()"));
    assert!(
        !motion_redraws(&mut app, 5, 3),
        "removing it opts out again"
    );
}

#[test]
fn hover_events_fire_enter_move_leave_with_leaf_coords() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    overlay_paint(&mut app, "", "nil");
    assert!(app.run_lua(
        r#"
        _G.__events = {}
        for _, name in ipairs({ "enter", "move", "leave" }) do
            _G.__paint:on(name, function(ev)
                table.insert(_G.__events, ("%s:%d,%d"):format(name, ev.x, ev.y))
            end)
        end
        "#
    ));
    app.render_silent();

    pointer_move(&mut app, 0, 0);
    pointer_move(&mut app, 5, 3);
    pointer_move(&mut app, 6, 3);
    pointer_move(&mut app, 20, 8);

    assert_eq!(
        app.eval_lua::<String>("return table.concat(_G.__events, ' ')")
            .unwrap(),
        "enter:1,1 move:1,1 move:2,1 leave:16,6"
    );
}

#[test]
fn pointer_paint_sees_leaf_relative_pointer() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    overlay_paint(
        &mut app,
        "_G.__pointer = ctx.pointer and (ctx.pointer.x .. ',' .. ctx.pointer.y)",
        "{ pointer = true }",
    );
    app.render_silent();

    assert!(
        motion_redraws(&mut app, 7, 3),
        "a pointer paint redraws on motion"
    );
    pointer_move(&mut app, 7, 3);
    app.render_silent();

    assert_eq!(
        app.eval_lua::<String>("return _G.__pointer").unwrap(),
        "3,1"
    );
}

#[test]
fn invalidate_requests_one_frame() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(r#"_G.__fx = smelt.paint.frame(function() end)"#));
    app.render_silent();
    assert!(app.tui.scheduled_frame_delay().is_none());

    assert!(app.eval_lua::<bool>("return _G.__fx:invalidate()").unwrap());
    assert!(
        app.tui.scheduled_frame_delay().is_some(),
        "invalidate asks for a frame"
    );
    app.render_silent();
    assert!(
        app.tui.scheduled_frame_delay().is_none(),
        "invalidate is one frame, not an animation"
    );

    assert!(app.run_lua("_G.__fx:remove()"));
    assert!(!app.eval_lua::<bool>("return _G.__fx:invalidate()").unwrap());
}

#[test]
fn frame_paints_run_by_order() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(
        r#"
        _G.__runs = {}
        local function fx(name) return function() table.insert(_G.__runs, name) end end
        smelt.paint.frame(fx("top"), { order = 10 })
        smelt.paint.frame(fx("first"))
        smelt.paint.frame(fx("under"), { order = -1 })
        smelt.paint.frame(fx("second"))
        "#
    ));

    app.render_silent();

    assert_eq!(
        app.eval_lua::<String>("return table.concat(_G.__runs, ' ')")
            .unwrap(),
        "under first second top"
    );
}

#[test]
fn history_shows_the_previous_frame_before_effects_and_what_changed() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(
        r#"
        _G.__seen = {}
        smelt.paint.frame(function(slice, ctx)
          local damaged = 0
          for _, rect in ipairs(ctx.damage) do damaged = damaged + rect.width * rect.height end
          table.insert(_G.__seen, {
            prev = ctx.prev and ctx.prev:get(0, 0).ch,
            damaged = damaged,
            area = slice:width() * slice:height(),
          })
          slice:set(0, 0, "@")
        end, { history = true })
        smelt.paint.frame(function(_, ctx) _G.__plain = ctx.prev == nil and ctx.damage == nil end)
        "#
    ));

    app.render_silent();
    app.render_silent();
    app.type_text("x");
    app.render_silent();

    let mut seen = |i: u32, key: &str| {
        app.eval_lua::<mlua::Value>(&format!("return _G.__seen[{i}].{key}"))
            .unwrap()
    };
    assert!(seen(1, "prev").is_nil(), "the first frame has no previous");
    assert_eq!(seen(1, "damaged").as_u32(), seen(1, "area").as_u32());
    assert_eq!(
        seen(2, "prev").as_string().unwrap().to_str().unwrap(),
        " ",
        "prev holds smelt's frame, not the effect drawn over it"
    );
    assert_eq!(seen(2, "damaged").as_u32(), Some(0), "nothing changed");
    let typed = seen(3, "damaged").as_u32().unwrap();
    assert!(
        typed > 0 && typed < 480 / 4,
        "typing damages a few rows: {typed}"
    );
    assert!(app.eval_lua::<bool>("return _G.__plain").unwrap());
}

#[test]
fn cells_read_and_written_back_keep_the_default_background() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(40, 12);
    app.type_text("abc");
    assert!(app.run_lua(
        r#"
        smelt.paint.frame(function(slice, ctx)
          local y = ctx.cursor.y
          slice:fill(0, y + 1, 3, 1, " ", { bg = 196 })
          -- Move the typed text one row down, onto the red band.
          for x = 0, 2 do
            local cell = slice:get(ctx.cursor.x - 3 + x, y)
            _G.__bg = cell.style.bg
            slice:set(x, y + 1, cell.ch, cell.style)
          end
        end)
        "#
    ));
    let frame = app.render_to_frame();
    assert_eq!(
        app.eval_lua::<mlua::Value>("return _G.__bg").unwrap(),
        mlua::Value::Boolean(false),
        "the terminal background reads as false"
    );
    let (y, _) = frame
        .rows
        .iter()
        .enumerate()
        .find(|(_, row)| row.starts_with("abc"))
        .unwrap_or_else(|| panic!("the copied text shows:\n{}", frame.text()));
    for x in 0..3 {
        assert_eq!(
            frame.styles[y][x].bg, None,
            "a copied cell keeps its own background, not the band under it"
        );
    }
}

#[test]
fn frame_paints_see_terminal_focus_and_repaint_when_it_changes() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(r#"smelt.paint.frame(function(_, ctx) _G.__focused = ctx.focused end)"#));
    app.render_silent();
    assert!(app.eval_lua::<bool>("return _G.__focused").unwrap());

    app.feed_one(SourceEvent::Term(Event::FocusLost));
    assert!(
        app.tui.scheduled_frame_delay().is_some(),
        "losing focus asks for a frame"
    );
    app.render_silent();
    assert!(!app.eval_lua::<bool>("return _G.__focused").unwrap());

    app.feed_one(SourceEvent::Term(Event::FocusGained));
    app.render_silent();
    assert!(app.eval_lua::<bool>("return _G.__focused").unwrap());
}

#[test]
fn any_window_that_scrolls_is_reported_and_taken_out_of_damage() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 24);
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new()
        local lines = {}
        for i = 1, 60 do lines[i] = "line " .. i end
        buf:lines(lines)
        float = smelt.win.new(buf, { surface = 'readonly_text', wrap = false })
        smelt.overlay.new({ layout = smelt.ui.layout.leaf(float), anchor = 'center',
          width = 30, height = 10, border = 'none' })
        smelt.paint.frame(function(_, ctx)
          local inside = 0
          local s = ctx.scrolls[1]
          for _, d in ipairs(ctx.damage) do
            if s and d.y >= s.y and d.y < s.y + s.height and d.x >= s.x and d.x < s.x + s.width then
              inside = inside + 1
            end
          end
          _G.__last = { count = #ctx.scrolls, dy = s and s.dy, height = s and s.height, inside = inside,
            is_float = s and s.win == float and s.win ~= smelt.transcript.win }
        end, { history = true })
        "#
    ));
    app.render_silent();
    app.render_silent();
    assert!(app.run_lua("float:scroll(3)"));
    app.render_silent();

    let last = app.eval_lua::<mlua::Table>("return _G.__last").unwrap();
    assert_eq!(
        last.get::<i32>("count").unwrap(),
        1,
        "only the float scrolled"
    );
    assert!(
        last.get::<bool>("is_float").unwrap(),
        "the scroll names the float"
    );
    assert_eq!(last.get::<i32>("dy").unwrap(), 3);
    assert!(last.get::<i32>("height").unwrap() > 3);
    assert_eq!(
        last.get::<i32>("inside").unwrap(),
        3,
        "only the three rows that scrolled in are damage"
    );
}

#[test]
fn scrolling_reports_the_move_and_leaves_only_new_rows_as_damage() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 16);
    for i in 0..40 {
        app.push_transcript_block(smelt_transcript::transcript_model::Block::Text {
            content: format!("reply number {i}").into(),
        });
    }
    assert!(app.run_lua(
        r#"
        smelt.paint.frame(function(slice, ctx)
          local s = ctx.scrolls[1]
          assert(not s or s.win == smelt.transcript.win)
          local damaged = {}
          for _, d in ipairs(ctx.damage) do
            if s and d.y >= s.y and d.y < s.y + s.height then
              local text = {}
              for x = d.x, d.x + d.width - 1 do text[#text + 1] = slice:get(x, d.y).ch end
              damaged[#damaged + 1] = { y = d.y - s.y, text = table.concat(text) }
            end
          end
          -- Every block on screen must agree on which display row is at the top.
          local origins, agree = {}, 0
          for _, b in ipairs(smelt.transcript.visible_blocks()) do
            local origin = b.rect and b.rect.y - b.top - b.first_row
            if origin and not origins[origin] then origins[origin], agree = true, agree + 1 end
          end
          _G.__last = { dy = s and s.dy, damaged = damaged, agree = agree }
        end, { history = true })
        "#
    ));
    app.render_silent();
    app.render_silent();

    let view = app
        .eval_lua::<mlua::Table>("return smelt.transcript.win:rect()")
        .unwrap();
    let (x, y): (u16, u16) = (view.get("x").unwrap(), view.get("y").unwrap());
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: x + 2,
        row: y + 2,
        modifiers: KeyModifiers::NONE,
    })));
    app.render_silent();

    let last = app.eval_lua::<mlua::Table>("return _G.__last").unwrap();
    let dy = last
        .get::<Option<i32>>("dy")
        .unwrap()
        .expect("a wheel scroll reports ctx.scrolls");
    assert!(dy < 0, "scrolling up moves the text down: {dy}");
    let damaged: Vec<(i32, String)> = last
        .get::<mlua::Table>("damaged")
        .unwrap()
        .sequence_values::<mlua::Table>()
        .map(|d| {
            let d = d.unwrap();
            (d.get("y").unwrap(), d.get("text").unwrap())
        })
        .collect();
    assert!(
        damaged
            .iter()
            .any(|(_, text)| text.contains("reply number")),
        "the reply that scrolled in is damage: {damaged:?}"
    );
    for (row, text) in &damaged {
        assert!(
            *row < -dy || text.contains("jump to bottom"),
            "only rows that scrolled in at the top, and the jump pill that appeared, are damage: {damaged:?}"
        );
    }
    assert_eq!(
        last.get::<i32>("agree").unwrap(),
        1,
        "top is exact for every block"
    );

    app.render_silent();
    let last = app.eval_lua::<mlua::Table>("return _G.__last").unwrap();
    assert!(
        last.get::<Option<i32>>("dy").unwrap().is_none(),
        "a still frame has no scroll"
    );
}

#[test]
fn visible_blocks_report_screen_rects_and_streaming() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 30);
    app.push_transcript_block(smelt_transcript::transcript_model::Block::Text {
        content: "settled reply".into(),
    });
    app.start_submitted_turn("question");
    app.dispatch_engine_event(protocol::EngineEvent::TextDelta {
        delta: "streaming words".into(),
    });
    assert!(app.run_lua(
        r#"
        smelt.paint.frame(function(slice)
          _G.__blocks = {}
          for _, block in ipairs(smelt.transcript.visible_blocks()) do
            local r = block.rect
            if r then
              local text = {}
              for x = r.x, r.x + r.width - 1 do text[#text + 1] = slice:get(x, r.y).ch end
              table.insert(_G.__blocks, {
                role = block.role, streaming = block.streaming, text = table.concat(text),
              })
            end
          end
        end)
        "#
    ));
    for _ in 0..5 {
        app.render_silent();
    }

    let blocks: Vec<(String, bool, String)> = app
        .eval_lua::<mlua::Table>("return _G.__blocks")
        .unwrap()
        .sequence_values::<mlua::Table>()
        .map(|block| {
            let block = block.unwrap();
            (
                block.get("role").unwrap(),
                block.get("streaming").unwrap(),
                block.get("text").unwrap(),
            )
        })
        .collect();
    let settled = blocks
        .iter()
        .find(|(_, _, text)| text.contains("settled reply"))
        .unwrap_or_else(|| panic!("the settled block row is on screen: {blocks:?}"));
    assert!(!settled.1, "{blocks:?}");
    let streaming = blocks
        .iter()
        .find(|(_, streaming, _)| *streaming)
        .unwrap_or_else(|| panic!("the streaming block is on screen: {blocks:?}"));
    assert_eq!(streaming.0, "assistant");
    let shown = streaming.2.trim_end();
    assert!(
        !shown.is_empty() && "streaming words".starts_with(shown),
        "the rect starts at the streamed text: {shown:?}"
    );
}

#[test]
fn dissolve_fades_deleted_text_then_goes_idle() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(40, 12);
    let example = r#"
        local fading, fx = {}, nil
        fx = smelt.paint.frame(function(slice, ctx)
          for _, r in ipairs(ctx.damage) do
            for x = r.x, r.x + r.width - 1 do
              local old = ctx.prev and ctx.prev:get(x, r.y)
              if old and old.ch ~= " " then
                fading[#fading + 1] = { x = x, y = r.y, ch = old.ch, t = 4 }
              end
            end
          end
          local kept = {}
          for _, f in ipairs(fading) do
            local cell = slice:get(f.x, f.y)
            if cell and cell.ch == " " then
              slice:set(f.x, f.y, f.ch, { dim = true })
            end
            f.t = f.t - 1
            if f.t > 0 then kept[#kept + 1] = f end
          end
          fading = kept
          if #fading > 0 then fx:invalidate_after(33) end
        end, { name = "test.dissolve", history = true })
    "#;
    assert!(app.run_lua(example), "{example}");
    app.type_text("zq");
    for _ in 0..8 {
        app.render_silent();
    }
    assert!(
        app.tui.scheduled_frame_delay().is_none(),
        "idle once fades end"
    );

    app.press(KeyCode::Backspace);
    let frame = app.render_to_frame();
    let (y, row) = frame
        .rows
        .iter()
        .enumerate()
        .find(|(_, row)| row.contains("zq"))
        .unwrap_or_else(|| panic!("the deleted q fades out:\n{}", frame.text()));
    let x = row.chars().position(|c| c == 'q').unwrap();
    assert!(frame.styles[y][x].dim, "the fading glyph is dim");
    assert!(app.tui.scheduled_frame_delay().is_some(), "fading animates");

    for _ in 0..6 {
        app.render_silent();
    }
    let frame = app.render_to_frame();
    assert!(!frame.text().contains("zq"), "{}", frame.text());
    assert!(
        app.tui.scheduled_frame_delay().is_none(),
        "the paint goes idle once nothing fades"
    );
}

#[test]
fn freshink_example_inks_only_new_text_while_the_view_follows_it() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 14);
    for i in 0..30 {
        app.push_transcript_block(smelt_transcript::transcript_model::Block::Text {
            content: format!("reply number {i}").into(),
        });
    }
    app.start_submitted_turn("question");
    let guide = include_str!("../../../../../docs/docs/extend/paint.md");
    let start = guide.find("-- Fresh ink:").expect("fresh ink example");
    let example = &guide[start..start + guide[start..].find("```").unwrap()];
    assert!(app.run_lua(example), "{example}");
    assert!(app.run_lua(
        r#"_G.__dys = {}
        smelt.paint.frame(function(_, ctx)
          for _, s in ipairs(ctx.scrolls) do table.insert(_G.__dys, s.dy) end
        end, { history = true })"#
    ));
    for _ in 0..3 {
        app.render_silent();
    }

    let ink = Some(crate::smelt_edit::Color::Rgb {
        r: 255,
        g: 236,
        b: 170,
    });
    let words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
    for (i, word) in words.iter().enumerate() {
        // The glyphs inked over the frames that reveal this delta, in order.
        let mut ink_seen = String::new();
        app.dispatch_engine_event(protocol::EngineEvent::TextDelta {
            // One paragraph that wraps over many rows, so the streaming block
            // itself scrolls.
            delta: format!("{word} words keep the one paragraph going on "),
        });
        for _ in 0..12 {
            let frame = app.render_to_frame();
            for (y, row) in frame.rows.iter().enumerate() {
                let inked: String = row
                    .chars()
                    .zip(&frame.styles[y])
                    .map(|(ch, style)| if style.fg == ink { ch } else { ' ' })
                    .collect();
                assert!(
                    !words[..i].iter().any(|old| inked.contains(old))
                        && !inked.contains("reply number"),
                    "text that only scrolled is not new: {inked:?}\n{}",
                    frame.text()
                );
                ink_seen.extend(inked.chars().filter(|ch| !ch.is_whitespace()));
            }
            app.clock.advance(Duration::from_millis(40));
        }
        assert!(
            ink_seen.starts_with(&format!("{word}wordskeep")),
            "each streamed glyph gets the ink as it shows: {ink_seen}"
        );
    }
    let scrolls = app
        .eval_lua::<String>("return table.concat(_G.__dys, ',')")
        .unwrap();
    assert!(
        !scrolls.is_empty(),
        "the view follows the stream and scrolls"
    );
}

#[test]
fn paint_time_is_the_monotonic_clock() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(
        r#"
        smelt.paint.frame(function(slice, ctx)
          _G.__times = { ctx.now_ms, smelt.time.monotonic_ms() }
        end)
        "#
    ));
    app.clock.advance(std::time::Duration::from_millis(250));
    app.render_silent();

    let (paint, monotonic) = app
        .eval_lua::<(u64, u64)>("return _G.__times[1], _G.__times[2]")
        .unwrap();
    assert_eq!(paint, monotonic);
    assert!(paint >= 250, "both follow the host clock: {paint}");
}

#[test]
fn map_pointer_sends_clicks_to_the_cell_the_user_sees() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    overlay_paint(&mut app, "", "nil");
    assert!(app.run_lua(
        r#"
        _G.__presses = {}
        _G.__paint:on("press", function(ev)
          table.insert(_G.__presses, ("%d,%d"):format(ev.x, ev.y))
        end)
        -- Upside down: screen row y shows laid-out row h - 1 - y.
        _G.__flip = smelt.paint.frame(function(slice)
          local snap = slice:snapshot()
          local h = slice:height()
          for y = 0, h - 1 do
            for x = 0, slice:width() - 1 do
              local cell = snap:get(x, h - 1 - y)
              if cell.width > 0 then slice:set(x, y, cell.ch, cell.style) end
            end
          end
        end, { map_pointer = function(x, y, _, h) return x, h - 1 - y end })
        _G.__noop = smelt.paint.frame(function() end, {
          order = 5, map_pointer = function() end,
        })
        "#
    ));
    app.render_silent();

    let left = crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left);
    app.mouse(left, (5, 8));
    assert_eq!(
        app.eval_lua::<String>("return table.concat(_G.__presses, ' ')")
            .unwrap(),
        "1,1",
        "the click on the flipped leaf lands on it"
    );

    assert!(app.run_lua("_G.__flip:remove()"));
    app.mouse(left, (5, 8));
    assert_eq!(
        app.eval_lua::<String>("return table.concat(_G.__presses, ' ')")
            .unwrap(),
        "1,1",
        "without the effect the click misses the leaf"
    );
}

#[test]
fn flip_turns_the_screen_upside_down() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(40, 12);
    app.type_text("flipme");
    app.render_silent();
    let before = app.render_to_frame();

    let example = r#"
        smelt.paint.frame(function(slice, ctx)
          local snap, w, h = slice:snapshot(), slice:width(), slice:height()
          for y = 0, h - 1 do
            for x = 0, w - 1 do
              local cell = snap:get(x, h - 1 - y)
              if cell.width > 0 then slice:set(x, y, cell.ch, cell.style) end
            end
          end
          if ctx.cursor then slice:set_cursor(ctx.cursor.x, h - 1 - ctx.cursor.y) end
        end, {
          name = "test.flip",
          order = 100,
          map_pointer = function(x, y, _, height) return x, height - 1 - y end,
        })
    "#;
    assert!(app.run_lua(example), "{example}");
    let after = app.render_to_frame();

    let mut flipped = before.rows.clone();
    flipped.reverse();
    assert_eq!(after.rows, flipped, "\n{}", after.text());
}

#[test]
fn every_color_argument_rejects_unknown_theme_groups() {
    let mut app = TestApp::builder().build();
    let errors = app
        .eval_lua::<String>(
            r#"
            local buf = smelt.buf.new()
            buf:lines({ "hello" })
            local calls = {
              rgb = function() return smelt.theme.rgb("Coment") end,
              mark = function() return buf:mark(1, 1, 0, { end_col = 2, style = { fg = "Coment" } }) end,
            }
            local out = {}
            for _, name in ipairs({ "rgb", "mark" }) do
              local ok, err = pcall(calls[name])
              out[#out + 1] = name .. "=" .. (ok and "ok" or tostring(err):match("unknown theme group '%w+'") or tostring(err))
            end
            assert(#smelt.theme.rgb("Comment") == 3)
            return table.concat(out, ";")
            "#,
        )
        .unwrap();
    assert_eq!(
        errors,
        "rgb=unknown theme group 'Coment';mark=unknown theme group 'Coment'"
    );
}

fn error_count(app: &TestApp) -> usize {
    let messages = app.tui.lua.core_shared().messages.lock().unwrap();
    messages
        .entries()
        .iter()
        .filter(|entry| entry.kind == smelt_core::messages::MessageKind::Error)
        .count()
}

#[test]
fn failing_paint_is_reported_once_and_removed() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        _G.__calls = 0
        _G.__fx = smelt.paint.frame(function()
          _G.__calls = _G.__calls + 1
          error("boom")
        end, { name = "test.failing" })
        "#
    ));
    let errors = error_count(&app);

    for _ in 0..5 {
        app.render_silent();
    }

    assert_eq!(app.eval_lua::<u32>("return _G.__calls").unwrap(), 1);
    assert_eq!(error_count(&app), errors + 1);
    assert!(!app.eval_lua::<bool>("return _G.__fx:invalidate()").unwrap());
}

#[test]
fn failing_pointer_map_is_reported_once_and_removed() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        _G.__maps = 0
        _G.__fx = smelt.paint.frame(function() end, {
          map_pointer = function()
            _G.__maps = _G.__maps + 1
            error("boom")
          end,
        })
        "#
    ));
    app.render_silent();
    let errors = error_count(&app);

    let left = crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left);
    app.mouse(left, (2, 2));
    app.mouse(left, (3, 3));

    assert_eq!(app.eval_lua::<u32>("return _G.__maps").unwrap(), 1);
    assert_eq!(error_count(&app), errors + 1);
    assert!(!app.eval_lua::<bool>("return _G.__fx:invalidate()").unwrap());
}
