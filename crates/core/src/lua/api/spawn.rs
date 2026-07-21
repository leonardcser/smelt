//! `smelt.spawn` - Lua coroutine on the `LuaTaskRuntime`. Returns a `Reg`
//! whose `:remove()` cancels the task.

use crate::lua::doc::Tier;
use crate::lua::lua_type::LuaCallback;
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use crate::lua::{LuaShared, TaskCompletion};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::extend(lua, smelt.clone(), "smelt", Tier::Host);
    let s = shared.clone();
    m.fn_(
        "spawn",
        "Run `handler` as a coroutine on the Lua task runtime. The handler may yield; its result is discarded. Returns a `Reg` whose `:remove()` cancels the task. Any in-flight `smelt.sleep` / `smelt.task.wait` raises `cancelled` and the coroutine unwinds. Spawns inherit the current task lifecycle; top-level spawns are app-scoped and survive agent-turn interrupts.",
        &["handler"],
        move |lua, handler: LuaCallback<(), ()>| -> LuaResult<LuaReg> {
            let id = {
                let mut rt = s.tasks.lock().map_err(|_| {
                    LuaError::RuntimeError("smelt.spawn: task runtime unavailable".into())
                })?;
                rt.spawn(
                    lua,
                    handler.into_inner(),
                    mlua::MultiValue::new(),
                    TaskCompletion::FireAndForget,
                )?
            };
            let s2 = s.clone();
            let lua = lua.weak();
            Ok(LuaReg::new(move || {
                let Some(lua) = lua.try_upgrade() else {
                    return false;
                };
                if let Ok(mut rt) = s2.tasks.lock() {
                    return rt.cancel_task(&lua, id);
                }
                false
            }))
        },
    )?;
    Ok(())
}
