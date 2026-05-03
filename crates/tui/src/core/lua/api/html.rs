//! `smelt.html` bindings — read-only HTML parsing over `app::html`.
//! Host-tier (works in tui and headless) — no Ui touch.

use mlua::prelude::*;

use crate::core::html;

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

    html_tbl.set(
        "to_markdown",
        lua.create_function(|lua, (source, base): (String, Option<String>)| {
            let md = html::to_markdown(&source, base.as_deref());
            let out = lua.create_table()?;
            out.set("title", md.title)?;
            out.set("content", md.content)?;
            let links = lua.create_table()?;
            for (i, link) in md.links.into_iter().enumerate() {
                links.set(i + 1, link)?;
            }
            out.set("links", links)?;
            Ok(out)
        })?,
    )?;

    html_tbl.set(
        "parse_ddg_results",
        lua.create_function(|lua, source: String| {
            let results = html::parse_ddg_results(&source);
            let out = lua.create_table()?;
            for (i, r) in results.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("title", r.title)?;
                row.set("link", r.link)?;
                row.set("description", r.description)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;

    smelt.set("html", html_tbl)?;
    Ok(())
}
