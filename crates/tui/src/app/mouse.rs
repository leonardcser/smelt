//! Mouse event handling: wheel scrolling, drag-select, scrollbar drag, cell-click hit-testing.

use super::*;
use crossterm::event::{MouseEvent, MouseEventKind};

impl TuiApp {
    // ── Mouse event dispatch ─────────────────────────────────────────────
    pub(super) fn handle_mouse(&mut self, me: MouseEvent) -> EventOutcome {
        use crossterm::event::MouseButton;
        // Wheel-over-overlay absorb, active-modal click-outside
        // absorb, and the scrollbar drag gesture all live in
        // `Ui::dispatch_event(Event::Mouse(_))`. For wheel-on-overlay
        // we surface a redraw so the pointer hover updates; modal
        // absorb returns Noop. Scrollbar Down/Drag/Up land Ui-side
        // and return `Status::Consumed`; the host reads back the new
        // `scroll_top` from `Ui::win` and propagates to its mirror
        // state for the owner pane. Anything Ui doesn't claim
        // (`Ignored`) keeps flowing through the TuiApp-side routing
        // below.
        let cap_before = self.ui.capture();
        if matches!(
            self.ui
                .dispatch_event(ui::Event::Mouse(me), &mut |_, _, _| {}),
            ui::Status::Consumed
        ) {
            let scrollbar_owner = match (cap_before, self.ui.capture()) {
                (Some(ui::HitTarget::Scrollbar { owner }), _)
                | (_, Some(ui::HitTarget::Scrollbar { owner })) => Some(owner),
                _ => None,
            };
            if let Some(owner) = scrollbar_owner {
                self.propagate_scrollbar_scroll(owner);
                if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
                    if owner == ui::TRANSCRIPT_WIN {
                        self.app_focus = crate::app::AppFocus::Content;
                    } else if owner == ui::PROMPT_WIN {
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

        if self.layout.hit_test(me.row, me.column) == content::HitRegion::Status {
            return EventOutcome::Noop;
        }

        // Wheel scroll routes through `Ui::hit_test` directly — no
        // capture, no per-pane Down/Drag/Up state to thread.
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

        // Down/Drag/Up Left for splits-leaf Windows fold into
        // `Ui::resolve_split_mouse`: hit-test → window resolution +
        // click-count tracking + `HitTarget::Window` capture so a
        // drag started on the prompt continues routing to the
        // prompt even if the pointer drifts onto the transcript.
        // The host's role narrows to per-pane forwarding +
        // app_focus / vim-mode side effects bracketing the call.
        if let Some((win, count)) = self.ui.resolve_split_mouse(me) {
            let is_down = matches!(me.kind, MouseEventKind::Down(MouseButton::Left));
            let is_up = matches!(me.kind, MouseEventKind::Up(MouseButton::Left));
            if win == ui::PROMPT_WIN {
                if is_down {
                    self.app_focus = crate::app::AppFocus::Prompt;
                    if self.input.win.vim_enabled {
                        self.prompt_drag_return_vim_mode = Some(self.vim_mode);
                    }
                }
                self.handle_prompt_mouse(me, count);
                if is_up {
                    if let Some(prev) = self.prompt_drag_return_vim_mode.take() {
                        if self.input.win.vim_enabled {
                            self.input.win.vim_state.set_mode(&mut self.vim_mode, prev);
                        }
                    }
                }
            } else if win == ui::TRANSCRIPT_WIN {
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

        // Down on a non-Window region (e.g. a chrome or a click that
        // hits no splits leaf): promote focus to Prompt when the
        // layout maps it to the prompt/status zone, mirroring the
        // pre-fold focus-shift behaviour.
        if matches!(me.kind, MouseEventKind::Down(_))
            && matches!(
                self.layout.hit_test(me.row, me.column),
                content::HitRegion::Prompt | content::HitRegion::Status
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

    /// Push selected text to the system clipboard and kill ring.
    /// Called on mouse-up when the transcript (or future dialog buffer)
    /// had an active selection.
    fn yank_to_clipboard(&mut self, text: String) {
        if self.core.clipboard.write(&text).is_ok() {
            self.core
                .clipboard
                .kill_ring
                .record_clipboard_write(text.clone());
        }
        self.core.clipboard.kill_ring.set_with_linewise(text, false);
    }

    /// Scroll the pane under the mouse cursor by `delta` lines (positive
    /// = down). Unified policy across transcript / prompt / dialog
    /// buffer panels: wheel moves `cpos` AND `scroll_top` together by
    /// `delta` visual rows. The cursor's viewport-relative row stays
    /// constant — visually pinned in the viewport while the buffer
    /// scrolls under it. Same UX as the transcript today, applied to
    /// every buffer surface. Routing reads `Ui::hit_test(row, col,
    /// None)`: a `Window(PROMPT_WIN)` hit drives the prompt; anything
    /// else falls back to the transcript scroll path.
    pub(super) fn scroll_under_mouse(&mut self, row: u16, col: u16, delta: isize) {
        let on_prompt = matches!(
            self.ui.hit_test(row, col, None),
            Some(ui::HitTarget::Window(w)) if w == ui::PROMPT_WIN
        );
        if on_prompt {
            self.app_focus = crate::app::AppFocus::Prompt;
            // Prompt's `edit_buf.buf` is the source buffer (≠ wrapped
            // display rows). The vertical-motion helper operates on
            // source rows; the renderer's `ensure_cursor_visible`
            // (Step 6) syncs `scroll_top` to keep the cursor visible.
            // Once 7b lands the row-space adapter, this collapses into
            // a shared `Window::scroll_by_lines` call with the rest.
            let (new_pos, new_want) = ui::text::vertical_move(
                &self.input.win.edit_buf.buf,
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
            .scroll_by_lines(delta, &rows, viewport, &mut self.vim_mode);
    }

    /// Frame-tick hook: if the user is mid-drag with the captured
    /// window's cursor on the top or bottom row of its viewport, scroll
    /// a single line so the selection widens past the visible area.
    /// One-line-per-tick avoids the choppy feel of multi-line jumps;
    /// the main loop ramps its sleep interval down the longer the
    /// cursor stays at the edge, which is how acceleration happens.
    /// Edge detection lives on `Ui::poll_drag_autoscroll`; per-pane
    /// scroll-by-line action stays here so transcript-specific cursor
    /// snapping rides with it.
    pub(super) fn tick_drag_autoscroll(&mut self) {
        let Some((win, delta)) = self.ui.poll_drag_autoscroll() else {
            return;
        };
        if win == ui::TRANSCRIPT_WIN && self.app_focus == crate::app::AppFocus::Content {
            self.move_content_cursor_by_lines(delta);
        }
    }

    /// Drive a prompt mouse event through `Window::handle_mouse` —
    /// same primitive the transcript and dialog buffer panels use.
    /// The prompt's source buffer ≠ wrapped display rows, so we
    /// translate window state into wrapped-row byte space before the
    /// call, run the dispatch, then translate cpos / anchors back to
    /// source bytes.
    ///
    /// Prompt mouse yank is not implemented yet — the wrapped display
    /// text would need byte-range translation back to source bytes.
    fn handle_prompt_mouse(&mut self, me: MouseEvent, click_count: u8) {
        let Some(vp) = ui::UiHost::viewport_for(self, ui::PROMPT_WIN) else {
            return;
        };
        let usable = vp.content_width as usize;
        let wrap = crate::content::prompt_wrap::PromptWrap::build(&self.input, usable);
        if wrap.rows.is_empty() {
            return;
        }

        // Pre-call: translate source-byte state on `state.win` into
        // wrapped-row-byte space. cpos, vim Visual anchor, selection
        // anchor, and the two drag anchors all need translation.
        let saved_src_cpos = self.input.win.cpos;
        let saved_src_anchor = self.input.win.selection_anchor;
        let saved_src_dword = self.input.win.drag_anchor_word;
        let saved_src_dline = self.input.win.drag_anchor_line;
        let saved_vim_visual_anchor = self
            .input
            .win
            .vim_enabled
            .then(|| ui::vim::visual_anchor(&self.input.win.vim_state, self.vim_mode))
            .flatten();

        self.input.win.cpos = wrap.src_to_wrapped(saved_src_cpos);
        self.input.win.selection_anchor = saved_src_anchor.map(|a| wrap.src_to_wrapped(a));
        self.input.win.drag_anchor_word =
            saved_src_dword.map(|(s, e)| (wrap.src_to_wrapped(s), wrap.src_to_wrapped(e)));
        self.input.win.drag_anchor_line =
            saved_src_dline.map(|(s, e)| (wrap.src_to_wrapped(s), wrap.src_to_wrapped(e)));
        if self.input.win.vim_enabled {
            if let Some(a) = saved_vim_visual_anchor {
                self.input.win.vim_state.begin_visual(
                    &mut self.vim_mode,
                    ui::VimMode::Visual,
                    wrap.src_to_wrapped(a),
                );
            }
        }

        let mouse_ctx = ui::MouseCtx {
            rows: &wrap.rows,
            soft_breaks: &wrap.soft_breaks,
            hard_breaks: &wrap.hard_breaks,
            viewport: vp,
            click_count,
            vim_mode: &mut self.vim_mode,
        };
        let (_, _yank) = self.input.win.handle_mouse(me, mouse_ctx);

        // Post-call: translate state on `state.win` back to source
        // bytes. `Window::mouse_up` already cleared its anchors, so
        // reading them after Up just yields `None`.
        let new_w_cpos = self.input.win.cpos;
        let new_w_anchor = self.input.win.selection_anchor;
        let new_w_dword = self.input.win.drag_anchor_word;
        let new_w_dline = self.input.win.drag_anchor_line;
        let new_w_vim_anchor = self
            .input
            .win
            .vim_enabled
            .then(|| ui::vim::visual_anchor(&self.input.win.vim_state, self.vim_mode))
            .flatten();

        self.input.win.cpos = wrap.wrapped_to_src(new_w_cpos);
        self.input.win.selection_anchor = new_w_anchor.map(|a| wrap.wrapped_to_src(a));
        self.input.win.drag_anchor_word =
            new_w_dword.map(|(s, e)| (wrap.wrapped_to_src(s), wrap.wrapped_to_src(e)));
        self.input.win.drag_anchor_line =
            new_w_dline.map(|(s, e)| (wrap.wrapped_to_src(s), wrap.wrapped_to_src(e)));
        if self.input.win.vim_enabled {
            if let Some(a) = new_w_vim_anchor {
                self.input.win.vim_state.begin_visual(
                    &mut self.vim_mode,
                    ui::VimMode::Visual,
                    wrap.wrapped_to_src(a),
                );
            }
        }
    }

    /// Drive a transcript-pane mouse event through `Window::handle_mouse`.
    /// Resolves the projected display rows, soft/hard line breaks and
    /// painted viewport once, snaps the click column into a selectable
    /// cell (so hidden-thinking summary rows route to the fold marker
    /// instead of empty padding), and lets the window mutate its own
    /// selection state.
    ///
    /// On `MouseUp`, returns the selected text so the host can yank
    /// it to the clipboard.
    fn handle_content_mouse(&mut self, me: MouseEvent, click_count: u8) -> Option<String> {
        let rows = ui::UiHost::rows_for(self, ui::TRANSCRIPT_WIN)?;
        if rows.is_empty() {
            return None;
        }
        let (soft, hard) = ui::UiHost::breaks_for(self, ui::TRANSCRIPT_WIN)?;
        let viewport = ui::UiHost::viewport_for(self, ui::TRANSCRIPT_WIN)?;
        let snapped = self.snap_event_for_selection(me, &rows, viewport);
        let mouse_ctx = ui::MouseCtx {
            rows: &rows,
            soft_breaks: &soft,
            hard_breaks: &hard,
            viewport,
            click_count,
            vim_mode: &mut self.vim_mode,
        };
        let (_, yank) = self.transcript_window.handle_mouse(snapped, mouse_ctx);
        yank
    }

    /// Translate `me`'s screen column into a *selectable* column for the
    /// clicked display row. In hidden-thinking summary rows some columns
    /// render padding/glyphs that aren't selectable, so a click past
    /// them must snap onto the nearest selectable cell — matches what
    /// `position_content_cursor_from_hit` did before unification.
    fn snap_event_for_selection(
        &mut self,
        me: MouseEvent,
        rows: &[String],
        vp: ui::WindowViewport,
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

    /// Mirror the new `scroll_top` `Ui::dispatch_event` wrote to
    /// `Ui::wins[owner]` back onto the host's parallel pane state so
    /// the next render reflects the scroll. For the transcript also
    /// recomputes `follow_tail` against the projected display rows
    /// and re-anchors its in-pane cursor to whichever visible row
    /// the scroll resolved to. PROMPT_WIN only needs the raw
    /// `scroll_top` copy — the next render-loop pass clamps it
    /// against the prompt buffer's height.
    fn propagate_scrollbar_scroll(&mut self, owner: ui::WinId) {
        let Some(scroll_top) = self.ui.win(owner).map(|w| w.scroll_top) else {
            return;
        };
        if owner == ui::TRANSCRIPT_WIN {
            self.transcript_window.scroll_top = scroll_top;
            let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
            let viewport = self.viewport_rows_estimate();
            let max_scroll = (rows.len() as u16).saturating_sub(viewport);
            self.transcript_window.follow_tail = self.transcript_window.scroll_top >= max_scroll;
            self.transcript_window
                .reanchor_to_visible_row(&rows, viewport);
        } else if owner == ui::PROMPT_WIN {
            self.input.win.scroll_top = scroll_top;
        }
    }
}
