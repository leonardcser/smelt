//! `smelt.timer` — one-shot and recurring timer callbacks. Each call
//! returns a `Reg` userdata whose `:remove()` cancels the timer.

use crate::lua::doc::register_fn;
use crate::lua::lua_type::LuaCallback;
use crate::lua::reg::LuaReg;
use crate::lua::LuaHandle;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use std::time::Duration;

type TimerHandler = LuaCallback<(), ()>;

#[lua_module(
    name = "smelt.timer",
    doc = "One-shot and recurring timer callbacks. Each call returns a `Reg` whose `:remove()` cancels the timer."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let timer_tbl = lua.create_table()?;
    register_fn(
        &timer_tbl,
        "smelt.timer",
        "set",
        "Schedule `handler` to run once after `ms` milliseconds. Returns a `Reg` whose `:remove()` cancels the timer before it fires.",
        &["ms", "handler"],
        lua,
        |lua, (ms, handler): (u64, TimerHandler)| -> LuaResult<LuaReg> {
            let handle = LuaHandle::from_func(lua, handler.into_inner())?;
            let id = crate::host::try_with_core(|core| {
                core.timers.set(Duration::from_millis(ms), handle)
            })
            .unwrap_or(0);
            Ok(LuaReg::new(move || {
                crate::host::try_with_core(|core| core.timers.cancel(id)).unwrap_or(false)
            }))
        },
    )?;
    register_fn(
        &timer_tbl,
        "smelt.timer",
        "every",
        "Schedule `handler` to fire repeatedly every `ms` milliseconds. Returns a `Reg` whose `:remove()` stops the timer. Raises if `ms` is `0`.",
        &["ms", "handler"],
        lua,
        |lua, (ms, handler): (u64, TimerHandler)| -> LuaResult<LuaReg> {
            if ms == 0 {
                return Err(LuaError::RuntimeError(
                    "smelt.timer.every: period must be > 0".into(),
                ));
            }
            let handle = LuaHandle::from_func(lua, handler.into_inner())?;
            let id = crate::host::try_with_core(|core| {
                core.timers.every(Duration::from_millis(ms), handle)
            })
            .unwrap_or(0);
            Ok(LuaReg::new(move || {
                crate::host::try_with_core(|core| core.timers.cancel(id)).unwrap_or(false)
            }))
        },
    )?;
    smelt.set("timer", timer_tbl)?;
    Ok(())
}
