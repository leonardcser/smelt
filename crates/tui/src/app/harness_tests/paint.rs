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
    app.app.event_needs_frame(&Event::Mouse(moved(x, y)))
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
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: "melting text".into(),
    });
    assert!(app.run_lua(
        r#"
        _G.__fx = smelt.paint.frame(function(slice)
            slice:map(function(cell)
                if cell.ch == "m" then
                    return { ch = "M", style = { fg = { rgb = { 1, 2, 3 } } } }
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
fn theme_rgb_resolves_indexed_and_named_colors() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r##"
        local function rgb(c) local t = smelt.theme.rgb(c).rgb; return t[1] .. "," .. t[2] .. "," .. t[3] end
        assert(rgb({ ansi = 196 }) == "255,0,0", rgb({ ansi = 196 }))
        assert(rgb({ ansi = 244 }) == "128,128,128", rgb({ ansi = 244 }))
        assert(rgb("#102030") == "16,32,48")
        assert(rgb("red") == "255,0,0")
        assert(smelt.theme.rgb("reset") == nil)
        assert(smelt.theme.rgb(nil) == nil)
        "##
    ));
}

#[test]
fn frame_paint_placed_in_a_layout_runs_once_per_frame() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(
        r#"
        _G.__calls = 0
        local fx = smelt.paint.frame(function() _G.__calls = _G.__calls + 1 end)
        smelt.overlay.new({
            anchor = "screen_at", corner = "nw", row = 0, col = 0,
            width = 4, height = 1, border = "none", modal = false,
            layout = smelt.ui.layout.leaf(fx),
        })
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
            slice:put_str(0, 0, "abc", { bg = { rgb = { 9, 9, 9 } } })
        end)
        _G.__over = smelt.paint.register(function(slice)
            _G.__seen = slice:get(1, 0).ch
            slice:set_style(0, 0, 3, 1, { bold = true })
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
fn fps_paint_schedules_frames_until_removed() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    app.render_silent();
    assert!(app.app.scheduled_frame_delay().is_none());

    assert!(app.run_lua(
        r#"_G.__anim = smelt.paint.frame(function() end, { name = "test.anim", fps = 50 })"#
    ));
    app.render_silent();
    let delay = app
        .app
        .scheduled_frame_delay()
        .expect("an fps paint keeps frames coming");
    assert!(delay <= Duration::from_millis(20), "{delay:?}");

    assert!(app.run_lua("_G.__anim:set_fps(nil)"));
    app.render_silent();
    assert!(app.app.scheduled_frame_delay().is_none());

    assert!(app.run_lua("_G.__anim:set_fps(10); _G.__anim:remove()"));
    app.render_silent();
    assert!(
        app.app.scheduled_frame_delay().is_none(),
        "a removed paint stops animating"
    );
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

    assert!(app.run_lua(r#"_G.__reg = smelt.win.transcript():on("enter", function() end)"#));
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
