//! Theme stories.

use smelt_core::style::Color;
use smelt_core::theme::intern;
use tui::smelt_term::layout::{Constraint, Gutters};
use tui::smelt_term::{LayoutTree, SplitConfig};

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

story!(theme_link_a_to_b_resolves_b, |ctx| {
    one_line_pane(ctx);
    {
        let theme = ctx.theme_mut();
        let mut s = theme.get("Special");
        s.fg = Some(Color::Cyan);
        theme.set("Special", s);
        theme.link("Aux", "Special");
    }
    let aux = intern("Aux");
    if let Some(buf) = ctx.ui.buf_mut(tui::smelt_term::BufId(1)) {
        buf.add_highlight_group_with_meta(0, 0, 5, aux, smelt_core::buffer::SpanMeta::default());
    }
    ctx.assert_snapshot();
});

story!(theme_link_chain_three_hops, |ctx| {
    one_line_pane(ctx);
    {
        let theme = ctx.theme_mut();
        let mut c = theme.get("HopC");
        c.fg = Some(Color::Magenta);
        c.bold = true;
        theme.set("HopC", c);
        theme.link("HopB", "HopC");
        theme.link("HopA", "HopB");
    }
    let a = intern("HopA");
    if let Some(buf) = ctx.ui.buf_mut(tui::smelt_term::BufId(1)) {
        buf.add_highlight_group_with_meta(0, 6, 11, a, smelt_core::buffer::SpanMeta::default());
    }
    ctx.assert_snapshot();
});

story!(theme_link_cycle_falls_back_to_default, |ctx| {
    one_line_pane(ctx);
    {
        let theme = ctx.theme_mut();
        theme.link("Cyc1", "Cyc2");
        theme.link("Cyc2", "Cyc1");
    }
    let g = intern("Cyc1");
    if let Some(buf) = ctx.ui.buf_mut(tui::smelt_term::BufId(1)) {
        buf.add_highlight_group_with_meta(0, 0, 5, g, smelt_core::buffer::SpanMeta::default());
    }
    ctx.assert_snapshot();
});

story!(theme_unknown_group_returns_default, |ctx| {
    one_line_pane(ctx);
    let unknown = intern("DefinitelyNotRegistered");
    if let Some(buf) = ctx.ui.buf_mut(tui::smelt_term::BufId(1)) {
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
