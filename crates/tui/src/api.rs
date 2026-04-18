//! Typed mutation surface for app code to drive window + buffer
//! state. Each operation is a named function with clear pre/post-
//! conditions, so call sites read as intent rather than tag dispatch.
//!
//! This mirrors neovim's `vim.api.{nvim_buf_*, nvim_win_*}` split:
//! `buf::*` operates on buffer text; `win::*` on cursor + viewport +
//! selection. The same surface internal code uses is what plugins will
//! eventually use — keep it small, named, and intent-shaped.

/// Buffer-level operations — text, attachments, whole-buffer replace.
pub mod buf {
    use crate::buffer::Buffer;
    use crate::input::InputState;

    /// Read the text of a buffer.
    pub fn get_text(buffer: &Buffer) -> &str {
        &buffer.buf
    }

    /// Replace the prompt buffer's text wholesale. Snapshots undo,
    /// clears attachments + shift-selection anchor, resets paste
    /// state, drops the completer so it re-derives, and places the
    /// cursor at `cursor` (or end-of-text if `None`).
    ///
    /// This is the canonical path for commands that stuff new text
    /// into the prompt (unqueue, resume restore, ghost accept). Direct
    /// `input.buf = …` writes skip these invariants and have been a
    /// recurring source of undo / completer / paste-state bugs.
    pub fn replace(input: &mut InputState, text: String, cursor: Option<usize>) {
        input.replace_text(text, cursor);
    }
}

/// Command dispatch — the single entry point for `/cmd` and `:cmd`
/// style actions. Internal handlers and (future) plugin handlers both
/// register here; keybindings resolve to names that route through
/// `run`. Modelled on nvim's `nvim_command` / `user_command` split.
pub mod cmd {
    use crate::app::App;

    /// Result of running a command — tells the caller whether the app
    /// should continue, quit, clear, open a dialog, etc. Thin wrapper
    /// around the internal `CommandAction` so the outer API can evolve
    /// without exposing every enum variant.
    pub use crate::app::commands::CommandOutcome as Outcome;

    /// Run a command line. Accepts `/name args...` or `:name args...`
    /// or a bare `name`. Parses the name, looks it up in the registry,
    /// falls back to the legacy match for commands not yet migrated.
    ///
    /// The same code path runs whether the user typed the command,
    /// pressed a keybind that resolved to `Action::Cmd(name)`, or a
    /// plugin invoked it programmatically.
    pub fn run(app: &mut App, line: &str) -> Outcome {
        crate::app::commands::run_command(app, line)
    }
}

/// Window-level operations — cursor, scroll, selection. Operates on
/// the shared [`crate::window::Window`] trait so prompt and transcript
/// share one call surface.
pub mod win {
    use crate::window::Window;

    /// Current cursor position (byte offset into the window's buffer).
    pub fn cursor(win: &dyn Window) -> usize {
        win.cursor()
    }

    /// Move the cursor to `pos` (clamped to the buffer's end). Does
    /// not touch selection — callers extending a selection must call
    /// `extend_selection_to` instead.
    pub fn set_cursor(win: &mut dyn Window, pos: usize) {
        win.set_cursor(pos);
    }

    /// Current selection range (`None` when no selection is active).
    pub fn selection(win: &dyn Window) -> Option<(usize, usize)> {
        win.selection()
    }

    /// Drop any in-progress selection (shift-anchor or vim visual).
    pub fn clear_selection(win: &mut dyn Window) {
        win.clear_selection();
    }

    /// Current top-of-viewport row (window-role specific interpretation).
    pub fn scroll_top(win: &dyn Window) -> u16 {
        win.scroll_top()
    }

    /// Set the top-of-viewport row. Only moves the viewport — does not
    /// re-anchor or move the cursor. Callers that need cursor follow
    /// (e.g. scrollbar drag) apply their own reanchor after.
    pub fn set_scroll_top(win: &mut dyn Window, row: u16) {
        win.set_scroll_top(row);
    }
}
