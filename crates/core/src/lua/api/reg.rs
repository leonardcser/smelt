//! `smelt.reg` - construct `Reg` handles from Lua. Useful for plugin
//! authors who compose several reactive subscriptions and want a single
//! cancellation surface to return.

use crate::lua::doc::Tier;
use crate::lua::lua_type::LuaCallback;
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "reg",
        "Helpers for constructing `Reg` handles. Plugins that own several reactive subscriptions can wrap their teardown logic in a single `Reg` returned to callers.",
        Tier::Host,
    )?;
    m.fn_(
        "new",
        "Wrap `undo` as a `Reg`. The first call to `:remove()` invokes `undo()` and returns `true`; subsequent calls are no-ops returning `false`. Errors raised inside `undo` are swallowed.",
        &["undo"],
        |lua, undo: LuaCallback<(), ()>| -> LuaResult<LuaReg> {
            let key = lua.create_registry_value(undo.into_inner())?;
            let lua = lua.weak();
            Ok(LuaReg::new(move || {
                let Some(lua) = lua.try_upgrade() else {
                    return false;
                };
                if let Ok(func) = lua.registry_value::<mlua::Function>(&key) {
                    let _ = func.call::<()>(());
                }
                let _ = lua.remove_registry_value(key);
                true
            }))
        },
    )?;
    Ok(())
}
