//! `smelt.mode` — `get / set / cycle / cycle_list` over `protocol::Mode`
//! (Plan / Apply / Yolo / Normal). Lives at top-level so it does
//! not collide with the future `smelt.subprocess` (sub-agents).
//!
//! `cycle` is seeded as a no-op stub here so callers always see a
//! function; `runtime/lua/smelt/modes.lua` overrides it with the
//! real Lua-side cycle implementation that reads `cycle_list` and
//! calls `set`.

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let mode_tbl = lua.create_table()?;

    mode_tbl.set(
        "get",
        app_read!(lua, |app| app.core.config.mode.as_str().to_string()),
    )?;

    mode_tbl.set(
        "set",
        lua.create_function(|_, v: String| {
            crate::lua::with_app(|app| match protocol::Mode::parse(&v) {
                Some(mode) => app.set_mode(mode),
                None => app.notify_error(format!("unknown mode: {v}")),
            });
            Ok(())
        })?,
    )?;

    // The configured cycle as a list of mode label strings. Returns
    // the full `protocol::Mode::ALL` order when no cycle is set so
    // callers don't need to handle the "empty cycle" edge case.
    mode_tbl.set(
        "cycle_list",
        app_read!(lua, |app| {
            let cycle: &[protocol::Mode] = if app.core.config.mode_cycle.is_empty() {
                protocol::Mode::ALL
            } else {
                &app.core.config.mode_cycle
            };
            cycle
                .iter()
                .map(|m| m.as_str().to_string())
                .collect::<Vec<_>>()
        }),
    )?;

    mode_tbl.set("cycle", lua.create_function(|_, ()| Ok(()))?)?;

    smelt.set("mode", mode_tbl)?;
    Ok(())
}
