use crate::transcript_model::ViewState;

/// Settings threaded through the layout stage.
#[derive(Debug, Clone, Copy)]
pub struct LayoutContext {
    pub width: u16,
    pub view_state: ViewState,
}

impl LayoutContext {
    pub fn new(width: u16, view_state: ViewState) -> Self {
        Self { width, view_state }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_records_all_fields() {
        let ctx = LayoutContext::new(120, ViewState::default());
        assert_eq!(ctx.width, 120);
        assert_eq!(ctx.view_state, ViewState::default());
    }

    #[test]
    fn copy_clone_yields_equal_data() {
        let ctx = LayoutContext::new(80, ViewState::default());
        let copy = ctx;
        assert_eq!(copy.width, ctx.width);
        assert_eq!(copy.view_state, ctx.view_state);
    }
}
