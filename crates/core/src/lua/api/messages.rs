//! `smelt.messages` — persistent message log. Full bodies (with tracebacks) live here; toasts show only the first line.

use crate::lua::LuaShared;
use crate::messages::MessageKind;
use mlua::prelude::*;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let tbl = lua.create_table()?;

    let s = shared.clone();
    tbl.set(
        "list",
        lua.create_function(move |lua, ()| {
            let messages = s
                .messages
                .lock()
                .map_err(|e| LuaError::RuntimeError(format!("messages lock: {e}")))?;
            let out = lua.create_table()?;
            for (i, entry) in messages.entries().iter().enumerate() {
                let row = lua.create_table()?;
                row.set("kind", entry.kind.as_str())?;
                row.set("source", entry.source.clone())?;
                row.set("summary", entry.summary.clone())?;
                row.set("full", entry.full.clone())?;
                row.set("ts_ms", system_time_to_ms(entry.ts))?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;

    let s = shared.clone();
    tbl.set(
        "count",
        lua.create_function(move |_, ()| Ok(s.messages.lock().map(|m| m.count()).unwrap_or(0)))?,
    )?;

    let s = shared.clone();
    tbl.set(
        "unread_count",
        lua.create_function(move |_, ()| {
            Ok(s.messages.lock().map(|m| m.unread_errors()).unwrap_or(0))
        })?,
    )?;

    let s = shared.clone();
    tbl.set(
        "mark_read",
        lua.create_function(move |_, ()| {
            if let Ok(mut m) = s.messages.lock() {
                m.mark_read();
            }
            Ok(())
        })?,
    )?;

    let s = shared.clone();
    tbl.set(
        "clear",
        lua.create_function(move |_, ()| {
            if let Ok(mut m) = s.messages.lock() {
                m.clear();
            }
            Ok(())
        })?,
    )?;

    let s = shared.clone();
    tbl.set(
        "append",
        lua.create_function(move |_, (kind, source, msg): (String, String, String)| {
            let kind = match kind.as_str() {
                "error" => MessageKind::Error,
                "warn" | "warning" => MessageKind::Warning,
                _ => MessageKind::Info,
            };
            if let Ok(mut m) = s.messages.lock() {
                m.append(kind, source, msg);
            }
            Ok(())
        })?,
    )?;

    smelt.set("messages", tbl)?;
    Ok(())
}

fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
