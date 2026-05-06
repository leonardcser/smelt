//! Vim stories — selection paint, motions, operators.
//! L3-prim shape: drives `Window::handle` through
//! `StoryCtx::press_vim`, which mounts buffer rows, runs the vim
//! state machine, and syncs the edited text back to the Buffer plus
//! repaints the Visual selection range as a highlight extmark.
//! No `LuaRuntime`, no engine.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui::ui::layout::{Constraint, Gutters};
use tui::ui::{Event, LayoutTree, SplitConfig};

use crate::storybook::StoryCtx;

fn pane(region: &str) -> SplitConfig {
    SplitConfig {
        region: region.to_string(),
        gutters: Gutters::default(),
    }
}

fn key(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

fn special(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn open_buffer(ctx: &mut StoryCtx, lines: &[&str], w: u16, h: u16) {
    ctx.set_viewport(w, h);
    let buf = ctx.buf_with_lines(lines.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let win = ctx.open_split(buf, pane("only"));
    ctx.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(win),
    )]));
    ctx.ui.set_focus(win);
}

fn press(ctx: &mut StoryCtx, ev: Event) {
    ctx.press_vim(ev);
}

fn press_chars(ctx: &mut StoryCtx, s: &str) {
    for c in s.chars() {
        press(ctx, key(c));
    }
}

// ── Selection / Visual ────────────────────────────────────────────

story!(visual_line_paints_selection_bg, |ctx| {
    open_buffer(ctx, &["alpha", "beta", "gamma"], 20, 4);
    press_chars(ctx, "Vj");
    ctx.assert_snapshot();
});

story!(visual_char_extends_with_l, |ctx| {
    // `v` enters char-visual; `lll` extends three columns to the right.
    open_buffer(ctx, &["abcdef ghij"], 20, 3);
    press_chars(ctx, "vlll");
    ctx.assert_snapshot();
});

story!(visual_line_o_swaps_anchor, |ctx| {
    // V on row 1, jj down to row 3, then `o` flips the anchor to
    // row 1 — selection identical, but cursor jumps back up.
    open_buffer(ctx, &["one", "two", "three", "four"], 20, 5);
    press_chars(ctx, "VjjO");
    ctx.assert_snapshot();
});

// ── Motions ───────────────────────────────────────────────────────

story!(normal_dollar_jumps_to_eol, |ctx| {
    // Block cursor lands on the last char of the line after `$`.
    open_buffer(ctx, &["short"], 10, 3);
    press(ctx, special(KeyCode::Esc));
    press(ctx, key('$'));
    ctx.assert_snapshot();
});

story!(normal_caret_jumps_to_first_nonblank, |ctx| {
    // Leading whitespace; `^` goes to first non-blank.
    open_buffer(ctx, &["   indented"], 16, 3);
    press(ctx, special(KeyCode::Esc));
    press(ctx, key('^'));
    ctx.assert_snapshot();
});

story!(normal_w_word_motion, |ctx| {
    // `w` advances by word; cursor sits on next word's first char.
    open_buffer(ctx, &["foo bar baz"], 16, 3);
    press(ctx, special(KeyCode::Esc));
    press_chars(ctx, "wl"); // step into next word + one more
    ctx.assert_snapshot();
});

story!(normal_gg_jumps_to_first_line, |ctx| {
    open_buffer(ctx, &["one", "two", "three", "four"], 16, 6);
    press(ctx, special(KeyCode::Esc));
    press_chars(ctx, "Ggg");
    ctx.assert_snapshot();
});

story!(normal_count_prefix_3w, |ctx| {
    // `3w` skips three words.
    open_buffer(ctx, &["a b c d e f g h"], 20, 3);
    press(ctx, special(KeyCode::Esc));
    press_chars(ctx, "3w");
    ctx.assert_snapshot();
});

// ── Operators ─────────────────────────────────────────────────────

story!(operator_dd_removes_line, |ctx| {
    // `dd` deletes the current line; remaining lines shift up.
    open_buffer(ctx, &["alpha", "beta", "gamma"], 20, 4);
    press(ctx, special(KeyCode::Esc));
    press_chars(ctx, "jdd"); // delete row 2 ("beta")
    ctx.assert_snapshot();
});

story!(operator_dw_removes_word, |ctx| {
    open_buffer(ctx, &["foo bar baz"], 20, 3);
    press(ctx, special(KeyCode::Esc));
    press_chars(ctx, "dw"); // delete "foo "
    ctx.assert_snapshot();
});

story!(operator_yy_then_p_pastes_below, |ctx| {
    // `yy` yanks current line; `p` pastes below.
    open_buffer(ctx, &["alpha", "beta"], 20, 4);
    press(ctx, special(KeyCode::Esc));
    press_chars(ctx, "yyp");
    ctx.assert_snapshot();
});

story!(operator_2x_deletes_two_chars, |ctx| {
    // Count prefix on `x`: `2x` deletes the two chars under and to the
    // right of the cursor. Substitute for the `.` dot-repeat coverage
    // that this suite would normally pin — dot-repeat isn't yet
    // implemented in `ui::vim` (see refactor/P9.n's note about moving
    // per-buffer state including the dot register onto Buffer).
    open_buffer(ctx, &["abcdef"], 12, 3);
    press(ctx, special(KeyCode::Esc));
    press_chars(ctx, "2x");
    ctx.assert_snapshot();
});

// ── Edge cases ────────────────────────────────────────────────────

story!(empty_buffer_normal_mode_no_panic, |ctx| {
    // Smoke: focused empty buffer with no lines tolerates motions.
    ctx.set_viewport(10, 3);
    let buf = ctx.buf();
    let win = ctx.open_split(buf, pane("only"));
    ctx.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(win),
    )]));
    ctx.ui.set_focus(win);
    press(ctx, special(KeyCode::Esc));
    press_chars(ctx, "0$jkw"); // motions over an empty document
    ctx.assert_snapshot();
});
