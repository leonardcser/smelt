//! Buffer-content stories — extmark painting, line decoration,
//! virtual text, soft-wrap, unicode width handling. Drives `Buffer`
//! mutators directly through `ctx.ui.buf_mut(...)`; nothing here
//! cares about Lua or the engine.

use smelt_core::buffer::{
    ExtmarkOpts, ExtmarkPayload, HlMode, LineDecoration, SpanMeta, VirtTextPos,
};
use smelt_core::style::{Color, Style};
use smelt_core::theme::intern;
use tui::smelt_term::layout::{Constraint, Gutters};
use tui::smelt_term::{BufId, LayoutTree, SplitConfig};

use crate::storybook::StoryCtx;

fn pane(region: &str) -> SplitConfig {
    SplitConfig {
        region: region.to_string(),
        gutters: Gutters::default(),
    }
}

fn open_with_lines(ctx: &mut StoryCtx, lines: &[&str], w: u16, h: u16) -> BufId {
    ctx.set_viewport(w, h);
    let buf = ctx.buf_with_lines(lines.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let win = ctx.open_split(buf, pane("only"));
    ctx.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(win),
    )]));
    buf
}

// ── Highlight extmarks ────────────────────────────────────────────

story!(highlight_range_paints_in_styles, |ctx| {
    let buf_id = open_with_lines(ctx, &["foo bar baz"], 16, 3);
    let style = Style {
        fg: Some(Color::Yellow),
        bold: true,
        ..Style::default()
    };
    if let Some(buf) = ctx.ui.buf_mut(buf_id) {
        buf.add_highlight(0, 4, 7, style); // highlight "bar"
    }
    ctx.assert_snapshot();
});

story!(multiple_highlights_same_line_layered, |ctx| {
    let buf_id = open_with_lines(ctx, &["one two three"], 18, 3);
    let s1 = Style {
        fg: Some(Color::Red),
        ..Style::default()
    };
    let s2 = Style {
        fg: Some(Color::Green),
        italic: true,
        ..Style::default()
    };
    if let Some(buf) = ctx.ui.buf_mut(buf_id) {
        buf.add_highlight(0, 0, 3, s1); // "one"
        buf.add_highlight(0, 4, 7, s2); // "two"
    }
    ctx.assert_snapshot();
});

story!(highlight_hl_eol_paints_to_line_end, |ctx| {
    // hl_eol = true extends a span past `end_col` to the right edge
    // of the visible row. Useful for diff +/- lines.
    let buf_id = open_with_lines(ctx, &["abc"], 10, 3);
    let style = Style {
        bg: Some(Color::DarkGreen),
        ..Style::default()
    };
    let group = intern("DiffAdd");
    {
        let theme = ctx.theme_mut();
        theme.set("DiffAdd", style);
    }
    if let Some(buf) = ctx.ui.buf_mut(buf_id) {
        let opts = ExtmarkOpts {
            end_row: None,
            end_col: Some(3),
            payload: ExtmarkPayload::Highlight {
                hl: group,
                meta: SpanMeta::default(),
                hl_eol: true,
                hl_mode: HlMode::default(),
                conceal: None,
            },
            priority: 0,
            right_gravity: true,
            end_right_gravity: false,
            id: None,
        };
        let ns = buf.create_namespace("test:diff_add");
        buf.set_extmark(ns, 0, 0, opts);
    }
    ctx.assert_snapshot();
});

// ── Line decoration ───────────────────────────────────────────────

story!(decoration_fill_bg_paints_full_row, |ctx| {
    let buf_id = open_with_lines(ctx, &["hello"], 12, 3);
    if let Some(buf) = ctx.ui.buf_mut(buf_id) {
        let dec = LineDecoration {
            fill_bg: Some(Color::DarkBlue),
            ..LineDecoration::default()
        };
        buf.set_decoration(0, dec);
    }
    ctx.assert_snapshot();
});

// ── Virtual text ──────────────────────────────────────────────────

story!(virt_text_eol_appends_after_content, |ctx| {
    let buf_id = open_with_lines(ctx, &["code"], 20, 3);
    if let Some(buf) = ctx.ui.buf_mut(buf_id) {
        let ns = buf.create_namespace("test:virt");
        buf.set_extmark(
            ns,
            0,
            0,
            ExtmarkOpts::virt_text(" // comment".into(), Some("Comment".into()))
                .with_virt_pos(VirtTextPos::Eol),
        );
    }
    ctx.assert_snapshot();
});

// ── Unicode width ─────────────────────────────────────────────────

story!(cjk_double_width_glyphs_render, |ctx| {
    // Each CJK char takes 2 cells; 5 chars → 10 visual columns.
    open_with_lines(ctx, &["你好世界"], 16, 3);
    ctx.assert_snapshot();
});

story!(emoji_double_width_glyphs_render, |ctx| {
    // Common emoji cluster + ASCII trailer.
    open_with_lines(ctx, &["smelt: ✨ 🚀 🔥 done"], 24, 3);
    ctx.assert_snapshot();
});

story!(mixed_ascii_and_cjk, |ctx| {
    open_with_lines(ctx, &["hello 你好 world"], 20, 3);
    ctx.assert_snapshot();
});

// ── Edge cases ────────────────────────────────────────────────────

story!(empty_buffer_fills_with_blanks, |ctx| {
    ctx.set_viewport(8, 3);
    let buf = ctx.buf();
    let win = ctx.open_split(buf, pane("only"));
    ctx.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(win),
    )]));
    ctx.assert_snapshot();
});

story!(line_longer_than_viewport_truncates, |ctx| {
    open_with_lines(
        ctx,
        &["this line is much longer than the viewport allows so it should truncate at the right edge"],
        20,
        3,
    );
    ctx.assert_snapshot();
});

story!(many_lines_more_than_viewport_height, |ctx| {
    let lines: Vec<&str> = ["row 01", "row 02", "row 03", "row 04", "row 05", "row 06"].into();
    open_with_lines(ctx, &lines, 12, 3);
    ctx.assert_snapshot();
});
