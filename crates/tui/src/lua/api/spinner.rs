//! `smelt.spinner` — shared spinner glyph and cadence so plugin
//! animations stay in sync with the built-in status pill. UiHost-only.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;

#[lua_module(
    name = "smelt.spinner",
    doc = "Shared spinner glyph and cadence for plugin animations. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let spinner_tbl = lua.create_table()?;
    register_ui_fn(
        &spinner_tbl,
        "smelt.spinner",
        "glyph",
        "Return the current spinner glyph (single grapheme). Stays in sync with the status bar's working pill so plugin spinners animate together.",
        &[],
        lua,
        |_, ()| Ok(smelt_core::content::spinner_glyph()),
    )?;
    register_ui_fn(
        &spinner_tbl,
        "smelt.spinner",
        "period_ms",
        "Return the spinner frame period in milliseconds. Use as the redraw interval to match the built-in cadence.",
        &[],
        lua,
        |_, ()| Ok(smelt_core::content::SPINNER_FRAME_MS),
    )?;
    smelt.set("spinner", spinner_tbl)?;
    Ok(())
}
