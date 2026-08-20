//! `smelt.state` - JSON-backed per-plugin persistence. The ephemeral
//! `smelt.state.get(name)` lookup (per-reload table) lives in bundled lua;
//! this module supplies the file I/O primitives that
//! `smelt.state.persistent(name)` builds on.

use crate::lua::api::{lua_table_to_json, lua_value_to_json};
use crate::lua::doc::Tier;
use crate::lua::json_to_lua;
use crate::lua::module::LuaMod;
use mlua::prelude::*;
use std::path::{Path, PathBuf};

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, state_root: &Path) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "state",
        "Per-plugin state. `smelt.state.get(name)` returns an ephemeral table that survives `/reload` only; `smelt.state.persistent(name)` returns a JSON-backed wrapper that survives restarts too.",
        Tier::Host,
    )?;
    let plugin_state_dir = state_root.join("plugins");
    let load_state_dir = plugin_state_dir.clone();
    m.private_fn(
        "__load",
        &["name"],
        move |lua, name: String| -> LuaResult<mlua::Table> {
            let path = state_path(&load_state_dir, &name);
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
        move |lua, (name, value): (String, mlua::Value)| -> LuaResult<()> {
            let json = match &value {
                mlua::Value::Table(t) => lua_table_to_json(lua, t),
                other => lua_value_to_json(lua, other),
            };
            let serialized = serde_json::to_string(&json)
                .map_err(|e| LuaError::RuntimeError(format!("smelt.state.__save: {e}")))?;
            let path = state_path(&plugin_state_dir, &name);
            save_state_json(&path, &serialized)
                .map_err(|e| LuaError::RuntimeError(format!("smelt.state.__save: {e}")))?;
            Ok(())
        },
    )?;
    Ok(())
}

fn state_path(plugin_state_dir: &Path, name: &str) -> PathBuf {
    plugin_state_dir.join(format!("{name}.json"))
}

fn save_state_json(path: &Path, serialized: &str) -> std::io::Result<()> {
    crate::fs::write_atomic(path, serialized.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_state_json_replaces_target_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins").join("upgrade.json");

        save_state_json(&path, "{\"version\":1}").unwrap();
        save_state_json(&path, "{\"version\":2}").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"version\":2}");
        assert!(!path.with_extension("json.tmp").exists());
    }
}
