//! `smelt.timer` + `smelt.defer` bindings — schedule one-shot and
//! recurring callbacks via the App-level `Timers` subsystem.

use crate::lua::LuaHandle;
use mlua::prelude::*;
use std::time::Duration;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let timer_tbl = lua.create_table()?;
    timer_tbl.set(
        "set",
        lua.create_function(|lua, (ms, handler): (u64, mlua::Function)| {
            let key = lua.create_registry_value(handler)?;
            Ok(crate::lua::try_with_host(|host| {
                host.timers()
                    .set(Duration::from_millis(ms), LuaHandle { key })
            })
            .unwrap_or(0))
        })?,
    )?;
    timer_tbl.set(
        "every",
        lua.create_function(|lua, (ms, handler): (u64, mlua::Function)| {
            if ms == 0 {
                return Err(LuaError::RuntimeError(
                    "smelt.timer.every: period must be > 0".into(),
                ));
            }
            let key = lua.create_registry_value(handler)?;
            Ok(crate::lua::try_with_host(|host| {
                host.timers()
                    .every(Duration::from_millis(ms), LuaHandle { key })
            })
            .unwrap_or(0))
        })?,
    )?;
    timer_tbl.set(
        "cancel",
        lua.create_function(|_, id: u64| {
            Ok(crate::lua::try_with_host(|host| host.timers().cancel(id)).unwrap_or(false))
        })?,
    )?;
    smelt.set("timer", timer_tbl)?;

    // `smelt.defer(ms, fn)` — alias for `smelt.timer.set` kept for the
    // nvim-shaped one-shot ergonomics. Returns nothing so existing
    // callers stay untouched.
    smelt.set(
        "defer",
        lua.create_function(|lua, (ms, handler): (u64, mlua::Function)| {
            let key = lua.create_registry_value(handler)?;
            crate::lua::try_with_app(|app| {
                app.core
                    .timers
                    .set(Duration::from_millis(ms), LuaHandle { key })
            });
            Ok(())
        })?,
    )?;
    Ok(())
}
