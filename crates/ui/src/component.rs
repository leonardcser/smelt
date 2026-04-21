use crate::grid::{GridSlice, Style};
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

#[derive(Debug, Clone)]
pub struct CursorInfo {
    pub col: u16,
    pub row: u16,
    pub style: Option<CursorStyle>,
}

#[derive(Debug, Clone)]
pub struct CursorStyle {
    pub glyph: char,
    pub style: Style,
}

impl CursorInfo {
    pub fn hardware(col: u16, row: u16) -> Self {
        Self {
            col,
            row,
            style: None,
        }
    }

    pub fn block(col: u16, row: u16, glyph: char, style: Style) -> Self {
        Self {
            col,
            row,
            style: Some(CursorStyle { glyph, style }),
        }
    }
}

pub trait Component: 'static {
    fn draw(&self, area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext);
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult;
    fn cursor(&self) -> Option<CursorInfo> {
        None
    }
    fn as_any(&self) -> &dyn std::any::Any {
        panic!("as_any not implemented")
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        panic!("as_any_mut not implemented")
    }
}
