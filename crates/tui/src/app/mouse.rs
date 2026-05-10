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
                    if let Some(text) = yank {
                        self.yank_to_clipboard(text);
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

    fn yank_to_clipboard(&mut self, text: String) {
        if self.core.clipboard.write(&text).is_ok() {
            self.core
                .clipboard
                .kill_ring
                .record_clipboard_write(text.clone());
        }
        self.core.clipboard.kill_ring.set_with_linewise(text, false);
    }

    /// Scroll the pane under the cursor by `delta` lines. Prompt: moves `cpos`; transcript: scrolls buffer.
    pub(crate) fn scroll_under_mouse(&mut self, row: u16, col: u16, delta: isize) {
        let on_prompt = matches!(
            self.ui.hit_test(row, col, None),
            Some(crate::smelt_term::HitTarget::Window(w)) if w == crate::app::PROMPT_WIN
        );
        if on_prompt {
            self.app_focus = crate::app::AppFocus::Prompt;
            let (new_pos, new_want) = crate::smelt_term::text::vertical_move(
                &self.input.source,
                self.input.win.cpos,
                delta,
                self.input.win.curswant,
            );
            self.input.win.curswant = Some(new_want);
            if new_pos != self.input.win.cpos {
                self.input.win.cpos = new_pos;
            }
            return;
        }
        if !self.has_transcript_content(self.core.config.settings.show_thinking) {
            return;
        }
        self.app_focus = crate::app::AppFocus::Content;
        let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window
            .scroll_by_lines(delta, &rows, viewport);
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

    /// Drive a prompt mouse event through `Window::handle_mouse`.
    /// Translates source-byte state to wrapped-row space before the call and back after,
    /// since the prompt's source buffer differs from wrapped display rows.
    fn handle_prompt_mouse(&mut self, me: MouseEvent, click_count: u8) {
        let Some(vp) = crate::smelt_term::UiHost::viewport_for(self, crate::app::PROMPT_WIN) else {
            return;
        };
        let usable = vp.content_width as usize;
        let wrap = crate::content::prompt_wrap::PromptWrap::build(&self.input, usable);
        if wrap.rows.is_empty() {
            return;
        }

        let saved_src_cpos = self.input.win.cpos;
        let saved_src_anchor = self.input.win.selection_anchor;
        let saved_src_dword = self.input.win.drag_anchor_word;
        let saved_src_dline = self.input.win.drag_anchor_line;
        let saved_vim_visual_anchor = self
            .input
            .win
            .vim_enabled
            .then(|| {
                crate::smelt_term::vim::visual_anchor(
                    &self.input.win.vim_state,
                    self.input.win.vim_mode,
                )
            })
            .flatten();

        self.input.win.cpos = wrap.src_to_wrapped(saved_src_cpos);
        self.input.win.selection_anchor = saved_src_anchor.map(|a| wrap.src_to_wrapped(a));
        self.input.win.drag_anchor_word =
            saved_src_dword.map(|(s, e)| (wrap.src_to_wrapped(s), wrap.src_to_wrapped(e)));
        self.input.win.drag_anchor_line =
            saved_src_dline.map(|(s, e)| (wrap.src_to_wrapped(s), wrap.src_to_wrapped(e)));
        if self.input.win.vim_enabled {
            if let Some(a) = saved_vim_visual_anchor {
                self.input
                    .win
                    .begin_visual(crate::smelt_term::VimMode::Visual, wrap.src_to_wrapped(a));
            }
        }

        let mouse_ctx = crate::smelt_term::MouseCtx {
            rows: &wrap.rows,
            soft_breaks: &wrap.soft_breaks,
            hard_breaks: &wrap.hard_breaks,
            viewport: vp,
            click_count,
        };
        let (_, _yank) = self.input.win.handle_mouse(me, mouse_ctx);

        // Translate back to source bytes (`Window::mouse_up` already cleared anchors on Up).
        let new_w_cpos = self.input.win.cpos;
        let new_w_anchor = self.input.win.selection_anchor;
        let new_w_dword = self.input.win.drag_anchor_word;
        let new_w_dline = self.input.win.drag_anchor_line;
        let new_w_vim_anchor = self
            .input
            .win
            .vim_enabled
            .then(|| {
                crate::smelt_term::vim::visual_anchor(
                    &self.input.win.vim_state,
                    self.input.win.vim_mode,
                )
            })
            .flatten();

        self.input.win.cpos = wrap.wrapped_to_src(new_w_cpos);
        self.input.win.selection_anchor = new_w_anchor.map(|a| wrap.wrapped_to_src(a));
        self.input.win.drag_anchor_word =
            new_w_dword.map(|(s, e)| (wrap.wrapped_to_src(s), wrap.wrapped_to_src(e)));
        self.input.win.drag_anchor_line =
            new_w_dline.map(|(s, e)| (wrap.wrapped_to_src(s), wrap.wrapped_to_src(e)));
        if self.input.win.vim_enabled {
            if let Some(a) = new_w_vim_anchor {
                self.input
                    .win
                    .begin_visual(crate::smelt_term::VimMode::Visual, wrap.wrapped_to_src(a));
            }
        }
    }

    /// Drive a transcript-pane mouse event through `Window::handle_mouse`.
    /// Snaps the click column to a selectable cell (hidden-thinking rows route to fold markers).
    /// On `MouseUp`, returns yanked text via `TranscriptSnapshot::copy_byte_range` so
    /// `copy_as` substitutions and non-selectable cells are honored.
    fn handle_content_mouse(&mut self, me: MouseEvent, click_count: u8) -> Option<String> {
        let rows = crate::smelt_term::UiHost::rows_for(self, crate::app::TRANSCRIPT_WIN)?;
        if rows.is_empty() {
            return None;
        }
        let (soft, hard) = crate::smelt_term::UiHost::breaks_for(self, crate::app::TRANSCRIPT_WIN)?;
        let viewport = crate::smelt_term::UiHost::viewport_for(self, crate::app::TRANSCRIPT_WIN)?;
        let snapped = self.snap_event_for_selection(me, &rows, viewport);
        let range = {
            let mouse_ctx = crate::smelt_term::MouseCtx {
                rows: &rows,
                soft_breaks: &soft,
                hard_breaks: &hard,
                viewport,
                click_count,
            };
            let (_, range) = self.transcript_window.handle_mouse(snapped, mouse_ctx);
            range?
        };
        let (start, end) = range;
        let theme = self.ui.theme().clone();
        let width = self.transcript_width() as u16;
        let show_thinking = self.core.config.settings.show_thinking;
        let snap = self.transcript_projection.snapshot(
            &mut self.transcript.history,
            width,
            show_thinking,
            &theme,
        );
        let text = snap.copy_byte_range(start, end);
        if text.is_empty() {
            None
        } else {
            Some(text)
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
        let line_idx = (self.transcript_window.scroll_top as usize + rel_row)
            .min(rows.len().saturating_sub(1));
        let rel_col = me.column.saturating_sub(vp.rect.left) as usize;
        let snapped =
            self.snap_col_to_selectable(line_idx, rel_col, self.core.config.settings.show_thinking);
        MouseEvent {
            column: vp.rect.left.saturating_add(snapped as u16),
            ..me
        }
    }

    /// Mirror `scroll_top` from `Ui::wins[owner]` onto the host pane state.
    /// For the transcript, also recomputes `follow_tail` and re-anchors the cursor.
    fn propagate_scrollbar_scroll(&mut self, owner: crate::smelt_term::WinId) {
        let Some(scroll_top) = self.ui.win(owner).map(|w| w.scroll_top) else {
            return;
        };
        if owner == crate::app::TRANSCRIPT_WIN {
            self.transcript_window.scroll_top = scroll_top;
            let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
            let viewport = self.viewport_rows_estimate();
            let max_scroll = (rows.len() as u16).saturating_sub(viewport);
            self.transcript_window.follow_tail = self.transcript_window.scroll_top >= max_scroll;
            self.transcript_window
                .reanchor_to_visible_row(&rows, viewport);
        } else if owner == crate::app::PROMPT_WIN {
            self.input.win.scroll_top = scroll_top;
        }
    }
}
