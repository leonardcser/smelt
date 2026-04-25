//! Mouse event handling: wheel scrolling, drag-select, scrollbar drag, cell-click hit-testing.

use super::*;
use crossterm::event::{MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

impl App {
    // ── Mouse event dispatch ─────────────────────────────────────────────
    pub(super) fn handle_mouse(&mut self, me: MouseEvent) -> EventOutcome {
        use crossterm::event::MouseButton;
        // Wheel routing over a float. The focused float claims vertical
        // scroll outright so a pointer that drifts off its rect doesn't
        // bleed wheel events into the transcript. With no focused float,
        // wheel still routes onto whatever float is under the pointer
        // (unfocused dropdowns — completer, etc.). Horizontal scroll is
        // absorbed (no natural analogue in a vertical list) but not
        // forwarded.
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
        if is_scroll {
            let target = self
                .ui
                .focused_float()
                .or_else(|| self.ui.float_at(me.row, me.column));
            if let Some(win) = target {
                let delta: isize = match me.kind {
                    MouseEventKind::ScrollUp => -3,
                    MouseEventKind::ScrollDown => 3,
                    _ => 0,
                };
                if delta != 0 {
                    if let Some(dlg) = self.ui.dialog_mut(win) {
                        let panel_idx = dlg
                            .panel_at(me.row, me.column)
                            .unwrap_or_else(|| dlg.focused_panel());
                        dlg.panel_scroll_by(panel_idx, delta);
                    }
                }
                return EventOutcome::Redraw;
            }
        }

        // Modal gate. With a focused float up, clicks / drags outside
        // the float's rect are absorbed (no selection extension, no
        // cursor repositioning in the transcript behind). Clicks inside
        // the float continue to the normal path below so the scrollbar
        // drag hit-test and float-aware handlers run.
        if let Some(focused) = self.ui.focused_float() {
            let inside_focused = self.ui.float_at(me.row, me.column) == Some(focused);
            if !inside_focused {
                return EventOutcome::Noop;
            }
        }

        // Down inside a float: scrollbar drag still pre-empts (App
        // owns the latching state machine), but anything else routes
        // through the compositor so each layer's `handle_mouse` runs
        // — click-to-focus widget panels, click-to-select list rows,
        // click-to-position TextInput cursor, etc. Wheel keeps the
        // direct path above because the focused-float-absorbs-wheel
        // policy needs App-level focus knowledge.
        if matches!(me.kind, MouseEventKind::Down(MouseButton::Left))
            && self.ui.float_at(me.row, me.column).is_some()
        {
            {
                if self.begin_dialog_scrollbar_drag_if_hit(me.row, me.column) {
                    self.mouse_drag_active = true;
                    return EventOutcome::Redraw;
                }
                let lua = &self.lua;
                let mut lua_invoke =
                    |handle: ui::LuaHandle,
                     win: ui::WinId,
                     payload: &ui::Payload,
                     panels: &[ui::PanelSnapshot]| {
                        lua.queue_invocation(handle, win, payload, panels);
                    };
                let result = self.ui.handle_mouse_with_lua(me, &mut lua_invoke);
                match result {
                    Some((win, ui::KeyResult::Capture)) => {
                        // Component asked for drag capture (e.g. TextInput
                        // text-select). Subsequent `Drag` / `Up` route to
                        // the same layer regardless of pointer position.
                        self.drag_on_layer = Some(win);
                        self.mouse_drag_active = true;
                    }
                    Some((win, ui::KeyResult::Action(ui::WidgetEvent::Select(idx)))) => {
                        // Completer picker click: commit the click index
                        // through the same path Tab uses (insert label,
                        // close picker, re-sync if /command).
                        let is_completer_picker =
                            self.input.completer.as_ref().and_then(|c| c.picker_win) == Some(win);
                        if is_completer_picker {
                            if let Some((Some(pwin), _was_command)) =
                                self.input.commit_completer_at(idx)
                            {
                                self.input.pending_picker_close.push(pwin);
                            }
                        }
                    }
                    _ => {}
                }
                self.flush_lua_callbacks();
                return EventOutcome::Redraw;
            }
        }

        // Drag / Up while a layer holds capture: route directly to that
        // layer, bypassing pane drag-select logic. Same model as the
        // existing scrollbar drag above.
        if self.drag_on_layer.is_some()
            && matches!(
                me.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            )
        {
            let win = self.drag_on_layer.unwrap();
            let lua = &self.lua;
            let mut lua_invoke = |handle: ui::LuaHandle,
                                  win: ui::WinId,
                                  payload: &ui::Payload,
                                  panels: &[ui::PanelSnapshot]| {
                lua.queue_invocation(handle, win, payload, panels);
            };
            let r = self.ui.handle_mouse_for(win, me, &mut lua_invoke);
            // Layer drag-select on release → copy yanked text to the
            // system clipboard. App owns the clipboard so the ui crate
            // stays platform-agnostic.
            if let Some(ui::KeyResult::Action(ui::WidgetEvent::Yank(text))) = r {
                let _ = crate::app::commands::copy_to_clipboard(&text);
            }
            if matches!(me.kind, MouseEventKind::Up(MouseButton::Left)) {
                self.drag_on_layer = None;
                self.mouse_drag_active = false;
            }
            self.flush_lua_callbacks();
            return EventOutcome::Redraw;
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
                let dragged = self.mouse_drag_active && self.drag_on_scrollbar.is_none();
                match self.app_focus {
                    crate::app::AppFocus::Content => {
                        // A double/triple-click already copied the
                        // initial word/line; on release we re-copy the
                        // (possibly extended) selection if the user
                        // dragged OR if there's a word/line anchor.
                        let had_anchor_span = self.transcript_window.drag_anchor_word.is_some()
                            || self.transcript_window.drag_anchor_line.is_some();
                        if dragged || had_anchor_span {
                            self.handle_content_mouse(me, 0);
                        } else {
                            self.transcript_window.drag_anchor_word = None;
                            self.transcript_window.drag_anchor_line = None;
                        }
                        if let Some(vim) = self.transcript_window.vim.as_mut() {
                            vim.set_mode(crate::vim::ViMode::Normal);
                        } else {
                            self.transcript_window.win_cursor.clear_anchor();
                        }
                    }
                    crate::app::AppFocus::Prompt => {
                        self.copy_prompt_selection_on_release();
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
                // Dialog-panel scrollbar grabs the gesture ahead of any
                // focus-aware handling — clicks on a float's thumb or
                // track jump-scroll and latch the drag.
                if matches!(me.kind, MouseEventKind::Down(MouseButton::Left))
                    && self.begin_dialog_scrollbar_drag_if_hit(me.row, me.column)
                {
                    self.mouse_drag_active = true;
                    return EventOutcome::Redraw;
                }
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

                // First, check if the click lands inside the input text
                // region — that's the only prompt-area hit we position
                // for. Clicks on queued messages, bars, status line etc.
                // only change focus.
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
                        if let Some(content::ViewportHit::Content { row, col }) =
                            vp.hit(me.row, me.column)
                        {
                            self.position_prompt_cursor_from_click(
                                row,
                                col,
                                vp.scroll_top as usize,
                                vp.content_width,
                            );
                        }
                        if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
                            // Save the pre-drag vim mode so mouse-up
                            // can restore it (a drag from Insert should
                            // not land the user in Normal). Applies to
                            // both single-click drag and double-click
                            // word-select, since both enter Visual.
                            if let Some(vim) = self.input.win.vim.as_ref() {
                                self.prompt_drag_return_vim_mode = Some(vim.mode());
                            }
                        }
                        if double {
                            self.select_and_copy_word_in_prompt();
                        } else if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
                            // Anchor a vim Visual selection at the click so
                            // a drag grows inclusive of the anchor char,
                            // matching keyboard `v` behaviour.
                            let anchor = self.input.win.cpos;
                            if let Some(vim) = self.input.win.vim.as_mut() {
                                vim.begin_visual(crate::vim::ViMode::Visual, anchor);
                            } else {
                                self.input.win.win_cursor.set_anchor(Some(anchor));
                            }
                        }
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
    /// = down). Scrolling over the prompt drives vim j/k on the input
    /// buffer; scrolling anywhere else drives the content pane. This
    /// keeps wheel behaviour consistent with the "buffer scroll is
    /// cursor motion" model used by keyboard navigation.
    pub(super) fn scroll_under_mouse(&mut self, row: u16, delta: isize) {
        if matches!(self.layout.hit_test(row, 0), content::HitRegion::Prompt) {
            self.app_focus = crate::app::AppFocus::Prompt;
            self.scroll_prompt_by_lines(delta);
            return;
        }
        if !self.has_transcript_content(self.settings.show_thinking) {
            return;
        }
        self.app_focus = crate::app::AppFocus::Content;
        // tmux copy-mode model: wheel pans the viewport; cpos / cursor
        // stay anchored at their absolute position and scroll out of
        // view. Keyboard motions (j/k, arrows) or a click re-anchor.
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window
            .scroll_view_by(delta, rows.len(), viewport);
    }

    fn scroll_prompt_by_lines(&mut self, delta: isize) {
        let buf = &self.input.win.edit_buf.buf;
        let new_pos = self
            .input
            .win
            .win_cursor
            .move_vertical(buf, self.input.win.cpos, delta);
        if new_pos != self.input.win.cpos {
            self.input.win.cpos = new_pos;
        }
    }

    /// Translate a click inside the prompt input region into a char
    /// offset in `state.buf` and move `state.cpos` there. `rel_row` is
    /// rows below the top of the input region; `col` is the screen
    /// column. Takes current wrap metrics from the last-drawn frame.
    fn position_prompt_cursor_from_click(
        &mut self,
        rel_row: u16,
        col: u16,
        scroll: usize,
        usable: u16,
    ) {
        let target_visual_row = rel_row as usize + scroll;
        let target_col = col as usize;
        let usable = usable as usize;
        let buf = &self.input.buf;

        // Simple char-wrap walk that mirrors `wrap_and_locate_cursor`'s
        // behaviour for the common case of plain text input.
        let mut visual_row = 0usize;
        let mut col_in_line = 0usize;
        let mut target_byte: Option<usize> = None;
        let mut last_byte_on_target_row: Option<usize> = None;
        for (byte_off, ch) in buf.char_indices() {
            if visual_row == target_visual_row {
                last_byte_on_target_row = Some(byte_off);
                if col_in_line == target_col {
                    target_byte = Some(byte_off);
                    break;
                }
            }
            if ch == '\n' {
                if visual_row == target_visual_row && target_byte.is_none() {
                    target_byte = Some(byte_off);
                    break;
                }
                visual_row += 1;
                col_in_line = 0;
                continue;
            }
            col_in_line += 1;
            if col_in_line >= usable {
                if visual_row == target_visual_row && target_byte.is_none() {
                    // Past the end of target row without hitting target
                    // col — clamp to end of line.
                    target_byte = Some(byte_off + ch.len_utf8());
                    break;
                }
                visual_row += 1;
                col_in_line = 0;
            }
        }
        let cpos = target_byte
            .or_else(|| {
                last_byte_on_target_row
                    .map(|b| b + buf[b..].chars().next().map_or(0, |c| c.len_utf8()))
            })
            .unwrap_or(buf.len());
        self.input.win.cpos = cpos.min(buf.len());
        let want = col as usize;
        self.input.win.win_cursor.set_curswant(Some(want));
    }

    /// Extend the active drag selection. Scrollbar drags re-snap the
    /// thumb; content drags delegate to `transcript_window.handle_mouse`
    /// (which knows how to grow word/line anchored drags); prompt
    /// drags still ride the local prompt-side path until Step 4 hands
    /// the prompt to `Window::handle_mouse` too.
    fn extend_selection_to(&mut self, me: MouseEvent) {
        if self.drag_on_scrollbar.is_some() {
            self.apply_scrollbar_drag(me.row);
            return;
        }
        match self.app_focus {
            crate::app::AppFocus::Content => {
                self.handle_content_mouse(me, 0);
            }
            crate::app::AppFocus::Prompt => {
                if let Some(vp) = self.prompt_viewport {
                    if let Some(content::ViewportHit::Content { row: r, col: c }) =
                        vp.hit(me.row, me.column)
                    {
                        self.position_prompt_cursor_from_click(
                            r,
                            c,
                            vp.scroll_top as usize,
                            vp.content_width,
                        );
                    }
                }
                // Vim Visual mode reads cpos directly for `visual_range`
                // — no separate anchor to extend. Only the non-vim path
                // needs the explicit win_cursor extend.
                if self.input.win.vim.is_none() {
                    self.input.win.win_cursor.extend(self.input.win.cpos);
                }
            }
        }
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

    /// Finalise a prompt drag-select: copy any non-empty selection to
    /// the clipboard and clear the anchor. A bare click (no drag) has
    /// anchor == cpos, so this is a no-op in that case. When vim drove
    /// the selection via `Visual` mode, restore whatever mode the user
    /// was in before the drag started (Normal / Insert) so a drag from
    /// Insert doesn't leave them stranded in Normal.
    fn copy_prompt_selection_on_release(&mut self) {
        if let Some((s, e)) = self.input.selection_range() {
            let text: String = self.input.win.edit_buf.buf[s..e].to_string();
            if crate::app::commands::copy_to_clipboard(&text).is_ok() {
                self.input.win.kill_ring.record_clipboard_write(text);
            }
        }
        if let Some(prev) = self.prompt_drag_return_vim_mode.take() {
            if let Some(vim) = self.input.win.vim.as_mut() {
                vim.set_mode(prev);
            }
        }
        self.input.win.win_cursor.clear_anchor();
    }

    /// Double-click on the prompt: select the word under the cursor
    /// (if any) via the shared `Buffer::select_word_at` helper, and
    /// copy it to the clipboard.
    fn select_and_copy_word_in_prompt(&mut self) {
        let cpos = self.input.win.cpos;
        if let Some((s, e)) = self.input.select_word_at(cpos) {
            let text = self.input.win.edit_buf.buf[s..e].to_string();
            if crate::app::commands::copy_to_clipboard(&text).is_ok() {
                self.input.win.kill_ring.record_clipboard_write(text);
            }
        }
    }

    /// Drive a transcript-pane mouse event through `Window::handle_mouse`.
    /// Resolves the projected display rows, soft/hard line breaks and
    /// painted viewport once, snaps the click column into a selectable
    /// cell (so hidden-thinking summary rows route to the fold marker
    /// instead of empty padding), then translates a returned
    /// `Yank` action through `copy_display_range` so the clipboard
    /// receives raw markdown rather than rendered display text.
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
        let action = self.transcript_window.handle_mouse(snapped, ctx);
        self.translate_content_yank(action);
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

    /// Bridge a `Window::handle_mouse` `Yank` action back to the
    /// transcript's display→raw mapping. The window yields display-row
    /// text; the clipboard wants the markdown source. Re-derive the
    /// `(s, e)` byte range from the window's anchors (set during
    /// double/triple-click) or its vim/win_cursor selection (after
    /// drag-up), then run the range through `copy_display_range`.
    fn translate_content_yank(&mut self, action: ui::MouseAction) {
        if !matches!(action, ui::MouseAction::Yank(_)) {
            return;
        }
        let (s, e) = if let Some((s, e)) = self.transcript_window.drag_anchor_word {
            (s, e)
        } else if let Some((s, e)) = self.transcript_window.drag_anchor_line {
            (s, e)
        } else {
            let rows = self.full_transcript_display_text(self.settings.show_thinking);
            let buf = rows.join("\n");
            let cpos = self.transcript_window.compute_cpos(&rows);
            let range = if let Some(vim) = self.transcript_window.vim.as_ref() {
                vim.visual_range(&buf, cpos)
            } else {
                self.transcript_window.selection_range(&rows)
            };
            let Some((s, e)) = range else { return };
            (ui::text::snap(&buf, s), ui::text::snap(&buf, e))
        };
        if e <= s {
            return;
        }
        let text = self.copy_display_range(s, e, self.settings.show_thinking);
        if crate::app::commands::copy_to_clipboard(&text).is_ok() {
            self.transcript_window
                .kill_ring
                .record_clipboard_write(text);
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

    /// If `(row, col)` lands on the scrollbar of a dialog panel owned by
    /// a compositor float, latch a `DialogPanel` drag and snap the
    /// thumb to the pointer. Returns `true` when the event was consumed.
    fn begin_dialog_scrollbar_drag_if_hit(&mut self, row: u16, col: u16) -> bool {
        let Some(win) = self.ui.float_at(row, col) else {
            return false;
        };
        let Some(dialog) = self.ui.dialog_mut(win) else {
            return false;
        };
        let Some(panel_idx) = dialog.panel_at(row, col) else {
            return false;
        };
        let Some(viewport) = dialog.panel_viewport(panel_idx) else {
            return false;
        };
        let Some(bar) = viewport.scrollbar else {
            return false;
        };
        if !bar.contains(viewport.rect, row, col) {
            return false;
        }
        let rel_row = row.saturating_sub(viewport.rect.top);
        let thumb_top = bar.thumb_top_for_click(rel_row);
        dialog.apply_panel_scrollbar_drag(panel_idx, thumb_top);
        self.drag_on_scrollbar = Some(crate::app::ScrollbarDragTarget::DialogPanel {
            win,
            panel: panel_idx,
        });
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
                        self.prompt_input_scroll = from_top as usize;
                    }
                }
            }
            crate::app::ScrollbarDragTarget::DialogPanel { win, panel } => {
                let Some(dialog) = self.ui.dialog_mut(win) else {
                    return;
                };
                let Some(viewport) = dialog.panel_viewport(panel) else {
                    return;
                };
                let Some(bar) = viewport.scrollbar else {
                    return;
                };
                let rel_row = row.saturating_sub(viewport.rect.top);
                let thumb_top = bar.thumb_top_for_click(rel_row);
                dialog.apply_panel_scrollbar_drag(panel, thumb_top);
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
