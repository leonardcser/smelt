//! `smelt.history` bindings — past submitted prompts.
//!   entries()      → array of strings (oldest first)
//!   search(query)  → [{index, score}] ranked by the
//!                    history-specific scorer (word-match boosts,
//!                    recency bonus). 1-based index into entries().

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let history_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.history",
        "Prompt history entries and search. UiHost-only.",
    );

    register_ui_fn(
        &history_tbl,
        "smelt.history",
        "entries",
        "Return the prompt history as an array of strings, oldest first. Mirrors what the up-arrow recall in the input bar walks through.",
        &[],
        lua,
        |lua, ()|  -> LuaResult<mlua::Table>{
            let entries = crate::lua::try_with_app(|app| app.input_history.entries().to_vec())
                .unwrap_or_default();
            let out = lua.create_table()?;
            for (i, entry) in entries.into_iter().enumerate() {
                out.set(i + 1, entry)?;
            }
            Ok(out)
        },
    )?;
    register_ui_fn(
        &history_tbl,
        "smelt.history",
        "search",
        "Rank prompt history against `query` using the history-specific scorer (word-match boost, recency bonus, dedupe). Returns `{ index, score }` rows where `index` is 1-based into `entries()`.",
        &["query"],
        lua,
        |lua, query: String|  -> LuaResult<mlua::Table>{
            let entries = crate::lua::try_with_app(|app| app.input_history.entries().to_vec())
                .unwrap_or_default();
            // Entries are oldest-first; iterate reversed and dedupe so recent ranks highest.
            let mut seen = std::collections::HashSet::new();
            let mut scored: Vec<(u32, usize, usize)> = Vec::new();
            for (rank, (orig_idx, entry)) in entries.iter().enumerate().rev().enumerate() {
                if !seen.insert(entry.as_str()) {
                    continue;
                }
                let label = entry
                    .trim_start()
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                if let Some(s) = crate::completer::history::history_score(label, &query, rank) {
                    scored.push((s, rank, orig_idx));
                }
            }
            scored.sort_by_key(|(s, rank, _)| (*s, *rank));
            let out = lua.create_table()?;
            for (i, (score, _rank, orig_idx)) in scored.into_iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("index", orig_idx + 1)?;
                entry.set("score", score)?;
                out.set(i + 1, entry)?;
            }
            Ok(out)
        },
    )?;
    smelt.set("history", history_tbl)?;
    Ok(())
}
