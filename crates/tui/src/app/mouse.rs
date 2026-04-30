//! Mouse event handling: wheel scrolling, drag-select, scrollbar drag, cell-click hit-testing.

use super::*;
use crossterm::event::{MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

impl App {
    // ── Mouse event dispatch ─────────────────────────────────────────────
    pub(super) fn handle_mouse(&mut self, me: MouseEvent) -> EventOutcome {
        use crossterm::event::MouseButton;
        // Wheel routing over an overlay. A focused overlay (picker /
        // cmdline / dialog) absorbs vertical scroll outright so a
        // pointer that drifts off its rect doesn't bleed wheel events
        // into the transcript behind it.
        //
        // Under this model wheel moves the viewport only (scroll_top)
        // and never the selection — `panel_scroll_by` is the primitive.
        let is_scroll = matches!(
            me.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        );
        if is_scroll && self.ui.focused_overlay().is_some() {
            return EventOutcome::Redraw;
        }

        // Modal gate. With an active modal overlay up, clicks / drags
        // outside its rect are absorbed (no selection extension, no
        // cursor repositioning in the transcript behind). Clicks
        // inside the overlay continue to the normal path below so
        // scrollbar-drag hit-test and overlay-aware handlers run.
        if let Some(modal_id) = self.ui.active_modal() {
            let inside = self
                .ui
                .overlay_hit_test(me.row, me.column, None)
                .is_some_and(|(id, _)| id == modal_id);
            if !inside {
                return EventOutcome::Noop;
            }
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
                // Scrollbar drag is an App-owned gesture and takes the
                // Up itself — the adapter call is skipped there.
                if self.drag_on_scrollbar.is_none() {
                    self.dispatch_focused_mouse(me, 0);
                }
                if self.app_focus == crate::app::AppFocus::Prompt {
                    if let Some(prev) = self.prompt_drag_return_vim_mode.take() {
                        if let Some(vim) = self.input.win.vim.as_mut() {
                            vim.set_mode(prev);
                        }
                    }
                }
                self.mouse_drag_active = false;
                self.drag_autoscroll_since = None;
                self.drag_on_scrollbar = None;
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
                // Click-count tracking: successive primary-button Downs
                // on the same cell within 400ms increment the count.
                // 2 → word-select + copy, 3 → line-select + copy. After
                // 3 the count wraps back to 1 so a fourth click starts
                // a fresh gesture.
                let now = Instant::now();
                let count = match self.last_click {
                    Some((t, r, c, n))
                        if now.duration_since(t) < Duration::from_millis(400)
                            && r == me.row
                            && c == me.column
                            && n < 3 =>
                    {
                        n + 1
                    }
                    _ => 1,
                };
                self.last_click = Some((now, me.row, me.column, count));
                let double = count == 2;

                // Prompt input area: route Down through the unified
                // `Window::handle_mouse` path (via the prompt mouse
                // adapter, which handles source ↔ wrapped translation).
                // Same primitive transcript and dialog buffer panels
                // use, so click cadence / drag anchors / yank-on-
                // release / theme selection bg are shared.
                let _ = double;
                if let Some(vp) = self.prompt_viewport {
                    if vp.contains(me.row, me.column) {
                        self.app_focus = crate::app::AppFocus::Prompt;
                        if self.begin_scrollbar_drag_if_hit(
                            me.row,
                            me.column,
                            crate::app::AppFocus::Prompt,
                        ) {
                            return EventOutcome::Redraw;
                        }
                        self.drag_on_scrollbar = None;
                        if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
                            if let Some(vim) = self.input.win.vim.as_ref() {
                                self.prompt_drag_return_vim_mode = Some(vim.mode());
                            }
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
                if !self.has_transcript_content(self.settings.show_thinking) {
                    return EventOutcome::Noop;
                }
                self.app_focus = crate::app::AppFocus::Content;
                // Route the event through the `Viewport` recorded by
                // the last paint: scrollbar clicks latch a
                // `ScrollbarDrag`; content clicks delegate to the
                // transcript Window's mouse handler.
                if self.begin_scrollbar_drag_if_hit(
                    me.row,
                    me.column,
                    crate::app::AppFocus::Content,
                ) {
                    return EventOutcome::Redraw;
                }
                self.drag_on_scrollbar = None;
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
            // display rows). `win_cursor.move_vertical` operates on
            // source rows; the renderer's `ensure_cursor_visible`
            // (Step 6) syncs `scroll_top` to keep the cursor visible.
            // Once 7b lands the row-space adapter, this collapses into
            // a shared `Window::scroll_by_lines` call with the rest.
            let buf = self.input.win.edit_buf.buf.clone();
            let new_pos = self
                .input
                .win
                .win_cursor
                .move_vertical(&buf, self.input.win.cpos, delta);
            if new_pos != self.input.win.cpos {
                self.input.win.cpos = new_pos;
            }
            return;
        }
        if !self.has_transcript_content(self.settings.show_thinking) {
            return;
        }
        self.app_focus = crate::app::AppFocus::Content;
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window
            .scroll_by_lines(delta, &rows, viewport);
    }

    /// Route a mouse event to the focused buffer surface's adapter.
    /// Both adapters wrap `Window::handle_mouse` with the per-surface
    /// row/break/viewport plumbing, so all the App layer needs is the
    /// focus dispatch.
    fn dispatch_focused_mouse(&mut self, me: MouseEvent, click_count: u8) {
        match self.app_focus {
            crate::app::AppFocus::Content => self.handle_content_mouse(me, click_count),
            crate::app::AppFocus::Prompt => self.handle_prompt_mouse(me, click_count),
        }
    }

    /// Drag handler: scrollbar drags re-snap the thumb; otherwise the
    /// event flows to the focused buffer adapter.
    fn extend_selection_to(&mut self, me: MouseEvent) {
        if self.drag_on_scrollbar.is_some() {
            self.apply_scrollbar_drag(me.row);
            return;
        }
        self.dispatch_focused_mouse(me, 0);
    }

    /// Frame-tick hook: if the user is mid-drag with the content cursor
    /// on the top or bottom row of the viewport, scroll a single line
    /// so the selection widens past the visible area. One-line-per-tick
    /// avoids the choppy feel of multi-line jumps; the main loop ramps
    /// its sleep interval down the longer the cursor stays at the edge,
    /// which is how acceleration happens.
    pub(super) fn tick_drag_autoscroll(&mut self) {
        if !self.mouse_drag_active
            || self.app_focus != crate::app::AppFocus::Content
            || self.drag_on_scrollbar.is_some()
        {
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
        let Some(vp) = self.prompt_viewport else {
            return;
        };
        let usable = vp.content_width as usize;
        let wrap = crate::content::prompt_wrap::PromptWrap::build(&self.input, usable);
        if wrap.rows.is_empty() {
            return;
        }

        // Pre-call: translate source-byte state on `state.win` into
        // wrapped-row-byte space. cpos, vim Visual anchor, win_cursor
        // anchor, and the two drag anchors all need translation.
        let saved_src_cpos = self.input.win.cpos;
        let saved_src_anchor = self.input.win.win_cursor.anchor();
        let saved_src_dword = self.input.win.drag_anchor_word;
        let saved_src_dline = self.input.win.drag_anchor_line;
        let saved_vim_visual_anchor = self.input.win.vim.as_ref().and_then(|v| v.visual_anchor());

        self.input.win.cpos = wrap.src_to_wrapped(saved_src_cpos);
        self.input
            .win
            .win_cursor
            .set_anchor(saved_src_anchor.map(|a| wrap.src_to_wrapped(a)));
        self.input.win.drag_anchor_word =
            saved_src_dword.map(|(s, e)| (wrap.src_to_wrapped(s), wrap.src_to_wrapped(e)));
        self.input.win.drag_anchor_line =
            saved_src_dline.map(|(s, e)| (wrap.src_to_wrapped(s), wrap.src_to_wrapped(e)));
        if let Some(vim) = self.input.win.vim.as_mut() {
            if let Some(a) = saved_vim_visual_anchor {
                vim.begin_visual(crate::vim::ViMode::Visual, wrap.src_to_wrapped(a));
            }
        }

        // Build the same `MouseCtx` shape the transcript uses.
        let ctx = ui::MouseCtx {
            rows: &wrap.rows,
            soft_breaks: &wrap.soft_breaks,
            hard_breaks: &wrap.hard_breaks,
            viewport: vp,
            click_count,
        };
        let action = self.input.win.handle_mouse(me, ctx);

        // Post-call: translate state on `state.win` back to source
        // bytes. `Window::mouse_up` already cleared its anchors, so
        // reading them after Up just yields `None`.
        let new_w_cpos = self.input.win.cpos;
        let new_w_anchor = self.input.win.win_cursor.anchor();
        let new_w_dword = self.input.win.drag_anchor_word;
        let new_w_dline = self.input.win.drag_anchor_line;
        let new_w_vim_anchor = self.input.win.vim.as_ref().and_then(|v| v.visual_anchor());

        self.input.win.cpos = wrap.wrapped_to_src(new_w_cpos);
        self.input
            .win
            .win_cursor
            .set_anchor(new_w_anchor.map(|a| wrap.wrapped_to_src(a)));
        self.input.win.drag_anchor_word =
            new_w_dword.map(|(s, e)| (wrap.wrapped_to_src(s), wrap.wrapped_to_src(e)));
        self.input.win.drag_anchor_line =
            new_w_dline.map(|(s, e)| (wrap.wrapped_to_src(s), wrap.wrapped_to_src(e)));
        if let Some(vim) = self.input.win.vim.as_mut() {
            if let Some(a) = new_w_vim_anchor {
                vim.begin_visual(crate::vim::ViMode::Visual, wrap.wrapped_to_src(a));
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
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        if rows.is_empty() {
            return;
        }
        let (soft, hard) = self.transcript_line_breaks(self.settings.show_thinking);
        let Some(viewport) = self.transcript_viewport else {
            return;
        };
        let snapped = self.snap_event_for_selection(me, &rows, viewport);
        let ctx = ui::MouseCtx {
            rows: &rows,
            soft_breaks: &soft,
            hard_breaks: &hard,
            viewport,
            click_count,
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
        let snapped = self.snap_col_to_selectable(line_idx, rel_col, self.settings.show_thinking);
        MouseEvent {
            column: vp.rect.left.saturating_add(snapped as u16),
            ..me
        }
    }

    /// Snap the viewport so the scrollbar thumb lands at screen row
    /// `screen_row`. Uses the `Viewport` recorded by the last
    /// paint — no re-measuring of the transcript on drag. Returns
    /// `true` when the region has a visible scrollbar and the jump was
    /// applied.
    /// If `(row, col)` lands on the scrollbar of `target`'s pane, latch
    /// a `ScrollbarDrag` that preserves the click's offset within the
    /// thumb, and snap the buffer's scroll so the thumb stays under the
    /// pointer. Returns `true` when the event was consumed.
    fn begin_scrollbar_drag_if_hit(
        &mut self,
        row: u16,
        col: u16,
        target: crate::app::AppFocus,
    ) -> bool {
        let Some(vp) = self.viewport_for(target) else {
            return false;
        };
        let Some(bar) = vp.scrollbar else {
            return false;
        };
        if !bar.contains(vp.rect, row, col) {
            return false;
        }
        self.drag_on_scrollbar = Some(crate::app::ScrollbarDragTarget::Focus(target));
        self.apply_scrollbar_drag(row);
        true
    }

    /// Apply an in-flight `ScrollbarDrag` to the current pointer row:
    /// translate the thumb-relative anchor back into a thumb-top, then
    /// into a buffer scroll offset via the region's proportional map.
    fn apply_scrollbar_drag(&mut self, row: u16) {
        let Some(target) = self.drag_on_scrollbar else {
            return;
        };
        match target {
            crate::app::ScrollbarDragTarget::Focus(focus) => {
                let Some(vp) = self.viewport_for(focus) else {
                    return;
                };
                let Some(bar) = vp.scrollbar else {
                    return;
                };
                let rel_row = row.saturating_sub(vp.rect.top);
                let thumb_top = bar.thumb_top_for_click(rel_row);
                let from_top = bar.scroll_from_top_for_thumb(thumb_top);
                match focus {
                    crate::app::AppFocus::Content => {
                        self.transcript_window.scroll_top = from_top;
                        let rows = self.full_transcript_display_text(self.settings.show_thinking);
                        let viewport = self.viewport_rows_estimate();
                        let max_scroll = (rows.len() as u16).saturating_sub(viewport);
                        self.transcript_window.follow_tail =
                            self.transcript_window.scroll_top >= max_scroll;
                        self.transcript_window
                            .reanchor_to_visible_row(&rows, viewport);
                    }
                    crate::app::AppFocus::Prompt => {
                        self.input.win.scroll_top = from_top;
                    }
                }
            }
        }
    }

    /// Lookup the currently-painted viewport for a pane.
    fn viewport_for(&self, target: crate::app::AppFocus) -> Option<ui::WindowViewport> {
        match target {
            crate::app::AppFocus::Content => self.transcript_viewport,
            crate::app::AppFocus::Prompt => self.prompt_viewport,
        }
    }
}
