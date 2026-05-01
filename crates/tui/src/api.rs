//! Typed mutation surface for app code to drive window + buffer
//! state. Each operation is a named function with clear pre/post-
//! conditions, so call sites read as intent rather than tag dispatch.
//!
//! This mirrors neovim's `vim.api.{nvim_buf_*, nvim_win_*}` split:
//! `buf::*` operates on buffer text; `win::*` on cursor + viewport +
//! selection. The same surface internal code uses is what plugins will
//! eventually use — keep it small, named, and intent-shaped.
//!
//! # Stability
//!
//! Breaking changes to any `pub fn` in this module bump [`VERSION`].
//! User scripts (Lua, Rust plugins) can branch on it to target a
//! specific API generation.

/// Semantic-version tag for the public API surface. Increments on any
/// signature change, removal, or behaviour-altering rename. Additive
/// changes (new functions) do not bump the version.
pub(crate) const VERSION: &str = "1";

/// Buffer-level operations — text, attachments, whole-buffer replace.
pub(crate) mod buf {
    use crate::input::PromptState;

    /// Replace the prompt buffer's text wholesale. Snapshots undo,
    /// clears attachments + shift-selection anchor, resets paste
    /// state, drops the completer so it re-derives, and places the
    /// cursor at `cursor` (or end-of-text if `None`).
    ///
    /// This is the canonical path for commands that stuff new text
    /// into the prompt (unqueue, resume restore, ghost accept). Direct
    /// `input.buf = …` writes skip these invariants and have been a
    /// recurring source of undo / completer / paste-state bugs.
    pub(crate) fn replace(
        input: &mut PromptState,
        text: String,
        cursor: Option<usize>,
        mode: ui::VimMode,
    ) {
        input.replace_text(text, cursor, mode);
    }
}

/// Command dispatch — the single entry point for `/cmd` and `:cmd`
/// style actions. Internal handlers and (future) plugin handlers both
/// register here; keybindings resolve to names that route through
/// `run`. Modelled on nvim's `nvim_command` / `user_command` split.
pub(crate) mod cmd {
    use crate::app::{CommandAction, TuiApp};

    /// Run a command line. Accepts `/name args...` or `:name args...`
    /// or a bare `name`. Parses the name, looks it up in the registry,
    /// and dispatches.
    ///
    /// The same code path runs whether the user typed the command,
    /// pressed a keybind that resolved to `Action::Cmd(name)`, or a
    /// plugin invoked it programmatically.
    pub(crate) fn run(app: &mut TuiApp, line: &str) -> CommandAction {
        crate::app::commands::run_command(app, line)
    }
}
