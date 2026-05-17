//! `smelt.text` — visual-width measurement. UiHost-only (because the
//! width metric matches the TUI's terminal-cell column count). Render
//! helpers live in `smelt.render`.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use unicode_width::UnicodeWidthStr;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "text",
        "Visual-width measurement. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "width",
        "Return the visual column count of `s`. Lua's `#s` counts bytes; use this for sizing extmark ranges or computing column offsets so multi-byte and wide characters land correctly.",
        &["s"],
        |_, s: String| Ok(UnicodeWidthStr::width(s.as_str()) as u64),
    )?;
    Ok(())
}
