//! Lua runtime types for `smelt-core`.

pub mod api;
pub mod doc;
pub mod lua_type;
pub mod runtime;
mod shared;
mod task;

pub use runtime::{autoload_modules, LuaRuntime};
pub use shared::{LuaResumeSink, LuaShared, RegisteredCommand, StatusSource, ToolHandles};
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

/// A Lua callable stored as a `RegistryKey` so it survives GC cycles.
pub struct LuaHandle {
    pub key: mlua::RegistryKey,
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
