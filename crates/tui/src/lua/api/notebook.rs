//! `smelt.notebook` — render, parse, read, and apply notebook edits.

use crate::content::builder::LineBuilder;
use crate::content::highlight::{print_inline_diff, print_syntax_file};
use crate::content::selection::wrap_line;
use crate::smelt_term::BufId;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};
use smelt_core::notebook;
use smelt_core::notebook::NotebookRenderData;
use smelt_core::theme::role_hl;
use std::collections::HashMap;

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let notebook = lua.create_table()?;
    record_module_doc(
        "smelt.notebook",
        "Render, parse, read, and apply notebook cell edits. UiHost-only.",
    );
    register_ui_fn(
        &notebook,
        "smelt.notebook",
        "render",
        "Render a notebook edit preview into the buffer (insert mode shows the new source highlighted; edit mode shows an inline diff). `args` is the notebook tool's argument table.",
        &["buf_id", "args"],
        lua,
        |_, (buf_id, args): (u64, mlua::Table)|  -> LuaResult<()>{
            let args = lua_table_to_json_map(&args)
                .map_err(|e| LuaError::RuntimeError(format!("notebook.render: {e}")))?;
            crate::lua::with_app(|app| {
                let Some(data) = smelt_core::notebook::preview_render_data(&args) else {
                    return;
                };
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    crate::content::to_buffer::render_into_buffer(
                        buf,
                        width,
                        &theme_snap,
                        |sink| render_notebook_preview(sink, &data, 0, u16::MAX),
                    );
                }
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &notebook,
        "smelt.notebook",
        "is_notebook_path",
        "Return `true` if `path` looks like a Jupyter notebook (`.ipynb` extension).",
        &["path"],
        lua,
        |_, p: String| Ok(smelt_core::notebook::is_notebook_path(&p)),
    )?;

    register_ui_fn(
        &notebook,
        "smelt.notebook",
        "parse",
        "Parse a notebook JSON string. Returns `(notebook, nil)` with `{ nbformat, nbformat_minor, cells = { { kind, id?, source, execution_count? } } }` on success, or `(nil, error)` on failure.",
        &["json"],
        lua,
        |lua, json: String| match notebook::parse(&json) {
            Ok(nb) => Ok((Some(notebook_to_lua(lua, &nb)?), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    // `smelt.notebook.read(path, offset, limit)` — same cell-by-cell text as the read_file tool.
    register_ui_fn(
        &notebook,
        "smelt.notebook",
        "read",
        "`smelt.notebook.read(path, offset, limit)` — same cell-by-cell text as the read_file tool.",
        &["path", "offset", "limit"],
        lua,
        |_, (path, offset, limit): (String, u64, u64)| -> LuaResult<(Option<String>, Option<String>)> {

            match smelt_core::notebook::render_notebook_text(&path, offset as usize, limit as usize)
            {
                Ok(s) => Ok((Some(s), None)),
                Err(err) => Ok((None, Some(err))),
            }

        },
    )?;

    // `smelt.notebook.apply_edit(args)` — write the file and return (message, metadata)
    // or (nil, error). Caller holds the per-path advisory flock.
    register_ui_fn(
        &notebook,
        "smelt.notebook",
        "apply_edit",
        "`smelt.notebook.apply_edit(args)` — write the file and return (message, metadata) or (nil, error). Caller holds the per-path advisory flock.",
        &["args"],
        lua,
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

    smelt.set("notebook", notebook)?;
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

fn render_notebook_preview(
    out: &mut LineBuilder,
    data: &NotebookRenderData,
    skip: u16,
    viewport: u16,
) {
    let title = data.title();
    let title_lines = wrap_line(&title, crate::content::term_width().saturating_sub(4));
    let mut skipped = skip;
    let mut emitted = 0u16;

    for line in &title_lines {
        if skipped > 0 {
            skipped -= 1;
            continue;
        }
        if viewport > 0 && emitted >= viewport {
            return;
        }
        out.print(" ");
        out.push_hl(role_hl("Muted"));
        out.print(line);
        out.pop_style();
        out.newline();
        emitted += 1;
    }

    let remaining = if viewport == 0 {
        0
    } else {
        viewport.saturating_sub(emitted)
    };
    if data.edit_mode == "insert" {
        if remaining == 0 && viewport > 0 {
            return;
        }
        print_syntax_file(out, &data.new_source, &data.path, skipped, remaining);
    } else {
        print_inline_diff(
            out,
            &data.old_source,
            &data.new_source,
            &data.path,
            &data.old_source,
            skipped,
            remaining,
        );
    }
}
