//! Layout-tree paint stories. Pure structural shapes with no Lua, no
//! engine — these hunt solver bugs, chrome painting, and gap math.

use tui::smelt_term::layout::{Border, Constraint, Gutters};
use tui::smelt_term::{LayoutTree, SplitConfig};

fn pane_config(region: &str) -> SplitConfig {
    SplitConfig {
        region: region.to_string(),
        gutters: Gutters::default(),
    }
}

story!(vbox_three_panes, |ctx| {
    ctx.set_viewport(40, 8);
    let top = ctx.buf_with_lines(["top pane"]);
    let mid = ctx.buf_with_lines(["middle pane"]);
    let bot = ctx.buf_with_lines(["bottom pane"]);
    let w_top = ctx.open_split(top, pane_config("top"));
    let w_mid = ctx.open_split(mid, pane_config("mid"));
    let w_bot = ctx.open_split(bot, pane_config("bot"));
    ctx.set_layout(LayoutTree::vbox(vec![
        (Constraint::Length(2), LayoutTree::leaf(w_top)),
        (Constraint::Fill, LayoutTree::leaf(w_mid)),
        (Constraint::Length(2), LayoutTree::leaf(w_bot)),
    ]));
    ctx.assert_snapshot();
});

story!(splits_paint_border_and_title, |ctx| {
    // Splits paint chrome the same way overlays do — `paint_layout_node`
    // runs over the splits tree, so `with_border` / `with_title` on the
    // top-level layout renders a frame around the inset content rect.
    ctx.set_viewport(30, 6);
    let buf = ctx.buf_with_lines(["inside the bordered pane"]);
    let win = ctx.open_split(buf, pane_config("only"));
    ctx.set_layout(
        LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(win))])
            .with_border(Border::ROUNDED)
            .with_title("frame"),
    );
    ctx.assert_snapshot();
});

story!(hbox_two_columns_with_gap, |ctx| {
    ctx.set_viewport(40, 4);
    let left = ctx.buf_with_lines(["left col"]);
    let right = ctx.buf_with_lines(["right col"]);
    let wl = ctx.open_split(left, pane_config("left"));
    let wr = ctx.open_split(right, pane_config("right"));
    ctx.set_layout(
        LayoutTree::hbox(vec![
            (Constraint::Fill, LayoutTree::leaf(wl)),
            (Constraint::Fill, LayoutTree::leaf(wr)),
        ])
        .with_gap(2),
    );
    ctx.assert_snapshot();
});

story!(vbox_nested_in_hbox, |ctx| {
    ctx.set_viewport(40, 6);
    let l = ctx.buf_with_lines(["sidebar"]);
    let t = ctx.buf_with_lines(["main top"]);
    let b = ctx.buf_with_lines(["main bottom"]);
    let wl = ctx.open_split(l, pane_config("sidebar"));
    let wt = ctx.open_split(t, pane_config("main_top"));
    let wb = ctx.open_split(b, pane_config("main_bot"));
    let main = LayoutTree::vbox(vec![
        (Constraint::Fill, LayoutTree::leaf(wt)),
        (Constraint::Length(2), LayoutTree::leaf(wb)),
    ]);
    ctx.set_layout(LayoutTree::hbox(vec![
        (Constraint::Length(10), LayoutTree::leaf(wl)),
        (Constraint::Fill, main),
    ]));
    ctx.assert_snapshot();
});

// ── Constraint coverage ───────────────────────────────────────────

story!(vbox_percentage_split, |ctx| {
    ctx.set_viewport(20, 10);
    let top = ctx.buf_with_lines(["30 percent"]);
    let bot = ctx.buf_with_lines(["70 percent"]);
    let wt = ctx.open_split(top, pane_config("top"));
    let wb = ctx.open_split(bot, pane_config("bot"));
    ctx.set_layout(LayoutTree::vbox(vec![
        (Constraint::Percentage(30), LayoutTree::leaf(wt)),
        (Constraint::Percentage(70), LayoutTree::leaf(wb)),
    ]));
    ctx.assert_snapshot();
});

story!(hbox_ratio_split_one_to_two, |ctx| {
    ctx.set_viewport(30, 3);
    let l = ctx.buf_with_lines(["L"]);
    let r = ctx.buf_with_lines(["R"]);
    let wl = ctx.open_split(l, pane_config("l"));
    let wr = ctx.open_split(r, pane_config("r"));
    ctx.set_layout(LayoutTree::hbox(vec![
        (Constraint::Ratio(1, 3), LayoutTree::leaf(wl)),
        (Constraint::Ratio(2, 3), LayoutTree::leaf(wr)),
    ]));
    ctx.assert_snapshot();
});

story!(vbox_min_competes_with_fill, |ctx| {
    // 12-row viewport, two children: Min(3) and Fill. Min is a Fill
    // with a floor of 3 — both share the 12 rows equally (6/6) since
    // the equal share already satisfies the floor.
    ctx.set_viewport(20, 12);
    let a = ctx.buf_with_lines(["min child"]);
    let b = ctx.buf_with_lines(["fill child"]);
    let wa = ctx.open_split(a, pane_config("a"));
    let wb = ctx.open_split(b, pane_config("b"));
    ctx.set_layout(LayoutTree::vbox(vec![
        (Constraint::Min(3), LayoutTree::leaf(wa)),
        (Constraint::Fill, LayoutTree::leaf(wb)),
    ]));
    ctx.assert_snapshot();
});

story!(vbox_max_clamps_pane_height, |ctx| {
    // Max(2) acts like Length(2) when the parent has at least 2 cells;
    // Fill takes the rest.
    ctx.set_viewport(20, 8);
    let cap = ctx.buf_with_lines(["capped at 2 rows tall — extra lines clip"]);
    let rest = ctx.buf_with_lines(["rest"]);
    let wc = ctx.open_split(cap, pane_config("cap"));
    let wr = ctx.open_split(rest, pane_config("rest"));
    ctx.set_layout(LayoutTree::vbox(vec![
        (Constraint::Max(2), LayoutTree::leaf(wc)),
        (Constraint::Fill, LayoutTree::leaf(wr)),
    ]));
    ctx.assert_snapshot();
});

story!(hbox_three_fill_split_evenly, |ctx| {
    // 30-column viewport, three Fill siblings → 10 each.
    ctx.set_viewport(30, 3);
    let a = ctx.buf_with_lines(["AAA"]);
    let b = ctx.buf_with_lines(["BBB"]);
    let c = ctx.buf_with_lines(["CCC"]);
    let wa = ctx.open_split(a, pane_config("a"));
    let wb = ctx.open_split(b, pane_config("b"));
    let wc = ctx.open_split(c, pane_config("c"));
    ctx.set_layout(LayoutTree::hbox(vec![
        (Constraint::Fill, LayoutTree::leaf(wa)),
        (Constraint::Fill, LayoutTree::leaf(wb)),
        (Constraint::Fill, LayoutTree::leaf(wc)),
    ]));
    ctx.assert_snapshot();
});

// ── Chrome + gap math ─────────────────────────────────────────────

story!(hbox_gap_consumes_pixels, |ctx| {
    // gap=4 between two equal-Fill children steals 4 cols off the
    // 20-col viewport; each child gets 8.
    ctx.set_viewport(20, 3);
    let l = ctx.buf_with_lines(["left"]);
    let r = ctx.buf_with_lines(["right"]);
    let wl = ctx.open_split(l, pane_config("l"));
    let wr = ctx.open_split(r, pane_config("r"));
    ctx.set_layout(
        LayoutTree::hbox(vec![
            (Constraint::Fill, LayoutTree::leaf(wl)),
            (Constraint::Fill, LayoutTree::leaf(wr)),
        ])
        .with_gap(4),
    );
    ctx.assert_snapshot();
});

story!(nested_borders_inset_correctly, |ctx| {
    // Outer Single, inner Rounded — verify the inner border sits
    // 1 cell inside the outer on every side.
    ctx.set_viewport(20, 8);
    let buf = ctx.buf_with_lines(["nested"]);
    let win = ctx.open_split(buf, pane_config("only"));
    let inner = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(win))])
        .with_border(Border::ROUNDED)
        .with_title("inner");
    ctx.set_layout(
        LayoutTree::vbox(vec![(Constraint::Fill, inner)])
            .with_border(Border::SINGLE)
            .with_title("outer"),
    );
    ctx.assert_snapshot();
});

// ── Edge cases ────────────────────────────────────────────────────

story!(single_cell_viewport_renders_safely, |ctx| {
    // 1×1 terminal — a Length(1) leaf gets one cell. Worst-case
    // smoke: no panic, no off-by-one in the chrome path.
    ctx.set_viewport(1, 1);
    let buf = ctx.buf_with_lines(["x"]);
    let win = ctx.open_split(buf, pane_config("only"));
    ctx.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(win),
    )]));
    ctx.assert_snapshot();
});

story!(vbox_overflow_clamps_content, |ctx| {
    // 6-row viewport with three Length(3) leaves (sum 9). Solver
    // distributes the deficit; later leaves should be squeezed or
    // fall off.
    ctx.set_viewport(20, 6);
    let a = ctx.buf_with_lines(["aaa one", "aaa two", "aaa three"]);
    let b = ctx.buf_with_lines(["bbb one", "bbb two", "bbb three"]);
    let c = ctx.buf_with_lines(["ccc one", "ccc two", "ccc three"]);
    let wa = ctx.open_split(a, pane_config("a"));
    let wb = ctx.open_split(b, pane_config("b"));
    let wc = ctx.open_split(c, pane_config("c"));
    ctx.set_layout(LayoutTree::vbox(vec![
        (Constraint::Length(3), LayoutTree::leaf(wa)),
        (Constraint::Length(3), LayoutTree::leaf(wb)),
        (Constraint::Length(3), LayoutTree::leaf(wc)),
    ]));
    ctx.assert_snapshot();
});

story!(vbox_mixed_length_fill_and_min, |ctx| {
    // Length(2) + Min(2) + Fill in a 10-row viewport. Length consumes
    // 2; the remaining 8 splits equally between Min and Fill (4/4),
    // since the Min floor is already met.
    ctx.set_viewport(15, 10);
    let head = ctx.buf_with_lines(["header line"]);
    let body = ctx.buf_with_lines(["body min line"]);
    let foot = ctx.buf_with_lines(["foot fill"]);
    let wh = ctx.open_split(head, pane_config("head"));
    let wbod = ctx.open_split(body, pane_config("body"));
    let wf = ctx.open_split(foot, pane_config("foot"));
    ctx.set_layout(LayoutTree::vbox(vec![
        (Constraint::Length(2), LayoutTree::leaf(wh)),
        (Constraint::Min(2), LayoutTree::leaf(wbod)),
        (Constraint::Fill, LayoutTree::leaf(wf)),
    ]));
    ctx.assert_snapshot();
});
