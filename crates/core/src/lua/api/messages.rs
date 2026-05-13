//! `smelt.messages` — persistent message log. Full bodies (with tracebacks) live here; toasts show only the first line.

use crate::lua::doc::{record_module_doc, register_fn};
use crate::lua::LuaShared;
use crate::messages::MessageKind;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let tbl = lua.create_table()?;
    record_module_doc(
        "smelt.messages",
        "Persistent message log with full bodies and tracebacks.",
    );

    let s = shared.clone();
    register_fn(
        &tbl,
        "smelt.messages",
        "list",
        "Return every persisted message as rows of `{ kind, source, summary, full, ts_ms }`, ordered oldest-first.",
        &[],
        lua,
        move |lua, ()|  -> LuaResult<mlua::Table>{
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
        },
    )?;

    {
        let s = shared.clone();
        register_fn(
            &tbl,
            "smelt.messages",
            "count",
            "Return the total number of messages currently in the log.",
            &[],
            lua,
            move |_, ()| Ok(s.messages.lock().map(|m| m.count()).unwrap_or(0)),
        )?;
    }

    let s = shared.clone();
    register_fn(
        &tbl,
        "smelt.messages",
        "unread_count",
        "Return the number of unread error messages in the log.",
        &[],
        lua,
        move |_, ()| Ok(s.messages.lock().map(|m| m.unread_errors()).unwrap_or(0)),
    )?;

    let s = shared.clone();
    register_fn(
        &tbl,
        "smelt.messages",
        "mark_read",
        "Mark every message in the log as read so `unread_count` returns `0` until new errors arrive.",
        &[],
        lua,
        move |_, ()|  -> LuaResult<()>{
            if let Ok(mut m) = s.messages.lock() {
                m.mark_read();
            }
            Ok(())
        },
    )?;

    let s = shared.clone();
    register_fn(
        &tbl,
        "smelt.messages",
        "clear",
        "Drop every message from the log.",
        &[],
        lua,
        move |_, ()| -> LuaResult<()> {
            if let Ok(mut m) = s.messages.lock() {
                m.clear();
            }
            Ok(())
        },
    )?;

    let s = shared.clone();
    register_fn(
        &tbl,
        "smelt.messages",
        "append",
        "Append a new message of `kind` (`\"error\"`, `\"warn\"`/`\"warning\"`, anything else falls back to `\"info\"`) attributed to `source` with body `msg`.",
        &["kind", "source", "msg"],
        lua,
        move |_, (kind, source, msg): (String, String, String)|  -> LuaResult<()>{
            let kind = match kind.as_str() {
                "error" => MessageKind::Error,
                "warn" | "warning" => MessageKind::Warning,
                _ => MessageKind::Info,
            };
            if let Ok(mut m) = s.messages.lock() {
                m.append(kind, source, msg);
            }
            Ok(())
        },
    )?;

    smelt.set("messages", tbl)?;
    Ok(())
}

fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
