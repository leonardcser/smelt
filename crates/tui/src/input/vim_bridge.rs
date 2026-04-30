//! Bridge between `PromptState` and the vim state machine.
//!
//! Vim borrows the input's live `buf`/`cpos`/`attachment_ids` plus the
//! `UndoHistory` owned by `PromptState`, the **single global** `VimMode`
//! owned by `TuiApp`, the **single global** `Clipboard` (kill ring + platform
//! sink) also owned by `TuiApp`, and the per-Window `curswant` +
//! `VimWindowState` (Visual anchor, last `f`/`t`) carried on
//! `ui::Window`. Vim itself holds only in-flight key-sequence state.

use super::{Action, History, PromptState};
use crossterm::event::{Event, KeyEvent};
use ui::vim::{self, VimContext};
use ui::{Clipboard, VimMode};

/// Outcome of the vim bridge for a single key event.
pub(super) enum VimBridgeResult {
    /// Vim consumed the key (possibly generating an Action for the caller).
    Handled(Action),
    /// Vim passed the key through; caller should continue to keymap lookup.
    Passthrough,
    /// Not a key event or vim disabled — caller handles as paste/resize/etc.
    NotAKey,
}

impl PromptState {
    pub(super) fn dispatch_vim(
        &mut self,
        ev: &Event,
        history: &mut Option<&mut History>,
        mode: &mut VimMode,
        clipboard: &mut Clipboard,
    ) -> VimBridgeResult {
        if !self.win.vim_enabled {
            return VimBridgeResult::NotAKey;
        }
        let Event::Key(key_ev) = ev else {
            return VimBridgeResult::NotAKey;
        };
        let key_ev: KeyEvent = *key_ev;

        let result = {
            let mut ctx = VimContext {
                buf: &mut self.win.edit_buf.buf,
                cpos: &mut self.win.cpos,
                attachments: &mut self.win.edit_buf.attachment_ids,
                history: &mut self.win.edit_buf.history,
                clipboard,
                mode,
                curswant: &mut self.win.curswant,
                vim_state: &mut self.win.vim_state,
            };
            vim::handle_key(key_ev, &mut ctx)
        };

        match result {
            vim::Action::Consumed => {
                // Clear shift+key selection on any vim-consumed key
                // (e.g. Esc in insert mode, Esc in visual mode).
                self.clear_selection();
                self.recompute_completer();
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::Submit => {
                if self.win.edit_buf.buf.is_empty() && self.win.edit_buf.attachment_ids.is_empty() {
                    VimBridgeResult::Handled(Action::SubmitEmpty)
                } else {
                    let display = self.message_display_text();
                    let content = self.build_content();
                    self.clear();
                    VimBridgeResult::Handled(Action::Submit { content, display })
                }
            }
            vim::Action::HistoryPrev => {
                if let Some(entry) = history
                    .as_deref_mut()
                    .and_then(|h| h.up(&self.win.edit_buf.buf))
                {
                    self.win.edit_buf.buf = entry.to_string();
                    self.win.cpos = 0;
                    self.sync_completer();
                }
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::HistoryNext => {
                if let Some(entry) = history.as_deref_mut().and_then(|h| h.down()) {
                    self.win.edit_buf.buf = entry.to_string();
                    self.win.cpos = self.win.edit_buf.buf.len();
                    self.sync_completer();
                }
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::EditInEditor => VimBridgeResult::Handled(Action::EditInEditor),
            vim::Action::CenterScroll => VimBridgeResult::Handled(Action::CenterScroll),
            vim::Action::Passthrough => VimBridgeResult::Passthrough,
        }
    }
}
