//! `smelt.timer` + `smelt.defer` — one-shot and recurring timer callbacks.

use crate::lua::doc::register_fn;
use crate::lua::lua_type::LuaCallback;
use crate::lua::LuaHandle;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use std::time::Duration;

type TimerHandler = LuaCallback<(), ()>;

#[lua_module(
    name = "smelt.timer",
    doc = "One-shot and recurring timer callbacks. `defer` is a fire-and-forget alias of `timer.set`."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let timer_tbl = lua.create_table()?;
    register_fn(
        &timer_tbl,
        "smelt.timer",
        "set",
        "Schedule `handler` to run once after `ms` milliseconds. Returns the timer id, or `0` if no host is installed.",
        &["ms", "handler"],
        lua,
        |lua, (ms, handler): (u64, TimerHandler)| -> LuaResult<u64> {
            let key = lua.create_registry_value(handler.into_inner())?;
            Ok(crate::host::try_with_core(|core| {
                core.timers
                    .set(Duration::from_millis(ms), LuaHandle { key })
            })
            .unwrap_or(0))
        },
    )?;
    register_fn(
        &timer_tbl,
        "smelt.timer",
        "every",
        "Schedule `handler` to fire repeatedly every `ms` milliseconds. Returns the timer id; raises if `ms` is `0`.",
        &["ms", "handler"],
        lua,
        |lua, (ms, handler): (u64, TimerHandler)| -> LuaResult<u64> {
            if ms == 0 {
                return Err(LuaError::RuntimeError(
                    "smelt.timer.every: period must be > 0".into(),
                ));
            }
            let key = lua.create_registry_value(handler.into_inner())?;
            Ok(crate::host::try_with_core(|core| {
                core.timers
                    .every(Duration::from_millis(ms), LuaHandle { key })
            })
            .unwrap_or(0))
        },
    )?;
    register_fn(
        &timer_tbl,
        "smelt.timer",
        "cancel",
        "Cancel a previously scheduled timer by `id`. Returns `true` if a timer was cancelled, `false` if none matched or no host is installed.",
        &["id"],
        lua,
        |_, id: u64| Ok(crate::host::try_with_core(|core| core.timers.cancel(id)).unwrap_or(false)),
    )?;
    smelt.set("timer", timer_tbl)?;

    register_fn(
        smelt,
        "smelt.timer",
        "defer",
        "Schedule `handler` to run once after `ms` milliseconds. Fire-and-forget alias of `timer.set` that does not return an id.",
        &["ms", "handler"],
        lua,
        |lua, (ms, handler): (u64, TimerHandler)|  -> LuaResult<()>{
            let key = lua.create_registry_value(handler.into_inner())?;
            crate::host::try_with_core(|core| {
                core.timers
                    .set(Duration::from_millis(ms), LuaHandle { key })
            });
            Ok(())
        },
    )?;
    Ok(())
}
