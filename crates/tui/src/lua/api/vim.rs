//! `smelt.vim` bindings — read and write the focused-pane `VimMode`.

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let vim_tbl = lua.create_table()?;
    vim_tbl.set(
        "mode",
        app_read!(lua, |app| {
            app.focused_vim_mode_label().unwrap_or_default()
        }),
    )?;
    vim_tbl.set(
        "set_mode",
        lua.create_function(|_, mode: String| {
            let target = match mode.as_str() {
                "Normal" | "normal" | "n" => crate::smelt_term::VimMode::Normal,
                "Insert" | "insert" | "i" => crate::smelt_term::VimMode::Insert,
                "Visual" | "visual" | "v" => crate::smelt_term::VimMode::Visual,
                "VisualLine" | "visual_line" | "V" => crate::smelt_term::VimMode::VisualLine,
                other => {
                    return Err(LuaError::RuntimeError(format!(
                        "smelt.vim.set_mode: unknown mode `{other}`"
                    )))
                }
            };
            crate::lua::with_app(|app| {
                app.set_focused_vim_mode(target);
            });
            Ok(())
        })?,
    )?;
    smelt.set("vim", vim_tbl)?;
    Ok(())
}
