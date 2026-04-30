//! `smelt.html` bindings — read-only HTML parsing over `tui::html`.
//! Host-tier (works in tui and headless) — no Ui touch.

use mlua::prelude::*;

use crate::html;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let html_tbl = lua.create_table()?;

    html_tbl.set(
        "title",
        lua.create_function(|_, source: String| Ok(html::title(&source)))?,
    )?;

    html_tbl.set(
        "links",
        lua.create_function(|_, (source, base): (String, Option<String>)| {
            Ok(html::links(&source, base.as_deref()))
        })?,
    )?;

    html_tbl.set(
        "to_text",
        lua.create_function(|_, source: String| Ok(html::to_text(&source)))?,
    )?;

    smelt.set("html", html_tbl)?;
    Ok(())
}
