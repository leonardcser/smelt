//! `smelt.lifecycle` — once-per-launch hooks tied to host phases.
//!
//! Hooks are keyed by event name (string), drained by the host at the
//! matching phase. Each hook receives a `ctx` table whose keys are
//! documented per-event. Emitted events:
//!
//!   - `"ready"` — after Lua bootstrap and CLI parsing, before the main
//!     loop. Ctx is empty. The full UiHost-tier surface is live.
//!   - `"shutdown"` — after the TUI tears down, before the process exits.
//!     Stdout is back in cooked mode so `print` lands in the user's
//!     terminal scrollback. Ctx: `{ session_id: string, has_messages:
//!     bool }`.
//!
//! Storage is the shared `HookRegistry` with `drain_for` semantics, so
//! adding a new event later is purely a host-side change
//! (`runtime.drain_lifecycle_hooks("name", build_ctx)`); no schema
//! change required. `on`, `on_ready`, and `on_shutdown` return an
//! `off()` that unregisters the hook before it fires (no-op afterwards).

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
        "Once-per-launch hooks keyed by event name. `on(event, fn)` is the general form; `on_ready` / `on_shutdown` are shorthands for the two emitted events. Each callback receives an event-specific `ctx` table. Each hook fires at most once per launch and is dropped from the registry on fire.",
        Tier::Host,
    )?;
    {
        let s = shared.clone();
        m.fn_(
            "on",
            "Queue `fn(ctx)` for the lifecycle event named `event`. Multiple hooks per event fire in registration order. Emitted events: `\"ready\"` (after bootstrap + argv parse, before the main loop; `ctx` is an empty table) and `\"shutdown\"` (after the TUI tears down, before process exit; `ctx = { session_id, has_messages }`). Returns an `off()` that unregisters the hook before it fires; calling `off()` after the hook has already fired is a no-op returning `false`.",
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
            "Shorthand for `lifecycle.on(\"ready\", fn)`. The host calls `fn(ctx)` once, after Lua bootstrap and CLI parsing finish and before the main loop starts. `ctx` is currently an empty table — reserved for forward compatibility. Use this to wire a CLI flag declared via `smelt.cli.register_flag` to a startup action. Returns an `off()` that unregisters the hook before `ready` fires.",
            &["fn"],
            move |lua, func: mlua::Function| -> LuaResult<LuaCallback<(), bool>> {
                let id = s.hooks.lifecycle.register(lua, func, "ready")?;
                let off = s.hooks.lifecycle.off_for(lua, id)?;
                LuaCallback::<(), bool>::from_lua(mlua::Value::Function(off), lua)
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "on_shutdown",
            "Shorthand for `lifecycle.on(\"shutdown\", fn)`. The host calls `fn(ctx)` once after the TUI tears down, before the process exits. Stdout is cooked at that point so `print` writes land in the user's terminal scrollback. `ctx = { session_id: string, has_messages: boolean }`. Hooks only fire on the normal exit path (quit / Ctrl-D / `smelt.quit`); SIGINT/SIGTERM bypass them. Returns an `off()` that unregisters the hook before `shutdown` fires.",
            &["fn"],
            move |lua, func: mlua::Function| -> LuaResult<LuaCallback<(), bool>> {
                let id = s.hooks.lifecycle.register(lua, func, "shutdown")?;
                let off = s.hooks.lifecycle.off_for(lua, id)?;
                LuaCallback::<(), bool>::from_lua(mlua::Value::Function(off), lua)
            },
        )?;
    }
    Ok(())
}
