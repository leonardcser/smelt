//! Pane focus chord (`Ctrl-W` + nav) toggling focus between prompt and transcript.

use crate::app::{EventOutcome, TuiApp};
use crossterm::event::{Event, KeyCode};
use std::time::Duration;

/// Max inter-key gap between `Ctrl-W` and its follow-up key.
const PANE_CHORD_WINDOW: Duration = Duration::from_millis(750);

impl TuiApp {
    pub(crate) fn handle_pane_chord(&mut self, ev: &Event) -> Option<EventOutcome> {
        use crossterm::event::KeyModifiers as M;
        let Event::Key(k) = ev else { return None };

        let now = self.core.clock.instant_now();
        if let Some(started) = self.timers.pending_pane_chord {
            if now.duration_since(started) < PANE_CHORD_WINDOW {
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
            self.timers.pending_pane_chord = Some(now);
            return Some(EventOutcome::Noop);
        }
        None
    }

    pub(crate) fn toggle_pane_focus(&mut self) {
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
}
