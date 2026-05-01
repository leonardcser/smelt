pub mod buffer;
pub mod callback;
pub mod clipboard;
pub mod compositor;
pub mod dialog;
pub mod edit_buffer;
pub mod event;
pub mod flush;
pub mod grid;
pub mod kill_ring;
pub mod layout;
pub mod motions;
pub mod overlay;
pub mod style;
pub mod text;
pub mod text_objects;
pub mod theme;
pub mod undo;
pub mod vim;
pub mod window;

mod id;

pub type AttachmentId = u64;

/// Callback shape for routing `Callback::Lua` handles out of Ui into
/// the host's Lua runtime. Receives the handle, the focused window,
/// and the event payload.
pub type LuaInvoke<'a> = dyn FnMut(callback::LuaHandle, id::WinId, &callback::Payload) + 'a;

pub use buffer::{Buffer, BufferParser, Span, SpanStyle};
pub use callback::{
    Callback, CallbackCtx, CallbackResult, Callbacks, KeyBind, LuaHandle, Payload, RustCallback,
    WinEvent,
};
pub use clipboard::{Clipboard, NullSink, Sink};
pub use compositor::Compositor;
pub use dialog::{PanelHeight, PanelSpec};
pub use edit_buffer::EditBuffer;
pub use event::{Event, FocusTarget, Status};
pub use flush::flush_diff;
pub use grid::{Cell, Grid, GridSlice, Style};
pub use id::{BufId, WinId, LUA_BUF_ID_BASE};
pub use kill_ring::KillRing;
pub use layout::{Anchor, Border, Constraint, Corner, Gutters, LayoutTree, Rect, SeparatorStyle};
pub use motions::FindKind;
pub use overlay::{HitTarget, Overlay, OverlayHitTarget, OverlayId};
pub use style::{HlAttrs, HlGroup};
pub use theme::Theme;
pub use undo::{UndoEntry, UndoHistory};
pub use vim::{VimMode, VimWindowState};
pub use window::{
    CursorShape, DrawContext, EventCtx, MouseCtx, ScrollbarState, SplitConfig, ViewportHit, Window,
    WindowViewport,
};

use std::collections::HashMap;

pub struct Ui {
    bufs: HashMap<BufId, Buffer>,
    wins: HashMap<WinId, Window>,
    next_buf_id: u64,
    next_win_id: u64,
    terminal_size: (u16, u16),
    compositor: Compositor,
    callbacks: Callbacks,
    /// Splits layout tree. The host publishes a fresh tree each frame
    /// via [`Ui::set_layout`]; rects are resolved against the current
    /// terminal area on demand. Leaves of this tree are the painted
    /// splits — Window::render paints each leaf in declaration order
    /// from `Ui::render`'s post-layer pass, before overlays.
    splits: LayoutTree,
    /// Theme registry — single source of truth for highlight groups.
    /// Cloned into every `DrawContext` at frame start so widgets read
    /// `ctx.theme.get(name)` instead of host-side colour constants. The
    /// host populates this at startup; users override via Lua.
    theme: Theme,
    /// Overlay storage. Each overlay is a positioned LayoutTree of
    /// windows; `Ui::overlay_open` returns an `OverlayId` and
    /// `resolve_anchor` is the per-frame positioning primitive.
    /// `Vec` preserves insertion order naturally — used as the
    /// secondary sort key in z-order ties (see
    /// [`Self::overlays_in_z_order`]).
    overlays: Vec<(OverlayId, Overlay)>,
    next_overlay_id: u32,
    /// Stack of prior focused windows. `set_focus` pushes the
    /// outgoing focus here; the overlay close paths walk back through
    /// it for the most recent still-existing focusable window.
    focus_history: Vec<WinId>,
    /// Currently focused window — the single source of truth for
    /// keyboard focus. May refer to an overlay leaf or a splits-tree
    /// leaf; the discrimination is derived at lookup time
    /// (`overlay_for_leaf` vs `splits.contains_leaf`).
    focus: Option<WinId>,
    /// In-flight gesture capture target. When set, mouse routing
    /// short-circuits hit-testing and delivers events to this target
    /// directly until the gesture ends (mouse-up clears it). Used by
    /// scrollbar drag — once the user grabs the thumb, subsequent
    /// drag rows must continue mapping to the same scrollbar even if
    /// the pointer wanders off the track. Auto-clears when the
    /// captured target's owning split disappears (`set_layout`) or
    /// owning overlay closes (`overlay_close`).
    capture: Option<HitTarget>,
    /// Last primary-button Down for click-count tracking. Successive
    /// Downs on the same cell within 400ms increment the count up to
    /// 3 (so the fourth click wraps back to 1). Tuple shape:
    /// `(time, row, col, count)`. The host calls `record_click` on
    /// each Down to get the next count. Once Ui owns Down dispatch
    /// (P2.b.4c.5), this becomes an internal helper.
    last_click: Option<(std::time::Instant, u16, u16, u8)>,
    /// Single global cursor shape for the focused window. The host
    /// sets this each frame from the focused window's mode/state
    /// (vim Insert → `Hardware`, vim Normal/Visual → `Block`,
    /// transcript content cursor → `Block`, nothing focused →
    /// `Hidden`). Read by paint paths into `DrawContext::cursor_shape`
    /// (only the focused window honours it) and by `Ui::render` to
    /// surface the hardware caret.
    cursor_shape: CursorShape,
    /// Timestamp drag-autoscroll was last engaged. `Some` while the
    /// user is mid-drag with the captured window's cursor parked at a
    /// viewport edge; `None` otherwise. The host's main loop reads
    /// [`Self::drag_autoscroll_started`] to ramp the tick interval
    /// down (~30ms → ~5ms) so selection-extension acceleration grows
    /// with how long the cursor has been parked at the edge.
    /// [`Self::poll_drag_autoscroll`] manages this slot internally.
    drag_autoscroll_since: Option<std::time::Instant>,
}

/// Reserved `WinId` for the main prompt input window. Stable id so Lua
/// can `smelt.win.on_event(prompt, …)` and `smelt.win.set_keymap(prompt, …)`
/// like any other window.
pub const PROMPT_WIN: WinId = WinId(0);

/// Reserved `WinId` for the transcript (scroll-back) window. Same
/// rationale as [`PROMPT_WIN`] — stable id for callback registration.
pub const TRANSCRIPT_WIN: WinId = WinId(1);

impl Ui {
    pub fn new() -> Self {
        Self {
            bufs: HashMap::new(),
            wins: HashMap::new(),
            next_buf_id: 1,
            // 0 is reserved for PROMPT_WIN, 1 for TRANSCRIPT_WIN.
            next_win_id: 2,
            terminal_size: (80, 24),
            compositor: Compositor::new(80, 24),
            callbacks: Callbacks::new(),
            splits: LayoutTree::vbox(Vec::new()),
            theme: Theme::new(),
            overlays: Vec::new(),
            next_overlay_id: 1,
            focus_history: Vec::new(),
            focus: None,
            capture: None,
            last_click: None,
            cursor_shape: CursorShape::Hidden,
            drag_autoscroll_since: None,
        }
    }

    /// Record a primary-button Down for click-count tracking and
    /// return the resulting count (1, 2, or 3). Successive Downs on
    /// the same cell within 400ms increment up to 3; the fourth
    /// click wraps back to 1 so a fresh gesture restarts. Internal
    /// helper for [`Self::resolve_split_mouse`].
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

    /// Resolve a primary-button Down/Drag/Up against the splits-leaf
    /// hit-test, latching a `HitTarget::Window(win)` capture on Down
    /// so subsequent Drag rows continue routing to the same Window
    /// even if the pointer drifts off its rect. Returns
    /// `Some((win, click_count))` when the host should forward the
    /// event to that Window's per-pane handler; `None` for any other
    /// kind, for hits outside splits leaves, or for orphan Drag/Up
    /// arriving without an established capture.
    ///
    /// `click_count` is the value [`Self::record_click`] returns on
    /// Down (1 / 2 / 3) and `0` for Drag/Up (no fresh click). Up
    /// clears the capture as a side-effect; on Down, capture is set
    /// before this method returns.
    ///
    /// Coexists with `Ui::dispatch_event`'s scrollbar capture: when
    /// `Ui::dispatch_event` claimed the event (scrollbar drag, wheel
    /// on overlay, modal click-outside), the host returns early and
    /// never calls this method. When `Ui::dispatch_event` returned
    /// `Ignored`, this method runs.
    pub fn resolve_split_mouse(&mut self, me: crossterm::event::MouseEvent) -> Option<(WinId, u8)> {
        use crossterm::event::{MouseButton, MouseEventKind};
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let win = match self.hit_test(me.row, me.column, None)? {
                    HitTarget::Window(w) if self.splits.contains_leaf(w) => w,
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

    /// Publish the splits layout for this frame. Leaves of the tree
    /// become the painted splits; their rects are resolved against
    /// the current terminal area on demand. If the focused window is
    /// no longer reachable as a splits leaf or overlay leaf after the
    /// swap, focus is cleared.
    pub fn set_layout(&mut self, tree: LayoutTree) {
        self.splits = tree;
        if let Some(focus) = self.focus {
            if !self.splits.contains_leaf(focus) && self.overlay_for_leaf(focus).is_none() {
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

    /// Read-only view of the splits tree. Test-only; production
    /// consumers rebuild the tree from app state and write it back
    /// via `set_layout`.
    #[cfg(test)]
    fn splits(&self) -> &LayoutTree {
        &self.splits
    }

    /// Resolve the splits tree against the current terminal area,
    /// returning the rect for each leaf. Walks the tree on every call
    /// — small trees in practice (3–4 leaves), so the cost is
    /// negligible.
    fn resolve_splits(&self) -> HashMap<WinId, Rect> {
        let (w, h) = self.terminal_size;
        let area = Rect::new(0, 0, w, h);
        layout::resolve_layout(&self.splits, area)
    }

    /// Resolved rect for a single splits leaf, or `None` when `win`
    /// isn't a leaf in the current splits tree.
    pub fn split_rect(&self, win: WinId) -> Option<Rect> {
        self.resolve_splits().get(&win).copied()
    }

    pub fn buf_create(&mut self, opts: buffer::BufCreateOpts) -> BufId {
        let id = BufId(self.next_buf_id);
        self.next_buf_id += 1;
        let buf = Buffer::new(id, opts);
        self.bufs.insert(id, buf);
        id
    }

    /// Create a buffer at an explicit id. Returns `Err` if a buffer
    /// with that id already exists — callers should mint a fresh id
    /// via `buf_create`. Plugin-facing IDs (Lua `smelt.buf.create`)
    /// live above `LUA_BUF_ID_BASE` so they can't collide with
    /// sequentially-allocated Rust buffers.
    pub fn buf_create_with_id(
        &mut self,
        id: BufId,
        opts: buffer::BufCreateOpts,
    ) -> Result<BufId, BufId> {
        if self.bufs.contains_key(&id) {
            return Err(id);
        }
        let buf = Buffer::new(id, opts);
        self.bufs.insert(id, buf);
        // Only advance the Rust-side allocator when the explicit id
        // sits inside Rust's own range. Lua-minted ids live above
        // `LUA_BUF_ID_BASE` and have their own atomic counter; pulling
        // `next_buf_id` past the base would make subsequent
        // `buf_create()` calls collide with the next Lua allocation.
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

    /// Mutably borrow the buffer backing `win`, if both the window and
    /// buffer exist.
    pub fn win_buf_mut(&mut self, win: WinId) -> Option<&mut Buffer> {
        let id = self.wins.get(&win)?.buf;
        self.bufs.get_mut(&id)
    }

    // ── Overlay (P1.c) ───────────────────────────────────────────────

    /// Register an overlay. Returns its stable `OverlayId`. The
    /// overlay's positioning is recomputed each frame from its
    /// `anchor` via `overlay::resolve_anchor`; mutate the anchor via
    /// `overlay_mut(id).anchor = …` to drag it.
    pub fn overlay_open(&mut self, overlay: Overlay) -> OverlayId {
        let id = OverlayId(self.next_overlay_id);
        self.next_overlay_id += 1;
        let modal = overlay.modal;
        let first_leaf = overlay.layout.leaves_in_order().into_iter().next();
        self.overlays.push((id, overlay));
        if modal {
            if let Some(leaf) = first_leaf {
                self.set_focus(leaf);
            }
        }
        id
    }

    /// Close an overlay. Returns the removed `Overlay` for callers
    /// that want to inspect its layout (e.g. to close the contained
    /// windows individually). When the currently-focused window is
    /// a leaf of the closed overlay, walks `focus_history` backward
    /// to the most recent still-focusable `WinId` and restores
    /// focus there. If the history is exhausted (or all entries are
    /// stale), focus is cleared. Focus on a window outside the
    /// closed overlay is preserved untouched, and `focus_history`
    /// is left alone.
    pub fn overlay_close(&mut self, id: OverlayId) -> Option<Overlay> {
        let pos = self.overlays.iter().position(|(oid, _)| *oid == id)?;
        let (_, removed) = self.overlays.remove(pos);
        // Clear capture when the closing overlay owned the gesture —
        // either chrome of this overlay or a leaf inside its layout.
        if let Some(cap) = self.capture {
            let owned = match cap {
                HitTarget::Chrome { owner } => owner == id,
                HitTarget::Window(w) | HitTarget::Scrollbar { owner: w } => {
                    removed.layout.contains_leaf(w)
                }
            };
            if owned {
                self.capture = None;
                self.drag_autoscroll_since = None;
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
                    if self.splits.contains_leaf(prior)
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

    /// Iterate overlays in stacking order (lowest `z` first; ties
    /// broken by insertion order — the live vec already carries
    /// insertion order, and `sort_by_key` is stable).
    fn overlays_in_z_order(&self) -> Vec<(OverlayId, &Overlay)> {
        let mut entries: Vec<(OverlayId, &Overlay)> =
            self.overlays.iter().map(|(id, o)| (*id, o)).collect();
        entries.sort_by_key(|(_, o)| o.z);
        entries
    }

    /// Topmost modal overlay, if any. "Topmost" = highest `z`; ties
    /// broken by insertion order (later open wins). Engine-drain gating
    /// (don't pull engine events while a modal is up) and modal-aware
    /// focus cycling (Tab stays inside the overlay) read this.
    pub fn active_modal(&self) -> Option<OverlayId> {
        self.overlays_in_z_order()
            .into_iter()
            .rev()
            .find_map(|(id, ov)| ov.modal.then_some(id))
    }

    /// Overlay containing the currently-focused window, if focus is
    /// inside one. Pure structural query — walks open overlays and
    /// asks whether the focused `WinId` appears as a leaf in their
    /// layouts. Returns `None` when focus is on a split window or
    /// nothing is focused.
    pub fn focused_overlay(&self) -> Option<OverlayId> {
        let focused = self.focus()?;
        self.overlays
            .iter()
            .find_map(|(id, ov)| ov.layout.contains_leaf(focused).then_some(*id))
    }

    /// Unified hit-test for a screen position. Returns the target
    /// the cell belongs to: an overlay leaf or chrome, or a splits
    /// leaf underneath. Overlays are checked first (topmost-z to
    /// lowest, modal-aware — see `overlay_hit_test`); when no overlay
    /// covers the point, walks splits leaves in declaration order.
    /// Splits leaves with a painted scrollbar return
    /// [`HitTarget::Scrollbar`] when the click lands on the bar's
    /// column; the rest of the leaf returns [`HitTarget::Window`].
    pub fn hit_test(&self, row: u16, col: u16, cursor: Option<(u16, u16)>) -> Option<HitTarget> {
        if let Some((id, target)) = self.overlay_hit_test(row, col, cursor) {
            return Some(match target {
                OverlayHitTarget::Window(w) => HitTarget::Window(w),
                OverlayHitTarget::Chrome => HitTarget::Chrome { owner: id },
            });
        }
        let split_rects = self.resolve_splits();
        for win in self.splits.leaves_in_order() {
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

    /// Hit-test a screen position against the open overlay set.
    /// Returns the topmost overlay whose resolved rect contains
    /// `(row, col)`, plus whether the hit landed on one of its leaf
    /// `Window`s or its chrome (border, title, gap, padding).
    /// `None` when no overlay covers the point. When a modal is
    /// active, only the modal and overlays at or above its `z`
    /// receive hits — lower-z overlays are blocked even if their
    /// rect contains the point. `cursor` is forwarded to
    /// [`Self::resolve_overlays`] for `Anchor::Cursor` resolution.
    fn overlay_hit_test(
        &self,
        row: u16,
        col: u16,
        cursor: Option<(u16, u16)>,
    ) -> Option<(OverlayId, OverlayHitTarget)> {
        let modal_z = self
            .active_modal()
            .and_then(|id| self.overlay(id).map(|o| o.z));
        // Topmost first.
        let mut resolved = self.resolve_overlays(cursor);
        resolved.reverse();
        for (id, rect, ov) in resolved {
            if let Some(min_z) = modal_z {
                if ov.z < min_z {
                    continue;
                }
            }
            if !rect.contains(row, col) {
                continue;
            }
            // Inside the overlay rect — is it a leaf or chrome?
            let leaf_rects = layout::resolve_layout(&ov.layout, rect);
            for (win_id, leaf_rect) in &leaf_rects {
                if leaf_rect.contains(row, col) {
                    return Some((id, OverlayHitTarget::Window(*win_id)));
                }
            }
            return Some((id, OverlayHitTarget::Chrome));
        }
        None
    }

    /// Resolve every overlay's screen rect for the upcoming frame.
    /// Returns z-ordered entries (lowest first) for which the anchor
    /// resolved — overlays whose `Anchor::Cursor` requires a missing
    /// `cursor`, or whose `Anchor::Win` target is absent from the
    /// splits tree, are skipped silently. The caller (compositor
    /// integration in C.5+) feeds the cursor it knows from focus.
    fn resolve_overlays(&self, cursor: Option<(u16, u16)>) -> Vec<(OverlayId, Rect, &Overlay)> {
        let (term_w, term_h) = self.terminal_size;
        let split_rects = self.resolve_splits();
        let ctx = overlay::AnchorContext {
            term_width: term_w,
            term_height: term_h,
            cursor,
            win_rects: &split_rects,
        };
        let mut out = Vec::with_capacity(self.overlays.len());
        for (id, ov) in self.overlays_in_z_order() {
            let size = ov.layout.natural_size((term_w, term_h));
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
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;
        let win = Window::new(id, buf, config);
        self.wins.insert(id, win);
        Some(id)
    }

    /// Open a window at a pre-reserved `WinId` (e.g. [`PROMPT_WIN`],
    /// [`TRANSCRIPT_WIN`]). Returns `false` when the id is already
    /// occupied or the buffer doesn't exist. Used by frontends that
    /// want a Window with a stable id callers can register Lua
    /// callbacks against — the reserved-id machinery skips fresh
    /// allocation, so this is the only path that lands a Window at
    /// id 0/1.
    pub fn win_open_split_at(&mut self, id: WinId, buf: BufId, config: SplitConfig) -> bool {
        if self.wins.contains_key(&id) || !self.bufs.contains_key(&buf) {
            return false;
        }
        let win = Window::new(id, buf, config);
        self.wins.insert(id, win);
        true
    }

    /// Close a window. Returns the Lua callback IDs that were
    /// attached (keymaps, events, fallback) so the caller can drop
    /// them from the Lua-side registry. When `id` is a leaf of an
    /// open overlay, closes the overlay and clears callbacks for
    /// every leaf in that overlay's tree (a single dialog typically
    /// renders as one leaf today; multi-panel overlays keep each
    /// leaf's bindings independent so close clears all of them
    /// atomically).
    #[must_use]
    pub fn win_close(&mut self, id: WinId) -> Vec<u64> {
        if let Some(overlay_id) = self.overlay_for_leaf(id) {
            let mut all_ids = Vec::new();
            if let Some(removed) = self.overlay_close(overlay_id) {
                for leaf in removed.layout.leaves_in_order() {
                    all_ids.extend(self.callbacks.clear_all(leaf));
                    self.wins.remove(&leaf);
                }
            }
            return all_ids;
        }
        self.wins.remove(&id);
        self.callbacks.clear_all(id)
    }

    // ── Callbacks ────────────────────────────────────────────────────
    //
    // Per-window keymap + event callbacks. The registry is the single
    // behavior mechanism shared by Rust and Lua.

    /// Bind a key chord on a specific window to a callback. Returns
    /// the displaced callback (if any) so Lua-side handles can be
    /// cleaned up by the caller.
    #[must_use]
    pub fn win_set_keymap(&mut self, win: WinId, key: KeyBind, cb: Callback) -> Option<Callback> {
        self.callbacks.set_keymap(win, key, cb)
    }

    /// Remove a keymap binding. Returns the removed callback, if any.
    #[must_use]
    pub fn win_clear_keymap(&mut self, win: WinId, key: KeyBind) -> Option<Callback> {
        self.callbacks.clear_keymap(win, key)
    }

    /// Register a catch-all key handler for a window. Runs after
    /// specific keymaps miss. Returns the displaced callback (if
    /// any).
    #[must_use]
    pub fn win_set_key_fallback(&mut self, win: WinId, cb: Callback) -> Option<Callback> {
        self.callbacks.set_key_fallback(win, cb)
    }

    /// Register a callback for a window lifecycle / semantic event.
    /// Multiple callbacks per (win, event) are supported and fire
    /// in registration order.
    pub fn win_on_event(&mut self, win: WinId, ev: WinEvent, cb: Callback) {
        self.callbacks.on_event(win, ev, cb);
    }

    /// Remove a specific event callback by its Lua handle id. Used by
    /// plugins that install cross-window handlers (e.g. a picker that
    /// listens to `text_changed` on the prompt) and need to tear down
    /// exactly their own binding on close.
    #[must_use]
    pub fn win_clear_event_by_id(&mut self, win: WinId, ev: WinEvent, id: u64) -> Option<Callback> {
        self.callbacks.clear_event_by_id(win, ev, id)
    }

    /// Fire a `WinEvent` on a window's registered callbacks.
    /// `lua_invoke` is called for each `Callback::Lua` with
    /// (handle, payload). Side effects flow through the `AppOp` queue
    /// that Rust callbacks have via `shared.ops` — no return channel.
    ///
    /// Overlay leaves redirect to the overlay's root leaf (first in
    /// declaration order). `dialog.lua` registers handlers on the
    /// `win_id` returned from `_open` (which is the root); events
    /// fired on any leaf bubble up so mixed dialogs hear them.
    ///
    /// Matches `UiHost::fire_win_event` from the target architecture
    /// — when `Host` / `UiHost` land in P2, this is the method the
    /// trait method delegates to.
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
        self.terminal_size = (w, h);
        self.compositor.resize(w, h);
    }

    pub fn terminal_size(&self) -> (u16, u16) {
        self.terminal_size
    }

    // ── Focus (canonical Win-typed API) ──────────────────────────

    /// Currently focused window, if any. Overlay-leaf focus wins
    /// over painted-split focus (a modal overlay's input claim
    /// suppresses split dispatch).
    pub fn focus(&self) -> Option<WinId> {
        self.focus
    }

    /// Currently focused `Window`, if its id is registered in
    /// `wins`. Convenience over `focus()` for callers that need the
    /// struct (cursor / selection / config). Splits whose `Window`
    /// hasn't been inserted into `wins` (e.g. the prompt /
    /// transcript pseudo-windows) return `None` here even when
    /// focused — `focus()` is the canonical reader.
    pub fn focused_window(&self) -> Option<&Window> {
        self.wins.get(&self.focus()?)
    }

    /// Focus a specific window. Accepts focusable splits leaves and
    /// overlay leaves (any leaf reachable in an open overlay's
    /// `LayoutTree`). Returns `false` when `win` is neither. On
    /// success, the prior focus is appended to `focus_history` so
    /// close paths can pop back to the previous focus target.
    /// Re-focusing the already-focused window is a no-op (no history
    /// push).
    pub fn set_focus(&mut self, win: WinId) -> bool {
        let prior = self.focus;
        if prior == Some(win) {
            return true;
        }
        let is_split_leaf = self.splits.contains_leaf(win)
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

    /// Return the `OverlayId` of an open overlay whose `LayoutTree`
    /// contains `win` as a leaf. `None` when `win` isn't a leaf of
    /// any open overlay. Used by leaf callbacks that need to close
    /// or otherwise manipulate the containing overlay.
    pub fn overlay_for_leaf(&self, win: WinId) -> Option<OverlayId> {
        for (id, ov) in &self.overlays {
            if ov.layout.contains_leaf(win) {
                return Some(*id);
            }
        }
        None
    }

    /// Return the "root" leaf of the overlay containing `win`: the
    /// first leaf in the layout tree's declaration order. This is
    /// the WinId returned to dialog.lua at open time, and the one
    /// it registers WinEvent callbacks against. `None` when `win`
    /// isn't part of any open overlay.
    ///
    /// `fire_win_event` redirects to this root so handlers fire
    /// regardless of which leaf the user actually interacted with
    /// — necessary for mixed dialogs where multiple leaves are
    /// interactive (e.g. options + input).
    fn overlay_root_for_leaf(&self, win: WinId) -> Option<WinId> {
        let id = self.overlay_for_leaf(win)?;
        let ov = self.overlay(id)?;
        ov.layout.leaves_in_order().first().copied()
    }

    /// Read-only view of the focus history (oldest first; the most
    /// recent prior focus is at the back). Test-only introspection
    /// helper — production state flows through `set_focus` /
    /// `overlay_close`.
    #[cfg(test)]
    fn focus_history(&self) -> &[WinId] {
        &self.focus_history
    }

    /// Move focus to the next focusable window in cycle order.
    /// Returns `true` if focus changed. Modal-aware: when an
    /// `active_modal` overlay is open, cycles through that overlay's
    /// focusable leaves only. Returns `false` outside a modal —
    /// cross-source (split + overlay-leaf) z-order is gated on the
    /// unified Ui facade.
    pub fn focus_next(&mut self) -> bool {
        self.focus_step(1)
    }

    /// Move focus to the previous focusable window. See `focus_next`
    /// for cycling and modal-awareness rules.
    pub fn focus_prev(&mut self) -> bool {
        self.focus_step(-1)
    }

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

    // ── Capture (in-flight gesture) ──────────────────────────────

    /// In-flight gesture target, if any. Mouse routing should consult
    /// this before [`Self::hit_test`]: while a gesture is captured,
    /// drag rows must continue flowing to the same target even if the
    /// pointer drifts off its rect.
    pub fn capture(&self) -> Option<HitTarget> {
        self.capture
    }

    /// Latch a gesture target. Call on mouse-down once the host has
    /// decided which target should own the in-flight gesture (e.g.
    /// scrollbar hit). [`Self::clear_capture`] releases it on
    /// mouse-up; [`Self::set_layout`] / [`Self::overlay_close`]
    /// auto-release if the target's owning split or overlay
    /// disappears.
    pub fn set_capture(&mut self, target: HitTarget) {
        self.capture = Some(target);
    }

    /// Release the in-flight gesture target. Idempotent.
    pub fn clear_capture(&mut self) {
        self.capture = None;
        self.drag_autoscroll_since = None;
    }

    /// Read the current global cursor shape.
    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    /// Set the global cursor shape for the focused window. Hosts call
    /// this each frame as the focused window's mode/state changes —
    /// for unfocused frames, set to [`CursorShape::Hidden`].
    pub fn set_cursor_shape(&mut self, shape: CursorShape) {
        self.cursor_shape = shape;
    }

    /// When drag-autoscroll is engaged, the timestamp it started.
    /// Hosts read this from their main-loop timer arm to ramp the
    /// tick interval (longer parking = faster scroll). `None` when
    /// no autoscroll is active.
    pub fn drag_autoscroll_started(&self) -> Option<std::time::Instant> {
        self.drag_autoscroll_since
    }

    /// Per-tick drag-autoscroll query. While the captured window's
    /// cursor sits on the top or bottom row of its viewport, returns
    /// `Some((win, delta))` where `delta` is `-1` (top) or `+1`
    /// (bottom) — hosts perform the per-pane scroll-by-line action
    /// for the returned window. The internal "started at" timestamp
    /// is set on first edge engagement and cleared when the cursor
    /// leaves the edge or capture releases. Reads `cursor_line` and
    /// the painted viewport height from the captured window directly,
    /// so hosts don't need to thread per-pane state.
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
        // `cursor_line` is viewport-relative: 0 = top row,
        // viewport-1 = bottom row. Top edge → cursor-up (-1) to
        // reveal older rows; bottom edge → cursor-down (+1) to
        // reveal newer rows.
        let delta: isize = if win.cursor_line == 0 {
            -1
        } else if (win.cursor_line as usize) >= viewport_h.saturating_sub(1) {
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
                self.splits.contains_leaf(w) || self.overlay_for_leaf(w).is_some()
            }
            HitTarget::Chrome { owner } => self.overlays.iter().any(|(id, _)| *id == owner),
        }
    }

    // ── Renderer delegation ───────────────────────────────────────

    pub fn render<W: std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        let resolved = self.resolve_overlays(None);
        let resolved: Vec<(OverlayId, Rect, Overlay)> = resolved
            .into_iter()
            .map(|(id, rect, ov)| (id, rect, ov.clone()))
            .collect();
        // Snapshot splits leaves with their resolved rects so the
        // post-layer closure can paint them without re-borrowing
        // `self`.
        let split_rects = self.resolve_splits();
        let painted_splits: Vec<(WinId, Rect)> = self
            .splits
            .leaves_in_order()
            .into_iter()
            .filter_map(|win| split_rects.get(&win).map(|r| (win, *r)))
            .collect();
        // Pre-pass: drive `Buffer::ensure_rendered_at` on each overlay
        // leaf at the width its leaf rect resolves to, so parsers
        // (markdown / plain wrap / diff / syntax) populate their
        // `lines` vector before the immutable paint walk reads them.
        for (_id, rect, overlay) in &resolved {
            let leaf_rects = layout::resolve_layout(&overlay.layout, *rect);
            for (win_id, leaf_rect) in &leaf_rects {
                let Some(buf_id) = self.wins.get(win_id).map(|w| w.buf) else {
                    continue;
                };
                if let Some(buf) = self.bufs.get_mut(&buf_id) {
                    buf.ensure_rendered_at(leaf_rect.width);
                }
            }
        }
        // Same pre-pass for painted splits.
        for (win_id, rect) in &painted_splits {
            let Some(buf_id) = self.wins.get(win_id).map(|w| w.buf) else {
                continue;
            };
            if let Some(buf) = self.bufs.get_mut(&buf_id) {
                buf.ensure_rendered_at(rect.width);
            }
        }
        // Resolve the focused window's hardware cursor (if any) so
        // input panels / cmdline / prompt draw a visible caret. The
        // compositor's focused-layer cursor path doesn't see overlay
        // leaves or painted splits; we route the cursor through
        // `render_with`'s closure return. The global `cursor_shape`
        // gates this — only `Hardware` surfaces a caret. `Block`
        // paints in-place via `Window::render`. Overlay > painted
        // split priority order is preserved (overlay first, painted
        // split as fallback); the compositor's focused-layer path
        // applies when the closure returns `None`.
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
        let term_size = self.terminal_size;
        self.compositor.render_with(&self.theme, w, |grid, theme| {
            // Paint splits first so overlays draw on top, matching the
            // prior order (status was a compositor layer at z=500;
            // overlays in the closure ran *after* every compositor
            // layer paint, so any overlap landed overlays-over-status).
            for (win_id, rect) in &painted_splits {
                paint_split(
                    grid,
                    theme,
                    *win_id,
                    *rect,
                    wins,
                    bufs,
                    term_size,
                    focus,
                    cursor_shape,
                );
            }
            for (_id, rect, overlay) in &resolved {
                paint_overlay(
                    grid,
                    theme,
                    *rect,
                    overlay,
                    wins,
                    bufs,
                    term_size,
                    focus,
                    cursor_shape,
                );
            }
            cursor_override
        })
    }

    /// Compute the absolute hardware cursor position for the focused
    /// overlay leaf, given pre-resolved overlay rects. Returns `None`
    /// when no overlay leaf is focused or the cursor falls outside the
    /// leaf's rect. `Window::cursor_line` is viewport-relative so we
    /// add it directly to the leaf's `top`.
    fn focused_overlay_cursor(
        &self,
        resolved: &[(OverlayId, Rect, Overlay)],
    ) -> Option<(u16, u16)> {
        let focus = self.focus?;
        self.overlay_for_leaf(focus)?;
        for (_id, rect, overlay) in resolved {
            let leaf_rects = layout::resolve_layout(&overlay.layout, *rect);
            let Some(leaf_rect) = leaf_rects.get(&focus) else {
                continue;
            };
            let win = self.wins.get(&focus)?;
            let abs_y = leaf_rect.top + win.cursor_line;
            let abs_x = leaf_rect.left + win.cursor_col;
            if abs_y < leaf_rect.top + leaf_rect.height && abs_x < leaf_rect.left + leaf_rect.width
            {
                return Some((abs_x, abs_y));
            }
            return None;
        }
        None
    }

    /// Compute the absolute hardware cursor position for the focused
    /// splits leaf. Returns `None` when focus isn't a splits leaf or
    /// its cursor coordinates fall outside the resolved rect. The
    /// caller has already gated on `cursor_shape == Hardware` —
    /// `Block` paints in-place via `Window::render`.
    /// `Window::cursor_line` / `cursor_col` are viewport-relative and
    /// we add them to the rect's origin.
    fn focused_painted_split_cursor(&self) -> Option<(u16, u16)> {
        let focus = self.focus?;
        if !self.splits.contains_leaf(focus) {
            return None;
        }
        let win = self.wins.get(&focus)?;
        let rect = self.split_rect(focus)?;
        let abs_y = rect.top + win.cursor_line;
        let abs_x = rect.left + win.cursor_col;
        if abs_y < rect.top + rect.height && abs_x < rect.left + rect.width {
            Some((abs_x, abs_y))
        } else {
            None
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn theme_mut(&mut self) -> &mut Theme {
        &mut self.theme
    }

    /// Single Ui-side entry point for terminal events. Fans out by
    /// variant:
    ///
    /// - [`Event::Key`] — routes through the focused window's keymap
    ///   table (resolved via [`Ui::focus`]). Bare Esc on an active
    ///   modal fires [`WinEvent::Dismiss`] on the modal's root leaf
    ///   and closes the modal as a built-in; subsequent variants
    ///   route through the regular [`Callbacks`] registry so
    ///   `on_event("dismiss", …)` handlers can flush pending state.
    ///   `lua_invoke` is called for each `Callback::Lua` with
    ///   (handle, win, payload); side effects flow through host-side
    ///   plumbing.
    /// - [`Event::Resize`] — applies to [`Ui::set_terminal_size`] and
    ///   reports `Consumed`. Hosts may still do additional resize
    ///   work (cache invalidation, layout adapters) on top.
    /// - [`Event::Mouse`] — absorbs wheel events that drift over a
    ///   focused overlay (so they don't bleed into the transcript
    ///   below), absorbs clicks/drags outside the rect of an active
    ///   modal overlay, and owns the scrollbar drag gesture: a
    ///   left-Down on a painted scrollbar latches a
    ///   [`HitTarget::Scrollbar`] capture and snaps the owning
    ///   window's `scroll_top`; subsequent left-Drag rows re-snap
    ///   while the capture holds; left-Up releases the capture.
    ///   Hosts mirroring `scroll_top` elsewhere read it back from
    ///   [`Ui::win`] after dispatch returns `Consumed` and apply any
    ///   per-window side effects (re-anchor, follow-tail). All other
    ///   mouse routing (content drag, click counts, prompt/transcript
    ///   cursor positioning) stays host-side; Ui returns `Ignored`
    ///   so the host can continue routing.
    /// - [`Event::FocusGained`] / [`Event::FocusLost`] /
    ///   [`Event::Paste`] — Ui has no state to update; returns
    ///   `Ignored` so hosts can track terminal focus / drive
    ///   paste-side effects.
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
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(HitTarget::Scrollbar { owner }) = self.capture {
                            self.apply_scrollbar_drag(owner, me.row);
                            return Status::Consumed;
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if matches!(self.capture, Some(HitTarget::Scrollbar { .. })) {
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
                if let Some(modal_id) = self.active_modal() {
                    let inside = self
                        .overlay_hit_test(me.row, me.column, None)
                        .is_some_and(|(id, _)| id == modal_id);
                    if !inside {
                        return Status::Consumed;
                    }
                }
                Status::Ignored
            }
            Event::FocusGained | Event::FocusLost | Event::Paste(_) => Status::Ignored,
        }
    }

    /// Translate a scrollbar-drag pointer row into a scroll offset on
    /// `owner`'s painted viewport and write it to
    /// `Ui::wins[owner].scroll_top`. No-op when `owner` is missing or
    /// has no painted scrollbar — defensive against torn state when a
    /// drag outlives the underlying viewport.
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
        // Modal overlay built-in: bare Esc on an active modal closes
        // the topmost modal. Universal dismiss is fundamental
        // behaviour, not user-customisable. Before closing, fires
        // `WinEvent::Dismiss` on the modal's root leaf so dialog-side
        // `on_event("dismiss", …)` handlers can flush pending state
        // (e.g. unsubmitted input text). `fire_win_event` already
        // redirects leaf events to the root, so a single dispatch
        // suffices regardless of which leaf has focus. Leaves can
        // register their own callbacks for `q` / `Ctrl+C` / Submit
        // etc. through the regular `Callbacks` registry — see the
        // `focus()`-routed dispatch below.
        if matches!(code, crossterm::event::KeyCode::Esc)
            && mods == crossterm::event::KeyModifiers::NONE
        {
            if let Some(modal) = self.active_modal() {
                if let Some(root) = self
                    .overlay(modal)
                    .and_then(|o| o.layout.leaves_in_order().first().copied())
                {
                    self.fire_win_event(root, WinEvent::Dismiss, Payload::None, lua_invoke);
                }
                // The Lua dismiss handler may have already called
                // `smelt.win.close(...)` (which routes through
                // `Ui::win_close` → `overlay_close`). Re-check before
                // closing so we don't double-pop focus_history.
                if self.overlay(modal).is_some() {
                    let _ = self.overlay_close(modal);
                }
                return Status::Consumed;
            }
        }
        let Some(win) = self.focus() else {
            return Status::Ignored;
        };
        let key = KeyBind::new(code, mods);
        // Pending follow-up dispatched after the keymap callback
        // returns. `CallbackResult::Event` writes here so a Rust
        // callback can synthesize a `WinEvent` (e.g. a list's Enter
        // binding firing `Submit`) without needing direct access to
        // `lua_invoke`.
        let mut follow_up: Option<(WinEvent, Payload)> = None;
        let result = if let Some(mut cb) = self.callbacks.take_keymap(win, key) {
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
        };

        if let Some((ev, payload)) = follow_up {
            self.fire_win_event(win, ev, payload, lua_invoke);
        }

        result
    }

    /// Fire `WinEvent::Tick` on every window that has a registered
    /// Tick callback. Used by the app event loop to drive per-frame
    /// refresh of dialogs with live external state (subagent list,
    /// process registry, …).
    pub fn dispatch_tick(&mut self, lua_invoke: &mut LuaInvoke) {
        let wins: Vec<WinId> = self.callbacks.wins_with_event(WinEvent::Tick);
        for win in wins {
            self.fire_win_event(win, WinEvent::Tick, Payload::None, lua_invoke);
        }
    }

    pub fn force_redraw(&mut self) {
        self.compositor.force_redraw();
    }
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

/// Compositor-bearing surface that frontends touching the screen
/// expose. Sibling to the `Host` trait in `tui::app::host` —
/// `UiHost` is **independent** of `Host` (no supertrait bound)
/// because `ui` cannot reference tui-defined types. Frontends that
/// need both impl each side by side.
///
/// Only `TuiApp` impls this trait. `HeadlessApp` does not — the
/// UiHost-only Lua bindings (`smelt.ui`, `smelt.win`, `smelt.buf`,
/// `smelt.statusline`) raise a runtime error when invoked from a
/// headless context (wired in P2.b.5).
///
/// Method names mirror `Ui`'s inherent surface so call sites read
/// the same whether they go through the trait or directly. `Ui`
/// also impls this trait — useful for tests that want to exercise
/// compositor-touching code without spinning up a full frontend.
pub trait UiHost {
    /// Borrow the inner `Ui` directly. Lets compositor-touching
    /// code reach helpers (overlay enumeration, hit-test, focus
    /// history, theme, render) without growing the trait surface.
    fn ui(&mut self) -> &mut Ui;

    /// Set keyboard focus to `win`. Returns `true` if focus
    /// changed. Mirrors [`Ui::set_focus`].
    fn set_focus(&mut self, win: WinId) -> bool;

    /// Fire a `WinEvent` on `win`'s registered callbacks. Mirrors
    /// [`Ui::fire_win_event`].
    fn fire_win_event(
        &mut self,
        win: WinId,
        ev: WinEvent,
        payload: Payload,
        lua_invoke: &mut LuaInvoke,
    );

    /// Create a fresh buffer with an auto-allocated `BufId`.
    /// Mirrors [`Ui::buf_create`].
    fn buf_create(&mut self, opts: buffer::BufCreateOpts) -> BufId;

    /// Mutably borrow a buffer by id. Mirrors [`Ui::buf_mut`].
    fn buf_mut(&mut self, id: BufId) -> Option<&mut Buffer>;

    /// Open a split-tree window backed by `buf`. Mirrors
    /// [`Ui::win_open_split`].
    fn win_open_split(&mut self, buf: BufId, config: SplitConfig) -> Option<WinId>;

    /// Close a window. Returns the Lua callback IDs that were
    /// attached so the caller can drop them from the Lua-side
    /// registry. Mirrors [`Ui::win_close`].
    #[must_use]
    fn win_close(&mut self, id: WinId) -> Vec<u64>;

    /// Mutably borrow a window by id. Mirrors [`Ui::win_mut`].
    fn win_mut(&mut self, id: WinId) -> Option<&mut Window>;

    /// Register an overlay. Mirrors [`Ui::overlay_open`].
    fn overlay_open(&mut self, overlay: Overlay) -> OverlayId;

    /// Close an overlay. Returns the removed `Overlay` for callers
    /// that want to inspect its layout. Mirrors [`Ui::overlay_close`].
    #[must_use]
    fn overlay_close(&mut self, id: OverlayId) -> Option<Overlay>;

    /// Last-painted viewport rect + content width + scrollbar geometry
    /// for `win`. Mirrors `Ui::win(win)?.viewport`. Hosts that project
    /// per-pane geometry through other paths (compositor cache, splits
    /// resolution) override this to keep one accessor canonical for
    /// mouse routing.
    fn viewport_for(&self, win: WinId) -> Option<WindowViewport>;

    /// Display rows for `win` — the same `&[String]` shape that
    /// [`MouseCtx::rows`] carries. The default implementation reads the
    /// backing buffer's lines (one row per source line, no soft wrap).
    /// Hosts override for windows whose painted rows differ from the
    /// source: `TuiApp::PROMPT_WIN` returns wrapped rows;
    /// `TuiApp::TRANSCRIPT_WIN` returns the projected display rows that
    /// include ephemeral content. Returns `None` when the window or its
    /// buffer is missing.
    fn rows_for(&mut self, win: WinId) -> Option<Vec<String>>;

    /// Soft and hard line-break byte positions in
    /// `rows_for(win)?.join("\n")`. Soft positions mark word-wrap
    /// continuations (transparent to word-select); hard positions mark
    /// real `\n` separators (the unit triple-click extends to). The
    /// default implementation treats every join as a hard break and
    /// returns no soft breaks. Hosts override for wrapped panes.
    /// Returns `None` when the window or its buffer is missing.
    fn breaks_for(&mut self, win: WinId) -> Option<(Vec<usize>, Vec<usize>)>;
}

/// `Ui` impls `UiHost` so direct `&mut Ui` callers (tests, helpers
/// that already hold the `Ui`) can dispatch through the trait too.
/// Bodies use the explicit `Ui::method(self, …)` path syntax to
/// disambiguate from the trait's same-named method.
impl UiHost for Ui {
    fn ui(&mut self) -> &mut Ui {
        self
    }
    fn set_focus(&mut self, win: WinId) -> bool {
        Ui::set_focus(self, win)
    }
    fn fire_win_event(
        &mut self,
        win: WinId,
        ev: WinEvent,
        payload: Payload,
        lua_invoke: &mut LuaInvoke,
    ) {
        Ui::fire_win_event(self, win, ev, payload, lua_invoke)
    }
    fn buf_create(&mut self, opts: buffer::BufCreateOpts) -> BufId {
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
    fn overlay_close(&mut self, id: OverlayId) -> Option<Overlay> {
        Ui::overlay_close(self, id)
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

/// Paint one painted-split window into `grid` at `rect` via
/// `Window::render`. Mirrors the leaf branch of `paint_layout_node` for
/// split-shaped windows that paint outside the overlay layout system.
/// Missing windows / buffers are silently skipped.
#[allow(clippy::too_many_arguments)]
fn paint_split(
    grid: &mut Grid,
    theme: &Theme,
    win_id: WinId,
    rect: Rect,
    wins: &HashMap<WinId, Window>,
    bufs: &HashMap<BufId, Buffer>,
    term_size: (u16, u16),
    focus: Option<WinId>,
    cursor_shape: CursorShape,
) {
    let Some(win) = wins.get(&win_id) else {
        return;
    };
    let Some(buf) = bufs.get(&win.buf) else {
        return;
    };
    let mut slice = grid.slice_mut(rect);
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
        theme: theme.clone(),
    };
    win.render(buf, &mut slice, &ctx);
}

/// Paint one resolved overlay into `grid`. Walks the overlay's layout
/// tree depth-first: containers paint chrome at their own rect, then
/// recurse into children at their resolved rects; leaves slice into
/// the grid and call `Window::render`. Missing windows / buffers are
/// silently skipped — the paint pass is best-effort, not authoritative
/// over registry state.
#[allow(clippy::too_many_arguments)]
fn paint_overlay(
    grid: &mut Grid,
    theme: &Theme,
    area: Rect,
    overlay: &Overlay,
    wins: &HashMap<WinId, Window>,
    bufs: &HashMap<BufId, Buffer>,
    term_size: (u16, u16),
    focus: Option<WinId>,
    cursor_shape: CursorShape,
) {
    // Overlays are opaque: clear the rect first so layers below
    // (statusline, prompt borders, transcript content) don't bleed
    // through gap rows or buffer lines that don't fill the leaf width.
    grid.clear(area);
    paint_layout_node(
        grid,
        theme,
        &overlay.layout,
        area,
        wins,
        bufs,
        term_size,
        focus,
        cursor_shape,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_layout_node(
    grid: &mut Grid,
    theme: &Theme,
    node: &LayoutTree,
    area: Rect,
    wins: &HashMap<WinId, Window>,
    bufs: &HashMap<BufId, Buffer>,
    term_size: (u16, u16),
    focus: Option<WinId>,
    cursor_shape: CursorShape,
) {
    match node {
        LayoutTree::Leaf(win_id) => {
            let Some(win) = wins.get(win_id) else {
                return;
            };
            let Some(buf) = bufs.get(&win.buf) else {
                return;
            };
            let mut slice = grid.slice_mut(area);
            let focused = focus == Some(*win_id);
            let ctx = DrawContext {
                terminal_width: term_size.0,
                terminal_height: term_size.1,
                focused,
                cursor_shape: if focused {
                    cursor_shape
                } else {
                    CursorShape::Hidden
                },
                theme: theme.clone(),
            };
            win.render(buf, &mut slice, &ctx);
        }
        LayoutTree::Vbox { items, chrome } | LayoutTree::Hbox { items, chrome } => {
            layout::paint_chrome(grid, area, chrome, theme);
            let vertical = matches!(node, LayoutTree::Vbox { .. });
            let inner = if chrome.border.is_some() {
                Rect::new(
                    area.top + 1,
                    area.left + 1,
                    area.width.saturating_sub(2),
                    area.height.saturating_sub(2),
                )
            } else {
                area
            };
            let primary_total = if vertical { inner.height } else { inner.width };
            let total_gap = chrome
                .gap
                .saturating_mul(items.len().saturating_sub(1) as u16);
            let available = primary_total.saturating_sub(total_gap);
            let sizes = layout::resolve_constraints(items, available);
            let mut offset = 0u16;
            for (i, ((_, child), &size)) in items.iter().zip(sizes.iter()).enumerate() {
                let child_area = if vertical {
                    Rect::new(inner.top + offset, inner.left, inner.width, size)
                } else {
                    Rect::new(inner.top, inner.left + offset, size, inner.height)
                };
                paint_layout_node(
                    grid,
                    theme,
                    child,
                    child_area,
                    wins,
                    bufs,
                    term_size,
                    focus,
                    cursor_shape,
                );
                offset += size;
                if i + 1 < items.len() {
                    offset += chrome.gap;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ui() -> Ui {
        let mut ui = Ui::new();
        ui.set_terminal_size(80, 24);
        ui
    }

    /// Dispatch a bare key through `Ui::dispatch_event` with a no-op
    /// `lua_invoke`. The Lua-runtime collaboration is exercised by
    /// the `tui` integration tests; tests here only assert on
    /// dispatcher behaviour.
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

    /// Open a Buffer-backed split Window at `win_id` and append it as
    /// a leaf to the splits tree — the test-only equivalent of what
    /// `TuiApp::new` does at startup for the prompt / transcript / status
    /// windows. Most focus / overlay tests just need a focusable target
    /// to exercise; this helper takes the boilerplate.
    fn make_split(ui: &mut Ui, win_id: WinId) {
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        let rust_first = ui.buf_create(buffer::BufCreateOpts::default());
        ui.buf_create_with_id(BufId(LUA_BUF_ID_BASE), buffer::BufCreateOpts::default())
            .unwrap();
        let rust_second = ui.buf_create(buffer::BufCreateOpts::default());
        assert_eq!(rust_second.0, rust_first.0 + 1);
        assert!(rust_second.0 < LUA_BUF_ID_BASE);
    }

    // ── Overlay API (P1.c) ───────────────────────────────────────────

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
                target: WinId(999),
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
            .with_border(layout::Border::Single),
            layout::Anchor::ScreenCenter,
        );
        let id = ui.overlay_open(bordered);
        // Inside overlay rect (row 7 = top border), outside the leaf.
        let hit = ui.overlay_hit_test(7, 30, None).unwrap();
        assert_eq!(hit.0, id);
        assert_eq!(hit.1, OverlayHitTarget::Chrome);
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
    fn overlay_hit_test_modal_blocks_lower_z() {
        let mut ui = make_ui();
        // Lower-z overlay covering (7,20)..(17,60).
        let _under =
            ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter).with_z(10));
        // Higher-z modal at same anchor, smaller (10x4 → centered (10,35)..(14,45)).
        let modal_id = ui.overlay_open(
            sized_overlay(10, 4, layout::Anchor::ScreenCenter)
                .with_z(100)
                .modal(true),
        );
        // Hit inside the modal → returns the modal.
        let hit = ui.overlay_hit_test(11, 36, None).unwrap();
        assert_eq!(hit.0, modal_id);
        // Hit inside the under overlay but outside the modal → blocked,
        // returns None (lower-z overlay can't receive the click).
        assert_eq!(ui.overlay_hit_test(8, 22, None), None);
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
        // No modal open → focus cycling is a no-op (gated on P1.f).
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
            .with_border(layout::Border::Single),
            layout::Anchor::ScreenCenter,
        ));
        let hit = ui.hit_test(7, 30, None).unwrap();
        assert_eq!(hit, HitTarget::Chrome { owner: id });
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        ui.set_capture(HitTarget::Chrome { owner: id });
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        // full terminal — so cursor_line=1 / cursor_col=3 → (3, 1).
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        ui.set_focus(win);
        let w = ui.win_mut(win).unwrap();
        w.cursor_line = 1;
        w.cursor_col = 3;
        assert_eq!(ui.focused_painted_split_cursor(), Some((3, 1)));
    }

    #[test]
    fn focused_painted_split_cursor_returns_none_when_unfocused() {
        let mut ui = make_ui();
        ui.set_terminal_size(20, 4);
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        w.cursor_line = 0;
        w.cursor_col = 0;
        // No focus call → focus stays None.
        assert_eq!(ui.focused_painted_split_cursor(), None);
    }

    #[test]
    fn handle_key_routes_to_overlay_leaf_callback() {
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        impl buffer::BufferParser for WidthRecorder {
            fn parse(&self, buf: &mut Buffer, _source: &str, width: u16) {
                self.calls.lock().unwrap().push(width);
                buf.set_all_lines(vec![format!("rendered@{width}")]);
            }
        }
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        .with_border(Border::Single);
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        .with_border(Border::Single)
        .with_title("title");
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        // Borrow Compositor's previous grid (post-flush swap) for assertions.
        let frame = ui.compositor.previous_for_test();
        // Centered (term 80x24, overlay natural 42 wide × 5 tall →
        // top=9 left=19). Title sits in the top border row at col=20.
        assert_eq!(frame.cell(19, 9).symbol, '┌');
        assert_eq!(frame.cell(20, 9).symbol, 't');
        assert_eq!(frame.cell(24, 9).symbol, 'e');
        // Leaf paints inside the border at (top+1, left+1) = (10, 20).
        assert_eq!(frame.cell(20, 10).symbol, 'o');
        assert_eq!(frame.cell(31, 10).symbol, 't');
    }

    // ── UiHost trait dispatch (P2.b.2) ───────────────────────────────

    #[test]
    fn ui_host_dispatch_round_trips_through_dyn() {
        // Drive every UiHost method through `&mut dyn UiHost` so the
        // trait shape is exercised end-to-end (not just the inherent
        // path). Mirrors how P2.b.5's Lua bindings will reach the
        // compositor — by trait, not by direct field access.
        fn drive(host: &mut dyn UiHost) -> (BufId, WinId, OverlayId) {
            let buf = host.buf_create(buffer::BufCreateOpts::default());
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
            host.win_mut(win).unwrap().cursor_col = 3;
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
        UiHost::fire_win_event(
            &mut ui,
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
        let removed = UiHost::overlay_close(&mut ui, oid);
        assert!(removed.is_some());
        let cb_ids = UiHost::win_close(&mut ui, win);
        assert!(cb_ids.is_empty());
        assert!(ui.buf(buf).is_some());
    }

    // ── UiHost per-pane data accessors (P2.b.4c.5b) ──────────────────

    #[test]
    fn ui_host_per_pane_data_default_impl() {
        // Ui's default `rows_for` / `breaks_for` / `viewport_for` cover
        // any window the host hasn't overridden — buffer lines as rows,
        // join positions as hard breaks, no soft wraps. Drives all
        // three through `&mut dyn UiHost` so the trait shape is
        // exercised end-to-end.
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
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
        ui.win_mut(win).unwrap().cursor_line = 0;
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
        // so cursor_line=9 is the bottom row.
        ui.win_mut(win).unwrap().cursor_line = 9;
        assert_eq!(ui.poll_drag_autoscroll(), Some((win, 1)));
    }

    #[test]
    fn poll_drag_autoscroll_clears_started_at_when_cursor_leaves_edge() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        ui.win_mut(win).unwrap().cursor_line = 0;
        let _ = ui.poll_drag_autoscroll();
        assert!(ui.drag_autoscroll_started().is_some());
        ui.win_mut(win).unwrap().cursor_line = 5;
        assert_eq!(ui.poll_drag_autoscroll(), None);
        assert_eq!(ui.drag_autoscroll_started(), None);
    }

    #[test]
    fn poll_drag_autoscroll_clears_started_at_when_capture_releases() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        ui.win_mut(win).unwrap().cursor_line = 0;
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
        ui.win_mut(win).unwrap().cursor_line = 0;
        assert_eq!(ui.poll_drag_autoscroll(), None);
    }
}
