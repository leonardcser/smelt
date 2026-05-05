//! Width-independent context plumbed through the layout stage.

use crate::transcript_model::ViewState;

/// Settings that flow through the layout stage. Layout produces a
/// theme-independent `DisplayBlock` so the only width-relevant inputs
/// are the viewport width, whether thinking blocks are expanded, and
/// the per-block view state (expanded / collapsed / trimmed).
#[derive(Debug, Clone, Copy)]
pub struct LayoutContext {
    pub(crate) width: u16,
    pub(crate) show_thinking: bool,
    pub(crate) view_state: ViewState,
}

impl LayoutContext {
    pub fn new(width: u16, show_thinking: bool, view_state: ViewState) -> Self {
        Self {
            width,
            show_thinking,
            view_state,
        }
    }
}
