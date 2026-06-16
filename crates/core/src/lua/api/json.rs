//! `smelt.json` - JSON encode/decode helpers for Lua plugins.

use crate::lua::api::lua_value_to_json;
use crate::lua::doc::Tier;
use crate::lua::json_to_lua;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "json",
        "Encode/decode JSON for Lua plugins. Tables with contiguous 1..N integer keys encode as arrays; other tables encode as objects.",
        Tier::Host,
    )?;

    m.fn_(
        "encode",
        "Encode a Lua value as JSON. Pass `{ pretty = true }` to format with indentation. Tables with contiguous 1..N integer keys encode as arrays; other tables encode as objects.",
        &["value", "opts"],
        |lua, (value, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<String> {
            let json = lua_value_to_json(lua, &value);
            let pretty = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("pretty").ok().flatten())
                .unwrap_or(false);
            if pretty {
                serde_json::to_string_pretty(&json).map_err(mlua::Error::external)
            } else {
                serde_json::to_string(&json).map_err(mlua::Error::external)
            }
        },
    )?;

    m.fn_(
        "decode",
        "Decode JSON into a Lua value. Returns `(value, nil)` on success or `(nil, err_string)` on failure.",
        &["text"],
        |lua, text: String| -> LuaResult<(mlua::Value, Option<String>)> {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(json) => Ok((json_to_lua(lua, &json)?, None)),
                Err(err) => Ok((mlua::Value::Nil, Some(err.to_string()))),
            }
        },
    )?;

    Ok(())
}
