//! `smelt.fuzzy` bindings — score a candidate string against a query.
//! Thin Lua surface over `tui::fuzzy::fuzzy_score`.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let fuzzy_tbl = lua.create_table()?;
    fuzzy_tbl.set(
        "score",
        lua.create_function(
            |_, (text, query): (String, String)| match crate::fuzzy::fuzzy_score(&text, &query) {
                Some(s) => Ok(Some(s)),
                None => Ok(None),
            },
        )?,
    )?;
    smelt.set("fuzzy", fuzzy_tbl)?;
    Ok(())
}
