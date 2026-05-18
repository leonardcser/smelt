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
//! Adding a new event later is a one-line change on the host side
//! (`shared.drain_lifecycle_hooks("name")`); no schema change required.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::{LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "lifecycle",
        "Once-per-launch hooks keyed by event name. `on(event, fn)` is the general form; `on_ready` is a shorthand for the most common case (react to a CLI flag at startup).",
        Tier::Host,
    )?;
    {
        let s = shared.clone();
        m.fn_(
            "on",
            "Queue `fn` for the lifecycle event named `event`. Multiple hooks per event fire in registration order. Today only `\"ready\"` is emitted (after bootstrap + argv parse, before the main loop); more events may be added without breaking this API.",
            &["event", "fn"],
            move |lua, (event, func): (String, mlua::Function)| -> LuaResult<()> {
                let handle = LuaHandle::from_func(lua, func)?;
                s.register_lifecycle_hook(&event, handle);
                Ok(())
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "on_ready",
            "Shorthand for `lifecycle.on(\"ready\", fn)`. The host calls `fn` once, after Lua bootstrap and CLI parsing finish and before the main loop starts. Use this to wire a CLI flag declared via `smelt.cli.register_flag` to a startup action — e.g. open a picker, load a session, dispatch a command.",
            &["fn"],
            move |lua, func: mlua::Function| -> LuaResult<()> {
                let handle = LuaHandle::from_func(lua, func)?;
                s.register_lifecycle_hook("ready", handle);
                Ok(())
            },
        )?;
    }
    Ok(())
}
