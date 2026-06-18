//! Stable IDs for the permanent split-tree windows.

use crate::smelt_edit::{BufId, DocumentHandle, WinId};

/// Stable id used for Lua callback/keymap registration.
pub const PROMPT_WIN: WinId = WinId(0);

/// Stable id used for Lua callback/keymap registration.
pub const TRANSCRIPT_WIN: WinId = WinId(1);

/// Stable document handle for the transcript row document.
pub const TRANSCRIPT_DOCUMENT: DocumentHandle = DocumentHandle(1);

/// Editing buffer created before `Ui` exists; never allocated via `Ui::buf_create`.
/// The real display buffer is a separate `Buffer` managed by `Ui`.
pub const PROMPT_EDIT_BUF: BufId = BufId(0);
