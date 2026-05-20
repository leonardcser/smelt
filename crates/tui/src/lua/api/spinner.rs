//! `smelt.spinner` — shared spinner glyph and cadence so plugin
//! animations stay in sync with the built-in working indicator, plus a
//! per-app busy-token stack that drives the prompt top-bar indicator
//! when long-running background work is in flight. UiHost-only.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "spinner",
        "Shared spinner glyph and cadence for plugin animations, plus a busy-token stack so long-running background work surfaces in the prompt top-bar indicator. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "glyph",
        "Return the current spinner glyph (single grapheme). Stays in sync with the prompt top-bar working indicator so plugin spinners animate together.",
        &[],
        |_, ()| Ok(smelt_core::content::spinner_glyph()),
    )?;
    m.fn_(
        "period_ms",
        "Return the spinner frame period in milliseconds. Use as the redraw interval to match the built-in cadence.",
        &[],
        |_, ()| Ok(smelt_core::content::SPINNER_FRAME_MS),
    )?;
    m.fn_(
        "busy",
        "Push a busy token onto the per-app stack and return a `Reg` whose `:remove()` pops it. While any token is live, the prompt top-bar indicator shows the spinner with the top token's `label`. Multiple plugins can hold tokens concurrently; the most recently pushed label wins.",
        &["label"],
        |_, label: String| -> LuaResult<LuaReg> {
            let id = crate::lua::with_app(|app| app.busy_stack.push(label));
            Ok(LuaReg::new(move || {
                crate::lua::try_with_app(|app| app.busy_stack.release(id)).unwrap_or(false)
            }))
        },
    )?;
    m.fn_(
        "is_busy",
        "Return `true` while at least one `smelt.spinner.busy` token is live.",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.busy_stack.is_busy()).unwrap_or(false)),
    )?;
    m.fn_(
        "busy_label",
        "Return the top busy-stack label, or `nil` when nothing is busy.",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.busy_stack.top_label()).flatten()),
    )?;
    Ok(())
}
