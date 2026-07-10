//! `smelt.timer` - one-shot and recurring timer callbacks. Each call
//! returns a `Reg` userdata whose `:remove()` cancels the timer.

use crate::lua::doc::Tier;
use crate::lua::lua_type::LuaCallback;
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use crate::lua::{LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;
use std::time::Duration;

type TimerHandler = LuaCallback<(), ()>;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "timer",
        "One-shot and recurring timer callbacks. Each call returns a `Reg` whose `:remove()` cancels the timer.",
        Tier::Host,
    )?;
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "set",
            "Schedule `handler` to run once after `ms` milliseconds. Returns a `Reg` whose `:remove()` cancels the timer before it fires.",
            &["ms", "handler"],
            move |lua, (ms, handler): (u64, TimerHandler)| -> LuaResult<LuaReg> {
                let handle = LuaHandle::from_func(lua, handler.into_inner())?;
                let generation = shared.generation_id();
                let id = crate::host::try_with_core(|core| {
                    core.timers.set_for_generation(
                        Duration::from_millis(ms),
                        handle,
                        generation,
                    )
                })
                .unwrap_or(0);
                Ok(LuaReg::new(move || {
                    crate::host::try_with_core(|core| core.timers.cancel(id)).unwrap_or(false)
                }))
            },
        )?;
    }
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "every",
            "Schedule `handler` to fire repeatedly every `ms` milliseconds. Returns a `Reg` whose `:remove()` stops the timer. Raises if `ms` is `0`.",
            &["ms", "handler"],
            move |lua, (ms, handler): (u64, TimerHandler)| -> LuaResult<LuaReg> {
                if ms == 0 {
                    return Err(LuaError::RuntimeError(
                        "smelt.timer.every: period must be > 0".into(),
                    ));
                }
                let handle = LuaHandle::from_func(lua, handler.into_inner())?;
                let generation = shared.generation_id();
                let id = crate::host::try_with_core(|core| {
                    core.timers.every_for_generation(
                        Duration::from_millis(ms),
                        handle,
                        generation,
                    )
                })
                .unwrap_or(0);
                Ok(LuaReg::new(move || {
                    crate::host::try_with_core(|core| core.timers.cancel(id)).unwrap_or(false)
                }))
            },
        )?;
    }
    Ok(())
}
