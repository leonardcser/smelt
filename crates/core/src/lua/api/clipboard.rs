//! `smelt.clipboard` — read/write the system clipboard.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let clipboard_tbl = lua.create_table()?;
    clipboard_tbl.set(
        "write",
        lua.create_function(|_, text: String| {
            crate::host::with_core(|core| core.clipboard.write(&text))
                .map_err(LuaError::RuntimeError)?;
            Ok(())
        })?,
    )?;
    clipboard_tbl.set(
        "read",
        lua.create_function(|_, ()| {
            Ok(crate::host::try_with_core(|core| core.clipboard.read()).flatten())
        })?,
    )?;

    smelt.set("clipboard", clipboard_tbl)?;
    Ok(())
}
