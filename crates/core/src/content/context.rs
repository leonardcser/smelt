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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_records_all_fields() {
        let ctx = LayoutContext::new(120, true, ViewState::default());
        assert_eq!(ctx.width, 120);
        assert!(ctx.show_thinking);
    }

    #[test]
    fn copy_clone_yields_equal_data() {
        let ctx = LayoutContext::new(80, false, ViewState::default());
        let copy = ctx;
        assert_eq!(copy.width, ctx.width);
        assert_eq!(copy.show_thinking, ctx.show_thinking);
    }
}
