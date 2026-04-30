//! `smelt.vim` bindings — read the App-owned single-global `VimMode`.

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let vim_tbl = lua.create_table()?;
    vim_tbl.set("mode", app_read!(lua, |app| format!("{:?}", app.vim_mode)))?;
    smelt.set("vim", vim_tbl)?;
    Ok(())
}
