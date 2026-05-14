//! Pane focus and block-scoped key dispatch.

use crate::app::{EventOutcome, TuiApp};
use crossterm::event::{Event, KeyCode, KeyEvent};
use smelt_core::{Block, BlockId, ViewState};
use std::time::{Duration, Instant};

/// Max inter-key gap between `Ctrl-W` and its follow-up key.
const PANE_CHORD_WINDOW: Duration = Duration::from_millis(750);

impl TuiApp {
    pub(crate) fn handle_pane_chord(&mut self, ev: &Event) -> Option<EventOutcome> {
        use crossterm::event::KeyModifiers as M;
        let Event::Key(k) = ev else { return None };

        if let Some(started) = self.timers.pending_pane_chord {
            if started.elapsed() < PANE_CHORD_WINDOW {
                let navigated = matches!(
                    (k.code, k.modifiers),
                    (KeyCode::Char('w'), _) | (KeyCode::Char('j' | 'k' | 'h' | 'l' | 'p'), M::NONE)
                );
                self.timers.pending_pane_chord = None;
                if navigated {
                    self.toggle_pane_focus();
                    return Some(EventOutcome::Redraw);
                }
                return None;
            }
            self.timers.pending_pane_chord = None;
        }

        if k.code == KeyCode::Char('w') && k.modifiers.contains(M::CONTROL) {
            self.timers.pending_pane_chord = Some(Instant::now());
            return Some(EventOutcome::Noop);
        }
        None
    }

    fn toggle_pane_focus(&mut self) {
        let target = match self.app_focus {
            crate::app::AppFocus::Prompt => crate::app::AppFocus::Content,
            crate::app::AppFocus::Content => crate::app::AppFocus::Prompt,
        };
        if target == crate::app::AppFocus::Content
            && !self.has_transcript_content(self.core.config.settings.show_thinking)
        {
            return;
        }
        self.app_focus = target;
        if self.app_focus == crate::app::AppFocus::Content {
            self.refocus_content();
        }
    }

    /// Warm up the content pane on focus switch: clamp cpos and sync cursor state.
    /// Without this, a resumed session has stale state and the first key is a no-op.
    fn refocus_content(&mut self) {
        let viewport = self.viewport_rows_estimate();
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
        win.expect("transcript window")
            .refocus(buf.expect("transcript buffer"), viewport);
        self.snap_transcript_cursor();
    }

    fn focused_block_id(&mut self) -> Option<BlockId> {
        let row = self.transcript_win().cursor_abs_row();
        self.transcript_projection.block_of_row(row)
    }

    /// Handle a key as a block-scoped binding. Returns `Some` if consumed, `None` to fall through.
    pub(crate) fn dispatch_block_key(&mut self, k: KeyEvent) -> Option<EventOutcome> {
        use crossterm::event::KeyModifiers as M;
        if k.modifiers != M::NONE {
            return None;
        }
        let block_id = self.focused_block_id()?;
        let is_tool = matches!(
            self.transcript.block(block_id),
            Some(Block::ToolCall { .. })
        );
        if !is_tool {
            return None;
        }
        match k.code {
            KeyCode::Char('e') => {
                let vs = self.block_view_state(block_id);
                let next = match vs {
                    ViewState::Expanded => ViewState::Collapsed,
                    _ => ViewState::Expanded,
                };
                self.set_block_view_state(block_id, next);
                Some(EventOutcome::Redraw)
            }
            _ => None,
        }
    }
}
