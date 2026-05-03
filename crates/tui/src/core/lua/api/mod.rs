//! Host-tier Lua API bindings — work in both Tui and headless contexts.
//! These bindings only access subsystems available through the `Host` trait.

mod au;
mod cell;
mod clipboard;
mod cmd;
mod engine;
mod frontend;
mod fs;
mod fuzzy;
mod grep;
mod history;
mod html;
mod http;
mod image;
mod keymap;
mod mcp;
mod metrics;
mod mode;
mod model;
mod os;
mod parse;
mod path;
mod permissions;
mod process;
mod provider;
mod reasoning;
mod session;
mod settings;
mod shell;
mod skills;
mod spawn;
mod task;
mod timer;
mod tools;
mod transcript;
mod vim;

use mlua::prelude::*;
use std::sync::Arc;

use crate::lua::LuaShared;

/// Register all Host-tier `smelt.*` namespaces.
pub(crate) fn register_host_api(
    lua: &Lua,
    smelt: &mlua::Table,
    smelt_keymap: &mlua::Table,
    shared: &Arc<LuaShared>,
) -> LuaResult<()> {
    au::register(lua, smelt)?;
    cell::register(lua, smelt)?;
    clipboard::register(lua, smelt)?;
    cmd::register(lua, smelt, shared)?;
    engine::register(lua, smelt, shared)?;
    frontend::register(lua, smelt)?;
    fs::register(lua, smelt)?;
    fuzzy::register(lua, smelt)?;
    grep::register(lua, smelt)?;
    history::register(lua, smelt)?;
    html::register(lua, smelt)?;
    http::register(lua, smelt)?;
    image::register(lua, smelt)?;
    keymap::register(lua, smelt_keymap, shared)?;
    mcp::register(lua, smelt, shared)?;
    metrics::register(lua, smelt)?;
    mode::register(lua, smelt)?;
    model::register(lua, smelt)?;
    os::register(lua, smelt)?;
    parse::register(lua, smelt)?;
    path::register(lua, smelt)?;
    permissions::register(lua, smelt, shared)?;
    process::register(lua, smelt)?;
    provider::register(lua, smelt, shared)?;
    reasoning::register(lua, smelt)?;
    session::register(lua, smelt)?;
    settings::register(lua, smelt, shared)?;
    shell::register(lua, smelt)?;
    skills::register(lua, smelt)?;
    spawn::register(lua, smelt, shared)?;
    task::register(lua, smelt, shared)?;
    timer::register(lua, smelt)?;
    tools::register(lua, smelt, shared)?;
    transcript::register(lua, smelt)?;
    vim::register(lua, smelt)?;

    Ok(())
}

/// Register a 0-arg getter that reads live state from `TuiApp` via
/// `try_with_app`. Replaces the old snapshot-mirror pattern — every
/// read goes through the TLS pointer installed at the top of each
/// tick / Lua-entry boundary.
///
/// Reads use `try_with_app` (not `with_app`) so callers from a context
/// without `install_app_ptr` get the type's `Default` instead of a
/// panic. In production every Lua-entry path installs the pointer, so
/// the fallback is dead; tests that exercise bindings without a `TuiApp`
/// get empty/zeroed values rather than panics.
macro_rules! app_read {
    ($lua:expr, |$app:ident| $body:expr) => {{
        $lua.create_function(
            |_, ()| Ok(crate::lua::try_with_app(|$app| $body).unwrap_or_default()),
        )?
    }};
}
pub(crate) use app_read;

// ── shared JSON helpers ────────────────────────────────────────────────

/// Convert a Lua table to a `serde_json::Value`. Tables with contiguous
/// 1..N integer keys become JSON arrays; anything else becomes an object.
pub(crate) fn lua_table_to_json(lua: &Lua, table: &mlua::Table) -> serde_json::Value {
    let mut pairs: Vec<(mlua::Value, mlua::Value)> = Vec::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        let Ok(kv) = pair else { continue };
        pairs.push(kv);
    }

    let is_array = !pairs.is_empty()
        && pairs
            .iter()
            .all(|(k, _)| matches!(k, mlua::Value::Integer(_)))
        && {
            let mut ints: Vec<i64> = pairs
                .iter()
                .filter_map(|(k, _)| match k {
                    mlua::Value::Integer(i) => Some(*i),
                    _ => None,
                })
                .collect();
            ints.sort_unstable();
            ints.first().copied() == Some(1) && ints.windows(2).all(|w| w[1] == w[0] + 1)
        };

    if is_array || pairs.is_empty() {
        let len = table.raw_len();
        let mut arr = Vec::with_capacity(len);
        for i in 1..=len {
            let val: mlua::Value = table.raw_get(i).unwrap_or(mlua::Value::Nil);
            arr.push(lua_value_to_json(lua, &val));
        }
        serde_json::Value::Array(arr)
    } else {
        let mut map = serde_json::Map::new();
        for (key, val) in pairs {
            let key_str = match &key {
                mlua::Value::String(s) => s.to_string_lossy().to_string(),
                mlua::Value::Integer(i) => i.to_string(),
                _ => continue,
            };
            map.insert(key_str, lua_value_to_json(lua, &val));
        }
        serde_json::Value::Object(map)
    }
}

fn lua_value_to_json(lua: &Lua, val: &mlua::Value) -> serde_json::Value {
    match val {
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(*b),
        mlua::Value::Integer(i) => serde_json::json!(*i),
        mlua::Value::Number(n) => serde_json::json!(*n),
        mlua::Value::String(s) => serde_json::Value::String(s.to_string_lossy().to_string()),
        mlua::Value::Table(t) => lua_table_to_json(lua, t),
        _ => serde_json::Value::Null,
    }
}

/// Convert a `serde_json::Value` into a Lua value. Objects become
/// tables keyed by string; arrays become 1-indexed sequences. Used by
/// FFI bindings that surface JSON-shaped metadata back to Lua.
pub(crate) fn json_to_lua_value(lua: &Lua, value: &serde_json::Value) -> LuaResult<mlua::Value> {
    use serde_json::Value as J;
    Ok(match value {
        J::Null => mlua::Value::Nil,
        J::Bool(b) => mlua::Value::Boolean(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                mlua::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                mlua::Value::Number(f)
            } else {
                mlua::Value::Nil
            }
        }
        J::String(s) => mlua::Value::String(lua.create_string(s)?),
        J::Array(arr) => {
            let t = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                t.set(i + 1, json_to_lua_value(lua, v)?)?;
            }
            mlua::Value::Table(t)
        }
        J::Object(obj) => {
            let t = lua.create_table()?;
            for (k, v) in obj {
                t.set(k.as_str(), json_to_lua_value(lua, v)?)?;
            }
            mlua::Value::Table(t)
        }
    })
}

/// Treat a Lua table as a `{ string => json }` arg map, the shape every
/// tool call accepts. Skips non-string keys.
pub(crate) fn lua_table_to_args(
    lua: &Lua,
    table: &mlua::Table,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut out = std::collections::HashMap::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>().flatten() {
        let (k, v) = pair;
        let key = match k {
            mlua::Value::String(s) => s.to_string_lossy().to_string(),
            _ => continue,
        };
        out.insert(key, lua_value_to_json(lua, &v));
    }
    out
}
