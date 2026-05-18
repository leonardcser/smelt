//! `smelt.lifecycle` — once-per-launch hooks tied to host phases.
//!
//! Hooks are keyed by event name (string), drained by the host at the
//! matching phase. Currently emitted events:
//!
//!   - `"ready"` — after Lua bootstrap and CLI parsing complete, before
//!     the main loop begins. Inside the hook the full UiHost-tier surface
//!     is live (`smelt.cli.get`, `smelt.cmd.run`, `smelt.session.load`,
//!     dialogs, ...). The intended place to react to a Lua-declared CLI
//!     flag without Rust mediating.
//!
//! Storage is the shared `HookRegistry` with `drain_for` semantics, so
//! adding a new event later is purely a host-side change
//! (`hooks.lifecycle.drain_for(lua, "name")`); no schema change required.
//! `on` and `on_ready` return an `off()` that unregisters the hook
//! before it fires (no-op afterwards).

use crate::lua::doc::Tier;
use crate::lua::lua_type::LuaCallback;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "lifecycle",
        "Once-per-launch hooks keyed by event name. `on(event, fn)` is the general form; `on_ready` is a shorthand for the most common case (react to a CLI flag at startup). Each hook fires at most once per launch and is dropped from the registry on fire.",
        Tier::Host,
    )?;
    {
        let s = shared.clone();
        m.fn_(
            "on",
            "Queue `fn` for the lifecycle event named `event`. Multiple hooks per event fire in registration order. Today only `\"ready\"` is emitted (after bootstrap + argv parse, before the main loop); more events may be added without breaking this API. Returns an `off()` that unregisters the hook before it fires; calling `off()` after the hook has already fired is a no-op returning `false`.",
            &["event", "fn"],
            move |lua, (event, func): (mlua::String, mlua::Function)| -> LuaResult<LuaCallback<(), bool>> {
                let event = event.to_string_lossy().to_string();
                let id = s.hooks.lifecycle.register(lua, func, event)?;
                let off = s.hooks.lifecycle.off_for(lua, id)?;
                LuaCallback::<(), bool>::from_lua(mlua::Value::Function(off), lua)
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "on_ready",
            "Shorthand for `lifecycle.on(\"ready\", fn)`. The host calls `fn` once, after Lua bootstrap and CLI parsing finish and before the main loop starts. Use this to wire a CLI flag declared via `smelt.cli.register_flag` to a startup action — e.g. open a picker, load a session, dispatch a command. Returns an `off()` that unregisters the hook before `ready` fires.",
            &["fn"],
            move |lua, func: mlua::Function| -> LuaResult<LuaCallback<(), bool>> {
                let id = s.hooks.lifecycle.register(lua, func, "ready")?;
                let off = s.hooks.lifecycle.off_for(lua, id)?;
                LuaCallback::<(), bool>::from_lua(mlua::Value::Function(off), lua)
            },
        )?;
    }
    Ok(())
}
