//! Host-tier Lua API bindings (TUI and headless).

#[macro_export]
macro_rules! host_read {
    ($lua:expr, |$host:ident| $body:expr) => {{
        $lua.create_function(|_, ()| {
            Ok($crate::host::try_with_core(|$host| $body).unwrap_or_default())
        })?
    }};
}

mod builtins;
pub(crate) mod cell;
mod cli;
mod clipboard;
mod clock;
mod cmd;
mod defaults;
mod frontend;
mod fs;
mod fuzzy;
mod grep;
mod html;
mod http;
mod image;
pub mod layout;
mod lifecycle;
mod mcp;
mod messages;
pub mod mode;
mod os;
mod parse;
mod path;
mod perf;
mod phase;
mod process;
mod provider;
pub mod reasoning;
mod reg;
mod remember;
mod shell;
mod skills;
mod spawn;
mod state;
mod task;
mod timer;
mod tools;
mod trust;

use mlua::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

/// Convert a Lua table to a `serde_json::Value`. Tables with contiguous
/// 1..N integer keys become JSON arrays; anything else becomes an object.
pub fn lua_table_to_json(lua: &Lua, table: &mlua::Table) -> serde_json::Value {
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

    if is_array {
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

/// Convert a Lua table into a `HashMap<String, serde_json::Value>`
/// suitable for tool arguments.
pub fn lua_table_to_args(lua: &Lua, table: &mlua::Table) -> HashMap<String, serde_json::Value> {
    match lua_table_to_json(lua, table) {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => HashMap::new(),
    }
}

/// Register all Host-tier namespaces on the `smelt` table.
pub fn register_host_api(
    lua: &Lua,
    smelt: &mlua::Table,
    _smelt_keymap: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    crate::lua::reg::register_class_doc();
    builtins::register(lua, smelt, shared)?;
    cell::register(lua, smelt, shared)?;
    clock::register(lua, smelt)?;
    cli::register(lua, smelt, shared)?;
    clipboard::register(lua, smelt)?;
    cmd::register(lua, smelt, shared)?;
    defaults::register(lua, smelt, shared)?;
    frontend::register(lua, smelt)?;
    fs::register(lua, smelt, shared)?;
    fuzzy::register(lua, smelt)?;
    grep::register(lua, smelt, shared)?;
    html::register(lua, smelt)?;
    http::register(lua, smelt, shared)?;
    image::register(lua, smelt)?;
    layout::register(lua, smelt)?;
    lifecycle::register(lua, smelt, shared)?;
    mcp::register(lua, smelt, shared)?;
    messages::register(lua, smelt, shared)?;
    mode::register(lua, smelt)?;
    os::register(lua, smelt)?;
    reasoning::register(lua, smelt)?;
    reg::register(lua, smelt)?;
    remember::register(lua, smelt, shared)?;
    parse::register(lua, smelt)?;
    path::register(lua, smelt)?;
    perf::register(lua, smelt)?;
    phase::register(lua, smelt, shared)?;
    process::register(lua, smelt, shared)?;
    provider::register(lua, smelt, shared)?;
    shell::register(lua, smelt)?;
    skills::register(lua, smelt)?;
    spawn::register(lua, smelt, shared)?;
    state::register(lua, smelt)?;
    task::register(lua, smelt, shared)?;
    timer::register(lua, smelt)?;
    tools::register(lua, smelt, shared)?;
    trust::register(lua, smelt)?;
    Ok(())
}
