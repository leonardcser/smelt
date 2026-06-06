//! `smelt.notebook` - parse, read, apply, and compute preview data for notebook edits.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::notebook;
use std::collections::HashMap;

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &std::sync::Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "notebook",
        "Parse, read, and apply notebook cell edits, plus compute preview data for the edit_notebook tool. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "preview_data",
        "Compute the preview payload for an `edit_notebook` call. Returns `nil` when the notebook can't be read/parsed or the target cell is out of range. The returned table has `{ edit_mode, path, title, old_source, new_source, syntax_ext }`.",
        &["args"],
        |lua, args: mlua::Table| -> LuaResult<Option<mlua::Table>> {
            let args = lua_table_to_json_map(&args)
                .map_err(|e| LuaError::RuntimeError(format!("notebook.preview_data: {e}")))?;
            let Some(data) = smelt_core::notebook::preview_render_data(&args) else {
                return Ok(None);
            };
            let t = lua.create_table()?;
            t.set("edit_mode", data.edit_mode.clone())?;
            t.set("path", data.path.clone())?;
            t.set("title", data.title())?;
            t.set("syntax_ext", data.syntax_ext())?;
            t.set("old_source", data.old_source.clone())?;
            t.set("new_source", data.new_source.clone())?;
            Ok(Some(t))
        },
    )?;
    m.fn_(
        "is_notebook_path",
        "Return `true` if `path` looks like a Jupyter notebook (`.ipynb` extension).",
        &["path"],
        |_, p: String| Ok(smelt_core::notebook::is_notebook_path(&p)),
    )?;

    m.fn_(
        "parse",
        "Parse a notebook JSON string. Returns `(notebook, nil)` with `{ nbformat, nbformat_minor, cells = { { kind, id?, source, execution_count? } } }` on success, or `(nil, error)` on failure.",
        &["json"],
        |lua, json: String| match notebook::parse(&json) {
            Ok(nb) => Ok((Some(notebook_to_lua(lua, &nb)?), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    m.fn_(
        "read",
        "Render a Jupyter notebook at `path` as cell-by-cell text starting at `offset` for at most `limit` cells. Returns `(text, nil)` on success or `(nil, err_msg)` on parse failure - same output the built-in `read_file` tool produces.",
        &["path", "offset", "limit"],
        |_, (path, offset, limit): (String, u64, u64)| -> LuaResult<(Option<String>, Option<String>)> {

            match smelt_core::notebook::render_notebook_text(&path, offset as usize, limit as usize)
            {
                Ok(s) => Ok((Some(s), None)),
                Err(err) => Ok((None, Some(err))),
            }

        },
    )?;

    m.fn_(
        "apply_edit",
        "Apply a notebook edit (cell insert/replace/delete) described by `args` and persist the new file. Returns `(message_table, nil)` on success or `(nil, err_msg)` on failure. Callers are expected to hold the per-path advisory flock.",
        &["args"],
        |lua, args: mlua::Table| -> LuaResult<(Option<mlua::Value>, Option<String>)> {

            let args_map = lua_table_to_json_map(&args)
                .map_err(|e| LuaError::RuntimeError(format!("notebook.apply_edit: {e}")))?;
            let result = crate::lua::try_with_app(|app| {
                smelt_core::notebook::apply_edit(&args_map, &app.core.files)
            });
            match result {
                Some(Ok(outcome)) => {
                    let row = lua.create_table()?;
                    row.set("message", outcome.message)?;
                    row.set(
                        "metadata",
                        super::json_to_lua_value(lua, &outcome.metadata)?,
                    )?;
                    Ok((Some(LuaValue::Table(row)), None))
                }
                Some(Err(err)) => Ok((None, Some(err))),
                None => Ok((None, Some("notebook.apply_edit: no app context".into()))),
            }

        },
    )?;

    {
        let sink = shared.core.resume_sink();
        m.private_fn(
            "__start_read",
            &["task_id", "path", "offset", "limit"],
            move |_, (task_id, path, offset, limit): (u64, String, u64, u64)| -> LuaResult<()> {
                sink.clone().spawn_blocking_resolve(
                    task_id,
                    move || match std::fs::read_to_string(&path) {
                        Ok(raw) => match smelt_core::notebook::render_notebook_text_from_raw(
                            &raw,
                            offset as usize,
                            limit as usize,
                        ) {
                            Ok(content) => {
                                let mtime_ms = smelt_core::fs::file_mtime_ms(&path).unwrap_or(0);
                                serde_json::json!({
                                    "content": content,
                                    "raw": raw,
                                    "mtime_ms": mtime_ms,
                                })
                            }
                            Err(err) => serde_json::json!({ "err": err }),
                        },
                        Err(err) => serde_json::json!({ "err": err.to_string() }),
                    },
                );
                Ok(())
            },
        )?;
    }

    {
        let sink = shared.core.resume_sink();
        m.private_fn(
            "__start_apply_edit",
            &["task_id", "args"],
            move |_, (task_id, args): (u64, mlua::Table)| -> LuaResult<()> {
                let args_map = lua_table_to_json_map(&args).map_err(|e| {
                    LuaError::RuntimeError(format!("notebook.apply_edit_async: {e}"))
                })?;
                let files = crate::lua::try_with_app(|app| app.core.files.clone());
                let Some(files) = files else {
                    sink.resolve_json(
                        task_id,
                        serde_json::json!({ "err": "notebook.apply_edit: no app context" }),
                    );
                    return Ok(());
                };
                let path = args_map
                    .get("notebook_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                sink.clone().spawn_blocking_resolve(task_id, move || {
                    let _lock = if !path.is_empty() && std::path::Path::new(&path).exists() {
                        match smelt_core::fs::try_flock(&path) {
                            Ok(lock) => Some(lock),
                            Err(err) => return serde_json::json!({ "err": err }),
                        }
                    } else {
                        None
                    };
                    match smelt_core::notebook::apply_edit(&args_map, &files) {
                        Ok(outcome) => serde_json::json!({
                            "message": outcome.message,
                            "metadata": outcome.metadata,
                        }),
                        Err(err) => serde_json::json!({ "err": err }),
                    }
                });
                Ok(())
            },
        )?;
    }

    Ok(())
}

fn notebook_to_lua(lua: &Lua, nb: &notebook::Notebook) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    if let Some(v) = nb.format {
        t.set("nbformat", v)?;
    }
    if let Some(v) = nb.format_minor {
        t.set("nbformat_minor", v)?;
    }
    let cells = lua.create_table()?;
    for (i, cell) in nb.cells.iter().enumerate() {
        cells.set(i + 1, cell_to_lua(lua, cell)?)?;
    }
    t.set("cells", cells)?;
    Ok(t)
}

fn cell_to_lua(lua: &Lua, cell: &notebook::Cell) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("kind", cell.kind.as_str())?;
    if let Some(id) = &cell.id {
        t.set("id", id.clone())?;
    }
    t.set("source", cell.source.clone())?;
    if let Some(n) = cell.execution_count {
        t.set("execution_count", n)?;
    }
    Ok(t)
}

/// Shallow Lua table → JSON map. Nested tables become arrays or objects based on shape.
fn lua_table_to_json_map(t: &mlua::Table) -> mlua::Result<HashMap<String, serde_json::Value>> {
    let mut out = HashMap::new();
    for pair in t.clone().pairs::<String, mlua::Value>() {
        let (k, v) = pair?;
        out.insert(k, lua_value_to_json(&v)?);
    }
    Ok(out)
}

fn lua_value_to_json(v: &mlua::Value) -> mlua::Result<serde_json::Value> {
    use serde_json::Value as J;
    Ok(match v {
        mlua::Value::Nil => J::Null,
        mlua::Value::Boolean(b) => J::Bool(*b),
        mlua::Value::Integer(i) => J::Number((*i).into()),
        mlua::Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(J::Number)
            .unwrap_or(J::Null),
        mlua::Value::String(s) => J::String(s.to_str()?.to_string()),
        mlua::Value::Table(t) => {
            let len = t.raw_len();
            if len > 0 {
                let mut arr = Vec::with_capacity(len);
                for i in 1..=len {
                    arr.push(lua_value_to_json(&t.raw_get::<mlua::Value>(i)?)?);
                }
                J::Array(arr)
            } else {
                let mut obj = serde_json::Map::new();
                for pair in t.clone().pairs::<String, mlua::Value>() {
                    let (k, v) = pair?;
                    obj.insert(k, lua_value_to_json(&v)?);
                }
                J::Object(obj)
            }
        }
        _ => J::Null,
    })
}
