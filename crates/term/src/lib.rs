//! Pure terminal renderer: double-buffered diff/flush, `LayoutTree`, `Grid`,
//! `paint_chrome`, and half-block-friendly cell primitives.
//! Editor concepts (`Window`, `Buffer`, overlays) live in `smelt-edit`.
//!
//! Key entry points:
//! - [`Compositor::render_with`] - drive a frame.
//! - [`paint_layout_tree`] - walk a [`LayoutTree`] and dispatch leaves.
//! - [`flush_diff`] - emit SGR escapes for a `Grid` diff.
//! - [`Grid`] / [`GridSlice`] - `set` / `put_str` (full overwrite; string
//!   writes return the clipped end column), `put_char` / `put_str_fg` /
//!   `put_line` (preserve bg where applicable).
//! - [`TerminalSession`] - raw-mode/alternate-screen lifecycle guard with
//!   suspend support for shell-outs.

pub mod ansi;
pub mod compositor;
pub mod flush;
pub mod geometry;
pub mod grid;
pub mod hit;
pub mod layout;
pub mod line;
pub mod session;
pub mod snapshot;
pub mod surface;

pub use compositor::Compositor;
pub use flush::flush_diff;
pub use geometry::Insets;
pub use grid::{
    display_width, truncate_width, Cell, CellUpdate, Grid, GridSlice, Style, TextAlign,
};
pub use hit::HitRegistry;
pub use layout::{
    resolve_containers_with, resolve_layout, resolve_layout_ordered, resolve_layout_ordered_with,
    resolve_layout_with, Align, Border, Constraint, ContainerId, Corner, Gutters, LayoutRect,
    LayoutTree, LeafSizer, Natural, NaturalRef, NoopSizer, PaintId, Rect, StaticNatural,
};
pub use line::{Line, Span};
pub use session::{SuspendScreen, TerminalSession, TerminalSessionBuilder};
pub use smelt_style::style::Color;
pub use smelt_style::theme::Theme;
pub use snapshot::SnapshotFrame;
pub use surface::Surface;

/// Per-leaf paint callback: `(paint_id, leaf_rect, grid, theme, terminal_size)`.
/// The renderer calls this for each resolved [`LayoutTree::Leaf`].
pub type PaintDispatch<'a> =
    dyn FnMut(PaintId, Rect, &mut Grid, &std::sync::Arc<Theme>, (u16, u16)) + 'a;

pub struct PaintLayoutOptions<'a> {
    pub sizer: &'a dyn layout::LeafSizer,
    pub root_chrome: layout::ChromePaintCtx,
}

/// Walk `node` against `area`, paint chrome on containers, and dispatch
/// each resolved leaf rect to `paint`. `Fit` constraints use the default
/// `NoopSizer` (contribute `0`); use `paint_layout_tree_with` to drive
/// content-aware sizing.
pub fn paint_layout_tree(
    grid: &mut Grid,
    theme: &std::sync::Arc<Theme>,
    node: &LayoutTree,
    area: Rect,
    term_size: (u16, u16),
    paint: &mut PaintDispatch,
) {
    paint_layout_tree_with(
        grid,
        theme,
        node,
        area,
        term_size,
        &layout::NoopSizer,
        paint,
    );
}

/// Like [`paint_layout_tree`] but uses `sizer` to resolve `Fit` constraints
/// against each leaf's natural size. Must use the same sizer as the rect
/// resolution that drives hit-testing and viewport setup, so painted rects
/// match.
pub fn paint_layout_tree_with(
    grid: &mut Grid,
    theme: &std::sync::Arc<Theme>,
    node: &LayoutTree,
    area: Rect,
    term_size: (u16, u16),
    sizer: &dyn layout::LeafSizer,
    paint: &mut PaintDispatch,
) {
    paint_layout_tree_with_options(
        grid,
        theme,
        node,
        area,
        term_size,
        PaintLayoutOptions {
            sizer,
            root_chrome: layout::ChromePaintCtx::empty(),
        },
        paint,
    );
}

pub fn paint_layout_tree_with_options(
    grid: &mut Grid,
    theme: &std::sync::Arc<Theme>,
    node: &LayoutTree,
    area: Rect,
    term_size: (u16, u16),
    options: PaintLayoutOptions<'_>,
    paint: &mut PaintDispatch,
) {
    let mut walk = PaintLayoutWalk {
        theme,
        term_size,
        options,
        paint,
    };
    paint_layout_tree_inner(grid, node, area, 0, &mut walk);
}

struct PaintLayoutWalk<'a, 'b> {
    theme: &'a std::sync::Arc<Theme>,
    term_size: (u16, u16),
    options: PaintLayoutOptions<'a>,
    paint: &'a mut PaintDispatch<'b>,
}

fn paint_layout_tree_inner(
    grid: &mut Grid,
    node: &LayoutTree,
    area: Rect,
    depth: usize,
    walk: &mut PaintLayoutWalk<'_, '_>,
) {
    let chrome_ctx = if depth == 0 {
        walk.options.root_chrome
    } else {
        layout::ChromePaintCtx::empty()
    };
    match node {
        LayoutTree::Leaf { id, chrome, .. } => {
            layout::paint_chrome_with(grid, area, chrome, walk.theme, chrome_ctx);
            let inner = layout::inset_for_chrome(area, chrome);
            (walk.paint)(*id, inner, grid, walk.theme, walk.term_size);
        }
        LayoutTree::Vbox { items, chrome } | LayoutTree::Hbox { items, chrome } => {
            layout::paint_chrome_with(grid, area, chrome, walk.theme, chrome_ctx);
            let vertical = matches!(node, LayoutTree::Vbox { .. });
            let (_, rects) =
                layout::layout_box_children(items, chrome, area, vertical, walk.options.sizer);
            for ((_, child), &rect) in items.iter().zip(rects.iter()) {
                paint_layout_tree_inner(grid, child, rect, depth + 1, walk);
            }
        }
    }
}
