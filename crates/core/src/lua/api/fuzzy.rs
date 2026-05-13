//! `smelt.fuzzy` — score a candidate string against a query.

use crate::lua::doc::{record_module_doc, register_fn};
use lua_doc_derive::lua_module;
use mlua::prelude::*;

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let fuzzy_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.fuzzy",
        "Fuzzy-match scoring for candidate strings against queries.",
    );

    register_fn(
        &fuzzy_tbl,
        "smelt.fuzzy",
        "score",
        "Return a fuzzy-match score for `text` against `query`. Higher is better; `nil` means no match.",
        &["text", "query"],
        lua,
        |_, (text, query): (String, String)| match crate::fuzzy::fuzzy_score(&text, &query) {
            Some(s) => Ok(Some(s)),
            None => Ok(None),
        },
    )?;

    smelt.set("fuzzy", fuzzy_tbl)?;
    Ok(())
}
