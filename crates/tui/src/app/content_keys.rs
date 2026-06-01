//! Content-pane key dispatch: thin wrapper around the shared viewer-key
//! dispatcher with transcript-specific extras (Ctrl-C to leave the pane,
//! cursor snapping past non-selectable cells).

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
        // This is transcript-only - the same chord on an overlay viewer is
        // claimed by the modal-dismiss tier instead.
        if k.modifiers.contains(M::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
            self.app_focus = crate::app::AppFocus::Prompt;
            return EventOutcome::Redraw;
        }

        let win_id = self.well_known.transcript;
        let status = self.dispatch_window_viewer_key(win_id, k);
        if matches!(status, crate::smelt_term::Status::Consumed) {
            self.snap_transcript_cursor();
            return EventOutcome::Redraw;
        }
        EventOutcome::Noop
    }
}
