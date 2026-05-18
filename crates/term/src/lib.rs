//! Pure terminal renderer: double-buffered diff/flush, `LayoutTree`, `Grid`,
//! `paint_chrome`, and half-block-friendly cell primitives.
//! Editor concepts (`Window`, `Buffer`, overlays) live in `smelt-edit`.
//!
//! Key entry points:
//! - [`Compositor::render_with`] — drive a frame.
//! - [`paint_layout_tree`] — walk a [`LayoutTree`] and dispatch leaves.
//! - [`flush_diff`] — emit SGR escapes for a `Grid` diff.
//! - [`Grid`] / [`GridSlice`] — `set`/`put_str` (full overwrite),
//!   `put_char`/`put_str_fg`/`put_line` (preserve bg).

pub mod compositor;
pub mod flush;
pub mod geometry;
pub mod grid;
pub mod hit;
pub mod layout;
pub mod line;
pub mod snapshot;
pub mod surface;

pub use compositor::Compositor;
pub use flush::flush_diff;
pub use grid::{Cell, CellUpdate, Grid, GridSlice, Style};
pub use hit::HitRegistry;
pub use layout::{
    Align, Border, Constraint, Corner, Gutters, LayoutTree, LeafSizer, NoopSizer, PaintId, Rect,
};
pub use line::{Line, Span};
pub use smelt_style::style::Color;
pub use smelt_style::theme::{Theme, DEFAULT_ACCENT};
pub use snapshot::SnapshotFrame;
pub use surface::Surface;

/// Per-leaf paint callback: `(paint_id, leaf_rect, grid, theme, terminal_size)`.
/// The renderer calls this for each resolved [`LayoutTree::Leaf`].
pub type PaintDispatch<'a> =
    dyn FnMut(PaintId, Rect, &mut Grid, &std::sync::Arc<Theme>, (u16, u16)) + 'a;

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
    match node {
        LayoutTree::Leaf { id, chrome } => {
            layout::paint_chrome(grid, area, chrome, theme);
            let inner = layout::inset_for_border(area, chrome.border);
            paint(*id, inner, grid, theme, term_size);
        }
        LayoutTree::Vbox { items, chrome } | LayoutTree::Hbox { items, chrome } => {
            layout::paint_chrome(grid, area, chrome, theme);
            let vertical = matches!(node, LayoutTree::Vbox { .. });
            let (_, rects) = layout::layout_box_children(items, chrome, area, vertical, sizer);
            for ((_, child), &rect) in items.iter().zip(rects.iter()) {
                paint_layout_tree_with(grid, theme, child, rect, term_size, sizer, paint);
            }
        }
    }
}
