//! Scheduled Lua callbacks (one-shot + recurring). `drain_due` returns
//! functions *after* releasing the borrow on `Timers`, so a callback can
//! safely re-enter `Timers::set` / `every` / `cancel`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::clock::Clock;
use mlua::Lua;

use crate::lua::LuaHandle;

pub(crate) type TimerId = u64;

struct TimerEntry {
    id: TimerId,
    deadline: Instant,
    period: Option<Duration>, // None = one-shot; Some(p) = re-arm with now+p
    handle: LuaHandle,
}

/// Vec storage is fine: timer counts stay small and fire order is determined
/// at drain time by deadline, not insertion order.
pub struct Timers {
    entries: Vec<TimerEntry>,
    next_id: TimerId,
    clock: Arc<dyn Clock>,
}

impl Timers {
    pub(crate) fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
            clock,
        }
    }

    pub(crate) fn set(&mut self, delay: Duration, handle: LuaHandle) -> TimerId {
        self.push(delay, None, handle)
    }

    pub(crate) fn every(&mut self, period: Duration, handle: LuaHandle) -> TimerId {
        self.push(period, Some(period), handle)
    }

    fn push(&mut self, delay: Duration, period: Option<Duration>, handle: LuaHandle) -> TimerId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let now = self.clock.instant_now();
        self.entries.push(TimerEntry {
            id,
            deadline: now + delay,
            period,
            handle,
        });
        id
    }

    pub(crate) fn cancel(&mut self, id: TimerId) -> bool {
        let Some(idx) = self.entries.iter().position(|e| e.id == id) else {
            return false;
        };
        self.entries.swap_remove(idx);
        true
    }

    pub fn drain_due(&mut self, lua: &Lua) -> Vec<mlua::Function> {
        let now = self.clock.instant_now();
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
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries.iter().map(|e| e.deadline).min()
    }

    /// Cancel every scheduled timer. Used by `/reload`.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::clock::{MonoClock, VirtualClock};
    use mlua::Lua;
    use std::time::SystemTime;

    fn handle(lua: &Lua, src: &str) -> LuaHandle {
        let func: mlua::Function = lua.load(src).eval().expect("load");
        LuaHandle::from_func(lua, func).expect("registry")
    }

    fn fixture() -> (Arc<VirtualClock>, Timers) {
        let clock = Arc::new(VirtualClock::new(Instant::now(), SystemTime::now()));
        let timers = Timers::new(Arc::clone(&clock) as Arc<dyn Clock>);
        (clock, timers)
    }

    #[test]
    fn one_shot_fires_then_drops() {
        let lua = Lua::new();
        let counter = lua.create_table().unwrap();
        counter.set("n", 0i64).unwrap();
        lua.globals().set("c", counter).unwrap();
        let h = handle(&lua, "function() c.n = c.n + 1 end");
        let (clock, mut t) = fixture();
        t.set(Duration::from_millis(10), h);
        assert_eq!(t.len(), 1);
        // Not yet due - clock has not advanced past the deadline.
        assert!(t.drain_due(&lua).is_empty());
        assert_eq!(t.len(), 1);
        clock.advance(Duration::from_millis(20));
        let due = t.drain_due(&lua);
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
        let (clock, mut t) = fixture();
        let id = t.every(Duration::from_millis(10), h);
        clock.advance(Duration::from_millis(20));
        let due = t.drain_due(&lua);
        assert_eq!(due.len(), 1);
        // Still in the queue, deadline pushed forward.
        assert_eq!(t.len(), 1);
        // Without further clock advance the re-armed timer is not due yet.
        assert!(t.drain_due(&lua).is_empty());
        assert!(t.cancel(id));
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn next_deadline_reports_earliest_timer() {
        let lua = Lua::new();
        let (clock, mut t) = fixture();
        let start = clock.instant_now();
        t.set(Duration::from_millis(250), handle(&lua, "function() end"));
        t.set(Duration::from_millis(40), handle(&lua, "function() end"));
        assert_eq!(t.next_deadline(), Some(start + Duration::from_millis(40)));
    }

    #[test]
    fn cancel_returns_false_for_unknown_id() {
        let (_clock, mut t) = fixture();
        assert!(!t.cancel(42));
    }
}
