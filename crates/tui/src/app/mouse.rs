//! Mouse event handling: wheel scrolling, drag-select, scrollbar drag, cell-click hit-testing.

use crate::app::{AppFocus, EventOutcome, TuiApp};
use crate::content::layout::HitRegion;
use crate::smelt_edit::{HitTarget, WinId};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

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

struct TranscriptMouseCell {
    row: usize,
    source_col: usize,
    scroll_left: usize,
    content_left: usize,
}

fn transcript_mouse_cell(
    win: &crate::smelt_edit::Window,
    me: MouseEvent,
    vp: crate::smelt_edit::WindowViewport,
    row_count: usize,
) -> TranscriptMouseCell {
    let rel_row = me.row.saturating_sub(vp.rect.top) as crate::smelt_edit::RowIndex;
    let abs_row = win.scroll_top().saturating_add(rel_row);
    let row = (win.local_visual_row(abs_row) as usize).min(row_count.saturating_sub(1));

    let content_left =
        (vp.gutter_width as usize).saturating_add(win.config.gutters.pad_left as usize);
    let viewport_col = me.column.saturating_sub(vp.rect.left) as usize;
    let content_col = viewport_col.saturating_sub(content_left);
    let scroll_left = win.scroll_left as usize;

    TranscriptMouseCell {
        row,
        source_col: content_col.saturating_add(scroll_left),
        scroll_left,
        content_left,
    }
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

impl TuiApp {
    // ── Mouse event dispatch ─────────────────────────────────────────────
    pub(crate) fn handle_mouse(&mut self, me: MouseEvent) -> EventOutcome {
        // `Ui::dispatch_event` handles wheel-over-overlay, modal click-outside absorb,
        // and scrollbar drag. Anything unclaimed (`Ignored`) flows through below.
        let cap_before = self.ui.capture();
        if matches!(
            self.ui
                .dispatch_event(crate::smelt_edit::Event::Mouse(me), &mut |_, _, _| {}),
            crate::smelt_edit::Status::Consumed
        ) {
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
            let hits_overlay =
                self.ui
                    .hit_test(me.row, me.column, None)
                    .is_some_and(|ht| match ht {
                        HitTarget::Window(w) => self.ui.overlay_for_leaf(w).is_some(),
                        HitTarget::Chrome { .. } => true,
                        _ => false,
                    });
            if !hits_overlay {
                let region = self.layout.hit_test(me.row, me.column);
                let has_content =
                    self.has_transcript_content(self.core.config.settings.show_thinking);
                if let Some(focus) = focus_for_region_click(region, has_content) {
                    self.app_focus = focus;
                }
            }
        }

        // `Ui::resolve_split_mouse` handles hit-test, click-count, and HitTarget capture
        // so drags stay on the originating window even if the pointer drifts.
        let now = self.core.clock.instant_now();
        if let Some((win, count)) = self.ui.resolve_split_mouse(me, now) {
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
                self.fire_pointer_event(win, ev, me, crate::smelt_edit::MouseButton::Left);
            }
            if win == crate::app::PROMPT_WIN {
                if is_down && self.ui.active_modal().is_none() {
                    self.ui.set_focus(win);
                }
                self.handle_prompt_mouse(me, count);
            } else if win == crate::app::TRANSCRIPT_WIN {
                // Empty transcript: don't start a drag-select on the void.
                if is_down && !self.has_transcript_content(self.core.config.settings.show_thinking)
                {
                    return EventOutcome::Noop;
                }
                if is_down && self.ui.active_modal().is_none() {
                    self.ui.set_focus(win);
                }
                let yank = self.handle_content_mouse(me, count);
                if is_up {
                    if let Some(out) = yank {
                        self.yank_to_clipboard(out);
                    }
                }
            } else if self.ui.win(win).is_some_and(|w| w.selectable) {
                // Generic selectable leaf: notifications, dialog bodies, future popups.
                // On Down, promote keyboard focus to this leaf if it's focusable -
                // overlays with multiple leaves (e.g. side-by-side panes) need click
                // to follow keyboard focus, not just the first leaf the overlay opened.
                // A non-focusable selectable leaf must not steal app_focus.
                if is_down && self.ui.win(win).is_some_and(|w| w.focusable) {
                    self.ui.set_focus(win);
                }
                let yank = self.handle_selectable_leaf_mouse(win, me, count);
                if is_up {
                    if let Some(out) = yank {
                        self.yank_to_clipboard(out);
                    }
                }
            }
            return EventOutcome::Redraw;
        }

        EventOutcome::Noop
    }

    fn pin_well_known_horizontal_scroll(&mut self) {
        for win_id in [crate::app::PROMPT_WIN, crate::app::TRANSCRIPT_WIN] {
            if let Some(win) = self.ui.win_mut(win_id) {
                win.scroll_left = 0;
            }
        }
    }

    /// Fire a pointer `WinEvent` (`Press`/`Release`/`Drag`) on `win` with
    /// leaf-relative cell coords. Coords are clamped to `(0, 0)` when the leaf
    /// has no live viewport (the event landed during a hit-test stale frame).
    fn fire_pointer_event(
        &mut self,
        win: WinId,
        event: crate::smelt_edit::WinEvent,
        me: MouseEvent,
        button: crate::smelt_edit::MouseButton,
    ) {
        let rect = self
            .ui
            .win(win)
            .and_then(|w| w.viewport)
            .map(|v| v.rect)
            .or_else(|| self.ui.paint_rect(crate::smelt_edit::PaintId::from(win)));
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
            win,
            event,
            crate::smelt_edit::Payload::Mouse {
                row: rel_row,
                col: rel_col,
                button,
            },
            &mut lua_invoke,
        );
    }

    fn yank_to_clipboard(&mut self, out: crate::smelt_edit::CopyOutput) {
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
        let (soft, hard) =
            crate::smelt_edit::UiHost::breaks_for(self, crate::app::PROMPT_WIN).unwrap_or_default();
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
    /// leaf that sets `Window::selectable = true`. Skips the snap + word/line-break
    /// machinery the transcript needs - drag-select only, no double/triple-click
    /// word/line expansion. Returns the yanked range on `Up` (caller copies it).
    fn handle_selectable_leaf_mouse(
        &mut self,
        win: crate::smelt_edit::WinId,
        me: MouseEvent,
        click_count: u8,
    ) -> Option<crate::smelt_edit::CopyOutput> {
        let viewport = crate::smelt_edit::UiHost::viewport_for(self, win)?;
        let buf_id = self.ui.win(win).map(|w| w.buf)?;
        let (soft, hard) = crate::smelt_edit::UiHost::breaks_for(self, win).unwrap_or_default();
        let range = {
            let mouse_ctx = crate::smelt_edit::MouseCtx {
                soft_breaks: &soft,
                hard_breaks: &hard,
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
    /// On `MouseUp`, returns the yanked range as `CopyOutput` via `Buffer::copy_range` -
    /// the transcript's `BufferCopy` impl walks the latest snapshot so `copy_as`
    /// substitutions, soft-wrap merging, and non-selectable cells are honored.
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
                w.drag_anchor_word.is_some() || w.drag_anchor_line.is_some()
            }
            _ => false,
        };
        let (rows, soft, hard) = {
            let buf = self.ui.buf(buf_id)?;
            let rows = buf.lines().to_vec();
            let (soft, hard) = if needs_breaks {
                projected_buf_breaks(buf)
            } else {
                (Vec::new(), Vec::new())
            };
            (rows, soft, hard)
        };
        if rows.is_empty() {
            return None;
        }
        let snapped = self.snap_event_for_selection(me, &rows, viewport);

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
    /// Accounts for horizontal scroll so the snap operates on source-cell columns, not
    /// viewport-relative ones.
    fn snap_event_for_selection(
        &mut self,
        me: MouseEvent,
        rows: &[String],
        vp: crate::smelt_edit::WindowViewport,
    ) -> MouseEvent {
        let cell = transcript_mouse_cell(self.transcript_win(), me, vp, rows.len());
        let snapped = self.snap_col_to_selectable(
            cell.row,
            cell.source_col,
            self.core.config.settings.show_thinking,
        );
        let screen_col = snapped
            .saturating_sub(cell.scroll_left)
            .saturating_add(cell.content_left);
        MouseEvent {
            column: vp.rect.left.saturating_add(screen_col as u16),
            ..me
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

    #[test]
    fn transcript_click_after_tail_render_lands_on_clicked_screen_row() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().scroll_to_bottom();
        app.render_normal_to(false, &mut std::io::sink());

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
        app.render_normal_to(false, &mut std::io::sink());
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
    fn transcript_drag_after_tail_render_starts_from_clicked_row() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        for i in 0..100 {
            app.push_block(smelt_core::Block::Text {
                content: format!("line {i}"),
            });
        }
        app.transcript_win_mut().scroll_to_bottom();
        app.render_normal_to(false, &mut std::io::sink());

        let vp = app
            .transcript_win()
            .viewport
            .expect("render populated transcript viewport");
        let top = app.transcript_win().scroll_top();
        let start_rel = 2u16;
        let end_rel = 4u16;
        let start_local = app
            .transcript_win()
            .local_visual_row(top.saturating_add(start_rel as crate::smelt_edit::RowIndex))
            as usize;
        let expected_start = app
            .ui
            .buf(app.transcript_win().buf)
            .and_then(|buf| buf.get_line(start_local))
            .unwrap_or("")
            .to_string();
        assert!(!expected_start.is_empty());

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
        app.render_normal_to(false, &mut std::io::sink());
        app.handle_mouse(drag);
        app.render_normal_to(false, &mut std::io::sink());
        app.handle_mouse(up);

        let yanked = app.core.clipboard.kill_ring.current();
        assert!(
            yanked.contains(&expected_start),
            "selection should start at clicked row {expected_start:?}, got {yanked:?}"
        );
        assert!(
            !yanked.contains("line 0"),
            "selection started from the top of the transcript: {yanked:?}"
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
        app.transcript_win_mut().scroll_to_bottom();
        app.render_normal_to(false, &mut std::io::sink());

        let vp = app
            .transcript_win()
            .viewport
            .expect("render populated transcript viewport");
        let top = app.transcript_win().scroll_top();
        let start_rel = 2u16;
        let end_rel = 4u16;
        let start_local = app
            .transcript_win()
            .local_visual_row(top.saturating_add(start_rel as crate::smelt_edit::RowIndex))
            as usize;
        let expected_start = app
            .ui
            .buf(app.transcript_win().buf)
            .and_then(|buf| buf.get_line(start_local))
            .unwrap_or("")
            .to_string();
        assert!(!expected_start.is_empty());

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
        app.render_normal_to(true, &mut std::io::sink());
        app.handle_mouse(drag);
        app.render_normal_to(true, &mut std::io::sink());
        app.handle_mouse(up);

        let yanked = app.core.clipboard.kill_ring.current();
        assert!(
            yanked.contains(&expected_start),
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
                smelt_core::buffer::SpanMeta {
                    selectable: false,
                    copy_as: None,
                },
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
            win.set_virtual_rows(20, 22);
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
