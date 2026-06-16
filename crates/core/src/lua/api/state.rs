//! `smelt.state` - JSON-backed per-plugin persistence. The ephemeral
//! `smelt.state(name)` lookup (per-reload table) lives in bundled lua;
//! this module supplies the file I/O primitives that
//! `smelt.state.persistent(name)` builds on.

use crate::config;
use crate::lua::api::{lua_table_to_json, lua_value_to_json};
use crate::lua::doc::Tier;
use crate::lua::json_to_lua;
use crate::lua::module::LuaMod;
use mlua::prelude::*;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_STATE_TMP_ID: AtomicU64 = AtomicU64::new(0);

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
                other => lua_value_to_json(lua, other),
            };
            let serialized = serde_json::to_string(&json)
                .map_err(|e| LuaError::RuntimeError(format!("smelt.state.__save: {e}")))?;
            let path = state_path(&name);
            save_state_json(&path, &serialized)
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

fn save_state_json(path: &Path, serialized: &str) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return std::fs::write(path, serialized);
    };

    let mut last_err = None;
    for _ in 0..16 {
        std::fs::create_dir_all(parent)?;
        let tmp = state_tmp_path(path);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(serialized.as_bytes()) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                drop(file);
                match std::fs::rename(&tmp, path) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp);
                        if e.kind() != std::io::ErrorKind::NotFound {
                            return Err(e);
                        }
                        last_err = Some(e);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "failed to allocate state temp file",
        )
    }))
}

fn state_tmp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_else(|| "state.json".into());
    let id = NEXT_STATE_TMP_ID.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_tmp_paths_are_unique_and_next_to_target() {
        let path = PathBuf::from("/tmp/smelt-state/plugins/foo.json");
        let a = state_tmp_path(&path);
        let b = state_tmp_path(&path);
        assert_ne!(a, b);
        assert_eq!(a.parent(), path.parent());
        assert_eq!(b.parent(), path.parent());
    }

    #[test]
    fn save_state_json_replaces_target_without_fixed_tmp_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins").join("upgrade.json");

        save_state_json(&path, "{\"version\":1}").unwrap();
        save_state_json(&path, "{\"version\":2}").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"version\":2}");
        assert!(!path.with_extension("json.tmp").exists());
    }
}
