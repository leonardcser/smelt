//! Chrome stories — borders + titles painted on overlay containers.
//! `paint_chrome` is invoked by the overlay-paint pass, not splits;
//! these stories pin the rendered chrome glyphs for each border
//! variant + title placement / truncation.

use tui::smelt_term::layout::{Anchor, Border, BorderSides, BorderStyle, Constraint, Corner, Gutters};
use tui::smelt_term::{LayoutTree, Overlay, SplitConfig};

use crate::storybook::StoryCtx;

fn pane(region: &str) -> SplitConfig {
    SplitConfig {
        region: region.to_string(),
        gutters: Gutters::default(),
    }
}

/// Quiet backdrop so chrome glyphs read clearly. Single-pane fill,
/// content "."-padded so the overlay layers cleanly on top.
fn dotted_backdrop(ctx: &mut StoryCtx, w: u16, h: u16) {
    ctx.set_viewport(w, h);
    let lines: Vec<String> = (0..h).map(|_| ".".repeat(w as usize)).collect();
    let buf = ctx.buf_with_lines(lines);
    let win = ctx.open_split(buf, pane("backdrop"));
    ctx.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(win),
    )]));
}

fn open_chrome_overlay(
    ctx: &mut StoryCtx,
    width: u16,
    height: u16,
    border: Option<Border>,
    title: Option<&str>,
) {
    let buf = ctx.buf_with_lines(["content"]);
    let dw = ctx
        .ui
        .win_open_split(buf, pane("inner"))
        .expect("buf exists");
    // Wrap the leaf in a vbox so it has an explicit primary-axis size
    // regardless of which sides the border paints — that way every
    // snapshot shows the leaf-row(s) plus whichever sides are on,
    // letting the reader visually verify which edges paint.
    let inner = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(dw))]);
    let mut tree = LayoutTree::hbox(vec![(Constraint::Length(width), inner)]);
    if let Some(b) = border {
        tree = tree.with_border(b);
    }
    if let Some(t) = title {
        tree = tree.with_title(t);
    }
    let mut overlay = Overlay::new(
        tree,
        Anchor::ScreenAt {
            row: 1,
            col: 1,
            corner: Corner::NW,
        },
    );
    overlay = overlay.with_size((width, height));
    ctx.ui.overlay_open(overlay);
}

// ── Border styles ─────────────────────────────────────────────────

story!(border_single_no_title, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(Border::SINGLE), None);
    ctx.assert_snapshot();
});

story!(border_single_with_title, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(Border::SINGLE), Some("title"));
    ctx.assert_snapshot();
});

story!(border_double_with_title, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(Border::DOUBLE), Some("dbl"));
    ctx.assert_snapshot();
});

story!(border_rounded_with_title, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(Border::ROUNDED), Some("rnd"));
    ctx.assert_snapshot();
});

story!(border_none_omits_frame, |ctx| {
    // No border at all — content paints at the full anchor rect, no
    // border row, no inset. Title is meaningless without a top border.
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, None, Some("ignored"));
    ctx.assert_snapshot();
});

// ── Title truncation ──────────────────────────────────────────────

story!(title_truncates_in_narrow_border, |ctx| {
    // Width 10 with rounded border leaves ~6 cells for the title.
    // The literal "this is a long title" must clip cleanly inside
    // the top border row.
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(
        ctx,
        10,
        3,
        Some(Border::SINGLE),
        Some("this is a long title"),
    );
    ctx.assert_snapshot();
});

// ── Per-side border combinations ──────────────────────────────────
//
// A `Border` carries `style` + `sides`; missing sides paint nothing
// at that edge AND don't reserve a row/column there. These pin each
// single-edge case plus a couple of multi-edge combos that compose
// only some of the corners.

fn sided(top: bool, right: bool, bottom: bool, left: bool) -> Border {
    Border::new(
        BorderStyle::Single,
        BorderSides {
            top,
            right,
            bottom,
            left,
        },
    )
}

story!(border_top_only_no_corners, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(
        ctx,
        12,
        3,
        Some(sided(true, false, false, false)),
        Some("top"),
    );
    ctx.assert_snapshot();
});

story!(border_bottom_only_no_corners, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(sided(false, false, true, false)), None);
    ctx.assert_snapshot();
});

story!(border_left_only_no_corners, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(sided(false, false, false, true)), None);
    ctx.assert_snapshot();
});

story!(border_right_only_no_corners, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(sided(false, true, false, false)), None);
    ctx.assert_snapshot();
});

story!(border_top_left_corner_only, |ctx| {
    // Top + left → a `┌` corner with the top edge running right and
    // the left edge running down. No bottom/right side, no other corners.
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(sided(true, false, false, true)), None);
    ctx.assert_snapshot();
});

story!(border_top_and_bottom_no_sides, |ctx| {
    // Horizontal "rule" both above and below — no corners, no
    // verticals.
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(sided(true, false, true, false)), None);
    ctx.assert_snapshot();
});

story!(border_left_and_right_only, |ctx| {
    // Two vertical "rules" with no top/bottom edge.
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(sided(false, true, false, true)), None);
    ctx.assert_snapshot();
});

story!(border_top_with_title_no_corners, |ctx| {
    // Title on a top-only border — no leading corner reserves the
    // start cell; title runs from the leftmost column.
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(
        ctx,
        12,
        3,
        Some(sided(true, false, false, false)),
        Some("messages"),
    );
    ctx.assert_snapshot();
});

// Note: the chrome painter's "width < 2 OR height < 2" early-return
// isn't reachable from the public Anchor API — natural-size
// resolution inflates overlay rects to fit the layout + chrome. If
// that path becomes reachable, add guard stories here.
