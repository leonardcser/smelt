//! Theme stories.

use smelt_core::style::Color;
use smelt_core::theme::intern;
use tui::smelt_edit::layout::{Constraint, Gutters};
use tui::smelt_edit::{LayoutTree, SplitConfig};

fn pane(region: &str) -> SplitConfig {
    SplitConfig {
        region: region.to_string(),
        gutters: Gutters::default(),
    }
}

fn one_line_pane(ctx: &mut crate::storybook::StoryCtx) {
    ctx.set_viewport(20, 3);
    let buf = ctx.buf_with_lines(["hello world"]);
    let w = ctx.open_split(buf, pane("only"));
    ctx.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(w),
    )]));
}

story!(default_theme_normal_fg, |ctx| {
    one_line_pane(ctx);
    ctx.assert_snapshot();
});

story!(theme_swap_repaints_without_buffer_edit, |ctx| {
    one_line_pane(ctx);

    ctx.assert_snapshot();

    {
        let theme = ctx.theme_mut();
        let mut s = theme.get("Normal");
        s.fg = Some(Color::Red);
        theme.set("Normal", s);
    }

    ctx.assert_snapshot();
});

story!(theme_unknown_group_returns_default, |ctx| {
    one_line_pane(ctx);
    let unknown = intern("DefinitelyNotRegistered");
    if let Some(buf) = ctx.ui.buf_mut(tui::smelt_edit::BufId(1)) {
        buf.add_highlight_group_with_meta(
            0,
            0,
            5,
            unknown,
            smelt_core::buffer::SpanMeta::default(),
        );
    }
    ctx.assert_snapshot();
});
