//! `smelt.agent` - adjust agent-facing prompt context from Lua plugins.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use mlua::prelude::*;

pub(super) const SYSTEM_PROMPT_FRAGMENTS_REGISTRY: &str = "__smelt_agent_system_prompt_fragments";

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "agent",
        "Agent-facing prompt customization for Lua plugins.",
        Tier::Host,
    )?;

    m.fn_(
        "add_system_prompt",
        "Append concise guidance to the system prompt while this Lua runtime is active. Intended for plugins that register tools needing extra usage policy. Returns a `Reg` whose `:remove()` removes the fragment.",
        &["text"],
        |lua, text: String| -> LuaResult<LuaReg> {
            let fragments = match lua.named_registry_value::<mlua::Table>(SYSTEM_PROMPT_FRAGMENTS_REGISTRY) {
                Ok(table) => table,
                Err(_) => {
                    let table = lua.create_table()?;
                    lua.set_named_registry_value(SYSTEM_PROMPT_FRAGMENTS_REGISTRY, table.clone())?;
                    table
                }
            };
            let id = fragments.raw_len() + 1;
            fragments.raw_set(id, text)?;

            let lua = lua.weak();
            Ok(LuaReg::new(move || {
                let Some(lua) = lua.try_upgrade() else {
                    return false;
                };
                let Ok(fragments) =
                    lua.named_registry_value::<mlua::Table>(SYSTEM_PROMPT_FRAGMENTS_REGISTRY)
                else {
                    return false;
                };
                let _ = fragments.raw_set(id, mlua::Value::Nil);
                true
            }))
        },
    )?;

    m.fn_(
        "runs",
        "List observed agents for a parent session. Records expose owner, parent_agent_id, typed status, supported controls and the child transcript session_id. Observation does not enable smelt-owned delegation.",
        &["parent_id"],
        |lua, parent_id: String| -> LuaResult<mlua::Value> {
            let records = crate::host::try_with_core(|core| {
                core.agents.restore(&core.sessions, &parent_id)?;
                Ok::<_, String>(core.agents.runs(&parent_id).map(|agent| {
                    let mut record = serde_json::to_value(&agent.info).expect("agent record serializes");
                    record["session_id"] = serde_json::json!(agent.session.id);
                    record["persistence_error"] = serde_json::json!(core.agents.archive_error(&parent_id, &agent.info.id));
                    record
                }).collect::<Vec<_>>())
            }).unwrap_or_else(|| Ok(Vec::new())).map_err(LuaError::external)?;
            crate::lua::serde_to_lua(lua, &records)
        },
    )?;
    m.live_only_fn(
        "stop",
        "Request cancellation from an observed agent's execution owner. Returns false for unknown, terminal or unsupported agents; never interrupts the parent as a fallback.",
        &["parent_id", "id"],
        |_, (parent_id, id): (String, String)| -> LuaResult<bool> {
            Ok(crate::host::try_with_core(|core| {
                let supported = core.agents.runs(&parent_id).any(|agent| agent.info.id == id
                    && agent.info.capabilities.stop && !agent.info.status.is_terminal());
                if supported {
                    core.engine.send(protocol::UiCommand::StopAgent { session_id: parent_id, agent_id: id });
                }
                supported
            }).unwrap_or(false))
        },
    )?;
    Ok(())
}

pub fn system_prompt_fragments(lua: &Lua) -> Vec<String> {
    let Ok(fragments) = lua.named_registry_value::<mlua::Table>(SYSTEM_PROMPT_FRAGMENTS_REGISTRY)
    else {
        return Vec::new();
    };
    let mut keyed = Vec::new();
    for (id, text) in fragments.pairs::<usize, String>().flatten() {
        if !text.trim().is_empty() {
            keyed.push((id, text));
        }
    }
    keyed.sort_by_key(|(id, _)| *id);
    keyed.into_iter().map(|(_, text)| text).collect()
}
