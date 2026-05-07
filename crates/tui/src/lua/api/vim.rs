//! `smelt.vim` bindings — read and write the App-owned single-global `VimMode`.

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let vim_tbl = lua.create_table()?;
    vim_tbl.set("mode", app_read!(lua, |app| format!("{:?}", app.vim_mode)))?;
    vim_tbl.set(
        "set_mode",
        lua.create_function(|_, mode: String| {
            let target = match mode.as_str() {
                "Normal" | "normal" | "n" => crate::ui::VimMode::Normal,
                "Insert" | "insert" | "i" => crate::ui::VimMode::Insert,
                "Visual" | "visual" | "v" => crate::ui::VimMode::Visual,
                "VisualLine" | "visual_line" | "V" => crate::ui::VimMode::VisualLine,
                other => {
                    return Err(LuaError::RuntimeError(format!(
                        "smelt.vim.set_mode: unknown mode `{other}`"
                    )))
                }
            };
            crate::lua::with_app(|app| {
                if app.input.vim_enabled() {
                    app.input.set_vim_mode(&mut app.vim_mode, target);
                }
            });
            Ok(())
        })?,
    )?;
    smelt.set("vim", vim_tbl)?;
    Ok(())
}
