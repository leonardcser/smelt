//! `smelt.html` — HTML parsing (title, links, to_text, to_markdown, DDG results).

use mlua::prelude::*;

use crate::html;
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "html",
        "HTML parsing: title extraction, link scraping, to_text, to_markdown, DDG results.",
        Tier::Host,
    )?;
    m.fn_(
        "title",
        "Return the `<title>` text of `source`, or `nil` if no title element is present.",
        &["source"],
        |_, source: String| Ok(html::title(&source)),
    )?;

    m.fn_(
        "links",
        "Extract all anchor `href` links from `source`. When `base` is supplied, relative URLs are resolved against it.",
        &["source", "base"],
        |_, (source, base): (String, Option<String>)| Ok(html::links(&source, base.as_deref())),
    )?;

    m.fn_(
        "to_text",
        "Strip HTML tags from `source` and return the visible text content.",
        &["source"],
        |_, source: String| Ok(html::to_text(&source)),
    )?;

    m.fn_(
        "to_markdown",
        "Convert `source` HTML to a `{ title, content, links }` table where `content` is markdown. Relative links resolve against `base` when supplied.",
        &["source", "base"],
        |lua, (source, base): (String, Option<String>)| -> LuaResult<mlua::Table> {
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

    m.fn_(
        "parse_ddg_results",
        "Parse a DuckDuckGo HTML results page into rows of `{ title, link, description }`.",
        &["source"],
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

    Ok(())
}
