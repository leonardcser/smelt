//! `smelt.fuzzy` - score / rank candidates via neo_frizbee.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "fuzzy",
        "Fuzzy-match scoring backed by neo_frizbee (SIMD Smith-Waterman).",
        Tier::Host,
    )?;
    m.fn_(
        "score",
        "Fuzzy-match score for `text` against `query`. Lower = better; `nil` = no match.",
        &["text", "query"],
        |_, (text, query): (String, String)| match crate::fuzzy::fuzzy_score(&text, &query) {
            Some(s) => Ok(Some(s)),
            None => Ok(None),
        },
    )?;

    m.fn_(
        "rank",
        "Rank `items` by fuzzy match against `query`. Returns 1-based indices, best first. Empty query → identity order. Items may be strings or tables with `label`/`description`/`search_terms`; set `_hay` to a precomputed concatenated haystack to skip per-call concatenation.",
        &["items", "query"],
        |_, (items, query): (mlua::Table, String)| -> LuaResult<Vec<usize>> {
            let len = items.raw_len();
            if query.is_empty() {
                return Ok((1..=len).collect());
            }
            let mut haystacks: Vec<String> = Vec::with_capacity(len);
            let mut original_idx: Vec<usize> = Vec::with_capacity(len);
            for i in 1..=len {
                let val: mlua::Value = items.raw_get(i)?;
                let hay = match val {
                    mlua::Value::String(s) => s.to_str()?.to_owned(),
                    mlua::Value::Table(t) => {
                        if let Ok(h) = t.get::<String>("_hay") {
                            h
                        } else {
                            let label: String = t.get("label").unwrap_or_default();
                            let desc: String = t.get("description").unwrap_or_default();
                            let terms: String = t.get("search_terms").unwrap_or_default();
                            let mut s = String::with_capacity(
                                label.len() + desc.len() + terms.len() + 2,
                            );
                            s.push_str(&label);
                            s.push(' ');
                            s.push_str(&desc);
                            s.push(' ');
                            s.push_str(&terms);
                            s
                        }
                    }
                    _ => continue,
                };
                haystacks.push(hay);
                original_idx.push(i);
            }
            Ok(crate::fuzzy::fuzzy_rank(&query, &haystacks)
                .into_iter()
                .map(|i| original_idx[i])
                .collect())
        },
    )?;

    Ok(())
}
