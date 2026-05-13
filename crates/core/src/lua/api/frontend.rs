//! `smelt.frontend` — query which frontend (TUI vs headless) is running.

use crate::lua::doc::register_fn;
use lua_doc_derive::lua_module;
use mlua::prelude::*;

#[lua_module(
    name = "smelt.frontend",
    doc = "Query which frontend is active (TUI vs headless)."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let frontend_tbl = lua.create_table()?;
    register_fn(
        &frontend_tbl,
        "smelt.frontend",
        "kind",
        "Return the active frontend kind (e.g. `\"tui\"` or `\"headless\"`). Falls back to `\"tui\"` when no host is installed.",
        &[],
        lua,
        |_, ()| -> LuaResult<String> {
            Ok(crate::host::try_with_core(|core| core.frontend.as_str().to_string())
                .unwrap_or(crate::runtime::FrontendKind::Tui.as_str().to_string()))
        },
    )?;

    register_fn(
        &frontend_tbl,
        "smelt.frontend",
        "is_interactive",
        "Return `true` when the frontend supports interactive prompts (a TTY user is present). Defaults to `true` when no host is installed.",
        &[],
        lua,
        |_, ()| -> LuaResult<bool> {
            Ok(crate::host::try_with_core(|core| core.frontend.is_interactive()).unwrap_or(true))
        },
    )?;

    smelt.set("frontend", frontend_tbl)?;
    Ok(())
}
