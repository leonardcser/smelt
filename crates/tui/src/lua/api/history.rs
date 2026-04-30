//! `smelt.history` bindings — past submitted prompts.
//!   entries()      → array of strings (oldest first)
//!   search(query)  → [{index, score}] ranked by the
//!                    history-specific scorer (word-match boosts,
//!                    recency bonus). 1-based index into entries().

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let history_tbl = lua.create_table()?;
    history_tbl.set(
        "entries",
        lua.create_function(|lua, ()| {
            let entries = crate::lua::try_with_app(|app| app.input_history.entries().to_vec())
                .unwrap_or_default();
            let out = lua.create_table()?;
            for (i, entry) in entries.into_iter().enumerate() {
                out.set(i + 1, entry)?;
            }
            Ok(out)
        })?,
    )?;
    history_tbl.set(
        "search",
        lua.create_function(|lua, query: String| {
            let entries = crate::lua::try_with_app(|app| app.input_history.entries().to_vec())
                .unwrap_or_default();
            // Oldest first in the vec; the scorer wants newest-first
            // so "recent" ranks highest. Iterate reversed and dedupe
            // to match the old `Completer::history` construction.
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
        })?,
    )?;
    smelt.set("history", history_tbl)?;
    Ok(())
}
