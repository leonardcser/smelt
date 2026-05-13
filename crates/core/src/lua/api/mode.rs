//! `smelt.mode` — get/set/cycle agent mode. `set` and `cycle` are stubs here; TUI/Lua override them.

use crate::lua::doc::{record_module_doc, register_fn};
use lua_doc_derive::{lua_module, LuaAlias};
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

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let mode_tbl = lua.create_table()?;
    record_module_doc("smelt.mode", "Agent mode read/cycle. `mode.set` and `mode.cycle` are injected by the TUI layer so they can access the live app state.");

    register_fn(
        &mode_tbl,
        "smelt.mode",
        "get",
        "Return the active agent mode (e.g. `\"normal\"`, `\"plan\"`, `\"apply\"`, `\"yolo\"`).",
        &[],
        lua,
        |_, ()| -> LuaResult<LuaAgentMode> {
            Ok(
                crate::host::try_with_core(|core| LuaAgentMode::from(core.config.mode))
                    .unwrap_or(LuaAgentMode::Normal),
            )
        },
    )?;

    register_fn(
        &mode_tbl,
        "smelt.mode",
        "set",
        "Set the active agent mode. No-op in core; the TUI overrides this binding.",
        &["mode"],
        lua,
        |_, _mode: LuaAgentMode| -> LuaResult<()> {
            // No-op in core; TUI overrides this binding.
            Ok(())
        },
    )?;

    register_fn(
        &mode_tbl,
        "smelt.mode",
        "cycle_list",
        "Return the configured agent-mode cycle; falls back to all known modes when the user has not customized one.",
        &[],
        lua,
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

    register_fn(
        &mode_tbl,
        "smelt.mode",
        "cycle",
        "Advance to the next agent mode in the configured cycle. No-op stub in core; the TUI overrides this binding.",
        &[],
        lua,
        |_, ()| Ok(()),
    )?;

    smelt.set("mode", mode_tbl)?;
    Ok(())
}
