//! Lua runtime types for `smelt-core`.

pub mod api;
pub mod doc;
pub mod hooks;
pub mod lua_type;
pub mod module;
pub mod reg;
pub mod runtime;
mod shared;
mod task;

pub use reg::LuaReg;

pub use hooks::{composite_off, HookEntry, HookRegistry};
pub use runtime::{autoload_modules, autoload_modules_filtered, LuaRuntime};
pub use shared::{
    CliFlagKind, CliFlagSpec, CliFlagValue, DefaultShell, Hooks, LuaResumeSink, LuaShared, Phase,
    RegisteredCommand, StatusSource, ToolHandles, LUA_BUF_ID_BASE,
};
pub use task::{
    current_task_cancel, with_task_cancel, LuaTaskRuntime, TaskCompletion, TaskDriveOutput,
    TaskEvent, ToolEnv,
};

/// Outcome of invoking a plugin tool handler.
pub enum ToolExecResult {
    /// Handler returned synchronously; forward content to the engine immediately.
    Immediate { content: String, is_error: bool },
    /// Handler yielded; result arrives later via `drive_tasks() -> TaskDriveOutput::ToolComplete`.
    Pending,
}

use mlua::prelude::*;

/// A Lua callable parked in the registry so it survives GC.
pub struct LuaHandle {
    pub key: mlua::RegistryKey,
}

impl LuaHandle {
    pub fn from_func(lua: &Lua, func: mlua::Function) -> LuaResult<Self> {
        Ok(Self {
            key: lua.create_registry_value(func)?,
        })
    }
}

/// Serialize a `Serialize` value through JSON into a Lua value. Convenience
/// for crossing the engine↔Lua boundary without hand-rolling a per-type
/// converter — used by `host_dispatch` to ship `protocol::Message`
/// payloads to provider middleware hooks.
pub fn serde_to_lua<T: serde::Serialize>(lua: &Lua, value: &T) -> LuaResult<mlua::Value> {
    let json = serde_json::to_value(value).map_err(mlua::Error::external)?;
    json_to_lua(lua, &json)
}

/// Deserialize a Lua value into a `DeserializeOwned` Rust type via JSON.
/// Inverse of [`serde_to_lua`]. Returns `None` if either the Lua→JSON
/// conversion drops fields the deserializer requires, or the JSON
/// doesn't match the target shape. Callers treat `None` as "no
/// mutation" (the original payload stays in flight).
pub fn lua_to_serde<T: serde::de::DeserializeOwned>(lua: &Lua, value: &mlua::Value) -> Option<T> {
    let json = match value {
        mlua::Value::Table(t) => api::lua_table_to_json(lua, t),
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(*b),
        mlua::Value::Integer(i) => serde_json::json!(*i),
        mlua::Value::Number(n) => serde_json::json!(*n),
        mlua::Value::String(s) => serde_json::Value::String(s.to_string_lossy().to_string()),
        _ => return None,
    };
    serde_json::from_value(json).ok()
}

pub fn json_to_lua(lua: &Lua, v: &serde_json::Value) -> LuaResult<mlua::Value> {
    match v {
        serde_json::Value::Null => Ok(mlua::Value::Nil),
        serde_json::Value::Bool(b) => Ok(mlua::Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(mlua::Value::Integer(i))
            } else {
                Ok(mlua::Value::Number(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Ok(mlua::Value::String(lua.create_string(s)?)),
        serde_json::Value::Array(arr) => {
            let t = lua.create_table()?;
            for (i, elem) in arr.iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, elem)?)?;
            }
            Ok(mlua::Value::Table(t))
        }
        serde_json::Value::Object(map) => {
            let t = lua.create_table()?;
            for (k, val) in map {
                t.set(k.as_str(), json_to_lua(lua, val)?)?;
            }
            Ok(mlua::Value::Table(t))
        }
    }
}
