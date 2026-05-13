//! `smelt.html` — HTML parsing (title, links, to_text, to_markdown, DDG results).

use mlua::prelude::*;

use crate::html;
use crate::lua::doc::{record_module_doc, register_fn};
use lua_doc_derive::lua_module;

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let html_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.html",
        "HTML parsing: title extraction, link scraping, to_text, to_markdown, DDG results.",
    );

    register_fn(
        &html_tbl,
        "smelt.html",
        "title",
        "Return the `<title>` text of `source`, or `nil` if no title element is present.",
        &["source"],
        lua,
        |_, source: String| Ok(html::title(&source)),
    )?;

    register_fn(
        &html_tbl,
        "smelt.html",
        "links",
        "Extract all anchor `href` links from `source`. When `base` is supplied, relative URLs are resolved against it.",
        &["source", "base"],
        lua,
        |_, (source, base): (String, Option<String>)| Ok(html::links(&source, base.as_deref())),
    )?;

    register_fn(
        &html_tbl,
        "smelt.html",
        "to_text",
        "Strip HTML tags from `source` and return the visible text content.",
        &["source"],
        lua,
        |_, source: String| Ok(html::to_text(&source)),
    )?;

    register_fn(
        &html_tbl,
        "smelt.html",
        "to_markdown",
        "Convert `source` HTML to a `{ title, content, links }` table where `content` is markdown. Relative links resolve against `base` when supplied.",
        &["source", "base"],
        lua,
        |lua, (source, base): (String, Option<String>)|  -> LuaResult<mlua::Table>{
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
        },
    )?;

    register_fn(
        &html_tbl,
        "smelt.html",
        "parse_ddg_results",
        "Parse a DuckDuckGo HTML results page into rows of `{ title, link, description }`.",
        &["source"],
        lua,
        |lua, source: String| -> LuaResult<mlua::Table> {
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
        },
    )?;

    smelt.set("html", html_tbl)?;
    Ok(())
}
