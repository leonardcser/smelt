//! `smelt.agent` - adjust agent-facing prompt context from Lua plugins.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use mlua::prelude::*;

pub(super) const SYSTEM_PROMPT_FRAGMENTS_REGISTRY: &str = "__smelt_agent_system_prompt_fragments";

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
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
