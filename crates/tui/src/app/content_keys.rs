//! Content-pane key dispatch: thin wrapper around the shared viewer-key
//! dispatcher with transcript-specific extras (block expand, Ctrl-C to leave
//! the pane, `q` to quit, cursor snapping past non-selectable cells).

use crate::app::{EventOutcome, TuiApp};
use crossterm::event::{Event, KeyCode};

impl TuiApp {
    pub(crate) fn handle_event_app_history(&mut self, ev: &Event) -> EventOutcome {
        let k = match ev {
            Event::Key(k) => *k,
            _ => return EventOutcome::Noop,
        };
        use crossterm::event::KeyModifiers as M;

        // Ctrl-C with no selection leaves the content pane back to the prompt.
        // This is transcript-only — the same chord on an overlay viewer is
        // claimed by the modal-dismiss tier instead.
        if k.modifiers.contains(M::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
            self.app_focus = crate::app::AppFocus::Prompt;
            return EventOutcome::Redraw;
        }

        // Block-level chord (e.g. `e` to expand a tool block). Runs before the
        // shared dispatcher so it wins over vim's `e` (end-of-word).
        if let Some(outcome) = self.dispatch_block_key(k) {
            return outcome;
        }

        let win_id = self.well_known.transcript;
        let yank_tick_before = self.core.clipboard.kill_ring.yank_tick();
        let status = self.dispatch_window_viewer_key(win_id, k);
        if matches!(status, crate::smelt_term::Status::Consumed) {
            // Vim yanks land in the kill ring; mirror them to the system
            // clipboard through the transcript's `BufferCopy` so soft-wrap
            // joins + `copy_as` substitutions are honoured.
            if self.core.clipboard.kill_ring.yank_tick() != yank_tick_before {
                let buf_id = self.transcript_win().buf;
                if let Some(buf) = self.ui.buf(buf_id) {
                    buf.sync_clipboard_from_kill_ring(&mut self.core.clipboard);
                }
            }
            self.snap_transcript_cursor();
            return EventOutcome::Redraw;
        }

        match (k.code, k.modifiers) {
            (KeyCode::Char('q'), M::NONE) => EventOutcome::Quit,
            _ => EventOutcome::Noop,
        }
    }
}
