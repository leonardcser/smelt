//! Well-known stable IDs for the split-tree windows that smelt always
//! carries.  These live in `app` (not `ui`) because they are application
//! semantics, not generic UI primitives.

use crate::ui::{BufId, WinId};

/// Reserved [`WinId`] for the main prompt input window.  Stable id so Lua
/// can `smelt.win.on_event(prompt, …)` and `smelt.win.set_keymap(prompt, …)`
/// like any other window.
pub const PROMPT_WIN: WinId = WinId(0);

/// Reserved [`WinId`] for the transcript (scroll-back) window.  Same
/// rationale as [`PROMPT_WIN`] — stable id for callback registration.
pub const TRANSCRIPT_WIN: WinId = WinId(1);

/// Conceptual [`BufId`] for the prompt editing buffer.  The editing buffer
/// is created before `Ui` exists (inside [`PromptState::new`](crate::input::PromptState)),
/// so it is never allocated via `Ui::buf_create`.  Naming the magic number
/// makes the duality explicit; the real display buffer is a separate
/// `Buffer` managed by `Ui`.
pub const PROMPT_EDIT_BUF: BufId = BufId(0);
