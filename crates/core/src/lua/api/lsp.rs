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
            "Configure available language servers. `servers` maps names to `{ cmd, language_id, extensions, root_markers }`; `start` defaults to `background`.",
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
                let payload = match dispatch_call(manager, &operation, args).await {
                    Ok(value) => serde_json::json!({ "result": value }),
                    Err(err) => serde_json::json!({ "err": err }),
                };
                sink.resolve_json(task_id, payload);
            });
            Ok(())
        },
    )
}

async fn dispatch_call(
    manager: Arc<crate::lsp::LspManager>,
    operation: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    match operation {
        "status" => Ok(serde_json::Value::String(manager.status().await)),
        "outline" => {
            let file_path = required_string(&args, "file_path")?;
            let symbol = optional_string(&args, "symbol");
            let kind = optional_string(&args, "kind");
            let name_contains = optional_string(&args, "name_contains");
            manager
                .outline(crate::lsp::OutlineOptions {
                    file_path: &file_path,
                    max_symbols: optional_usize(&args, "max_symbols").unwrap_or(200),
                    symbol: symbol.as_deref(),
                    kind: kind.as_deref(),
                    name_contains: name_contains.as_deref(),
                    max_depth: optional_usize(&args, "max_depth"),
                })
                .await
        }
        "workspace_symbols" => {
            let query = required_string(&args, "query")?;
            let kind = optional_string(&args, "kind");
            let path_glob = optional_string(&args, "path_glob");
            manager
                .workspace_symbols(
                    &query,
                    kind.as_deref(),
                    path_glob.as_deref(),
                    optional_usize(&args, "limit").unwrap_or(20),
                    args.get("exact")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                )
                .await
        }
        "inspect_symbol_at" => {
            let file_path = required_string(&args, "file_path")?;
            manager
                .inspect_symbol(
                    &file_path,
                    int_arg(&args, "line")?,
                    int_arg(&args, "column")?,
                    optional_u64(&args, "depth").unwrap_or(1),
                )
                .await
        }
        "inspect_symbol" => {
            let (file_path, line, column) = resolve_symbol_query(&manager, &args).await?;
            manager
                .inspect_symbol(
                    &file_path,
                    line,
                    column,
                    optional_u64(&args, "depth").unwrap_or(1),
                )
                .await
        }
        "definition" => {
            let file_path = required_string(&args, "file_path")?;
            manager
                .definition(
                    &file_path,
                    int_arg(&args, "line")?,
                    int_arg(&args, "column")?,
                )
                .await
        }
        "references" => {
            let file_path = required_string(&args, "file_path")?;
            let include = args
                .get("include_declaration")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let limit = optional_usize(&args, "limit").unwrap_or(50);
            let raw = args
                .get("raw")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            manager
                .references(
                    &file_path,
                    int_arg(&args, "line")?,
                    int_arg(&args, "column")?,
                    crate::lsp::ReferenceOptions {
                        include_declaration: include,
                        limit,
                        raw,
                    },
                )
                .await
        }
        "diagnostics" => {
            let file_path = args.get("file_path").and_then(serde_json::Value::as_str);
            manager.diagnostics(file_path).await
        }
        "rename_preview" | "rename" => {
            let file_path = required_string(&args, "file_path")?;
            let new_name = required_string(&args, "new_name")?;
            manager
                .rename(
                    &file_path,
                    int_arg(&args, "line")?,
                    int_arg(&args, "column")?,
                    &new_name,
                    operation == "rename",
                )
                .await
        }
        _ => Err(format!("unknown LSP operation: {operation}")),
    }
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

fn optional_string(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn optional_usize(args: &serde_json::Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

async fn resolve_symbol_query(
    manager: &Arc<crate::lsp::LspManager>,
    args: &serde_json::Value,
) -> Result<(String, u64, u64), String> {
    let query = required_string(args, "query")?;
    let kind = optional_string(args, "kind");
    let path_glob = optional_string(args, "path_glob");
    let result = manager
        .workspace_symbols(
            &query,
            kind.as_deref(),
            path_glob.as_deref(),
            5,
            args.get("exact")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
        )
        .await?;
    let symbols = result
        .get("symbols")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("no symbol found for query: {query}"))?;
    if symbols.is_empty() {
        return Err(format!("no symbol found for query: {query}"));
    }
    let exact_count = symbols
        .iter()
        .filter(|symbol| symbol.get("rank").and_then(serde_json::Value::as_str) == Some("exact"))
        .count();
    if exact_count > 1 {
        return Err(serde_json::json!({
            "error": "ambiguous symbol query",
            "query": query,
            "candidates": symbols,
        })
        .to_string());
    }
    let symbol = symbols.first().unwrap();
    Ok((
        symbol
            .get("file_path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("symbol has no file_path: {query}"))?
            .to_string(),
        symbol
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| format!("symbol has no line: {query}"))?,
        symbol
            .get("column")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| format!("symbol has no column: {query}"))?,
    ))
}

fn optional_u64(args: &serde_json::Value, key: &str) -> Option<u64> {
    args.get(key).and_then(serde_json::Value::as_u64)
}
