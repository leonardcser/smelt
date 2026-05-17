//! `smelt.text` — visual-width measurement. UiHost-only (because the
//! width metric matches the TUI's terminal-cell column count). Render
//! helpers live in `smelt.render`.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;
use unicode_width::UnicodeWidthStr;

#[lua_module(name = "smelt.text", doc = "Visual-width measurement. UiHost-only.")]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let text = lua.create_table()?;
    register_ui_fn(
        &text,
        "smelt.text",
        "width",
        "Return the visual column count of `s`. Lua's `#s` counts bytes; use this for sizing extmark ranges or computing column offsets so multi-byte and wide characters land correctly.",
        &["s"],
        lua,
        |_, s: String| Ok(UnicodeWidthStr::width(s.as_str()) as u64),
    )?;
    smelt.set("text", text)?;
    Ok(())
}
