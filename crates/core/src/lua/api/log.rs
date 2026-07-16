//! `smelt.log` - structured JSONL log entries written to the rotating
//! engine log. Lua callers use this to emit machine-readable telemetry.
//! Host-tier so bundled runtime code can log without a terminal UI.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

use crate::lua::LuaShared;

fn emit(
    lua: &Lua,
    shared: &LuaShared,
    level: engine::log::Level,
    event: String,
    data: Option<mlua::Value>,
) {
    let payload: serde_json::Value = match data {
        Some(v) => smelt_core::lua::lua_to_serde(lua, &v)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        None => serde_json::Value::Object(Default::default()),
    };
    shared.log_entry(level, event, payload);
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "log",
        "Structured JSONL log entries written to the engine log file. Use for machine-readable telemetry events; pair with `smelt.notify` for user-visible toasts.",
        Tier::Host,
    )?;
    let info_logs = Arc::clone(shared);
    m.fn_(
        "info",
        "Write a JSONL log entry at Info level. `event` is a short stable name; `data` is an optional table serialized into the entry body.",
        &["event", "data"],
        move |lua, (event, data): (String, Option<mlua::Value>)| -> LuaResult<()> {
            emit(lua, &info_logs, engine::log::Level::Info, event, data);
            Ok(())
        },
    )?;
    let warn_logs = Arc::clone(shared);
    m.fn_(
        "warn",
        "Write a JSONL log entry at Warn level.",
        &["event", "data"],
        move |lua, (event, data): (String, Option<mlua::Value>)| -> LuaResult<()> {
            emit(lua, &warn_logs, engine::log::Level::Warn, event, data);
            Ok(())
        },
    )?;
    let error_logs = Arc::clone(shared);
    m.fn_(
        "error",
        "Write a JSONL log entry at Error level.",
        &["event", "data"],
        move |lua, (event, data): (String, Option<mlua::Value>)| -> LuaResult<()> {
            emit(lua, &error_logs, engine::log::Level::Error, event, data);
            Ok(())
        },
    )?;
    Ok(())
}
