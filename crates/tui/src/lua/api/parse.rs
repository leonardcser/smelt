//! `smelt.parse` bindings — pure parsers exposed to Lua. Today carries
//! `frontmatter(content) -> table | nil, body`; markdown / diff /
//! syntax block parsers ride P4.b's transcript pipeline migration onto
//! `BufferParser`.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let parse_tbl = lua.create_table()?;

    // smelt.parse.frontmatter(content) -> (table | nil, body).
    // Splits a leading `---\n…\n---\n` YAML block off `content` and
    // returns it as a Lua table plus the remaining body. Returns
    // `(nil, content)` when no frontmatter is present.
    parse_tbl.set(
        "frontmatter",
        lua.create_function(|lua, content: String| {
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
            let value: serde_json::Value =
                serde_yml::from_str(yaml).unwrap_or(serde_json::Value::Null);
            let lua_value = json_to_lua(lua, value)?;
            Ok((lua_value, body))
        })?,
    )?;

    smelt.set("parse", parse_tbl)?;
    Ok(())
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
