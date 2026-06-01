//! `smelt.mode` - callable selector for the agent mode.
//! `smelt.mode()` reads, `smelt.mode(v)` sets (TUI override), and
//! `smelt.mode.cycle_list()` returns the configured cycle.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "mode",
        "Agent-mode selector. `smelt.mode()` reads the active mode; `smelt.mode(v)` sets it (overridden by the TUI to apply the change). `smelt.mode.cycle_list()` lists the configured cycle.",
        Tier::Host,
    )?;
    m.fn_(
        "cycle_list",
        "Return the configured agent-mode cycle; falls back to the built-in default when the user has not customized one.",
        &[],
        |_, ()| -> LuaResult<Vec<String>> {
            Ok(crate::host::try_with_core(|core| {
                let cycle = if core.config.mode_cycle.is_empty() {
                    protocol::AgentMode::default_cycle()
                } else {
                    core.config.mode_cycle.clone()
                };
                cycle.into_iter().map(String::from).collect()
            })
            .unwrap_or_default())
        },
    )?;

    // `__call`: get when no arg, no-op set stub here (TUI overrides).
    m.callable(
        |lua, (_tbl, v): (mlua::Table, Option<String>)| -> LuaResult<mlua::Value> {
            if v.is_some() {
                return Ok(mlua::Value::Nil);
            }
            let cur = crate::host::try_with_core(|core| core.config.mode.as_str().to_string())
                .unwrap_or_else(|| protocol::AgentMode::normal().to_string());
            cur.into_lua(lua)
        },
    )?;

    Ok(())
}
