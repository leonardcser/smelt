//! Content-pane key dispatch: thin wrapper around the shared viewer-key
//! dispatcher with transcript-specific extras (block expand, Ctrl-C to leave
//! the pane, `q` to quit, cursor snapping past non-selectable cells).

use crate::app::{EventOutcome, TuiApp};
use crossterm::event::{Event, KeyCode, KeyEvent};

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

    /// Move the content-pane cursor by `delta` lines (cursor-led: viewport pans only
    /// when the cursor would leave it). Used by Ctrl-U/D, arrows, and j/k. Mouse
    /// wheel uses `pan_by_lines` instead — that path is viewport-led.
    pub(crate) fn move_content_cursor_by_lines(&mut self, delta: isize) {
        let viewport = self.viewport_rows_estimate();
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
        win.expect("transcript window").move_cursor_by_lines(
            buf.expect("transcript buffer"),
            delta,
            viewport,
        );
        self.snap_transcript_cursor();
    }

    /// Drive `key` through the transcript window's vim engine. Returns `true`
    /// when vim consumed the key (caller should return `Redraw`).
    ///
    /// Yank handling is shared with the prompt: vim writes only to the
    /// kill ring, then `Buffer::sync_clipboard_from_kill_ring` consults the
    /// transcript's `BufferCopy` impl and pushes rendered text to the system
    /// clipboard.
    fn handle_content_vim_key(&mut self, k: KeyEvent) -> bool {
        let viewport_rows = self.viewport_rows_estimate();
        // EventCtx carries a full `WindowViewport`, but key dispatch
        // reads only `viewport.rect.height`. Synthesise a minimal one
        // from the layout's row count — `viewport_rows_estimate()`
        // returns the layout-derived height even before the transcript
        // has painted, where `UiHost::viewport_for` would still be `None`.
        let viewport = crate::smelt_term::WindowViewport::new(
            crate::smelt_term::Rect::new(0, 0, 0, viewport_rows),
            0,
            0,
            0,
            None,
        );
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        let yank_tick_before = self.core.clipboard.kill_ring.yank_tick();
        let now = self.core.clock.instant_now();
        let status = {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let ctx = crate::smelt_term::EventCtx {
                soft_breaks: &[],
                hard_breaks: &[],
                viewport,
                click_count: 1,
                clipboard: &mut self.core.clipboard,
                now,
            };
            win.expect("transcript window").handle(
                buf.expect("transcript buffer"),
                crate::smelt_term::Event::Key(k),
                ctx,
            )
        };
        let total_rows = self
            .ui
            .buf(buf_id)
            .map(|b| b.lines().len() as u16)
            .unwrap_or(0);
        let max_scroll = total_rows.saturating_sub(viewport_rows);
        let win = self.transcript_win_mut();
        win.follow_tail = win.scroll_top >= max_scroll;
        if matches!(status, crate::smelt_term::Status::Ignored) {
            return false;
        }
        if self.core.clipboard.kill_ring.yank_tick() != yank_tick_before {
            if let Some(buf) = self.ui.buf(buf_id) {
                buf.sync_clipboard_from_kill_ring(&mut self.core.clipboard);
            }
        }
        self.snap_transcript_cursor();
        true
    }
}
