//! `smelt.time` - wall-clock time primitives.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use chrono::{Local, TimeZone, Utc};
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "time",
        "Wall-clock time parsing and formatting. Host-tier so plugins can render provider timestamps consistently in both TUI and headless contexts.",
        Tier::Host,
    )?;
    m.fn_(
        "now",
        "Return the current Unix timestamp in seconds. Backed by the host clock so tests can freeze time by swapping in a virtual clock.",
        &[],
        |_, ()| {
            let seconds = crate::host::try_with_core(|core| {
                core.clock
                    .system_now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            })
            .unwrap_or(0);
            Ok(seconds)
        },
    )?;
    m.fn_(
        "now_ms",
        "Return the current Unix timestamp in milliseconds. Backed by the host clock so tests can freeze time by swapping in a virtual clock.",
        &[],
        |_, ()| {
            let ms = crate::host::try_with_core(|core| {
                engine::clock::unix_time_ms(core.clock.as_ref())
            })
            .unwrap_or(0);
            Ok(ms)
        },
    )?;
    m.fn_(
        "parse_iso8601",
        "Parse an ISO-8601 / RFC3339 timestamp and return Unix seconds, not milliseconds, or nil when the input is invalid.",
        &["timestamp"],
        |_, timestamp: String| {
            Ok(chrono::DateTime::parse_from_rfc3339(&timestamp)
                .ok()
                .map(|dt| dt.timestamp()))
        },
    )?;
    m.fn_(
        "format",
        "Format Unix seconds in the user's local time zone with a strftime-style format string.",
        &["timestamp", "format"],
        |_, (timestamp, format): (i64, String)| {
            Ok(Local
                .timestamp_opt(timestamp, 0)
                .single()
                .map(|dt| dt.format(&format).to_string()))
        },
    )?;
    m.fn_(
        "format_utc",
        "Format Unix seconds in UTC with a strftime-style format string.",
        &["timestamp", "format"],
        |_, (timestamp, format): (i64, String)| {
            Ok(Utc
                .timestamp_opt(timestamp, 0)
                .single()
                .map(|dt| dt.format(&format).to_string()))
        },
    )?;
    Ok(())
}
