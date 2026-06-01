//! Layout-tree paint stories. Pure structural shapes - these hunt
//! solver bugs, chrome painting, and gap math.

use smelt_term::layout::{Border, Constraint};
use smelt_term::{LayoutTree, PaintId, Rect};

use crate::story;
use crate::storybook::StoryCtx;

const TOP: PaintId = PaintId(1);
const MID: PaintId = PaintId(2);
const BOT: PaintId = PaintId(3);

fn paint_full(ctx: &mut StoryCtx, tree: &LayoutTree) {
    let area = Rect::new(0, 0, ctx.width, ctx.height);
    ctx.paint_tree(tree, area);
}

story!(vbox_three_panes, |ctx| {
    ctx.set_viewport(40, 8);
    ctx.set_leaf(TOP, ["top pane"]);
    ctx.set_leaf(MID, ["middle pane"]);
    ctx.set_leaf(BOT, ["bottom pane"]);
    let tree = LayoutTree::vbox(vec![
        (Constraint::Length(2), LayoutTree::leaf(TOP)),
        (Constraint::Fill, LayoutTree::leaf(MID)),
        (Constraint::Length(2), LayoutTree::leaf(BOT)),
    ]);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

story!(splits_paint_border_and_title, |ctx| {
    ctx.set_viewport(30, 6);
    ctx.set_leaf(PaintId(1), ["inside the bordered pane"]);
    let tree = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(PaintId(1)))])
        .with_border(Border::ROUNDED)
        .with_title("frame");
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

story!(hbox_two_columns_with_gap, |ctx| {
    ctx.set_viewport(40, 4);
    ctx.set_leaf(PaintId(1), ["left col"]);
    ctx.set_leaf(PaintId(2), ["right col"]);
    let tree = LayoutTree::hbox(vec![
        (Constraint::Fill, LayoutTree::leaf(PaintId(1))),
        (Constraint::Fill, LayoutTree::leaf(PaintId(2))),
    ])
    .with_gap(2);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

story!(vbox_nested_in_hbox, |ctx| {
    ctx.set_viewport(40, 6);
    ctx.set_leaf(PaintId(1), ["sidebar"]);
    ctx.set_leaf(PaintId(2), ["main top"]);
    ctx.set_leaf(PaintId(3), ["main bottom"]);
    let main = LayoutTree::vbox(vec![
        (Constraint::Fill, LayoutTree::leaf(PaintId(2))),
        (Constraint::Length(2), LayoutTree::leaf(PaintId(3))),
    ]);
    let tree = LayoutTree::hbox(vec![
        (Constraint::Length(10), LayoutTree::leaf(PaintId(1))),
        (Constraint::Fill, main),
    ]);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

// ── Constraint coverage ───────────────────────────────────────────

story!(vbox_percentage_split, |ctx| {
    ctx.set_viewport(20, 10);
    ctx.set_leaf(PaintId(1), ["30 percent"]);
    ctx.set_leaf(PaintId(2), ["70 percent"]);
    let tree = LayoutTree::vbox(vec![
        (Constraint::Percentage(30), LayoutTree::leaf(PaintId(1))),
        (Constraint::Percentage(70), LayoutTree::leaf(PaintId(2))),
    ]);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

story!(hbox_ratio_split_one_to_two, |ctx| {
    ctx.set_viewport(30, 3);
    ctx.set_leaf(PaintId(1), ["L"]);
    ctx.set_leaf(PaintId(2), ["R"]);
    let tree = LayoutTree::hbox(vec![
        (Constraint::Ratio(1, 3), LayoutTree::leaf(PaintId(1))),
        (Constraint::Ratio(2, 3), LayoutTree::leaf(PaintId(2))),
    ]);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

story!(vbox_min_competes_with_fill, |ctx| {
    ctx.set_viewport(20, 12);
    ctx.set_leaf(PaintId(1), ["min child"]);
    ctx.set_leaf(PaintId(2), ["fill child"]);
    let tree = LayoutTree::vbox(vec![
        (Constraint::Min(3), LayoutTree::leaf(PaintId(1))),
        (Constraint::Fill, LayoutTree::leaf(PaintId(2))),
    ]);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

story!(vbox_max_clamps_pane_height, |ctx| {
    ctx.set_viewport(20, 8);
    ctx.set_leaf(PaintId(1), ["capped at 2 rows tall - extra lines clip"]);
    ctx.set_leaf(PaintId(2), ["rest"]);
    let tree = LayoutTree::vbox(vec![
        (Constraint::Max(2), LayoutTree::leaf(PaintId(1))),
        (Constraint::Fill, LayoutTree::leaf(PaintId(2))),
    ]);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

story!(hbox_three_fill_split_evenly, |ctx| {
    ctx.set_viewport(30, 3);
    ctx.set_leaf(PaintId(1), ["AAA"]);
    ctx.set_leaf(PaintId(2), ["BBB"]);
    ctx.set_leaf(PaintId(3), ["CCC"]);
    let tree = LayoutTree::hbox(vec![
        (Constraint::Fill, LayoutTree::leaf(PaintId(1))),
        (Constraint::Fill, LayoutTree::leaf(PaintId(2))),
        (Constraint::Fill, LayoutTree::leaf(PaintId(3))),
    ]);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

// ── Chrome + gap math ─────────────────────────────────────────────

story!(hbox_gap_consumes_pixels, |ctx| {
    ctx.set_viewport(20, 3);
    ctx.set_leaf(PaintId(1), ["left"]);
    ctx.set_leaf(PaintId(2), ["right"]);
    let tree = LayoutTree::hbox(vec![
        (Constraint::Fill, LayoutTree::leaf(PaintId(1))),
        (Constraint::Fill, LayoutTree::leaf(PaintId(2))),
    ])
    .with_gap(4);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

story!(nested_borders_inset_correctly, |ctx| {
    ctx.set_viewport(20, 8);
    ctx.set_leaf(PaintId(1), ["nested"]);
    let inner = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(PaintId(1)))])
        .with_border(Border::ROUNDED)
        .with_title("inner");
    let tree = LayoutTree::vbox(vec![(Constraint::Fill, inner)])
        .with_border(Border::SINGLE)
        .with_title("outer");
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

// ── Edge cases ────────────────────────────────────────────────────

story!(single_cell_viewport_renders_safely, |ctx| {
    ctx.set_viewport(1, 1);
    ctx.set_leaf(PaintId(1), ["x"]);
    let tree = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(PaintId(1)))]);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

story!(vbox_overflow_clamps_content, |ctx| {
    ctx.set_viewport(20, 6);
    ctx.set_leaf(PaintId(1), ["aaa one", "aaa two", "aaa three"]);
    ctx.set_leaf(PaintId(2), ["bbb one", "bbb two", "bbb three"]);
    ctx.set_leaf(PaintId(3), ["ccc one", "ccc two", "ccc three"]);
    let tree = LayoutTree::vbox(vec![
        (Constraint::Length(3), LayoutTree::leaf(PaintId(1))),
        (Constraint::Length(3), LayoutTree::leaf(PaintId(2))),
        (Constraint::Length(3), LayoutTree::leaf(PaintId(3))),
    ]);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});

story!(vbox_mixed_length_fill_and_min, |ctx| {
    ctx.set_viewport(15, 10);
    ctx.set_leaf(PaintId(1), ["header line"]);
    ctx.set_leaf(PaintId(2), ["body min line"]);
    ctx.set_leaf(PaintId(3), ["foot fill"]);
    let tree = LayoutTree::vbox(vec![
        (Constraint::Length(2), LayoutTree::leaf(PaintId(1))),
        (Constraint::Min(2), LayoutTree::leaf(PaintId(2))),
        (Constraint::Fill, LayoutTree::leaf(PaintId(3))),
    ]);
    paint_full(ctx, &tree);
    ctx.assert_snapshot();
});
