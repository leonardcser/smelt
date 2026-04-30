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
        // Drag + release drive tmux-style click-drag-copy. Works in
        // both the prompt and the content pane — each extends its own
        // buffer's selection anchor.
        match me.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                self.mouse_drag_active = true;
                self.extend_selection_to(me);
                return EventOutcome::Redraw;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // `Window::mouse_up` self-guards bare clicks (no yank,
                // no clear-on-empty) and handles its own anchor cleanup,
                // so both branches just dispatch unconditionally.
                self.dispatch_focused_mouse(me, 0);
                if self.app_focus == crate::app::AppFocus::Prompt {
                    if let Some(prev) = self.prompt_drag_return_vim_mode.take() {
                        if self.input.win.vim_enabled {
                            self.input.win.vim_state.set_mode(&mut self.vim_mode, prev);
                        }
                    }
                }
                self.mouse_drag_active = false;
                self.drag_autoscroll_since = None;
                return EventOutcome::Redraw;
            }
            _ => {}
        }

        match me.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_under_mouse(me.row, -3);
                EventOutcome::Redraw
            }
            MouseEventKind::ScrollDown => {
                self.scroll_under_mouse(me.row, 3);
                EventOutcome::Redraw
            }
            MouseEventKind::Down(_) => {
                // 2 → word-select + copy, 3 → line-select + copy. The
                // 400ms / same-cell / cap-at-3 policy lives on `Ui`.
                let count = self.ui.record_click(me.row, me.column);
                let double = count == 2;

                // Prompt input area: route Down through the unified
                // `Window::handle_mouse` path (via the prompt mouse
                // adapter, which handles source ↔ wrapped translation).
                // Same primitive transcript and dialog buffer panels
                // use, so click cadence / drag anchors / yank-on-
                // release / theme selection bg are shared.
                let _ = double;
                if let Some(vp) = self.viewport_for(ui::PROMPT_WIN) {
                    if vp.contains(me.row, me.column) {
                        self.app_focus = crate::app::AppFocus::Prompt;
                        if matches!(me.kind, MouseEventKind::Down(MouseButton::Left))
                            && self.input.win.vim_enabled
                        {
                            self.prompt_drag_return_vim_mode = Some(self.vim_mode);
                        }
                        self.handle_prompt_mouse(me, count);
                        return EventOutcome::Redraw;
                    }
                }

                if matches!(
                    self.layout.hit_test(me.row, me.column),
                    content::HitRegion::Prompt | content::HitRegion::Status
                ) {
                    if self.app_focus != crate::app::AppFocus::Prompt {
                        self.app_focus = crate::app::AppFocus::Prompt;
                        return EventOutcome::Redraw;
                    }
                    return EventOutcome::Noop;
                }
                if !self.has_transcript_content(self.core.config.settings.show_thinking) {
                    return EventOutcome::Noop;
                }
                self.app_focus = crate::app::AppFocus::Content;
                // Route the event through the `Viewport` recorded by
                // the last paint: content clicks delegate to the
                // transcript Window's mouse handler. Scrollbar clicks
                // are absorbed earlier by `Ui::dispatch_event`.
                self.handle_content_mouse(me, count);
                EventOutcome::Redraw
            }
            _ => EventOutcome::Noop,
        }
    }

    /// Scroll the pane under the mouse cursor by `delta` lines (positive
    /// = down). Unified policy across transcript / prompt / dialog
    /// buffer panels: wheel moves `cpos` AND `scroll_top` together by
    /// `delta` visual rows. The cursor's viewport-relative row stays
    /// constant — visually pinned in the viewport while the buffer
    /// scrolls under it. Same UX as the transcript today, applied to
    /// every buffer surface.
    pub(super) fn scroll_under_mouse(&mut self, row: u16, delta: isize) {
        if matches!(self.layout.hit_test(row, 0), content::HitRegion::Prompt) {
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

    /// Route a mouse event to the focused buffer surface's adapter.
    /// Both adapters wrap `Window::handle_mouse` with the per-surface
    /// row/break/viewport plumbing, so all the TuiApp layer needs is the
    /// focus dispatch.
    fn dispatch_focused_mouse(&mut self, me: MouseEvent, click_count: u8) {
        match self.app_focus {
            crate::app::AppFocus::Content => self.handle_content_mouse(me, click_count),
            crate::app::AppFocus::Prompt => self.handle_prompt_mouse(me, click_count),
        }
    }

    /// Drag handler: scrollbar drags are absorbed Ui-side; this only
    /// fires for content drags, which extend the focused buffer's
    /// selection.
    fn extend_selection_to(&mut self, me: MouseEvent) {
        self.dispatch_focused_mouse(me, 0);
    }

    /// Frame-tick hook: if the user is mid-drag with the content cursor
    /// on the top or bottom row of the viewport, scroll a single line
    /// so the selection widens past the visible area. One-line-per-tick
    /// avoids the choppy feel of multi-line jumps; the main loop ramps
    /// its sleep interval down the longer the cursor stays at the edge,
    /// which is how acceleration happens.
    pub(super) fn tick_drag_autoscroll(&mut self) {
        if !self.mouse_drag_active || self.app_focus != crate::app::AppFocus::Content {
            self.drag_autoscroll_since = None;
            return;
        }
        let viewport = self.viewport_rows_estimate();
        if viewport == 0 {
            self.drag_autoscroll_since = None;
            return;
        }
        // `cursor_line` counts from the top of the viewport: 0 = top
        // row, viewport-1 = bottom row. Top edge → cursor-up (-1) so the
        // viewport scrolls to reveal older rows; bottom edge → cursor-
        // down (+1) so newer rows come into view.
        let delta: isize = if self.transcript_window.cursor_line == 0 {
            -1
        } else if self.transcript_window.cursor_line >= viewport.saturating_sub(1) {
            1
        } else {
            self.drag_autoscroll_since = None;
            return;
        };
        self.drag_autoscroll_since
            .get_or_insert_with(std::time::Instant::now);
        self.move_content_cursor_by_lines(delta);
    }

    /// Drive a prompt mouse event through `Window::handle_mouse` —
    /// same primitive the transcript and dialog buffer panels use.
    /// The prompt's source buffer ≠ wrapped display rows, so we
    /// translate window state into wrapped-row byte space before the
    /// call, run the dispatch, then translate cpos / anchors back to
    /// source bytes. Yank text is re-sliced from source so soft-wrap
    /// `\n`s don't leak into the clipboard.
    fn handle_prompt_mouse(&mut self, me: MouseEvent, click_count: u8) {
        let Some(vp) = self.viewport_for(ui::PROMPT_WIN) else {
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

        // Build the same `MouseCtx` shape the transcript uses.
        let ctx = ui::MouseCtx {
            rows: &wrap.rows,
            soft_breaks: &wrap.soft_breaks,
            hard_breaks: &wrap.hard_breaks,
            viewport: vp,
            click_count,
            vim_mode: &mut self.vim_mode,
        };
        let action = self.input.win.handle_mouse(me, ctx);

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

        let _ = action;
    }

    /// Drive a transcript-pane mouse event through `Window::handle_mouse`.
    /// Resolves the projected display rows, soft/hard line breaks and
    /// painted viewport once, snaps the click column into a selectable
    /// cell (so hidden-thinking summary rows route to the fold marker
    /// instead of empty padding), and lets the window mutate its own
    /// selection state.
    fn handle_content_mouse(&mut self, me: MouseEvent, click_count: u8) {
        let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
        if rows.is_empty() {
            return;
        }
        let (soft, hard) = self.transcript_line_breaks(self.core.config.settings.show_thinking);
        let Some(viewport) = self.viewport_for(ui::TRANSCRIPT_WIN) else {
            return;
        };
        let snapped = self.snap_event_for_selection(me, &rows, viewport);
        let ctx = ui::MouseCtx {
            rows: &rows,
            soft_breaks: &soft,
            hard_breaks: &hard,
            viewport,
            click_count,
            vim_mode: &mut self.vim_mode,
        };
        let _ = self.transcript_window.handle_mouse(snapped, ctx);
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

    /// Lookup the currently-painted viewport for a known split owner.
    /// `Window::viewport` is the source of truth; the host writes it
    /// at paint time and reads back whenever a mouse event needs the
    /// last-painted geometry.
    fn viewport_for(&self, owner: ui::WinId) -> Option<ui::WindowViewport> {
        self.ui.win(owner).and_then(|w| w.viewport)
    }
}
