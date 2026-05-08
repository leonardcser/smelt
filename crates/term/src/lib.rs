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
pub use layout::{Border, Constraint, Corner, Gutters, LayoutTree, PaintId, Rect};
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
/// each resolved leaf rect to `paint`.
pub fn paint_layout_tree(
    grid: &mut Grid,
    theme: &std::sync::Arc<Theme>,
    node: &LayoutTree,
    area: Rect,
    term_size: (u16, u16),
    paint: &mut PaintDispatch,
) {
    match node {
        LayoutTree::Leaf(id) => {
            paint(*id, area, grid, theme, term_size);
        }
        LayoutTree::Vbox { items, chrome } | LayoutTree::Hbox { items, chrome } => {
            layout::paint_chrome(grid, area, chrome, theme);
            let vertical = matches!(node, LayoutTree::Vbox { .. });
            let inner = layout::inset_for_border(area, chrome.border);
            let primary_total = if vertical { inner.height } else { inner.width };
            let total_gap = chrome
                .gap
                .saturating_mul(items.len().saturating_sub(1) as u16);
            let available = primary_total.saturating_sub(total_gap);
            let sizes = layout::resolve_constraints(items, available);
            let mut offset = 0u16;
            for (i, ((_, child), &size)) in items.iter().zip(sizes.iter()).enumerate() {
                let child_area = if vertical {
                    Rect::new(inner.top + offset, inner.left, inner.width, size)
                } else {
                    Rect::new(inner.top, inner.left + offset, size, inner.height)
                };
                paint_layout_tree(grid, theme, child, child_area, term_size, paint);
                offset += size;
                if i + 1 < items.len() {
                    offset += chrome.gap;
                }
            }
        }
    }
}
