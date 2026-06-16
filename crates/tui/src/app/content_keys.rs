//! Content-pane key dispatch: thin wrapper around the shared viewer-key
//! dispatcher with transcript-specific extras (Ctrl-C to leave the pane,
//! cursor snapping past non-selectable cells).

use crate::app::{EventOutcome, TuiApp};
use crate::content::transcript_buf::{FoldAction, FoldActivation};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;

const TRANSCRIPT_FOLD_CHORD_WINDOW: Duration = Duration::from_millis(750);

impl TuiApp {
    fn transcript_cursor_row(&self) -> Option<crate::smelt_edit::RowIndex> {
        let win = self.transcript_win();
        if let Some(pos) = win.row_cursor() {
            return Some(pos.row);
        }
        let buf = self.ui.buf(win.buf)?;
        let text = buf.text();
        let cpos = smelt_buffer::text::snap(&text, win.cpos());
        Some(
            smelt_buffer::text::slice(&text, 0..cpos)
                .bytes()
                .filter(|b| *b == b'\n')
                .count() as crate::smelt_edit::RowIndex,
        )
    }

    fn handle_transcript_fold_key(&mut self, k: KeyEvent) -> Option<EventOutcome> {
        if k.modifiers != KeyModifiers::NONE {
            self.timers.pending_transcript_fold_chord = None;
            return None;
        }
        let now = self.core.clock.instant_now();
        let pending = self
            .timers
            .pending_transcript_fold_chord
            .take()
            .is_some_and(|started| now.duration_since(started) < TRANSCRIPT_FOLD_CHORD_WINDOW);
        if pending {
            let action = match k.code {
                KeyCode::Char('a') => Some(FoldAction::Toggle),
                KeyCode::Char('o') => Some(FoldAction::Open),
                KeyCode::Char('c') => Some(FoldAction::Close),
                KeyCode::Char('R') => {
                    return Some(if self.fold_all_transcript_nodes(FoldAction::Open) {
                        EventOutcome::Redraw
                    } else {
                        EventOutcome::Noop
                    });
                }
                KeyCode::Char('M') => {
                    return Some(if self.fold_all_transcript_nodes(FoldAction::Close) {
                        EventOutcome::Redraw
                    } else {
                        EventOutcome::Noop
                    });
                }
                _ => None,
            }?;
            let Some(row) = self.transcript_cursor_row() else {
                return Some(EventOutcome::Noop);
            };
            return Some(
                if self.fold_transcript_node_at_row(row, action, FoldActivation::AnyNodeRow) {
                    EventOutcome::Redraw
                } else {
                    EventOutcome::Noop
                },
            );
        }

        match k.code {
            KeyCode::Char('z') => {
                self.timers.pending_transcript_fold_chord = Some(now);
                Some(EventOutcome::Noop)
            }
            KeyCode::Enter => {
                let row = self.transcript_cursor_row()?;
                Some(
                    if self.fold_transcript_node_at_row(
                        row,
                        FoldAction::Toggle,
                        FoldActivation::AnyNodeRow,
                    ) {
                        EventOutcome::Redraw
                    } else {
                        EventOutcome::Noop
                    },
                )
            }
            _ => None,
        }
    }

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
        if self.try_open_search_for_key(k) {
            return EventOutcome::Noop;
        }
        if let Some(outcome) = self.handle_transcript_fold_key(k) {
            return outcome;
        }
        let status = self.dispatch_window_viewer_key(win_id, k);
        if matches!(status, crate::smelt_edit::Status::Consumed) {
            self.snap_transcript_cursor();
            return EventOutcome::Redraw;
        }
        EventOutcome::Noop
    }
}
