//! Chrome stories — borders + titles painted on overlay containers.
//! `paint_chrome` is invoked by the overlay-paint pass, not splits;
//! these stories pin the rendered chrome glyphs for each border
//! variant + title placement / truncation.

use tui::ui::layout::{Anchor, Border, Constraint, Corner, Gutters};
use tui::ui::{LayoutTree, Overlay, SplitConfig};

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
    border: Border,
    title: Option<&str>,
) {
    let buf = ctx.buf_with_lines(["content"]);
    let dw = ctx
        .ui
        .win_open_split(buf, pane("inner"))
        .expect("buf exists");
    let mut tree = LayoutTree::hbox(vec![(Constraint::Length(width), LayoutTree::leaf(dw))])
        .with_border(border);
    if let Some(t) = title {
        tree = tree.with_title(t);
    }
    let _ = height;
    ctx.ui.overlay_open(Overlay::new(
        tree,
        Anchor::ScreenAt {
            row: 1,
            col: 1,
            corner: Corner::NW,
        },
    ));
}

// ── Border styles ─────────────────────────────────────────────────

story!(border_single_no_title, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Border::Single, None);
    ctx.assert_snapshot();
});

story!(border_single_with_title, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Border::Single, Some("title"));
    ctx.assert_snapshot();
});

story!(border_double_with_title, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Border::Double, Some("dbl"));
    ctx.assert_snapshot();
});

story!(border_rounded_with_title, |ctx| {
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Border::Rounded, Some("rnd"));
    ctx.assert_snapshot();
});

story!(border_none_omits_frame, |ctx| {
    // Overlay with `Border::None` paints content at the full anchor
    // rect — no border row, no inset. Title is meaningless without
    // a top border.
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 12, 3, Border::None, Some("ignored"));
    ctx.assert_snapshot();
});

// ── Title truncation ──────────────────────────────────────────────

story!(title_truncates_in_narrow_border, |ctx| {
    // Width 10 with rounded border leaves ~6 cells for the title.
    // The literal "this is a long title" must clip cleanly inside
    // the top border row.
    dotted_backdrop(ctx, 18, 5);
    open_chrome_overlay(ctx, 10, 3, Border::Single, Some("this is a long title"));
    ctx.assert_snapshot();
});

// Note: the chrome painter's "width < 2 OR height < 2" early-return
// isn't reachable from the public Anchor API — natural-size
// resolution inflates overlay rects to fit the layout + chrome. If
// that path becomes reachable, add guard stories here.
