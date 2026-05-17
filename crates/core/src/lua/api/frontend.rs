//! `smelt.frontend` — query which frontend (TUI vs headless) is running.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "frontend",
        "Query which frontend is active (TUI vs headless).",
        Tier::Host,
    )?;
    m.fn_(
        "kind",
        "Return the active frontend kind (e.g. `\"tui\"` or `\"headless\"`). Falls back to `\"tui\"` when no host is installed.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(crate::host::try_with_core(|core| core.frontend.as_str().to_string())
                .unwrap_or(crate::runtime::FrontendKind::Tui.as_str().to_string()))
        },
    )?;

    m.fn_(
        "is_interactive",
        "Return `true` when the frontend supports interactive prompts (a TTY user is present). Defaults to `true` when no host is installed.",
        &[],
        |_, ()| -> LuaResult<bool> {
            Ok(crate::host::try_with_core(|core| core.frontend.is_interactive()).unwrap_or(true))
        },
    )?;

    Ok(())
}
