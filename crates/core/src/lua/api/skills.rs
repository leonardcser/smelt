//! `smelt.skills` — list/load skill content from the `SkillLoader` populated at startup.

use crate::lua::doc::register_fn;
use lua_doc_derive::lua_module;
use mlua::prelude::*;

#[lua_module(
    name = "smelt.skills",
    doc = "List and load skill content from the SkillLoader populated at startup."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let tbl = lua.create_table()?;
    register_fn(
        &tbl,
        "smelt.skills",
        "content",
        "Load the skill named `name` and return `(content, nil)` on success or `(nil, err_string)` if the skill is missing or failed to load.",
        &["name"],
        lua,
        |_, name: String| -> LuaResult<(Option<String>, Option<String>)> {
            let resolved = crate::host::try_with_core(|core| {
                core.skills.as_ref().map(|loader| loader.content(&name))
            })
            .flatten();
            match resolved {
                Some(Ok(content)) => Ok((Some(content), None)),
                Some(Err(msg)) => Ok((None, Some(msg))),
                None => Ok((None, Some("no skills loaded".to_string()))),
            }
        },
    )?;

    register_fn(
        &tbl,
        "smelt.skills",
        "list",
        "Return the names of every skill discovered by the loader as a Lua array. Empty when no skills are loaded.",
        &[],
        lua,
        |lua, ()|  -> LuaResult<mlua::Table>{
            let names: Vec<String> = crate::host::try_with_core(|core| {
                core.skills
                    .as_ref()
                    .map(|loader| loader.names())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
            let t = lua.create_table()?;
            for (i, n) in names.into_iter().enumerate() {
                t.set(i + 1, n)?;
            }
            Ok(t)
        },
    )?;

    smelt.set("skills", tbl)?;
    Ok(())
}
