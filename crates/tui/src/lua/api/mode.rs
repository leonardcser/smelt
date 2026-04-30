//! `smelt.mode` — `get / set / cycle` over `protocol::Mode`
//! (Plan / Apply / Yolo / Normal). Lives at top-level so it does
//! not collide with the future `smelt.subprocess` (sub-agents).

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let mode_tbl = lua.create_table()?;

    mode_tbl.set(
        "get",
        app_read!(lua, |app| app.core.config.mode.as_str().to_string()),
    )?;

    mode_tbl.set(
        "set",
        lua.create_function(|_, v: String| {
            crate::lua::with_app(|app| match protocol::Mode::parse(&v) {
                Some(mode) => app.set_mode(mode),
                None => app.notify_error(format!("unknown mode: {v}")),
            });
            Ok(())
        })?,
    )?;

    mode_tbl.set(
        "cycle",
        lua.create_function(|_, ()| {
            crate::lua::with_app(|app| app.toggle_mode());
            Ok(())
        })?,
    )?;

    smelt.set("mode", mode_tbl)?;
    Ok(())
}
