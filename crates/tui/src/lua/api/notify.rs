//! `smelt.notify` — informational notifications in the status area.
//! Callable: `smelt.notify("msg")` shows an info toast; `smelt.notify.error("msg")`
//! shows an error toast (highlighted with the error color). UiHost-only.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "notify",
        "Status-area notifications. Call `smelt.notify(\"msg\")` for an info toast or `smelt.notify.error(\"msg\")` for an error toast. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "error",
        "Show an error notification in the status area (highlighted with the error color).",
        &["msg"],
        |_, msg: String| -> LuaResult<()> {
            crate::lua::with_app(|app| app.notify_error(msg));
            Ok(())
        },
    )?;
    // Callable: smelt.notify("msg") -> info toast.
    let call = lua.create_function(|_, (_tbl, msg): (mlua::Table, String)| -> LuaResult<()> {
        crate::lua::with_app(|app| app.notify(msg));
        Ok(())
    })?;
    let mt = lua.create_table()?;
    mt.set("__call", call)?;
    m.tbl.set_metatable(Some(mt))?;
    Ok(())
}
