//! Theme stories — verify the HlGroup-id contract: changing a theme
//! style should re-render existing buffers without rewriting their
//! content. The same buffer paints with different colors when the
//! theme's `Normal.fg` flips.

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

/// Plain "hello world" buffer in a Fill leaf, viewport 20×3. The
/// content stays constant so successive theme tweaks land in the
/// styles sidecar only.
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

    // Frame 0: stock theme.
    ctx.assert_snapshot();

    // Mutate Normal.fg in-place; the HlGroup-id contract means the
    // buffer's extmarks (none yet — plain text) and any group-id
    // references resolve through the new style at paint time.
    {
        let theme = ctx.theme_mut();
        let mut s = theme.get("Normal");
        s.fg = Some(Color::Red);
        theme.set("Normal", s);
    }

    // Frame 1: same buffer, new fg.
    ctx.assert_snapshot();
});

story!(theme_link_a_to_b_resolves_b, |ctx| {
    // `Aux` doesn't exist in the registry; link it to `Special` and
    // paint a span with `Aux` — the resolved style should match
    // `Special`.
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
    // A → B → C; A should paint as C.
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
    // Cyc1 → Cyc2 → Cyc1 cycle — resolver caps at 16 hops and
    // returns Style::default(). The painted span should look unset.
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
    // Span with a never-set group resolves to Style::default() —
    // identical to no extmark at all. The styles sidecar should be
    // empty for these cells.
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
