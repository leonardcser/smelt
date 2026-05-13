//! `smelt.task` — `alloc`/`resume` for the yield-then-resume coroutine bridge.

use crate::lua::doc::register_fn;
use crate::lua::{LuaShared, TaskEvent};
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

#[lua_module(
    name = "smelt.task",
    doc = "Yield-then-resume coroutine bridge: alloc and resume external tasks."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let task_tbl = lua.create_table()?;
    {
        let s = shared.clone();
        register_fn(
            &task_tbl,
            "smelt.task",
            "alloc",
            "Allocate and return a fresh external task id used to pair a yielded coroutine with a later `task.resume` call.",
            &[],
            lua,
            move |_, ()| Ok(s.next_external_id.fetch_add(1, Ordering::Relaxed)),
        )?;
    }
    {
        let s = shared.clone();
        register_fn(
            &task_tbl,
            "smelt.task",
            "resume",
            "Resume the yielded task `id` with `value`. The runtime delivers `value` as the return of the matching `coroutine.yield`.",
            &["id", "value"],
            lua,
            move |lua, (id, value): (u64, mlua::Value)|  -> LuaResult<()>{
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
    smelt.set("task", task_tbl)?;
    Ok(())
}
