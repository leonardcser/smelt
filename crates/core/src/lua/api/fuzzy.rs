//! `smelt.fuzzy` - score / rank candidates via neo_frizbee.

use crate::fuzzy::CaseMatching;
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
        "Fuzzy-match score for `text` against `query`. Lower = better; `nil` = no match. `opts.casing` is `\"smart\"` (default: case-insensitive until `query` contains an uppercase letter), `\"ignore\"` or `\"respect\"`.",
        &["text", "query", "opts"],
        |_, (text, query, opts): (String, String, Option<mlua::Table>)| {
            Ok(crate::fuzzy::fuzzy_score(&text, &query, casing(opts.as_ref())?))
        },
    )?;

    m.fn_(
        "rank",
        "Rank `items` by fuzzy match against `query`. Returns 1-based indices, best first. Empty query → identity order. Items may be strings or tables with `label`/`description`/`search_terms`; set `_hay` to a precomputed concatenated haystack to skip per-call concatenation. `opts.casing` works as in `smelt.fuzzy.score`.",
        &["items", "query", "opts"],
        |_, (items, query, opts): (mlua::Table, String, Option<mlua::Table>)| -> LuaResult<Vec<usize>> {
            let casing = casing(opts.as_ref())?;
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
            Ok(crate::fuzzy::fuzzy_rank(&query, &haystacks, casing)
                .into_iter()
                .map(|i| original_idx[i])
                .collect())
        },
    )?;

    Ok(())
}

fn casing(opts: Option<&mlua::Table>) -> LuaResult<CaseMatching> {
    let Some(name) = opts
        .map(|t| t.get::<Option<String>>("casing"))
        .transpose()?
        .flatten()
    else {
        return Ok(CaseMatching::Smart);
    };
    match name.as_str() {
        "smart" => Ok(CaseMatching::Smart),
        "ignore" => Ok(CaseMatching::Ignore),
        "respect" => Ok(CaseMatching::Respect),
        other => Err(LuaError::runtime(format!(
            "smelt.fuzzy: invalid casing {other:?}, expected \"smart\", \"ignore\" or \"respect\""
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(lua: &Lua, casing: &str) -> mlua::Table {
        let table = lua.create_table().unwrap();
        table.set("casing", casing).unwrap();
        table
    }

    #[test]
    fn casing_defaults_to_smart() {
        let lua = Lua::new();
        assert_eq!(casing(None).unwrap(), CaseMatching::Smart);
        assert_eq!(
            casing(Some(&lua.create_table().unwrap())).unwrap(),
            CaseMatching::Smart
        );
    }

    #[test]
    fn casing_parses_every_mode() {
        let lua = Lua::new();
        assert_eq!(
            casing(Some(&opts(&lua, "smart"))).unwrap(),
            CaseMatching::Smart
        );
        assert_eq!(
            casing(Some(&opts(&lua, "ignore"))).unwrap(),
            CaseMatching::Ignore
        );
        assert_eq!(
            casing(Some(&opts(&lua, "respect"))).unwrap(),
            CaseMatching::Respect
        );
    }

    #[test]
    fn casing_rejects_unknown_modes() {
        let lua = Lua::new();
        let err = casing(Some(&opts(&lua, "loud"))).unwrap_err().to_string();
        assert!(err.contains("invalid casing \"loud\""), "{err}");
    }
}
