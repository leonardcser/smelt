//! `smelt.state` — JSON-backed per-plugin persistence. The ephemeral
//! `smelt.state(name)` lookup (per-reload table) lives in bundled lua;
//! this module supplies the file I/O primitives that
//! `smelt.state.persistent(name)` builds on.

use crate::config;
use crate::lua::api::lua_table_to_json;
use crate::lua::doc::Tier;
use crate::lua::json_to_lua;
use crate::lua::module::LuaMod;
use mlua::prelude::*;
use std::path::PathBuf;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "state",
        "Per-plugin state. `smelt.state(name)` returns an ephemeral table that survives `/reload` only; `smelt.state.persistent(name)` returns a JSON-backed wrapper that survives restarts too.",
        Tier::Host,
    )?;
    m.private_fn(
        "__load",
        &["name"],
        |lua, name: String| -> LuaResult<mlua::Table> {
            let path = state_path(&name);
            let Ok(raw) = std::fs::read_to_string(&path) else {
                return lua.create_table();
            };
            let json: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or(serde_json::Value::Object(Default::default()));
            match json_to_lua(lua, &json)? {
                mlua::Value::Table(t) => Ok(t),
                _ => lua.create_table(),
            }
        },
    )?;
    m.private_fn(
        "__save",
        &["name", "value"],
        |lua, (name, value): (String, mlua::Value)| -> LuaResult<()> {
            let json = match &value {
                mlua::Value::Table(t) => lua_table_to_json(lua, t),
                other => lua_value_to_json_pub(lua, other),
            };
            let serialized = serde_json::to_string(&json)
                .map_err(|e| LuaError::RuntimeError(format!("smelt.state.__save: {e}")))?;
            let path = state_path(&name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| LuaError::RuntimeError(format!("smelt.state.__save: {e}")))?;
            }
            // Atomic write: tmp + rename so a crash mid-write doesn't
            // corrupt the file.
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serialized)
                .map_err(|e| LuaError::RuntimeError(format!("smelt.state.__save: {e}")))?;
            std::fs::rename(&tmp, &path)
                .map_err(|e| LuaError::RuntimeError(format!("smelt.state.__save: {e}")))?;
            Ok(())
        },
    )?;
    Ok(())
}

fn state_path(name: &str) -> PathBuf {
    config::state_dir()
        .join("plugins")
        .join(format!("{name}.json"))
}

/// Wrapper so we can call the private `lua_value_to_json` from a sibling
/// module. Mirrors `lua_table_to_json`'s public path.
fn lua_value_to_json_pub(lua: &Lua, value: &mlua::Value) -> serde_json::Value {
    // Same logic as `lua_value_to_json` in the parent module — duplicated
    // here because that one is private.
    match value {
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(*b),
        mlua::Value::Integer(i) => serde_json::json!(*i),
        mlua::Value::Number(n) => serde_json::json!(*n),
        mlua::Value::String(s) => serde_json::Value::String(s.to_string_lossy().to_string()),
        mlua::Value::Table(t) => lua_table_to_json(lua, t),
        _ => serde_json::Value::Null,
    }
}
