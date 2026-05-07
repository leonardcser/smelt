//! Overlay stories — placement + chrome paint over splits, plus
//! every Anchor variant.

use tui::ui::layout::{Anchor, Border, Constraint, Corner, Gutters};
use tui::ui::{LayoutTree, Overlay, SplitConfig};

fn pane(region: &str) -> SplitConfig {
    SplitConfig {
        region: region.to_string(),
        gutters: Gutters::default(),
    }
}

/// Build a backdrop transcript pane filling the viewport so overlay
/// stories share a consistent base scene.
fn backdrop(ctx: &mut crate::storybook::StoryCtx, w: u16, h: u16) {
    ctx.set_viewport(w, h);
    let lines: Vec<String> = (1..=h)
        .map(|i| {
            let mut row = String::new();
            for col in 0..w {
                row.push(if col % 4 == 0 {
                    char::from_digit((i % 10) as u32, 10).unwrap_or('.')
                } else {
                    '.'
                });
            }
            row
        })
        .collect();
    let buf = ctx.buf_with_lines(lines);
    let win = ctx.open_split(buf, pane("backdrop"));
    ctx.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(win),
    )]));
}

/// Push a small bordered overlay at `anchor`. Outer box sizes to
/// `(width + 2, height + 2)` so the anchor's clamping behaviour shows
/// against a deterministic-shaped frame, and the buffer's first
/// `height` lines are visible inside the border.
fn open_box_overlay(
    ctx: &mut crate::storybook::StoryCtx,
    title: &str,
    width: u16,
    height: u16,
    anchor: Anchor,
) {
    let dlg = ctx.buf_with_lines([title, "+++++"]);
    let dw = ctx.ui.win_open_split(dlg, pane("box")).expect("buf exists");
    let layout = LayoutTree::vbox(vec![(
        Constraint::Length(height),
        LayoutTree::hbox(vec![(Constraint::Length(width), LayoutTree::leaf(dw))]),
    )])
    .with_border(Border::ROUNDED)
    .with_title(title);
    ctx.ui
        .overlay_open(Overlay::new(layout, anchor).with_z(100));
}

story!(overlay_centered_modal_over_splits, |ctx| {
    ctx.set_viewport(40, 10);

    // Backdrop: one full-height pane the user would normally interact
    // with (a transcript stand-in).
    let backdrop = ctx.buf_with_lines(
        (1..=10)
            .map(|i| format!("backdrop row {i:02}"))
            .collect::<Vec<_>>(),
    );
    let bw = ctx.open_split(backdrop, pane("backdrop"));
    ctx.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(bw),
    )]));

    // Modal: one bordered leaf centered, smaller than the screen.
    // Leaves don't carry natural size, so size the modal explicitly:
    // Length(3) rows for the buffer's three lines, Length(14) cols for
    // its widest line ("  • alpha" plus a little headroom). Outer
    // box becomes 16×5 once the rounded border is added.
    let dlg = ctx.buf_with_lines(["pick one:", "  • alpha", "  • beta"]);
    let dw = ctx
        .ui
        .win_open_split(dlg, pane("dialog"))
        .expect("buf exists");
    let layout = LayoutTree::vbox(vec![(
        Constraint::Length(3),
        LayoutTree::hbox(vec![(Constraint::Length(14), LayoutTree::leaf(dw))]),
    )])
    .with_border(Border::ROUNDED)
    .with_title("modal");
    ctx.ui.overlay_open(
        Overlay::new(layout, Anchor::ScreenCenter)
            .modal(true)
            .with_z(100),
    );

    ctx.assert_snapshot();
});

// ── Anchor variant coverage ───────────────────────────────────────

story!(anchor_screen_at_topleft_corner, |ctx| {
    backdrop(ctx, 30, 8);
    open_box_overlay(
        ctx,
        "TL",
        6,
        2,
        Anchor::ScreenAt {
            row: 1,
            col: 1,
            corner: Corner::NW,
        },
    );
    ctx.assert_snapshot();
});

story!(anchor_screen_at_topright_corner, |ctx| {
    backdrop(ctx, 30, 8);
    open_box_overlay(
        ctx,
        "TR",
        6,
        2,
        Anchor::ScreenAt {
            row: 1,
            col: 28,
            corner: Corner::NE,
        },
    );
    ctx.assert_snapshot();
});

story!(anchor_screen_at_bottomleft_corner, |ctx| {
    backdrop(ctx, 30, 8);
    open_box_overlay(
        ctx,
        "BL",
        6,
        2,
        Anchor::ScreenAt {
            row: 6,
            col: 1,
            corner: Corner::SW,
        },
    );
    ctx.assert_snapshot();
});

story!(anchor_clamped_when_offscreen, |ctx| {
    // Anchor would place the overlay past the right edge — the
    // resolver clamps the rect to the terminal bounds.
    backdrop(ctx, 30, 6);
    open_box_overlay(
        ctx,
        "OOB",
        10,
        2,
        Anchor::ScreenAt {
            row: 0,
            col: 100,
            corner: Corner::NW,
        },
    );
    ctx.assert_snapshot();
});

story!(anchor_screen_bottom_docked, |ctx| {
    // ScreenBottom reserves `above_rows` cells for the statusline +
    // docks the overlay there full-width. `Percentage(100)` on the
    // inner Hbox sizes the leaf to the terminal width — same shape the
    // cmdline uses in production.
    backdrop(ctx, 30, 8);
    let dlg = ctx.buf_with_lines(["docked"]);
    let dw = ctx
        .ui
        .win_open_split(dlg, pane("dock"))
        .expect("buf exists");
    let layout = LayoutTree::vbox(vec![(
        Constraint::Length(1),
        LayoutTree::hbox(vec![(Constraint::Percentage(100), LayoutTree::leaf(dw))]),
    )])
    .with_border(Border::SINGLE)
    .with_title("dock");
    ctx.ui
        .overlay_open(Overlay::new(layout, Anchor::ScreenBottom { above_rows: 1 }));
    ctx.assert_snapshot();
});

// ── Z-ordering ────────────────────────────────────────────────────

story!(two_overlays_stack_by_z, |ctx| {
    backdrop(ctx, 30, 8);
    // Lower-z overlay first.
    let lo = ctx.buf_with_lines(["LO"]);
    let lw = ctx.ui.win_open_split(lo, pane("lo")).expect("buf exists");
    let lo_layout = LayoutTree::hbox(vec![(Constraint::Length(8), LayoutTree::leaf(lw))])
        .with_border(Border::SINGLE)
        .with_title("lo");
    ctx.ui.overlay_open(
        Overlay::new(
            lo_layout,
            Anchor::ScreenAt {
                row: 2,
                col: 6,
                corner: Corner::NW,
            },
        )
        .with_z(10),
    );
    // Higher-z overlay overlapping the first; should paint on top.
    let hi = ctx.buf_with_lines(["HI"]);
    let hw = ctx.ui.win_open_split(hi, pane("hi")).expect("buf exists");
    let hi_layout = LayoutTree::hbox(vec![(Constraint::Length(8), LayoutTree::leaf(hw))])
        .with_border(Border::ROUNDED)
        .with_title("hi");
    ctx.ui.overlay_open(
        Overlay::new(
            hi_layout,
            Anchor::ScreenAt {
                row: 3,
                col: 10,
                corner: Corner::NW,
            },
        )
        .with_z(50),
    );
    ctx.assert_snapshot();
});

// ── Win-attached anchor ───────────────────────────────────────────

story!(anchor_win_attaches_above_target, |ctx| {
    // Two-pane vbox; toast overlay attaches to the top of the bottom
    // pane via Win { attach: NW, row_offset: -1 }.
    ctx.set_viewport(30, 8);
    let top = ctx.buf_with_lines((1..=4).map(|i| format!("top row {i}")).collect::<Vec<_>>());
    let bot = ctx.buf_with_lines(["bot row"]);
    let wt = ctx.open_split(top, pane("top"));
    let wb = ctx.open_split(bot, pane("bot"));
    ctx.set_layout(LayoutTree::vbox(vec![
        (Constraint::Fill, LayoutTree::leaf(wt)),
        (Constraint::Length(2), LayoutTree::leaf(wb)),
    ]));

    let toast = ctx.buf_with_lines(["toast!"]);
    let tw = ctx
        .ui
        .win_open_split(toast, pane("toast"))
        .expect("buf exists");
    // Wrap the leaf in a 1-row Vbox so the toast has a non-zero
    // natural height (leaves carry no intrinsic size).
    let layout = LayoutTree::vbox(vec![(
        Constraint::Length(1),
        LayoutTree::hbox(vec![(Constraint::Length(8), LayoutTree::leaf(tw))]),
    )]);
    ctx.ui.overlay_open(Overlay::new(
        layout,
        Anchor::Win {
            target: wb,
            attach: Corner::NW,
            row_offset: -1,
            col_offset: 0,
        },
    ));
    ctx.assert_snapshot();
});
