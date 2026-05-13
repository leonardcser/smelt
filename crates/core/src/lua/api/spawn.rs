//! `smelt.spawn` — fire-and-forget Lua coroutine on the `LuaTaskRuntime`.

use crate::lua::doc::register_fn;
use crate::lua::lua_type::LuaCallback;
use crate::lua::{LuaShared, TaskCompletion};
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use std::sync::Arc;

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let s = shared.clone();
    register_fn(
        smelt,
        "smelt",
        "spawn",
        "Run `handler` as a fire-and-forget coroutine on the Lua task runtime. The handler may yield; its result is discarded.",
        &["handler"],
        lua,
        move |lua, handler: LuaCallback<(), ()>|  -> LuaResult<()>{
            if let Ok(mut rt) = s.tasks.lock() {
                rt.spawn(
                    lua,
                    handler.into_inner(),
                    mlua::MultiValue::new(),
                    TaskCompletion::FireAndForget,
                )?;
            }
            Ok(())
        },
    )?;
    Ok(())
}
