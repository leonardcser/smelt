//! `smelt.frontend` — query which frontend (TUI vs headless) is running.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let frontend_tbl = lua.create_table()?;

    frontend_tbl.set(
        "kind",
        lua.create_function(|_, ()| {
            Ok(crate::host::try_with_core(|core| core.frontend.as_str())
                .unwrap_or(crate::runtime::FrontendKind::Tui.as_str()))
        })?,
    )?;

    frontend_tbl.set(
        "is_interactive",
        lua.create_function(|_, ()| {
            Ok(crate::host::try_with_core(|core| core.frontend.is_interactive()).unwrap_or(true))
        })?,
    )?;

    smelt.set("frontend", frontend_tbl)?;
    Ok(())
}
