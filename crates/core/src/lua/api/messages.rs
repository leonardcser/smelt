//! `smelt.messages` - persistent message log. Full bodies (with tracebacks) live here; toasts show only the first line.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use crate::messages::MessageKind;
use mlua::prelude::*;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "messages",
        "Persistent message log with full bodies and tracebacks.",
        Tier::Host,
    )?;
    let s = shared.clone();
    m.fn_(
        "list",
        "Return every persisted message as rows of `{ kind, source, summary, full, ts_ms }`, ordered oldest-first.",
        &[],
        move |lua, ()| -> LuaResult<mlua::Table> {
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
        m.fn_(
            "count",
            "Return the total number of messages currently in the log.",
            &[],
            move |_, ()| Ok(s.messages.lock().map(|m| m.count()).unwrap_or(0)),
        )?;
    }

    let s = shared.clone();
    m.fn_(
        "unread_count",
        "Return the number of unread error messages in the log.",
        &[],
        move |_, ()| Ok(s.messages.lock().map(|m| m.unread_errors()).unwrap_or(0)),
    )?;

    let s = shared.clone();
    m.fn_(
        "mark_read",
        "Mark every message in the log as read so `unread_count` returns `0` until new errors arrive.",
        &[],
        move |_, ()| -> LuaResult<()> {
            if let Ok(mut m) = s.messages.lock() {
                m.mark_read();
            }
            Ok(())
        },
    )?;

    let s = shared.clone();
    m.fn_(
        "clear",
        "Drop every message from the log.",
        &[],
        move |_, ()| -> LuaResult<()> {
            if let Ok(mut m) = s.messages.lock() {
                m.clear();
            }
            Ok(())
        },
    )?;

    let s = shared.clone();
    m.fn_(
        "append",
        "Append a new message of `kind` (`\"error\"`, `\"warn\"`/`\"warning\"`, anything else falls back to `\"info\"`) attributed to `source` with body `msg`.",
        &["kind", "source", "msg"],
        move |_, (kind, source, msg): (String, String, String)| -> LuaResult<()> {
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

    Ok(())
}

fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
