//! `smelt.reasoning` — callable selector for reasoning effort. Mirrors
//! `smelt.mode`. `smelt.reasoning()` reads, `smelt.reasoning(v)` sets,
//! `smelt.reasoning.cycle_list()` returns the configured cycle.

use crate::lua::doc::{record_module_doc, register_fn};
use lua_doc_derive::LuaAlias;
use mlua::prelude::*;

/// Reasoning effort level string literal.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.reasoning.Effort", mirror = "protocol::ReasoningEffort")]
pub enum LuaReasoningEffort {
    Off,
    Low,
    Medium,
    High,
    Max,
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    record_module_doc(
        "smelt.reasoning",
        "Reasoning-effort selector. `smelt.reasoning()` reads the active effort; `smelt.reasoning(v)` sets it (overridden by the TUI to apply the change). `smelt.reasoning.cycle_list()` lists the configured cycle.",
    );

    let reasoning_tbl = lua.create_table()?;
    register_fn(
        &reasoning_tbl,
        "smelt.reasoning",
        "cycle_list",
        "Return the configured reasoning-effort cycle.",
        &[],
        lua,
        |_, ()| -> LuaResult<Vec<LuaReasoningEffort>> {
            Ok(crate::host::try_with_core(|core| {
                core.config
                    .reasoning_cycle
                    .iter()
                    .copied()
                    .map(LuaReasoningEffort::from)
                    .collect()
            })
            .unwrap_or_default())
        },
    )?;

    // `__call`: get when no arg, no-op set stub here (TUI overrides).
    let f = lua.create_function(
        |lua, (_tbl, v): (mlua::Table, Option<LuaReasoningEffort>)| -> LuaResult<mlua::Value> {
            if v.is_some() {
                return Ok(mlua::Value::Nil);
            }
            let cur = crate::host::try_with_core(|core| {
                LuaReasoningEffort::from(core.config.reasoning_effort)
            })
            .unwrap_or(LuaReasoningEffort::Medium);
            cur.into_lua(lua)
        },
    )?;
    let mt = lua.create_table()?;
    mt.set("__call", f)?;
    reasoning_tbl.set_metatable(Some(mt))?;

    smelt.set("reasoning", reasoning_tbl)?;
    Ok(())
}
