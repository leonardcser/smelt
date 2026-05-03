//! `smelt.frontend` bindings — read which frontend wraps the running
//! `Core` so tools can branch between the human-facing TUI and the
//! headless paths.
//!
//! Today only `TuiApp` installs the TLS app pointer, so reads from
//! Lua always see `Tui`. When `HeadlessApp` gains a Lua driver
//! (P2.b.5b), the same TLS slot will carry either frontend and the
//! same binding will dispatch to the right `frontend` field — no
//! signature change.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let frontend_tbl = lua.create_table()?;

    frontend_tbl.set(
        "kind",
        lua.create_function(|_, ()| {
            Ok(crate::lua::try_with_app(|app| app.core.frontend.as_str())
                .unwrap_or(crate::core::FrontendKind::Tui.as_str()))
        })?,
    )?;

    frontend_tbl.set(
        "is_interactive",
        lua.create_function(|_, ()| {
            Ok(crate::lua::try_with_app(|app| app.core.frontend.is_interactive()).unwrap_or(true))
        })?,
    )?;

    smelt.set("frontend", frontend_tbl)?;
    Ok(())
}
