//! `smelt.spawn` binding — fire-and-forget Lua coroutine on the
//! `LuaTaskRuntime`. Used by plugin authors who want a background
//! coroutine without owning its result.

use crate::lua::{LuaShared, TaskCompletion};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let s = shared.clone();
    smelt.set(
        "spawn",
        lua.create_function(move |lua, handler: mlua::Function| {
            if let Ok(mut rt) = s.tasks.lock() {
                rt.spawn(
                    lua,
                    handler,
                    mlua::MultiValue::new(),
                    TaskCompletion::FireAndForget,
                )?;
            }
            Ok(())
        })?,
    )?;
    Ok(())
}
