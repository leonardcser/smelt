//! Buffer-content stories.

use smelt_core::buffer::{
    ExtmarkOpts, ExtmarkPayload, HlMode, LineDecoration, SpanMeta, VirtTextPos,
};
use smelt_core::content::builder::render_into;
use smelt_core::content::highlight::render_markdown_table;
use smelt_core::style::{Color, Style};
use smelt_core::theme::{intern, Theme};
use tui::smelt_edit::layout::{Constraint, Gutters};
use tui::smelt_edit::{BufId, LayoutTree, SplitConfig};

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

story!(highlight_range_paints_in_styles, |ctx| {
    let buf_id = open_with_lines(ctx, &["foo bar baz"], 16, 3);
    let style = Style {
        fg: Some(Color::Yellow),
        bold: true,
        ..Style::default()
    };
    if let Some(buf) = ctx.ui.buf_mut(buf_id) {
        buf.add_highlight(0, 4, 7, style);
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
                on_cursor_row: false,
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

story!(table_selection_masks_chrome_and_padding, |ctx| {
    ctx.set_viewport(18, 5);
    let buf_id = ctx.buf();
    if let Some(buf) = ctx.ui.buf_mut(buf_id) {
        let rows = vec![
            vec!["A".to_string(), "B".to_string()],
            vec!["1".to_string(), "2".to_string()],
        ];
        let theme = Theme::default();
        render_into(buf, 18, &theme, |out| {
            render_markdown_table(out, &rows, &[], 18, false, None, "");
        });
        let selection = smelt_buffer::coords::byte_range_to_row_ranges(buf, 0, buf.text().len());
        buf.set_selection(selection);
    }
    let win = ctx.open_split(buf_id, pane("only"));
    ctx.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(win),
    )]));
    ctx.assert_snapshot();
});

story!(decoration_fill_bg_paints_content_region, |ctx| {
    // Width 12 with default Gutters reserves the rightmost column for the
    // scrollbar, leaving 11 cells of content. `fill_bg` covers exactly the
    // content region - the scrollbar column stays unstyled - matching
    // `pad_row_to_layout_width`'s span so blank padding rows align with
    // content rows in user-message blocks.
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

story!(cjk_double_width_glyphs_render, |ctx| {
    open_with_lines(ctx, &["你好世界"], 16, 3);
    ctx.assert_snapshot();
});

story!(emoji_double_width_glyphs_render, |ctx| {
    open_with_lines(ctx, &["smelt: ✨ 🚀 🔥 done"], 24, 3);
    ctx.assert_snapshot();
});

story!(mixed_ascii_and_cjk, |ctx| {
    open_with_lines(ctx, &["hello 你好 world"], 20, 3);
    ctx.assert_snapshot();
});

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
