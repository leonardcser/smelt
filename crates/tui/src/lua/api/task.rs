//! `smelt.task` bindings — `alloc` mints external task ids the
//! `_bootstrap.lua` yield primitives suspend on; `resume` delivers a
//! resolution to a pending coroutine. Used internally by the wrappers
//! around tool-as-task / dialog / picker / async helpers.

use crate::lua::{LuaShared, TaskEvent};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let task_tbl = lua.create_table()?;
    {
        let s = shared.clone();
        task_tbl.set(
            "alloc",
            lua.create_function(move |_, ()| {
                Ok(s.next_external_id.fetch_add(1, Ordering::Relaxed))
            })?,
        )?;
    }
    {
        let s = shared.clone();
        task_tbl.set(
            "resume",
            lua.create_function(move |lua, (id, value): (u64, mlua::Value)| {
                let key = lua.create_registry_value(value)?;
                if let Ok(mut inbox) = s.task_inbox.lock() {
                    inbox.push(TaskEvent::ExternalResolved {
                        external_id: id,
                        value: key,
                    });
                }
                Ok(())
            })?,
        )?;
    }
    smelt.set("task", task_tbl)?;
    Ok(())
}
