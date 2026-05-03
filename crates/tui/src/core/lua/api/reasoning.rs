//! `smelt.reasoning` — `get / set / cycle / cycle_list` over
//! `protocol::ReasoningEffort` (Off / Low / Medium / High / Max).
//! Mirrors `smelt.mode`; lives at top-level so the surface stays
//! symmetric.
//!
//! `cycle` is seeded as a no-op stub here so callers always see a
//! function; `runtime/lua/smelt/modes.lua` overrides it with the
//! real Lua-side cycle implementation that reads `cycle_list` and
//! calls `set`.

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let reasoning_tbl = lua.create_table()?;

    reasoning_tbl.set(
        "get",
        app_read!(lua, |app| app
            .core
            .config
            .reasoning_effort
            .label()
            .to_string()),
    )?;

    reasoning_tbl.set(
        "set",
        lua.create_function(|_, v: String| {
            crate::lua::with_app(|app| match protocol::ReasoningEffort::parse(&v) {
                Some(effort) => app.set_reasoning_effort(effort),
                None => app.notify_error(format!("unknown reasoning effort: {v}")),
            });
            Ok(())
        })?,
    )?;

    // The configured cycle as a list of effort labels. Returns the
    // empty list when no cycle is configured — Lua callers treat
    // that as "leave reasoning unchanged" to mirror the historical
    // `cycle_within` no-op behaviour.
    reasoning_tbl.set(
        "cycle_list",
        app_read!(lua, |app| {
            app.core
                .config
                .reasoning_cycle
                .iter()
                .map(|e| e.label().to_string())
                .collect::<Vec<_>>()
        }),
    )?;

    reasoning_tbl.set("cycle", lua.create_function(|_, ()| Ok(()))?)?;

    smelt.set("reasoning", reasoning_tbl)?;
    Ok(())
}
