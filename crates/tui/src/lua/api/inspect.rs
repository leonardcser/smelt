//! Lua bindings for the `/inspect` session introspection web UI.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    _shared: &Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let inspect = LuaMod::under(
        lua,
        smelt,
        "inspect",
        "Local session introspection web UI. `smelt.inspect.url()` reads the running server URL; the optional `inspect` plugin adds `smelt.inspect.start()`, `smelt.inspect.stop()`, and `smelt.inspect.open()`.",
        Tier::UiHost,
    )?;

    inspect.private_fn(
        "__start",
        &["task_id"],
        move |_, task_id: u64| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                let shared_server = Arc::clone(&app.inspect_server);
                if let Some(ref server) = *shared_server.lock().unwrap() {
                    app.lua.shared().resume_sink().resolve_json(
                        task_id,
                        serde_json::json!({ "ok": true, "url": server.url() }),
                    );
                    return Ok(());
                }

                let sink = app.lua.shared().resume_sink();
                tokio::spawn(async move {
                    let payload = match crate::inspect_server::Server::start().await {
                        Ok(server) => {
                            let url = server.url();
                            *shared_server.lock().unwrap() = Some(server);
                            serde_json::json!({ "ok": true, "url": url })
                        }
                        Err(err) => {
                            serde_json::json!({ "ok": false, "error": err.to_string() })
                        }
                    };
                    sink.resolve_json(task_id, payload);
                });
                Ok(())
            })
        },
    )?;

    inspect.private_fn(
        "__stop",
        &["task_id"],
        move |_, task_id: u64| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                let shared_server = Arc::clone(&app.inspect_server);
                let sink = app.lua.shared().resume_sink();
                tokio::spawn(async move {
                    let server = shared_server.lock().unwrap().take();
                    let payload = if let Some(mut server) = server {
                        server.stop().await;
                        serde_json::json!({ "ok": true })
                    } else {
                        serde_json::json!({ "ok": false, "error": "server not running" })
                    };
                    sink.resolve_json(task_id, payload);
                });
                Ok(())
            })
        },
    )?;

    inspect.private_fn(
        "__open_url",
        &["url"],
        move |lua, url: String| -> LuaResult<mlua::Table> {
            let result = lua.create_table()?;
            match engine::browser::open_url_if_available(&url) {
                engine::browser::BrowserOpenResult::Opened => {
                    result.set("ok", true)?;
                    result.set("opened", true)?;
                }
                engine::browser::BrowserOpenResult::Unavailable(reason) => {
                    result.set("ok", false)?;
                    result.set("opened", false)?;
                    result.set("reason", reason)?;
                }
                engine::browser::BrowserOpenResult::Failed(err) => {
                    result.set("ok", false)?;
                    result.set("opened", false)?;
                    result.set("error", err)?;
                }
            }
            Ok(result)
        },
    )?;

    inspect.fn_(
        "url",
        "Return the URL of the running inspector server, or nil if it is not running.",
        &[],
        move |lua, ()| -> LuaResult<mlua::Value> {
            crate::lua::try_with_app(|app| {
                app.inspect_server
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|s| s.url())
                    .into_lua(lua)
            })
            .unwrap_or(Ok(mlua::Value::Nil))
        },
    )?;

    Ok(())
}
