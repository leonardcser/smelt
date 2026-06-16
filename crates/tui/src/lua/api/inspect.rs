//! Lua bindings for the `/inspect` session introspection web UI.

use super::LuaShared;
use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, _shared: &Arc<LuaShared>) -> LuaResult<()> {
    let inspect = LuaMod::under(
        lua,
        smelt,
        "inspect",
        "Local session introspection web UI. `smelt.inspect.start()` opens a browser-ready server; `smelt.inspect.url()` and `smelt.inspect.stop()` query and close it.",
        Tier::UiHost,
    )?;

    inspect.fn_(
        "start",
        "Start the local session-inspector web server on an ephemeral loopback port and return its URL. If a server is already running, returns the existing URL.",
        &[],
        move |lua, ()| -> LuaResult<mlua::Value> {
            crate::lua::with_app(|app| {
                if let Some(ref server) = app.inspect_server {
                    return server.url().into_lua(lua);
                }
                let runtime = tokio::runtime::Handle::try_current()
                    .map_err(|e| LuaError::external(format!("no async runtime: {e}")))?;
                let server = runtime
                    .block_on(crate::inspect_server::Server::start())
                    .map_err(|e| LuaError::external(format!("failed to start inspector: {e}")))?;
                let url = server.url();
                app.inspect_server = Some(server);
                url.into_lua(lua)
            })
        },
    )?;

    inspect.fn_(
        "stop",
        "Stop the running session-inspector web server, if any.",
        &[],
        move |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if let Some(mut server) = app.inspect_server.take() {
                    let runtime = tokio::runtime::Handle::try_current()
                        .map_err(|e| LuaError::external(format!("no async runtime: {e}")))?;
                    runtime.block_on(server.stop());
                }
                Ok(())
            })
        },
    )?;

    inspect.fn_(
        "url",
        "Return the URL of the running inspector server, or nil if it is not running.",
        &[],
        move |lua, ()| -> LuaResult<mlua::Value> {
            crate::lua::try_with_app(|app| {
                app.inspect_server.as_ref().map(|s| s.url()).into_lua(lua)
            })
            .unwrap_or(Ok(mlua::Value::Nil))
        },
    )?;

    Ok(())
}
