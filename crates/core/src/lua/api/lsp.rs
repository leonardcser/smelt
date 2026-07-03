//! `smelt.lsp` - generic stdio LSP client used by the optional LSP plugin.

use crate::lua::api::lua_table_to_json;
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "lsp",
        "Generic stdio Language Server Protocol client. Used by the optional LSP tool plugin.",
        Tier::Host,
    )?;

    register_call(&m, shared)?;

    {
        let s = shared.clone();
        m.fn_(
            "configure",
            "Configure available language servers. `servers` maps names to `{ cmd, language_id, extensions, root_markers }`.",
            &["config"],
            move |lua, config: mlua::Table| -> LuaResult<()> {
                let config_json = lua_table_to_json(lua, &config);
                let config = serde_json::from_value::<crate::lsp::LspConfig>(config_json)
                    .map_err(mlua::Error::external)?;
                s.lsp.configure_detached(config);
                Ok(())
            },
        )?;
    }
    Ok(())
}

fn register_call(m: &LuaMod, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let s = shared.clone();
    m.private_fn(
        "__call",
        &["task_id", "operation", "args"],
        move |lua, (task_id, operation, args): (u64, String, mlua::Table)| -> LuaResult<()> {
            let args = lua_table_to_json(lua, &args);
            let manager = s.lsp.clone();
            let sink = s.resume_sink();
            tokio::spawn(async move {
                let payload = match manager.call(&operation, args).await {
                    Ok(value) => serde_json::json!({ "result": value }),
                    Err(err) => serde_json::json!({ "err": err }),
                };
                sink.resolve_json(task_id, payload);
            });
            Ok(())
        },
    )
}
