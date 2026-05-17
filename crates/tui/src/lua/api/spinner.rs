//! `smelt.spinner` — shared spinner glyph and cadence so plugin
//! animations stay in sync with the built-in status pill. UiHost-only.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "spinner",
        "Shared spinner glyph and cadence for plugin animations. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "glyph",
        "Return the current spinner glyph (single grapheme). Stays in sync with the status bar's working pill so plugin spinners animate together.",
        &[],
        |_, ()| Ok(smelt_core::content::spinner_glyph()),
    )?;
    m.fn_(
        "period_ms",
        "Return the spinner frame period in milliseconds. Use as the redraw interval to match the built-in cadence.",
        &[],
        |_, ()| Ok(smelt_core::content::SPINNER_FRAME_MS),
    )?;
    Ok(())
}
