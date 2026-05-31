//! Per-window keymap and event callback registry. Also hosts overlay-scoped
//! keymaps: bindings that fire when any leaf of the overlay holds focus,
//! letting an overlay own its key handling without each leaf re-registering.
use super::{OverlayId, WinId};
use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::HashMap;

/// A keyboard chord (`Hash + Eq` for registry keying).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyBind {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl KeyBind {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        Self { code, mods }
    }

    #[cfg(test)]
    pub(crate) fn plain(code: KeyCode) -> Self {
        Self {
            code,
            mods: KeyModifiers::NONE,
        }
    }

    #[cfg(test)]
    pub(crate) fn char(c: char) -> Self {
        Self::plain(KeyCode::Char(c))
    }

    #[cfg(test)]
    pub(crate) fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            mods: KeyModifiers::CONTROL,
        }
    }
}

/// Window lifecycle / semantic events.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WinEvent {
    Open,
    Close,
    FocusGained,
    FocusLost,
    SelectionChanged,
    /// Enter pressed on a List or Input; payload carries `index` / `text`.
    Submit,
    /// Input buffer edited; payload carries the new text.
    TextChanged,
    Dismiss,
    /// Fired once per event-loop iteration for live-refresh overlays.
    Tick,
    /// Mouse-down landed inside this leaf. Payload carries leaf-relative `row`/`col`
    /// and the button. Fires before focus promotion; non-focusable leaves still receive it.
    Press,
    /// Mouse-up after a `Press` on this leaf. Fires on the leaf that owned the
    /// press, even if the pointer drifted out (capture). Same payload as `Press`.
    Release,
    /// Mouse motion while a button is held after a `Press` on this leaf. Same
    /// payload as `Press`; `row`/`col` are leaf-relative for the new position.
    Drag,
    /// Window's scroll state changed. Payload carries the new `top` and `follow` flag.
    Scrolled,
    /// Leaf's viewport rect changed (first paint, terminal resize, layout
    /// reflow). Payload carries the new `{ row, col, width, height }` rect
    /// and the inner `content_width` in `Payload::Rect`.
    Resized,
    /// User accepted a placeholder suggestion via an `accept_keys` chord.
    /// Payload carries the accepted text in `Payload::Text`.
    PlaceholderAccepted,
    /// User dismissed the placeholder via a `dismiss_keys` chord.
    /// Payload carries the dismissed text in `Payload::Text`.
    PlaceholderDismissed,
}

/// Mouse button identity carried in `Payload::Mouse`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Payload attached to a callback invocation.
#[derive(Clone, Debug)]
pub enum Payload {
    None,
    Key {
        code: KeyCode,
        mods: KeyModifiers,
    },
    Selection {
        index: usize,
    },
    Text {
        content: String,
    },
    /// Mouse event hit a leaf. `row`/`col` are leaf-relative cell coordinates.
    /// Used for `Press`, `Release`, and `Drag`.
    Mouse {
        row: u16,
        col: u16,
        button: MouseButton,
    },
    /// Scroll state changed. `top` is the new `scroll_top`; `follow` is `follow_tail`.
    Scroll {
        top: u16,
        follow: bool,
    },
    /// Resize payload. `row`/`col`/`width`/`height` describe the new outer
    /// rect (matches `win:rect()`); `content_width` is the inner cell
    /// budget after gutter and pad subtraction (matches `win:content_width()`).
    Rect {
        row: u16,
        col: u16,
        width: u16,
        height: u16,
        content_width: u16,
    },
}

/// Result returned by a callback.
#[derive(Clone, Debug)]
pub enum CallbackResult {
    Consumed,
    Pass,
    /// Consumed, and fire a follow-up `WinEvent` on the same window.
    Event(WinEvent, Payload),
}

/// Opaque handle to a Lua callback; the Lua runtime owns the actual function.
#[derive(Clone, Copy, Debug)]
pub struct LuaHandle(pub u64);

/// Rust-side callback closure.
pub(crate) type RustCallback = Box<dyn FnMut(&mut CallbackCtx<'_>) -> CallbackResult>;

/// A callback: either a Rust closure or a Lua handle.
pub enum Callback {
    Rust(RustCallback),
    Lua(LuaHandle),
}

impl std::fmt::Debug for Callback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Callback::Rust(_) => f.write_str("Callback::Rust(<closure>)"),
            Callback::Lua(h) => write!(f, "Callback::Lua({})", h.0),
        }
    }
}

impl Callback {
    fn lua_id(&self) -> Option<u64> {
        match self {
            Callback::Lua(LuaHandle(id)) => Some(*id),
            Callback::Rust(_) => None,
        }
    }
}

/// Context passed to Rust callbacks. Provides full `&mut Ui` access.
pub struct CallbackCtx<'a> {
    pub ui: &'a mut super::Ui,
    pub win: WinId,
    pub payload: Payload,
}

/// Per-window callback registry owned by `Ui`.
#[derive(Default)]
pub(crate) struct Callbacks {
    keymaps: HashMap<WinId, HashMap<KeyBind, Callback>>,
    events: HashMap<WinId, HashMap<WinEvent, Vec<Callback>>>,
    /// Per-window fallback key handler tried after specific keymaps miss.
    key_fallback: HashMap<WinId, Callback>,
    /// Per-overlay keymaps. Fire when any leaf of the overlay holds focus,
    /// after a per-window keymap miss but before global Lua keymaps.
    overlay_keymaps: HashMap<OverlayId, HashMap<KeyBind, Callback>>,
}

impl Callbacks {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Install a (win, key) keymap. Returns the displaced `Callback`, if any.
    #[must_use]
    pub(crate) fn set_keymap(
        &mut self,
        win: WinId,
        key: KeyBind,
        cb: Callback,
    ) -> Option<Callback> {
        self.keymaps.entry(win).or_default().insert(key, cb)
    }

    pub(crate) fn clear_keymap(&mut self, win: WinId, key: KeyBind) -> Option<Callback> {
        self.keymaps.get_mut(&win).and_then(|t| t.remove(&key))
    }

    /// Remove a specific event callback by Lua handle id.
    pub(crate) fn clear_event_by_id(
        &mut self,
        win: WinId,
        ev: WinEvent,
        id: u64,
    ) -> Option<Callback> {
        let list = self.events.get_mut(&win)?.get_mut(&ev)?;
        let pos = list
            .iter()
            .position(|cb| matches!(cb, Callback::Lua(LuaHandle(h)) if *h == id))?;
        Some(list.remove(pos))
    }

    pub(crate) fn on_event(&mut self, win: WinId, ev: WinEvent, cb: Callback) {
        self.events
            .entry(win)
            .or_default()
            .entry(ev)
            .or_default()
            .push(cb);
    }

    /// Remove every binding for `win`. Returns Lua handle IDs for caller cleanup.
    #[must_use]
    pub(crate) fn clear_all(&mut self, win: WinId) -> Vec<u64> {
        let mut lua_ids = Vec::new();
        if let Some(table) = self.keymaps.remove(&win) {
            lua_ids.extend(table.into_values().filter_map(|cb| cb.lua_id()));
        }
        if let Some(events) = self.events.remove(&win) {
            for cbs in events.into_values() {
                lua_ids.extend(cbs.into_iter().filter_map(|cb| cb.lua_id()));
            }
        }
        if let Some(cb) = self.key_fallback.remove(&win) {
            if let Some(id) = cb.lua_id() {
                lua_ids.push(id);
            }
        }
        lua_ids
    }

    fn retain_non_lua(cb: &Callback, lua_ids: &mut Vec<u64>) -> bool {
        if let Some(id) = cb.lua_id() {
            lua_ids.push(id);
            false
        } else {
            true
        }
    }

    /// Remove every Lua callback across window and overlay registries. Rust
    /// callbacks are preserved. Returns Lua handle ids for caller cleanup.
    #[must_use]
    pub(crate) fn clear_lua_callbacks(&mut self) -> Vec<u64> {
        let mut lua_ids = Vec::new();

        self.keymaps.retain(|_, table| {
            table.retain(|_, cb| Self::retain_non_lua(cb, &mut lua_ids));
            !table.is_empty()
        });

        self.events.retain(|_, events| {
            events.retain(|_, callbacks| {
                callbacks.retain(|cb| Self::retain_non_lua(cb, &mut lua_ids));
                !callbacks.is_empty()
            });
            !events.is_empty()
        });

        self.key_fallback
            .retain(|_, cb| Self::retain_non_lua(cb, &mut lua_ids));

        self.overlay_keymaps.retain(|_, table| {
            table.retain(|_, cb| Self::retain_non_lua(cb, &mut lua_ids));
            !table.is_empty()
        });

        lua_ids
    }

    /// Register a per-window fallback key handler. Returns the displaced `Callback`, if any.
    #[must_use]
    pub(crate) fn set_key_fallback(&mut self, win: WinId, cb: Callback) -> Option<Callback> {
        self.key_fallback.insert(win, cb)
    }

    pub(crate) fn take_key_fallback(&mut self, win: WinId) -> Option<Callback> {
        self.key_fallback.remove(&win)
    }

    pub(crate) fn restore_key_fallback(&mut self, win: WinId, cb: Callback) {
        self.key_fallback.insert(win, cb);
    }

    /// List every window with at least one callback registered for `ev`.
    pub(crate) fn wins_with_event(&self, ev: WinEvent) -> Vec<WinId> {
        self.events
            .iter()
            .filter_map(|(win, table)| table.get(&ev).filter(|v| !v.is_empty()).map(|_| *win))
            .collect()
    }

    /// Remove a keymap callback for invocation. Caller must restore it after.
    /// Removal + restore avoids a reentrant-borrow conflict with `&mut Ui`.
    pub(crate) fn take_keymap(&mut self, win: WinId, key: KeyBind) -> Option<Callback> {
        self.keymaps.get_mut(&win)?.remove(&key)
    }

    pub(crate) fn restore_keymap(&mut self, win: WinId, key: KeyBind, cb: Callback) {
        self.keymaps.entry(win).or_default().insert(key, cb);
    }

    // ── Overlay-scoped keymaps ───────────────────────────────────────

    #[must_use]
    pub(crate) fn set_overlay_keymap(
        &mut self,
        overlay: OverlayId,
        key: KeyBind,
        cb: Callback,
    ) -> Option<Callback> {
        self.overlay_keymaps
            .entry(overlay)
            .or_default()
            .insert(key, cb)
    }

    pub(crate) fn clear_overlay_keymap(
        &mut self,
        overlay: OverlayId,
        key: KeyBind,
    ) -> Option<Callback> {
        self.overlay_keymaps
            .get_mut(&overlay)
            .and_then(|t| t.remove(&key))
    }

    pub(crate) fn take_overlay_keymap(
        &mut self,
        overlay: OverlayId,
        key: KeyBind,
    ) -> Option<Callback> {
        self.overlay_keymaps.get_mut(&overlay)?.remove(&key)
    }

    pub(crate) fn restore_overlay_keymap(
        &mut self,
        overlay: OverlayId,
        key: KeyBind,
        cb: Callback,
    ) {
        self.overlay_keymaps
            .entry(overlay)
            .or_default()
            .insert(key, cb);
    }

    /// Remove every overlay-scoped binding. Returns Lua handle ids for caller cleanup.
    #[must_use]
    pub(crate) fn clear_overlay_all(&mut self, overlay: OverlayId) -> Vec<u64> {
        let mut lua_ids = Vec::new();
        if let Some(table) = self.overlay_keymaps.remove(&overlay) {
            for cb in table.into_values() {
                if let Callback::Lua(LuaHandle(id)) = cb {
                    lua_ids.push(id);
                }
            }
        }
        lua_ids
    }

    /// Same take/restore pattern for event callbacks (takes the whole Vec).
    pub(crate) fn take_event(&mut self, win: WinId, ev: WinEvent) -> Option<Vec<Callback>> {
        self.events.get_mut(&win)?.remove(&ev)
    }

    pub(crate) fn restore_event(&mut self, win: WinId, ev: WinEvent, cbs: Vec<Callback>) {
        self.events.entry(win).or_default().insert(ev, cbs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wid(n: u64) -> WinId {
        WinId(n)
    }

    #[test]
    fn set_and_take_keymap() {
        let mut cbs = Callbacks::new();
        let key = KeyBind::plain(KeyCode::Enter);
        let _ = cbs.set_keymap(wid(1), key, Callback::Lua(LuaHandle(42)));
        let taken = cbs.take_keymap(wid(1), key);
        assert!(matches!(taken, Some(Callback::Lua(LuaHandle(42)))));
        assert!(cbs.take_keymap(wid(1), key).is_none());
    }

    #[test]
    fn clear_all_removes_both_tables() {
        let mut cbs = Callbacks::new();
        let _ = cbs.set_keymap(wid(1), KeyBind::char('q'), Callback::Lua(LuaHandle(1)));
        cbs.on_event(wid(1), WinEvent::Submit, Callback::Lua(LuaHandle(2)));
        let _ = cbs.clear_all(wid(1));
        assert!(cbs.take_keymap(wid(1), KeyBind::char('q')).is_none());
        assert!(cbs.take_event(wid(1), WinEvent::Submit).is_none());
    }

    fn oid(n: u32) -> OverlayId {
        OverlayId(n)
    }

    #[test]
    fn set_and_take_overlay_keymap() {
        let mut cbs = Callbacks::new();
        let key = KeyBind::plain(KeyCode::Tab);
        let _ = cbs.set_overlay_keymap(oid(1), key, Callback::Lua(LuaHandle(7)));
        let taken = cbs.take_overlay_keymap(oid(1), key);
        assert!(matches!(taken, Some(Callback::Lua(LuaHandle(7)))));
        assert!(cbs.take_overlay_keymap(oid(1), key).is_none());
    }

    #[test]
    fn clear_overlay_all_returns_lua_ids() {
        let mut cbs = Callbacks::new();
        let _ = cbs.set_overlay_keymap(oid(2), KeyBind::char('q'), Callback::Lua(LuaHandle(11)));
        let _ = cbs.set_overlay_keymap(oid(2), KeyBind::char('w'), Callback::Lua(LuaHandle(12)));
        let mut ids = cbs.clear_overlay_all(oid(2));
        ids.sort();
        assert_eq!(ids, vec![11, 12]);
        assert!(cbs
            .take_overlay_keymap(oid(2), KeyBind::char('q'))
            .is_none());
    }

    #[test]
    fn keybind_constructors() {
        assert_eq!(
            KeyBind::char('w'),
            KeyBind {
                code: KeyCode::Char('w'),
                mods: KeyModifiers::NONE,
            }
        );
        assert_eq!(
            KeyBind::ctrl('a'),
            KeyBind {
                code: KeyCode::Char('a'),
                mods: KeyModifiers::CONTROL,
            }
        );
    }
}
