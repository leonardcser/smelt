//! `smelt.mode` — callable selector for the agent mode.
//! `smelt.mode()` reads, `smelt.mode(v)` sets (TUI override), and
//! `smelt.mode.cycle_list()` returns the configured cycle.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use lua_doc_derive::LuaAlias;
use mlua::prelude::*;

/// Agent mode string literal.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.mode.Mode", mirror = "protocol::AgentMode")]
pub enum LuaAgentMode {
    Normal,
    Plan,
    Apply,
    Yolo,
}

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
        "Return the configured agent-mode cycle; falls back to all known modes when the user has not customized one.",
        &[],
        |_, ()| -> LuaResult<Vec<LuaAgentMode>> {
            Ok(crate::host::try_with_core(|core| {
                let cycle: &[protocol::AgentMode] = if core.config.mode_cycle.is_empty() {
                    protocol::AgentMode::ALL
                } else {
                    &core.config.mode_cycle
                };
                cycle.iter().copied().map(LuaAgentMode::from).collect()
            })
            .unwrap_or_default())
        },
    )?;

    // `__call`: get when no arg, no-op set stub here (TUI overrides).
    let f = lua.create_function(
        |lua, (_tbl, v): (mlua::Table, Option<LuaAgentMode>)| -> LuaResult<mlua::Value> {
            if v.is_some() {
                return Ok(mlua::Value::Nil);
            }
            let cur = crate::host::try_with_core(|core| LuaAgentMode::from(core.config.mode))
                .unwrap_or(LuaAgentMode::Normal);
            cur.into_lua(lua)
        },
    )?;
    let mt = lua.create_table()?;
    mt.set("__call", f)?;
    m.tbl.set_metatable(Some(mt))?;

    Ok(())
}
