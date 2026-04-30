//! `smelt.notebook` bindings.
//!
//! - `render(buf_id, args)` paints an `edit_notebook` preview into a
//!   Buffer the caller owns (UiHost-only). Reuses
//!   `ConfirmPreview::from_tool` so the picker/confirm/dialog paths
//!   all render notebook ops the same way.
//! - `parse / source_to_string / cell_index_by_id / is_notebook_path`
//!   are Host-tier read shapes over `tui::notebook` for plugins that
//!   want to introspect a notebook's structure.

use crate::app::dialogs::confirm_preview::ConfirmPreview;
use crate::notebook;
use mlua::prelude::*;
use std::collections::HashMap;
use ui::BufId;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let notebook = lua.create_table()?;
    notebook.set(
        "render",
        lua.create_function(|_, (buf_id, args): (u64, mlua::Table)| {
            let args = lua_table_to_json_map(&args)
                .map_err(|e| LuaError::RuntimeError(format!("notebook.render: {e}")))?;
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                let preview = ConfirmPreview::from_tool("edit_notebook", "", &args);
                if !preview.is_some() {
                    return;
                }
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    preview.render_into_buffer(buf, width, &theme_snap);
                }
            });
            Ok(())
        })?,
    )?;
    notebook.set(
        "is_notebook_path",
        lua.create_function(|_, p: String| Ok(crate::notebook::is_notebook_path(&p)))?,
    )?;

    notebook.set(
        "parse",
        lua.create_function(|lua, json: String| match notebook::parse(&json) {
            Ok(nb) => Ok((Some(notebook_to_lua(lua, &nb)?), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
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

/// Shallow Lua → JSON map conversion for tool-arg tables. Strings,
/// numbers, booleans, nil pass through; nested tables become arrays
/// or objects depending on whether they look sequence-shaped.
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
