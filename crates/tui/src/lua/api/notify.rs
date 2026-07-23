//! `smelt.notify` - informational toasts in the status area.
//!
//! `smelt.notify.info("msg")`, `smelt.notify.warn("msg")`, and
//! `smelt.notify.error("msg")` each show a one-line toast over the
//! prompt-above region AND append the full body to the persistent
//! message log (`smelt.messages`) so the user can recover the details
//! later via `/messages`. An optional second positional arg names the
//! source plugin (defaults to `"lua"`) so `/messages` can attribute
//! every toast back to whoever raised it. Multi-line bodies
//! (tracebacks, command stderr) collapse to the first line in the
//! toast and the toast is clipped to terminal width so it can never
//! spill onto adjacent rows. UiHost-only.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::messages::MessageKind;
use std::sync::Arc;

use crate::lua::LuaShared;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "notify",
        "Status-area toasts. Each call appends the body to `smelt.messages` and surfaces a one-line summary in the toast row above the prompt. The optional `source` arg tags the entry in `/messages` (defaults to `\"lua\"`). UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "error",
        "Show an error toast (highlighted with the error color) and append the body to the message log. Pass `source` to tag the `/messages` entry (e.g. `\"upgrade\"`); defaults to `\"lua\"`.",
        &["msg", "source"],
        {
            let shared = Arc::clone(shared);
            move |_, (msg, source): (String, Option<String>)| -> LuaResult<()> {
                record_from_lua(&shared, MessageKind::Error, source, msg);
                Ok(())
            }
        },
    )?;
    m.fn_(
        "warn",
        "Show a warning toast and append the body to the message log. Pass `source` to tag the `/messages` entry; defaults to `\"lua\"`.",
        &["msg", "source"],
        {
            let shared = Arc::clone(shared);
            move |_, (msg, source): (String, Option<String>)| -> LuaResult<()> {
                record_from_lua(&shared, MessageKind::Warning, source, msg);
                Ok(())
            }
        },
    )?;
    m.fn_(
        "info",
        "Show an informational toast and append the body to the message log. Pass `source` to tag the `/messages` entry; defaults to `\"lua\"`.",
        &["msg", "source"],
        {
            let shared = Arc::clone(shared);
            move |_, (msg, source): (String, Option<String>)| -> LuaResult<()> {
                record_from_lua(&shared, MessageKind::Info, source, msg);
                Ok(())
            }
        },
    )?;
    Ok(())
}

fn record_from_lua(
    shared: &Arc<LuaShared>,
    kind: MessageKind,
    source: Option<String>,
    msg: String,
) {
    let source = source.unwrap_or_else(|| "lua".into());
    if !shared.core.external_effects_active() {
        shared
            .staged_notices
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((kind, source, msg));
    } else {
        crate::lua::try_with_runtime_host(|host| host.record_notice(kind, source, msg));
    }
}

pub(crate) fn take_staged_notices(shared: &Arc<LuaShared>) -> Vec<(MessageKind, String, String)> {
    std::mem::take(
        &mut *shared
            .staged_notices
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    )
}
