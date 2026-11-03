//! `smelt.history` bindings - past submitted prompts.
//!   entries()      → array of strings (oldest first)
//!   search(query, limit?) → 1-based indices into entries(), best first.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "history",
        "Prompt history entries and search. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "entries",
        "Return the prompt history as an array of strings, oldest first. Mirrors what the up-arrow recall in the input bar walks through.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let entries =
                crate::lua::try_with_conversation_host(|host| host.prompt_history()).unwrap_or_default();
            let out = lua.create_table()?;
            for (i, entry) in entries.into_iter().enumerate() {
                out.set(i + 1, entry)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "search",
        "Search prompt history for `query`. Every query word must appear in the entry; whole-entry, prefix and word-start matches rank above substring matches, then fuzzy matches on the first line. Within a tier, matches near the start of the first line rank above matches further along it, then on later lines; newer entries break ties. Duplicates are dropped, and an empty query lists the newest entries. Returns up to `limit` (default: all) 1-based indices into `entries()`, best first.",
        &["query", "limit"],
        |_, (query, limit): (String, Option<usize>)| -> LuaResult<Vec<usize>> {
            let limit = limit.unwrap_or(usize::MAX);
            let matches = crate::lua::try_with_conversation_host(|host| {
                host.search_prompt_history(&query, limit)
            })
            .unwrap_or_default();
            Ok(matches.into_iter().map(|index| index + 1).collect())
        },
    )?;
    Ok(())
}
