//! `smelt.ui.*` leftovers — ghost text on the prompt and shared
//! spinner glyph + cadence. Overlay/picker have their own top-level
//! constructors (`smelt.overlay`, `smelt.picker`); layout primitives
//! still live under `smelt.ui.layout` to keep them visually distinct
//! from the host-tier `smelt.layout` block layouts.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;

pub(super) fn register(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    register_ghost(lua, smelt_ui)?;
    register_spinner(lua, smelt_ui)?;
    super::ui_layout::register(lua, smelt_ui)?;
    Ok(())
}

#[lua_module(
    name = "smelt.ui.ghost",
    doc = "Ghost text on the prompt — dim suggestion shown after the cursor. UiHost-only."
)]
fn register_ghost(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    let ghost_tbl = lua.create_table()?;
    register_ui_fn(
        &ghost_tbl,
        "smelt.ui.ghost",
        "set",
        "Set the prompt's ghost text (the dim suggestion shown after the cursor). Replaces any existing ghost completion.",
        &["text"],
        lua,
        |_, text: String| -> LuaResult<()> {
            crate::lua::with_app(|app| app.set_prompt_completer(text));
            Ok(())
        },
    )?;
    register_ui_fn(
        &ghost_tbl,
        "smelt.ui.ghost",
        "clear",
        "Clear the prompt's ghost text. Idempotent.",
        &[],
        lua,
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| app.clear_prompt_completer());
            Ok(())
        },
    )?;
    smelt_ui.set("ghost", ghost_tbl)?;
    Ok(())
}

#[lua_module(
    name = "smelt.ui.spinner",
    doc = "Shared spinner glyph and cadence for plugin animations. UiHost-only."
)]
fn register_spinner(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    let spinner_tbl = lua.create_table()?;
    register_ui_fn(
        &spinner_tbl,
        "smelt.ui.spinner",
        "glyph",
        "Return the current spinner glyph (single grapheme). Stays in sync with the status bar's working pill so plugin spinners animate together.",
        &[],
        lua,
        |_, ()| Ok(smelt_core::content::spinner_glyph()),
    )?;
    register_ui_fn(
        &spinner_tbl,
        "smelt.ui.spinner",
        "period_ms",
        "Return the spinner frame period in milliseconds. Use as the redraw interval to match the built-in cadence.",
        &[],
        lua,
        |_, ()| Ok(smelt_core::content::SPINNER_FRAME_MS),
    )?;
    smelt_ui.set("spinner", spinner_tbl)?;
    Ok(())
}
