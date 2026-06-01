//! `smelt.task` - `alloc`/`resume` for the yield-then-resume coroutine bridge.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::{LuaShared, TaskEvent};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "task",
        "Yield-then-resume coroutine bridge: alloc and resume external tasks.",
        Tier::Host,
    )?;
    {
        let s = shared.clone();
        m.fn_(
            "alloc",
            "Allocate and return a fresh external task id used to pair a yielded coroutine with a later `task.resume` call.",
            &[],
            move |_, ()| Ok(s.next_external_id.fetch_add(1, Ordering::Relaxed)),
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "resume",
            "Resume the yielded task `id` with `value`. The runtime delivers `value` as the return of the matching `coroutine.yield`.",
            &["id", "value"],
            move |lua, (id, value): (u64, mlua::Value)| -> LuaResult<()> {
                let key = lua.create_registry_value(value)?;
                if let Ok(mut inbox) = s.task_inbox.lock() {
                    inbox.push(TaskEvent::ExternalResolved {
                        external_id: id,
                        value: key,
                    });
                }
                Ok(())
            },
        )?;
    }
    Ok(())
}
