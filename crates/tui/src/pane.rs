//! Pane abstraction — UI containers that own a `Buffer` + viewport
//! state and handle their own focus/mouse/scroll.
//!
//! The TUI currently has two panes:
//!
//! - **Prompt** — writable buffer; lives inside `InputState` which
//!   layers the completer/menu/history around it.
//! - **Content** — readonly buffer that mirrors the rendered
//!   transcript; drives vim motions, visual selection, yank.
//!
//! This module holds `ContentPane`, the state + behaviour formerly
//! spread across `App.content_buffer` + `history_cursor_line/col` +
//! `history_scroll_offset`. The prompt pane is kept as `InputState`
//! (plus its completer/menu side-car) so the rich edit stack there
//! doesn't have to be wrapped in a trait object right now.

use crate::buffer::Buffer;

/// Readonly pane showing the scrollback / transcript. Owns its
/// `Buffer` (vim instance + kill ring + undo + text cache) and the
/// viewport scroll / cursor row+col.
pub struct ContentPane {
    /// Underlying readonly buffer — vim motions run against its
    /// `buf`, which the caller refreshes from the rendered transcript
    /// before every key dispatch (the transcript is rebuilt each frame).
    pub buffer: Buffer,
    /// Rows scrolled away from the bottom of the transcript. 0 = the
    /// most recent row is visible at the bottom of the viewport.
    pub scroll_offset: u16,
    /// Cursor row relative to the viewport bottom (0 = bottom-most
    /// row, `viewport_rows - 1` = top row).
    pub cursor_line: u16,
    /// Cursor column (visual char index within the line).
    pub cursor_col: u16,
}

impl ContentPane {
    pub fn new() -> Self {
        Self {
            buffer: Buffer::readonly(),
            scroll_offset: 0,
            cursor_line: 0,
            cursor_col: 0,
        }
    }
}

impl Default for ContentPane {
    fn default() -> Self {
        Self::new()
    }
}

/// Identifier for which pane currently holds focus. `AppFocus` in
/// `app/mod.rs` mirrors these values — both will be unified once the
/// prompt side migrates to a `PromptPane` wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneId {
    Prompt,
    Content,
}
