//! `smelt.state` - JSON-backed per-plugin persistence. The ephemeral
//! `smelt.state.get(name)` lookup (per-reload table) lives in bundled lua;
//! this module supplies the file I/O primitives that
//! `smelt.state.persistent(name)` builds on.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::value;
use mlua::prelude::*;
use std::path::{Path, PathBuf};

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, state_root: &Path) -> LuaResult<()> {
    let m = LuaMod::supported(
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
            match value::from_json(lua, &json)? {
                mlua::Value::Table(t) => Ok(t),
                _ => lua.create_table(),
            }
        },
    )?;
    m.private_live_only_fn(
        "__save",
        &["name", "value"],
        move |lua, (name, state): (String, mlua::Value)| -> LuaResult<()> {
            let json = value::to_json(lua, &state)?;
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
