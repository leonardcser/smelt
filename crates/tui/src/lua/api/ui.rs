//! `smelt.ui.*` overlay primitives — ghost text on the prompt,
//! shared spinner glyph + cadence, picker overlay (set_items /
//! set_selected / _open), dialog overlay (_open). UiHost-only.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    register_ghost_text(lua, smelt_ui)?;
    register_spinner(lua, smelt_ui)?;
    register_picker(lua, smelt_ui)?;
    register_dialog(lua, smelt_ui)?;
    Ok(())
}

fn register_ghost_text(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    let ghost_text_tbl = lua.create_table()?;
    ghost_text_tbl.set(
        "set",
        lua.create_function(|_, text: String| {
            crate::lua::with_app(|app| app.set_prompt_completer(text));
            Ok(())
        })?,
    )?;
    ghost_text_tbl.set(
        "clear",
        lua.create_function(|_, ()| {
            crate::lua::with_app(|app| app.clear_prompt_completer());
            Ok(())
        })?,
    )?;
    smelt_ui.set("ghost_text", ghost_text_tbl)?;
    Ok(())
}

fn register_spinner(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    // Same glyph set and cadence the status bar uses for its
    // "working" pill, exposed as primitives so Lua plugins (e.g.
    // /btw's "thinking" placeholder) can animate in lockstep with
    // the rest of the UI. Lua drives the animation via
    // `smelt.defer(period_ms, tick)`; `glyph()` returns the current
    // frame without any server-side state.
    let spinner_tbl = lua.create_table()?;
    spinner_tbl.set(
        "glyph",
        lua.create_function(|_, ()| Ok(crate::content::spinner_glyph()))?,
    )?;
    spinner_tbl.set(
        "period_ms",
        lua.create_function(|_, ()| Ok(crate::content::SPINNER_FRAME_MS))?,
    )?;
    smelt_ui.set("spinner", spinner_tbl)?;
    Ok(())
}

fn register_picker(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    let picker_tbl = lua.create_table()?;
    picker_tbl.set(
        "set_selected",
        lua.create_function(|_, (win_id, idx): (u64, i64)| {
            let index = if idx < 0 { 0 } else { idx as usize };
            crate::lua::with_app(|app| {
                crate::picker::set_selected(app, ui::WinId(win_id), index);
            });
            Ok(())
        })?,
    )?;
    picker_tbl.set(
        "_open",
        lua.create_function(|_, opts: mlua::Table| -> LuaResult<u64> {
            let win_id = crate::lua::with_app(|app| crate::lua::ui_ops::open_picker(app, opts))
                .map_err(|e| LuaError::RuntimeError(format!("picker.open: {e}")))?;
            Ok(win_id.0)
        })?,
    )?;
    picker_tbl.set(
        "set_items",
        lua.create_function(|_, (win_id, items_tbl): (u64, mlua::Table)| {
            let mut items = Vec::new();
            for pair in items_tbl.sequence_values::<mlua::Value>() {
                let v = pair?;
                let it =
                    crate::lua::ui_ops::parse_picker_item(&v).map_err(LuaError::RuntimeError)?;
                items.push(it);
            }
            crate::lua::with_app(|app| {
                crate::picker::set_items(app, ui::WinId(win_id), items, 0);
            });
            Ok(())
        })?,
    )?;
    smelt_ui.set("picker", picker_tbl)?;
    Ok(())
}

fn register_dialog(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    let dialog_tbl = lua.create_table()?;

    // smelt.ui.dialog._open(opts) → (win_id, leaves).
    // `leaves` is a sequence parallel to `opts.panels`, holding the
    // leaf WinId opened for each panel. `dialog.lua`'s `make_handle`
    // pairs each spec with its leaf so panel handles can drive
    // focus + per-panel queries (e.g. input `:text()`) through the
    // standard `smelt.win.*` / `smelt.buf.*` surface.
    dialog_tbl.set(
        "_open",
        lua.create_function(|lua, opts: mlua::Table| -> LuaResult<(u64, mlua::Table)> {
            let result = crate::lua::with_app(|app| crate::lua::ui_ops::open_dialog(app, opts))
                .map_err(|e| LuaError::RuntimeError(format!("dialog.open: {e}")))?;
            let leaves = lua.create_table()?;
            for (i, win) in result.leaves.iter().enumerate() {
                leaves.set(i + 1, win.0)?;
            }
            Ok((result.root.0, leaves))
        })?,
    )?;

    smelt_ui.set("dialog", dialog_tbl)?;
    Ok(())
}
