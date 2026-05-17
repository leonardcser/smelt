//! `smelt-edit` — editor layer over `smelt-term`.
//!
//! `Ui` ties `Window`s (per-buffer view + viewport + scrollbar + gutter)
//! to layout and routes events. Renderer primitives come from `smelt-term`.

pub mod callback;
pub(crate) mod event;
pub mod gutter;
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
    /// Stable-name → resource-id maps for the hot-reload path. Plugins
    /// that pass `opts.name = "foo"` to `buf_create` / `win_open_*` /
    /// `overlay_open` get the same id back on every (re-)load, so
    /// `/reload` can mutate the resource in place instead of tearing
    /// it down and losing scroll/cursor/size. Anonymous resources
    /// (`opts.name == nil`) skip these maps and are reaped on reload.
    named_bufs: HashMap<String, BufId>,
    named_wins: HashMap<String, WinId>,
    named_overlays: HashMap<String, OverlayId>,
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
    /// Groups of windows whose `scroll_top` is mirrored. Each group tracks the
    /// last value observed per member; the side that moved this frame becomes
    /// the leader and its value is copied to all others. Synced once per
    /// frame via [`Ui::sync_scroll_links`] before paint.
    scroll_groups: Vec<ScrollGroup>,
}

/// One scroll-mirror group. `members` lists the participating window ids;
/// `last` is the post-sync `(scroll_top, scroll_left)` of each member captured
/// the previous frame, used to detect which side moved on which axis this
/// frame. Each axis is leader-detected independently — a horizontal pan on
/// one member mirrors only horizontally, leaving vertical positions untouched.
struct ScrollGroup {
    members: Vec<WinId>,
    last: Vec<(u16, u16)>,
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
            named_bufs: HashMap::new(),
            named_wins: HashMap::new(),
            named_overlays: HashMap::new(),
            focus_history: Vec::new(),
            focus: None,
            capture: None,
            last_click: None,
            cursor_shape: CursorShape::Hidden,
            drag_autoscroll_since: None,
            chrome_drag: None,
            scroll_groups: Vec::new(),
        }
    }

    /// Link `wins` into a single scroll-mirror group. Any existing groups that
    /// overlap with `wins` are merged into one. Duplicates and unknown ids are
    /// dropped silently. A group with fewer than two live members is discarded.
    pub fn link_scroll(&mut self, wins: &[WinId]) {
        let mut next: Vec<WinId> = Vec::new();
        for w in wins {
            if self.wins.contains_key(w) && !next.contains(w) {
                next.push(*w);
            }
        }
        if next.len() < 2 {
            return;
        }
        // Merge any existing group whose members intersect `next`.
        let mut keep: Vec<ScrollGroup> = Vec::with_capacity(self.scroll_groups.len());
        for g in std::mem::take(&mut self.scroll_groups) {
            if g.members.iter().any(|m| next.contains(m)) {
                for m in g.members {
                    if !next.contains(&m) {
                        next.push(m);
                    }
                }
            } else {
                keep.push(g);
            }
        }
        let last = next
            .iter()
            .map(|w| {
                self.wins
                    .get(w)
                    .map(|win| (win.scroll_top, win.scroll_left))
                    .unwrap_or((0, 0))
            })
            .collect();
        keep.push(ScrollGroup {
            members: next,
            last,
        });
        self.scroll_groups = keep;
    }

    /// Mirror `(scroll_top, scroll_left)` across every group, axis-by-axis.
    /// Pruning rules:
    /// - drop members whose window no longer exists;
    /// - drop the whole group once it has fewer than two live members.
    ///
    /// Each axis is leader-detected independently: the first member whose
    /// current value drifted from `last` wins on that axis. The leader's
    /// value is written to every other member and to each `last` entry.
    /// Ties (everyone equal on an axis) are a no-op on that axis.
    pub fn sync_scroll_links(&mut self) {
        if self.scroll_groups.is_empty() {
            return;
        }
        let mut groups = std::mem::take(&mut self.scroll_groups);
        groups.retain_mut(|g| {
            // Prune dead members and keep `last` aligned.
            let mut i = 0;
            while i < g.members.len() {
                if self.wins.contains_key(&g.members[i]) {
                    i += 1;
                } else {
                    g.members.remove(i);
                    g.last.remove(i);
                }
            }
            if g.members.len() < 2 {
                return false;
            }
            let now: Vec<(u16, u16)> = g
                .members
                .iter()
                .map(|w| {
                    self.wins
                        .get(w)
                        .map(|win| (win.scroll_top, win.scroll_left))
                        .unwrap_or((0, 0))
                })
                .collect();
            // Vertical axis.
            let v_leader = now.iter().zip(g.last.iter()).position(|(n, l)| n.0 != l.0);
            let v_target = v_leader.map(|i| now[i].0);
            // Horizontal axis.
            let h_leader = now.iter().zip(g.last.iter()).position(|(n, l)| n.1 != l.1);
            let h_target = h_leader.map(|i| now[i].1);
            for (i, wid) in g.members.iter().enumerate() {
                if let Some(w) = self.wins.get_mut(wid) {
                    if let Some(t) = v_target {
                        if now[i].0 != t {
                            w.scroll_top = t;
                        }
                    }
                    if let Some(t) = h_target {
                        if now[i].1 != t {
                            w.scroll_left = t;
                        }
                    }
                }
                let row = v_target.unwrap_or(now[i].0);
                let col = h_target.unwrap_or(now[i].1);
                g.last[i] = (row, col);
            }
            true
        });
        self.scroll_groups = groups;
    }

    /// Returns 1/2/3 for successive Downs on the same cell within 400ms; wraps at 4.
    fn record_click(&mut self, row: u16, col: u16, now: std::time::Instant) -> u8 {
        use std::time::Duration;
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

    /// Hit-test a primary-button Down/Drag/Up against any leaf (splits or overlay).
    /// Latches capture on Down; returns `(win, click_count)` where `click_count`
    /// is 0 for Drag/Up. Up clears capture. Call only when `dispatch_event`
    /// returned `Ignored` — that method owns scrollbar drag and modal blocking.
    /// Overlay leaves participate so a `selectable` notification or dialog body
    /// can drive its own selection through `Window::handle_mouse`.
    pub fn resolve_split_mouse(
        &mut self,
        me: crossterm::event::MouseEvent,
        now: std::time::Instant,
    ) -> Option<(WinId, u8)> {
        use crossterm::event::{MouseButton, MouseEventKind};
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let win = match self.hit_test(me.row, me.column, None)? {
                    HitTarget::Window(w) => w,
                    _ => return None,
                };
                self.set_capture(HitTarget::Window(win));
                let count = self.record_click(me.row, me.column, now);
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

    // ── Named resources (hot-reload-survivable handles) ──────────────

    pub fn named_buf(&self, name: &str) -> Option<BufId> {
        self.named_bufs.get(name).copied()
    }

    pub fn name_buf(&mut self, name: impl Into<String>, id: BufId) {
        self.named_bufs.insert(name.into(), id);
    }

    pub fn named_win(&self, name: &str) -> Option<WinId> {
        self.named_wins.get(name).copied()
    }

    pub fn name_win(&mut self, name: impl Into<String>, id: WinId) {
        self.named_wins.insert(name.into(), id);
    }

    pub fn named_overlay(&self, name: &str) -> Option<OverlayId> {
        self.named_overlays.get(name).copied()
    }

    pub fn name_overlay(&mut self, name: impl Into<String>, id: OverlayId) {
        self.named_overlays.insert(name.into(), id);
    }

    // ── Named-refresh shortcuts ──────────────────────────────────────
    //
    // Each `lookup_named_X_mut` fuses the two-step `named_X` →
    // `X_mut` Option chain that every Lua refresh path needs. Callers
    // get both the stable id (to return to Lua) and a mutable reference
    // (to apply opts) in one go.

    pub fn lookup_named_buf_mut(&mut self, name: &str) -> Option<(BufId, &mut Buffer)> {
        let bid = self.named_bufs.get(name).copied()?;
        let buf = self.bufs.get_mut(&bid)?;
        Some((bid, buf))
    }

    pub fn lookup_named_overlay_mut(&mut self, name: &str) -> Option<(OverlayId, &mut Overlay)> {
        let id = self.named_overlays.get(name).copied()?;
        let ov = self
            .overlays
            .iter_mut()
            .find_map(|(oid, ov)| (*oid == id).then_some(ov))?;
        Some((id, ov))
    }

    /// Close every overlay whose id isn't in `named_overlays` and remove
    /// every Lua-created buffer (id ≥ `lua_buf_threshold`) that no
    /// surviving window references. Used by `/reload` so anonymous
    /// dialogs/pickers from the previous cycle don't linger as ghost
    /// overlays. Named resources are left untouched — plugins recover
    /// them by passing the same `opts.name` on re-open. Returns the
    /// union of Lua callback ids the caller must release.
    pub fn reap_anonymous(&mut self, lua_buf_threshold: u64) -> Vec<u64> {
        let keep_overlays: std::collections::HashSet<OverlayId> =
            self.named_overlays.values().copied().collect();
        let doomed: Vec<WinId> = self
            .overlays
            .iter()
            .filter(|(id, _)| !keep_overlays.contains(id))
            .filter_map(|(_, ov)| ov.layout.leaves_in_order().into_iter().next())
            .map(|p| WinId(p.0))
            .collect();
        let mut ids = Vec::new();
        for leaf in doomed {
            ids.extend(self.win_close(leaf));
        }
        let referenced: std::collections::HashSet<BufId> =
            self.wins.values().map(|w| w.buf).collect();
        let named_bufs: std::collections::HashSet<BufId> =
            self.named_bufs.values().copied().collect();
        let drop_bufs: Vec<BufId> = self
            .bufs
            .keys()
            .copied()
            .filter(|id| id.0 >= lua_buf_threshold)
            .filter(|id| !referenced.contains(id) && !named_bufs.contains(id))
            .collect();
        for id in drop_bufs {
            self.bufs.remove(&id);
        }
        ids
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

    pub fn overlay_count(&self) -> usize {
        self.overlays.len()
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

    /// Pan the wheel-scrollable leaf at `(row, col)` by `delta` visual rows.
    /// Called from both the coalesced-wheel flush in the host and the
    /// `MouseEventKind::Scroll{Up,Down}` arm of `dispatch_event` (which fires
    /// when wheel events bypass coalescing, e.g. an overlay is focused).
    /// Returns `true` when something was actually panned.
    pub fn scroll_at(&mut self, row: u16, col: u16, delta: isize) -> bool {
        if delta == 0 {
            return false;
        }
        let target_win = match self.hit_test(row, col, None) {
            Some(HitTarget::Window(w)) => Some(w),
            Some(HitTarget::Scrollbar { owner }) => Some(owner),
            _ => None,
        };
        let Some(w) = target_win else {
            return false;
        };
        let leaf_info = self.wins.get(&w).and_then(|win| {
            let scrollable = win.mouse_scroll || win.config.gutters.scrollbar;
            scrollable.then(|| win.viewport.map(|vp| (win.buf, vp.rect.height)))?
        });
        let Some((buf_id, vp_height)) = leaf_info else {
            return false;
        };
        let (win, buf) = self.win_and_buf_mut(w, buf_id);
        if let (Some(win), Some(buf)) = (win, buf) {
            win.pan_by_lines(buf, delta, vp_height);
            true
        } else {
            false
        }
    }

    /// Horizontal twin of [`Self::scroll_at`]. Routes a column-delta to the
    /// hit window's `pan_by_columns`. Returns whether anything panned.
    pub fn scroll_at_horizontal(&mut self, row: u16, col: u16, delta: isize) -> bool {
        if delta == 0 {
            return false;
        }
        let target_win = match self.hit_test(row, col, None) {
            Some(HitTarget::Window(w)) => Some(w),
            Some(HitTarget::Scrollbar { owner }) => Some(owner),
            _ => None,
        };
        let Some(w) = target_win else {
            return false;
        };
        let vp_width = self.wins.get(&w).and_then(|win| {
            let scrollable = win.mouse_scroll || win.config.gutters.scrollbar;
            scrollable.then(|| win.viewport.map(|vp| vp.content_width))?
        });
        let Some(vp_width) = vp_width else {
            return false;
        };
        if let Some(win) = self.wins.get_mut(&w) {
            win.pan_by_columns(delta, vp_width);
            true
        } else {
            false
        }
    }

    /// Hit-test a screen position. Checks overlays (topmost-z first, modal-aware)
    /// then splits leaves. Scrollbar column returns `HitTarget::Scrollbar`.
    pub fn hit_test(&self, row: u16, col: u16, cursor: Option<(u16, u16)>) -> Option<HitTarget> {
        if let Some((id, target)) = self.overlay_hit_test(row, col, cursor) {
            return Some(match target {
                OverlayHitTarget::Window(w) => HitTarget::Window(w),
                OverlayHitTarget::Scrollbar(w) => HitTarget::Scrollbar { owner: w },
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
            let sizer = UiLeafSizer {
                wins: &self.wins,
                bufs: &self.bufs,
            };
            let leaf_rects = layout::resolve_layout_with(&ov.layout, rect, &sizer);
            for (paint_id, leaf_rect) in &leaf_rects {
                if leaf_rect.contains(row, col) {
                    let win = WinId(paint_id.0);
                    if self
                        .wins
                        .get(&win)
                        .and_then(|w| w.viewport)
                        .and_then(|vp| vp.scrollbar.map(|bar| (vp, bar)))
                        .is_some_and(|(vp, bar)| bar.contains(vp.rect, row, col))
                    {
                        return Some((id, OverlayHitTarget::Scrollbar(win)));
                    }
                    return Some((id, OverlayHitTarget::Window(win)));
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
                .unwrap_or_else(|| ov.layout.natural_size_with((term_w, term_h), self));
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

    /// Fire a `WinEvent` on `win`. Callbacks registered on `win` for `ev` fire in
    /// registration order. The event does not bubble to other leaves — consumers that
    /// need to catch events from multiple panels (e.g. `dialog.lua`) register on each
    /// relevant leaf.
    pub fn fire_win_event(
        &mut self,
        win: WinId,
        ev: WinEvent,
        payload: Payload,
        lua_invoke: &mut LuaInvoke,
    ) {
        let Some(mut cbs) = self.callbacks.take_event(win, ev) else {
            return;
        };
        for cb in cbs.iter_mut() {
            match cb {
                Callback::Rust(inner) => {
                    let mut ctx = CallbackCtx {
                        ui: self,
                        win,
                        payload: payload.clone(),
                    };
                    let _ = inner(&mut ctx);
                }
                Callback::Lua(handle) => {
                    lua_invoke(*handle, win, &payload);
                }
            }
        }
        self.callbacks.restore_event(win, ev, cbs);
    }

    pub fn win(&self, id: WinId) -> Option<&Window> {
        self.wins.get(&id)
    }

    pub fn win_mut(&mut self, id: WinId) -> Option<&mut Window> {
        self.wins.get_mut(&id)
    }

    /// Read-only iterator over every live `(WinId, &Window)` pair. Order
    /// is unspecified (backed by `HashMap`), so callers must not rely on it.
    pub fn iter_wins(&self) -> impl Iterator<Item = (WinId, &Window)> {
        self.wins.iter().map(|(id, w)| (*id, w))
    }

    /// Read-only iterator over every live `(BufId, &Buffer)` pair. Order
    /// is unspecified (backed by `HashMap`), so callers must not rely on it.
    pub fn iter_bufs(&self) -> impl Iterator<Item = (BufId, &Buffer)> {
        self.bufs.iter().map(|(id, b)| (*id, b))
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

    /// Leaf that currently drives the single visible terminal cursor. During a mouse
    /// drag-select the cursor temporarily moves to the dragging leaf (so the user sees
    /// feedback at the drag end even on a non-focusable leaf like a notification);
    /// otherwise it follows keyboard focus. The renderer queries this once per frame
    /// and routes `cursor_shape` accordingly so only one leaf paints a block.
    pub fn active_cursor_leaf(&self) -> Option<WinId> {
        if let Some(HitTarget::Window(w)) = self.capture {
            if self
                .wins
                .get(&w)
                .and_then(|win| win.drag_endpoint)
                .is_some()
            {
                return Some(w);
            }
        }
        self.focus
    }

    /// `true` if any window currently has an active drag (mid-drag `drag_endpoint`).
    /// `render_loop` reads this so the global `cursor_shape` flips to `Block` for the
    /// duration of the drag, even when keyboard focus is on a leaf that normally has
    /// no visible cursor.
    pub fn any_drag_active(&self) -> bool {
        if let Some(HitTarget::Window(w)) = self.capture {
            return self
                .wins
                .get(&w)
                .and_then(|win| win.drag_endpoint)
                .is_some();
        }
        false
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
            let sizer = UiLeafSizer {
                wins: &self.wins,
                bufs: &self.bufs,
            };
            let leaf_rects = layout::resolve_layout_with(&overlay.layout, *rect, &sizer);
            for (paint_id, leaf_rect) in &leaf_rects {
                let win_id = WinId(paint_id.0);
                let Some(win) = self.wins.get(&win_id) else {
                    continue;
                };
                let buf_id = win.buf;
                let gutter_width = self
                    .bufs
                    .get(&buf_id)
                    .map(|buf| win.gutter_width(buf))
                    .unwrap_or(0)
                    .min(leaf_rect.width);
                let content_width = win
                    .config
                    .gutters
                    .content_width(leaf_rect.width)
                    .min(leaf_rect.width.saturating_sub(gutter_width));
                if let Some(buf) = self.bufs.get_mut(&buf_id) {
                    buf.ensure_rendered_at(content_width);
                }
                if let (Some(buf), Some(win)) = (self.bufs.get(&buf_id), self.wins.get_mut(&win_id))
                {
                    win.ensure_layout(buf, content_width);
                }
                let total_rows = self
                    .bufs
                    .get(&buf_id)
                    .map(|buf| buf.lines().len().min(u16::MAX as usize) as u16)
                    .unwrap_or(0);
                if let Some(win) = self.wins.get_mut(&win_id) {
                    if win.pending_scroll_to_cursor && leaf_rect.height > 0 {
                        win.keep_cursor_visible(total_rows, leaf_rect.height, content_width);
                        win.pending_scroll_to_cursor = false;
                    }
                    let scrollbar = if win.config.gutters.scrollbar && leaf_rect.width > 0 {
                        let bar_col = leaf_rect.left + leaf_rect.width.saturating_sub(1);
                        window::ScrollbarState::new(bar_col, total_rows, leaf_rect.height)
                    } else {
                        None
                    };
                    win.viewport = Some(
                        window::WindowViewport::new(
                            *leaf_rect,
                            content_width,
                            total_rows,
                            win.scroll_top,
                            scrollbar,
                        )
                        .with_gutter_width(gutter_width),
                    );
                }
            }
        }
        for (win_id, rect) in &painted_splits {
            let Some(win) = self.wins.get(win_id) else {
                continue;
            };
            let buf_id = win.buf;
            let gutter_width = self
                .bufs
                .get(&buf_id)
                .map(|buf| win.gutter_width(buf))
                .unwrap_or(0)
                .min(rect.width);
            let content_width = win
                .config
                .gutters
                .content_width(rect.width)
                .min(rect.width.saturating_sub(gutter_width));
            if let Some(buf) = self.bufs.get_mut(&buf_id) {
                buf.ensure_rendered_at(content_width);
            }
            if let (Some(buf), Some(win)) = (self.bufs.get(&buf_id), self.wins.get_mut(win_id)) {
                win.ensure_layout(buf, content_width);
            }
            let total_rows = self
                .bufs
                .get(&buf_id)
                .map(|buf| buf.lines().len().min(u16::MAX as usize) as u16)
                .unwrap_or(0);
            if let Some(win) = self.wins.get_mut(win_id) {
                let scrollbar = if win.config.gutters.scrollbar && rect.width > 0 {
                    let bar_col = rect.left + rect.width.saturating_sub(1);
                    window::ScrollbarState::new(bar_col, total_rows, rect.height)
                } else {
                    None
                };
                win.viewport = Some(
                    window::WindowViewport::new(
                        *rect,
                        content_width,
                        total_rows,
                        win.scroll_top,
                        scrollbar,
                    )
                    .with_gutter_width(gutter_width),
                );
            }
        }
        let focus = self.focus;
        let active_cursor = self.active_cursor_leaf();
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
                            // Exactly one leaf paints a block cursor per frame:
                            // the active cursor leaf (drag-active → focus). Others
                            // receive `Hidden` regardless of focus.
                            let owns_cursor = active_cursor == Some(win_id);
                            let ctx = DrawContext {
                                terminal_width: term_size.0,
                                terminal_height: term_size.1,
                                focused,
                                cursor_shape: if owns_cursor {
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
                let sizer = UiLeafSizer { wins, bufs };
                smelt_term::paint_layout_tree_with(
                    grid,
                    theme,
                    &splits_tree,
                    Rect::new(0, 0, term_w, term_h),
                    term_size,
                    &sizer,
                    &mut dispatch,
                );
                for (_id, rect, overlay) in &resolved {
                    paint_overlay(
                        grid,
                        theme,
                        *rect,
                        overlay,
                        term_size,
                        &sizer,
                        &mut dispatch,
                    );
                }
            })
    }

    /// Paint directly into the compositor's `Grid`, bypassing layout and overlay machinery.
    pub fn render_raw<W, F>(&mut self, w: &mut W, paint: F) -> std::io::Result<()>
    where
        W: std::io::Write,
        F: FnOnce(&mut Grid, &Theme),
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
        let sizer = UiLeafSizer {
            wins: &self.wins,
            bufs: &self.bufs,
        };
        for (_oid, ov_rect, ov) in self.resolve_overlays(None) {
            if let Some(rect) = layout::resolve_layout_with(&ov.layout, ov_rect, &sizer).get(&id) {
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

    pub fn theme(&self) -> &std::sync::Arc<Theme> {
        self.surface.theme()
    }

    pub fn theme_mut(&mut self) -> &mut Theme {
        self.surface.theme_mut()
    }

    /// Route a terminal event. Key: fires keymaps; bare Esc/Ctrl-C on a modal dismisses it.
    /// Resize: updates terminal size. Mouse: owns scrollbar drag and chrome drag; absorbs
    /// wheel on focused overlays. Clicks outside a modal pass through to the host so
    /// drag-select on background splits still works — the host is expected to skip
    /// `app_focus` promotion while a modal is active, leaving the modal focused.
    /// Returns `Ignored` for everything else so the host can continue routing.
    pub fn dispatch_event(&mut self, ev: Event, lua_invoke: &mut LuaInvoke) -> Status {
        use crossterm::event::{KeyEvent, MouseButton, MouseEventKind};
        match ev {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => {
                if let Status::Consumed = self.dispatch_key(code, modifiers, lua_invoke) {
                    return Status::Consumed;
                }
                if let Status::Consumed = self.dispatch_key_fallback(code, modifiers, lua_invoke) {
                    return Status::Consumed;
                }
                self.try_dismiss_modal_for_chord(code, modifiers, lua_invoke)
            }
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
                        // so the user can grab anywhere on it to move the panel. A `selectable`
                        // leaf opts out: drag inside it produces a text selection instead.
                        let drag_target: Option<(OverlayId, overlay::ChromeZone)> = match hit {
                            Some(HitTarget::Chrome { owner, zone }) => Some((owner, zone)),
                            Some(HitTarget::Window(w)) => {
                                let (leaf_focusable, leaf_selectable) = self
                                    .wins
                                    .get(&w)
                                    .map(|win| (win.focusable, win.selectable))
                                    .unwrap_or((true, false));
                                self.overlay_for_leaf(w).and_then(|owner| {
                                    let ov_draggable =
                                        self.overlay(owner).map(|o| o.draggable).unwrap_or(false);
                                    (!leaf_focusable && ov_draggable && !leaf_selectable)
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
                if is_scroll {
                    // Route the wheel direction to the appropriate axis on the
                    // leaf under the cursor (when it's wheel-scrollable). When
                    // an overlay is focused, claim the event regardless so it
                    // can't bleed through to splits underneath.
                    let (vdelta, hdelta) = match me.kind {
                        MouseEventKind::ScrollUp => (-3_isize, 0),
                        MouseEventKind::ScrollDown => (3, 0),
                        MouseEventKind::ScrollLeft => (0, -3),
                        MouseEventKind::ScrollRight => (0, 3),
                        _ => (0, 0),
                    };
                    let mut consumed = false;
                    if vdelta != 0 {
                        consumed |= self.scroll_at(me.row, me.column, vdelta);
                    }
                    if hdelta != 0 {
                        consumed |= self.scroll_at_horizontal(me.row, me.column, hdelta);
                    }
                    if consumed || self.focused_overlay().is_some() {
                        return Status::Consumed;
                    }
                }
                // A modal grabs keyboard focus and Esc/Ctrl-C, but mouse events
                // outside its rect still flow to the splits underneath so the
                // user can drag-select transcript content. The caller is
                // responsible for not promoting `app_focus` while a modal is
                // active — keeping the modal as the focused overlay produces
                // the natural snap-back-after-drag behavior.
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
        let buf_id = win.buf;
        let viewport_rows = vp.rect.height;
        let (win, buf) = self.win_and_buf_mut(owner, buf_id);
        if let (Some(win), Some(buf)) = (win, buf) {
            win.scroll_to_preserving_cursor_screen_row(from_top, buf, viewport_rows);
        }
    }

    pub fn dispatch_key(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> Status {
        // Tier 1 of the key cascade: per-window specific keymaps. Returns
        // `Status::Ignored` when no exact chord match exists so the caller
        // can run global Lua keymaps next, then `dispatch_key_fallback`,
        // then `try_dismiss_modal_for_chord` as the final resort.
        let key = KeyBind::new(code, mods);
        self.run_key_callback(
            code,
            mods,
            lua_invoke,
            |s, win| s.callbacks.take_keymap(win, key),
            |s, win, cb| s.callbacks.restore_keymap(win, key, cb),
        )
    }

    /// Tier 3 of the key cascade: per-window catch-all fallback (the "text
    /// input" tier — e.g. a dialog input that inserts any printable char).
    /// Runs only after `dispatch_event(Event::Key)` returned `Ignored` AND
    /// global Lua keymaps declined, so site-wide chords like `?` → /help take
    /// precedence over a leaf's blanket printable-char capture.
    pub fn dispatch_key_fallback(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> Status {
        self.run_key_callback(
            code,
            mods,
            lua_invoke,
            |s, win| s.callbacks.take_key_fallback(win),
            |s, win, cb| s.callbacks.restore_key_fallback(win, cb),
        )
    }

    /// Shared shell for the tier-1 and tier-3 key dispatchers: look up the
    /// callback on the focused window with `take`, invoke it (Rust or Lua),
    /// hand it back via `restore`, and fan out any `CallbackResult::Event`
    /// follow-up. Both tiers differ only in how they fetch the callback.
    fn run_key_callback(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
        take: impl FnOnce(&mut Self, WinId) -> Option<Callback>,
        restore: impl FnOnce(&mut Self, WinId, Callback),
    ) -> Status {
        let Some(win) = self.focus() else {
            return Status::Ignored;
        };
        let Some(mut cb) = take(self, win) else {
            return Status::Ignored;
        };

        let mut follow_up: Option<(WinEvent, Payload)> = None;
        let result = match &mut cb {
            Callback::Rust(inner) => {
                let mut ctx = CallbackCtx {
                    ui: self,
                    win,
                    payload: Payload::Key { code, mods },
                };
                match inner(&mut ctx) {
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
        restore(self, win, cb);

        if let Some((ev, payload)) = follow_up {
            if let Some(win) = self.focus() {
                self.fire_win_event(win, ev, payload, lua_invoke);
            }
        }

        result
    }

    /// Final tier of the key cascade: dismiss the active modal on a bare
    /// `Esc` or `Ctrl-C`. Returns `Status::Consumed` only when a modal was
    /// actually dismissed. Caller runs this AFTER specific keymaps, global
    /// Lua keymaps, leaf fallback, and any overlay-viewer handler — the
    /// modal should close only when no one else claimed the chord.
    pub fn try_dismiss_modal_for_chord(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> Status {
        let is_dismiss_chord = matches!(code, crossterm::event::KeyCode::Esc)
            && mods == crossterm::event::KeyModifiers::NONE
            || matches!(code, crossterm::event::KeyCode::Char('c'))
                && mods == crossterm::event::KeyModifiers::CONTROL;
        if !is_dismiss_chord {
            return Status::Ignored;
        }
        let Some(modal) = self.active_modal() else {
            return Status::Ignored;
        };
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
        Status::Consumed
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
    sizer: &dyn layout::LeafSizer,
    paint: &mut PaintDispatch,
) {
    grid.clear(area);
    smelt_term::paint_layout_tree_with(grid, theme, &overlay.layout, area, term_size, sizer, paint);
}

/// Looks up each window leaf's natural size from its buffer's current
/// (wrapped) line count. Paint regions and unknown ids report `(0, 0)`.
/// Callers that want the line count to reflect a freshly wrapped width must
/// run `Buffer::ensure_rendered_at` first; this sizer reads only.
pub(crate) struct UiLeafSizer<'a> {
    pub wins: &'a HashMap<WinId, Window>,
    pub bufs: &'a HashMap<BufId, Buffer>,
}

impl<'a> layout::LeafSizer for UiLeafSizer<'a> {
    fn leaf_natural_size(&self, id: PaintId, cap: (u16, u16)) -> (u16, u16) {
        let win_id = WinId(id.0);
        let Some(win) = self.wins.get(&win_id) else {
            return (0, 0);
        };
        let Some(buf) = self.bufs.get(&win.buf) else {
            return (0, 0);
        };
        let h = (buf.lines().len() as u32).min(u16::MAX as u32) as u16;
        (cap.0, h.min(cap.1))
    }
}

impl layout::LeafSizer for Ui {
    fn leaf_natural_size(&self, id: PaintId, cap: (u16, u16)) -> (u16, u16) {
        UiLeafSizer {
            wins: &self.wins,
            bufs: &self.bufs,
        }
        .leaf_natural_size(id, cap)
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
        // `smelt.win.new(buf, { focusable = false })`).
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
    fn fire_win_event_fires_on_leaf_not_root() {
        // Events fire on the leaf they're directed at. Consumers that need to catch
        // events from multiple panels register on each leaf themselves.
        let mut ui = make_ui();
        let a = WinId(70);
        let b = WinId(71);
        let _id = ui.overlay_open(modal_overlay_with_leaves(a, b, WinId(72)));
        let saw_a = std::sync::Arc::new(std::sync::Mutex::new(false));
        let saw_b = std::sync::Arc::new(std::sync::Mutex::new(false));
        {
            let saw_cb = saw_a.clone();
            ui.win_on_event(
                a,
                WinEvent::Submit,
                Callback::Rust(Box::new(move |_| {
                    *saw_cb.lock().unwrap() = true;
                    CallbackResult::Consumed
                })),
            );
        }
        {
            let saw_cb = saw_b.clone();
            ui.win_on_event(
                b,
                WinEvent::Submit,
                Callback::Rust(Box::new(move |_| {
                    *saw_cb.lock().unwrap() = true;
                    CallbackResult::Consumed
                })),
            );
        }
        ui.fire_win_event(b, WinEvent::Submit, Payload::None, &mut |_, _, _| {});
        assert!(!*saw_a.lock().unwrap(), "root handler must not fire");
        assert!(*saw_b.lock().unwrap(), "leaf handler must fire");
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
        // Outer overlay: 42×5 (40 leaf width + 2 border, 3 leaf height + 2
        // border). Leaf rect width = 40, minus the 1-col scrollbar reservation
        // from `Gutters::default()` ⇒ parser called with 39.
        let widths = calls.lock().unwrap().clone();
        assert!(
            widths.contains(&39),
            "parser must be invoked at the leaf's content width; got {widths:?}"
        );
    }

    #[test]
    fn overlay_leaf_with_overflowing_content_auto_attaches_scrollbar() {
        // Verifies the post-render auto-attach path: an overlay leaf opened with
        // the default `Gutters::default()` (scrollbar = true) and a buffer whose
        // line count exceeds the leaf height must end up with a populated
        // `viewport.scrollbar`. This is the contract the wheel-routing and
        // paint_scrollbar paths rely on for /stats, /help, /messages.
        let mut ui = make_ui(); // 80x24 terminal
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            let lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
            b.set_all_lines(lines);
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
        // A 40-wide × 10-tall content area inside a single border ⇒ 42×12 overlay.
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(10),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        let vp = ui
            .wins
            .get(&win)
            .and_then(|w| w.viewport)
            .expect("viewport populated by pre-pass");
        assert_eq!(vp.total_rows, 60, "total_rows must equal buf.line_count()");
        assert_eq!(vp.rect.height, 10, "leaf height = layout-resolved height");
        let bar = vp.scrollbar.expect(
            "scrollbar must auto-attach: gutters.scrollbar=true and total_rows>viewport_rows",
        );
        assert_eq!(bar.total_rows, 60);
        assert_eq!(bar.viewport_rows, 10);
        // Bar column = leaf_rect.right - 1. Overlay centered: term 80, ov w=42
        // ⇒ left=19, leaf left=20, leaf width=40 ⇒ bar_col=59.
        assert_eq!(bar.col, 59, "scrollbar lives at rightmost column of leaf");
    }

    #[test]
    fn wheel_over_overflowing_overlay_leaf_pans_scroll_top() {
        // Real-world regression for /stats, /help, /messages: a mouse wheel
        // event over a focused overlay leaf with overflowing content must
        // update `scroll_top` (so wheel-scroll feels live).
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            let lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
            b.set_all_lines(lines);
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
        if let Some(w) = ui.win_mut(win) {
            w.focusable = true;
        }
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(10),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        let ov = Overlay::new(layout, layout::Anchor::ScreenCenter);
        let ov = Overlay { modal: true, ..ov };
        ui.overlay_open(ov);
        // Render once so the pre-pass populates the viewport before wheel routing
        // hit-tests against it.
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        assert_eq!(ui.focus(), Some(win), "modal focused first leaf");
        assert_eq!(ui.win(win).unwrap().scroll_top, 0, "starts at top");
        // Wheel-down over a cell inside the leaf rect (centered overlay puts
        // the leaf at left=20..60, top=7..17 on a 80x24 terminal).
        let scroll = mouse_event(crossterm::event::MouseEventKind::ScrollDown, 10, 30);
        let status = ui.dispatch_event(scroll, &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        assert!(
            ui.win(win).unwrap().scroll_top > 0,
            "wheel must advance scroll_top via pan_by_lines"
        );
    }

    #[test]
    fn hit_test_returns_scrollbar_for_overlay_bar_column() {
        // Regression: previously hit_test short-circuited to HitTarget::Window for any
        // overlay position, making the auto-attached overlay scrollbar undraggable.
        // After the fix, the rightmost column of an overflowing overlay leaf must
        // return HitTarget::Scrollbar so the drag-capture path fires.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            let lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
            b.set_all_lines(lines);
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
            Constraint::Length(10),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        // bar_col = 59 (see overlay_leaf_with_overflowing_content_auto_attaches_scrollbar).
        let hit = ui
            .hit_test(10, 59, None)
            .expect("rightmost col hits something");
        assert!(
            matches!(hit, HitTarget::Scrollbar { owner } if owner == win),
            "rightmost column of overflowing overlay leaf must be Scrollbar, got {hit:?}",
        );
        // Adjacent column to the left is still Window.
        let hit = ui.hit_test(10, 58, None).expect("interior col hits window");
        assert!(
            matches!(hit, HitTarget::Window(w) if w == win),
            "interior column must still be Window, got {hit:?}",
        );
    }

    #[test]
    fn overlay_leaf_without_overflow_has_no_scrollbar() {
        // Companion to the above: when content fits, ScrollbarState::new returns None.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines(vec!["short".into()]);
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
            Constraint::Length(10),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        let vp = ui
            .wins
            .get(&win)
            .and_then(|w| w.viewport)
            .expect("viewport populated");
        assert!(
            vp.scrollbar.is_none(),
            "no overflow → ScrollbarState::new returns None"
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
            {
                let w = host.win_mut(win).unwrap();
                w.cursor_row = 0;
                w.cursor_col = 3;
            }
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
        let t0 = std::time::Instant::now();
        // Same cell, no time gap → climbs to 3, then wraps.
        assert_eq!(ui.record_click(5, 7, t0), 1);
        assert_eq!(ui.record_click(5, 7, t0), 2);
        assert_eq!(ui.record_click(5, 7, t0), 3);
        assert_eq!(ui.record_click(5, 7, t0), 1);
        // Different cell resets the count.
        assert_eq!(ui.record_click(5, 7, t0), 2);
        assert_eq!(ui.record_click(8, 7, t0), 1);
    }

    /// Two clicks on the same cell more than 400ms apart count as fresh
    /// singles, not a double-click. Exercising the `now` parameter directly
    /// proves the gap check reads the host clock rather than `Instant::now`.
    #[test]
    fn record_click_resets_after_400ms_gap() {
        let mut ui = make_ui();
        let t0 = std::time::Instant::now();
        assert_eq!(ui.record_click(5, 7, t0), 1);
        let later = t0 + std::time::Duration::from_millis(401);
        assert_eq!(ui.record_click(5, 7, later), 1);
        // A follow-up within 400ms of `later` still pairs with it.
        let still_near = later + std::time::Duration::from_millis(100);
        assert_eq!(ui.record_click(5, 7, still_near), 2);
    }

    /// Set up a single splits leaf at `(0, 0, 20, 10)` with a painted
    /// scrollbar at column 19 covering 100 rows of content. Returns
    /// the leaf's `WinId` so callers can latch capture / hit-test
    /// against it.
    fn make_scrollbar_split(ui: &mut Ui) -> WinId {
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
            b.set_all_lines(lines);
        }
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
        let resolved = ui.resolve_split_mouse(me, std::time::Instant::now());
        assert_eq!(resolved, Some((win, 1)));
        assert_eq!(ui.capture(), Some(HitTarget::Window(win)));
        // A second Down on the same cell increments the click count.
        let resolved = ui.resolve_split_mouse(me, std::time::Instant::now());
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
        let resolved = ui.resolve_split_mouse(drag, std::time::Instant::now());
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
        let resolved = ui.resolve_split_mouse(up, std::time::Instant::now());
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
        assert_eq!(ui.resolve_split_mouse(me, std::time::Instant::now()), None);
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
        assert_eq!(
            ui.resolve_split_mouse(drag, std::time::Instant::now()),
            None
        );
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
        assert_eq!(ui.resolve_split_mouse(me, std::time::Instant::now()), None);
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
        {
            let w = ui.win_mut(win).unwrap();
            w.cursor_row = 0;
        }
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
        {
            let w = ui.win_mut(win).unwrap();
            w.cursor_row = 9;
        }
        assert_eq!(ui.poll_drag_autoscroll(), Some((win, 1)));
    }

    #[test]
    fn poll_drag_autoscroll_clears_started_at_when_cursor_leaves_edge() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        {
            let w = ui.win_mut(win).unwrap();
            w.cursor_row = 0;
        }
        let _ = ui.poll_drag_autoscroll();
        assert!(ui.drag_autoscroll_started().is_some());
        {
            let w = ui.win_mut(win).unwrap();
            w.cursor_row = 5;
        }
        assert_eq!(ui.poll_drag_autoscroll(), None);
        assert_eq!(ui.drag_autoscroll_started(), None);
    }

    #[test]
    fn poll_drag_autoscroll_clears_started_at_when_capture_releases() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        {
            let w = ui.win_mut(win).unwrap();
            w.cursor_row = 0;
        }
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
        ui.win_mut(win).unwrap().cursor_row = 0;
        assert_eq!(ui.poll_drag_autoscroll(), None);
    }
}
