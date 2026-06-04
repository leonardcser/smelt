//! `smelt.vim` bindings - read and write the focused-pane `VimMode`.

use lua_doc_derive::LuaAlias;
use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

/// Vim mode string literal.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.vim.Mode", mirror = "crate::smelt_edit::VimMode")]
pub enum LuaVimMode {
    Insert,
    Normal,
    Visual,
    VisualLine,
}

impl LuaVimMode {
    /// Snake-case label matching the `smelt.vim.Mode` Lua alias. Use this
    /// when emitting vim-mode strings into Lua-visible payloads so the
    /// value compares equal to what `smelt.vim.mode()` would return.
    pub fn label(self) -> &'static str {
        match self {
            LuaVimMode::Insert => "insert",
            LuaVimMode::Normal => "normal",
            LuaVimMode::Visual => "visual",
            LuaVimMode::VisualLine => "visual_line",
        }
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "vim",
        "Read and write the vim mode of the focused vim-enabled surface. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "mode",
        "Return the vim mode of the focused surface, or `nil` if it isn't vim-enabled.",
        &[],
        |_, ()| -> LuaResult<Option<LuaVimMode>> {
            Ok(
                crate::lua::try_with_app(|app| app.focused_vim_mode().map(LuaVimMode::from))
                    .flatten(),
            )
        },
    )?;
    m.fn_(
        "set_mode",
        "Switch the vim mode of the focused vim-enabled window. Raises on unknown values.",
        &["mode"],
        |_, mode: LuaVimMode| -> LuaResult<()> {
            let target = mode.into();
            crate::lua::with_app(|app| {
                app.set_focused_vim_mode(target);
            });
            Ok(())
        },
    )?;
    Ok(())
}
