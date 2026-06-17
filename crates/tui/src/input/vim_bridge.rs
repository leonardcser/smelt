//! Bridge between `PromptState` and the vim state machine.
//!
//! Vim borrows the input's live `buf`/`cpos`/`attachment_ids` plus the
//! `UndoHistory` owned by `PromptState`, the prompt `Window`'s per-window
//! `vim_mode`, `curswant`, and `VimWindowState` (Visual anchor, last `f`/`t`,
//! pending operator, count accumulators), and the single global `Clipboard`
//! (kill ring + platform sink) owned by `TuiApp`. Vim itself holds no
//! cross-call state - it's a pure function of the borrowed context.

use super::{Action, History, PromptCtx, PromptState};
use crate::smelt_edit::vim;
use crate::smelt_edit::Clipboard;
use crossterm::event::{Event, KeyEvent};

/// Outcome of the vim bridge for a single key event.
pub(super) enum VimBridgeResult {
    /// Vim consumed the key (possibly generating an Action for the caller).
    Handled(Action),
    /// Vim passed the key through; caller should continue to keymap lookup.
    Passthrough,
    /// Not a key event or vim disabled - caller handles as paste/resize/etc.
    NotAKey,
}

impl PromptState {
    pub(super) fn dispatch_vim(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        ev: &Event,
        history: &mut Option<&mut History>,
        clipboard: &mut Clipboard,
        now: std::time::Instant,
    ) -> VimBridgeResult {
        if !ctx.win.vim_enabled() {
            return VimBridgeResult::NotAKey;
        }
        let Event::Key(key_ev) = ev else {
            return VimBridgeResult::NotAKey;
        };
        let key_ev: KeyEvent = *key_ev;

        let yank_tick_before = clipboard.kill_ring.yank_tick();
        let result = ctx.win.handle_vim_key(ctx.buf, clipboard, key_ev, now);
        if clipboard.kill_ring.yank_tick() != yank_tick_before {
            ctx.buf.sync_clipboard_from_kill_ring(clipboard);
        }

        match result {
            vim::Action::Consumed => {
                // Clear shift+key selection on any vim-consumed key
                // (e.g. Esc in insert mode, Esc in visual mode).
                self.clear_selection(ctx.win);
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::Submit => VimBridgeResult::Handled(self.submit_action(ctx, false)),
            vim::Action::HistoryPrev => {
                if let Some(entry) = history
                    .as_deref_mut()
                    .and_then(|h| h.up(&Self::strip_attachment_markers(ctx.buf.source())))
                {
                    let entry = entry.to_string();
                    self.install_history_entry(ctx, &entry, false);
                }
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::HistoryNext => {
                if let Some(entry) = history.as_deref_mut().and_then(|h| h.down()) {
                    let entry = entry.to_string();
                    self.install_history_entry(ctx, &entry, true);
                }
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::CenterScroll => VimBridgeResult::Handled(Action::CenterScroll),
            vim::Action::PanColumns(d) => VimBridgeResult::Handled(Action::PanColumns(d)),
            vim::Action::Passthrough => VimBridgeResult::Passthrough,
        }
    }
}
