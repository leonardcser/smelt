//! `smelt.clipboard` — read / write the system clipboard. Both call
//! into `app.core.clipboard.{read,write}` so every text I/O routes
//! through the App-level Clipboard subsystem.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let clipboard_tbl = lua.create_table()?;
    clipboard_tbl.set(
        "write",
        lua.create_function(|_, text: String| {
            crate::lua::with_app(|app| app.core.clipboard.write(&text))
                .map_err(LuaError::RuntimeError)?;
            Ok(())
        })?,
    )?;
    clipboard_tbl.set(
        "read",
        lua.create_function(|_, ()| {
            Ok(crate::lua::try_with_app(|app| app.core.clipboard.read()).flatten())
        })?,
    )?;

    smelt.set("clipboard", clipboard_tbl)?;
    Ok(())
}
