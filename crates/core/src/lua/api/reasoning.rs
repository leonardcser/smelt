//! `smelt.reasoning` — `get / set / cycle / cycle_list` over
//! `protocol::ReasoningEffort` (Off / Low / Medium / High / Max).
//! Mirrors `smelt.mode`; lives at top-level so the surface stays
//! symmetric.
//!
//! `cycle` is seeded as a no-op stub here so callers always see a
//! function; `runtime/lua/smelt/modes.lua` overrides it with the
//! real Lua-side cycle implementation that reads `cycle_list` and
//! calls `set`. `set` is a no-op stub in core; the TUI overrides it.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let reasoning_tbl = lua.create_table()?;

    reasoning_tbl.set(
        "get",
        lua.create_function(|_, ()| {
            Ok(crate::host::try_with_host(|host| {
                host.config().reasoning_effort.label().to_string()
            })
            .unwrap_or_default())
        })?,
    )?;

    reasoning_tbl.set(
        "set",
        lua.create_function(|_, _v: String| {
            // No-op in core; TUI overrides this binding.
            Ok(())
        })?,
    )?;

    // The configured cycle as a list of effort labels. Returns the
    // empty list when no cycle is configured.
    reasoning_tbl.set(
        "cycle_list",
        lua.create_function(|lua, ()| {
            let labels: Vec<String> = crate::host::try_with_host(|host| {
                host.config()
                    .reasoning_cycle
                    .iter()
                    .map(|e| e.label().to_string())
                    .collect()
            })
            .unwrap_or_default();
            let t = lua.create_table()?;
            for (i, label) in labels.into_iter().enumerate() {
                t.set(i + 1, label)?;
            }
            Ok(t)
        })?,
    )?;

    reasoning_tbl.set("cycle", lua.create_function(|_, ()| Ok(()))?)?;

    smelt.set("reasoning", reasoning_tbl)?;
    Ok(())
}
