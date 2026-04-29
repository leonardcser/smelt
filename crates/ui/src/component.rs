use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crate::theme::Theme;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent};

/// Semantic events emitted by widgets when a key resolves into a
/// high-level action. Replaces the old stringly-typed
/// `KeyResult::Action(String)` dispatch. Picker emits `Select`;
/// modal overlays dismiss on Esc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidgetEvent {
    /// User dismissed the surrounding container (Esc on a modal).
    Dismiss,
    /// User selected a specific row by index.
    Select(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyResult {
    Consumed,
    Ignored,
    Action(WidgetEvent),
    /// Mouse-only: the component handled a `Down` event AND wants drag
    /// capture — App should route subsequent `Drag` and `Up` events to
    /// this layer regardless of pointer position until release. Used by
    /// `TextInput` for click-drag text selection. Returned only from
    /// `handle_mouse`; treated as `Consumed` in key paths.
    Capture,
}

#[derive(Default, Clone)]
pub struct DrawContext {
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub focused: bool,
    /// Theme registry resolved per-frame from `Ui`. Widgets read named
    /// highlight groups (`"Visual"`, `"SmeltAccent"`, …) via
    /// `theme.get(name)`; missing names return `Style::default()`.
    pub theme: Theme,
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
    /// Resolve any internal layout that depends on the frame's
    /// allocated rect. Called once per frame before `draw`. Default
    /// no-op; components with sub-layout (e.g. Dialog's panel stack)
    /// override this.
    fn prepare(&mut self, _area: Rect, _ctx: &DrawContext) {}
    fn draw(&self, area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext);
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult;
    /// Handle a mouse event. Coordinates in `event` are absolute
    /// (terminal-relative). Components that care about clicks/drags
    /// override this; default returns `Ignored`. Wheel scroll is
    /// routed by App through dedicated scroll methods, not here, so
    /// implementations can ignore `ScrollUp/Down` variants.
    fn handle_mouse(&mut self, _event: MouseEvent) -> KeyResult {
        KeyResult::Ignored
    }
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
