//! Mouse event handling: wheel scrolling, drag-select, scrollbar drag, cell-click hit-testing.

use crate::app::{EventOutcome, TuiApp};
use crate::content::layout::HitRegion;
use crossterm::event::{MouseEvent, MouseEventKind};

impl TuiApp {
    // ── Mouse event dispatch ─────────────────────────────────────────────
    pub(crate) fn handle_mouse(&mut self, me: MouseEvent) -> EventOutcome {
        use crossterm::event::MouseButton;
        // `Ui::dispatch_event` handles wheel-over-overlay, modal click-outside absorb,
        // and scrollbar drag. Anything unclaimed (`Ignored`) flows through below.
        let cap_before = self.ui.capture();
        if matches!(
            self.ui
                .dispatch_event(crate::smelt_term::Event::Mouse(me), &mut |_, _, _| {}),
            crate::smelt_term::Status::Consumed
        ) {
            let scrollbar_owner = match (cap_before, self.ui.capture()) {
                (Some(crate::smelt_term::HitTarget::Scrollbar { owner }), _)
                | (_, Some(crate::smelt_term::HitTarget::Scrollbar { owner })) => Some(owner),
                _ => None,
            };
            if let Some(owner) = scrollbar_owner {
                self.propagate_scrollbar_scroll(owner);
                if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
                    if owner == crate::app::TRANSCRIPT_WIN {
                        self.app_focus = crate::app::AppFocus::Content;
                    } else if owner == crate::app::PROMPT_WIN {
                        self.app_focus = crate::app::AppFocus::Prompt;
                    }
                }
                return EventOutcome::Redraw;
            }
            let is_scroll = matches!(
                me.kind,
                MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight
            );
            return if is_scroll {
                EventOutcome::Redraw
            } else {
                EventOutcome::Noop
            };
        }

        if self.layout.hit_test(me.row, me.column) == HitRegion::Status {
            return EventOutcome::Noop;
        }

        match me.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_under_mouse(me.row, me.column, -3);
                return EventOutcome::Redraw;
            }
            MouseEventKind::ScrollDown => {
                self.scroll_under_mouse(me.row, me.column, 3);
                return EventOutcome::Redraw;
            }
            _ => {}
        }

        // `Ui::resolve_split_mouse` handles hit-test, click-count, and HitTarget capture
        // so drags stay on the originating window even if the pointer drifts.
        if let Some((win, count)) = self.ui.resolve_split_mouse(me) {
            let is_down = matches!(me.kind, MouseEventKind::Down(MouseButton::Left));
            let is_up = matches!(me.kind, MouseEventKind::Up(MouseButton::Left));
            if win == crate::app::PROMPT_WIN {
                if is_down {
                    self.app_focus = crate::app::AppFocus::Prompt;
                }
                self.handle_prompt_mouse(me, count);
            } else if win == crate::app::TRANSCRIPT_WIN {
                if is_down && !self.has_transcript_content(self.core.config.settings.show_thinking)
                {
                    return EventOutcome::Noop;
                }
                if is_down {
                    self.app_focus = crate::app::AppFocus::Content;
                }
                let yank = self.handle_content_mouse(me, count);
                if is_up {
                    if let Some(out) = yank {
                        self.yank_to_clipboard(out);
                    }
                }
            } else if self.ui.win(win).is_some_and(|w| w.selectable) {
                // Generic selectable leaf: notifications, dialog bodies, future popups.
                // Focus is left untouched — a non-focusable selectable leaf must not
                // steal app_focus; a focusable one was already focused by the overlay.
                let yank = self.handle_selectable_leaf_mouse(win, me, count);
                if is_up {
                    if let Some(out) = yank {
                        self.yank_to_clipboard(out);
                    }
                }
            }
            return EventOutcome::Redraw;
        }

        // Down on a non-window region in the prompt/status zone: promote focus to Prompt.
        if matches!(me.kind, MouseEventKind::Down(_))
            && matches!(
                self.layout.hit_test(me.row, me.column),
                HitRegion::Prompt | HitRegion::Status
            )
        {
            if self.app_focus != crate::app::AppFocus::Prompt {
                self.app_focus = crate::app::AppFocus::Prompt;
                return EventOutcome::Redraw;
            }
            return EventOutcome::Noop;
        }

        EventOutcome::Noop
    }

    fn yank_to_clipboard(&mut self, out: crate::smelt_term::CopyOutput) {
        if self.core.clipboard.write(&out.clipboard).is_ok() {
            self.core
                .clipboard
                .kill_ring
                .record_clipboard_write(out.clipboard);
        }
        self.core
            .clipboard
            .kill_ring
            .set_with_linewise(out.kill_ring, false);
    }

    /// Scroll the pane under the cursor by `delta` lines, tmux-style: the viewport
    /// pans and the cursor stays on the same screen row.
    /// Wheel does not change `app_focus` — only clicks promote focus.
    pub(crate) fn scroll_under_mouse(&mut self, row: u16, col: u16, delta: isize) {
        let on_prompt = matches!(
            self.ui.hit_test(row, col, None),
            Some(crate::smelt_term::HitTarget::Window(w)) if w == crate::app::PROMPT_WIN
        );
        if on_prompt {
            self.pan_prompt_by_lines(delta);
            return;
        }
        if !self.has_transcript_content(self.core.config.settings.show_thinking) {
            return;
        }
        let viewport = self.viewport_rows_estimate();
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
        win.expect("transcript window").pan_by_lines(
            buf.expect("transcript buffer"),
            delta,
            viewport,
        );
    }

    /// Pan the prompt's wrapped-row viewport by `delta`, keeping the cursor on the
    /// same screen row. `cpos` lives in source bytes; convert through the buffer's
    /// parser so the window pan operates on wrapped rows, then convert back.
    fn pan_prompt_by_lines(&mut self, delta: isize) {
        let Some(vp) = crate::smelt_term::UiHost::viewport_for(self, crate::app::PROMPT_WIN) else {
            return;
        };
        let (win, buf) = self
            .ui
            .win_and_buf_mut(crate::app::PROMPT_WIN, crate::app::PROMPT_EDIT_BUF);
        win.expect("prompt window").pan_by_lines(
            buf.expect("prompt edit buffer"),
            delta,
            vp.rect.height,
        );
    }

    /// Scroll one line when a drag cursor sits at the viewport edge (autoscroll).
    /// One line per tick; the main loop ramps sleep interval for acceleration.
    pub(crate) fn tick_drag_autoscroll(&mut self) {
        let Some((win, delta)) = self.ui.poll_drag_autoscroll() else {
            return;
        };
        if win == crate::app::TRANSCRIPT_WIN && self.app_focus == crate::app::AppFocus::Content {
            self.move_content_cursor_by_lines(delta);
        }
    }

    /// Drive a prompt mouse event through `Window::handle_mouse`. On `Up` with
    /// a non-empty selection, route the yanked range through `buf.copy_range`
    /// (the prompt's `BufferCopy` expands `\u{FFFC}` markers to `[label]` for
    /// the system clipboard while keeping raw markers in the kill ring for
    /// vim paste-back).
    fn handle_prompt_mouse(&mut self, me: MouseEvent, click_count: u8) {
        let Some(vp) = crate::smelt_term::UiHost::viewport_for(self, crate::app::PROMPT_WIN) else {
            return;
        };
        let usable = vp.content_width as usize;
        let yank = {
            let (win, buf) = self
                .ui
                .win_and_buf_mut(crate::app::PROMPT_WIN, crate::app::PROMPT_EDIT_BUF);
            let buf = buf.expect("prompt edit buffer");
            let win = win.expect("prompt window");
            buf.ensure_rendered_at(usable as u16);
            let mouse_ctx = crate::smelt_term::MouseCtx {
                soft_breaks: &[],
                hard_breaks: &[],
                viewport: vp,
                click_count,
            };
            let (_, range) = win.handle_mouse(buf, me, mouse_ctx);
            range.map(|(s, e)| buf.copy_range(s..e))
        };
        if let Some(out) = yank {
            if !out.is_empty() {
                self.yank_to_clipboard(out);
            }
        }
    }

    /// Generic selectable-leaf path used by notifications, dialog bodies, and any other
    /// leaf that sets `Window::selectable = true`. Skips the snap + word/line-break
    /// machinery the transcript needs — drag-select only, no double/triple-click
    /// word/line expansion. Returns the yanked range on `Up` (caller copies it).
    fn handle_selectable_leaf_mouse(
        &mut self,
        win: crate::smelt_term::WinId,
        me: MouseEvent,
        click_count: u8,
    ) -> Option<crate::smelt_term::CopyOutput> {
        let viewport = crate::smelt_term::UiHost::viewport_for(self, win)?;
        let buf_id = self.ui.win(win).map(|w| w.buf)?;
        let range = {
            let mouse_ctx = crate::smelt_term::MouseCtx {
                soft_breaks: &[],
                hard_breaks: &[],
                viewport,
                click_count,
            };
            let (win_mut, buf_mut) = self.ui.win_and_buf_mut(win, buf_id);
            let (_, range) = win_mut?.handle_mouse(buf_mut?, me, mouse_ctx);
            range?
        };
        let buf = self.ui.buf(buf_id)?;
        let out = buf.copy_range(range.0..range.1);
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// Drive a transcript-pane mouse event through `Window::handle_mouse`.
    /// Snaps the click column to a selectable cell (hidden-thinking rows route to fold markers).
    /// On `MouseUp`, returns the yanked range as `CopyOutput` via `Buffer::copy_range` —
    /// the transcript's `BufferCopy` impl walks the latest snapshot so `copy_as`
    /// substitutions, soft-wrap merging, and non-selectable cells are honored.
    fn handle_content_mouse(
        &mut self,
        me: MouseEvent,
        click_count: u8,
    ) -> Option<crate::smelt_term::CopyOutput> {
        let viewport = crate::smelt_term::UiHost::viewport_for(self, crate::app::TRANSCRIPT_WIN)?;
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        let rows: Vec<String> = self
            .ui
            .buf(buf_id)
            .map(|b| b.lines().to_vec())
            .unwrap_or_default();
        if rows.is_empty() {
            return None;
        }
        let snapped = self.snap_event_for_selection(me, &rows, viewport);

        // Breaks only matter for word/line selection and word/line-anchored drags;
        // skip the full-transcript walk otherwise.
        let needs_breaks = match me.kind {
            MouseEventKind::Down(_) => click_count >= 2,
            MouseEventKind::Drag(_) => {
                let w = self.transcript_win();
                w.drag_anchor_word.is_some() || w.drag_anchor_line.is_some()
            }
            _ => false,
        };
        let (soft, hard) = if needs_breaks {
            crate::smelt_term::UiHost::breaks_for(self, crate::app::TRANSCRIPT_WIN)
                .unwrap_or_default()
        } else {
            (Vec::new(), Vec::new())
        };

        let range = {
            let mouse_ctx = crate::smelt_term::MouseCtx {
                soft_breaks: &soft,
                hard_breaks: &hard,
                viewport,
                click_count,
            };
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let (_, range) = win
                .expect("transcript window")
                .handle_mouse(buf?, snapped, mouse_ctx);
            range?
        };
        let buf = self.ui.buf(buf_id)?;
        let out = buf.copy_range(range.0..range.1);
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// Translate `me`'s column to the nearest selectable cell for the clicked display row.
    fn snap_event_for_selection(
        &mut self,
        me: MouseEvent,
        rows: &[String],
        vp: crate::smelt_term::WindowViewport,
    ) -> MouseEvent {
        let rel_row = me.row.saturating_sub(vp.rect.top) as usize;
        let line_idx =
            (self.transcript_win().scroll_top as usize + rel_row).min(rows.len().saturating_sub(1));
        let rel_col = me.column.saturating_sub(vp.rect.left) as usize;
        let snapped =
            self.snap_col_to_selectable(line_idx, rel_col, self.core.config.settings.show_thinking);
        MouseEvent {
            column: vp.rect.left.saturating_add(snapped as u16),
            ..me
        }
    }

    /// Mirror `scroll_top` from `Ui::wins[owner]` onto the host pane state.
    /// Transcript scrollbar drags use the same viewport-led path as wheel scroll.
    fn propagate_scrollbar_scroll(&mut self, owner: crate::smelt_term::WinId) {
        let Some(scroll_top) = self.ui.win(owner).map(|w| w.scroll_top) else {
            return;
        };
        if owner == crate::app::TRANSCRIPT_WIN {
            let viewport = self.viewport_rows_estimate();
            let win_id = self.well_known.transcript;
            let buf_id = self.transcript_win().buf;
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            win.expect("transcript window")
                .scroll_to_preserving_cursor_screen_row(
                    scroll_top,
                    buf.expect("transcript buffer"),
                    viewport,
                );
        } else if owner == crate::app::PROMPT_WIN {
            let Some(vp) = crate::smelt_term::UiHost::viewport_for(self, crate::app::PROMPT_WIN)
            else {
                return;
            };
            let (win, buf) = self
                .ui
                .win_and_buf_mut(crate::app::PROMPT_WIN, crate::app::PROMPT_EDIT_BUF);
            let buf = buf.expect("prompt edit buffer");
            let win = win.expect("prompt window");
            buf.ensure_rendered_at(vp.content_width);
            win.scroll_to_preserving_cursor_screen_row(scroll_top, buf, vp.rect.height);
        }
    }
}
