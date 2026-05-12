//! `smelt-edit` — editor layer over `smelt-term`.
//!
//! `Ui` ties `Window`s (per-buffer view + viewport + scrollbar + gutter)
//! to layout and routes events. Renderer primitives come from `smelt-term`.

pub mod callback;
pub(crate) mod event;
pub(crate) mod motions;
pub(crate) mod overlay;
pub mod text;
pub(crate) mod text_objects;
pub mod vim;
pub(crate) mod window;

pub use smelt_buffer::attachment::AttachmentId;
pub use smelt_buffer::buffer::{
    BufCreateOpts, BufId, Buffer, BufferCopy, BufferParser, CopyOutput, ExtmarkOpts,
    ExtmarkPayload, SelectionRange, SpanMeta, SpanStyle, LUA_BUF_ID_BASE,
};
pub use smelt_buffer::clipboard::Clipboard;
pub use smelt_buffer::undo::{UndoEntry, UndoHistory};

pub use smelt_term::{
    flush_diff, paint_layout_tree, Border, Cell, CellUpdate, Color, Compositor, Constraint, Corner,
    Grid, GridSlice, Gutters, HitRegistry, LayoutTree, Line, PaintDispatch, PaintId, Rect,
    SnapshotFrame, Span, Style, Theme, DEFAULT_ACCENT,
};
pub use smelt_term::{grid, layout};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WinId(pub u64);

impl WinId {
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for WinId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "win:{}", self.0)
    }
}

/// Each `WinId` doubles as the `PaintId` for its layout-tree leaf.
impl From<WinId> for PaintId {
    fn from(w: WinId) -> Self {
        PaintId(w.0)
    }
}

/// Routes `Callback::Lua` handles out to the host's Lua runtime.
pub type LuaInvoke<'a> = dyn FnMut(callback::LuaHandle, WinId, &callback::Payload) + 'a;

use callback::Callbacks;
pub use callback::{Callback, CallbackCtx, CallbackResult, KeyBind, LuaHandle, Payload, WinEvent};
pub use event::{Event, Status};
use overlay::OverlayHitTarget;
pub use overlay::{HitTarget, Overlay, OverlayId};
pub use vim::VimMode;
pub use window::{
    CursorShape, DrawContext, EventCtx, MouseCtx, ScrollbarState, SplitConfig, Window,
    WindowViewport,
};

use std::collections::HashMap;

pub struct Ui {
    bufs: HashMap<BufId, Buffer>,
    wins: HashMap<WinId, Window>,
    next_buf_id: u64,
    next_win_id: u64,
    surface: smelt_term::Surface,
    callbacks: Callbacks,
    /// Insertion order is the secondary z-order sort key; see [`Self::overlays_in_z_order`].
    overlays: Vec<(OverlayId, Overlay)>,
    next_overlay_id: u32,
    /// `set_focus` pushes the outgoing focus here; overlay-close walks it back.
    focus_history: Vec<WinId>,
    focus: Option<WinId>,
    /// Gesture target that bypasses hit-testing for the duration of a drag.
    /// Auto-clears when the owning split or overlay disappears.
    capture: Option<HitTarget>,
    /// `(time, row, col, count)` — tracks successive Down events for click-count.
    last_click: Option<(std::time::Instant, u16, u16, u8)>,
    /// Global cursor shape; only the focused window honours it.
    cursor_shape: CursorShape,
    /// Timestamp when edge-drag autoscroll last engaged; drives host tick-rate ramp.
    drag_autoscroll_since: Option<std::time::Instant>,
    /// In-flight chrome drag/resize gesture; `None` when idle.
    chrome_drag: Option<ChromeDrag>,
}

#[derive(Clone, Copy, Debug)]
struct ChromeDrag {
    overlay: OverlayId,
    zone: overlay::ChromeZone,
    start_rect: Rect,
    origin_row: u16,
    origin_col: u16,
}

impl Ui {
    pub fn new() -> Self {
        Self {
            bufs: HashMap::new(),
            wins: HashMap::new(),
            next_buf_id: 1,
            next_win_id: 0,
            surface: smelt_term::Surface::new(80, 24),
            callbacks: Callbacks::new(),
            overlays: Vec::new(),
            next_overlay_id: 1,
            focus_history: Vec::new(),
            focus: None,
            capture: None,
            last_click: None,
            cursor_shape: CursorShape::Hidden,
            drag_autoscroll_since: None,
            chrome_drag: None,
        }
    }

    /// Returns 1/2/3 for successive Downs on the same cell within 400ms; wraps at 4.
    fn record_click(&mut self, row: u16, col: u16) -> u8 {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let count = match self.last_click {
            Some((t, r, c, n))
                if now.duration_since(t) < Duration::from_millis(400)
                    && r == row
                    && c == col
                    && n < 3 =>
            {
                n + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, row, col, count));
        count
    }

    /// Hit-test a primary-button Down/Drag/Up against splits leaves.
    /// Latches capture on Down; returns `(win, click_count)` where `click_count`
    /// is 0 for Drag/Up. Up clears capture. Call only when `dispatch_event`
    /// returned `Ignored` — that method owns scrollbar drag and modal blocking.
    pub fn resolve_split_mouse(&mut self, me: crossterm::event::MouseEvent) -> Option<(WinId, u8)> {
        use crossterm::event::{MouseButton, MouseEventKind};
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let win = match self.hit_test(me.row, me.column, None)? {
                    HitTarget::Window(w) if self.splits().contains_leaf(w) => w,
                    _ => return None,
                };
                self.set_capture(HitTarget::Window(win));
                let count = self.record_click(me.row, me.column);
                Some((win, count))
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.capture {
                Some(HitTarget::Window(win)) => Some((win, 0)),
                _ => None,
            },
            MouseEventKind::Up(MouseButton::Left) => match self.capture {
                Some(HitTarget::Window(win)) => {
                    self.clear_capture();
                    Some((win, 0))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Replace the splits layout. Clears focus and capture if their targets
    /// are no longer reachable.
    pub fn set_layout(&mut self, tree: LayoutTree) {
        self.surface.set_layout(tree);
        if let Some(focus) = self.focus {
            if !self.splits().contains_leaf(focus) && self.overlay_for_leaf(focus).is_none() {
                self.focus = None;
            }
        }
        if let Some(cap) = self.capture {
            if !self.capture_target_alive(cap) {
                self.capture = None;
                self.drag_autoscroll_since = None;
            }
        }
    }

    fn splits(&self) -> &LayoutTree {
        self.surface.layout()
    }

    fn resolve_splits(&self) -> HashMap<WinId, Rect> {
        layout::resolve_layout(self.splits(), self.surface.area())
            .into_iter()
            .map(|(p, r)| (WinId(p.0), r))
            .collect()
    }

    pub fn split_rect(&self, win: WinId) -> Option<Rect> {
        self.resolve_splits().get(&win).copied()
    }

    pub fn buf_create(&mut self, opts: BufCreateOpts) -> BufId {
        let id = BufId(self.next_buf_id);
        self.next_buf_id += 1;
        let buf = Buffer::new(id, opts);
        self.bufs.insert(id, buf);
        id
    }

    /// Create a buffer at an explicit id. Returns `Err` if the id is already occupied.
    /// Lua-minted ids live above `LUA_BUF_ID_BASE`; advancing `next_buf_id` past that
    /// boundary would cause Rust and Lua allocators to collide.
    pub fn buf_create_with_id(&mut self, id: BufId, opts: BufCreateOpts) -> Result<BufId, BufId> {
        if self.bufs.contains_key(&id) {
            return Err(id);
        }
        let buf = Buffer::new(id, opts);
        self.bufs.insert(id, buf);
        // Only advance when the id is in Rust's range — Lua ids have their own counter.
        if id.0 < LUA_BUF_ID_BASE && id.0 >= self.next_buf_id {
            self.next_buf_id = id.0 + 1;
        }
        Ok(id)
    }

    pub fn buf(&self, id: BufId) -> Option<&Buffer> {
        self.bufs.get(&id)
    }

    pub fn buf_mut(&mut self, id: BufId) -> Option<&mut Buffer> {
        self.bufs.get_mut(&id)
    }

    pub fn buf_destroy(&mut self, id: BufId) -> Option<Buffer> {
        self.bufs.remove(&id)
    }

    pub fn win_buf_mut(&mut self, win: WinId) -> Option<&mut Buffer> {
        let id = self.wins.get(&win)?.buf;
        self.bufs.get_mut(&id)
    }

    /// Borrow both a window and a buffer mutably at once. Safe because they live
    /// in disjoint collections inside `Ui`.
    pub fn win_and_buf_mut(
        &mut self,
        win: WinId,
        buf: BufId,
    ) -> (Option<&mut Window>, Option<&mut Buffer>) {
        let win_ref = self.wins.get_mut(&win);
        let buf_ref = self.bufs.get_mut(&buf);
        (win_ref, buf_ref)
    }

    // ── Overlay ──────────────────────────────────────────────────────

    /// Register an overlay and return its `OverlayId`. Modal overlays auto-focus the first leaf.
    pub fn overlay_open(&mut self, overlay: Overlay) -> OverlayId {
        let id = OverlayId(self.next_overlay_id);
        self.next_overlay_id += 1;
        let modal = overlay.modal;
        let first_leaf = overlay.layout.leaves_in_order().into_iter().next();
        self.overlays.push((id, overlay));
        if modal {
            if let Some(leaf) = first_leaf {
                self.set_focus(WinId(leaf.0));
            }
        }
        id
    }

    /// Close an overlay. Returns the removed `Overlay`. Restores focus to the most
    /// recent still-focusable entry in `focus_history`, or clears focus if history
    /// is exhausted. Focus outside the closed overlay is left untouched.
    pub fn overlay_close(&mut self, id: OverlayId) -> Option<Overlay> {
        let pos = self.overlays.iter().position(|(oid, _)| *oid == id)?;
        let (_, removed) = self.overlays.remove(pos);
        if let Some(cap) = self.capture {
            let owned = match cap {
                HitTarget::Chrome { owner, .. } => owner == id,
                HitTarget::Window(w) | HitTarget::Scrollbar { owner: w } => {
                    removed.layout.contains_leaf(w)
                }
            };
            if owned {
                self.capture = None;
                self.drag_autoscroll_since = None;
                self.chrome_drag = None;
            }
        }

        if let Some(focused) = self.focus {
            if removed.layout.contains_leaf(focused) {
                self.focus = None;
                while let Some(prior) = self.focus_history.pop() {
                    if self.overlay_for_leaf(prior).is_some() {
                        self.focus = Some(prior);
                        return Some(removed);
                    }
                    if self.splits().contains_leaf(prior)
                        && self.wins.get(&prior).map(|w| w.focusable).unwrap_or(false)
                    {
                        self.focus = Some(prior);
                        return Some(removed);
                    }
                }
                // History exhausted — focus stays cleared.
            }
        }
        Some(removed)
    }

    pub fn overlay(&self, id: OverlayId) -> Option<&Overlay> {
        self.overlays
            .iter()
            .find_map(|(oid, ov)| (*oid == id).then_some(ov))
    }

    pub fn overlay_mut(&mut self, id: OverlayId) -> Option<&mut Overlay> {
        self.overlays
            .iter_mut()
            .find_map(|(oid, ov)| (*oid == id).then_some(ov))
    }

    fn overlays_in_z_order(&self) -> Vec<(OverlayId, &Overlay)> {
        let mut entries: Vec<(OverlayId, &Overlay)> =
            self.overlays.iter().map(|(id, o)| (*id, o)).collect();
        entries.sort_by_key(|(_, o)| o.z);
        entries
    }

    /// Topmost (highest-z) open modal overlay, if any.
    pub fn active_modal(&self) -> Option<OverlayId> {
        self.overlays_in_z_order()
            .into_iter()
            .rev()
            .find_map(|(id, ov)| ov.modal.then_some(id))
    }

    /// Overlay whose layout contains the currently-focused window, if any.
    pub fn focused_overlay(&self) -> Option<OverlayId> {
        let focused = self.focus()?;
        self.overlays
            .iter()
            .find_map(|(id, ov)| ov.layout.contains_leaf(focused).then_some(*id))
    }

    /// Hit-test a screen position. Checks overlays (topmost-z first, modal-aware)
    /// then splits leaves. Scrollbar column returns `HitTarget::Scrollbar`.
    pub fn hit_test(&self, row: u16, col: u16, cursor: Option<(u16, u16)>) -> Option<HitTarget> {
        if let Some((id, target)) = self.overlay_hit_test(row, col, cursor) {
            return Some(match target {
                OverlayHitTarget::Window(w) => HitTarget::Window(w),
                OverlayHitTarget::Chrome(zone) => HitTarget::Chrome { owner: id, zone },
            });
        }
        let split_rects = self.resolve_splits();
        for paint_id in self.splits().leaves_in_order() {
            let win = WinId(paint_id.0);
            if let Some(rect) = split_rects.get(&win) {
                if !rect.contains(row, col) {
                    continue;
                }
                if let Some(bar_owner) = self
                    .wins
                    .get(&win)
                    .and_then(|w| w.viewport)
                    .and_then(|vp| vp.scrollbar.map(|bar| (vp, bar)))
                    .filter(|(vp, bar)| bar.contains(vp.rect, row, col))
                    .map(|_| win)
                {
                    return Some(HitTarget::Scrollbar { owner: bar_owner });
                }
                return Some(HitTarget::Window(win));
            }
        }
        None
    }

    /// Hit-test against overlays only. The active modal is opaque at its rect for
    /// lower-z overlays; higher-z overlays still receive clicks on the parts that
    /// cover the modal.
    fn overlay_hit_test(
        &self,
        row: u16,
        col: u16,
        cursor: Option<(u16, u16)>,
    ) -> Option<(OverlayId, OverlayHitTarget)> {
        let modal_id = self.active_modal();
        let resolved = self.resolve_overlays(cursor);
        let modal_info: Option<(Rect, u16)> = modal_id.and_then(|mid| {
            resolved
                .iter()
                .find_map(|(oid, rect, ov)| (*oid == mid).then_some((*rect, ov.z)))
        });
        let mut resolved = resolved;
        resolved.reverse(); // topmost first
        for (id, rect, ov) in resolved {
            if !rect.contains(row, col) {
                continue;
            }
            // Overlays at or above the modal's z paint on top and receive clicks;
            // overlays below are blocked at the modal's rect.
            if let Some((mr, mz)) = modal_info {
                if ov.z < mz && mr.contains(row, col) {
                    continue;
                }
            }
            let leaf_rects = layout::resolve_layout(&ov.layout, rect);
            for (paint_id, leaf_rect) in &leaf_rects {
                if leaf_rect.contains(row, col) {
                    return Some((id, OverlayHitTarget::Window(WinId(paint_id.0))));
                }
            }
            return Some((
                id,
                OverlayHitTarget::Chrome(chrome_zone(rect, ov, row, col)),
            ));
        }
        None
    }

    /// Returns z-ordered (lowest first) overlay rects. Overlays whose anchor
    /// cannot resolve (missing cursor / missing Win target) are silently skipped.
    fn resolve_overlays(&self, cursor: Option<(u16, u16)>) -> Vec<(OverlayId, Rect, &Overlay)> {
        let (term_w, term_h) = self.surface.terminal_size();
        let split_rects = self.resolve_splits();
        let ctx = overlay::AnchorContext {
            term_width: term_w,
            term_height: term_h,
            cursor,
            win_rects: &split_rects,
        };
        let mut out = Vec::with_capacity(self.overlays.len());
        for (id, ov) in self.overlays_in_z_order() {
            let size = ov
                .size_override
                .unwrap_or_else(|| ov.layout.natural_size((term_w, term_h)));
            if let Some(rect) = overlay::resolve_anchor(&ov.anchor, size, &ctx) {
                out.push((id, rect, ov));
            }
        }
        out
    }

    pub fn win_open_split(&mut self, buf: BufId, config: SplitConfig) -> Option<WinId> {
        if !self.bufs.contains_key(&buf) {
            return None;
        }
        while self.wins.contains_key(&WinId(self.next_win_id)) {
            self.next_win_id += 1;
        }
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;
        let win = Window::new(id, buf, config);
        self.wins.insert(id, win);
        Some(id)
    }

    /// Open a window at an explicit `WinId`. Returns `false` if the id is occupied
    /// or the buffer doesn't exist. Use when callers need a stable id for Lua callbacks.
    pub fn win_open_split_at(&mut self, id: WinId, buf: BufId, config: SplitConfig) -> bool {
        if self.wins.contains_key(&id) || !self.bufs.contains_key(&buf) {
            return false;
        }
        let win = Window::new(id, buf, config);
        self.wins.insert(id, win);
        true
    }

    /// Close a window. Returns Lua callback ids for the caller to drop from the Lua registry.
    /// When `id` is an overlay leaf, closes the whole overlay and clears all leaf callbacks.
    #[must_use]
    pub fn win_close(&mut self, id: WinId) -> Vec<u64> {
        if let Some(overlay_id) = self.overlay_for_leaf(id) {
            let mut all_ids = Vec::new();
            if let Some(removed) = self.overlay_close(overlay_id) {
                for leaf in removed.layout.leaves_in_order() {
                    let win = WinId(leaf.0);
                    all_ids.extend(self.callbacks.clear_all(win));
                    self.wins.remove(&win);
                }
            }
            return all_ids;
        }
        self.wins.remove(&id);
        self.callbacks.clear_all(id)
    }

    // ── Callbacks ────────────────────────────────────────────────────

    /// Bind a key to a callback. Returns the displaced callback, if any.
    #[must_use]
    pub fn win_set_keymap(&mut self, win: WinId, key: KeyBind, cb: Callback) -> Option<Callback> {
        self.callbacks.set_keymap(win, key, cb)
    }

    #[must_use]
    pub fn win_clear_keymap(&mut self, win: WinId, key: KeyBind) -> Option<Callback> {
        self.callbacks.clear_keymap(win, key)
    }

    /// Catch-all key handler; runs after specific keymaps miss. Returns the displaced callback.
    #[must_use]
    pub fn win_set_key_fallback(&mut self, win: WinId, cb: Callback) -> Option<Callback> {
        self.callbacks.set_key_fallback(win, cb)
    }

    /// Register an event callback. Multiple callbacks per (win, event) fire in registration order.
    pub fn win_on_event(&mut self, win: WinId, ev: WinEvent, cb: Callback) {
        self.callbacks.on_event(win, ev, cb);
    }

    /// Remove a specific event callback by Lua handle id.
    #[must_use]
    pub fn win_clear_event_by_id(&mut self, win: WinId, ev: WinEvent, id: u64) -> Option<Callback> {
        self.callbacks.clear_event_by_id(win, ev, id)
    }

    /// Fire a `WinEvent`. Overlay leaves redirect to the overlay root (first declaration-order
    /// leaf) so `dialog.lua` handlers registered on the root fire regardless of which leaf
    /// triggered the event.
    pub fn fire_win_event(
        &mut self,
        win: WinId,
        ev: WinEvent,
        payload: Payload,
        lua_invoke: &mut LuaInvoke,
    ) {
        let target = self.overlay_root_for_leaf(win).unwrap_or(win);
        let Some(mut cbs) = self.callbacks.take_event(target, ev) else {
            return;
        };
        for cb in cbs.iter_mut() {
            match cb {
                Callback::Rust(inner) => {
                    let mut ctx = CallbackCtx {
                        ui: self,
                        win: target,
                        payload: payload.clone(),
                    };
                    let _ = inner(&mut ctx);
                }
                Callback::Lua(handle) => {
                    lua_invoke(*handle, target, &payload);
                }
            }
        }
        self.callbacks.restore_event(target, ev, cbs);
    }

    pub fn win(&self, id: WinId) -> Option<&Window> {
        self.wins.get(&id)
    }

    pub fn win_mut(&mut self, id: WinId) -> Option<&mut Window> {
        self.wins.get_mut(&id)
    }

    pub fn set_terminal_size(&mut self, w: u16, h: u16) {
        self.surface.set_terminal_size(w, h);
    }

    pub fn terminal_size(&self) -> (u16, u16) {
        self.surface.terminal_size()
    }

    // ── Focus ──────────────────────────────────────────────────────

    pub fn focus(&self) -> Option<WinId> {
        self.focus
    }

    pub fn focused_window(&self) -> Option<&Window> {
        self.wins.get(&self.focus()?)
    }

    /// Focus `win`. Returns `false` if it isn't a focusable splits leaf or overlay leaf.
    /// Re-focusing the current window is a no-op (no history push).
    pub fn set_focus(&mut self, win: WinId) -> bool {
        let prior = self.focus;
        if prior == Some(win) {
            return true;
        }
        let is_split_leaf = self.splits().contains_leaf(win)
            && self.wins.get(&win).map(|w| w.focusable).unwrap_or(false);
        let is_overlay_leaf = self.overlay_for_leaf(win).is_some();
        if !is_split_leaf && !is_overlay_leaf {
            return false;
        }
        if let Some(p) = prior {
            self.focus_history.push(p);
        }
        self.focus = Some(win);
        true
    }

    /// Returns the `OverlayId` of the open overlay whose layout contains `win`, if any.
    pub fn overlay_for_leaf(&self, win: WinId) -> Option<OverlayId> {
        for (id, ov) in &self.overlays {
            if ov.layout.contains_leaf(win) {
                return Some(*id);
            }
        }
        None
    }

    /// Returns the first declaration-order leaf of the overlay containing `win`.
    /// `fire_win_event` redirects to this root so dialog handlers fire on any leaf interaction.
    fn overlay_root_for_leaf(&self, win: WinId) -> Option<WinId> {
        let id = self.overlay_for_leaf(win)?;
        let ov = self.overlay(id)?;
        ov.layout
            .leaves_in_order()
            .first()
            .copied()
            .map(|p| WinId(p.0))
    }

    #[cfg(test)]
    fn focus_history(&self) -> &[WinId] {
        &self.focus_history
    }

    #[cfg(test)]
    fn focus_next(&mut self) -> bool {
        self.focus_step(1)
    }

    #[cfg(test)]
    fn focus_prev(&mut self) -> bool {
        self.focus_step(-1)
    }

    #[cfg(test)]
    fn focus_step(&mut self, dir: i32) -> bool {
        let Some(modal_id) = self.active_modal() else {
            return false;
        };
        let Some(modal) = self.overlay(modal_id) else {
            return false;
        };
        let leaves: Vec<WinId> = modal
            .layout
            .leaves_in_order()
            .into_iter()
            .map(|p| WinId(p.0))
            .filter(|w| self.wins.contains_key(w))
            .collect();
        if leaves.is_empty() {
            return false;
        }
        let current = self.focus();
        let current_idx = current
            .and_then(|w| leaves.iter().position(|x| *x == w))
            .map(|i| i as i32)
            .unwrap_or(-1);
        let len = leaves.len() as i32;
        let next_idx = (current_idx + dir).rem_euclid(len) as usize;
        let target = leaves[next_idx];
        if Some(target) == current {
            return false;
        }
        self.set_focus(target)
    }

    // ── Capture ──────────────────────────────────────────────────

    pub fn capture(&self) -> Option<HitTarget> {
        self.capture
    }

    fn set_capture(&mut self, target: HitTarget) {
        self.capture = Some(target);
    }

    fn clear_capture(&mut self) {
        self.capture = None;
        self.drag_autoscroll_since = None;
    }

    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    pub fn set_cursor_shape(&mut self, shape: CursorShape) {
        self.cursor_shape = shape;
    }

    /// Timestamp when edge-drag autoscroll started; hosts use this to ramp the tick interval.
    pub fn drag_autoscroll_started(&self) -> Option<std::time::Instant> {
        self.drag_autoscroll_since
    }

    /// While the captured window's cursor is parked at a viewport edge, returns
    /// `Some((win, delta))` with `delta = -1` (top) or `+1` (bottom). Manages the
    /// autoscroll timestamp internally.
    pub fn poll_drag_autoscroll(&mut self) -> Option<(WinId, isize)> {
        let win_id = match self.capture {
            Some(HitTarget::Window(w)) => w,
            _ => {
                self.drag_autoscroll_since = None;
                return None;
            }
        };
        let win = self.wins.get(&win_id)?;
        let viewport_h = match win.viewport {
            Some(v) => v.rect.height as usize,
            None => {
                self.drag_autoscroll_since = None;
                return None;
            }
        };
        if viewport_h == 0 {
            self.drag_autoscroll_since = None;
            return None;
        }
        // Drag-autoscroll fires when the cursor sits at the top/bottom edge of the
        // viewport. `cursor_row` is buffer-absolute; convert via `cursor_screen_row`
        // (cursor off-screen → no autoscroll).
        let Some(screen_row) = win.cursor_screen_row(viewport_h as u16) else {
            self.drag_autoscroll_since = None;
            return None;
        };
        let delta: isize = if screen_row == 0 {
            -1
        } else if (screen_row as usize) >= viewport_h.saturating_sub(1) {
            1
        } else {
            self.drag_autoscroll_since = None;
            return None;
        };
        self.drag_autoscroll_since
            .get_or_insert_with(std::time::Instant::now);
        Some((win_id, delta))
    }

    fn capture_target_alive(&self, target: HitTarget) -> bool {
        match target {
            HitTarget::Window(w) | HitTarget::Scrollbar { owner: w } => {
                self.splits().contains_leaf(w) || self.overlay_for_leaf(w).is_some()
            }
            HitTarget::Chrome { owner, .. } => self.overlays.iter().any(|(id, _)| *id == owner),
        }
    }

    // ── Renderer ─────────────────────────────────────────────────

    pub fn render<W: std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.render_with_paints(w, |_, _, _| {})
    }

    /// Render one frame, delegating non-Window `Paint(id)` leaves to `paint(id, slice, ctx)`.
    pub fn render_with_paints<W, F>(&mut self, w: &mut W, mut paint: F) -> std::io::Result<()>
    where
        W: std::io::Write,
        F: FnMut(PaintId, &mut GridSlice<'_>, &DrawContext),
    {
        let resolved = self.resolve_overlays(None);
        let resolved: Vec<(OverlayId, Rect, Overlay)> = resolved
            .into_iter()
            .map(|(id, rect, ov)| (id, rect, ov.clone()))
            .collect();
        let split_rects = self.resolve_splits();
        let painted_splits: Vec<(WinId, Rect)> = self
            .splits()
            .leaves_in_order()
            .into_iter()
            .filter_map(|p| {
                let win = WinId(p.0);
                split_rects.get(&win).map(|r| (win, *r))
            })
            .collect();
        // Pre-pass: call `ensure_rendered_at` on each overlay leaf so parsers populate
        // their lines before the immutable paint walk. Also writes `Window.viewport` so
        // input dispatch (vim nav, mouse hit-test) sees the same geometry as the compositor.
        for (_id, rect, overlay) in &resolved {
            let leaf_rects = layout::resolve_layout(&overlay.layout, *rect);
            for (paint_id, leaf_rect) in &leaf_rects {
                let win_id = WinId(paint_id.0);
                let Some(buf_id) = self.wins.get(&win_id).map(|w| w.buf) else {
                    continue;
                };
                if let Some(buf) = self.bufs.get_mut(&buf_id) {
                    buf.ensure_rendered_at(leaf_rect.width);
                }
                let total_rows = self
                    .bufs
                    .get(&buf_id)
                    .map(|b| b.line_count() as u16)
                    .unwrap_or(0);
                if let Some(win) = self.wins.get_mut(&win_id) {
                    win.viewport = Some(window::WindowViewport::new(
                        *leaf_rect,
                        leaf_rect.height,
                        total_rows,
                        win.scroll_top,
                        None,
                    ));
                }
            }
        }
        for (win_id, rect) in &painted_splits {
            let Some(buf_id) = self.wins.get(win_id).map(|w| w.buf) else {
                continue;
            };
            if let Some(buf) = self.bufs.get_mut(&buf_id) {
                buf.ensure_rendered_at(rect.width);
            }
        }
        // Hardware cursor: overlay leaves and painted splits are outside the compositor's
        // focused-layer cursor path, so we compute the absolute position here and return
        // it from the closure. Overlay wins over split if both are focused.
        let cursor_override = if matches!(self.cursor_shape, CursorShape::Hardware) {
            self.focused_overlay_cursor(&resolved)
                .or_else(|| self.focused_painted_split_cursor())
        } else {
            None
        };
        let focus = self.focus;
        let cursor_shape = self.cursor_shape;
        let wins = &self.wins;
        let bufs = &self.bufs;
        let term_size = self.surface.terminal_size();
        let splits_tree = self.splits().clone();
        let term_w = self.surface.terminal_size().0;
        let term_h = self.surface.terminal_size().1;
        let theme_arc = std::sync::Arc::clone(self.surface.theme());
        let theme_for_compositor = std::sync::Arc::clone(&theme_arc);
        self.surface
            .compositor_mut()
            .render_with(&theme_for_compositor, w, move |grid, _theme| {
                let theme = &theme_arc;
                let mut dispatch = |id: PaintId,
                                    area: Rect,
                                    grid: &mut Grid,
                                    theme: &std::sync::Arc<Theme>,
                                    term_size: (u16, u16)| {
                    let win_id = WinId(id.0);
                    if let Some(win) = wins.get(&win_id) {
                        if let Some(buf) = bufs.get(&win.buf) {
                            let mut slice = grid.slice_mut(area);
                            let focused = focus == Some(win_id);
                            let ctx = DrawContext {
                                terminal_width: term_size.0,
                                terminal_height: term_size.1,
                                focused,
                                cursor_shape: if focused {
                                    cursor_shape
                                } else {
                                    CursorShape::Hidden
                                },
                                theme: std::sync::Arc::clone(theme),
                                vim_mode: win.vim_mode,
                            };
                            win.render(buf, &mut slice, &ctx);
                            return;
                        }
                    }
                    let mut slice = grid.slice_mut(area);
                    let ctx = DrawContext {
                        terminal_width: term_size.0,
                        terminal_height: term_size.1,
                        focused: false,
                        cursor_shape: CursorShape::Hidden,
                        theme: std::sync::Arc::clone(theme),
                        vim_mode: VimMode::default(),
                    };
                    paint(id, &mut slice, &ctx);
                };
                paint_layout_tree(
                    grid,
                    theme,
                    &splits_tree,
                    Rect::new(0, 0, term_w, term_h),
                    term_size,
                    &mut dispatch,
                );
                for (_id, rect, overlay) in &resolved {
                    paint_overlay(grid, theme, *rect, overlay, term_size, &mut dispatch);
                }
                cursor_override
            })
    }

    /// Paint directly into the compositor's `Grid`, bypassing layout and overlay machinery.
    pub fn render_raw<W, F>(&mut self, w: &mut W, paint: F) -> std::io::Result<()>
    where
        W: std::io::Write,
        F: FnOnce(&mut Grid, &Theme) -> Option<(u16, u16)>,
    {
        self.surface.render_raw(w, paint)
    }

    /// Resolved screen rect for a `PaintId` leaf across splits and overlays.
    pub fn paint_rect(&self, id: PaintId) -> Option<Rect> {
        let (term_w, term_h) = self.surface.terminal_size();
        let area = Rect::new(0, 0, term_w, term_h);
        if let Some(rect) = layout::resolve_layout(self.splits(), area).get(&id) {
            return Some(*rect);
        }
        for (_oid, ov_rect, ov) in self.resolve_overlays(None) {
            if let Some(rect) = layout::resolve_layout(&ov.layout, ov_rect).get(&id) {
                return Some(*rect);
            }
        }
        None
    }

    /// Render to a sink and return the resulting grid snapshot. Used by the storybook harness.
    pub fn snapshot(&mut self) -> SnapshotFrame {
        let mut sink = std::io::sink();
        self.render(&mut sink).expect("snapshot render to sink");
        SnapshotFrame::from_grid(self.surface.compositor().previous())
    }

    fn focused_overlay_cursor(
        &self,
        resolved: &[(OverlayId, Rect, Overlay)],
    ) -> Option<(u16, u16)> {
        let focus = self.focus?;
        self.overlay_for_leaf(focus)?;
        let focus_paint = PaintId::from(focus);
        for (_id, rect, overlay) in resolved {
            let leaf_rects = layout::resolve_layout(&overlay.layout, *rect);
            let Some(leaf_rect) = leaf_rects.get(&focus_paint) else {
                continue;
            };
            let win = self.wins.get(&focus)?;
            let screen_row = win.cursor_screen_row(leaf_rect.height)?;
            let abs_y = leaf_rect.top + screen_row;
            let abs_x = leaf_rect.left + win.cursor_col();
            if abs_y < leaf_rect.top + leaf_rect.height && abs_x < leaf_rect.left + leaf_rect.width
            {
                return Some((abs_x, abs_y));
            }
            return None;
        }
        None
    }

    /// Absolute hardware cursor for the focused splits leaf; applies `pad_left` to land past chrome.
    fn focused_painted_split_cursor(&self) -> Option<(u16, u16)> {
        let focus = self.focus?;
        if !self.splits().contains_leaf(focus) {
            return None;
        }
        let win = self.wins.get(&focus)?;
        let rect = self.split_rect(focus)?;
        let pad_left = win.config.gutters.pad_left;
        let screen_row = win.cursor_screen_row(rect.height)?;
        let abs_y = rect.top + screen_row;
        let abs_x = rect.left + pad_left + win.cursor_col();
        if abs_y < rect.top + rect.height && abs_x < rect.left + rect.width {
            Some((abs_x, abs_y))
        } else {
            None
        }
    }

    pub fn theme(&self) -> &Theme {
        self.surface.theme().as_ref()
    }

    pub fn theme_mut(&mut self) -> &mut Theme {
        self.surface.theme_mut()
    }

    /// Route a terminal event. Key: fires keymaps; bare Esc/Ctrl-C on a modal dismisses it.
    /// Resize: updates terminal size. Mouse: owns scrollbar drag and chrome drag; absorbs
    /// wheel on focused overlays and clicks blocked by a modal. Returns `Ignored` for
    /// everything else so the host can continue routing.
    pub fn dispatch_event(&mut self, ev: Event, lua_invoke: &mut LuaInvoke) -> Status {
        use crossterm::event::{KeyEvent, MouseButton, MouseEventKind};
        match ev {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => self.dispatch_key(code, modifiers, lua_invoke),
            Event::Resize(w, h) => {
                self.set_terminal_size(w, h);
                Status::Consumed
            }
            Event::Mouse(me) => {
                match me.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(HitTarget::Scrollbar { owner }) =
                            self.hit_test(me.row, me.column, None)
                        {
                            self.set_capture(HitTarget::Scrollbar { owner });
                            self.apply_scrollbar_drag(owner, me.row);
                            return Status::Consumed;
                        }
                        let hit = self.hit_test(me.row, me.column, None);
                        let raise = match hit {
                            Some(HitTarget::Chrome { owner, .. }) => Some(owner),
                            Some(HitTarget::Window(w)) => self.overlay_for_leaf(w),
                            _ => None,
                        };
                        if let Some(owner) = raise {
                            self.raise_overlay_to_front(owner);
                        }
                        // Non-focusable leaf of a draggable overlay is treated as Body chrome
                        // so the user can grab anywhere on it to move the panel.
                        let drag_target: Option<(OverlayId, overlay::ChromeZone)> = match hit {
                            Some(HitTarget::Chrome { owner, zone }) => Some((owner, zone)),
                            Some(HitTarget::Window(w)) => {
                                let leaf_focusable =
                                    self.wins.get(&w).map(|win| win.focusable).unwrap_or(true);
                                self.overlay_for_leaf(w).and_then(|owner| {
                                    let ov_draggable =
                                        self.overlay(owner).map(|o| o.draggable).unwrap_or(false);
                                    (!leaf_focusable && ov_draggable)
                                        .then_some((owner, overlay::ChromeZone::Body))
                                })
                            }
                            _ => None,
                        };
                        if let Some((owner, zone)) = drag_target {
                            let drag_kind = match zone {
                                overlay::ChromeZone::Title | overlay::ChromeZone::Body => {
                                    self.overlay(owner).map(|ov| ov.draggable).unwrap_or(false)
                                }
                                overlay::ChromeZone::Resize => {
                                    self.overlay(owner).map(|ov| ov.resizable).unwrap_or(false)
                                }
                            };
                            if drag_kind {
                                if let Some(rect) = self.resolved_overlay_rect(owner) {
                                    self.set_capture(HitTarget::Chrome { owner, zone });
                                    self.chrome_drag = Some(ChromeDrag {
                                        overlay: owner,
                                        zone,
                                        start_rect: rect,
                                        origin_row: me.row,
                                        origin_col: me.column,
                                    });
                                    return Status::Consumed;
                                }
                            }
                            // Non-drag chrome click still consumes so it doesn't fall through.
                            if matches!(hit, Some(HitTarget::Chrome { .. })) {
                                return Status::Consumed;
                            }
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(HitTarget::Scrollbar { owner }) = self.capture {
                            self.apply_scrollbar_drag(owner, me.row);
                            return Status::Consumed;
                        }
                        if let Some(drag) = self.chrome_drag {
                            self.apply_chrome_drag(drag, me.row, me.column);
                            return Status::Consumed;
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if matches!(self.capture, Some(HitTarget::Scrollbar { .. })) {
                            self.clear_capture();
                            return Status::Consumed;
                        }
                        if self.chrome_drag.is_some() {
                            self.chrome_drag = None;
                            self.clear_capture();
                            return Status::Consumed;
                        }
                    }
                    _ => {}
                }
                let is_scroll = matches!(
                    me.kind,
                    MouseEventKind::ScrollUp
                        | MouseEventKind::ScrollDown
                        | MouseEventKind::ScrollLeft
                        | MouseEventKind::ScrollRight
                );
                if is_scroll && self.focused_overlay().is_some() {
                    return Status::Consumed;
                }
                // Modal blocks splits hits while open; overlay hits already returned above.
                if self.active_modal().is_some()
                    && self.overlay_hit_test(me.row, me.column, None).is_none()
                {
                    return Status::Consumed;
                }
                Status::Ignored
            }
            Event::FocusGained | Event::FocusLost | Event::Paste(_) => Status::Ignored,
        }
    }

    /// Raise `id` above all other overlays' `z`. Called on any Down inside an overlay.
    fn raise_overlay_to_front(&mut self, id: OverlayId) {
        let max_z = self.overlays.iter().map(|(_, o)| o.z).max().unwrap_or(0);
        if let Some((_, ov)) = self.overlays.iter_mut().find(|(oid, _)| *oid == id) {
            ov.z = max_z.saturating_add(1);
        }
    }

    fn resolved_overlay_rect(&self, id: OverlayId) -> Option<Rect> {
        self.resolve_overlays(None)
            .into_iter()
            .find_map(|(oid, rect, _)| if oid == id { Some(rect) } else { None })
    }

    /// Apply a chrome-drag delta. Title/Body move the overlay via `ScreenAt`; Resize grows it.
    fn apply_chrome_drag(&mut self, drag: ChromeDrag, row: u16, col: u16) {
        let dy = row as i32 - drag.origin_row as i32;
        let dx = col as i32 - drag.origin_col as i32;
        let Some((_, ov)) = self
            .overlays
            .iter_mut()
            .find(|(oid, _)| *oid == drag.overlay)
        else {
            return;
        };
        match drag.zone {
            overlay::ChromeZone::Title | overlay::ChromeZone::Body => {
                let new_top = drag.start_rect.top as i32 + dy;
                let new_left = drag.start_rect.left as i32 + dx;
                ov.anchor = layout::Anchor::ScreenAt {
                    row: new_top,
                    col: new_left,
                    corner: Corner::NW,
                };
            }
            overlay::ChromeZone::Resize => {
                let (min_w, min_h) = MIN_OVERLAY_SIZE;
                let new_w = (drag.start_rect.width as i32 + dx).max(min_w as i32) as u16;
                let new_h = (drag.start_rect.height as i32 + dy).max(min_h as i32) as u16;
                ov.size_override = Some((new_w, new_h));
            }
        }
    }

    fn apply_scrollbar_drag(&mut self, owner: WinId, row: u16) {
        let Some(win) = self.wins.get(&owner) else {
            return;
        };
        let Some(vp) = win.viewport else {
            return;
        };
        let Some(bar) = vp.scrollbar else {
            return;
        };
        let rel_row = row.saturating_sub(vp.rect.top);
        let thumb_top = bar.thumb_top_for_click(rel_row);
        let from_top = bar.scroll_from_top_for_thumb(thumb_top);
        if let Some(win) = self.wins.get_mut(&owner) {
            win.scroll_top = from_top;
        }
    }

    fn dispatch_key(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> Status {
        let key = KeyBind::new(code, mods);
        // `CallbackResult::Event` writes here to synthesize a WinEvent after the key callback.
        let mut follow_up: Option<(WinEvent, Payload)> = None;
        let result = if let Some(win) = self.focus() {
            if let Some(mut cb) = self.callbacks.take_keymap(win, key) {
                let r = match &mut cb {
                    Callback::Rust(inner) => {
                        let mut ctx = CallbackCtx {
                            ui: self,
                            win,
                            payload: Payload::Key { code, mods },
                        };
                        let r = inner(&mut ctx);
                        match r {
                            CallbackResult::Consumed => Status::Consumed,
                            CallbackResult::Pass => Status::Ignored,
                            CallbackResult::Event(ev, payload) => {
                                follow_up = Some((ev, payload));
                                Status::Consumed
                            }
                        }
                    }
                    Callback::Lua(handle) => {
                        let payload = Payload::Key { code, mods };
                        lua_invoke(*handle, win, &payload);
                        Status::Consumed
                    }
                };
                self.callbacks.restore_keymap(win, key, cb);
                r
            } else if let Some(mut cb) = self.callbacks.take_key_fallback(win) {
                let r = match &mut cb {
                    Callback::Rust(inner) => {
                        let mut ctx = CallbackCtx {
                            ui: self,
                            win,
                            payload: Payload::Key { code, mods },
                        };
                        let r = inner(&mut ctx);
                        match r {
                            CallbackResult::Consumed => Status::Consumed,
                            CallbackResult::Pass => Status::Ignored,
                            CallbackResult::Event(ev, payload) => {
                                follow_up = Some((ev, payload));
                                Status::Consumed
                            }
                        }
                    }
                    Callback::Lua(handle) => {
                        let payload = Payload::Key { code, mods };
                        lua_invoke(*handle, win, &payload);
                        Status::Consumed
                    }
                };
                self.callbacks.restore_key_fallback(win, cb);
                r
            } else {
                Status::Ignored
            }
        } else {
            Status::Ignored
        };

        if let Some((ev, payload)) = follow_up {
            if let Some(win) = self.focus() {
                self.fire_win_event(win, ev, payload, lua_invoke);
            }
        }

        // Leaf gets first dibs on Esc/Ctrl-C; if ignored, dismiss the active modal.
        let is_dismiss_chord = matches!(code, crossterm::event::KeyCode::Esc)
            && mods == crossterm::event::KeyModifiers::NONE
            || matches!(code, crossterm::event::KeyCode::Char('c'))
                && mods == crossterm::event::KeyModifiers::CONTROL;
        if result == Status::Ignored && is_dismiss_chord {
            if let Some(modal) = self.active_modal() {
                if let Some(root) = self
                    .overlay(modal)
                    .and_then(|o| o.layout.leaves_in_order().first().copied())
                    .map(|p| WinId(p.0))
                {
                    self.fire_win_event(root, WinEvent::Dismiss, Payload::None, lua_invoke);
                }
                // Guard: Lua dismiss handler may have already closed the overlay.
                if self.overlay(modal).is_some() {
                    let _ = self.overlay_close(modal);
                }
                return Status::Consumed;
            }
        }

        result
    }

    pub fn dispatch_tick(&mut self, lua_invoke: &mut LuaInvoke) {
        let wins: Vec<WinId> = self.callbacks.wins_with_event(WinEvent::Tick);
        for win in wins {
            self.fire_win_event(win, WinEvent::Tick, Payload::None, lua_invoke);
        }
    }

    pub fn force_redraw(&mut self) {
        self.surface.force_redraw();
    }
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

/// Compositor-bearing surface exposed by frontends. `TuiApp` implements this; `HeadlessApp`
/// does not — UiHost-only Lua bindings raise a runtime error in headless context.
/// `Ui` also impls it so tests can exercise compositor code without a full frontend.
pub trait UiHost {
    fn ui(&mut self) -> &mut Ui;
    fn set_focus(&mut self, win: WinId) -> bool;
    fn buf_create(&mut self, opts: BufCreateOpts) -> BufId;
    fn buf_mut(&mut self, id: BufId) -> Option<&mut Buffer>;
    fn win_open_split(&mut self, buf: BufId, config: SplitConfig) -> Option<WinId>;
    #[must_use]
    fn win_close(&mut self, id: WinId) -> Vec<u64>;
    fn win_mut(&mut self, id: WinId) -> Option<&mut Window>;
    fn overlay_open(&mut self, overlay: Overlay) -> OverlayId;

    /// Last-painted viewport for `win`. Hosts override when they project geometry differently.
    fn viewport_for(&self, win: WinId) -> Option<WindowViewport>;

    /// Display rows for `win`. Default reads the backing buffer's lines.
    /// Hosts override for windows whose painted rows differ from the source
    /// (wrapped prompt, transcript projection).
    fn rows_for(&mut self, win: WinId) -> Option<Vec<String>>;

    /// Soft (word-wrap) and hard (`\n`) break byte positions in `rows_for(win)?.join("\n")`.
    /// Default returns no soft breaks and hard breaks at join points.
    fn breaks_for(&mut self, win: WinId) -> Option<(Vec<usize>, Vec<usize>)>;
}

impl UiHost for Ui {
    fn ui(&mut self) -> &mut Ui {
        self
    }
    fn set_focus(&mut self, win: WinId) -> bool {
        Ui::set_focus(self, win)
    }
    fn buf_create(&mut self, opts: BufCreateOpts) -> BufId {
        Ui::buf_create(self, opts)
    }
    fn buf_mut(&mut self, id: BufId) -> Option<&mut Buffer> {
        Ui::buf_mut(self, id)
    }
    fn win_open_split(&mut self, buf: BufId, config: SplitConfig) -> Option<WinId> {
        Ui::win_open_split(self, buf, config)
    }
    fn win_close(&mut self, id: WinId) -> Vec<u64> {
        Ui::win_close(self, id)
    }
    fn win_mut(&mut self, id: WinId) -> Option<&mut Window> {
        Ui::win_mut(self, id)
    }
    fn overlay_open(&mut self, overlay: Overlay) -> OverlayId {
        Ui::overlay_open(self, overlay)
    }
    fn viewport_for(&self, win: WinId) -> Option<WindowViewport> {
        Ui::win(self, win).and_then(|w| w.viewport)
    }
    fn rows_for(&mut self, win: WinId) -> Option<Vec<String>> {
        let buf_id = Ui::win(self, win)?.buf;
        let buf = Ui::buf(self, buf_id)?;
        Some(buf.lines().to_vec())
    }
    fn breaks_for(&mut self, win: WinId) -> Option<(Vec<usize>, Vec<usize>)> {
        let buf_id = Ui::win(self, win)?.buf;
        let buf = Ui::buf(self, buf_id)?;
        let lines = buf.lines();
        let mut hard = Vec::new();
        let mut pos = 0usize;
        for (i, l) in lines.iter().enumerate() {
            pos += l.len();
            if i + 1 < lines.len() {
                hard.push(pos);
                pos += 1;
            }
        }
        Some((Vec::new(), hard))
    }
}

/// Minimum `(width, height)` a resize gesture can shrink an overlay to.
const MIN_OVERLAY_SIZE: (u16, u16) = (8, 3);

/// Classify a chrome hit inside an overlay rect: top row → Title, bottom-right cell → Resize, else Body.
fn chrome_zone(rect: Rect, ov: &Overlay, row: u16, col: u16) -> overlay::ChromeZone {
    if ov.resizable
        && rect.height >= 1
        && rect.width >= 1
        && row + 1 == rect.bottom()
        && col + 1 == rect.right()
    {
        return overlay::ChromeZone::Resize;
    }
    if row == rect.top {
        return overlay::ChromeZone::Title;
    }
    overlay::ChromeZone::Body
}

/// Paint one resolved overlay: clear the rect (overlays are opaque) then walk its layout tree.
fn paint_overlay(
    grid: &mut Grid,
    theme: &std::sync::Arc<Theme>,
    area: Rect,
    overlay: &Overlay,
    term_size: (u16, u16),
    paint: &mut PaintDispatch,
) {
    grid.clear(area);
    paint_layout_tree(grid, theme, &overlay.layout, area, term_size, paint);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ui() -> Ui {
        let mut ui = Ui::new();
        ui.set_terminal_size(80, 24);
        ui
    }

    fn dispatch_key(
        ui: &mut Ui,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> Status {
        ui.dispatch_event(
            Event::Key(crossterm::event::KeyEvent::new(code, mods)),
            &mut |_, _, _| {},
        )
    }

    fn make_split(ui: &mut Ui, win_id: WinId) {
        let buf = ui.buf_create(BufCreateOpts::default());
        assert!(ui.win_open_split_at(
            win_id,
            buf,
            SplitConfig {
                region: format!("test:{}", win_id.0),
                gutters: layout::Gutters::default(),
            },
        ));
        let mut leaves: Vec<(Constraint, LayoutTree)> = ui
            .splits()
            .leaves_in_order()
            .into_iter()
            .map(|w| (Constraint::Fill, LayoutTree::leaf(w)))
            .collect();
        leaves.push((Constraint::Fill, LayoutTree::leaf(win_id)));
        ui.set_layout(LayoutTree::vbox(leaves));
    }

    #[test]
    fn buf_create_with_id_lua_range_does_not_advance_rust_allocator() {
        let mut ui = make_ui();
        let rust_first = ui.buf_create(BufCreateOpts::default());
        ui.buf_create_with_id(BufId(LUA_BUF_ID_BASE), BufCreateOpts::default())
            .unwrap();
        let rust_second = ui.buf_create(BufCreateOpts::default());
        assert_eq!(rust_second.0, rust_first.0 + 1);
        assert!(rust_second.0 < LUA_BUF_ID_BASE);
    }

    // ── Overlay API ──────────────────────────────────────────────────

    fn stub_overlay() -> Overlay {
        let layout = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(WinId(99)))]);
        Overlay::new(layout, layout::Anchor::ScreenCenter)
    }

    #[test]
    fn overlay_open_returns_unique_ids() {
        let mut ui = make_ui();
        let a = ui.overlay_open(stub_overlay());
        let b = ui.overlay_open(stub_overlay());
        assert_ne!(a, b);
        assert!(ui.overlay(a).is_some());
        assert!(ui.overlay(b).is_some());
    }

    #[test]
    fn overlay_close_removes_overlay() {
        let mut ui = make_ui();
        let id = ui.overlay_open(stub_overlay());
        let removed = ui.overlay_close(id);
        assert!(removed.is_some());
        assert!(ui.overlay(id).is_none());
        assert!(ui.overlay_close(id).is_none());
    }

    #[test]
    fn overlay_mut_allows_anchor_drag() {
        let mut ui = make_ui();
        let id = ui.overlay_open(stub_overlay());
        ui.overlay_mut(id).unwrap().anchor = layout::Anchor::ScreenAt {
            row: 5,
            col: 10,
            corner: Corner::NW,
        };
        assert!(matches!(
            ui.overlay(id).unwrap().anchor,
            layout::Anchor::ScreenAt {
                row: 5,
                col: 10,
                ..
            }
        ));
    }

    #[test]
    fn overlays_in_z_order_sorts_by_z_then_id() {
        let mut ui = make_ui();
        let high = ui.overlay_open(stub_overlay().with_z(100));
        let mid = ui.overlay_open(stub_overlay().with_z(50));
        let low_a = ui.overlay_open(stub_overlay().with_z(10));
        let low_b = ui.overlay_open(stub_overlay().with_z(10));
        let order: Vec<OverlayId> = ui
            .overlays_in_z_order()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        // Lowest z first; same z falls back to insertion order (id).
        assert_eq!(order, vec![low_a, low_b, mid, high]);
    }

    fn sized_overlay(width: u16, height: u16, anchor: layout::Anchor) -> Overlay {
        // Single-leaf box wrapped in an Hbox of fixed width holding a
        // Vbox of fixed height — exercises both axes' natural-size
        // composition.
        let layout = LayoutTree::hbox(vec![(
            Constraint::Length(width),
            LayoutTree::vbox(vec![(
                Constraint::Length(height),
                LayoutTree::leaf(WinId(99)),
            )]),
        )]);
        Overlay::new(layout, anchor)
    }

    #[test]
    fn resolve_overlays_centers_screen_center_anchor() {
        let mut ui = make_ui();
        let id = ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter));
        let resolved = ui.resolve_overlays(None);
        assert_eq!(resolved.len(), 1);
        let (got_id, rect, _) = &resolved[0];
        assert_eq!(*got_id, id);
        // Centered: term 80x24, overlay 40x10 → top=7, left=20.
        assert_eq!(*rect, Rect::new(7, 20, 40, 10));
    }

    #[test]
    fn resolve_overlays_skips_cursor_anchor_when_cursor_missing() {
        let mut ui = make_ui();
        ui.overlay_open(sized_overlay(
            10,
            5,
            layout::Anchor::Cursor {
                corner: Corner::NW,
                row_offset: 0,
                col_offset: 0,
            },
        ));
        // No cursor supplied → overlay drops out of the resolved set.
        assert!(ui.resolve_overlays(None).is_empty());
        // With a cursor, it resolves.
        let resolved = ui.resolve_overlays(Some((4, 6)));
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn resolve_overlays_skips_win_anchor_when_target_missing() {
        let mut ui = make_ui();
        ui.overlay_open(sized_overlay(
            10,
            5,
            layout::Anchor::Win {
                target: WinId(999).into(),
                attach: Corner::NW,
                row_offset: 0,
                col_offset: 0,
            },
        ));
        assert!(ui.resolve_overlays(None).is_empty());
        // Once the target lands as a splits leaf with a known rect,
        // the overlay resolves anchored to it. Build a tree that
        // produces rect (top=5, left=10, width=30, height=8) on an
        // 80x24 terminal.
        let target = WinId(999);
        let buf = ui.buf_create(BufCreateOpts::default());
        assert!(ui.win_open_split_at(
            target,
            buf,
            SplitConfig {
                region: "anchor".into(),
                gutters: layout::Gutters::default(),
            },
        ));
        // vbox: 5 rows blank + 8-row hbox (10 cols blank + 30-col leaf
        // + fill) + fill below.
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(5), LayoutTree::vbox(Vec::new())),
            (
                Constraint::Length(8),
                LayoutTree::hbox(vec![
                    (Constraint::Length(10), LayoutTree::vbox(Vec::new())),
                    (Constraint::Length(30), LayoutTree::leaf(target)),
                    (Constraint::Fill, LayoutTree::vbox(Vec::new())),
                ]),
            ),
            (Constraint::Fill, LayoutTree::vbox(Vec::new())),
        ]);
        ui.set_layout(tree);
        assert_eq!(ui.split_rect(target), Some(Rect::new(5, 10, 30, 8)));
        let resolved = ui.resolve_overlays(None);
        assert_eq!(resolved.len(), 1);
        let (_, rect, _) = &resolved[0];
        assert_eq!(*rect, Rect::new(5, 10, 10, 5));
    }

    #[test]
    fn active_modal_empty_returns_none() {
        let ui = make_ui();
        assert_eq!(ui.active_modal(), None);
    }

    #[test]
    fn active_modal_skips_non_modal_overlays() {
        let mut ui = make_ui();
        ui.overlay_open(stub_overlay()); // non-modal
        ui.overlay_open(stub_overlay().with_z(100)); // non-modal, higher z
        assert_eq!(ui.active_modal(), None);
    }

    #[test]
    fn focused_window_returns_window_for_split_with_inserted_win() {
        let mut ui = make_ui();
        let win = WinId(7);
        make_split(&mut ui, win);
        ui.set_focus(win);
        assert_eq!(ui.focused_window().map(|w| w.id), Some(win));
    }

    #[test]
    fn overlay_close_with_focus_inside_pops_to_prior() {
        let mut ui = make_ui();
        let outside = WinId(7);
        make_split(&mut ui, outside);
        // Inside-the-overlay leaf id (stub_overlay uses WinId(99)).
        let inside = WinId(99);
        make_split(&mut ui, inside);
        let id = ui.overlay_open(stub_overlay());

        ui.set_focus(outside);
        ui.set_focus(inside);
        assert_eq!(ui.focus(), Some(inside));
        assert_eq!(ui.focus_history(), &[outside]);

        ui.overlay_close(id);
        // Pop walked back to `outside`; history drained.
        assert_eq!(ui.focus(), Some(outside));
        assert!(ui.focus_history().is_empty());
    }

    #[test]
    fn overlay_close_with_focus_outside_leaves_focus_alone() {
        let mut ui = make_ui();
        let outside = WinId(50);
        make_split(&mut ui, outside);
        let id = ui.overlay_open(stub_overlay());
        ui.set_focus(outside);
        ui.overlay_close(id);
        assert_eq!(ui.focus(), Some(outside));
    }

    #[test]
    fn overlay_close_with_exhausted_history_clears_focus() {
        let mut ui = make_ui();
        let inside = WinId(99); // stub_overlay's leaf
        make_split(&mut ui, inside);
        let id = ui.overlay_open(stub_overlay());
        ui.set_focus(inside);
        // No prior focus — history empty.
        assert!(ui.focus_history().is_empty());
        ui.overlay_close(id);
        assert_eq!(ui.focus(), None);
    }

    #[test]
    fn focused_overlay_returns_none_when_no_focus() {
        let mut ui = make_ui();
        ui.overlay_open(stub_overlay());
        assert_eq!(ui.focused_overlay(), None);
    }

    #[test]
    fn focused_overlay_returns_overlay_containing_focused_leaf() {
        let mut ui = make_ui();
        let win = WinId(99);
        make_split(&mut ui, win);
        let id = ui.overlay_open(stub_overlay()); // stub uses Leaf(WinId(99))
        ui.set_focus(win);
        assert_eq!(ui.focused_overlay(), Some(id));
    }

    #[test]
    fn focused_overlay_returns_none_when_focus_on_unrelated_split() {
        let mut ui = make_ui();
        let other = WinId(50);
        make_split(&mut ui, other);
        ui.overlay_open(stub_overlay());
        ui.set_focus(other);
        assert_eq!(ui.focused_overlay(), None);
    }

    #[test]
    fn active_modal_returns_topmost_modal() {
        let mut ui = make_ui();
        let _bg = ui.overlay_open(stub_overlay().with_z(100)); // higher z but non-modal
        let m_low = ui.overlay_open(stub_overlay().with_z(10).modal(true));
        let m_mid = ui.overlay_open(stub_overlay().with_z(50).modal(true));
        assert_eq!(ui.active_modal(), Some(m_mid));
        // Closing the topmost modal falls back to the next.
        ui.overlay_close(m_mid);
        assert_eq!(ui.active_modal(), Some(m_low));
    }

    #[test]
    fn focus_returns_none_on_fresh_ui() {
        let ui = make_ui();
        assert_eq!(ui.focus(), None);
        assert!(ui.focus_history().is_empty());
    }

    #[test]
    fn set_focus_unknown_win_returns_false() {
        let mut ui = make_ui();
        assert!(!ui.set_focus(WinId(999)));
        assert_eq!(ui.focus(), None);
    }

    #[test]
    fn set_focus_on_registered_split_focuses_the_win() {
        let mut ui = make_ui();
        let win = WinId(7);
        make_split(&mut ui, win);
        assert!(ui.set_focus(win));
        assert_eq!(ui.focus(), Some(win));
        assert!(ui.focus_history().is_empty());
    }

    #[test]
    fn set_focus_pushes_prior_focus_to_history() {
        let mut ui = make_ui();
        let a = WinId(7);
        let b = WinId(8);
        make_split(&mut ui, a);
        make_split(&mut ui, b);
        ui.set_focus(a);
        ui.set_focus(b);
        assert_eq!(ui.focus(), Some(b));
        assert_eq!(ui.focus_history(), &[a]);
    }

    #[test]
    fn set_focus_same_win_is_noop() {
        let mut ui = make_ui();
        let win = WinId(7);
        make_split(&mut ui, win);
        ui.set_focus(win);
        assert!(ui.set_focus(win));
        assert!(ui.focus_history().is_empty());
    }

    #[test]
    fn set_focus_chain_builds_history_in_order() {
        let mut ui = make_ui();
        for n in 1..=4 {
            make_split(&mut ui, WinId(n));
        }
        ui.set_focus(WinId(1));
        ui.set_focus(WinId(2));
        ui.set_focus(WinId(3));
        ui.set_focus(WinId(4));
        assert_eq!(ui.focus(), Some(WinId(4)));
        assert_eq!(ui.focus_history(), &[WinId(1), WinId(2), WinId(3)]);
    }

    #[test]
    fn overlay_hit_test_returns_none_when_empty() {
        let ui = make_ui();
        assert_eq!(ui.overlay_hit_test(10, 30, None), None);
    }

    #[test]
    fn overlay_hit_test_window_inside_leaf() {
        let mut ui = make_ui();
        // 40x10 overlay centered at (7, 20)..(17, 60); single Leaf.
        let id = ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter));
        let hit = ui.overlay_hit_test(10, 30, None).unwrap();
        assert_eq!(hit.0, id);
        assert!(matches!(hit.1, OverlayHitTarget::Window(WinId(99))));
    }

    #[test]
    fn overlay_hit_test_chrome_when_inside_overlay_outside_leaves() {
        let mut ui = make_ui();
        // Outer Vbox with single-border + inner Hbox of fixed width
        // gives the overlay a concrete (42, 10) natural size centered
        // at (7, 19). Border consumes the top/bottom row + left/right
        // col; leaf occupies rows 8..16, cols 20..60.
        let bordered = Overlay::new(
            LayoutTree::vbox(vec![(
                Constraint::Length(8),
                LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(WinId(99)))]),
            )])
            .with_border(layout::Border::SINGLE),
            layout::Anchor::ScreenCenter,
        );
        let id = ui.overlay_open(bordered);
        // Inside overlay rect (row 7 = top border), outside the leaf.
        let hit = ui.overlay_hit_test(7, 30, None).unwrap();
        assert_eq!(hit.0, id);
        assert_eq!(hit.1, OverlayHitTarget::Chrome(overlay::ChromeZone::Title));
        // Inside the leaf → Window.
        let hit = ui.overlay_hit_test(10, 30, None).unwrap();
        assert!(matches!(hit.1, OverlayHitTarget::Window(WinId(99))));
    }

    #[test]
    fn overlay_hit_test_returns_none_outside_overlay_rect() {
        let mut ui = make_ui();
        ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter));
        // (0, 0) is outside the overlay's centered rect.
        assert_eq!(ui.overlay_hit_test(0, 0, None), None);
    }

    #[test]
    fn body_click_drags_non_focusable_draggable_overlay_no_modal() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let leaf = WinId(123);
        assert!(ui.win_open_split_at(
            leaf,
            buf,
            SplitConfig {
                region: "test".into(),
                gutters: Default::default(),
            }
        ));
        ui.win_mut(leaf).unwrap().focusable = false;

        let perf_layout = LayoutTree::hbox(vec![(
            Constraint::Length(10),
            LayoutTree::vbox(vec![(Constraint::Length(4), LayoutTree::leaf(leaf))]),
        )])
        .with_border(layout::Border::SINGLE)
        .with_title("perf");
        let perf = ui.overlay_open(
            Overlay::new(
                perf_layout,
                layout::Anchor::ScreenAt {
                    row: 0,
                    col: 60,
                    corner: layout::Corner::NW,
                },
            )
            .draggable(true),
        );
        // Click on the body (inside the leaf, not the border).
        let down = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 65,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let status = ui.dispatch_event(Event::Mouse(down), &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        assert!(
            ui.chrome_drag.is_some(),
            "body click on non-focusable leaf of a draggable overlay should latch drag (no modal)"
        );
        assert_eq!(ui.chrome_drag.unwrap().overlay, perf);
    }

    #[test]
    fn body_click_drags_non_focusable_draggable_overlay_through_modal() {
        // Regression: a non-focusable leaf inside a draggable overlay
        // (perf panel — pure HUD with `focusable = false`) treats body
        // clicks the same as chrome Title/Body for drag purposes,
        // *and* this works while a modal dialog is open below.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let leaf = WinId(123);
        assert!(ui.win_open_split_at(
            leaf,
            buf,
            SplitConfig {
                region: "test".into(),
                gutters: Default::default(),
            }
        ));
        // Mark the leaf non-focusable (matches `perf_panel.lua`'s
        // `smelt.win.open(buf, { focusable = false })`).
        ui.win_mut(leaf).unwrap().focusable = false;

        let perf_layout = LayoutTree::hbox(vec![(
            Constraint::Length(10),
            LayoutTree::vbox(vec![(Constraint::Length(4), LayoutTree::leaf(leaf))]),
        )])
        .with_border(layout::Border::SINGLE)
        .with_title("perf");
        let perf = ui.overlay_open(
            Overlay::new(
                perf_layout,
                layout::Anchor::ScreenAt {
                    row: 0,
                    col: 60,
                    corner: layout::Corner::NW,
                },
            )
            .draggable(true),
        );
        ui.overlay_open(
            sized_overlay(80, 8, layout::Anchor::ScreenBottom { above_rows: 1 }).modal(true),
        );

        // Click on the body (inside the leaf, not the border).
        let down = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 65,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let status = ui.dispatch_event(Event::Mouse(down), &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        assert!(
            ui.chrome_drag.is_some(),
            "body click on non-focusable leaf of a draggable overlay should latch drag"
        );
        assert_eq!(ui.chrome_drag.unwrap().overlay, perf);
    }

    #[test]
    fn chrome_drag_latches_on_non_modal_overlay_with_modal_open() {
        // Regression: a draggable non-modal overlay (perf panel) and a
        // modal dialog (messages) both at default z=50 should not
        // block each other's chrome drag — if the click lands on the
        // perf panel's chrome, the drag must latch.
        let mut ui = make_ui();
        // Perf panel: bordered, top-right corner, draggable. Outer
        // rect 10×4 with single border → top row is Title chrome.
        let perf_layout = LayoutTree::hbox(vec![(
            Constraint::Length(10),
            LayoutTree::vbox(vec![(Constraint::Length(4), LayoutTree::leaf(WinId(99)))]),
        )])
        .with_border(layout::Border::SINGLE)
        .with_title("perf");
        let perf = ui.overlay_open(
            Overlay::new(
                perf_layout,
                layout::Anchor::ScreenAt {
                    row: 0,
                    col: 60,
                    corner: layout::Corner::NW,
                },
            )
            .draggable(true),
        );
        // Messages dialog: bottom-docked modal.
        ui.overlay_open(
            sized_overlay(80, 8, layout::Anchor::ScreenBottom { above_rows: 1 }).modal(true),
        );
        // Click on perf's title row (row 0, somewhere on the border).
        let down = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 65,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let hit = ui.hit_test(0, 65, None);
        let modal = ui.active_modal();
        let status = ui.dispatch_event(Event::Mouse(down), &mut |_, _, _| {});
        assert!(
            ui.chrome_drag.is_some(),
            "chrome drag should latch on perf panel even with modal open: hit={hit:?} modal={modal:?} status={status:?}"
        );
        let drag = ui.chrome_drag.unwrap();
        assert_eq!(drag.overlay, perf);
    }

    #[test]
    fn overlay_hit_test_modal_blocks_only_inside_its_rect() {
        let mut ui = make_ui();
        // Lower-z overlay covering (7,20)..(17,60).
        let under = ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter).with_z(10));
        // Higher-z modal at same anchor, smaller (10x4 → centered (10,35)..(14,45)).
        let modal_id = ui.overlay_open(
            sized_overlay(10, 4, layout::Anchor::ScreenCenter)
                .with_z(100)
                .modal(true),
        );
        // Hit inside the modal → returns the modal (modal is opaque on
        // its own rect).
        let hit = ui.overlay_hit_test(11, 36, None).unwrap();
        assert_eq!(hit.0, modal_id);
        // Hit inside the under overlay but outside the modal — the
        // visible part of the under overlay still receives clicks. The
        // modal only blocks at its own rect, not globally.
        let outside = ui.overlay_hit_test(8, 22, None).unwrap();
        assert_eq!(outside.0, under);
    }

    fn modal_overlay_with_leaves(a: WinId, b: WinId, c: WinId) -> Overlay {
        let layout = LayoutTree::vbox(vec![
            (Constraint::Length(3), LayoutTree::leaf(a)),
            (
                Constraint::Length(3),
                LayoutTree::hbox(vec![
                    (Constraint::Length(20), LayoutTree::leaf(b)),
                    (Constraint::Length(20), LayoutTree::leaf(c)),
                ]),
            ),
        ]);
        Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true)
    }

    #[test]
    fn focus_next_returns_false_outside_modal() {
        let mut ui = make_ui();
        let win = WinId(50);
        make_split(&mut ui, win);
        ui.set_focus(win);
        // No modal open → focus cycling is a no-op.
        assert!(!ui.focus_next());
        assert_eq!(ui.focus(), Some(win));
    }

    #[test]
    fn focus_next_cycles_modal_leaves() {
        let mut ui = make_ui();
        let a = WinId(100);
        let b = WinId(101);
        let c = WinId(102);
        for w in [a, b, c] {
            make_split(&mut ui, w);
        }
        ui.overlay_open(modal_overlay_with_leaves(a, b, c));
        ui.set_focus(a);
        assert!(ui.focus_next());
        assert_eq!(ui.focus(), Some(b));
        assert!(ui.focus_next());
        assert_eq!(ui.focus(), Some(c));
        // Wrap.
        assert!(ui.focus_next());
        assert_eq!(ui.focus(), Some(a));
    }

    #[test]
    fn focus_prev_walks_backwards_with_wrap() {
        let mut ui = make_ui();
        let a = WinId(100);
        let b = WinId(101);
        let c = WinId(102);
        for w in [a, b, c] {
            make_split(&mut ui, w);
        }
        ui.overlay_open(modal_overlay_with_leaves(a, b, c));
        ui.set_focus(a);
        assert!(ui.focus_prev());
        assert_eq!(ui.focus(), Some(c));
        assert!(ui.focus_prev());
        assert_eq!(ui.focus(), Some(b));
    }

    #[test]
    fn focus_next_skips_unregistered_leaves() {
        let mut ui = make_ui();
        let a = WinId(100);
        let c = WinId(102);
        // b (101) intentionally not registered.
        make_split(&mut ui, a);
        make_split(&mut ui, c);
        ui.overlay_open(modal_overlay_with_leaves(a, WinId(101), c));
        ui.set_focus(a);
        assert!(ui.focus_next());
        assert_eq!(ui.focus(), Some(c));
    }

    #[test]
    fn hit_test_returns_overlay_window_when_overlay_covers_point() {
        let mut ui = make_ui();
        ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter));
        // Centered (7,20)..(17,60); (10,30) lands on the leaf.
        let hit = ui.hit_test(10, 30, None).unwrap();
        assert!(matches!(hit, HitTarget::Window(WinId(99))));
    }

    #[test]
    fn hit_test_returns_chrome_with_overlay_owner() {
        let mut ui = make_ui();
        let id = ui.overlay_open(Overlay::new(
            LayoutTree::vbox(vec![(
                Constraint::Length(8),
                LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(WinId(99)))]),
            )])
            .with_border(layout::Border::SINGLE),
            layout::Anchor::ScreenCenter,
        ));
        let hit = ui.hit_test(7, 30, None).unwrap();
        assert_eq!(
            hit,
            HitTarget::Chrome {
                owner: id,
                zone: overlay::ChromeZone::Title
            }
        );
    }

    #[test]
    fn hit_test_returns_none_when_nothing_covers_point() {
        let ui = make_ui();
        assert_eq!(ui.hit_test(0, 0, None), None);
    }

    #[test]
    fn overlay_hit_test_topmost_wins_when_no_modal() {
        let mut ui = make_ui();
        let _bottom =
            ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter).with_z(10));
        let top = ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter).with_z(50));
        let hit = ui.overlay_hit_test(10, 30, None).unwrap();
        assert_eq!(hit.0, top);
    }

    #[test]
    fn resolve_overlays_returns_z_ordered_resolved_set() {
        let mut ui = make_ui();
        let high = ui.overlay_open(sized_overlay(20, 5, layout::Anchor::ScreenCenter).with_z(100));
        let low = ui.overlay_open(sized_overlay(10, 4, layout::Anchor::ScreenCenter).with_z(10));
        let resolved = ui.resolve_overlays(None);
        let ids: Vec<OverlayId> = resolved.iter().map(|(id, _, _)| *id).collect();
        assert_eq!(ids, vec![low, high]);
    }

    #[test]
    fn overlay_open_modal_focuses_first_leaf() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "t".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(Constraint::Length(3), LayoutTree::leaf(win))]);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true));
        assert_eq!(ui.focus(), Some(win));
    }

    #[test]
    fn set_focus_accepts_overlay_leaf() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "t".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(Constraint::Length(3), LayoutTree::leaf(win))]);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter)); // not modal
        assert!(ui.set_focus(win));
        assert_eq!(ui.focus(), Some(win));
    }

    #[test]
    fn set_focus_accepts_focusable_splits_leaf() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        assert!(ui.set_focus(win));
        assert_eq!(ui.focus(), Some(win));
    }

    #[test]
    fn set_focus_rejects_non_focusable_splits_leaf() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.win_mut(win).unwrap().focusable = false;
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        assert!(!ui.set_focus(win));
        assert_eq!(ui.focus(), None);
    }

    #[test]
    fn set_layout_drops_focus_when_focused_leaf_disappears() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        ui.set_focus(win);
        assert_eq!(ui.focus(), Some(win));
        // New tree omits the focused leaf — focus clears.
        ui.set_layout(LayoutTree::vbox(Vec::new()));
        assert_eq!(ui.focus(), None);
    }

    #[test]
    fn capture_starts_unset() {
        let ui = make_ui();
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn set_capture_then_clear_capture() {
        let mut ui = make_ui();
        let target = HitTarget::Scrollbar { owner: WinId(7) };
        ui.set_capture(target);
        assert_eq!(ui.capture(), Some(target));
        ui.clear_capture();
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn set_layout_clears_capture_when_split_owner_disappears() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        ui.set_capture(HitTarget::Scrollbar { owner: win });
        // Replacement tree omits `win` — capture must clear.
        ui.set_layout(LayoutTree::vbox(Vec::new()));
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn set_layout_keeps_capture_when_split_owner_persists() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let tree = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(win))]);
        ui.set_layout(tree.clone());
        let target = HitTarget::Scrollbar { owner: win };
        ui.set_capture(target);
        ui.set_layout(tree);
        assert_eq!(ui.capture(), Some(target));
    }

    #[test]
    fn overlay_close_clears_capture_for_overlay_chrome() {
        let mut ui = make_ui();
        let id = ui.overlay_open(stub_overlay());
        ui.set_capture(HitTarget::Chrome {
            owner: id,
            zone: overlay::ChromeZone::Body,
        });
        ui.overlay_close(id);
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn overlay_close_clears_capture_for_overlay_leaf() {
        let mut ui = make_ui();
        let id = ui.overlay_open(stub_overlay());
        ui.set_capture(HitTarget::Window(WinId(99)));
        ui.overlay_close(id);
        // The gesture that captured the leaf ends with the overlay
        // it lived in.
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn overlay_close_keeps_capture_for_unrelated_target() {
        let mut ui = make_ui();
        let id = ui.overlay_open(stub_overlay());
        let other = WinId(50);
        make_split(&mut ui, other);
        ui.set_capture(HitTarget::Scrollbar { owner: other });
        ui.overlay_close(id);
        assert_eq!(ui.capture(), Some(HitTarget::Scrollbar { owner: other }));
    }

    #[test]
    fn focused_painted_split_cursor_returns_hardware_cursor_position() {
        let mut ui = make_ui();
        ui.set_terminal_size(20, 4);
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        // Tree resolves to (top=0, left=0, width=20, height=4) — the
        // full terminal — so cursor_row=1 / cursor_col=3 → (3, 1).
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        ui.set_focus(win);
        let w = ui.win_mut(win).unwrap();
        w.set_cursor_position(1, 3);
        assert_eq!(ui.focused_painted_split_cursor(), Some((3, 1)));
    }

    #[test]
    fn focused_painted_split_cursor_returns_none_when_unfocused() {
        let mut ui = make_ui();
        ui.set_terminal_size(20, 4);
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        let w = ui.win_mut(win).unwrap();
        w.set_cursor_position(0, 0);
        // No focus call → focus stays None.
        assert_eq!(ui.focused_painted_split_cursor(), None);
    }

    #[test]
    fn handle_key_routes_to_overlay_leaf_callback() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "t".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(Constraint::Length(3), LayoutTree::leaf(win))]);
        let oid = ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true));
        let cb: Callback = Callback::Rust(Box::new(move |ctx| {
            if let Some(o) = ctx.ui.overlay_for_leaf(ctx.win) {
                let _ = ctx.ui.overlay_close(o);
            }
            CallbackResult::Consumed
        }));
        let _ = ui.win_set_keymap(
            win,
            KeyBind::new(
                crossterm::event::KeyCode::Char('q'),
                crossterm::event::KeyModifiers::NONE,
            ),
            cb,
        );
        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, Status::Consumed);
        assert!(ui.overlay(oid).is_none());
    }

    #[test]
    fn callback_result_event_dispatches_winevent_after_keymap() {
        // A built-in keymap callback (e.g. a list's Enter binding)
        // returns `CallbackResult::Event(Submit, payload)`. The
        // dispatcher must follow up with `fire_win_event` so any
        // registered `on_event(win, "submit", ...)` handler fires.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "list".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        // Wrap the win in a modal overlay so it becomes a focusable
        // leaf reachable via `set_focus`.
        let layout = LayoutTree::vbox(vec![(Constraint::Length(3), LayoutTree::leaf(win))]);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true));

        let submit_cb: Callback = Callback::Rust(Box::new(|_| {
            CallbackResult::Event(WinEvent::Submit, Payload::Selection { index: 7 })
        }));
        let _ = ui.win_set_keymap(
            win,
            KeyBind::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
            submit_cb,
        );

        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
        let observed_cb = observed.clone();
        ui.win_on_event(
            win,
            WinEvent::Submit,
            Callback::Rust(Box::new(move |ctx| {
                if let Payload::Selection { index } = ctx.payload {
                    observed_cb.lock().unwrap().push(index);
                }
                CallbackResult::Consumed
            })),
        );

        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, Status::Consumed);
        assert_eq!(*observed.lock().unwrap(), vec![7]);
    }

    #[test]
    fn dispatch_event_esc_closes_active_modal() {
        let mut ui = make_ui();
        let id = ui.overlay_open(modal_overlay_with_leaves(WinId(50), WinId(51), WinId(52)));
        assert_eq!(ui.active_modal(), Some(id));
        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, Status::Consumed);
        assert_eq!(ui.active_modal(), None);
    }

    #[test]
    fn dispatch_event_esc_with_modifiers_does_not_dismiss_modal() {
        let mut ui = make_ui();
        let id = ui.overlay_open(modal_overlay_with_leaves(WinId(50), WinId(51), WinId(52)));
        // Esc + Shift falls through to normal dispatch — built-in
        // dismiss is bare Esc only.
        let _ = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::SHIFT,
        );
        assert_eq!(ui.active_modal(), Some(id));
    }

    #[test]
    fn modal_esc_fires_dismiss_once_on_overlay_root() {
        // Multi-panel overlay: dialog.lua registers
        // `on_event("dismiss", …)` on the dialog's root WinId (the
        // first leaf in declaration order, returned from `_open`).
        // Esc must fire Dismiss exactly once on the root — not
        // once per leaf — so dialog.lua's single handler runs once
        // and the parked task resumes once. Non-root leaves with
        // their own Dismiss callbacks are addressed via root
        // redirect inside `fire_win_event`.
        let mut ui = make_ui();
        let a = WinId(60);
        let b = WinId(61);
        let c = WinId(62);
        let id = ui.overlay_open(modal_overlay_with_leaves(a, b, c));
        let count = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        // Only the root (a) gets a callback — like dialog.lua does.
        let count_cb = count.clone();
        ui.win_on_event(
            a,
            WinEvent::Dismiss,
            Callback::Rust(Box::new(move |_| {
                *count_cb.lock().unwrap() += 1;
                CallbackResult::Consumed
            })),
        );
        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, Status::Consumed);
        assert_eq!(*count.lock().unwrap(), 1);
        assert!(ui.overlay(id).is_none());
    }

    #[test]
    fn dispatch_event_ctrl_c_closes_active_modal() {
        let mut ui = make_ui();
        let id = ui.overlay_open(modal_overlay_with_leaves(WinId(50), WinId(51), WinId(52)));
        assert_eq!(ui.active_modal(), Some(id));
        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(result, Status::Consumed);
        assert_eq!(ui.active_modal(), None);
    }

    #[test]
    fn modal_ctrl_c_fires_dismiss_once_on_overlay_root() {
        let mut ui = make_ui();
        let a = WinId(60);
        let b = WinId(61);
        let c = WinId(62);
        let id = ui.overlay_open(modal_overlay_with_leaves(a, b, c));
        let count = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let count_cb = count.clone();
        ui.win_on_event(
            a,
            WinEvent::Dismiss,
            Callback::Rust(Box::new(move |_| {
                *count_cb.lock().unwrap() += 1;
                CallbackResult::Consumed
            })),
        );
        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(result, Status::Consumed);
        assert_eq!(*count.lock().unwrap(), 1);
        assert!(ui.overlay(id).is_none());
    }

    #[test]
    fn modal_esc_consumed_by_focused_leaf_does_not_dismiss() {
        // Esc chain: the focused window gets first dibs. If its
        // keymap consumes Esc, the modal stays open.
        let mut ui = make_ui();
        let a = WinId(60);
        let b = WinId(61);
        let id = ui.overlay_open(modal_overlay_with_leaves(a, b, WinId(62)));
        ui.set_focus(a);

        let esc_consumed = std::sync::Arc::new(std::sync::Mutex::new(false));
        let esc_cb = esc_consumed.clone();
        let cb: Callback = Callback::Rust(Box::new(move |_| {
            *esc_cb.lock().unwrap() = true;
            CallbackResult::Consumed
        }));
        let _ = ui.win_set_keymap(
            a,
            KeyBind::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            cb,
        );

        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, Status::Consumed);
        assert!(*esc_consumed.lock().unwrap());
        // Modal stays open because the leaf consumed Esc.
        assert!(ui.overlay(id).is_some());
    }

    #[test]
    fn fire_win_event_on_non_root_leaf_redirects_to_root() {
        // When a callback fires `WinEvent::Submit` on a non-root
        // leaf (e.g. an input panel below an options panel),
        // `fire_win_event` redirects to the overlay's root so the
        // dialog.lua handler registered on the root sees it.
        let mut ui = make_ui();
        let a = WinId(70);
        let b = WinId(71);
        let _id = ui.overlay_open(modal_overlay_with_leaves(a, b, WinId(72)));
        let saw = std::sync::Arc::new(std::sync::Mutex::new(false));
        let saw_cb = saw.clone();
        ui.win_on_event(
            a,
            WinEvent::Submit,
            Callback::Rust(Box::new(move |_| {
                *saw_cb.lock().unwrap() = true;
                CallbackResult::Consumed
            })),
        );
        // Fire Submit on the NON-root leaf; root's callback should fire.
        ui.fire_win_event(b, WinEvent::Submit, Payload::None, &mut |_, _, _| {});
        assert!(*saw.lock().unwrap());
    }

    #[test]
    fn win_close_on_overlay_leaf_closes_overlay_and_clears_all_leaves() {
        // Lua flow: `smelt.win.close(win_id)` is the canonical way for
        // a dialog to dismiss itself. When `win_id` is a leaf of an
        // open overlay the call must close the whole overlay (not just
        // detach one panel) and clear callbacks for every leaf so the
        // Lua-side registry drops them in lockstep.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win_a = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "a".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let win_b = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "b".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![
            (Constraint::Length(3), LayoutTree::leaf(win_a)),
            (Constraint::Length(3), LayoutTree::leaf(win_b)),
        ]);
        let oid = ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true));
        let cb_noop: Callback = Callback::Rust(Box::new(|_| CallbackResult::Consumed));
        ui.win_on_event(win_a, WinEvent::Dismiss, cb_noop);
        let cb_noop2: Callback = Callback::Rust(Box::new(|_| CallbackResult::Consumed));
        ui.win_on_event(win_b, WinEvent::Dismiss, cb_noop2);

        let _ = ui.win_close(win_a);

        assert!(ui.overlay(oid).is_none());
        // Both leaves' Window entries gone from the registry.
        assert!(ui.win(win_a).is_none());
        assert!(ui.win(win_b).is_none());
        // Closing again is a no-op — overlay is already gone.
        assert_eq!(ui.win_close(win_a), Vec::<u64>::new());
    }

    #[test]
    fn render_drives_ensure_rendered_at_for_each_overlay_leaf() {
        // Plain / markdown / diff parsers populate the buffer's lines
        // lazily on `ensure_rendered_at(width)`. The overlay paint walk
        // takes immutable references and can't drive the parser, so
        // `Ui::render` must do a pre-pass that calls
        // `ensure_rendered_at` for each leaf at the leaf's resolved
        // width before paint.
        use std::sync::{Arc, Mutex};
        struct WidthRecorder {
            calls: Arc<Mutex<Vec<u16>>>,
        }
        impl BufferParser for WidthRecorder {
            fn parse(&self, buf: &mut Buffer, _source: &str, width: u16) {
                self.calls.lock().unwrap().push(width);
                buf.set_all_lines(vec![format!("rendered@{width}")]);
            }
        }
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let calls = Arc::new(Mutex::new(Vec::<u16>::new()));
        if let Some(b) = ui.buf_mut(buf) {
            b.set_parser(Arc::new(WidthRecorder {
                calls: calls.clone(),
            }));
            b.set_source("seed".into());
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(3),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        // Outer overlay: 42×5 (40 leaf width + 2 border, 3 leaf height +
        // 2 border). Leaf rect width = 40 ⇒ parser called with 40.
        let widths = calls.lock().unwrap().clone();
        assert!(
            widths.contains(&40),
            "parser must be invoked at the leaf's resolved width; got {widths:?}"
        );
    }

    #[test]
    fn render_paints_overlay_leaf_buffer() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines(vec!["overlay-text".into()]);
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(3),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE)
        .with_title("title");
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        // Borrow Compositor's previous grid (post-flush swap) for assertions.
        let frame = ui.surface.compositor().previous();
        // Centered (term 80x24, overlay natural 42 wide × 5 tall →
        // top=9 left=19). Title sits in the top border row at col=20.
        assert_eq!(frame.cell(19, 9).symbol, '┌');
        assert_eq!(frame.cell(20, 9).symbol, 't');
        assert_eq!(frame.cell(24, 9).symbol, 'e');
        // Leaf paints inside the border at (top+1, left+1) = (10, 20).
        assert_eq!(frame.cell(20, 10).symbol, 'o');
        assert_eq!(frame.cell(31, 10).symbol, 't');
    }

    // ── UiHost trait dispatch ────────────────────────────────────────

    #[test]
    fn ui_host_dispatch_round_trips_through_dyn() {
        // Drive every UiHost method through `&mut dyn UiHost` so the
        // trait shape is exercised end-to-end (not just the inherent
        // path). Mirrors how Lua bindings reach the compositor — by
        // trait, not by direct field access.
        fn drive(host: &mut dyn UiHost) -> (BufId, WinId, OverlayId) {
            let buf = host.buf_create(BufCreateOpts::default());
            host.buf_mut(buf)
                .unwrap()
                .set_all_lines(vec!["uihost".into()]);
            let win = host
                .win_open_split(
                    buf,
                    SplitConfig {
                        region: "uihost-test".into(),
                        gutters: Gutters::default(),
                    },
                )
                .unwrap();
            host.win_mut(win).unwrap().set_cursor_position(0, 3);
            // Hosting `win` in a modal overlay both makes it focusable
            // (overlay leaf) and exercises `overlay_open`. The modal
            // also auto-focuses the first leaf — re-asserting via the
            // explicit `set_focus` keeps that method on the trait path.
            let layout = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(win))]);
            let oid =
                host.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true));
            assert!(host.set_focus(win));
            // `ui()` must yield the same compositor every other method
            // mutates; assert the focused window matches what we just set.
            assert_eq!(host.ui().focus(), Some(win));
            (buf, win, oid)
        }

        let mut ui = make_ui();
        let (buf, win, oid) = drive(&mut ui);

        // Fire-event path through the trait. The callback observes the
        // payload that the trait dispatch threaded through.
        let saw = std::sync::Arc::new(std::sync::Mutex::new(false));
        let saw_cb = saw.clone();
        ui.win_on_event(
            win,
            WinEvent::TextChanged,
            Callback::Rust(Box::new(move |_| {
                *saw_cb.lock().unwrap() = true;
                CallbackResult::Consumed
            })),
        );
        ui.fire_win_event(
            win,
            WinEvent::TextChanged,
            Payload::Text {
                content: "uihost".into(),
            },
            &mut |_, _, _| {},
        );
        assert!(*saw.lock().unwrap());

        // Close paths through the trait clean up the structures the
        // open paths created.
        let removed = ui.overlay_close(oid);
        assert!(removed.is_some());
        let cb_ids = UiHost::win_close(&mut ui, win);
        assert!(cb_ids.is_empty());
        assert!(ui.buf(buf).is_some());
    }

    // ── UiHost per-pane data accessors ───────────────────────────────

    #[test]
    fn ui_host_per_pane_data_default_impl() {
        // Ui's default `rows_for` / `breaks_for` / `viewport_for` cover
        // any window the host hasn't overridden — buffer lines as rows,
        // join positions as hard breaks, no soft wraps. Drives all
        // three through `&mut dyn UiHost` so the trait shape is
        // exercised end-to-end.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        ui.buf_mut(buf)
            .unwrap()
            .set_all_lines(vec!["hello".into(), "world!".into(), "ok".into()]);
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "per-pane-default".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let rect = layout::Rect::new(0, 0, 20, 10);
        ui.win_mut(win).unwrap().viewport = Some(window::WindowViewport::new(rect, 20, 0, 0, None));

        fn assert_default_shape(host: &mut dyn UiHost, win: WinId) {
            let vp = host.viewport_for(win).unwrap();
            assert_eq!(vp.rect.width, 20);
            let rows = host.rows_for(win).unwrap();
            assert_eq!(rows, vec!["hello", "world!", "ok"]);
            // "hello\nworld!\nok" — `\n` after "hello" lives at byte 5,
            // `\n` after "world!" at byte 12. Both are hard breaks; soft
            // breaks are empty for an unwrapped buffer.
            let (soft, hard) = host.breaks_for(win).unwrap();
            assert!(soft.is_empty(), "default impl emits no soft breaks");
            assert_eq!(hard, vec![5, 12]);
        }
        assert_default_shape(&mut ui, win);

        // Unknown window → `None` for every accessor.
        let stranger = WinId(9999);
        assert!(UiHost::viewport_for(&ui, stranger).is_none());
        assert!(UiHost::rows_for(&mut ui, stranger).is_none());
        assert!(UiHost::breaks_for(&mut ui, stranger).is_none());
    }

    #[test]
    fn record_click_caps_at_three_then_wraps() {
        let mut ui = make_ui();
        // Same cell, no time gap → climbs to 3, then wraps.
        assert_eq!(ui.record_click(5, 7), 1);
        assert_eq!(ui.record_click(5, 7), 2);
        assert_eq!(ui.record_click(5, 7), 3);
        assert_eq!(ui.record_click(5, 7), 1);
        // Different cell resets the count.
        assert_eq!(ui.record_click(5, 7), 2);
        assert_eq!(ui.record_click(8, 7), 1);
    }

    /// Set up a single splits leaf at `(0, 0, 20, 10)` with a painted
    /// scrollbar at column 19 covering 100 rows of content. Returns
    /// the leaf's `WinId` so callers can latch capture / hit-test
    /// against it.
    fn make_scrollbar_split(ui: &mut Ui) -> WinId {
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        let rect = layout::Rect::new(0, 0, 20, 10);
        let bar = window::ScrollbarState::new(19, 100, 10).unwrap();
        let w = ui.win_mut(win).unwrap();
        w.viewport = Some(window::WindowViewport::new(rect, 19, 100, 0, Some(bar)));
        win
    }

    fn mouse_event(kind: crossterm::event::MouseEventKind, row: u16, col: u16) -> Event {
        Event::Mouse(crossterm::event::MouseEvent {
            kind,
            row,
            column: col,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
    }

    #[test]
    fn hit_test_returns_scrollbar_for_splits_leaf_with_painted_bar() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        // Column 19 is the scrollbar column; rows 0..10 are inside the
        // viewport.
        assert_eq!(
            ui.hit_test(3, 19, None),
            Some(HitTarget::Scrollbar { owner: win })
        );
        // Same row, column 18 → content, not scrollbar.
        assert_eq!(ui.hit_test(3, 18, None), Some(HitTarget::Window(win)));
    }

    #[test]
    fn dispatch_mouse_left_down_on_scrollbar_latches_capture_and_snaps_scroll() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        let down = mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            9,
            19,
        );
        let status = ui.dispatch_event(down, &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        assert_eq!(ui.capture(), Some(HitTarget::Scrollbar { owner: win }));
        // Bottom row click → bar snaps to max scroll (90 = total - viewport).
        assert_eq!(ui.win(win).unwrap().scroll_top, 90);
    }

    #[test]
    fn dispatch_mouse_left_drag_with_scrollbar_capture_resnaps_scroll() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Scrollbar { owner: win });
        let drag = mouse_event(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            5,
            5,
        );
        let status = ui.dispatch_event(drag, &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        // Capture survives the drag.
        assert_eq!(ui.capture(), Some(HitTarget::Scrollbar { owner: win }));
        // Mid-track drag advances scroll past zero.
        assert!(ui.win(win).unwrap().scroll_top > 0);
    }

    #[test]
    fn dispatch_mouse_left_up_with_scrollbar_capture_clears_it() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Scrollbar { owner: win });
        let up = mouse_event(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            0,
            0,
        );
        let status = ui.dispatch_event(up, &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn dispatch_mouse_left_down_off_scrollbar_returns_ignored() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        let down = mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            3,
            5,
        );
        let status = ui.dispatch_event(down, &mut |_, _, _| {});
        // Splits leaf without scrollbar capture is host-routed.
        assert_eq!(status, Status::Ignored);
        assert_eq!(ui.capture(), None);
        let _ = win;
    }

    fn raw_mouse_event(
        kind: crossterm::event::MouseEventKind,
        row: u16,
        col: u16,
    ) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            row,
            column: col,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    #[test]
    fn resolve_split_mouse_down_latches_window_capture_and_records_click() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        // Click on content (col 5, row 3) — not on the scrollbar.
        let me = raw_mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            3,
            5,
        );
        let resolved = ui.resolve_split_mouse(me);
        assert_eq!(resolved, Some((win, 1)));
        assert_eq!(ui.capture(), Some(HitTarget::Window(win)));
        // A second Down on the same cell increments the click count.
        let resolved = ui.resolve_split_mouse(me);
        assert_eq!(resolved, Some((win, 2)));
    }

    #[test]
    fn resolve_split_mouse_drag_routes_to_captured_window_off_rect() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        // Drag at (50, 50) — well outside the leaf rect — still routes
        // to `win` because capture is latched.
        let drag = raw_mouse_event(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            50,
            50,
        );
        let resolved = ui.resolve_split_mouse(drag);
        assert_eq!(resolved, Some((win, 0)));
        assert_eq!(ui.capture(), Some(HitTarget::Window(win)));
    }

    #[test]
    fn resolve_split_mouse_up_clears_window_capture() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        let up = raw_mouse_event(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            0,
            0,
        );
        let resolved = ui.resolve_split_mouse(up);
        assert_eq!(resolved, Some((win, 0)));
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn resolve_split_mouse_down_on_scrollbar_returns_none() {
        let mut ui = make_ui();
        let _win = make_scrollbar_split(&mut ui);
        // Click on the scrollbar column — Ui::dispatch_event handles
        // that gesture; resolve_split_mouse declines.
        let me = raw_mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            3,
            19,
        );
        assert_eq!(ui.resolve_split_mouse(me), None);
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn resolve_split_mouse_orphan_drag_returns_none() {
        let mut ui = make_ui();
        let _win = make_scrollbar_split(&mut ui);
        let drag = raw_mouse_event(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            3,
            5,
        );
        assert_eq!(ui.resolve_split_mouse(drag), None);
    }

    #[test]
    fn resolve_split_mouse_non_left_returns_none() {
        let mut ui = make_ui();
        let _win = make_scrollbar_split(&mut ui);
        let me = raw_mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
            3,
            5,
        );
        assert_eq!(ui.resolve_split_mouse(me), None);
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn poll_drag_autoscroll_returns_none_without_window_capture() {
        let mut ui = make_ui();
        let _win = make_scrollbar_split(&mut ui);
        assert_eq!(ui.poll_drag_autoscroll(), None);
        assert_eq!(ui.drag_autoscroll_started(), None);
    }

    #[test]
    fn poll_drag_autoscroll_fires_at_top_edge_and_latches_started_at() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        ui.win_mut(win).unwrap().set_cursor_position(0, 0);
        let result = ui.poll_drag_autoscroll();
        assert_eq!(result, Some((win, -1)));
        assert!(ui.drag_autoscroll_started().is_some());
    }

    #[test]
    fn poll_drag_autoscroll_fires_at_bottom_edge() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        // make_scrollbar_split paints a viewport with rect height = 10,
        // so cursor_row=9 is the bottom row.
        ui.win_mut(win).unwrap().set_cursor_position(9, 0);
        assert_eq!(ui.poll_drag_autoscroll(), Some((win, 1)));
    }

    #[test]
    fn poll_drag_autoscroll_clears_started_at_when_cursor_leaves_edge() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        ui.win_mut(win).unwrap().set_cursor_position(0, 0);
        let _ = ui.poll_drag_autoscroll();
        assert!(ui.drag_autoscroll_started().is_some());
        ui.win_mut(win).unwrap().set_cursor_position(5, 0);
        assert_eq!(ui.poll_drag_autoscroll(), None);
        assert_eq!(ui.drag_autoscroll_started(), None);
    }

    #[test]
    fn poll_drag_autoscroll_clears_started_at_when_capture_releases() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        ui.win_mut(win).unwrap().set_cursor_position(0, 0);
        let _ = ui.poll_drag_autoscroll();
        assert!(ui.drag_autoscroll_started().is_some());
        ui.clear_capture();
        assert_eq!(ui.drag_autoscroll_started(), None);
        assert_eq!(ui.poll_drag_autoscroll(), None);
    }

    #[test]
    fn poll_drag_autoscroll_returns_none_for_scrollbar_capture() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Scrollbar { owner: win });
        ui.win_mut(win).unwrap().set_cursor_position(0, 0);
        assert_eq!(ui.poll_drag_autoscroll(), None);
    }
}
