//! `smelt.skills` bindings — read the loaded `SkillLoader` populated
//! at startup. Backs `runtime/lua/smelt/tools/load_skill.lua`.
//!
//! The loader scans `~/.config/smelt/skills/*/SKILL.md` plus
//! workspace-local + config-extra paths once at boot; both engine
//! (system prompt section) and tui (this binding) read from the
//! same `Arc<SkillLoader>` clone.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let tbl = lua.create_table()?;

    tbl.set(
        "content",
        lua.create_function(|_, name: String| {
            let resolved = crate::lua::try_with_app(|app| {
                app.core.skills.as_ref().map(|loader| loader.content(&name))
            })
            .flatten();
            match resolved {
                Some(Ok(content)) => Ok((Some(content), None)),
                Some(Err(msg)) => Ok((None, Some(msg))),
                None => Ok((None, Some("no skills loaded".to_string()))),
            }
        })?,
    )?;

    tbl.set(
        "list",
        lua.create_function(|lua, ()| {
            let names: Vec<String> = crate::lua::try_with_app(|app| {
                app.core
                    .skills
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
        })?,
    )?;

    smelt.set("skills", tbl)?;
    Ok(())
}
