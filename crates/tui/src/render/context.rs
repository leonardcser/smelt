//! Width-independent context plumbed through layout and paint stages.

use crate::theme::Theme;

/// Settings that flow through the layout stage. Layout produces a
/// theme-independent `DisplayBlock` so the only width-relevant inputs
/// are the terminal width and whether thinking blocks are expanded.
#[derive(Debug, Clone, Copy)]
pub struct LayoutContext {
    pub width: u16,
    pub show_thinking: bool,
}

/// Context for the paint stage. Carries the active theme snapshot so
/// `ColorRole::*` spans resolve to the same colors for every block in
/// one redraw.
#[derive(Debug, Clone, Copy)]
pub struct PaintContext<'a> {
    pub theme: &'a Theme,
    pub term_width: u16,
}
