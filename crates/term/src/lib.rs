//! `smelt-term` — terminal rendering library.
//!
//! Pure renderer: `Compositor`, `Grid`, `LayoutTree`, double-buffered
//! diff/flush, `paint_chrome`, half-block-friendly cell primitives. No
//! editor concepts (`Window`, `Buffer`, vim, callbacks, overlays);
//! those live in `smelt-edit`.
//!
//! Public entry points:
//! - [`Compositor`] / [`Compositor::render_with`] — drive a frame.
//! - [`paint_layout_tree`] — walk a [`LayoutTree`], paint chrome, and
//!   dispatch each leaf to a host-supplied paint callback.
//! - [`flush_diff`] — emit cell-diff SGR escapes for a `Grid` diff.
//! - [`Grid`] / [`GridSlice`] — the cell grid, with `set` / `put_str`
//!   (full overwrite) and `put_char` / `put_str_fg` / `put_line`
//!   (partial — preserve bg).

pub mod compositor;
pub mod flush;
pub mod geometry;
pub mod grid;
pub mod hit;
pub mod layout;
pub mod line;
pub mod snapshot;

pub use compositor::Compositor;
pub use flush::flush_diff;
pub use grid::{to_crossterm_color, Cell, CellUpdate, Grid, GridSlice, Style};
pub use hit::HitRegistry;
pub use layout::{Border, Constraint, Corner, Gutters, LayoutTree, PaintId, Rect};
pub use line::{Line, Span};
pub use smelt_buffer::style::Color;
pub use smelt_buffer::theme::{Theme, DEFAULT_ACCENT};
pub use snapshot::SnapshotFrame;

/// Per-leaf paint dispatcher: `(paint_id, leaf_rect, grid, theme,
/// terminal_size)`. The renderer hands each [`LayoutTree::Leaf`] to
/// this callback after resolving its rect; the callback owns the
/// painting (typically `grid.slice_mut(rect)` and writing through the
/// slice) and ascribes whatever semantics it likes to the paint id.
pub type PaintDispatch<'a> =
    dyn FnMut(PaintId, Rect, &mut Grid, &std::sync::Arc<Theme>, (u16, u16)) + 'a;

/// Walk `node` against `area`, painting container chrome and
/// dispatching each leaf to `paint`. Containers (`Vbox` / `Hbox`)
/// render their border + title before recursing into children at
/// resolved rects; leaves are forwarded to the paint callback with
/// the resolved leaf rect. The renderer ascribes no semantics to
/// `PaintId` — host code (typically `smelt-edit`'s `Ui::render`) maps
/// it back to whatever leaf it represents.
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
