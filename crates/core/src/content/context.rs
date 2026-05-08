use crate::transcript_model::ViewState;

/// Settings threaded through the layout stage.
#[derive(Debug, Clone, Copy)]
pub struct LayoutContext {
    pub width: u16,
    pub show_thinking: bool,
    pub view_state: ViewState,
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
