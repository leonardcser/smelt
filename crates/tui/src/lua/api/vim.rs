//! `smelt.vim` bindings — read and write the App-owned single-global `VimMode`.

use lua_doc_derive::{lua_module, LuaAlias};
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};

/// Vim mode string literal.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.vim.Mode", mirror = "crate::smelt_term::VimMode")]
pub enum LuaVimMode {
    Insert,
    Normal,
    Visual,
    VisualLine,
}

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let vim_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.vim",
        "Read and write the app-owned vim mode state. UiHost-only.",
    );

    register_ui_fn(
        &vim_tbl,
        "smelt.vim",
        "mode",
        "Return the current vim mode.",
        &[],
        lua,
        |_, ()| -> LuaResult<LuaVimMode> {
            Ok(
                crate::lua::try_with_app(|app| LuaVimMode::from(app.vim_mode))
                    .unwrap_or(LuaVimMode::Normal),
            )
        },
    )?;
    register_ui_fn(
        &vim_tbl,
        "smelt.vim",
        "set_mode",
        "Switch the vim mode of the focused vim-enabled window. Raises on unknown values.",
        &["mode"],
        lua,
        |_, mode: LuaVimMode| -> LuaResult<()> {
            let target = mode.into();
            crate::lua::with_app(|app| {
                if app.input.vim_enabled() {
                    app.input.set_vim_mode(&mut app.vim_mode, target);
                }
            });
            Ok(())
        },
    )?;
    smelt.set("vim", vim_tbl)?;
    Ok(())
}
