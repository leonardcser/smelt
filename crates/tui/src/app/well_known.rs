//! Stable IDs for the permanent split-tree windows.

use crate::smelt_term::{BufId, WinId};

/// Stable id used for Lua callback/keymap registration.
pub const PROMPT_WIN: WinId = WinId(0);

/// Stable id used for Lua callback/keymap registration.
pub const TRANSCRIPT_WIN: WinId = WinId(1);

/// Read-only chrome leaf above the input (queued + stash + top bar).
pub const PROMPT_ABOVE_WIN: WinId = WinId(2);

/// Read-only chrome leaf below the input (bottom bar).
pub const PROMPT_BELOW_WIN: WinId = WinId(3);

/// Editing buffer created before `Ui` exists; never allocated via `Ui::buf_create`.
/// The real display buffer is a separate `Buffer` managed by `Ui`.
pub const PROMPT_EDIT_BUF: BufId = BufId(0);
