//! `smelt.mode` — `get / set / cycle / cycle_list` over `protocol::AgentMode`
//! (Plan / Apply / Yolo / Normal). Lives at top-level so it does
//! not collide with the future `smelt.process` long-lived IPC.
//!
//! `cycle` is seeded as a no-op stub here so callers always see a
//! function; `runtime/lua/smelt/modes.lua` overrides it with the
//! real Lua-side cycle implementation that reads `cycle_list` and
//! calls `set`. `set` is a no-op stub in core; the TUI overrides it
//! with a binding that mutates `app.core.config.mode`.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let mode_tbl = lua.create_table()?;

    mode_tbl.set(
        "get",
        lua.create_function(|_, ()| {
            Ok(crate::host::try_with_host(|host| host.config().mode.as_str().to_string())
                .unwrap_or_default())
        })?,
    )?;

    mode_tbl.set(
        "set",
        lua.create_function(|_, _v: String| {
            // No-op in core; TUI overrides this binding.
            Ok(())
        })?,
    )?;

    // The configured cycle as a list of mode label strings. Returns
    // the full `protocol::AgentMode::ALL` order when no cycle is set so
    // callers don't need to handle the "empty cycle" edge case.
    mode_tbl.set(
        "cycle_list",
        lua.create_function(|lua, ()| {
            let cycle: Vec<String> = crate::host::try_with_host(|host| {
                let cycle: &[protocol::AgentMode] = if host.config().mode_cycle.is_empty() {
                    protocol::AgentMode::ALL
                } else {
                    &host.config().mode_cycle
                };
                cycle.iter().map(|m| m.as_str().to_string()).collect()
            })
            .unwrap_or_default();
            let t = lua.create_table()?;
            for (i, label) in cycle.into_iter().enumerate() {
                t.set(i + 1, label)?;
            }
            Ok(t)
        })?,
    )?;

    mode_tbl.set("cycle", lua.create_function(|_, ()| Ok(()))?)?;

    smelt.set("mode", mode_tbl)?;
    Ok(())
}
