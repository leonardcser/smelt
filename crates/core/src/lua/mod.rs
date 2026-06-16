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
pub mod watchers;

pub use reg::LuaReg;

pub use hooks::{composite_reg, HookEntry, HookRegistry};
pub use runtime::{
    autoload_modules, autoload_modules_filtered, ensure_builtins_extracted, init_lua_path,
    load_bootstrap_chunks, LuaRuntime, OPTIONAL_PLUGINS,
};
pub use shared::{
    AskCallbacks, CliFlagKind, CliFlagSpec, CliFlagValue, DefaultShell, Hooks, LuaResumeSink,
    LuaShared, Phase, RegisteredCommand, RegisteredKeymap, ToolHandles, TranscriptGroupBucket,
    TranscriptGroupFieldMatch, TranscriptGroupSelector, TranscriptGroupSpec, LUA_BUF_ID_BASE,
};
pub(crate) use task::step_task_owned;
pub use task::{
    current_command_queue_target, current_task_cancel, current_task_scope, with_task_cancel,
    CommandQueueTarget, LuaTaskRuntime, TaskCompletion, TaskDriveOutput, TaskEvent, TaskScope,
    ToolEnv,
};

/// Outcome of invoking a plugin tool handler.
pub enum ToolExecResult {
    /// Handler returned synchronously; forward content to the engine immediately.
    Immediate {
        content: String,
        is_error: bool,
        metadata: Option<serde_json::Value>,
    },
    /// Handler yielded; result arrives later via `drive_tasks() -> TaskDriveOutput::ToolComplete`.
    Pending,
}

use mlua::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// Global counters of `LuaHandle` lifecycle events. The pair is a
/// **drop-counter ledger**: every `from_func` increments `created`,
/// every `Drop` increments `dropped`. The difference is the net live
/// count of registry-backed callables.
///
/// Used by the fuzz harness as a leak oracle that survives refactors -
/// a per-field walk has to be updated by anyone adding a new
/// `LuaHandle` field. The drop counter has no such surface and catches
/// handles that don't live in any tracked field at all (e.g. a closure
/// that was stashed only in a Lua table).
static LUA_HANDLES_CREATED: AtomicU64 = AtomicU64::new(0);
static LUA_HANDLES_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Snapshot the `(created, dropped)` counters. Live = created - dropped.
pub fn lua_handle_inventory() -> (u64, u64) {
    (
        LUA_HANDLES_CREATED.load(Ordering::Relaxed),
        LUA_HANDLES_DROPPED.load(Ordering::Relaxed),
    )
}

/// Convenience: net live handle count, derived from [`lua_handle_inventory`].
pub fn lua_handles_live() -> u64 {
    let (c, d) = lua_handle_inventory();
    c.saturating_sub(d)
}

/// A Lua callable parked in the registry so it survives GC.
pub struct LuaHandle {
    pub key: mlua::RegistryKey,
}

impl LuaHandle {
    pub fn from_func(lua: &Lua, func: mlua::Function) -> LuaResult<Self> {
        let key = lua.create_registry_value(func)?;
        LUA_HANDLES_CREATED.fetch_add(1, Ordering::Relaxed);
        Ok(Self { key })
    }
}

impl Drop for LuaHandle {
    fn drop(&mut self) {
        LUA_HANDLES_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Serialize a `Serialize` value through JSON into a Lua value. Convenience
/// for crossing the engine↔Lua boundary without hand-rolling a per-type
/// converter - used by `host_dispatch` to ship `protocol::Message`
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

/// Decode a Lua value into one styled line.
pub(crate) fn styled_line_from_lua(
    value: mlua::Value,
    label: &str,
) -> LuaResult<Vec<protocol::StyledSpan>> {
    match value {
        mlua::Value::Nil => Ok(Vec::new()),
        mlua::Value::String(s) => Ok(vec![protocol::StyledSpan {
            text: s.to_string_lossy(),
            ..Default::default()
        }]),
        mlua::Value::Table(line_table) => {
            let mut spans = Vec::new();
            for span in line_table.sequence_values::<mlua::Value>() {
                spans.push(styled_span_from_lua(span?, label)?);
            }
            Ok(spans)
        }
        other => Err(mlua::Error::external(format!(
            "{label}: expected styled line, got {}",
            other.type_name()
        ))),
    }
}

/// Decode a Lua value into the styled-lines shape shared by tool summaries,
/// transcript blocks, and `smelt.layout.runs`.
pub(crate) fn styled_lines_from_lua(
    value: mlua::Value,
    label: &str,
) -> LuaResult<protocol::StyledLines> {
    use protocol::{StyledLines, StyledSpan};

    match value {
        mlua::Value::Nil => Ok(StyledLines::empty()),
        mlua::Value::String(s) => Ok(StyledLines::from_plain(s.to_string_lossy())),
        mlua::Value::Table(lines_table) => {
            let mut lines = Vec::new();
            for line in lines_table.sequence_values::<mlua::Value>() {
                let line = line?;
                match line {
                    mlua::Value::Nil => lines.push(Vec::new()),
                    mlua::Value::String(s) => lines.push(vec![StyledSpan {
                        text: s.to_string_lossy(),
                        ..Default::default()
                    }]),
                    mlua::Value::Table(t) => {
                        let mut spans = Vec::new();
                        for span in t.sequence_values::<mlua::Value>() {
                            spans.push(styled_span_from_lua(span?, label)?);
                        }
                        lines.push(spans);
                    }
                    other => {
                        return Err(mlua::Error::external(format!(
                            "{label}: expected line table or string, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
            Ok(StyledLines(lines))
        }
        other => Err(mlua::Error::external(format!(
            "{label}: expected styled lines, got {}",
            other.type_name()
        ))),
    }
}

fn styled_span_from_lua(span: mlua::Value, label: &str) -> LuaResult<protocol::StyledSpan> {
    use protocol::StyledSpan;

    let span_table = match span {
        mlua::Value::String(s) => {
            return Ok(StyledSpan {
                text: s.to_string_lossy(),
                ..Default::default()
            })
        }
        mlua::Value::Table(t) => t,
        other => {
            return Err(mlua::Error::external(format!(
                "{label}: expected span table or string, got {}",
                other.type_name()
            )))
        }
    };
    let style = span_table.get::<Option<mlua::Table>>("style")?;
    let mut out = StyledSpan {
        text: span_table
            .get::<Option<String>>("text")?
            .or_else(|| span_table.get::<Option<String>>(1).ok().flatten())
            .unwrap_or_default(),
        syntax: span_table.get::<Option<String>>("syntax")?,
        hl: span_table.get::<Option<String>>("hl")?,
        fg: span_table.get::<Option<String>>("fg")?,
        bg: span_table.get::<Option<String>>("bg")?,
        dim: span_table.get::<Option<bool>>("dim")?.unwrap_or(false),
        bold: span_table.get::<Option<bool>>("bold")?.unwrap_or(false),
        italic: span_table.get::<Option<bool>>("italic")?.unwrap_or(false),
        selectable: span_table
            .get::<Option<bool>>("selectable")?
            .unwrap_or(true),
        title_suffix: span_table
            .get::<Option<bool>>("title_suffix")?
            .unwrap_or(false),
    };
    if let Some(style) = style {
        out.hl = out.hl.or(style.get::<Option<String>>("hl")?);
        out.fg = out.fg.or(style.get::<Option<String>>("fg")?);
        out.bg = out.bg.or(style.get::<Option<String>>("bg")?);
        out.dim |= style.get::<Option<bool>>("dim")?.unwrap_or(false);
        out.bold |= style.get::<Option<bool>>("bold")?.unwrap_or(false);
        out.italic |= style.get::<Option<bool>>("italic")?.unwrap_or(false);
    }
    Ok(out)
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
