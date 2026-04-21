use crate::grid::GridSlice;
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyResult {
    Consumed,
    Ignored,
    Action(String),
}

pub struct DrawContext {
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub focused: bool,
}

pub trait Component: 'static {
    fn draw(&self, area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext);
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult;
    fn cursor(&self) -> Option<(u16, u16)> {
        None
    }
    fn as_any(&self) -> &dyn std::any::Any {
        panic!("as_any not implemented")
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        panic!("as_any_mut not implemented")
    }
}
