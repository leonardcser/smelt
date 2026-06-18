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

    register_call(&m, shared, "__status", |manager, _| async move {
        Ok(serde_json::Value::String(manager.status().await))
    })?;
    register_call(
        &m,
        shared,
        "__document_symbols",
        |manager, args| async move {
            let file_path = required_string(&args, "file_path")?;
            manager.document_symbols(&file_path).await
        },
    )?;
    register_call(&m, shared, "__definition", |manager, args| async move {
        let file_path = required_string(&args, "file_path")?;
        manager
            .definition(
                &file_path,
                int_arg(&args, "line")?,
                int_arg(&args, "column")?,
            )
            .await
    })?;
    register_call(&m, shared, "__references", |manager, args| async move {
        let file_path = required_string(&args, "file_path")?;
        let include = args
            .get("include_declaration")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        manager
            .references(
                &file_path,
                int_arg(&args, "line")?,
                int_arg(&args, "column")?,
                include,
            )
            .await
    })?;
    register_call(&m, shared, "__diagnostics", |manager, args| async move {
        let file_path = args.get("file_path").and_then(serde_json::Value::as_str);
        manager.diagnostics(file_path).await
    })?;
    register_call(&m, shared, "__rename_preview", |manager, args| async move {
        let file_path = required_string(&args, "file_path")?;
        let new_name = required_string(&args, "new_name")?;
        manager
            .rename(
                &file_path,
                int_arg(&args, "line")?,
                int_arg(&args, "column")?,
                &new_name,
                false,
            )
            .await
    })?;
    register_call(&m, shared, "__rename", |manager, args| async move {
        let file_path = required_string(&args, "file_path")?;
        let new_name = required_string(&args, "new_name")?;
        manager
            .rename(
                &file_path,
                int_arg(&args, "line")?,
                int_arg(&args, "column")?,
                &new_name,
                true,
            )
            .await
    })?;

    {
        let s = shared.clone();
        m.fn_(
            "configure",
            "Configure available language servers. `servers` maps names to `{ cmd, languages, root_markers }`; `start` defaults to `background`.",
            &["config"],
            move |lua, config: mlua::Table| -> LuaResult<()> {
                let config_json = lua_table_to_json(lua, &config);
                let config = serde_json::from_value::<crate::lsp::LspConfig>(config_json)
                    .map_err(mlua::Error::external)?;
                s.lsp.configure_sync(config);
                Ok(())
            },
        )?;
    }
    Ok(())
}

fn register_call<F, Fut>(
    m: &LuaMod,
    shared: &Arc<LuaShared>,
    name: &'static str,
    f: F,
) -> LuaResult<()>
where
    F: Fn(Arc<crate::lsp::LspManager>, serde_json::Value) -> Fut + Send + Sync + Copy + 'static,
    Fut: std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'static,
{
    let s = shared.clone();
    m.private_fn(
        name,
        &["task_id", "args"],
        move |lua, (task_id, args): (u64, mlua::Table)| -> LuaResult<()> {
            let args = lua_table_to_json(lua, &args);
            let manager = s.lsp.clone();
            let sink = s.resume_sink();
            tokio::spawn(async move {
                let payload = match f(manager, args).await {
                    Ok(value) => serde_json::json!({ "result": value }),
                    Err(err) => serde_json::json!({ "err": err }),
                };
                sink.resolve_json(task_id, payload);
            });
            Ok(())
        },
    )
}

fn required_string(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required argument: {key}"))
}

fn int_arg(args: &serde_json::Value, key: &str) -> Result<u64, String> {
    args.get(key)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| format!("missing required argument: {key}"))
}
