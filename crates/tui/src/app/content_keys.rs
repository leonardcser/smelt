//! Content-pane key dispatch: vim/novim key handlers over the transcript buffer.

use crate::app::{EventOutcome, TuiApp};
use crossterm::event::{Event, KeyCode, KeyEvent};

impl TuiApp {
    pub(crate) fn handle_event_app_history(&mut self, ev: &Event) -> EventOutcome {
        let k = match ev {
            Event::Key(k) => *k,
            _ => return EventOutcome::Noop,
        };
        use crossterm::event::KeyModifiers as M;

        if k.modifiers.contains(M::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
            self.app_focus = crate::app::AppFocus::Prompt;
            return EventOutcome::Redraw;
        }

        if let Some(dn) =
            crate::smelt_term::vim::page_motion_delta(k, self.viewport_rows_estimate())
        {
            self.move_content_cursor_by_lines(dn);
            return EventOutcome::Redraw;
        }

        if k.modifiers.contains(M::SHIFT)
            && matches!(
                k.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
            )
        {
            return self.handle_content_novim_key(k);
        }
        if let Some(outcome) = self.dispatch_block_key(k) {
            return outcome;
        }

        if self.transcript_win().vim_enabled {
            if self.handle_content_vim_key(k) {
                return EventOutcome::Redraw;
            }
            match (k.code, k.modifiers) {
                (KeyCode::Char('q'), M::NONE) => EventOutcome::Quit,
                _ => EventOutcome::Noop,
            }
        } else {
            self.handle_content_novim_key(k)
        }
    }

    /// Content-pane key handler when vim is disabled. Drives the same
    /// selection mechanism as the prompt: shift+movement extends via
    /// `ShiftSelection`; plain movement clears it; Ctrl-C / ⌘C copies.
    fn handle_content_novim_key(&mut self, k: KeyEvent) -> EventOutcome {
        use crate::keymap::{lookup, KeyAction, KeyContext};
        use crossterm::event::KeyModifiers as M;
        // Pull in the latest nav-only text (selectable chars) so cpos
        // stays valid across streaming updates.

        let viewport = self.viewport_rows_estimate();
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        let buf_empty = self
            .ui
            .buf(buf_id)
            .map(|b| b.text().is_empty())
            .unwrap_or(true);
        {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            win.expect("transcript window")
                .resync(buf.expect("transcript buffer"), viewport);
        }
        let ctx = KeyContext {
            buf_empty,
            vim_non_insert: false,
            vim_enabled: false,
            agent_running: false,
            ghost_text_visible: false,
        };
        if let Some(action) = lookup(k.code, k.modifiers, &ctx) {
            let extending = matches!(
                action,
                KeyAction::SelectLeft
                    | KeyAction::SelectRight
                    | KeyAction::SelectUp
                    | KeyAction::SelectDown
                    | KeyAction::SelectWordForward
                    | KeyAction::SelectWordBackward
                    | KeyAction::SelectStartOfLine
                    | KeyAction::SelectEndOfLine
            );
            match action {
                KeyAction::MoveLeft
                | KeyAction::MoveRight
                | KeyAction::MoveUp
                | KeyAction::MoveDown
                | KeyAction::MoveStartOfLine
                | KeyAction::MoveEndOfLine
                | KeyAction::MoveWordForward
                | KeyAction::MoveWordBackward => {
                    self.transcript_win_mut().selection_anchor = None;
                }
                _ if extending => {
                    let cpos = self.transcript_win().cpos;
                    self.transcript_win_mut().extend_selection(cpos);
                }
                _ => {}
            }
            let delta: Option<isize> = match action {
                KeyAction::MoveUp | KeyAction::SelectUp => Some(-1),
                KeyAction::MoveDown | KeyAction::SelectDown => Some(1),
                _ => None,
            };
            if let Some(d) = delta {
                self.move_content_cursor_by_lines(d);
                return EventOutcome::Redraw;
            }
            let buf = self.ui.buf(buf_id).expect("transcript buffer");
            let text = buf.text().to_string();
            let cpos = self.transcript_win().cpos;
            let mv: Option<usize> = match action {
                KeyAction::MoveLeft | KeyAction::SelectLeft => {
                    Some(crate::smelt_term::text::prev_char_boundary(&text, cpos))
                }
                KeyAction::MoveRight | KeyAction::SelectRight => {
                    Some(crate::smelt_term::text::next_char_boundary(&text, cpos))
                }
                KeyAction::MoveStartOfLine | KeyAction::SelectStartOfLine => {
                    Some(crate::smelt_term::text::line_start(&text, cpos))
                }
                KeyAction::MoveEndOfLine | KeyAction::SelectEndOfLine => {
                    Some(crate::smelt_term::text::line_end(&text, cpos))
                }
                KeyAction::MoveWordForward | KeyAction::SelectWordForward => {
                    Some(crate::smelt_term::text::word_forward_pos(
                        &text,
                        cpos,
                        crate::smelt_term::text::CharClass::Word,
                    ))
                }
                KeyAction::MoveWordBackward | KeyAction::SelectWordBackward => {
                    Some(crate::smelt_term::text::word_backward_pos(
                        &text,
                        cpos,
                        crate::smelt_term::text::CharClass::Word,
                    ))
                }
                KeyAction::CopySelection => {
                    let range = self.transcript_win().selection_range(buf);
                    if let Some((s, e)) = range {
                        let s = crate::smelt_term::text::snap(&text, s);
                        let e = crate::smelt_term::text::snap(&text, e);
                        if s < e {
                            if let Some(buf) = self.ui.buf(buf_id) {
                                let out = buf.copy_range(s..e);
                                if !out.clipboard.is_empty() {
                                    let _ = self.core.clipboard.write(&out.clipboard);
                                }
                            }
                        }
                    }
                    return EventOutcome::Redraw;
                }
                _ => None,
            };
            drop(text);
            if let Some(new_cpos) = mv {
                self.transcript_win_mut().cpos = new_cpos;
                self.snap_transcript_cursor();

                let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
                win.expect("transcript window")
                    .resync(buf.expect("transcript buffer"), viewport);
                return EventOutcome::Redraw;
            }
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
        let status = {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let ctx = crate::smelt_term::EventCtx {
                soft_breaks: &[],
                hard_breaks: &[],
                viewport,
                click_count: 1,
                clipboard: &mut self.core.clipboard,
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
