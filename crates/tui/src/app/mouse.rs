//! Mouse event handling: wheel scrolling, drag-select, scrollbar drag, cell-click hit-testing.

use crate::app::transcript::TranscriptProjectionRestore;
use crate::app::transcript_scroll_trace::TranscriptScrollIntent;
use crate::app::{AppFocus, EventOutcome, PromptResizeClick, PromptResizeDrag, TuiApp};
use crate::content::layout::HitRegion;
use crate::smelt_edit::{HitTarget, RowIndex, WinId};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

const PROMPT_RESIZE_DOUBLE_CLICK_WINDOW: std::time::Duration =
    std::time::Duration::from_millis(500);

/// `true` for the four wheel directions. Used to decide whether an event
/// merits a redraw after `Ui::dispatch_event` consumed it.
pub(crate) fn is_scroll_event(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
    )
}

/// `true` for a left-button press. The dispatcher uses this to gate focus
/// promotion and yank-on-release.
pub(crate) fn is_left_down(kind: MouseEventKind) -> bool {
    matches!(kind, MouseEventKind::Down(MouseButton::Left))
}

/// Inspect the capture state immediately before and after `Ui::dispatch_event`
/// and report which window owns the scrollbar drag, if any. Either edge of
/// the transition counts: capture *starts* on Down, but a drag/Up still
/// observes the scrollbar in `cap_before` after `Ui::dispatch_event` clears it.
pub(crate) fn scrollbar_owner_from_capture_transition(
    before: Option<HitTarget>,
    after: Option<HitTarget>,
) -> Option<WinId> {
    match (before, after) {
        (Some(HitTarget::Scrollbar { owner }), _) | (_, Some(HitTarget::Scrollbar { owner })) => {
            Some(owner)
        }
        _ => None,
    }
}

/// Map a well-known [`WinId`] to the [`AppFocus`] it represents. Used by the
/// scrollbar-click path so dragging the transcript scrollbar promotes
/// content focus and dragging the prompt scrollbar promotes prompt focus.
/// Returns `None` for any other window.
pub(crate) fn app_focus_for_owner(owner: WinId) -> Option<AppFocus> {
    if owner == crate::app::TRANSCRIPT_WIN {
        Some(AppFocus::Content)
    } else if owner == crate::app::PROMPT_WIN {
        Some(AppFocus::Prompt)
    } else {
        None
    }
}

/// Compute the new [`AppFocus`] for a left-down click landing in `region`.
/// `has_transcript_content` is `false` when the transcript pane is empty
/// (we don't promote focus to a blank pane). Returns `None` to leave focus
/// untouched.
pub(crate) fn focus_for_region_click(
    region: HitRegion,
    has_transcript_content: bool,
) -> Option<AppFocus> {
    match region {
        HitRegion::Prompt => Some(AppFocus::Prompt),
        HitRegion::Transcript if has_transcript_content => Some(AppFocus::Content),
        _ => None,
    }
}

fn is_prompt_chrome_window(win: &crate::smelt_edit::Window) -> bool {
    matches!(win.config.region.as_str(), "prompt_above" | "prompt_below")
}

fn is_prompt_resize_handle(win: &crate::smelt_edit::Window) -> bool {
    win.config.region == "prompt_above"
}

fn projected_buf_breaks(buf: &crate::smelt_edit::Buffer) -> (Vec<usize>, Vec<usize>) {
    let lines = buf.lines();
    let mut soft = Vec::new();
    let mut hard = Vec::new();
    let mut pos = 0usize;

    for (row, line) in lines.iter().enumerate() {
        pos += line.len();
        if row + 1 == lines.len() {
            break;
        }
        if buf.decoration_at(row + 1).soft_wrapped {
            soft.push(pos);
        } else {
            hard.push(pos);
        }
        pos += 1;
    }

    (soft, hard)
}

enum TranscriptScrollInputResolution {
    UseCapturedIntent,
    UseResolvedScrollAfterDispatch,
}

struct TranscriptScrollInputCandidate {
    label: String,
    intent: TranscriptScrollIntent,
    window_scroll_before: RowIndex,
    resolution: TranscriptScrollInputResolution,
}

impl TuiApp {
    pub(crate) fn scroll_at_with_transcript_intent(
        &mut self,
        row: u16,
        col: u16,
        delta: isize,
        label: impl Into<String>,
    ) -> bool {
        let candidate = self.transcript_window_at(row, col).and_then(|win| {
            (win == crate::app::TRANSCRIPT_WIN).then(|| TranscriptScrollInputCandidate {
                label: label.into(),
                intent: TranscriptScrollIntent::UserDelta { rows: delta },
                window_scroll_before: self.transcript_scroll_top(),
                resolution: TranscriptScrollInputResolution::UseCapturedIntent,
            })
        });
        if candidate.is_some() {
            self.record_transcript_scroll_input(candidate);
            return true;
        }
        let panned = self.ui.scroll_at(row, col, delta);
        if panned {
            self.record_transcript_scroll_input(candidate);
        }
        panned
    }

    pub(crate) fn tick_drag_autoscroll_with_transcript_intent(&mut self) -> bool {
        let Some((win, delta)) = self.ui.begin_drag_autoscroll_tick() else {
            return false;
        };
        if win != crate::app::TRANSCRIPT_WIN {
            return self.ui.tick_drag_autoscroll();
        }

        let candidate = TranscriptScrollInputCandidate {
            label: "drag_autoscroll".to_string(),
            intent: TranscriptScrollIntent::UserDelta { rows: delta },
            window_scroll_before: self.transcript_scroll_top(),
            resolution: TranscriptScrollInputResolution::UseCapturedIntent,
        };
        let Some(restore) = self.advance_transcript_drag_autoscroll_endpoint(delta) else {
            return false;
        };
        self.record_transcript_scroll_intent_for_projection(
            candidate.label,
            candidate.intent,
            candidate.window_scroll_before,
            restore,
            None,
            None,
        );
        true
    }

    fn advance_transcript_drag_autoscroll_endpoint(
        &mut self,
        delta: isize,
    ) -> Option<TranscriptProjectionRestore> {
        let (viewport_rows, scroll_top, total_rows, mut state) = {
            let win = self.ui.win(crate::app::TRANSCRIPT_WIN)?;
            let viewport = win.viewport?;
            let state = win.document_view_state();
            if !state.active {
                return None;
            }
            (
                viewport.rect.height,
                win.scroll_top(),
                state.materialized.total_rows,
                state,
            )
        };
        if viewport_rows == 0 || total_rows == 0 || delta == 0 {
            return None;
        }
        let max_scroll = total_rows.saturating_sub(RowIndex::from(viewport_rows.max(1)));
        if (delta < 0 && scroll_top == 0) || (delta > 0 && scroll_top >= max_scroll) {
            return None;
        }

        if state.selection_anchor.is_none() {
            state.selection_anchor = state.drag_endpoint.or(Some(state.cursor));
            state.selection_includes_cursor_cell = true;
        }
        if state.drag_endpoint.is_none() {
            state.drag_endpoint = Some(state.cursor);
        }
        if let Some(win) = self.ui.win_mut(crate::app::TRANSCRIPT_WIN) {
            win.set_document_view_state(state);
        }
        let edge_row = if delta < 0 {
            0
        } else {
            viewport_rows.saturating_sub(1)
        };
        Some(TranscriptProjectionRestore {
            cursor_screen_row: None,
            drag_endpoint_screen_row: Some(edge_row),
        })
    }

    pub(crate) fn transcript_scroll_top(&self) -> RowIndex {
        self.ui
            .win(crate::app::TRANSCRIPT_WIN)
            .map(|win| win.scroll_top())
            .unwrap_or(0)
    }

    fn transcript_window_at(&self, row: u16, col: u16) -> Option<WinId> {
        match self.ui.hit_test(row, col, None)? {
            HitTarget::Window(win) | HitTarget::Scrollbar { owner: win } => Some(win),
            HitTarget::Paint(_) | HitTarget::Chrome { .. } => None,
        }
    }

    fn transcript_wheel_input_candidate(
        &self,
        me: MouseEvent,
    ) -> Option<TranscriptScrollInputCandidate> {
        let rows = match me.kind {
            MouseEventKind::ScrollUp => -3,
            MouseEventKind::ScrollDown => 3,
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => return None,
            _ => return None,
        };
        (self.transcript_window_at(me.row, me.column)? == crate::app::TRANSCRIPT_WIN).then(|| {
            TranscriptScrollInputCandidate {
                label: match me.kind {
                    MouseEventKind::ScrollUp => "wheel_up".to_string(),
                    MouseEventKind::ScrollDown => "wheel_down".to_string(),
                    _ => unreachable!(),
                },
                intent: TranscriptScrollIntent::UserDelta { rows },
                window_scroll_before: self.transcript_scroll_top(),
                resolution: TranscriptScrollInputResolution::UseCapturedIntent,
            }
        })
    }

    fn transcript_scrollbar_input_candidate(
        &self,
        me: MouseEvent,
        cap_before: Option<HitTarget>,
    ) -> Option<TranscriptScrollInputCandidate> {
        let owner = match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                match self.ui.hit_test(me.row, me.column, None)? {
                    HitTarget::Scrollbar { owner } => owner,
                    _ => return None,
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => match cap_before? {
                HitTarget::Scrollbar { owner } => owner,
                _ => return None,
            },
            _ => return None,
        };
        if owner != crate::app::TRANSCRIPT_WIN {
            return None;
        }
        let intent = self
            .transcript_scrollbar_fraction(me.row)
            .unwrap_or_else(|| {
                TranscriptScrollIntent::ApproximateRowSeek(self.transcript_scroll_top())
            });
        let resolution = if matches!(me.kind, MouseEventKind::Drag(MouseButton::Left)) {
            TranscriptScrollInputResolution::UseResolvedScrollAfterDispatch
        } else {
            TranscriptScrollInputResolution::UseCapturedIntent
        };
        Some(TranscriptScrollInputCandidate {
            label: "scrollbar".to_string(),
            intent,
            window_scroll_before: self.transcript_scroll_top(),
            resolution,
        })
    }

    fn transcript_scrollbar_fraction(&self, row: u16) -> Option<TranscriptScrollIntent> {
        let win = self.ui.win(crate::app::TRANSCRIPT_WIN)?;
        let viewport = win.viewport?;
        let bar = viewport.scrollbar?;
        let rel_row = row.saturating_sub(viewport.rect.top);
        let numerator = u64::from(bar.thumb_top_for_click(rel_row));
        let denominator = u64::from(bar.max_thumb_top().max(1));
        Some(TranscriptScrollIntent::ScrollbarFraction {
            numerator,
            denominator,
        })
    }

    fn record_transcript_scroll_input(
        &mut self,
        candidate: Option<TranscriptScrollInputCandidate>,
    ) {
        let Some(candidate) = candidate else {
            return;
        };
        let intent = match candidate.resolution {
            TranscriptScrollInputResolution::UseCapturedIntent => candidate.intent,
            TranscriptScrollInputResolution::UseResolvedScrollAfterDispatch => {
                TranscriptScrollIntent::ApproximateRowSeek(self.transcript_scroll_top())
            }
        };
        self.record_transcript_scroll_intent_for_projection(
            candidate.label,
            intent,
            candidate.window_scroll_before,
            TranscriptProjectionRestore::default(),
            None,
            None,
        );
    }

    // ── Mouse event dispatch ─────────────────────────────────────────────
    pub(crate) fn handle_mouse(&mut self, me: MouseEvent) -> EventOutcome {
        if self.update_prompt_resize_drag(me) {
            return EventOutcome::Redraw;
        }

        // `Ui::dispatch_event` handles wheel-over-overlay, modal click-outside absorb,
        // and scrollbar drag. Anything unclaimed (`Ignored`) flows through below.
        let cap_before = self.ui.capture();
        let scroll_input = self
            .transcript_wheel_input_candidate(me)
            .or_else(|| self.transcript_scrollbar_input_candidate(me, cap_before));
        if is_scroll_event(me.kind)
            && matches!(
                scroll_input.as_ref().map(|input| &input.intent),
                Some(TranscriptScrollIntent::UserDelta { .. })
            )
        {
            self.record_transcript_scroll_input(scroll_input);
            self.pin_well_known_horizontal_scroll();
            crate::picker::sync_scrolled(self);
            return EventOutcome::Redraw;
        }
        if matches!(
            self.ui
                .dispatch_event(crate::smelt_edit::Event::Mouse(me), &mut |_, _, _| {}),
            crate::smelt_edit::Status::Consumed
        ) {
            self.record_transcript_scroll_input(scroll_input);
            if is_scroll_event(me.kind) {
                self.pin_well_known_horizontal_scroll();
            }
            if let Some(owner) =
                scrollbar_owner_from_capture_transition(cap_before, self.ui.capture())
            {
                // While a modal is open, the modal keeps focus - scrolling a
                // background pane's scrollbar must not steal it.
                if is_left_down(me.kind) && self.ui.active_modal().is_none() {
                    if let Some(focus) = app_focus_for_owner(owner) {
                        self.app_focus = focus;
                    }
                }
                return EventOutcome::Redraw;
            }
            return if is_scroll_event(me.kind) {
                crate::picker::sync_scrolled(self);
                EventOutcome::Redraw
            } else {
                EventOutcome::Noop
            };
        }

        // Wheel scroll is handled generically by `Ui::dispatch_event` - when it
        // falls through here, the wheel didn't land on a scrollable leaf and is
        // safe to drop.
        if is_scroll_event(me.kind) {
            return EventOutcome::Noop;
        }

        // Region-based focus promotion on Down: the prompt block (top bar +
        // input + bottom bar) is one focus unit, and the transcript pane is
        // another. Doing this BEFORE per-window dispatch means click handlers
        // on individual leaves don't have to know about app-level focus, and
        // visually-grouped chrome (prompt's top/bottom bars) inherits the
        // input's focus naturally.
        //
        // Skip promotion when an overlay covers the point - overlays paint above
        // splits, so clicking a pill / notification / picker should not shift
        // app_focus to the underlying pane even when the overlay leaf is not
        // focusable or selectable.
        if is_left_down(me.kind) && self.ui.active_modal().is_none() {
            let hit_target = self.ui.hit_test(me.row, me.column, None);
            let hits_overlay = hit_target.is_some_and(|ht| match ht {
                HitTarget::Window(w) => self.ui.overlay_for_leaf(w).is_some(),
                HitTarget::Paint(p) => self.ui.overlay_for_paint(p).is_some(),
                HitTarget::Chrome { .. } => true,
                _ => false,
            });
            if !hits_overlay {
                let region = self.layout.hit_test(me.row, me.column);
                let has_content = self.has_transcript_content();
                if let Some(focus) = focus_for_region_click(region, has_content) {
                    self.app_focus = focus;
                    if matches!(focus, AppFocus::Prompt)
                        && hit_target.is_some_and(|ht| match ht {
                            HitTarget::Window(w) => {
                                self.ui.win(w).is_some_and(is_prompt_chrome_window)
                            }
                            _ => false,
                        })
                    {
                        self.ui.set_focus(crate::app::PROMPT_WIN);
                    }
                }
            }
        }

        // `Ui::resolve_split_mouse` handles hit-test, click-count, and HitTarget capture
        // so drags stay on the originating window even if the pointer drifts.
        let now = self.core.clock.instant_now();
        if let Some((target, count)) = self.ui.resolve_split_mouse(me, now) {
            let is_down = is_left_down(me.kind);
            let is_up = matches!(me.kind, MouseEventKind::Up(MouseButton::Left));
            let is_drag = matches!(me.kind, MouseEventKind::Drag(MouseButton::Left));
            // Fire Press/Release/Drag on the captured leaf with leaf-relative coords.
            // Built-in transcript / input / list handling below still runs - pointer
            // events are purely additive for Lua subscribers.
            let pointer_event = if is_down {
                Some(crate::smelt_edit::WinEvent::Press)
            } else if is_up {
                Some(crate::smelt_edit::WinEvent::Release)
            } else if is_drag {
                Some(crate::smelt_edit::WinEvent::Drag)
            } else {
                None
            };
            if let Some(ev) = pointer_event {
                self.fire_pointer_event(target, ev, me, crate::smelt_edit::MouseButton::Left);
            }
            let HitTarget::Window(win) = target else {
                return EventOutcome::Redraw;
            };
            if win == crate::app::PROMPT_WIN {
                if is_down && self.ui.active_modal().is_none() {
                    self.ui.set_focus(win);
                }
                self.handle_prompt_mouse(me, count);
            } else if win == crate::app::TRANSCRIPT_WIN {
                // Empty transcript: don't start a drag-select on the void.
                if is_down && !self.has_transcript_content() {
                    return EventOutcome::Noop;
                }
                if is_down && self.ui.active_modal().is_none() {
                    self.ui.set_focus(win);
                }
                if (is_down || is_drag)
                    && self.ui.win(win).is_some_and(|win| win.is_following_tail())
                {
                    if let Some(win) = self.ui.win_mut(win) {
                        win.pin_current_scroll();
                    }
                }
                if is_down && me.modifiers.contains(KeyModifiers::CONTROL) {
                    if let Some(pos) = self.document_view_position_at_mouse_for_win(win, me) {
                        if let Some(action) = self.document_action_at(win, pos) {
                            self.dispatch_span_action(action);
                            return EventOutcome::Redraw;
                        }
                    }
                }
                let yank = self
                    .handle_document_view_mouse_for_win(win, me, count, now)
                    .1;
                if is_up {
                    if let Some(out) = yank {
                        self.yank_to_clipboard(out);
                    }
                }
            } else if self
                .ui
                .win(win)
                .is_some_and(|w| w.supports_text_selection())
            {
                if is_down
                    && self.ui.active_modal().is_none()
                    && self.ui.win(win).is_some_and(is_prompt_chrome_window)
                {
                    self.app_focus = AppFocus::Prompt;
                    self.ui.set_focus(crate::app::PROMPT_WIN);
                }
                // Generic selectable leaf: notifications, dialog bodies, future popups.
                // On Down, promote keyboard focus to this leaf if it's focusable -
                // overlays with multiple leaves (e.g. side-by-side panes) need click
                // to follow keyboard focus, not just the first leaf the overlay opened.
                // A non-focusable selectable leaf must not steal app_focus.
                let (status, yank) = if self.win_uses_document_view(win) {
                    self.handle_document_view_mouse_for_win(win, me, count, now)
                } else {
                    self.handle_selectable_leaf_mouse(win, me, count)
                };
                if matches!(status, crate::smelt_edit::Status::Ignored) {
                    let is_resize_handle = self.ui.win(win).is_some_and(is_prompt_resize_handle);
                    if is_down && is_resize_handle && self.ui.active_modal().is_none() {
                        if self.handle_prompt_resize_click(me) {
                            return EventOutcome::Redraw;
                        }
                        self.start_prompt_resize_drag(me);
                        return EventOutcome::Redraw;
                    }
                    if is_down {
                        self.ui.cancel_pointer_interaction();
                    }
                    return if self.ui.win(win).is_some_and(is_prompt_chrome_window) {
                        EventOutcome::Redraw
                    } else {
                        EventOutcome::Noop
                    };
                }
                if is_down
                    && self
                        .ui
                        .win(win)
                        .is_some_and(|w| w.accepts_focus() && !is_prompt_chrome_window(w))
                {
                    self.ui.set_focus(win);
                }
                if is_up {
                    if let Some(out) = yank {
                        self.yank_to_clipboard(out);
                    }
                }
            } else if is_down {
                if self.ui.win(win).is_some_and(is_prompt_chrome_window)
                    && self.ui.active_modal().is_none()
                {
                    self.app_focus = AppFocus::Prompt;
                    self.ui.set_focus(crate::app::PROMPT_WIN);
                    self.ui.cancel_pointer_interaction();
                    return EventOutcome::Redraw;
                }
                self.ui.cancel_pointer_interaction();
                return EventOutcome::Noop;
            }
            return EventOutcome::Redraw;
        }

        EventOutcome::Noop
    }

    fn update_prompt_resize_drag(&mut self, me: MouseEvent) -> bool {
        let Some(mut drag) = self.prompt_resize_drag else {
            return false;
        };
        match me.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                self.prompt_resize_last_click = None;
                drag.dragged = true;
                self.set_prompt_resize_drag(Some(drag));
                let delta = drag.start_row as i32 - me.row as i32;
                let max_rows =
                    Self::max_manual_prompt_input_rows_for(self.ui.terminal_size().1) as i32;
                let rows = (drag.start_input_rows as i32 + delta).clamp(1, max_rows) as u16;
                self.prompt_input_rows_override = Some(rows);
                self.prompt_input_rows = rows;
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.set_prompt_resize_drag(None);
                if drag.dragged && self.prompt_input_rows_override == Some(1) {
                    self.reset_prompt_input_rows();
                }
                true
            }
            _ => true,
        }
    }

    fn handle_prompt_resize_click(&mut self, me: MouseEvent) -> bool {
        let now = self.core.clock.instant_now();
        let double_click = self.prompt_resize_last_click.is_some_and(|last| {
            last.row == me.row
                && last.col == me.column
                && now.saturating_duration_since(last.at) <= PROMPT_RESIZE_DOUBLE_CLICK_WINDOW
        });
        if double_click {
            self.reset_prompt_input_rows();
            return true;
        }
        self.prompt_resize_last_click = Some(PromptResizeClick {
            row: me.row,
            col: me.column,
            at: now,
        });
        false
    }

    fn reset_prompt_input_rows(&mut self) {
        self.prompt_input_rows_override = None;
        self.set_prompt_resize_drag(None);
        self.prompt_resize_last_click = None;
    }

    fn start_prompt_resize_drag(&mut self, me: MouseEvent) {
        self.set_prompt_resize_drag(Some(PromptResizeDrag {
            chrome: "top",
            start_row: me.row,
            start_input_rows: self.prompt_input_rows.max(1),
            dragged: false,
        }));
        self.app_focus = AppFocus::Prompt;
        self.ui.set_focus(crate::app::PROMPT_WIN);
        self.ui.cancel_pointer_interaction();
    }

    fn pin_well_known_horizontal_scroll(&mut self) {
        for win_id in [crate::app::PROMPT_WIN, crate::app::TRANSCRIPT_WIN] {
            if let Some(win) = self.ui.win_mut(win_id) {
                win.scroll_left = 0;
            }
        }
    }

    /// Fire a pointer `WinEvent` (`Press`/`Release`/`Drag`) on `target` with
    /// leaf-relative cell coords. Coords are clamped to `(0, 0)` when the leaf
    /// has no live viewport (the event landed during a hit-test stale frame).
    fn fire_pointer_event(
        &mut self,
        target: HitTarget,
        event: crate::smelt_edit::WinEvent,
        me: MouseEvent,
        button: crate::smelt_edit::MouseButton,
    ) {
        let (leaf, rect) = match target {
            HitTarget::Window(win) => {
                let rect = self
                    .ui
                    .win(win)
                    .and_then(|w| w.viewport)
                    .map(|v| v.rect)
                    .or_else(|| self.ui.paint_rect(crate::smelt_edit::PaintId::from(win)));
                (win, rect)
            }
            HitTarget::Paint(paint) => {
                let rect = self.ui.paint_rect(paint);
                (WinId(paint.0), rect)
            }
            HitTarget::Scrollbar { .. } | HitTarget::Chrome { .. } => return,
        };
        let (rel_row, rel_col) = match rect {
            Some(rect) => (
                me.row.saturating_sub(rect.top),
                me.column.saturating_sub(rect.left),
            ),
            None => (0, 0),
        };
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              w: WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, w, payload);
        };
        self.ui.fire_win_event(
            leaf,
            event,
            crate::smelt_edit::Payload::Mouse {
                row: rel_row,
                col: rel_col,
                button,
            },
            &mut lua_invoke,
        );
    }

    pub(crate) fn yank_to_clipboard(&mut self, out: crate::smelt_edit::CopyOutput) {
        if out.clipboard.is_empty() && out.kill_ring.is_empty() {
            return;
        }
        if !out.clipboard.is_empty() && self.core.clipboard.write(&out.clipboard).is_ok() {
            self.core
                .clipboard
                .kill_ring
                .record_clipboard_write(out.clipboard);
        }
        if !out.kill_ring.is_empty() {
            self.core
                .clipboard
                .kill_ring
                .set_with_linewise(out.kill_ring, false);
        }
    }

    /// Drive a prompt mouse event through `Window::handle_mouse`. On `Up` with
    /// a non-empty selection, route the yanked range through `buf.copy_range`
    /// (the prompt's `BufferCopy` expands `\u{FFFC}` markers to `[label]` for
    /// the system clipboard while keeping raw markers in the kill ring for
    /// vim paste-back).
    fn handle_prompt_mouse(&mut self, me: MouseEvent, click_count: u8) {
        let Some(vp) = crate::smelt_edit::UiHost::viewport_for(self, crate::app::PROMPT_WIN) else {
            return;
        };
        let usable = vp.content_width as usize;
        let hard = {
            let Some(buf_id) = self.ui.win(crate::app::PROMPT_WIN).map(|win| win.buf) else {
                return;
            };
            let Some(buf) = self.ui.buf(buf_id) else {
                return;
            };
            crate::smelt_edit::hard_breaks_for_text(buf.source())
        };
        let soft = Vec::new();
        let yank = {
            let (win, buf) = self
                .ui
                .win_and_buf_mut(crate::app::PROMPT_WIN, crate::app::PROMPT_EDIT_BUF);
            let buf = buf.expect("prompt edit buffer");
            let win = win.expect("prompt window");
            buf.ensure_rendered_at(usable as u16);
            let mouse_ctx = crate::smelt_edit::MouseCtx {
                soft_breaks: &soft,
                hard_breaks: &hard,
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
    /// leaf that supports text selection. Edit/window owns chrome-hit policy;
    /// this host path only dispatches the event and applies copied text on `Up`.
    fn handle_selectable_leaf_mouse(
        &mut self,
        win: crate::smelt_edit::WinId,
        me: MouseEvent,
        click_count: u8,
    ) -> (
        crate::smelt_edit::Status,
        Option<crate::smelt_edit::CopyOutput>,
    ) {
        let Some(viewport) = crate::smelt_edit::UiHost::viewport_for(self, win) else {
            return (crate::smelt_edit::Status::Ignored, None);
        };
        let Some(buf_id) = self.ui.win(win).map(|w| w.buf) else {
            return (crate::smelt_edit::Status::Ignored, None);
        };
        let (soft, hard) = self
            .ui
            .buf(buf_id)
            .map(projected_buf_breaks)
            .unwrap_or_default();
        let (status, range) = {
            let mouse_ctx = crate::smelt_edit::MouseCtx {
                soft_breaks: &soft,
                hard_breaks: &hard,
                viewport,
                click_count,
            };
            let (win_mut, buf_mut) = self.ui.win_and_buf_mut(win, buf_id);
            match (win_mut, buf_mut) {
                (Some(win_mut), Some(buf_mut)) => win_mut.handle_mouse(buf_mut, me, mouse_ctx),
                _ => return (crate::smelt_edit::Status::Ignored, None),
            }
        };
        let Some(range) = range else {
            return (status, None);
        };
        let Some(buf) = self.ui.buf(buf_id) else {
            return (status, None);
        };
        let text = smelt_buffer::coords::copy_byte_range(buf, range.0, range.1);
        let out = crate::smelt_edit::CopyOutput::same(text);
        let out = if out.is_empty() { None } else { Some(out) };
        (status, out)
    }

    /// Drive a transcript-pane mouse event through `Window::handle_mouse`.
    /// On `MouseUp`, returns the yanked range as `CopyOutput` via `Buffer::copy_range` -
    /// the transcript's `BufferCopy` impl walks the latest snapshot so `copy_as`
    /// substitutions, soft-wrap merging, and non-selectable cells are honored.
    #[cfg(test)]
    fn handle_content_mouse(
        &mut self,
        me: MouseEvent,
        click_count: u8,
    ) -> Option<crate::smelt_edit::CopyOutput> {
        let viewport = crate::smelt_edit::UiHost::viewport_for(self, crate::app::TRANSCRIPT_WIN)?;
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        let needs_breaks = match me.kind {
            MouseEventKind::Down(_) => click_count >= 2,
            MouseEventKind::Drag(_) => {
                let w = self.transcript_win();
                w.has_drag_anchor()
            }
            _ => false,
        };
        let (has_rows, soft, hard) = {
            let buf = self.ui.buf(buf_id)?;
            let has_rows = !buf.lines().is_empty();
            let (soft, hard) = if needs_breaks {
                projected_buf_breaks(buf)
            } else {
                (Vec::new(), Vec::new())
            };
            (has_rows, soft, hard)
        };
        if !has_rows {
            return None;
        }

        if matches!(me.kind, MouseEventKind::Down(MouseButton::Left))
            && me.modifiers.contains(KeyModifiers::CONTROL)
        {
            let pos = {
                let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
                win.expect("transcript window")
                    .viewer_doc_pos_at_mouse(buf?, me, viewport)
            }?;
            let action = self.document_action_at(win_id, pos);
            if let Some(action) = action {
                self.dispatch_span_action(action);
                return None;
            }
        }

        if self.transcript_win().has_materialized_rows() {
            let now = self.core.clock.instant_now();
            let range = {
                let mouse_ctx = crate::smelt_edit::MouseCtx {
                    soft_breaks: &soft,
                    hard_breaks: &hard,
                    viewport,
                    click_count,
                };
                let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
                let (_, range) = win
                    .expect("transcript window")
                    .handle_row_mouse(buf?, me, mouse_ctx, now);
                range?
            };
            let out = self.copy_document_rows(win_id, range)?;
            return if out.clipboard.is_empty() && out.kill_ring.is_empty() {
                None
            } else {
                Some(out)
            };
        }

        // Breaks only matter for word/line selection and word/line-anchored drags.
        // They must come from the same projected buffer that mouse selection reads;
        // rematerializing full transcript breaks can describe a different generation
        // and produce stale byte offsets.

        let range = {
            let mouse_ctx = crate::smelt_edit::MouseCtx {
                soft_breaks: &soft,
                hard_breaks: &hard,
                viewport,
                click_count,
            };
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let (_, range) = win
                .expect("transcript window")
                .handle_mouse(buf?, me, mouse_ctx);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_edit::HitTarget;

    fn ev(kind: MouseEventKind) -> MouseEventKind {
        kind
    }

    // ── is_scroll_event ──────────────────────────────────────────────────

    #[test]
    fn is_scroll_event_covers_all_four_wheel_directions() {
        assert!(is_scroll_event(ev(MouseEventKind::ScrollUp)));
        assert!(is_scroll_event(ev(MouseEventKind::ScrollDown)));
        assert!(is_scroll_event(ev(MouseEventKind::ScrollLeft)));
        assert!(is_scroll_event(ev(MouseEventKind::ScrollRight)));
    }

    #[test]
    fn is_scroll_event_rejects_click_and_drag_events() {
        assert!(!is_scroll_event(ev(MouseEventKind::Down(
            MouseButton::Left
        ))));
        assert!(!is_scroll_event(ev(MouseEventKind::Up(MouseButton::Left))));
        assert!(!is_scroll_event(ev(MouseEventKind::Drag(
            MouseButton::Left
        ))));
        assert!(!is_scroll_event(ev(MouseEventKind::Moved)));
    }

    // ── is_left_down ─────────────────────────────────────────────────────

    #[test]
    fn is_left_down_matches_left_button_press_only() {
        assert!(is_left_down(MouseEventKind::Down(MouseButton::Left)));
        assert!(!is_left_down(MouseEventKind::Up(MouseButton::Left)));
        assert!(!is_left_down(MouseEventKind::Down(MouseButton::Right)));
        assert!(!is_left_down(MouseEventKind::Down(MouseButton::Middle)));
        assert!(!is_left_down(MouseEventKind::Drag(MouseButton::Left)));
    }

    // ── scrollbar_owner_from_capture_transition ─────────────────────────

    /// A WinId guaranteed not to collide with `PROMPT_WIN`/`TRANSCRIPT_WIN`,
    /// for testing the "unknown owner" branch of `app_focus_for_owner`.
    fn other_win() -> WinId {
        WinId(9999)
    }

    #[test]
    fn scrollbar_owner_is_none_when_neither_capture_was_a_scrollbar() {
        assert_eq!(scrollbar_owner_from_capture_transition(None, None), None);
    }

    #[test]
    fn scrollbar_owner_recovers_from_before_when_after_is_cleared() {
        // `Up` clears capture but we still want to know who owned the drag.
        let owner = other_win();
        let before = Some(HitTarget::Scrollbar { owner });
        assert_eq!(
            scrollbar_owner_from_capture_transition(before, None),
            Some(owner)
        );
    }

    #[test]
    fn scrollbar_owner_picks_up_after_when_before_was_empty() {
        // `Down` sets capture for the first time; `before` was `None`.
        let owner = other_win();
        let after = Some(HitTarget::Scrollbar { owner });
        assert_eq!(
            scrollbar_owner_from_capture_transition(None, after),
            Some(owner)
        );
    }

    // ── app_focus_for_owner ──────────────────────────────────────────────

    #[test]
    fn app_focus_for_owner_maps_transcript_window_to_content_focus() {
        assert_eq!(
            app_focus_for_owner(crate::app::TRANSCRIPT_WIN),
            Some(AppFocus::Content)
        );
    }

    #[test]
    fn app_focus_for_owner_maps_prompt_window_to_prompt_focus() {
        assert_eq!(
            app_focus_for_owner(crate::app::PROMPT_WIN),
            Some(AppFocus::Prompt)
        );
    }

    #[test]
    fn app_focus_for_owner_returns_none_for_unrelated_windows() {
        assert_eq!(app_focus_for_owner(other_win()), None);
    }

    // ── focus_for_region_click ───────────────────────────────────────────

    #[test]
    fn region_click_on_prompt_always_promotes_to_prompt_focus() {
        assert_eq!(
            focus_for_region_click(HitRegion::Prompt, true),
            Some(AppFocus::Prompt)
        );
        assert_eq!(
            focus_for_region_click(HitRegion::Prompt, false),
            Some(AppFocus::Prompt)
        );
    }

    #[test]
    fn region_click_on_transcript_with_content_promotes_to_content_focus() {
        assert_eq!(
            focus_for_region_click(HitRegion::Transcript, true),
            Some(AppFocus::Content)
        );
    }

    #[test]
    fn region_click_on_empty_transcript_does_not_steal_focus() {
        // No content means there's nothing to focus into - leave the user
        // in the prompt.
        assert_eq!(focus_for_region_click(HitRegion::Transcript, false), None);
    }

    #[test]
    fn region_click_outside_does_not_change_focus() {
        assert_eq!(focus_for_region_click(HitRegion::Outside, true), None);
    }

    fn left_down(row: u16, column: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn left_drag(row: u16, column: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            row,
            column,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn left_up(row: u16, column: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            row,
            column,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn app_with_seeded_prompt_top_bar() -> crate::app::TuiApp {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.push_block(smelt_core::Block::Text {
            content: "content focus target".into(),
        });
        app.app_focus = AppFocus::Content;
        app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
        app.render_normal_to(&mut std::io::sink());

        let top = app
            .ui
            .named_win("smelt.prompt_bar.top")
            .expect("prompt top bar window");
        app.ui.set_terminal_size(20, 2);
        app.ui.set_layout(crate::smelt_edit::LayoutTree::vbox(vec![
            (
                crate::smelt_edit::Constraint::Length(1),
                crate::smelt_edit::LayoutTree::leaf(top),
            ),
            (
                crate::smelt_edit::Constraint::Fill,
                crate::smelt_edit::LayoutTree::leaf(crate::app::TRANSCRIPT_WIN),
            ),
        ]));
        let vp = crate::smelt_edit::WindowViewport::new(
            crate::smelt_edit::Rect::new(0, 0, 20, 1),
            20,
            1,
            0,
            None,
        );
        let buf_id = app.ui.win(top).expect("top bar win").buf;
        {
            let buf = app.ui.buf_mut(buf_id).expect("top bar buf");
            buf.set_all_lines(vec!["abc----xyz".into()]);
            buf.add_highlight_group_with_meta(
                0,
                3,
                7,
                smelt_core::theme::intern("Normal"),
                smelt_core::buffer::SpanMeta::unselectable(),
            );
        }
        {
            let win = app.ui.win_mut(top).expect("top bar win");
            win.viewport = Some(vp);
            win.set_surface(crate::smelt_edit::WindowSurface::selectable_text());
        }
        app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
        app
    }

    #[test]
    fn paint_leaf_press_keeps_capture_until_release() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.ui.set_terminal_size(20, 10);
        let (paint, _) = app
            .paint_registry
            .register(1, Some("test.paint_leaf_mouse".into()));
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let events = events.clone();
            app.ui.leaf_on_event(
                paint,
                crate::smelt_edit::WinEvent::Press,
                crate::smelt_edit::Callback::Rust(Box::new(move |_| {
                    events.borrow_mut().push("press");
                    crate::smelt_edit::CallbackResult::Consumed
                })),
            );
        }
        {
            let events = events.clone();
            app.ui.leaf_on_event(
                paint,
                crate::smelt_edit::WinEvent::Release,
                crate::smelt_edit::Callback::Rust(Box::new(move |_| {
                    events.borrow_mut().push("release");
                    crate::smelt_edit::CallbackResult::Consumed
                })),
            );
        }
        app.ui.overlay_open(
            crate::smelt_edit::Overlay::new(
                crate::smelt_edit::LayoutTree::leaf(paint),
                crate::smelt_edit::layout::Anchor::ScreenCenter,
            )
            .with_size((4, 2)),
        );
        let rect = app.ui.paint_rect(paint).expect("paint leaf rect");

        app.handle_mouse(left_down(rect.top, rect.left));
        assert_eq!(app.ui.capture(), Some(HitTarget::Paint(paint)));
        app.handle_mouse(left_up(
            rect.top + rect.height + 1,
            rect.left + rect.width + 1,
        ));

        assert_eq!(app.ui.capture(), None);
        assert_eq!(&*events.borrow(), &["press", "release"]);
    }

    #[test]
    fn prompt_top_bar_chrome_click_focuses_prompt_without_selecting() {
        let mut app = app_with_seeded_prompt_top_bar();
        let down = left_down(0, 4);

        app.handle_mouse(down);

        assert_eq!(app.app_focus, AppFocus::Prompt);
        assert_eq!(app.ui.focus(), Some(crate::app::TRANSCRIPT_WIN));
        assert_eq!(app.ui.capture(), None);
        assert!(app.core.clipboard.kill_ring.current().is_empty());
    }

    #[test]
    fn prompt_top_bar_selectable_text_drag_copies_and_focuses_prompt() {
        let mut app = app_with_seeded_prompt_top_bar();

        app.handle_mouse(left_down(0, 0));
        app.handle_mouse(left_drag(0, 3));
        app.handle_mouse(left_up(0, 3));

        assert_eq!(app.app_focus, AppFocus::Prompt);
        assert_eq!(app.ui.focus(), Some(crate::app::TRANSCRIPT_WIN));
        assert_eq!(app.prompt_input_rows_override, None);
        assert_eq!(app.core.clipboard.kill_ring.current(), "abc");
    }

    fn prompt_resize_handle_cell(app: &mut crate::app::TuiApp) -> (u16, u16) {
        app.render_normal_to(&mut std::io::sink());
        let top = app
            .ui
            .named_win("smelt.prompt_bar.top")
            .expect("prompt top bar window");
        let vp = app
            .ui
            .win(top)
            .and_then(|w| w.viewport)
            .expect("prompt top viewport");
        let width = vp.content_width.clamp(1, 8);
        let buf_id = app.ui.win(top).expect("top bar win").buf;
        {
            let buf = app.ui.buf_mut(buf_id).expect("top bar buf");
            buf.set_all_lines(vec!["─".repeat(width as usize)]);
            buf.add_highlight_group_with_meta(
                0,
                0,
                width,
                smelt_core::theme::intern("SmeltBar"),
                smelt_core::buffer::SpanMeta::unselectable(),
            );
        }
        (vp.rect.top, vp.rect.left)
    }

    #[test]
    fn prompt_resize_state_publishes_target_chrome() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;

        app.set_prompt_resize_drag(Some(PromptResizeDrag {
            chrome: "bottom",
            start_row: 0,
            start_input_rows: 1,
            dragged: false,
        }));
        assert_eq!(
            app.core.signals.get::<String>("prompt_resize_chrome"),
            Some("bottom".to_string())
        );

        app.set_prompt_resize_drag(Some(PromptResizeDrag {
            chrome: "both",
            start_row: 0,
            start_input_rows: 1,
            dragged: false,
        }));
        assert_eq!(
            app.core.signals.get::<String>("prompt_resize_chrome"),
            Some("both".to_string())
        );

        app.set_prompt_resize_drag(None);
        assert_eq!(
            app.core.signals.get::<String>("prompt_resize_chrome"),
            Some(String::new())
        );
    }

    #[test]
    fn prompt_top_chrome_drag_grows_manual_prompt_rows() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        let (row, col) = prompt_resize_handle_cell(&mut app);
        let start_rows = app.prompt_input_rows;

        app.handle_mouse(left_down(row, col));
        assert_eq!(
            app.core.signals.get::<bool>("prompt_resize_active"),
            Some(true)
        );
        assert_eq!(
            app.core.signals.get::<String>("prompt_resize_chrome"),
            Some("top".to_string())
        );
        app.handle_mouse(left_drag(row.saturating_sub(2), col));
        app.handle_mouse(left_up(row.saturating_sub(2), col));
        assert_eq!(
            app.core.signals.get::<bool>("prompt_resize_active"),
            Some(false)
        );
        assert_eq!(
            app.core.signals.get::<String>("prompt_resize_chrome"),
            Some(String::new())
        );
        app.render_normal_to(&mut std::io::sink());

        let expected = (start_rows + 2).min(crate::app::TuiApp::max_manual_prompt_input_rows_for(
            app.ui.terminal_size().1,
        ));
        assert_eq!(app.prompt_input_rows_override, Some(expected));
        assert_eq!(app.prompt_input_rows, expected);
        assert_eq!(app.prompt_win().viewport.unwrap().rect.height, expected);
    }

    #[test]
    fn prompt_top_chrome_drag_shrinks_manual_prompt_rows() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.prompt_input_rows_override = Some(4);
        let (row, col) = prompt_resize_handle_cell(&mut app);

        app.handle_mouse(left_down(row, col));
        app.handle_mouse(left_drag(row + 2, col));
        app.handle_mouse(left_up(row + 2, col));
        app.render_normal_to(&mut std::io::sink());

        assert_eq!(app.prompt_input_rows_override, Some(2));
        assert_eq!(app.prompt_input_rows, 2);
        assert_eq!(app.prompt_win().viewport.unwrap().rect.height, 2);
    }

    #[test]
    fn prompt_manual_resize_can_exceed_auto_height_cap() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.ui.set_terminal_size(30, 20);
        app.prompt_input_rows_override = Some(13);

        app.render_normal_to(&mut std::io::sink());

        assert_eq!(crate::app::TuiApp::max_auto_prompt_input_rows_for(20), 10);
        assert_eq!(crate::app::TuiApp::max_manual_prompt_input_rows_for(20), 14);
        assert_eq!(app.prompt_input_rows, 13);
        assert_eq!(app.prompt_win().viewport.unwrap().rect.height, 13);
    }

    #[test]
    fn prompt_auto_height_uses_half_screen_cap() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.ui.set_terminal_size(20, 12);
        app.set_placeholder(
            crate::app::PROMPT_WIN,
            "this prediction is deliberately long enough to need far more than six wrapped rows in a narrow prompt input viewport".into(),
        );

        app.render_normal_to(&mut std::io::sink());

        assert_eq!(app.prompt_input_rows, 6);
        assert_eq!(app.prompt_win().viewport.unwrap().rect.height, 6);
    }

    #[test]
    fn prompt_resize_handle_double_click_resets_to_auto_rows() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.prompt_input_rows_override = Some(4);
        let (row, col) = prompt_resize_handle_cell(&mut app);

        app.handle_mouse(left_down(row, col));
        app.handle_mouse(left_up(row, col));
        app.handle_mouse(left_down(row, col));
        app.handle_mouse(left_up(row, col));

        assert_eq!(app.prompt_input_rows_override, None);
    }

    #[test]
    fn prompt_resize_drag_to_minimum_resets_to_auto_rows() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.ui.set_terminal_size(20, 16);
        app.prompt_input_rows_override = Some(4);
        app.set_placeholder(
            crate::app::PROMPT_WIN,
            "this prediction is long enough to wrap across several prompt rows".into(),
        );
        let (row, col) = prompt_resize_handle_cell(&mut app);

        app.handle_mouse(left_down(row, col));
        app.handle_mouse(left_drag(row + 20, col));
        app.handle_mouse(left_up(row + 20, col));
        app.render_normal_to(&mut std::io::sink());

        assert_eq!(app.prompt_input_rows_override, None);
        assert!(app.prompt_input_rows > 1);
    }

    #[test]
    fn manual_prompt_rows_are_exact_even_when_prompt_content_overflows() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.prompt_input_rows_override = Some(1);
        app.ui
            .buf_mut(crate::app::PROMPT_EDIT_BUF)
            .expect("prompt buf")
            .set_source("one\ntwo\nthree".into());

        app.render_normal_to(&mut std::io::sink());

        let viewport = app.prompt_win().viewport.expect("prompt viewport");
        assert_eq!(app.prompt_input_rows, 1);
        assert_eq!(viewport.rect.height, 1);
        assert!(
            viewport.total_rows > viewport.rect.height as crate::smelt_edit::RowIndex,
            "manual height should create a scrollable prompt viewport when content overflows"
        );
    }

    #[test]
    fn prompt_placeholder_wraps_into_auto_prompt_rows() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.ui.set_terminal_size(20, 16);
        app.set_placeholder(
            crate::app::PROMPT_WIN,
            "this prediction is long enough to wrap across several prompt rows".into(),
        );

        app.render_normal_to(&mut std::io::sink());

        let viewport = app.prompt_win().viewport.expect("prompt viewport");
        assert!(app.prompt_input_rows > 1);
        assert_eq!(viewport.rect.height, app.prompt_input_rows);
        assert!(viewport.total_rows > 1);
    }

    #[test]
    fn prompt_placeholder_respects_manual_prompt_rows() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.ui.set_terminal_size(20, 16);
        app.prompt_input_rows_override = Some(1);
        app.set_placeholder(
            crate::app::PROMPT_WIN,
            "this prediction is long enough to wrap across several prompt rows".into(),
        );

        app.render_normal_to(&mut std::io::sink());

        let viewport = app.prompt_win().viewport.expect("prompt viewport");
        assert_eq!(app.prompt_input_rows, 1);
        assert_eq!(viewport.rect.height, 1);
        assert!(viewport.total_rows > 1);
    }

    #[test]
    fn prompt_bottom_bar_click_focuses_prompt() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.push_block(smelt_core::Block::Text {
            content: "content focus target".into(),
        });
        app.app_focus = AppFocus::Content;
        app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
        app.render_normal_to(&mut std::io::sink());

        let bottom = app
            .ui
            .named_win("smelt.prompt_bar.bottom")
            .expect("prompt bottom bar window");
        let vp = app
            .ui
            .win(bottom)
            .and_then(|w| w.viewport)
            .expect("bottom bar viewport");
        let down = left_down(vp.rect.top, vp.rect.left);

        app.handle_mouse(down);

        assert_eq!(app.app_focus, AppFocus::Prompt);
        assert_eq!(app.ui.focus(), Some(crate::app::PROMPT_WIN));
        assert_eq!(app.ui.capture(), None);
        assert!(app.core.clipboard.kill_ring.current().is_empty());
    }

    #[test]
    fn transcript_click_after_tail_render_lands_on_clicked_screen_row() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());
        let rendered_rows = app.ui.buf(app.transcript_win().buf).unwrap().lines();
        assert!(rendered_rows.iter().any(|line| line == "line 99"));
        assert!(
            !rendered_rows.iter().any(|line| line == "line 0"),
            "tail render should materialize a bounded visible slice, not the full transcript"
        );

        let vp = app
            .transcript_win()
            .viewport
            .expect("render populated transcript viewport");
        let click_row = vp.rect.top.saturating_add(3);
        let click_col = vp.rect.left;
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: click_row,
            column: click_col,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..down
        };

        app.handle_mouse(down);
        app.render_normal_to(&mut std::io::sink());
        app.handle_mouse(up);

        let win = app.transcript_win();
        assert_eq!(
            win.cursor_screen_row(vp.rect.height),
            Some(click_row - vp.rect.top),
            "scroll_top={} local_scroll={} cursor_abs={} cursor_local={} cursor_col={} rows={:?}",
            win.scroll_top(),
            win.local_visual_row(win.scroll_top()),
            win.cursor_abs_row(),
            win.cursor_row(),
            win.cursor_col(),
            app.ui.buf(win.buf).unwrap().lines(),
        );
    }

    #[test]
    fn transcript_tail_follow_tracks_growth_through_render_projection() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());
        let before_top = app.transcript_win().scroll_top();

        app.push_block(smelt_core::Block::Text {
            content: "line 100".into(),
        });
        app.render_normal_to(&mut std::io::sink());

        let win = app.transcript_win();
        let vp = win.viewport.expect("render populated transcript viewport");
        let buf = app.ui.buf(win.buf).expect("transcript buffer");
        assert!(
            win.is_at_tail(buf, vp.rect.height),
            "tail-follow render should end at tail after transcript growth"
        );
        assert!(
            win.scroll_top() >= before_top,
            "tail-follow should not move upward when rows are appended"
        );
        let rendered_rows = buf.lines();
        assert!(rendered_rows.iter().any(|line| line == "line 100"));
        assert!(
            !rendered_rows.iter().any(|line| line == "line 0"),
            "tail projection should stay bounded after growth"
        );
    }

    #[test]
    fn transcript_wheel_down_to_bottom_reengages_tail_follow() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());
        let tail_top = app.transcript_win().scroll_top();
        let vp = app
            .transcript_win()
            .viewport
            .expect("render populated transcript viewport");

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            row: vp.rect.top,
            column: vp.rect.left,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        app.render_normal_to(&mut std::io::sink());
        assert!(
            app.transcript_win().scroll_top() < tail_top,
            "wheel up should move off tail"
        );
        assert!(!app.transcript_win().is_following_tail());

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            row: vp.rect.top,
            column: vp.rect.left,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        app.render_normal_to(&mut std::io::sink());

        let win = app.transcript_win();
        assert_eq!(win.scroll_top(), tail_top);
        assert!(win.is_following_tail());
    }

    #[test]
    fn transcript_page_down_to_bottom_reengages_tail_follow() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());
        let tail_top = app.transcript_win().scroll_top();
        let viewport_rows = app
            .transcript_win()
            .viewport
            .expect("render populated transcript viewport")
            .rect
            .height;
        let start_top = tail_top.saturating_sub(viewport_rows as crate::smelt_edit::RowIndex);
        app.transcript_win_mut().pin_scroll(start_top);
        assert!(!app.transcript_win().is_following_tail());

        app.record_transcript_scroll_intent(
            "page_down",
            TranscriptScrollIntent::PageDelta { pages: 1 },
            start_top,
        );
        app.render_normal_to(&mut std::io::sink());

        let win = app.transcript_win();
        assert_eq!(win.scroll_top(), tail_top);
        assert!(win.is_following_tail());
    }

    #[test]
    fn transcript_scrollbar_click_bottom_reengages_tail_follow() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());
        let tail_top = app.transcript_win().scroll_top();
        let vp = app
            .transcript_win()
            .viewport
            .expect("render populated transcript viewport");
        let bar = vp.scrollbar.expect("transcript scrollbar");
        let start_top = tail_top.saturating_sub(vp.rect.height as crate::smelt_edit::RowIndex);
        app.transcript_win_mut().pin_scroll(start_top);
        assert!(!app.transcript_win().is_following_tail());

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: vp.rect.bottom().saturating_sub(1),
            column: bar.col,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        app.render_normal_to(&mut std::io::sink());

        let win = app.transcript_win();
        assert_eq!(win.scroll_top(), tail_top);
        assert!(win.is_following_tail());
    }

    #[test]
    fn transcript_pinned_scroll_does_not_tail_follow_after_growth() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());
        let tail_top = app.transcript_win().scroll_top();
        let vp = app
            .transcript_win()
            .viewport
            .expect("render populated transcript viewport");

        for _ in 0..12 {
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                row: vp.rect.top,
                column: vp.rect.left,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
        }
        app.render_normal_to(&mut std::io::sink());
        let pinned_top = app.transcript_win().scroll_top();
        assert!(pinned_top < tail_top, "wheel up should move off tail");
        assert!(!app.transcript_win().is_following_tail());

        app.push_block(smelt_core::Block::Text {
            content: "line 100".into(),
        });
        app.render_normal_to(&mut std::io::sink());

        let win = app.transcript_win();
        let rendered_rows = app.ui.buf(win.buf).unwrap().lines();
        assert_eq!(win.scroll_top(), pinned_top);
        assert!(!win.is_following_tail());
        assert!(
            !rendered_rows.iter().any(|line| line == "line 100"),
            "pinned projection should not rematerialize the tail after growth"
        );
    }

    #[test]
    fn transcript_drag_after_tail_render_starts_from_clicked_row() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());

        let vp = app
            .transcript_win()
            .viewport
            .expect("render populated transcript viewport");
        let (start_rel, expected_start) = {
            let win = app.transcript_win();
            let top = win.scroll_top();
            let buf = app.ui.buf(win.buf).expect("transcript buffer");
            (0..vp.rect.height)
                .find_map(|rel| {
                    let local = win
                        .local_visual_row(top.saturating_add(rel as crate::smelt_edit::RowIndex))
                        as usize;
                    let line = buf.get_line(local)?;
                    line.starts_with("line ").then(|| (rel, line.to_string()))
                })
                .expect("visible transcript line")
        };
        let end_rel = start_rel
            .saturating_add(2)
            .min(vp.rect.height.saturating_sub(1));

        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: vp.rect.top.saturating_add(start_rel),
            column: vp.rect.left,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            row: vp.rect.top.saturating_add(end_rel),
            ..down
        };
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..drag
        };

        app.handle_mouse(down);
        app.render_normal_to(&mut std::io::sink());
        app.handle_mouse(drag);
        app.render_normal_to(&mut std::io::sink());
        app.handle_mouse(up);

        let yanked = app.core.clipboard.kill_ring.current();
        assert!(
            yanked.starts_with(&expected_start),
            "selection should start at clicked row {expected_start:?}, got {yanked:?}"
        );
        assert!(
            !yanked.contains("line 0"),
            "selection started from the top of the transcript: {yanked:?}"
        );
    }

    #[test]
    fn row_document_transcript_drag_renders_cursor_and_selection_while_captured() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());

        let vp = app
            .transcript_win()
            .viewport
            .expect("render populated transcript viewport");
        let start_rel = 3u16;
        let end_rel = 5u16;
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: vp.rect.top.saturating_add(start_rel),
            column: vp.rect.left,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            row: vp.rect.top.saturating_add(end_rel),
            ..down
        };

        app.handle_mouse(down);
        app.render_normal_to(&mut std::io::sink());
        app.handle_mouse(drag);
        app.render_normal_to(&mut std::io::sink());

        let win = app.transcript_win();
        assert_eq!(win.cursor_screen_row(vp.rect.height), Some(end_rel));
        let selection = app
            .ui
            .buf(win.buf)
            .expect("transcript buffer")
            .range_layer(crate::smelt_edit::RangeLayer::Selection);
        assert!(
            !selection.is_empty(),
            "row-backed drag selection should be projected while capture freezes transcript materialization"
        );
    }

    #[test]
    fn transcript_drag_while_streaming_keeps_clicked_anchor() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());

        let vp = app
            .transcript_win()
            .viewport
            .expect("render populated transcript viewport");
        let (start_rel, expected_start) = {
            let win = app.transcript_win();
            let top = win.scroll_top();
            let buf = app.ui.buf(win.buf).expect("transcript buffer");
            (0..vp.rect.height)
                .find_map(|rel| {
                    let local = win
                        .local_visual_row(top.saturating_add(rel as crate::smelt_edit::RowIndex))
                        as usize;
                    let line = buf.get_line(local)?;
                    line.starts_with("line ").then(|| (rel, line.to_string()))
                })
                .expect("visible transcript line")
        };
        let end_rel = start_rel
            .saturating_add(2)
            .min(vp.rect.height.saturating_sub(1));

        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: vp.rect.top.saturating_add(start_rel),
            column: vp.rect.left,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            row: vp.rect.top.saturating_add(end_rel),
            ..down
        };
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..drag
        };

        app.handle_mouse(down);
        app.push_block(smelt_core::Block::Text {
            content: "streamed after selection started".into(),
        });
        app.render_normal_to(&mut std::io::sink());
        app.handle_mouse(drag);
        app.render_normal_to(&mut std::io::sink());
        app.handle_mouse(up);

        let yanked = app.core.clipboard.kill_ring.current();
        assert!(
            yanked.starts_with(&expected_start),
            "streaming during drag should keep selection anchored at clicked row {expected_start:?}, got {yanked:?}"
        );
        assert!(
            !yanked.contains("streamed after selection started"),
            "selection anchor jumped to streamed tail content: {yanked:?}"
        );
    }
    #[test]
    fn transcript_click_uses_local_row_in_tail_projection() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        let buf_id = app.transcript_win().buf;
        {
            let buf = app.ui.buf_mut(buf_id).expect("transcript buffer");
            buf.set_all_lines(vec!["alpha".into(), ">>>>target".into()]);
            buf.add_highlight_group_with_meta(
                1,
                0,
                4,
                smelt_core::theme::intern("Normal"),
                smelt_core::buffer::SpanMeta::unselectable(),
            );
        }

        let viewport = crate::smelt_edit::WindowViewport::new(
            crate::smelt_edit::Rect::new(5, 3, 40, 2),
            40,
            22,
            20,
            None,
        );
        {
            let win = app
                .ui
                .win_mut(crate::app::TRANSCRIPT_WIN)
                .expect("transcript window");
            win.set_materialized_rows(20, 2, 22);
            win.pin_scroll(20);
            win.scroll_left = 0;
            win.viewport = Some(viewport);
        }

        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: viewport.rect.top,
            column: viewport.rect.left,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..down
        };

        app.handle_content_mouse(down, 1);
        app.handle_content_mouse(up, 1);

        let win = app.transcript_win();
        assert_eq!(win.cursor_abs_row(), 20);
        assert_eq!(win.cursor_col(), 0);
    }

    #[test]
    fn prompt_triple_click_yanks_only_clicked_source_line() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        let source = "first line\nsecond line\nthird line";
        {
            let buf = app
                .ui
                .buf_mut(crate::app::PROMPT_EDIT_BUF)
                .expect("prompt edit buffer");
            buf.set_source(source.into());
            buf.ensure_rendered_at(80);
        }
        let viewport = crate::smelt_edit::WindowViewport::new(
            crate::smelt_edit::Rect::new(0, 0, 80, 5),
            80,
            3,
            0,
            None,
        );
        app.ui
            .win_mut(crate::app::PROMPT_WIN)
            .expect("prompt window")
            .viewport = Some(viewport);

        for (row, expected) in [(0, "first line"), (1, "second line"), (2, "third line")] {
            app.handle_prompt_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    row,
                    column: 2,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                3,
            );
            app.handle_prompt_mouse(
                MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    row,
                    column: 2,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                3,
            );

            assert_eq!(app.core.clipboard.kill_ring.current(), expected);
        }
    }
}
