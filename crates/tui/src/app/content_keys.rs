//! Content-pane key dispatch: the keys the `transcript` keymap scope leaves
//! over go to the shared viewer-key dispatcher, after the `z` fold chords,
//! with cursor snapping past non-selectable cells.

use crate::app::{EventOutcome, TuiApp};
use crate::keymap::Fold;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use smelt_view::transcript_buf::{FoldAction, FoldActivation};
use std::time::Duration;

const TRANSCRIPT_FOLD_CHORD_WINDOW: Duration = Duration::from_millis(750);

/// Keys that complete a `z` fold chord.
fn is_fold_key(code: KeyCode) -> bool {
    matches!(code, KeyCode::Char('a' | 'o' | 'c' | 'R' | 'M'))
}

impl TuiApp {
    fn transcript_cursor_row(&self) -> Option<crate::smelt_edit::RowIndex> {
        let win = self.transcript_win();
        if let Some(pos) = win.row_cursor() {
            return Some(pos.row);
        }
        let buf = self.ui.buf(win.buf)?;
        let text = buf.text();
        let cpos = smelt_buffer::text::snap_grapheme(&text, win.cpos());
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
        let started = self.timers.pending_transcript_fold_chord.take();
        let pending = started
            .is_some_and(|started| now.duration_since(started) < TRANSCRIPT_FOLD_CHORD_WINDOW);
        if started.is_some() && !(pending && is_fold_key(k.code)) {
            // The `z` starts a vim command (`zt`, `zz`, `zb`, ...), not a fold chord.
            let z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
            self.dispatch_window_viewer_key(self.well_known.transcript, z);
            return None;
        }
        if pending {
            let fold = match k.code {
                KeyCode::Char('a') => Fold::Toggle,
                KeyCode::Char('o') => Fold::Open,
                KeyCode::Char('c') => Fold::Close,
                KeyCode::Char('R') => Fold::OpenAll,
                KeyCode::Char('M') => Fold::CloseAll,
                _ => return None,
            };
            return Some(self.fold_transcript(fold));
        }

        if k.code == KeyCode::Char('z') {
            self.timers.pending_transcript_fold_chord = Some(now);
            return Some(EventOutcome::Noop);
        }
        None
    }

    /// Fold or unfold transcript blocks: the one under the cursor, or all.
    pub(crate) fn fold_transcript(&mut self, fold: Fold) -> EventOutcome {
        let changed = match fold {
            Fold::OpenAll => self.fold_all_transcript_nodes(FoldAction::Open),
            Fold::CloseAll => self.fold_all_transcript_nodes(FoldAction::Close),
            Fold::Toggle | Fold::Open | Fold::Close => {
                let action = match fold {
                    Fold::Open => FoldAction::Open,
                    Fold::Close => FoldAction::Close,
                    _ => FoldAction::Toggle,
                };
                self.transcript_cursor_row().is_some_and(|row| {
                    self.fold_transcript_node_at_row(row, action, FoldActivation::AnyNodeRow)
                })
            }
        };
        if changed {
            EventOutcome::Redraw
        } else {
            EventOutcome::Noop
        }
    }

    pub(crate) fn handle_event_app_history(&mut self, ev: &Event) -> EventOutcome {
        let k = match ev {
            Event::Key(k) => *k,
            _ => return EventOutcome::Noop,
        };
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
