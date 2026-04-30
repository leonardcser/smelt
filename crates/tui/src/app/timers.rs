//! `Timers` — scheduled Lua callbacks (one-shot + recurring).
//!
//! Each entry holds a stable id, a deadline, an optional period (None
//! = one-shot, Some = re-arm with that interval after firing), and a
//! `LuaHandle` that owns the callback registry slot. Bindings call
//! [`Timers::set`] / [`Timers::every`] / [`Timers::cancel`] through the
//! TLS app pointer; the main loop drains due entries each iteration
//! via `App::tick_timers`.
//!
//! The drain pass walks entries with `retain_mut`: due one-shots are
//! removed, due periodics are re-armed in place, and the LuaHandles for
//! due entries are pulled out as `mlua::Function` clones so the
//! callbacks fire *after* the borrow on `Timers` releases. That lets a
//! callback re-enter `with_app(|app| app.core.timers.set(...))` without a
//! re-entrant borrow.

use std::time::{Duration, Instant};

use mlua::Lua;

use crate::lua::LuaHandle;

/// Stable handle returned by `set` / `every`. `cancel` consumes one to
/// drop the underlying registry slot.
pub type TimerId = u64;

struct TimerEntry {
    id: TimerId,
    deadline: Instant,
    /// `None` = one-shot (removed on fire); `Some(p)` = recurring,
    /// re-armed with `now + p` after each fire.
    period: Option<Duration>,
    handle: LuaHandle,
}

/// Scheduler for Lua-callback timers. Storage is a `Vec` because timer
/// counts stay small and order doesn't matter — fire order is by
/// deadline, picked at drain time.
#[derive(Default)]
pub struct Timers {
    entries: Vec<TimerEntry>,
    next_id: TimerId,
}

impl Timers {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
        }
    }

    /// Schedule a one-shot callback to fire after `delay`. Returns the
    /// id `cancel` accepts.
    pub fn set(&mut self, delay: Duration, handle: LuaHandle) -> TimerId {
        self.push(delay, None, handle)
    }

    /// Schedule a recurring callback to fire every `period`, starting
    /// `period` from now. Returns the id `cancel` accepts.
    pub fn every(&mut self, period: Duration, handle: LuaHandle) -> TimerId {
        self.push(period, Some(period), handle)
    }

    fn push(&mut self, delay: Duration, period: Option<Duration>, handle: LuaHandle) -> TimerId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.entries.push(TimerEntry {
            id,
            deadline: Instant::now() + delay,
            period,
            handle,
        });
        id
    }

    /// Cancel the timer with `id`. Returns `true` if a timer was
    /// removed; `false` if `id` was unknown (already fired or never
    /// existed).
    pub fn cancel(&mut self, id: TimerId) -> bool {
        let Some(idx) = self.entries.iter().position(|e| e.id == id) else {
            return false;
        };
        self.entries.swap_remove(idx);
        true
    }

    /// Walk entries: collect due callbacks, re-arm periodics in place,
    /// drop one-shots. Returns the functions in walk order so the
    /// caller can fire them after the borrow on `self` releases. A
    /// callback that re-enters `Timers::set` / `every` / `cancel` is
    /// safe — those calls land on a fresh `&mut Timers` taken via
    /// `with_app`.
    pub fn drain_due(&mut self, now: Instant, lua: &Lua) -> Vec<mlua::Function> {
        let mut due = Vec::new();
        self.entries.retain_mut(|e| {
            if e.deadline > now {
                return true;
            }
            if let Ok(func) = lua.registry_value::<mlua::Function>(&e.handle.key) {
                due.push(func);
            }
            if let Some(period) = e.period {
                e.deadline = now + period;
                true
            } else {
                false
            }
        });
        due
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn handle(lua: &Lua, src: &str) -> LuaHandle {
        let func: mlua::Function = lua.load(src).eval().expect("load");
        let key = lua.create_registry_value(func).expect("registry");
        LuaHandle { key }
    }

    #[test]
    fn one_shot_fires_then_drops() {
        let lua = Lua::new();
        let counter = lua.create_table().unwrap();
        counter.set("n", 0i64).unwrap();
        lua.globals().set("c", counter).unwrap();
        let h = handle(&lua, "function() c.n = c.n + 1 end");
        let mut t = Timers::new();
        t.set(Duration::from_millis(0), h);
        assert_eq!(t.len(), 1);
        std::thread::sleep(Duration::from_millis(2));
        let due = t.drain_due(Instant::now(), &lua);
        assert_eq!(due.len(), 1);
        for f in due {
            f.call::<()>(()).unwrap();
        }
        assert_eq!(t.len(), 0);
        let n: i64 = lua
            .globals()
            .get::<mlua::Table>("c")
            .unwrap()
            .get("n")
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn recurring_re_arms_and_fires_again() {
        let lua = Lua::new();
        let h = handle(&lua, "function() end");
        let mut t = Timers::new();
        let id = t.every(Duration::from_millis(0), h);
        std::thread::sleep(Duration::from_millis(2));
        let due = t.drain_due(Instant::now(), &lua);
        assert_eq!(due.len(), 1);
        // Still in the queue, deadline pushed forward.
        assert_eq!(t.len(), 1);
        assert!(t.cancel(id));
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn cancel_returns_false_for_unknown_id() {
        let mut t = Timers::new();
        assert!(!t.cancel(42));
    }
}
