//! `smelt.clipboard` — read / write the system clipboard. Both
//! call into `app.core.clipboard.{read,write}` so every text I/O
//! routes through the App-level Clipboard subsystem.
//!
//! `smelt.clipboard(text)` is the legacy callable form (treated as
//! a write); `smelt.clipboard.write(text)` and `smelt.clipboard.read()`
//! are the explicit verbs.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    // Build a metatable so `smelt.clipboard("text")` keeps working
    // (calling the table writes), while `.read` / `.write` give
    // explicit access.
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

    let mt = lua.create_table()?;
    mt.set(
        "__call",
        lua.create_function(|_, (_self, text): (mlua::Table, String)| {
            crate::lua::with_app(|app| app.core.clipboard.write(&text))
                .map_err(LuaError::RuntimeError)?;
            Ok(())
        })?,
    )?;
    clipboard_tbl.set_metatable(Some(mt))?;

    smelt.set("clipboard", clipboard_tbl)?;
    Ok(())
}
