//! `Surface` — pre-assembled renderer facade.
//!
//! Holds the four pieces a non-trivial renderer always needs together:
//! a [`Compositor`], an [`Arc<Theme>`], a [`LayoutTree`], and the
//! current terminal size. Exposes the Ui-shaped surface
//! (`set_layout` / `set_terminal_size` / `force_redraw` / `paint_rect`)
//! so non-editor consumers don't hand-roll the wiring every time.
//!
//! `smelt-edit`'s `Ui` is a thin editor layer on top of `Surface` —
//! it forwards renderer plumbing to the inner `Surface` and adds
//! editor-specific state (windows, buffers, focus, capture, overlays).
//! Standalone consumers (treemaps, dashboards, anything that doesn't
//! want vim or buffers) use `Surface` directly.

use std::io::Write;
use std::sync::Arc;

use smelt_buffer::theme::Theme;

use crate::compositor::Compositor;
use crate::grid::{Grid, GridSlice};
use crate::layout::{resolve_layout, LayoutTree, PaintId, Rect};
use crate::paint_layout_tree;

pub struct Surface {
    compositor: Compositor,
    layout: LayoutTree,
    theme: Arc<Theme>,
    size: (u16, u16),
}

impl Surface {
    pub fn new(width: u16, height: u16) -> Self {
        Self::with_theme(width, height, Theme::new())
    }

    pub fn with_theme(width: u16, height: u16, theme: Theme) -> Self {
        Self {
            compositor: Compositor::new(width, height),
            layout: LayoutTree::vbox(Vec::new()),
            theme: Arc::new(theme),
            size: (width, height),
        }
    }

    pub fn terminal_size(&self) -> (u16, u16) {
        self.size
    }

    pub fn set_terminal_size(&mut self, w: u16, h: u16) {
        self.size = (w, h);
        self.compositor.resize(w, h);
    }

    /// Full-screen rect at the current terminal size.
    pub fn area(&self) -> Rect {
        Rect::new(0, 0, self.size.0, self.size.1)
    }

    pub fn layout(&self) -> &LayoutTree {
        &self.layout
    }

    pub fn set_layout(&mut self, layout: LayoutTree) {
        self.layout = layout;
    }

    pub fn theme(&self) -> &Arc<Theme> {
        &self.theme
    }

    /// Mutable handle for in-place theme edits. Uses `Arc::make_mut`
    /// internally — copy-on-write, so mutations only clone when other
    /// references are outstanding.
    pub fn theme_mut(&mut self) -> &mut Theme {
        Arc::make_mut(&mut self.theme)
    }

    pub fn force_redraw(&mut self) {
        self.compositor.force_redraw();
    }

    /// Resolved screen rect for a `PaintId` leaf in the current layout
    /// tree, or `None` if the id isn't a leaf.
    pub fn paint_rect(&self, id: PaintId) -> Option<Rect> {
        resolve_layout(&self.layout, self.area()).get(&id).copied()
    }

    pub fn compositor(&self) -> &Compositor {
        &self.compositor
    }

    pub fn compositor_mut(&mut self) -> &mut Compositor {
        &mut self.compositor
    }

    /// Standard render path: walks the layout tree, paints chrome,
    /// hands each leaf's `GridSlice` to `paint`. The closure typically
    /// matches on `PaintId` and dispatches to its own painters.
    pub fn render<W, F>(&mut self, w: &mut W, mut paint: F) -> std::io::Result<()>
    where
        W: Write,
        F: FnMut(PaintId, &mut GridSlice<'_>, &Arc<Theme>),
    {
        let layout = self.layout.clone();
        let area = self.area();
        let size = self.size;
        let theme_arc = Arc::clone(&self.theme);
        self.compositor
            .render_with(&self.theme, w, move |grid, _theme| {
                let theme = &theme_arc;
                let mut dispatch = |id: PaintId,
                                    leaf: Rect,
                                    grid: &mut Grid,
                                    theme: &Arc<Theme>,
                                    _ts: (u16, u16)| {
                    let mut slice = grid.slice_mut(leaf);
                    paint(id, &mut slice, theme);
                };
                paint_layout_tree(grid, theme, &layout, area, size, &mut dispatch);
                None
            })
    }

    /// Bypass the layout tree entirely — paint directly into the
    /// compositor's grid. Returns optional absolute `(col, row)` for
    /// hardware cursor placement.
    pub fn render_raw<W, F>(&mut self, w: &mut W, paint: F) -> std::io::Result<()>
    where
        W: Write,
        F: FnOnce(&mut Grid, &Theme) -> Option<(u16, u16)>,
    {
        self.compositor.render_with(&self.theme, w, paint)
    }
}
