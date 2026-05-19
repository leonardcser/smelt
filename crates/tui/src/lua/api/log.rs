//! `smelt.log` — structured JSONL log entries written to the rotating
//! engine log. Lua callers use this to emit machine-readable telemetry
//! events (e.g. compaction summaries) that complement `smelt.notify`
//! toasts. UiHost-only.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

fn emit(lua: &Lua, level: engine::log::Level, event: &str, data: Option<mlua::Value>) {
    let payload: serde_json::Value = match data {
        Some(v) => smelt_core::lua::lua_to_serde(lua, &v)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        None => serde_json::Value::Object(Default::default()),
    };
    engine::log::entry(level, event, &payload);
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "log",
        "Structured JSONL log entries written to the engine log file. Use for machine-readable telemetry events; pair with `smelt.notify` for user-visible toasts.",
        Tier::UiHost,
    )?;
    m.fn_(
        "info",
        "Write a JSONL log entry at Info level. `event` is a short stable name; `data` is an optional table serialized into the entry body.",
        &["event", "data"],
        |lua, (event, data): (String, Option<mlua::Value>)| -> LuaResult<()> {
            emit(lua, engine::log::Level::Info, &event, data);
            Ok(())
        },
    )?;
    m.fn_(
        "warn",
        "Write a JSONL log entry at Warn level.",
        &["event", "data"],
        |lua, (event, data): (String, Option<mlua::Value>)| -> LuaResult<()> {
            emit(lua, engine::log::Level::Warn, &event, data);
            Ok(())
        },
    )?;
    m.fn_(
        "error",
        "Write a JSONL log entry at Error level.",
        &["event", "data"],
        |lua, (event, data): (String, Option<mlua::Value>)| -> LuaResult<()> {
            emit(lua, engine::log::Level::Error, &event, data);
            Ok(())
        },
    )?;
    Ok(())
}
