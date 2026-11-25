//! Per-window keymap and event callback registry. Also holds the keymap scope
//! frames of windows, modals and overlays: a scope name plus the actions that
//! its bindings run.
use super::{ModalId, OverlayId, RowIndex, WinId};
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
        // Uppercase text already encodes Shift; terminals may also report it.
        let mods =
            if code == KeyCode::BackTab || matches!(code, KeyCode::Char(c) if c.is_uppercase()) {
                mods - KeyModifiers::SHIFT
            } else {
                mods
            };
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
    /// Mouse-down landed inside this leaf. Payload carries leaf-relative `x`/`y`
    /// and the button. Fires before focus promotion; non-focusable leaves still receive it.
    Press,
    /// Mouse-up after a `Press` on this leaf. Fires on the leaf that owned the
    /// press, even if the pointer drifted out (capture). Same payload as `Press`.
    Release,
    /// Mouse motion while a button is held after a `Press` on this leaf. Same
    /// payload as `Press`; `x`/`y` are leaf-relative for the new position.
    Drag,
    /// The pointer moved onto this leaf with no button held. Same payload as
    /// `Press`, without a button.
    PointerEnter,
    /// The pointer left this leaf with no button held. Coordinates are relative
    /// to this leaf and may lie outside it.
    PointerLeave,
    /// The pointer moved within this leaf with no button held.
    PointerMove,
    /// Window's scroll state changed. Payload carries the new `top` and `follow` flag.
    Scrolled,
    /// Leaf's viewport rect changed (first paint, terminal resize, layout
    /// reflow). Payload carries the new `{ x, y, width, height }` rect
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
    Paste {
        content: String,
    },
    Selection {
        index: usize,
    },
    Text {
        content: String,
    },
    /// Mouse event on a leaf. `x`/`y` are leaf-relative cell coordinates and
    /// may be negative for `PointerLeave` or a title press. `button` is `None`
    /// for pointer motion. `title` is the id of the pressed border-title span.
    Mouse {
        x: i32,
        y: i32,
        button: Option<MouseButton>,
        title: Option<std::sync::Arc<str>>,
    },
    /// Scroll state changed. `top` is the new `scroll_top`; `follow` is tail mode.
    Scroll {
        top: RowIndex,
        follow: bool,
    },
    /// Resize payload. `x`/`y`/`width`/`height` describe the new outer
    /// rect (matches `win:rect()`); `content_width` is the inner cell
    /// budget after gutter and pad subtraction (matches `win:content_width()`).
    Rect {
        x: u16,
        y: u16,
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

/// A container that can carry a keymap scope.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ScopeOwner {
    Win(WinId),
    Modal(ModalId),
    Overlay(OverlayId),
}

/// The keymap scope a container belongs to and the Lua callbacks that
/// implement its actions.
#[derive(Debug)]
pub(crate) struct ScopeFrame {
    pub(crate) name: String,
    actions: HashMap<String, LuaHandle>,
}

impl ScopeFrame {
    fn lua_ids(self) -> impl Iterator<Item = u64> {
        self.actions.into_values().map(|handle| handle.0)
    }
}

/// Per-window callback registry owned by `Ui`.
#[derive(Default)]
pub(crate) struct Callbacks {
    keymaps: HashMap<WinId, HashMap<KeyBind, Callback>>,
    events: HashMap<WinId, HashMap<WinEvent, Vec<Callback>>>,
    /// Per-window fallback key handler tried after specific keymaps miss.
    key_fallback: HashMap<WinId, Callback>,
    /// Keymap scope frames of windows, modals and overlays.
    scopes: HashMap<ScopeOwner, ScopeFrame>,
    /// Native keymap scopes of windows, whose actions the host implements.
    /// They sit under the window's own scope frame.
    base_scopes: HashMap<WinId, &'static str>,
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

    /// Remove the (win, key) keymap only while it is still Lua callback `id`.
    pub(crate) fn clear_keymap(&mut self, win: WinId, key: KeyBind, id: u64) -> Option<Callback> {
        let table = self.keymaps.get_mut(&win)?;
        if table.get(&key)?.lua_id() != Some(id) {
            return None;
        }
        table.remove(&key)
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

    /// Whether any leaf has a callback for `ev`.
    pub(crate) fn listens_for(&self, ev: WinEvent) -> bool {
        self.events.values().any(|events| {
            events
                .get(&ev)
                .is_some_and(|callbacks| !callbacks.is_empty())
        })
    }

    pub(crate) fn has_event(&self, win: WinId, ev: WinEvent) -> bool {
        self.events
            .get(&win)
            .and_then(|events| events.get(&ev))
            .is_some_and(|callbacks| !callbacks.is_empty())
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
        lua_ids.extend(self.clear_scope(ScopeOwner::Win(win)));
        self.base_scopes.remove(&win);
        lua_ids
    }

    /// Move the Rust-owned callbacks out of this registry, dropping every Lua
    /// callback with the retired generation. Candidate registries start with no
    /// callbacks and merge these back only after a successful commit.
    pub(crate) fn take_rust_callbacks(&mut self) -> Self {
        let mut callbacks = std::mem::take(self);
        callbacks.keymaps.retain(|_, table| {
            table.retain(|_, callback| matches!(callback, Callback::Rust(_)));
            !table.is_empty()
        });
        callbacks.events.retain(|_, events| {
            events.retain(|_, callbacks| {
                callbacks.retain(|callback| matches!(callback, Callback::Rust(_)));
                !callbacks.is_empty()
            });
            !events.is_empty()
        });
        callbacks
            .key_fallback
            .retain(|_, callback| matches!(callback, Callback::Rust(_)));
        // Scope frames hold Lua callbacks only; native scopes stay.
        callbacks.scopes.clear();
        callbacks
    }

    /// Merge callbacks owned by the Rust UI into a committed candidate.
    /// Candidate keymaps win exact conflicts, matching registration against a
    /// live UI, while Rust event callbacks retain their original first position.
    pub(crate) fn merge_rust_callbacks(&mut self, callbacks: Self) {
        for (win, table) in callbacks.keymaps {
            let target = self.keymaps.entry(win).or_default();
            for (key, callback) in table {
                target.entry(key).or_insert(callback);
            }
        }
        for (win, events) in callbacks.events {
            let target_events = self.events.entry(win).or_default();
            for (event, mut callbacks) in events {
                callbacks.extend(target_events.remove(&event).unwrap_or_default());
                target_events.insert(event, callbacks);
            }
        }
        for (win, callback) in callbacks.key_fallback {
            self.key_fallback.entry(win).or_insert(callback);
        }
        for (win, scope) in callbacks.base_scopes {
            self.base_scopes.entry(win).or_insert(scope);
        }
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

    // ── Keymap scopes ────────────────────────────────────────────────

    /// Put `owner` in scope `name` with `actions`, replacing its frame.
    /// Returns the Lua handle ids of the replaced actions.
    #[must_use]
    pub(crate) fn set_scope(
        &mut self,
        owner: ScopeOwner,
        name: String,
        actions: HashMap<String, LuaHandle>,
    ) -> Vec<u64> {
        self.scopes
            .insert(owner, ScopeFrame { name, actions })
            .into_iter()
            .flat_map(ScopeFrame::lua_ids)
            .collect()
    }

    /// Remove the scope frame of `owner`. Returns Lua handle ids for cleanup.
    #[must_use]
    pub(crate) fn clear_scope(&mut self, owner: ScopeOwner) -> Vec<u64> {
        self.scopes
            .remove(&owner)
            .into_iter()
            .flat_map(ScopeFrame::lua_ids)
            .collect()
    }

    pub(crate) fn scope(&self, owner: ScopeOwner) -> Option<&ScopeFrame> {
        self.scopes.get(&owner)
    }

    pub(crate) fn action(&self, owner: ScopeOwner, name: &str) -> Option<LuaHandle> {
        self.scopes.get(&owner)?.actions.get(name).copied()
    }

    /// Put `win` in native scope `name`, or out of its native scope.
    pub(crate) fn set_base_scope(&mut self, win: WinId, name: Option<&'static str>) {
        match name {
            Some(name) => self.base_scopes.insert(win, name),
            None => self.base_scopes.remove(&win),
        };
    }

    pub(crate) fn base_scope(&self, win: WinId) -> Option<&'static str> {
        self.base_scopes.get(&win).copied()
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
    fn clearing_a_replaced_keymap_keeps_the_newer_binding() {
        let mut cbs = Callbacks::new();
        let key = KeyBind::char('q');
        let _ = cbs.set_keymap(wid(1), key, Callback::Lua(LuaHandle(1)));
        let _ = cbs.set_keymap(wid(1), key, Callback::Lua(LuaHandle(2)));
        assert!(cbs.clear_keymap(wid(1), key, 1).is_none());
        assert!(matches!(
            cbs.clear_keymap(wid(1), key, 2),
            Some(Callback::Lua(LuaHandle(2)))
        ));
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

    fn actions(pairs: &[(&str, u64)]) -> HashMap<String, LuaHandle> {
        pairs
            .iter()
            .map(|(name, id)| (name.to_string(), LuaHandle(*id)))
            .collect()
    }

    #[test]
    fn scope_frames_replace_and_release_their_actions() {
        let mut cbs = Callbacks::new();
        let overlay = ScopeOwner::Overlay(OverlayId(3));
        let modal = ScopeOwner::Modal(ModalId(3));
        assert!(cbs
            .set_scope(overlay, "diff".into(), actions(&[("stage", 1)]))
            .is_empty());
        let _ = cbs.set_scope(modal, "resume".into(), actions(&[("stage", 2)]));
        assert_eq!(cbs.scope(overlay).unwrap().name, "diff");
        assert_eq!(cbs.action(overlay, "stage").map(|h| h.0), Some(1));
        assert!(cbs.action(overlay, "push").is_none());

        assert_eq!(
            cbs.set_scope(overlay, "diff".into(), actions(&[("push", 3)])),
            vec![1]
        );
        assert_eq!(cbs.clear_scope(modal), vec![2]);
        assert!(cbs.scope(modal).is_none());
        assert_eq!(cbs.clear_all(WinId(9)), Vec::<u64>::new());
    }

    #[test]
    fn backtab_keybind_ignores_shift_modifier() {
        assert_eq!(
            KeyBind::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyBind::new(KeyCode::BackTab, KeyModifiers::NONE)
        );
        assert_eq!(
            KeyBind::new(KeyCode::BackTab, KeyModifiers::SHIFT | KeyModifiers::ALT),
            KeyBind::new(KeyCode::BackTab, KeyModifiers::ALT)
        );
    }

    #[test]
    fn uppercase_keybinds_ignore_redundant_shift() {
        for ch in ['G', 'H', 'L', 'É', 'Ω'] {
            for mods in [KeyModifiers::NONE, KeyModifiers::CONTROL, KeyModifiers::ALT] {
                assert_eq!(
                    KeyBind::new(KeyCode::Char(ch), mods | KeyModifiers::SHIFT),
                    KeyBind::new(KeyCode::Char(ch), mods)
                );
            }
        }
        for code in [KeyCode::Left, KeyCode::Char('g'), KeyCode::Char(' ')] {
            assert_ne!(
                KeyBind::new(code, KeyModifiers::SHIFT),
                KeyBind::new(code, KeyModifiers::NONE)
            );
        }
    }
}
