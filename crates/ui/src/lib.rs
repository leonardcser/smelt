pub mod buffer;
pub mod buffer_view;
pub mod callback;
pub mod clipboard;
pub mod cmdline;
pub mod component;
pub mod compositor;
pub mod dialog;
pub mod edit_buffer;
pub mod flush;
pub mod grid;
pub mod kill_ring;
pub mod layout;
pub mod notification;
pub mod option_list;
pub mod overlay;
pub mod picker;
pub mod status_bar;
pub mod style;
pub mod text;
pub mod text_input;
pub mod theme;
pub mod undo;
pub mod vim;
pub mod window;
pub mod window_cursor;

mod id;

pub type AttachmentId = u64;

/// Callback shape for routing `Callback::Lua` handles out of Ui into
/// the host's Lua runtime. Receives the handle, the focused window,
/// the event payload, and a live snapshot of the window's dialog
/// panels for pull-read inside the Lua callback. Empty slice when the
/// window isn't a dialog.
pub type LuaInvoke<'a> =
    dyn FnMut(callback::LuaHandle, id::WinId, &callback::Payload, &[dialog::PanelSnapshot]) + 'a;

pub use buffer::{BufType, Buffer, BufferParser, Span, SpanStyle};
pub use buffer_view::BufferView;
pub use callback::{
    Callback, CallbackCtx, CallbackResult, Callbacks, KeyBind, LuaHandle, Payload, RustCallback,
    WinEvent,
};
pub use cmdline::{Cmdline, CmdlineStyle};
pub use component::{Component, CursorInfo, CursorStyle, DrawContext, KeyResult, WidgetEvent};
pub use compositor::Compositor;
pub use dialog::{
    Dialog, DialogConfig, PanelContent, PanelHeight, PanelSnapshot, PanelSpec, PanelWidget,
};
pub use edit_buffer::EditBuffer;
pub use flush::flush_diff;
pub use grid::{Cell, Grid, GridSlice, Style};
pub use id::{BufId, WinId, LUA_BUF_ID_BASE};
pub use kill_ring::KillRing;
pub use layout::{
    Anchor, Border, Constraint, Corner, FitMax, FloatRelative, Gutters, LayoutTree, Placement,
    Rect, SeparatorStyle,
};
pub use notification::{Notification, NotificationLevel, NotificationStyle};
pub use option_list::{OptionItem, OptionList};
pub use overlay::{HitTarget, Overlay, OverlayHitTarget, OverlayId};
pub use picker::{Picker, PickerItem, PickerStyle};
pub use status_bar::{StatusBar, StatusSegment};
pub use style::{HlAttrs, HlGroup};
pub use text_input::TextInput;
pub use theme::Theme;
pub use undo::{UndoEntry, UndoHistory};
pub use vim::{ViMode, Vim};
pub use window::{
    FloatConfig, MouseAction, MouseCtx, ScrollbarState, SplitConfig, ViewportHit, WinConfig,
    Window, WindowViewport,
};
pub use window_cursor::WindowCursor;

use std::collections::HashMap;

pub struct Ui {
    bufs: HashMap<BufId, Buffer>,
    wins: HashMap<WinId, Window>,
    current_win: Option<WinId>,
    next_buf_id: u64,
    next_win_id: u64,
    layout: Option<LayoutTree>,
    terminal_size: (u16, u16),
    compositor: Compositor,
    callbacks: Callbacks,
    /// Rects for split / non-float windows (PROMPT_WIN, TRANSCRIPT_WIN,
    /// …). Float rects are computed per-frame from `FloatConfig`; split
    /// rects are laid out by the host app (TUI render loop) and pushed
    /// in via [`Ui::set_window_rect`] so `Placement::DockedAbove` can
    /// look them up without knowing layout specifics.
    split_rects: HashMap<WinId, Rect>,
    /// Compositor layer-id ↔ `WinId` for split windows. Lets the focused
    /// split's per-window keymap fire from `handle_key_with_lua`, the
    /// same dispatch path used by floats. Floats use the `"float:N"`
    /// layer-id prefix instead of this map.
    splits: HashMap<String, WinId>,
    /// Theme registry — single source of truth for highlight groups.
    /// Cloned into every `DrawContext` at frame start so widgets read
    /// `ctx.theme.get(name)` instead of host-side colour constants. The
    /// host populates this at startup; users override via Lua.
    theme: Theme,
    /// P1.c overlay storage. Each overlay is a positioned LayoutTree
    /// of windows; `Ui::overlay_open` returns an `OverlayId` and
    /// `resolve_anchor` is the per-frame positioning primitive.
    /// Today's `FloatConfig` plumbing still drives rendering — the
    /// migration is incremental (C.4+).
    overlays: HashMap<OverlayId, Overlay>,
    next_overlay_id: u32,
    /// Stack of prior focused windows. `set_focus` pushes the
    /// outgoing focus here; the compositor / overlay close paths
    /// (later) walk back through it for the most recent
    /// still-existing focusable window. The current focus is held
    /// by the compositor as a layer id; this list is a parallel
    /// view in `WinId` terms so the focus model speaks the same
    /// language as the public `set_focus`/`focus` API.
    focus_history: Vec<WinId>,
    /// Currently focused overlay leaf, if any. Overlay leaves are
    /// not compositor layers — they live inside an `Overlay`'s
    /// `LayoutTree` — so focus-on-overlay-leaf can't be expressed
    /// via `compositor.focus(layer_id)`. When set, this takes
    /// precedence over `compositor.focused()` in the public
    /// `focus()` accessor and routes key events to the leaf's
    /// callback registry. Cleared when the containing overlay
    /// closes.
    overlay_focus: Option<WinId>,
}

/// Reserved `WinId` for the main prompt input window. The prompt is
/// rendered as a compositor layer (not inserted into `Ui::wins`), but
/// callbacks (keymaps, `WinEvent` subscribers) are keyed by `WinId`,
/// so we reserve a stable id so Lua can `smelt.win.on_event(prompt, …)`
/// and `smelt.win.set_keymap(prompt, …)` like any other window.
pub const PROMPT_WIN: WinId = WinId(0);

/// Reserved `WinId` for the transcript (scroll-back) window. Same
/// rationale as [`PROMPT_WIN`] — stable id for callback registration.
pub const TRANSCRIPT_WIN: WinId = WinId(1);

impl Ui {
    pub fn new() -> Self {
        Self {
            bufs: HashMap::new(),
            wins: HashMap::new(),
            current_win: None,
            next_buf_id: 1,
            // 0 is reserved for PROMPT_WIN, 1 for TRANSCRIPT_WIN.
            next_win_id: 2,
            layout: None,
            terminal_size: (80, 24),
            compositor: Compositor::new(80, 24),
            callbacks: Callbacks::new(),
            split_rects: HashMap::new(),
            splits: HashMap::new(),
            theme: Theme::new(),
            overlays: HashMap::new(),
            next_overlay_id: 1,
            focus_history: Vec::new(),
            overlay_focus: None,
        }
    }

    /// Bind a compositor split-layer id to a `WinId` so the focused
    /// split flows through [`Ui::handle_key_with_lua`] like floats do.
    /// Call once per split at startup.
    pub fn register_split(&mut self, layer_id: impl Into<String>, win_id: WinId) {
        self.splits.insert(layer_id.into(), win_id);
    }

    fn layer_to_win(&self, layer_id: &str) -> Option<WinId> {
        parse_float_layer_id(layer_id).or_else(|| self.splits.get(layer_id).copied())
    }

    /// Publish a split window's current rect. Call each frame from the
    /// host app's layout pass so `Placement::DockedAbove` can resolve
    /// correctly. Floats don't need this — their rects come from
    /// `FloatConfig`.
    pub fn set_window_rect(&mut self, win_id: WinId, rect: Rect) {
        self.split_rects.insert(win_id, rect);
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
        if id.0 >= self.next_buf_id {
            self.next_buf_id = id.0 + 1;
        }
        Ok(id)
    }

    pub fn buf_delete(&mut self, id: BufId) {
        self.wins.retain(|_, w| w.buf != id);
        self.bufs.remove(&id);
    }

    pub fn buf(&self, id: BufId) -> Option<&Buffer> {
        self.bufs.get(&id)
    }

    pub fn buf_mut(&mut self, id: BufId) -> Option<&mut Buffer> {
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
        self.overlays.insert(id, overlay);
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
        let removed = self.overlays.remove(&id)?;
        if let Some(focused) = self.focus() {
            if removed.layout.contains_leaf(focused) {
                self.overlay_focus = None;
                while let Some(prior) = self.focus_history.pop() {
                    if self.overlay_for_leaf(prior).is_some() {
                        self.overlay_focus = Some(prior);
                        self.compositor.clear_focus();
                        return Some(removed);
                    }
                    if let Some(layer_id) = self.layer_id_for_win(prior) {
                        self.compositor.focus(layer_id);
                        return Some(removed);
                    }
                }
                // History exhausted — clear stale focus so the next
                // input doesn't dispatch through a vanished layer.
                self.compositor.clear_focus();
            }
        }
        Some(removed)
    }

    pub fn overlay(&self, id: OverlayId) -> Option<&Overlay> {
        self.overlays.get(&id)
    }

    pub fn overlay_mut(&mut self, id: OverlayId) -> Option<&mut Overlay> {
        self.overlays.get_mut(&id)
    }

    /// Iterate overlays in stacking order (lowest `z` first; ties
    /// broken by insertion order via `OverlayId`).
    pub fn overlays_in_z_order(&self) -> Vec<(OverlayId, &Overlay)> {
        let mut entries: Vec<(OverlayId, &Overlay)> =
            self.overlays.iter().map(|(id, o)| (*id, o)).collect();
        entries.sort_by_key(|(id, o)| (o.z, id.0));
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
    /// the cell belongs to: an overlay leaf or chrome, or a split
    /// window underneath. Overlays are checked first (topmost-z to
    /// lowest, modal-aware — see `overlay_hit_test`); when no
    /// overlay covers the point, falls back to the compositor's
    /// layer-level lookup which today owns split + float layers.
    /// `Scrollbar` results are reserved for P1.d when Window
    /// publishes its scrollbar rect; this method never returns
    /// `Scrollbar` yet.
    pub fn hit_test(
        &self,
        row: u16,
        col: u16,
        cursor: Option<(u16, u16)>,
    ) -> Option<HitTarget> {
        if let Some((id, target)) = self.overlay_hit_test(row, col, cursor) {
            return Some(match target {
                OverlayHitTarget::Window(w) => HitTarget::Window(w),
                OverlayHitTarget::Chrome => HitTarget::Chrome { owner: id },
            });
        }
        let layer_id = self.compositor.hit_test(row, col)?;
        let win = self.layer_to_win(layer_id)?;
        Some(HitTarget::Window(win))
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
    pub fn overlay_hit_test(
        &self,
        row: u16,
        col: u16,
        cursor: Option<(u16, u16)>,
    ) -> Option<(OverlayId, OverlayHitTarget)> {
        let modal_z = self.active_modal().and_then(|id| self.overlays.get(&id).map(|o| o.z));
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
    /// `cursor`, or whose `Anchor::Win` target is absent from
    /// `split_rects`, are skipped silently. The caller (compositor
    /// integration in C.5+) feeds the cursor it knows from focus.
    pub fn resolve_overlays(
        &self,
        cursor: Option<(u16, u16)>,
    ) -> Vec<(OverlayId, Rect, &Overlay)> {
        let (term_w, term_h) = self.terminal_size;
        let ctx = overlay::AnchorContext {
            term_width: term_w,
            term_height: term_h,
            cursor,
            win_rects: &self.split_rects,
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
        let win = Window::new(id, buf, WinConfig::Split(config));
        self.wins.insert(id, win);
        if self.current_win.is_none() {
            self.current_win = Some(id);
        }
        Some(id)
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
                    if self.current_win == Some(leaf) {
                        self.current_win = self.wins.keys().next().copied();
                    }
                }
            }
            return all_ids;
        }
        let layer_id = float_layer_id(id);
        self.compositor.remove(&layer_id);
        self.wins.remove(&id);
        let lua_ids = self.callbacks.clear_all(id);
        if self.current_win == Some(id) {
            self.current_win = self.wins.keys().next().copied();
        }
        lua_ids
    }

    // ── Picker ───────────────────────────────────────────────────────
    //
    // Non-focusable dropdown component. One primitive reused by the
    // prompt `/` completer, the cmdline `:` completer, and Lua
    // `smelt.ui.picker.open`. Mirrors Neovim's `pum_grid`: caller drives
    // selection, component paints.

    /// Open a `Picker` float. Accepts any `FloatConfig`; typically
    /// callers pass `focusable: false` and a manually-positioned
    /// `Placement`. `reversed` paints logical index 0 on the bottom
    /// visual row — used by pickers that dock *above* the prompt
    /// (completer `/`, cmdline `:`) so the best match is closest to
    /// where the user is typing. Returns the new `WinId` for later
    /// updates via `picker_mut` and eventual `win_close`.
    pub fn picker_open(
        &mut self,
        config: FloatConfig,
        items: Vec<picker::PickerItem>,
        selected: usize,
        style: picker::PickerStyle,
        reversed: bool,
    ) -> Option<WinId> {
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;

        let (tw, th) = self.terminal_size;
        let rect = resolve_float_rect(&config, tw, th, None, &self.split_rects);
        let zindex = config.zindex;

        let mut p = picker::Picker::new()
            .with_style(style)
            .with_reversed(reversed);
        p.set_items(items);
        p.set_selected(selected);

        // Pickers don't own a buffer; they render from internal state.
        // Still register a placeholder window so focus / close / keymap
        // paths stay uniform with other floats.
        let placeholder_buf = BufId(0);
        let focusable = config.focusable;
        let mut win = Window::new(id, placeholder_buf, WinConfig::Float(config));
        win.focusable = focusable;
        self.wins.insert(id, win);

        let layer_id = float_layer_id(id);
        // Pickers are non-focusable in the keymap sense; mouse clicks
        // should not promote them to focused or raise their zindex
        // (they sit beneath dialogs/cmdline by design).
        let opts = compositor::LayerOpts {
            focus_on_click: focusable,
            raise_on_click: focusable,
        };
        self.compositor
            .add_with_opts(&layer_id, Box::new(p), rect, zindex, opts);
        if focusable {
            self.compositor.focus(&layer_id);
        }

        Some(id)
    }

    /// Mutable access to an open `Picker` component. Used to update
    /// items and selection as the caller's filter/selection state
    /// changes.
    pub fn picker_mut(&mut self, win_id: WinId) -> Option<&mut picker::Picker> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component_mut(&layer_id)?;
        comp.as_any_mut().downcast_mut::<picker::Picker>()
    }

    // ── Notification ─────────────────────────────────────────────────
    //
    // Non-focusable ephemeral toast. Sibling to `Picker` in the named-
    // components family. Caller controls lifecycle (open / update /
    // dismiss on key → `win_close`).

    pub fn notification_open(
        &mut self,
        config: FloatConfig,
        message: impl Into<String>,
        level: notification::NotificationLevel,
        style: notification::NotificationStyle,
    ) -> Option<WinId> {
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;

        let (tw, th) = self.terminal_size;
        let rect = resolve_float_rect(&config, tw, th, None, &self.split_rects);
        let zindex = config.zindex;

        let n = notification::Notification::new(message, level).with_style(style);

        let placeholder_buf = BufId(0);
        let focusable = config.focusable;
        let mut win = Window::new(id, placeholder_buf, WinConfig::Float(config));
        win.focusable = focusable;
        self.wins.insert(id, win);

        let layer_id = float_layer_id(id);
        // Toast: non-focusable, sits below dialogs by design. Click
        // dispatches `Dismiss` (handled by App), so suppress focus +
        // raise so the click doesn't bump the toast above a real modal.
        let opts = compositor::LayerOpts {
            focus_on_click: false,
            raise_on_click: false,
        };
        self.compositor
            .add_with_opts(&layer_id, Box::new(n), rect, zindex, opts);
        if focusable {
            self.compositor.focus(&layer_id);
        }

        Some(id)
    }

    pub fn notification_mut(&mut self, win_id: WinId) -> Option<&mut notification::Notification> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component_mut(&layer_id)?;
        comp.as_any_mut()
            .downcast_mut::<notification::Notification>()
    }

    // ── Cmdline ──────────────────────────────────────────────────────
    //
    // Focusable single-row compositor float for `:` / `/`-style command
    // entry. Sibling to `Picker` and `Notification`. Callers that want
    // Tab-complete register per-window keymaps and drive completion via
    // `Cmdline::{text, set_text}`.

    pub fn cmdline_open(
        &mut self,
        config: FloatConfig,
        cmdline: cmdline::Cmdline,
    ) -> Option<WinId> {
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;

        let (tw, th) = self.terminal_size;
        let rect = resolve_float_rect(&config, tw, th, None, &self.split_rects);
        let zindex = config.zindex;

        let placeholder_buf = BufId(0);
        let focusable = config.focusable;
        let mut win = Window::new(id, placeholder_buf, WinConfig::Float(config));
        win.focusable = focusable;
        self.wins.insert(id, win);

        let layer_id = float_layer_id(id);
        self.compositor
            .add(&layer_id, Box::new(cmdline), rect, zindex);
        if focusable {
            self.compositor.focus(&layer_id);
        }

        Some(id)
    }

    pub fn cmdline_mut(&mut self, win_id: WinId) -> Option<&mut cmdline::Cmdline> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component_mut(&layer_id)?;
        comp.as_any_mut().downcast_mut::<cmdline::Cmdline>()
    }

    pub fn cmdline(&self, win_id: WinId) -> Option<&cmdline::Cmdline> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component(&layer_id)?;
        comp.as_any().downcast_ref::<cmdline::Cmdline>()
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
    /// specific keymaps miss and before `Component::handle_key`.
    /// Returns the displaced callback (if any).
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

    /// Dispatch a window event to its registered callbacks.
    /// `lua_invoke` is called for each `Callback::Lua` with
    /// (handle, payload). Side effects flow through the `AppOp` queue
    /// that Rust callbacks have via `shared.ops` — no return channel.
    pub fn dispatch_event(
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
                    let panels = self.snapshot_dialog_panels(win);
                    lua_invoke(*handle, win, &payload, &panels);
                }
            }
        }
        self.callbacks.restore_event(win, ev, cbs);
    }

    /// Open a multi-panel `Dialog` as a compositor float layer.
    ///
    /// Panels' buffers stay in `Ui::bufs`; each panel's interaction
    /// state (cursor, scroll) lives in a dialog-local `Window`. The
    /// dialog's own `WinId` is registered in `Ui::wins` as a float so
    /// focus / close paths match `win_open_float`.
    pub fn dialog_open(
        &mut self,
        float_config: FloatConfig,
        dialog_config: dialog::DialogConfig,
        panels: Vec<dialog::PanelSpec>,
    ) -> Option<WinId> {
        let all_bufs_registered = panels.iter().all(|p| match &p.content {
            dialog::PanelContent::Buffer(b) => self.bufs.contains_key(b),
            dialog::PanelContent::Widget(_) => true,
        });
        if !all_bufs_registered {
            return None;
        }
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;

        let (tw, th) = self.terminal_size;
        let zindex = float_config.zindex;

        // Use the first buffer-backed panel's buffer as the dialog
        // window's "buf" pointer for registry purposes (dialogs are
        // multi-buffer and may be widget-only).
        let primary_buf = panels
            .iter()
            .find_map(|p| match &p.content {
                dialog::PanelContent::Buffer(b) => Some(*b),
                dialog::PanelContent::Widget(_) => None,
            })
            .unwrap_or(BufId(0));

        let panel_structs = dialog::build_panels(panels, &mut self.bufs);
        let mut dlg = dialog::Dialog::new(dialog_config, panel_structs);
        // Pre-sync so `FitContent` sees a populated `line_count` on
        // the first frame — otherwise the dialog lands at the cap
        // height on open, then snaps to fit on the next render.
        //
        // The float rect's width is independent of `natural_h` (every
        // `Placement` resolves width from terminal size + its width
        // constraint, not the content height), so we compute it first
        // with `natural_h = None` and feed the content width into
        // formatter-backed buffers before sampling `natural_height`.
        let pre_rect = resolve_float_rect(&float_config, tw, th, None, &self.split_rects);
        let content_width = pre_rect.width.saturating_sub(1);
        dlg.sync_from_bufs_mut(content_width, &mut self.bufs);
        let natural_h = Some(dlg.natural_height());
        let rect = resolve_float_rect(&float_config, tw, th, natural_h, &self.split_rects);

        let focusable = float_config.focusable;
        let mut win = Window::new(id, primary_buf, WinConfig::Float(float_config));
        win.focusable = focusable;
        self.wins.insert(id, win);

        let layer_id = float_layer_id(id);
        self.compositor.add(&layer_id, Box::new(dlg), rect, zindex);
        self.compositor.focus(&layer_id);

        Some(id)
    }

    /// Access a `Dialog` compositor layer by its WinId for
    /// post-creation configuration (hints, dismiss keys, etc.).
    pub fn dialog_mut(&mut self, win_id: WinId) -> Option<&mut dialog::Dialog> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component_mut(&layer_id)?;
        comp.as_any_mut().downcast_mut::<dialog::Dialog>()
    }

    pub fn dialog(&self, win_id: WinId) -> Option<&dialog::Dialog> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component(&layer_id)?;
        comp.as_any().downcast_ref::<dialog::Dialog>()
    }

    /// Build a live snapshot of every panel on the `Dialog` at `win_id`
    /// for Lua pull-read. Empty vec when the window isn't a dialog.
    /// `selected` is the 0-based selection index when the panel is a
    /// list widget; `text` is the current text for input widgets.
    pub fn snapshot_dialog_panels(&self, win_id: WinId) -> Vec<dialog::PanelSnapshot> {
        let Some(dlg) = self.dialog(win_id) else {
            return Vec::new();
        };
        (0..dlg.panel_count())
            .map(|i| dialog::PanelSnapshot {
                selected: dlg.selected_index_at(i),
                text: dlg.panel_widget_text(i).unwrap_or_default(),
            })
            .collect()
    }

    pub fn win(&self, id: WinId) -> Option<&Window> {
        self.wins.get(&id)
    }

    pub fn win_mut(&mut self, id: WinId) -> Option<&mut Window> {
        self.wins.get_mut(&id)
    }

    pub fn win_list(&self) -> Vec<WinId> {
        self.wins.keys().copied().collect()
    }

    pub fn current_win(&self) -> Option<WinId> {
        self.current_win
    }

    pub fn set_current_win(&mut self, id: WinId) {
        if self.wins.contains_key(&id) {
            self.current_win = Some(id);
        }
    }

    pub fn set_terminal_size(&mut self, w: u16, h: u16) {
        self.terminal_size = (w, h);
        self.compositor.resize(w, h);
    }

    pub fn terminal_size(&self) -> (u16, u16) {
        self.terminal_size
    }

    pub fn set_layout(&mut self, tree: LayoutTree) {
        self.layout = Some(tree);
    }

    pub fn layout(&self) -> Option<&LayoutTree> {
        self.layout.as_ref()
    }

    pub fn floats_z_ordered(&self) -> Vec<WinId> {
        let mut floats: Vec<_> = self
            .wins
            .iter()
            .filter(|(_, w)| matches!(w.config, WinConfig::Float(_)))
            .map(|(id, w)| {
                let z = match &w.config {
                    WinConfig::Float(f) => f.zindex,
                    _ => 0,
                };
                (*id, z)
            })
            .collect();
        floats.sort_by_key(|(_, z)| *z);
        floats.into_iter().map(|(id, _)| id).collect()
    }

    pub fn resolve_splits(&self) -> HashMap<WinId, Rect> {
        let (w, h) = self.terminal_size;
        match &self.layout {
            Some(tree) => layout::resolve_layout(tree, Rect::new(0, 0, w, h)),
            None => HashMap::new(),
        }
    }

    pub fn resolve_float(&self, win_id: WinId) -> Option<Rect> {
        let win = self.wins.get(&win_id)?;
        let fc = match &win.config {
            WinConfig::Float(f) => f,
            _ => return None,
        };
        let (tw, th) = self.terminal_size;
        let natural_h = self.natural_layer_height(win_id);
        Some(resolve_float_rect(fc, tw, th, natural_h, &self.split_rects))
    }

    pub fn resolve_float_rects(&self) -> Vec<(WinId, Rect)> {
        let (tw, th) = self.terminal_size;
        self.floats_z_ordered()
            .into_iter()
            .filter_map(|id| {
                let win = self.wins.get(&id)?;
                let fc = match &win.config {
                    WinConfig::Float(f) => f,
                    _ => return None,
                };
                let natural_h = self.natural_layer_height(id);
                Some((
                    id,
                    resolve_float_rect(fc, tw, th, natural_h, &self.split_rects),
                ))
            })
            .collect()
    }

    /// Peek at a float layer's natural height for placement. Dialogs
    /// expose `line_count`-derived height (used by `FitContent`); pickers
    /// clamp item-count to `max_visible_rows` (used by `DockedAbove`).
    /// Returns `None` for other layer kinds — placement variants that
    /// don't consult natural height treat this as "use the max cap."
    fn natural_layer_height(&self, win_id: WinId) -> Option<u16> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component(&layer_id)?;
        if let Some(dlg) = comp.as_any().downcast_ref::<dialog::Dialog>() {
            return Some(dlg.natural_height());
        }
        if let Some(p) = comp.as_any().downcast_ref::<picker::Picker>() {
            return Some(p.natural_height());
        }
        None
    }

    // ── Focus (canonical Win-typed API) ──────────────────────────

    /// Currently focused window, if any. Overlay-leaf focus wins
    /// over compositor focus (a modal overlay's input claim
    /// suppresses split / float dispatch). Falls back to the
    /// compositor's focused layer translated to its `WinId` (split
    /// via `register_split`, float via the `"float:N"` layer-id
    /// prefix).
    pub fn focus(&self) -> Option<WinId> {
        if let Some(win) = self.overlay_focus {
            return Some(win);
        }
        self.compositor.focused().and_then(|id| self.layer_to_win(id))
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

    pub fn focused_window_mut(&mut self) -> Option<&mut Window> {
        let id = self.focus()?;
        self.wins.get_mut(&id)
    }

    /// Focus a specific window. Accepts splits, floats (via their
    /// compositor layer id), and overlay leaves (any leaf reachable
    /// in an open overlay's `LayoutTree`). Returns `false` when `win`
    /// is none of those. On success, the prior focus is appended to
    /// `focus_history` so close paths can pop back to the previous
    /// focus target. Re-focusing the already-focused window is a
    /// no-op (no history push).
    pub fn set_focus(&mut self, win: WinId) -> bool {
        let prior = self.focus();
        if prior == Some(win) {
            return true;
        }
        if let Some(layer_id) = self.layer_id_for_win(win) {
            if let Some(p) = prior {
                self.focus_history.push(p);
            }
            self.overlay_focus = None;
            self.compositor.focus(layer_id);
            return true;
        }
        if self.overlay_for_leaf(win).is_some() {
            if let Some(p) = prior {
                self.focus_history.push(p);
            }
            self.overlay_focus = Some(win);
            self.compositor.clear_focus();
            return true;
        }
        false
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

    /// Read-only view of the focus history (oldest first; the most
    /// recent prior focus is at the back). Test + introspection
    /// helper; production callers should reach through `set_focus`.
    pub fn focus_history(&self) -> &[WinId] {
        &self.focus_history
    }

    /// Move focus to the next focusable window in cycle order.
    /// Returns `true` if focus changed. Modal-aware: when an
    /// `active_modal` overlay is open, cycles through that overlay's
    /// focusable leaves only. Returns `false` outside a modal —
    /// cross-source (split + float + overlay-leaf) z-order is gated
    /// on the unified Ui facade in P1.f.
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
        let Some(modal) = self.overlays.get(&modal_id) else {
            return false;
        };
        let leaves: Vec<WinId> = modal
            .layout
            .leaves_in_order()
            .into_iter()
            .filter(|w| {
                self.layer_id_for_win(*w).is_some() || self.wins.contains_key(w)
            })
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

    /// Resolve a `WinId` to its current compositor layer id. Splits
    /// register their layer-id explicitly via `register_split`;
    /// floats use the `"float:N"` prefix and only count when an
    /// actual layer is registered (a freshly-created `WinId` with no
    /// layer is unfocusable).
    fn layer_id_for_win(&self, win: WinId) -> Option<String> {
        // Splits: invert the layer-id → WinId map.
        if let Some((layer_id, _)) = self.splits.iter().find(|(_, w)| **w == win) {
            return Some(layer_id.clone());
        }
        // Floats: WinId is associated with a `float:N` layer only
        // when the compositor knows that layer.
        let candidate = float_layer_id(win);
        if self.compositor.component(&candidate).is_some() {
            return Some(candidate);
        }
        None
    }

    // ── Layer management ─────────────────────────────────────────

    pub fn add_layer(
        &mut self,
        id: impl Into<String>,
        component: Box<dyn Component>,
        rect: Rect,
        zindex: u16,
    ) {
        self.compositor.add(id, component, rect, zindex);
    }

    pub fn set_layer_rect(&mut self, id: &str, rect: Rect) {
        self.compositor.set_rect(id, rect);
    }

    pub fn focus_layer(&mut self, id: impl Into<String>) {
        self.compositor.focus(id);
    }

    pub fn layer_mut<T: 'static>(&mut self, id: &str) -> Option<&mut T> {
        self.compositor
            .component_mut(id)?
            .as_any_mut()
            .downcast_mut::<T>()
    }

    // ── Compositor delegation ──────────────────────────────────────

    pub fn render<W: std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.sync_float_content();
        self.sync_float_rects();
        let resolved = self.resolve_overlays(None);
        let resolved: Vec<(OverlayId, Rect, Overlay)> = resolved
            .into_iter()
            .map(|(id, rect, ov)| (id, rect, ov.clone()))
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
        let wins = &self.wins;
        let bufs = &self.bufs;
        let term_size = self.terminal_size;
        self.compositor
            .render_with(&self.theme, w, |grid, theme| {
                for (_id, rect, overlay) in &resolved {
                    paint_overlay(grid, theme, *rect, overlay, wins, bufs, term_size);
                }
            })
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn theme_mut(&mut self) -> &mut Theme {
        &mut self.theme
    }

    pub fn handle_key(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> KeyResult {
        self.handle_key_with_lua(code, mods, &mut |_, _, _, _| {})
    }

    /// Dispatch a key event through the focused window's keymap
    /// table, falling back to `Component::handle_key` if no binding
    /// matches. Floats and registered splits both flow through here —
    /// the focused layer's `WinId` is resolved via [`Ui::layer_to_win`].
    /// `lua_invoke` is called for each `Callback::Lua` with (handle,
    /// payload). Side effects (app-level commands, etc.) flow through
    /// `AppOp` from the host side.
    pub fn handle_key_with_lua(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> KeyResult {
        // Modal overlay built-in: bare Esc on an active modal closes
        // the topmost modal. Universal dismiss is fundamental
        // behaviour, not user-customisable. Before closing, fires
        // `WinEvent::Dismiss` on every leaf of the modal so
        // dialog-side `on_event("dismiss", …)` handlers can flush
        // pending state (e.g. unsubmitted input text). Leaves can
        // register their own callbacks for `q` / `Ctrl+C` / Submit
        // etc. through the regular `Callbacks` registry — see the
        // `focus()`-routed dispatch below.
        if matches!(code, crossterm::event::KeyCode::Esc)
            && mods == crossterm::event::KeyModifiers::NONE
        {
            if let Some(modal) = self.active_modal() {
                let leaves: Vec<WinId> = self
                    .overlay(modal)
                    .map(|o| o.layout.leaves_in_order())
                    .unwrap_or_default();
                for leaf in &leaves {
                    self.dispatch_event(*leaf, WinEvent::Dismiss, Payload::None, lua_invoke);
                }
                // The Lua dismiss handler may have already called
                // `smelt.win.close(...)` (which routes through
                // `Ui::win_close` → `overlay_close`). Re-check before
                // closing so we don't double-pop focus_history.
                if self.overlay(modal).is_some() {
                    let _ = self.overlay_close(modal);
                }
                return KeyResult::Consumed;
            }
        }
        let focused = self.focus();
        let Some(win) = focused else {
            return self.compositor.handle_key(code, mods);
        };
        let key = KeyBind::new(code, mods);
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
                        CallbackResult::Consumed => KeyResult::Consumed,
                        CallbackResult::Pass => self.compositor.handle_key(code, mods),
                    }
                }
                Callback::Lua(handle) => {
                    let payload = Payload::Key { code, mods };
                    let panels = self.snapshot_dialog_panels(win);
                    lua_invoke(*handle, win, &payload, &panels);
                    KeyResult::Consumed
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
                        CallbackResult::Consumed => KeyResult::Consumed,
                        CallbackResult::Pass => self.compositor.handle_key(code, mods),
                    }
                }
                Callback::Lua(handle) => {
                    let payload = Payload::Key { code, mods };
                    let panels = self.snapshot_dialog_panels(win);
                    lua_invoke(*handle, win, &payload, &panels);
                    KeyResult::Consumed
                }
            };
            self.callbacks.restore_key_fallback(win, cb);
            r
        } else {
            self.compositor.handle_key(code, mods)
        };

        // Auto-translate widget events into typed `WinEvent` callbacks
        // when the focused window has a matching callback registered.
        if let KeyResult::Action(action) = &result {
            let mapped = match action {
                WidgetEvent::Dismiss | WidgetEvent::Cancel => {
                    Some((WinEvent::Dismiss, Payload::None))
                }
                WidgetEvent::Submit | WidgetEvent::SelectDefault => {
                    Some((WinEvent::Submit, Payload::None))
                }
                WidgetEvent::TextChanged => Some((WinEvent::TextChanged, Payload::None)),
                WidgetEvent::Select(index) => {
                    Some((WinEvent::Submit, Payload::Selection { index: *index }))
                }
                WidgetEvent::SubmitText(content) => Some((
                    WinEvent::Submit,
                    Payload::Text {
                        content: content.clone(),
                    },
                )),
            };
            if let Some((ev, payload)) = mapped {
                if self.callbacks.has_event(win, ev) {
                    self.dispatch_event(win, ev, payload, lua_invoke);
                    return KeyResult::Consumed;
                }
            }
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
            self.dispatch_event(win, WinEvent::Tick, Payload::None, lua_invoke);
        }
    }

    pub fn focused_float(&self) -> Option<WinId> {
        let focused = self.compositor.focused()?;
        parse_float_layer_id(focused)
    }

    /// Buffer-bearing `Window` driving the user's current view, if any.
    /// Walks the focused float (a dialog) into its focused panel and
    /// returns that panel's `Window` when the panel is interactive.
    /// Falls through to `None` when the focused float is a picker /
    /// notification / widget panel — the host then decides whether to
    /// fall back to a split (transcript / prompt) for status purposes.
    /// Same model nvim uses: a focused float that isn't a "real" buffer
    /// view contributes no mode to the statusline.
    pub fn focused_dialog_buffer_window(&self) -> Option<&Window> {
        let win_id = self.focused_float()?;
        self.dialog(win_id)?.focused_buffer_window()
    }

    /// Topmost float (by zindex) whose rect contains (row, col). Used
    /// by the host's mouse dispatcher to route wheel / click events
    /// onto the float they actually land on — independent of focus —
    /// so scrolling an unfocused-but-visible float just works.
    pub fn float_at(&self, row: u16, col: u16) -> Option<WinId> {
        self.compositor
            .hit_test(row, col)
            .and_then(parse_float_layer_id)
    }

    /// Snapshot of every float window's `WinId`, in HashMap iteration
    /// order. Callers shouldn't rely on the ordering for painting —
    /// that's the compositor's job.
    pub fn float_ids(&self) -> Vec<WinId> {
        self.wins
            .iter()
            .filter_map(|(id, w)| matches!(w.config, WinConfig::Float(_)).then_some(*id))
            .collect()
    }

    /// Look up the `FloatConfig` for a window, if it's a float.
    /// Non-float windows (splits) return `None`.
    pub fn float_config(&self, win: WinId) -> Option<&FloatConfig> {
        match &self.wins.get(&win)?.config {
            WinConfig::Float(cfg) => Some(cfg),
            WinConfig::Split(_) => None,
        }
    }

    /// Mutable access to a float's `FloatConfig`. Used by callers that
    /// need to reposition a float in response to layout changes (e.g.
    /// the completer follows the prompt's top row). The next `render`
    /// call re-resolves the `Placement` automatically.
    pub fn float_config_mut(&mut self, win: WinId) -> Option<&mut FloatConfig> {
        match &mut self.wins.get_mut(&win)?.config {
            WinConfig::Float(cfg) => Some(cfg),
            WinConfig::Split(_) => None,
        }
    }

    pub fn force_redraw(&mut self) {
        self.compositor.force_redraw();
    }

    fn sync_float_content(&mut self) {
        let (tw, th) = self.terminal_size;
        // Pair each dialog layer with the content width it should
        // render at. Width is resolved from `Placement` alone — it
        // never depends on `natural_h`, so we can compute it before
        // the formatter runs — then we shave a column for the
        // potential scrollbar so wrapped content never bleeds under
        // the thumb.
        let targets: Vec<(String, u16)> = self
            .wins
            .iter()
            .filter(|(_, w)| w.is_float())
            .map(|(id, w)| {
                let width = match &w.config {
                    WinConfig::Float(fc) => {
                        resolve_float_rect(fc, tw, th, None, &self.split_rects).width
                    }
                    _ => tw,
                };
                (float_layer_id(*id), width.saturating_sub(1))
            })
            .collect();
        for (layer_id, content_width) in targets {
            // Split-borrow: `self.bufs` and `self.compositor` are
            // disjoint fields, so both mutable borrows coexist.
            let bufs = &mut self.bufs;
            if let Some(comp) = self.compositor.component_mut(&layer_id) {
                if let Some(dlg) = comp.as_any_mut().downcast_mut::<dialog::Dialog>() {
                    dlg.sync_from_bufs_mut(content_width, bufs);
                }
            }
        }
    }

    /// Re-resolve every float's rect from its `Placement` against the
    /// current terminal size and push it into the compositor. Called
    /// each frame from `render`, so floats track terminal resizes,
    /// prompt geometry changes, and dialog natural-height changes
    /// without any per-component wiring. Order matters: must run after
    /// `sync_float_content` so `Dialog::natural_height` reflects the
    /// current panel contents.
    fn sync_float_rects(&mut self) {
        for (id, rect) in self.resolve_float_rects() {
            self.compositor.set_rect(&float_layer_id(id), rect);
        }
    }
}

fn resolve_constraint_dim(c: Constraint, total: u16) -> u16 {
    match c {
        Constraint::Length(n) | Constraint::Min(n) | Constraint::Max(n) => n.min(total),
        Constraint::Percentage(pct) => ((total as u32 * pct as u32) / 100) as u16,
        Constraint::Ratio(num, denom) => {
            if denom == 0 {
                0
            } else {
                ((total as u32 * num as u32) / denom as u32) as u16
            }
        }
        Constraint::Fill | Constraint::Fit => total,
    }
}

fn resolve_float_rect(
    fc: &FloatConfig,
    term_w: u16,
    term_h: u16,
    natural_h: Option<u16>,
    split_rects: &HashMap<WinId, Rect>,
) -> Rect {
    resolve_placement(&fc.placement, term_w, term_h, natural_h, split_rects)
}

fn resolve_placement(
    p: &layout::Placement,
    term_w: u16,
    term_h: u16,
    natural_h: Option<u16>,
    split_rects: &HashMap<WinId, Rect>,
) -> Rect {
    match p {
        layout::Placement::DockBottom {
            above_rows,
            full_width,
            max_width,
            max_height,
        } => {
            let avail_h = term_h.saturating_sub(*above_rows);
            let cap = resolve_constraint_dim(*max_height, avail_h).min(avail_h);
            // Dialogs / pickers report a `natural_h` derived from panel
            // content; clamp to the cap so the rect grows with content
            // up to the cap instead of always allocating the cap.
            // Floats without a natural height (cmdline) keep the cap.
            let h = match natural_h {
                Some(n) => n.min(cap),
                None => cap,
            };
            let w = if *full_width {
                term_w
            } else {
                resolve_constraint_dim(*max_width, term_w).min(term_w)
            };
            let left = (term_w.saturating_sub(w)) / 2;
            let top = term_h.saturating_sub(*above_rows).saturating_sub(h);
            Rect::new(top, left, w, h)
        }
        layout::Placement::FitContent { max } => {
            // Dock at bottom, full width, 1 status-bar row reserved.
            // Height = natural_h clamped to the `max` cap; natural_h
            // comes from `Ui::natural_dialog_height(win_id)` at the
            // call site. If the caller didn't supply one (rect is
            // being computed before the dialog registered — first
            // open), fall back to the cap so the dialog lands somewhere
            // sensible and the next render corrects it.
            let above_rows = 1u16;
            let avail_h = term_h.saturating_sub(above_rows);
            let cap = match max {
                layout::FitMax::HalfScreen => avail_h / 2,
                layout::FitMax::FullScreen => avail_h,
            };
            let h = natural_h.unwrap_or(cap).min(cap).min(avail_h).max(1);
            let top = term_h.saturating_sub(above_rows).saturating_sub(h);
            Rect::new(top, 0, term_w, h)
        }
        layout::Placement::Centered { width, height } => {
            let w = resolve_constraint_dim(*width, term_w).min(term_w);
            let h = resolve_constraint_dim(*height, term_h).min(term_h);
            let top = term_h.saturating_sub(h) / 2;
            let left = term_w.saturating_sub(w) / 2;
            Rect::new(top, left, w, h)
        }
        layout::Placement::AnchorCursor {
            row_offset,
            col_offset,
            width,
            height,
        } => {
            // Without an explicit caret position in the ui crate, treat
            // AnchorCursor as an offset from the top-left. The tui layer
            // sets the layer rect after construction.
            let w = resolve_constraint_dim(*width, term_w).min(term_w);
            let h = resolve_constraint_dim(*height, term_h).min(term_h);
            let top = (*row_offset).max(0) as u16;
            let left = (*col_offset).max(0) as u16;
            let w = w.min(term_w.saturating_sub(left));
            let h = h.min(term_h.saturating_sub(top));
            Rect::new(top, left, w, h)
        }
        layout::Placement::Manual {
            anchor,
            row,
            col,
            width,
            height,
        } => {
            let w = resolve_constraint_dim(*width, term_w);
            let h = resolve_constraint_dim(*height, term_h);
            let (top, left) = match anchor {
                layout::Corner::NW => (*row, *col),
                layout::Corner::NE => (*row, *col - w as i32),
                layout::Corner::SW => (*row - h as i32, *col),
                layout::Corner::SE => (*row - h as i32, *col - w as i32),
            };
            let top = top.max(0) as u16;
            let left = left.max(0) as u16;
            let w = w.min(term_w.saturating_sub(left));
            let h = h.min(term_h.saturating_sub(top));
            Rect::new(top, left, w, h)
        }
        layout::Placement::DockedAbove { target, max_height } => {
            // Target rect comes from `split_rects` (host app publishes
            // split-window rects each frame). If missing, fall back to
            // a centered-ish placement so the float still renders.
            let anchor_rect = split_rects
                .get(target)
                .copied()
                .unwrap_or_else(|| Rect::new(term_h / 2, 0, term_w, 1));
            let max_h = resolve_constraint_dim(*max_height, anchor_rect.top).max(1);
            let h = natural_h.unwrap_or(max_h).min(max_h).max(1);
            let top = anchor_rect.top.saturating_sub(h);
            Rect::new(top, anchor_rect.left, anchor_rect.width, h)
        }
    }
}

fn float_layer_id(win_id: WinId) -> String {
    format!("float:{}", win_id.0)
}

fn parse_float_layer_id(id: &str) -> Option<WinId> {
    id.strip_prefix("float:")?.parse::<u64>().ok().map(WinId)
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

/// Paint one resolved overlay into `grid`. Walks the overlay's layout
/// tree depth-first: containers paint chrome at their own rect, then
/// recurse into children at their resolved rects; leaves slice into
/// the grid and call `Window::render`. Missing windows / buffers are
/// silently skipped — the paint pass is best-effort, not authoritative
/// over registry state.
fn paint_overlay(
    grid: &mut Grid,
    theme: &Theme,
    area: Rect,
    overlay: &Overlay,
    wins: &HashMap<WinId, Window>,
    bufs: &HashMap<BufId, Buffer>,
    term_size: (u16, u16),
) {
    paint_layout_node(grid, theme, &overlay.layout, area, wins, bufs, term_size);
}

fn paint_layout_node(
    grid: &mut Grid,
    theme: &Theme,
    node: &LayoutTree,
    area: Rect,
    wins: &HashMap<WinId, Window>,
    bufs: &HashMap<BufId, Buffer>,
    term_size: (u16, u16),
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
            let ctx = DrawContext {
                terminal_width: term_size.0,
                terminal_height: term_size.1,
                focused: false,
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
                paint_layout_node(grid, theme, child, child_area, wins, bufs, term_size);
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

    fn open_stub_float(ui: &mut Ui, config: FloatConfig) -> WinId {
        ui.picker_open(
            config,
            vec![picker::PickerItem::new("stub")],
            0,
            picker::PickerStyle::default(),
            false,
        )
        .unwrap()
    }

    #[test]
    fn float_default_config() {
        let mut ui = make_ui();
        let win = open_stub_float(&mut ui, FloatConfig::default());
        let rect = ui.resolve_float(win).unwrap();
        // Default placement: Centered 80%x50%.
        assert_eq!(rect.width, 64);
        assert_eq!(rect.height, 12);
    }

    #[test]
    fn float_manual_placement() {
        let mut ui = make_ui();
        let win = open_stub_float(
            &mut ui,
            FloatConfig {
                placement: Placement::Manual {
                    anchor: Corner::NW,
                    row: 4,
                    col: 10,
                    width: Constraint::Length(60),
                    height: Constraint::Length(16),
                },
                ..Default::default()
            },
        );
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect, Rect::new(4, 10, 60, 16));
    }

    #[test]
    fn float_se_anchor() {
        let mut ui = make_ui();
        let win = open_stub_float(
            &mut ui,
            FloatConfig {
                placement: Placement::Manual {
                    anchor: Corner::SE,
                    row: 24,
                    col: 80,
                    width: Constraint::Length(40),
                    height: Constraint::Length(10),
                },
                ..Default::default()
            },
        );
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect, Rect::new(14, 40, 40, 10));
    }

    #[test]
    fn float_clamped_to_terminal() {
        let mut ui = make_ui();
        let win = open_stub_float(
            &mut ui,
            FloatConfig {
                placement: Placement::Manual {
                    anchor: Corner::NW,
                    row: 20,
                    col: 70,
                    width: Constraint::Length(30),
                    height: Constraint::Length(10),
                },
                ..Default::default()
            },
        );
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect.width, 10);
        assert_eq!(rect.height, 4);
    }

    #[test]
    fn dock_bottom_full_width() {
        let mut ui = make_ui();
        let win = open_stub_float(
            &mut ui,
            FloatConfig {
                placement: Placement::dock_bottom_full_width(Constraint::Length(6)),
                ..Default::default()
            },
        );
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect.width, 80);
        // Fits content: stub picker has 1 item → natural_height = 1,
        // capped (but not raised) by the 6-row max.
        assert_eq!(rect.height, 1);
        assert_eq!(rect.left, 0);
        // above_rows=1 by default → top = 24 - 1 - 1 = 22
        assert_eq!(rect.top, 22);
    }

    #[test]
    fn floats_z_ordered_returns_sorted() {
        let mut ui = make_ui();
        let w1 = open_stub_float(
            &mut ui,
            FloatConfig {
                zindex: 100,
                ..Default::default()
            },
        );
        let w2 = open_stub_float(
            &mut ui,
            FloatConfig {
                zindex: 10,
                ..Default::default()
            },
        );
        let w3 = open_stub_float(
            &mut ui,
            FloatConfig {
                zindex: 50,
                ..Default::default()
            },
        );
        let ordered = ui.floats_z_ordered();
        assert_eq!(ordered, vec![w2, w3, w1]);
    }

    #[test]
    fn resolve_float_rects_matches_z_order() {
        let mut ui = make_ui();
        let w1 = open_stub_float(
            &mut ui,
            FloatConfig {
                zindex: 100,
                ..Default::default()
            },
        );
        let w2 = open_stub_float(
            &mut ui,
            FloatConfig {
                zindex: 10,
                ..Default::default()
            },
        );
        let rects = ui.resolve_float_rects();
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].0, w2);
        assert_eq!(rects[1].0, w1);
    }

    /// Dismiss dispatch chain regression: dialog widget on Esc /
    /// Ctrl-C → `WidgetEvent::Dismiss` → `WinEvent::Dismiss` →
    /// registered callback fires via `lua_invoke`. Guards against
    /// silent breakage of the path Lua dialogs use to know when the
    /// user backed out.
    #[test]
    fn focused_dialog_esc_invokes_dismiss_callback() {
        use crate::dialog::{DialogConfig, PanelHeight, PanelSpec};
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts {
            buftype: buffer::BufType::Scratch,
            modifiable: false,
        });
        let win = ui
            .dialog_open(
                FloatConfig {
                    focusable: true,
                    placement: Placement::DockBottom {
                        above_rows: 0,
                        full_width: true,
                        max_width: Constraint::Percentage(100),
                        max_height: Constraint::Percentage(100),
                    },
                    ..Default::default()
                },
                DialogConfig::default(),
                vec![PanelSpec::content(buf, PanelHeight::Fit)],
            )
            .unwrap();

        let invoked = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let invoked_cb = invoked.clone();
        ui.win_on_event(
            win,
            WinEvent::Dismiss,
            Callback::Rust(Box::new(move |_| {
                *invoked_cb.lock().unwrap() += 1;
                CallbackResult::Consumed
            })),
        );

        let _ = ui.handle_key_with_lua(KeyCode::Esc, KeyModifiers::NONE, &mut |_, _, _, _| {});
        assert_eq!(
            *invoked.lock().unwrap(),
            1,
            "Esc should invoke the registered Dismiss callback exactly once"
        );

        // Reopen and confirm Ctrl-C does the same.
        let buf2 = ui.buf_create(buffer::BufCreateOpts {
            buftype: buffer::BufType::Scratch,
            modifiable: false,
        });
        let win2 = ui
            .dialog_open(
                FloatConfig {
                    focusable: true,
                    placement: Placement::DockBottom {
                        above_rows: 0,
                        full_width: true,
                        max_width: Constraint::Percentage(100),
                        max_height: Constraint::Percentage(100),
                    },
                    ..Default::default()
                },
                DialogConfig::default(),
                vec![PanelSpec::content(buf2, PanelHeight::Fit)],
            )
            .unwrap();
        let invoked2 = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let invoked2_cb = invoked2.clone();
        ui.win_on_event(
            win2,
            WinEvent::Dismiss,
            Callback::Rust(Box::new(move |_| {
                *invoked2_cb.lock().unwrap() += 1;
                CallbackResult::Consumed
            })),
        );
        let _ = ui.handle_key_with_lua(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &mut |_, _, _, _| {},
        );
        assert_eq!(*invoked2.lock().unwrap(), 1, "Ctrl-C should fire Dismiss");
    }

    // ── Overlay API (P1.c) ───────────────────────────────────────────

    fn stub_overlay() -> Overlay {
        let layout =
            LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(WinId(99)))]);
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
            layout::Anchor::ScreenAt { row: 5, col: 10, .. }
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
            LayoutTree::vbox(vec![(Constraint::Length(height), LayoutTree::leaf(WinId(99)))]),
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
            },
        ));
        assert!(ui.resolve_overlays(None).is_empty());
        // Once the target's rect is published, it resolves.
        ui.set_window_rect(WinId(999), Rect::new(5, 10, 30, 8));
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
        // wins.get(...) needs a real Window — open a split with a real buf.
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: layout::Gutters::default(),
                },
            )
            .expect("split opens");
        ui.register_split("a", win);
        ui.set_focus(win);
        assert_eq!(ui.focused_window().map(|w| w.id), Some(win));
    }

    #[test]
    fn overlay_close_with_focus_inside_pops_to_prior() {
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
        let outside = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: layout::Gutters::default(),
                },
            )
            .expect("split opens");
        ui.register_split("outside", outside);
        // Inside-the-overlay leaf id (stub_overlay uses WinId(99)).
        let inside = WinId(99);
        ui.register_split("inside", inside);
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
        ui.register_split("outside", outside);
        let id = ui.overlay_open(stub_overlay());
        ui.set_focus(outside);
        ui.overlay_close(id);
        assert_eq!(ui.focus(), Some(outside));
    }

    #[test]
    fn overlay_close_with_exhausted_history_clears_focus() {
        let mut ui = make_ui();
        let inside = WinId(99); // stub_overlay's leaf
        ui.register_split("inside", inside);
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
        ui.register_split("dlg-leaf", win);
        let id = ui.overlay_open(stub_overlay()); // stub uses Leaf(WinId(99))
        ui.set_focus(win);
        assert_eq!(ui.focused_overlay(), Some(id));
    }

    #[test]
    fn focused_overlay_returns_none_when_focus_on_unrelated_split() {
        let mut ui = make_ui();
        let other = WinId(50);
        ui.register_split("split", other);
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
        ui.register_split("transcript", win);
        assert!(ui.set_focus(win));
        assert_eq!(ui.focus(), Some(win));
        assert!(ui.focus_history().is_empty());
    }

    #[test]
    fn set_focus_pushes_prior_focus_to_history() {
        let mut ui = make_ui();
        let a = WinId(7);
        let b = WinId(8);
        ui.register_split("a", a);
        ui.register_split("b", b);
        ui.set_focus(a);
        ui.set_focus(b);
        assert_eq!(ui.focus(), Some(b));
        assert_eq!(ui.focus_history(), &[a]);
    }

    #[test]
    fn set_focus_same_win_is_noop() {
        let mut ui = make_ui();
        let win = WinId(7);
        ui.register_split("a", win);
        ui.set_focus(win);
        assert!(ui.set_focus(win));
        assert!(ui.focus_history().is_empty());
    }

    #[test]
    fn set_focus_chain_builds_history_in_order() {
        let mut ui = make_ui();
        for n in 1..=4 {
            ui.register_split(format!("split:{n}"), WinId(n));
        }
        ui.set_focus(WinId(1));
        ui.set_focus(WinId(2));
        ui.set_focus(WinId(3));
        ui.set_focus(WinId(4));
        assert_eq!(ui.focus(), Some(WinId(4)));
        assert_eq!(
            ui.focus_history(),
            &[WinId(1), WinId(2), WinId(3)]
        );
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
        let _under = ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter).with_z(10));
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
        ui.register_split("a", win);
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
        for (id, w) in [("a", a), ("b", b), ("c", c)] {
            ui.register_split(id, w);
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
        for (id, w) in [("a", a), ("b", b), ("c", c)] {
            ui.register_split(id, w);
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
        ui.register_split("a", a);
        ui.register_split("c", c);
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
        let id = ui.overlay_open(
            Overlay::new(
                LayoutTree::vbox(vec![(
                    Constraint::Length(8),
                    LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(WinId(99)))]),
                )])
                .with_border(layout::Border::Single),
                layout::Anchor::ScreenCenter,
            ),
        );
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
        let result = ui.handle_key(
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, KeyResult::Consumed);
        assert!(ui.overlay(oid).is_none());
    }

    #[test]
    fn handle_key_esc_closes_active_modal() {
        let mut ui = make_ui();
        let id = ui.overlay_open(modal_overlay_with_leaves(WinId(50), WinId(51), WinId(52)));
        assert_eq!(ui.active_modal(), Some(id));
        let result = ui.handle_key(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, KeyResult::Consumed);
        assert_eq!(ui.active_modal(), None);
    }

    #[test]
    fn handle_key_esc_with_modifiers_does_not_dismiss_modal() {
        let mut ui = make_ui();
        let id = ui.overlay_open(modal_overlay_with_leaves(WinId(50), WinId(51), WinId(52)));
        // Esc + Shift falls through to normal dispatch — built-in
        // dismiss is bare Esc only.
        let _ = ui.handle_key(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::SHIFT,
        );
        assert_eq!(ui.active_modal(), Some(id));
    }

    #[test]
    fn modal_esc_fires_dismiss_on_every_leaf_before_closing() {
        // Multi-panel overlay: Lua dialogs register `on_event("dismiss",
        // …)` per-leaf to flush input state before the overlay
        // disappears. Esc must walk every leaf so a multi-input dialog
        // doesn't lose state on the unfocused panels.
        let mut ui = make_ui();
        let a = WinId(60);
        let b = WinId(61);
        let c = WinId(62);
        let id = ui.overlay_open(modal_overlay_with_leaves(a, b, c));
        let count = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        for leaf in [a, b, c] {
            let count_cb = count.clone();
            ui.win_on_event(
                leaf,
                WinEvent::Dismiss,
                Callback::Rust(Box::new(move |_| {
                    *count_cb.lock().unwrap() += 1;
                    CallbackResult::Consumed
                })),
            );
        }
        let result = ui.handle_key(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, KeyResult::Consumed);
        assert_eq!(*count.lock().unwrap(), 3);
        assert!(ui.overlay(id).is_none());
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
}
