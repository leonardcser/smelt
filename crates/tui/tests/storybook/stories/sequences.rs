//! Multi-step bug-cluster stories.
//!
//! These exercise sequences the audit flagged as recurring sources of
//! regressions: resize transitions, scroll/wrap reflow on narrow widths,
//! and yank/paste/undo/redo round-trips. Each `assert_snapshot()` call
//! emits a `.step-N` file so the viewer shows the full sequence.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui::smelt_term::layout::{Constraint, Gutters};
use tui::smelt_term::{Event, LayoutTree, SplitConfig};

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

fn press_chars(ctx: &mut StoryCtx, s: &str) {
    for c in s.chars() {
        ctx.press_vim(key(c));
    }
}

// ── Resize transitions ────────────────────────────────────────────

story!(resize_wide_to_narrow_keeps_content, |ctx| {
    open_buffer(
        ctx,
        &[
            "alpha bravo charlie",
            "delta echo foxtrot",
            "golf hotel india",
        ],
        30,
        5,
    );
    ctx.assert_snapshot();
    ctx.set_viewport(14, 5);
    ctx.assert_snapshot();
    ctx.set_viewport(40, 5);
    ctx.assert_snapshot();
});

story!(resize_shrinks_height_below_content, |ctx| {
    open_buffer(ctx, &["row 1", "row 2", "row 3", "row 4", "row 5"], 16, 6);
    ctx.assert_snapshot();
    ctx.set_viewport(16, 3);
    ctx.assert_snapshot();
});

// ── Yank / paste / undo round-trip ────────────────────────────────

story!(yank_line_paste_then_undo_redo, |ctx| {
    open_buffer(ctx, &["alpha", "beta", "gamma"], 16, 5);
    ctx.press_vim(special(KeyCode::Esc));
    // yank current line then paste below.
    press_chars(ctx, "yyp");
    ctx.assert_snapshot();
    // undo restores the original three-line buffer.
    ctx.press_vim(key('u'));
    ctx.assert_snapshot();
    // redo re-applies the paste.
    ctx.press_vim(Event::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )));
    ctx.assert_snapshot();
});

story!(visual_select_delete_then_undo, |ctx| {
    open_buffer(ctx, &["abcdef ghij"], 16, 3);
    ctx.press_vim(special(KeyCode::Esc));
    press_chars(ctx, "vllld");
    ctx.assert_snapshot();
    ctx.press_vim(key('u'));
    ctx.assert_snapshot();
});

// ── Visual select survives resize ─────────────────────────────────

story!(visual_selection_survives_width_change, |ctx| {
    open_buffer(ctx, &["alpha bravo charlie delta"], 30, 3);
    ctx.press_vim(special(KeyCode::Esc));
    press_chars(ctx, "vlllllllll");
    ctx.assert_snapshot();
    ctx.set_viewport(16, 3);
    ctx.assert_snapshot();
});
