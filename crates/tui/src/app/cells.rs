//! `Cells` — typed reactive name → value registry with deferred
//! subscriber notification.
//!
//! Each cell is one `Rc<dyn Any>` slot keyed by name plus a list of
//! subscribers. Writes (`set_dyn`) snapshot the new value and the
//! subscriber list into a pending fire queue; the loop drains the
//! queue at a safe point (after the current `&mut Cells` / `&mut App`
//! borrow releases) so subscriber bodies can re-enter `Cells` / other
//! subsystems freely. Same "queue, release the borrow, then fire"
//! pattern Timers and Lua keymap dispatch already use.
//!
//! `Rc<dyn Any>` is intentionally `!Send`-friendly: `Cells` lives on
//! the `!Send` `App` and Lua values (carried as `mlua::RegistryKey`
//! inside `LuaCellValue`) are non-Send too. Snapshots use `Rc::clone`
//! so cell values never need a deep-clone impl — the subscriber sees
//! the value as it stood at the moment of the `set`, even if later
//! writes overwrite the slot before the drain.
//!
//! Today every cell is Lua-defined (`smelt.cell.new`); built-in
//! Rust-typed cells (`vim_mode`, `agent_mode`, …) and Rust-side
//! subscribers migrate in P2.a.4c.

use std::any::Any;
use std::collections::HashMap;
use std::rc::Rc;

use crate::lua::LuaHandle;

/// Cell value wrapper for Lua-originated cells. Stores the value as a
/// stable `mlua::RegistryKey` so it survives Lua GC; Lua-side
/// `smelt.cell.get(name)` resolves the key back to a Lua value, and
/// the drain pump does the same when firing Lua subscribers.
/// Rust-typed built-ins (a.4c) carry their typed Rust value directly
/// and register a per-cell converter the drain pump uses to project
/// them into Lua at fire time.
pub struct LuaCellValue {
    pub key: mlua::RegistryKey,
}

/// Stable id returned by `subscribe_kind` and consumed by
/// `unsubscribe`.
pub type SubscriptionId = u64;

/// What kind of callback to fire when a cell changes. Today only the
/// `Lua` variant ships; P2.a.4c adds `Rust(Rc<dyn Fn(&dyn Any)>)`
/// for built-in subscribers (statusline spec bindings, plugin-tool
/// hook routing, …).
#[derive(Clone)]
pub enum SubscriberKind {
    /// Handle to an `mlua::Function` stashed in the Lua registry. The
    /// drain pump resolves it against the live Lua state at fire time
    /// and projects the cell value through a per-type Lua converter.
    Lua(Rc<LuaHandle>),
}

struct Subscriber {
    id: SubscriptionId,
    kind: SubscriberKind,
}

struct Slot {
    value: Rc<dyn Any>,
    subscribers: Vec<Subscriber>,
}

/// One queued notification: the value snapshot at the moment of `set`
/// plus the subscriber callbacks captured at that moment. The caller
/// fires each callback in registration order after the `&mut Cells`
/// borrow releases.
pub struct PendingFire {
    pub name: String,
    pub value: Rc<dyn Any>,
    pub callbacks: Vec<SubscriberKind>,
}

/// Typed name → value registry plus a pending-fire queue.
#[derive(Default)]
pub struct Cells {
    slots: HashMap<String, Slot>,
    pending: Vec<PendingFire>,
    next_id: SubscriptionId,
}

impl Cells {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a cell with its initial value. Idempotent — calling
    /// twice with the same name resets the value and drops every
    /// subscriber.
    pub fn declare<T: Any + 'static>(&mut self, name: impl Into<String>, initial: T) {
        self.slots.insert(
            name.into(),
            Slot {
                value: Rc::new(initial),
                subscribers: Vec::new(),
            },
        );
    }

    /// Read the current value of `name` as an opaque trait object.
    /// Returns `None` when the cell isn't declared. Callers downcast
    /// via `Rc::downcast` or borrow as `&dyn Any` for the more common
    /// `downcast_ref::<T>()` shape.
    pub fn get_dyn(&self, name: &str) -> Option<&Rc<dyn Any>> {
        self.slots.get(name).map(|s| &s.value)
    }

    /// Overwrite the cell's value and queue every subscriber for
    /// firing at the next drain. Returns `true` on success, `false`
    /// when `name` is undeclared.
    pub fn set_dyn(&mut self, name: &str, value: Rc<dyn Any>) -> bool {
        let Some(slot) = self.slots.get_mut(name) else {
            return false;
        };
        slot.value = value;
        if slot.subscribers.is_empty() {
            return true;
        }
        let snapshot = Rc::clone(&slot.value);
        let callbacks: Vec<SubscriberKind> =
            slot.subscribers.iter().map(|s| s.kind.clone()).collect();
        self.pending.push(PendingFire {
            name: name.to_string(),
            value: snapshot,
            callbacks,
        });
        true
    }

    /// Register a subscriber callback against `name`. Returns the
    /// subscription id `unsubscribe` accepts, or `None` when `name`
    /// isn't declared.
    pub fn subscribe_kind(&mut self, name: &str, kind: SubscriberKind) -> Option<SubscriptionId> {
        let slot = self.slots.get_mut(name)?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        slot.subscribers.push(Subscriber { id, kind });
        Some(id)
    }

    /// Remove the subscriber with `id` from `name`. Returns `true`
    /// if a subscriber was found and removed; `false` otherwise (cell
    /// undeclared or id unknown).
    pub fn unsubscribe(&mut self, name: &str, id: SubscriptionId) -> bool {
        let Some(slot) = self.slots.get_mut(name) else {
            return false;
        };
        let Some(idx) = slot.subscribers.iter().position(|s| s.id == id) else {
            return false;
        };
        slot.subscribers.remove(idx);
        true
    }

    /// Pull every queued fire out of the registry. The caller invokes
    /// each `PendingFire`'s callbacks after the `&mut Cells` borrow
    /// releases. Empty when no `set_dyn` has fired since the last
    /// drain.
    pub fn drain_pending(&mut self) -> Vec<PendingFire> {
        std::mem::take(&mut self.pending)
    }

    /// Cheap probe used by the main-loop drain pump to skip the
    /// drain when nothing's pending.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn handle(lua: &Lua, src: &str) -> Rc<LuaHandle> {
        let func: mlua::Function = lua.load(src).eval().expect("load");
        let key = lua.create_registry_value(func).expect("registry");
        Rc::new(LuaHandle { key })
    }

    #[test]
    fn declare_then_get_dyn_returns_initial_value() {
        let mut c = Cells::new();
        c.declare("count", 7u32);
        let v = c.get_dyn("count").expect("declared");
        assert_eq!(v.downcast_ref::<u32>(), Some(&7u32));
    }

    #[test]
    fn get_dyn_returns_none_for_undeclared() {
        let c = Cells::new();
        assert!(c.get_dyn("missing").is_none());
    }

    #[test]
    fn set_dyn_updates_value() {
        let mut c = Cells::new();
        c.declare("count", 0u32);
        assert!(c.set_dyn("count", Rc::new(42u32)));
        let v = c.get_dyn("count").unwrap();
        assert_eq!(v.downcast_ref::<u32>(), Some(&42u32));
    }

    #[test]
    fn set_dyn_returns_false_for_undeclared() {
        let mut c = Cells::new();
        assert!(!c.set_dyn("missing", Rc::new(1u32)));
    }

    #[test]
    fn set_without_subscribers_does_not_queue() {
        let mut c = Cells::new();
        c.declare("count", 0u32);
        c.set_dyn("count", Rc::new(1u32));
        assert!(!c.has_pending());
        assert_eq!(c.drain_pending().len(), 0);
    }

    #[test]
    fn subscribe_queues_fire_on_set() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 0u32);
        let id = c
            .subscribe_kind(
                "count",
                SubscriberKind::Lua(handle(&lua, "function(v) end")),
            )
            .expect("declared");
        assert!(id == 0); // first subscription is id 0
        c.set_dyn("count", Rc::new(5u32));
        assert!(c.has_pending());
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].name, "count");
        assert_eq!(fires[0].callbacks.len(), 1);
        // Snapshot carries the post-set value.
        assert_eq!(fires[0].value.downcast_ref::<u32>(), Some(&5u32));
    }

    #[test]
    fn multiple_subscribers_appear_in_registration_order() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 0u32);
        for src in [
            "function() return 1 end",
            "function() return 2 end",
            "function() return 3 end",
        ] {
            c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, src)))
                .unwrap();
        }
        c.set_dyn("count", Rc::new(1u32));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].callbacks.len(), 3);
    }

    #[test]
    fn unsubscribe_removes_callback() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 0u32);
        let id = c
            .subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        assert!(c.unsubscribe("count", id));
        c.set_dyn("count", Rc::new(1u32));
        // No subscribers, no pending fire.
        assert!(!c.has_pending());
        // Unsubscribing again is a no-op.
        assert!(!c.unsubscribe("count", id));
    }

    #[test]
    fn snapshot_carries_value_at_set_time() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 0u32);
        c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        c.set_dyn("count", Rc::new(1u32));
        c.set_dyn("count", Rc::new(2u32));
        let fires = c.drain_pending();
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].value.downcast_ref::<u32>(), Some(&1u32));
        assert_eq!(fires[1].value.downcast_ref::<u32>(), Some(&2u32));
    }

    #[test]
    fn drain_pending_is_idempotent() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("count", 0u32);
        c.subscribe_kind("count", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        c.set_dyn("count", Rc::new(1u32));
        assert_eq!(c.drain_pending().len(), 1);
        assert!(c.drain_pending().is_empty());
    }

    #[test]
    fn redeclare_resets_value_and_drops_subscribers() {
        let lua = Lua::new();
        let mut c = Cells::new();
        c.declare("flag", false);
        c.subscribe_kind("flag", SubscriberKind::Lua(handle(&lua, "function() end")))
            .unwrap();
        c.declare("flag", true);
        let v = c.get_dyn("flag").unwrap();
        assert_eq!(v.downcast_ref::<bool>(), Some(&true));
        c.set_dyn("flag", Rc::new(false));
        // Redeclare dropped the prior subscriber.
        assert!(!c.has_pending());
    }

    #[test]
    fn subscribe_returns_none_for_undeclared() {
        let lua = Lua::new();
        let mut c = Cells::new();
        assert!(c
            .subscribe_kind(
                "missing",
                SubscriberKind::Lua(handle(&lua, "function() end"))
            )
            .is_none());
    }

    #[test]
    fn lua_cell_value_round_trip() {
        let lua = Lua::new();
        let value: mlua::Value = lua.load("\"hello\"").eval().unwrap();
        let key = lua.create_registry_value(value).unwrap();
        let mut c = Cells::new();
        c.declare("greeting", LuaCellValue { key });
        let v = c.get_dyn("greeting").unwrap();
        let lv = v.downcast_ref::<LuaCellValue>().unwrap();
        let resolved: String = lua.registry_value(&lv.key).unwrap();
        assert_eq!(resolved, "hello");
    }
}
