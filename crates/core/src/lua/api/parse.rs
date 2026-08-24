//! `smelt.parse` - pure parsers: `frontmatter(content) -> (table | nil, body)`.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "parse",
        "Pure parsers: frontmatter extraction from markdown documents.",
        Tier::Host,
    )?;
    m.fn_(
        "json",
        "Parse a JSON document into a Lua value. Returns the decoded value on success, `nil` on parse error. Objects become tables, arrays become 1-indexed tables, numbers become integers when they fit losslessly.",
        &["content"],
        |lua, content: String| -> LuaResult<mlua::Value> {
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(v) => json_to_lua(lua, v),
                Err(_) => Ok(mlua::Value::Nil),
            }
        },
    )?;
    m.fn_(
        "frontmatter",
        "Split `---`-delimited YAML frontmatter from a markdown document. Returns `(frontmatter_table, body)` or `(nil, content)` when no frontmatter is present.",
        &["content"],
        |lua, content: String| -> LuaResult<(mlua::Value, String)> {
            let Some(rest) = content.strip_prefix("---") else {
                return Ok((mlua::Value::Nil, content));
            };
            let Some(end) = rest.find("\n---") else {
                return Ok((mlua::Value::Nil, content));
            };
            let yaml = &rest[..end];
            let body_start = 3 + end + 4;
            let body = if body_start < content.len() {
                content[body_start..].to_string()
            } else {
                String::new()
            };
            let value = yaml_to_json(yaml);
            let lua_value = json_to_lua(lua, value)?;
            Ok((lua_value, body))
        },
    )?;

    Ok(())
}

fn yaml_to_json(yaml: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let mut current_key: Option<String> = None;
    let mut current_arr: Vec<serde_json::Value> = Vec::new();

    for line in yaml.lines() {
        let trimmed = smelt_buffer::text::trim_whitespace(line);
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = trimmed.split_once(':') {
            if let Some(k) = current_key.take() {
                map.insert(k, serde_json::Value::Array(current_arr.clone()));
                current_arr.clear();
            }
            let key = smelt_buffer::text::trim_whitespace(key).to_owned();
            let val = smelt_buffer::text::trim_whitespace(val);
            if val.is_empty() {
                current_key = Some(key);
            } else {
                map.insert(key, serde_json::Value::String(unquote(val)));
            }
        } else if let Some(rest) = trimmed.strip_prefix("-") {
            let val = smelt_buffer::text::trim_whitespace(rest);
            current_arr.push(serde_json::Value::String(unquote(val)));
        }
    }
    if let Some(k) = current_key.take() {
        map.insert(k, serde_json::Value::Array(current_arr));
    }
    serde_json::Value::Object(map)
}

fn unquote(s: &str) -> String {
    let mut graphemes = smelt_buffer::cell_width::grapheme_indices(s);
    let Some((_, first)) = graphemes.next() else {
        return String::new();
    };
    let Some((last_start, last)) = graphemes.next_back() else {
        return s.to_string();
    };
    let quote = first.chars().next().unwrap();
    if matches!(quote, '"' | '\'') && last.ends_with(quote) {
        return s[first.len()..last_start].to_string();
    }
    s.to_string()
}

fn json_to_lua(lua: &Lua, v: serde_json::Value) -> LuaResult<mlua::Value> {
    use serde_json::Value;
    match v {
        Value::Null => Ok(mlua::Value::Nil),
        Value::Bool(b) => Ok(mlua::Value::Boolean(b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(mlua::Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(mlua::Value::Number(f))
            } else {
                Ok(mlua::Value::Nil)
            }
        }
        Value::String(s) => Ok(mlua::Value::String(lua.create_string(&s)?)),
        Value::Array(arr) => {
            let t = lua.create_table()?;
            for (i, item) in arr.into_iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, item)?)?;
            }
            Ok(mlua::Value::Table(t))
        }
        Value::Object(map) => {
            let t = lua.create_table()?;
            for (k, item) in map {
                t.set(k, json_to_lua(lua, item)?)?;
            }
            Ok(mlua::Value::Table(t))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_values_keep_graphemes_atomic() {
        let parsed = yaml_to_json("key:  \u{301}value\u{600} ");
        assert_eq!(parsed["key"], " \u{301}value\u{600} ");
        assert_eq!(unquote("\"\u{301}value\u{600}\""), "value");
    }
}
