//! `smelt.lifecycle` - host-phase hooks.
//!
//! Hooks are keyed by event name (string), drained by the host at the
//! matching phase. Each hook receives a `ctx` table whose keys are
//! documented per-event. Emitted events:
//!
//!   - `"ready"` - fires every time the Lua context comes up: once on
//!     cold start and once after every `/reload`. The host registry is
//!     wiped between generations, so plugins that re-register their hook
//!     in module body get drained on every cycle. `ctx = { kind }`
//!     where `kind = "launch" | "reload"`. Normal configuration and plugin
//!     bodies already run with the full UiHost-tier surface. Use this hook for
//!     work that must happen only after the generation is committed and its
//!     layout is current. Plain module body remains the right place to declare
//!     resources and callbacks.
//!   - `"shutdown"` - after the TUI tears down, before the process exits.
//!     Stdout is back in cooked mode so `print` lands in the user's
//!     terminal scrollback. Ctx: `{ session_id: string, has_messages: bool,
//!     ephemeral: bool }`. Once per graceful process exit.
//!
//! Storage is the shared `HookRegistry` with `drain_for` semantics, so
//! adding a new event later is purely a host-side change
//! (`runtime.drain_lifecycle_hooks("name", build_ctx)`); no schema
//! change required. `on`, `on_ready`, `on_shutdown` return a `Reg`
//! whose `:remove()` unregisters the hook before it fires (no-op
//! afterwards).

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "lifecycle",
        "Host-phase hooks keyed by event name. `on(event, fn)` is the general form; `on_ready` / `on_shutdown` are shorthands. `on_ready` fires after each generation is committed (cold start and `/reload`) with `ctx = { kind = \"launch\" | \"reload\" }`; use it for work that requires committed runtime state or current layout. Normal configuration and plugin bodies already have UiHost access. `on_shutdown` fires once on graceful process exit. The registry is wiped between generations - re-register the hook in module body if you want it to fire every time. Each `on*` returns a `Reg` whose `:remove()` unregisters the hook before it fires.",
        Tier::Host,
    )?;
    {
        let s = shared.clone();
        m.fn_(
            "on",
            "Queue `fn(ctx)` for the lifecycle event named `event`. Multiple hooks per event fire in registration order. Emitted events: `\"ready\"` (after launch and after each `/reload`; `ctx = { kind = \"launch\" | \"reload\" }`) and `\"shutdown\"` (after the TUI tears down, before process exit; `ctx = { session_id, has_messages, ephemeral }`). Returns a `Reg` whose `:remove()` unregisters the hook before it fires; calling `:remove()` after the hook has already fired is a no-op returning `false`.",
            &["event", "fn"],
            move |lua, (event, func): (mlua::LuaString, mlua::Function)| -> LuaResult<LuaReg> {
                let event = event.to_string_lossy().to_string();
                let id = s.hooks.lifecycle.register(lua, func, event)?;
                Ok(s.hooks.lifecycle.reg_for(id))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "on_ready",
            "Shorthand for `lifecycle.on(\"ready\", fn)`. The host calls `fn(ctx)` after generation zero commits (`ctx.kind = \"launch\"`) and after every successful `/reload` (`ctx.kind = \"reload\"`). The full UiHost-tier surface is also available while normal configuration and plugin bodies load; use this hook specifically for work that requires committed runtime state or current layout. The registry is wiped between generations, so re-registering this hook at module top is the correct way to make it fire every cycle. Prefer plain module body for declarations and callback registration. Returns a `Reg` whose `:remove()` unregisters the hook before it fires.",
            &["fn"],
            move |lua, func: mlua::Function| -> LuaResult<LuaReg> {
                let id = s.hooks.lifecycle.register(lua, func, "ready")?;
                Ok(s.hooks.lifecycle.reg_for(id))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "on_shutdown",
            "Shorthand for `lifecycle.on(\"shutdown\", fn)`. The host calls `fn(ctx)` once after the TUI tears down, before the process exits. Stdout is cooked at that point so `print` writes land in the user's terminal scrollback. `ctx = { session_id: string, has_messages: boolean, ephemeral: boolean }`. Hooks fire on graceful exit paths, including handled interrupt and termination signals. Returns a `Reg` whose `:remove()` unregisters the hook before `shutdown` fires.",
            &["fn"],
            move |lua, func: mlua::Function| -> LuaResult<LuaReg> {
                let id = s.hooks.lifecycle.register(lua, func, "shutdown")?;
                Ok(s.hooks.lifecycle.reg_for(id))
            },
        )?;
    }
    Ok(())
}
