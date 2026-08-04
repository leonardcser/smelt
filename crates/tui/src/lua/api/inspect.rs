//! Lua bindings for the `/inspect` session introspection web UI.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &std::sync::Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let inspect = LuaMod::advanced(
        lua,
        smelt,
        "inspect",
        "Local session introspection web UI. `smelt.inspect.url()` reads the running server URL; the optional `inspect` plugin adds `smelt.inspect.start()`, `smelt.inspect.stop()`, and `smelt.inspect.open()`.",
        Tier::UiHost,
    )?;

    inspect.private_live_only_fn(
        "__start",
        &["task_id"],
        move |_, task_id: u64| -> LuaResult<()> {
            crate::lua::with_platform_host(|host| host.start_inspect_server(task_id));
            Ok(())
        },
    )?;

    inspect.private_live_only_fn(
        "__stop",
        &["task_id"],
        move |_, task_id: u64| -> LuaResult<()> {
            crate::lua::with_platform_host(|host| host.stop_inspect_server(task_id));
            Ok(())
        },
    )?;

    let open_context = std::sync::Arc::clone(&shared.core);
    inspect.private_live_only_fn(
        "__open_url",
        &["url"],
        move |lua, url: String| -> LuaResult<mlua::Table> {
            let result = lua.create_table()?;
            let cwd = open_context.evaluation_cwd();
            match engine::opener::open_url_if_available_in(&url, &cwd) {
                engine::opener::OpenResult::Opened => {
                    result.set("ok", true)?;
                    result.set("opened", true)?;
                }
                engine::opener::OpenResult::Unavailable(reason) => {
                    result.set("ok", false)?;
                    result.set("opened", false)?;
                    result.set("reason", reason)?;
                }
                engine::opener::OpenResult::Failed(err) => {
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
            crate::lua::try_with_platform_host(|host| host.inspect_server_url().into_lua(lua))
                .unwrap_or(Ok(mlua::Value::Nil))
        },
    )?;

    Ok(())
}
