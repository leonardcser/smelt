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

        if k.modifiers.contains(M::CONTROL) {
            let half = (self.viewport_rows_estimate() / 2).max(1) as isize;
            let full = (self.viewport_rows_estimate() as isize).max(1);
            let delta: Option<isize> = match k.code {
                KeyCode::Char('u') => Some(-half),
                KeyCode::Char('d') => Some(half),
                KeyCode::Char('b') => Some(-full),
                KeyCode::Char('f') => Some(full),
                KeyCode::Char('y') => Some(-1),
                KeyCode::Char('e') => Some(1),
                _ => None,
            };
            if let Some(dn) = delta {
                self.move_content_cursor_by_lines(dn);
                return EventOutcome::Redraw;
            }
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

        if self.transcript_window.vim_enabled {
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

        let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window.resync(&rows, viewport);
        let ctx = KeyContext {
            buf_empty: self.transcript_window.text.is_empty(),
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
                    self.transcript_window.selection_anchor = None;
                }
                _ if extending => {
                    let cpos = self.transcript_window.cpos;
                    self.transcript_window.extend_selection(cpos);
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
            let buf = self.transcript_window.text.clone();
            let mv: Option<usize> = match action {
                KeyAction::MoveLeft | KeyAction::SelectLeft => Some(
                    crate::smelt_term::text::prev_char_boundary(&buf, self.transcript_window.cpos),
                ),
                KeyAction::MoveRight | KeyAction::SelectRight => Some(
                    crate::smelt_term::text::next_char_boundary(&buf, self.transcript_window.cpos),
                ),
                KeyAction::MoveStartOfLine | KeyAction::SelectStartOfLine => Some(
                    crate::smelt_term::text::line_start(&buf, self.transcript_window.cpos),
                ),
                KeyAction::MoveEndOfLine | KeyAction::SelectEndOfLine => Some(
                    crate::smelt_term::text::line_end(&buf, self.transcript_window.cpos),
                ),
                KeyAction::MoveWordForward | KeyAction::SelectWordForward => {
                    Some(crate::smelt_term::text::word_forward_pos(
                        &buf,
                        self.transcript_window.cpos,
                        crate::smelt_term::text::CharClass::Word,
                    ))
                }
                KeyAction::MoveWordBackward | KeyAction::SelectWordBackward => {
                    Some(crate::smelt_term::text::word_backward_pos(
                        &buf,
                        self.transcript_window.cpos,
                        crate::smelt_term::text::CharClass::Word,
                    ))
                }
                KeyAction::CopySelection => {
                    if let Some((s, e)) = self.transcript_window.selection_range(&rows) {
                        let s = crate::smelt_term::text::snap(&buf, s);
                        let e = crate::smelt_term::text::snap(&buf, e);
                        if s < e {
                            let copy = self.copy_display_range(
                                s,
                                e,
                                self.core.config.settings.show_thinking,
                            );
                            let _ = self.core.clipboard.write(&copy);
                        }
                    }
                    return EventOutcome::Redraw;
                }
                _ => None,
            };
            if let Some(new_cpos) = mv {
                self.transcript_window.cpos = new_cpos;
                self.snap_transcript_cursor();

                let rows =
                    self.full_transcript_display_text(self.core.config.settings.show_thinking);
                let viewport = self.viewport_rows_estimate();
                self.transcript_window.resync(&rows, viewport);
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
        let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window
            .move_cursor_by_lines(delta, &rows, viewport);
        self.snap_transcript_cursor();
    }

    /// Build the transcript buffer, run `key` through the content-pane
    /// `Vim` instance, and mirror the resulting cursor / visual / yank
    /// state back onto our scroll + cursor. Returns `true` when vim
    /// consumed the key (caller should return `Redraw`).
    ///
    /// The transcript yank path mutes the platform sink during vim
    /// dispatch (via `Clipboard::swap_sink`) so vim's `yank_range`
    /// captures the *raw* source range into the kill ring without
    /// pushing the raw markdown to the system clipboard. After vim
    /// returns we look up the source range, build the *rendered* copy
    /// via `copy_display_range`, and push that — so external pastes
    /// see the rendered text rather than the raw markdown.
    fn handle_content_vim_key(&mut self, k: KeyEvent) -> bool {
        let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
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
        let status = {
            let prev_sink = self
                .core
                .clipboard
                .swap_sink(Box::new(smelt_core::NullSink));
            let ctx = crate::smelt_term::EventCtx {
                rows: &rows,
                soft_breaks: &[],
                hard_breaks: &[],
                viewport,
                click_count: 1,
                clipboard: &mut self.core.clipboard,
            };
            let r = self
                .transcript_window
                .handle(crate::smelt_term::Event::Key(k), ctx);
            self.core.clipboard.swap_sink(prev_sink);
            r
        };
        if matches!(status, crate::smelt_term::Status::Ignored) {
            return false;
        }
        let raw = self.core.clipboard.kill_ring.current().to_string();
        if !raw.is_empty() {
            let copy = if let Some((s, e)) = self.core.clipboard.kill_ring.source_range() {
                self.copy_display_range(s, e, self.core.config.settings.show_thinking)
            } else {
                raw
            };
            self.core
                .clipboard
                .kill_ring
                .set_with_linewise(String::new(), false);
            if self.core.clipboard.write(&copy).is_ok() {
                self.core.clipboard.kill_ring.record_clipboard_write(copy);
            }
        }
        self.snap_transcript_cursor();
        true
    }
}
