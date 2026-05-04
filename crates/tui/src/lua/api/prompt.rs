//! `smelt.prompt` bindings — the main editable input surface.
//!
//! `win_id()` returns the stable `WinId` so plugins can reuse
//! `smelt.win.on_event(prompt, "text_changed", …)` and
//! `smelt.win.set_keymap(prompt, …)`. `text()` snapshots the
//! current buffer; `set_text(s)` replaces it.

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let prompt_tbl = lua.create_table()?;
    prompt_tbl.set(
        "win_id",
        lua.create_function(|_, ()| Ok(crate::app::PROMPT_WIN.0))?,
    )?;
    prompt_tbl.set("text", app_read!(lua, |app| app.input.win.text.clone()))?;
    prompt_tbl.set(
        "set_text",
        lua.create_function(|_, text: String| {
            crate::lua::with_app(|app| {
                let mode = app.vim_mode;
                crate::api::buf::replace(&mut app.input, text, None, mode);
            });
            Ok(())
        })?,
    )?;
    prompt_tbl.set(
        "set_section",
        lua.create_function(|_, (name, content): (String, String)| {
            crate::lua::with_app(|app| app.prompt_sections.set(&name, content));
            Ok(())
        })?,
    )?;
    prompt_tbl.set(
        "remove_section",
        lua.create_function(|_, name: String| {
            crate::lua::with_app(|app| app.prompt_sections.remove(&name));
            Ok(())
        })?,
    )?;
    smelt.set("prompt", prompt_tbl)?;
    Ok(())
}
