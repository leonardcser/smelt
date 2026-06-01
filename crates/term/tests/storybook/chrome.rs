//! Chrome stories - borders + titles painted on container nodes.
//! Each story paints a dotted backdrop, then layers a bordered subtree
//! at `(col=1, row=1)` so the rendered chrome glyphs read clearly
//! against the surrounding fill.

use smelt_term::layout::{Border, Constraint, EdgeStyle};
use smelt_term::{LayoutTree, PaintId, Rect};

use crate::story;
use crate::storybook::StoryCtx;

const PAINT_BACKDROP: PaintId = PaintId(0);
const PAINT_CONTENT: PaintId = PaintId(1);

/// Build a fill-everything backdrop tree. The leaf's painted text is
/// supplied via `ctx.set_leaf(PAINT_BACKDROP, …)` so the surrounding
/// dots match the original storybook's "dotted backdrop" shape.
fn backdrop_tree() -> LayoutTree {
    LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(PAINT_BACKDROP))])
}

fn dotted_backdrop(ctx: &mut StoryCtx, w: u16, h: u16) {
    ctx.set_viewport(w, h);
    let line = ".".repeat(w as usize);
    let lines: Vec<String> = (0..h).map(|_| line.clone()).collect();
    ctx.set_leaf(PAINT_BACKDROP, lines);
}

fn open_chrome_overlay(
    ctx: &mut StoryCtx,
    width: u16,
    height: u16,
    border: Option<Border>,
    title: Option<&str>,
) {
    let inner = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(PAINT_CONTENT))]);
    let mut tree = LayoutTree::hbox(vec![(Constraint::Length(width), inner)]);
    if let Some(b) = border {
        tree = tree.with_border(b);
    }
    if let Some(t) = title {
        tree = tree.with_title(t.to_string());
    }
    ctx.set_leaf(PAINT_CONTENT, ["content"]);
    ctx.paint_backdrop_then_chrome(&backdrop_tree(), &tree, Rect::new(1, 1, width, height));
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
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, None, Some("ignored"));
    ctx.assert_snapshot();
});

// ── Title truncation ──────────────────────────────────────────────

story!(title_truncates_in_narrow_border, |ctx| {
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

fn sided(top: bool, right: bool, bottom: bool, left: bool) -> Border {
    let on = |b: bool| if b { Some(EdgeStyle::new()) } else { None };
    Border {
        top: on(top),
        right: on(right),
        bottom: on(bottom),
        left: on(left),
        ..Border::single()
    }
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
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(sided(true, false, false, true)), None);
    ctx.assert_snapshot();
});

story!(border_top_and_bottom_no_sides, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(sided(true, false, true, false)), None);
    ctx.assert_snapshot();
});

story!(border_left_and_right_only, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Some(sided(false, true, false, true)), None);
    ctx.assert_snapshot();
});

story!(border_top_with_title_no_corners, |ctx| {
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
