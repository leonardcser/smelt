//! `smelt.notebook` bindings.
//!
//! - `render(buf_id, args)` paints an `edit_notebook` preview into a
//!   Buffer the caller owns (UiHost-only). It asks `app::notebook`
//!   for typed `NotebookRenderData` from the tool args (insert /
//!   delete / replace cell), then prints it via the same syntax /
//!   inline-diff helpers the transcript renderer uses.
//! - `parse / is_notebook_path` are Host-tier read shapes over
//!   `app::notebook` for plugins that want to introspect a
//!   notebook's structure.

use crate::core::notebook;
use crate::core::notebook::NotebookRenderData;
use crate::core::content::display::{ColorRole, ColorValue};
use crate::content::highlight::{print_inline_diff, print_syntax_file};
use crate::content::layout_out::SpanCollector;
use crate::content::selection::wrap_line;
use crate::ui::BufId;
use mlua::prelude::*;
use std::collections::HashMap;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let notebook = lua.create_table()?;
    notebook.set(
        "render",
        lua.create_function(|_, (buf_id, args): (u64, mlua::Table)| {
            let args = lua_table_to_json_map(&args)
                .map_err(|e| LuaError::RuntimeError(format!("notebook.render: {e}")))?;
            crate::lua::with_app(|app| {
                let Some(data) = crate::core::notebook::preview_render_data(&args) else {
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
        })?,
    )?;
    notebook.set(
        "is_notebook_path",
        lua.create_function(|_, p: String| Ok(crate::core::notebook::is_notebook_path(&p)))?,
    )?;

    notebook.set(
        "parse",
        lua.create_function(|lua, json: String| match notebook::parse(&json) {
            Ok(nb) => Ok((Some(notebook_to_lua(lua, &nb)?), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    // `smelt.notebook.read(path, offset, limit)` returns the same
    // line-numbered cell-by-cell text the engine `read_file` tool
    // produces for `.ipynb` paths. Used by the Lua `read_file` tool
    // when the caller hands it a notebook path.
    notebook.set(
        "read",
        lua.create_function(|_, (path, offset, limit): (String, u64, u64)| {
            match crate::core::notebook::render_notebook_text(
                &path,
                offset as usize,
                limit as usize,
            ) {
                Ok(s) => Ok((Some(s), None)),
                Err(err) => Ok((None, Some(err))),
            }
        })?,
    )?;

    // `smelt.notebook.apply_edit(args)` performs the JSON cell munging
    // for the Lua `edit_notebook` tool. The caller already holds the
    // per-path advisory flock; this writes the file, populates the
    // shared file-state cache via `record_write`, and returns the
    // confirmation message + metadata table on success. On failure
    // returns `(nil, error_string)`.
    notebook.set(
        "apply_edit",
        lua.create_function(|lua, args: mlua::Table| {
            let args_map = lua_table_to_json_map(&args)
                .map_err(|e| LuaError::RuntimeError(format!("notebook.apply_edit: {e}")))?;
            let result = crate::lua::try_with_app(|app| {
                crate::core::notebook::apply_edit(&args_map, &app.core.files)
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

fn render_notebook_preview(
    out: &mut SpanCollector,
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
        out.push_fg(ColorValue::Role(ColorRole::Muted));
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
