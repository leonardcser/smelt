//! `smelt.reasoning` — `get / set / cycle` over `protocol::ReasoningEffort`
//! (Off / Low / Medium / High / Max). Mirrors `smelt.mode`; lives at
//! top-level so the surface stays symmetric.

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let reasoning_tbl = lua.create_table()?;

    reasoning_tbl.set(
        "get",
        app_read!(lua, |app| app
            .core
            .config
            .reasoning_effort
            .label()
            .to_string()),
    )?;

    reasoning_tbl.set(
        "set",
        lua.create_function(|_, v: String| {
            crate::lua::with_app(|app| match protocol::ReasoningEffort::parse(&v) {
                Some(effort) => app.set_reasoning_effort(effort),
                None => app.notify_error(format!("unknown reasoning effort: {v}")),
            });
            Ok(())
        })?,
    )?;

    reasoning_tbl.set(
        "cycle",
        lua.create_function(|_, ()| {
            crate::lua::with_app(|app| app.cycle_reasoning());
            Ok(())
        })?,
    )?;

    smelt.set("reasoning", reasoning_tbl)?;
    Ok(())
}
