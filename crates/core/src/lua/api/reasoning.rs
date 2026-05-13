//! `smelt.reasoning` — get/set/cycle reasoning effort. Mirrors `smelt.mode`; stubs overridden by TUI/Lua.

use crate::lua::doc::register_fn;
use lua_doc_derive::{lua_module, LuaAlias};
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

#[lua_module(
    name = "smelt.reasoning",
    doc = "Reasoning effort read/cycle. `reasoning.set` and `reasoning.cycle` are injected by the TUI layer so they can access the live app state."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let reasoning_tbl = lua.create_table()?;
    register_fn(
        &reasoning_tbl,
        "smelt.reasoning",
        "get",
        "Return the current reasoning effort label (e.g. `\"low\"`, `\"medium\"`, `\"high\"`).",
        &[],
        lua,
        |_, ()| -> LuaResult<LuaReasoningEffort> {
            Ok(crate::host::try_with_core(|core| {
                LuaReasoningEffort::from(core.config.reasoning_effort)
            })
            .unwrap_or(LuaReasoningEffort::Medium))
        },
    )?;

    register_fn(
        &reasoning_tbl,
        "smelt.reasoning",
        "set",
        "Set the reasoning effort to the given label. No-op in core; the TUI overrides this binding.",
        &["effort"],
        lua,
        |_, _effort: LuaReasoningEffort| -> LuaResult<()> {
            // No-op in core; TUI overrides this binding.
            Ok(())
        },
    )?;

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

    register_fn(
        &reasoning_tbl,
        "smelt.reasoning",
        "cycle",
        "Advance to the next reasoning effort in the configured cycle. No-op stub in core; the TUI overrides this binding.",
        &[],
        lua,
        |_, ()| Ok(()),
    )?;

    smelt.set("reasoning", reasoning_tbl)?;
    Ok(())
}
