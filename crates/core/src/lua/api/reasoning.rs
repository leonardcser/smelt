//! `smelt.reasoning` - reasoning-effort selector. Mirrors `smelt.mode`.
//! `smelt.reasoning.current()` reads, `smelt.reasoning.set(v)` sets in a TUI
//! session, and `smelt.reasoning.cycle_list()` returns the configured cycle.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
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
    let m = LuaMod::under(
        lua,
        smelt,
        "reasoning",
        "Reasoning-effort selector. `smelt.reasoning.current()` reads the active effort, `smelt.reasoning.set(v)` sets it in a TUI session, and `smelt.reasoning.cycle_list()` lists the configured cycle.",
        Tier::Host,
    )?;
    m.fn_(
        "cycle_list",
        "Return the configured reasoning-effort cycle.",
        &[],
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

    m.fn_(
        "current",
        "Return the active reasoning effort.",
        &[],
        |_, ()| -> LuaResult<LuaReasoningEffort> {
            Ok(crate::host::try_with_core(|core| {
                LuaReasoningEffort::from(core.config.reasoning_effort)
            })
            .unwrap_or(LuaReasoningEffort::Medium))
        },
    )?;

    Ok(())
}
