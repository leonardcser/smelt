//! `smelt.notify` — informational notifications in the status area.
//! Callable: `smelt.notify("msg")` shows an info toast; `smelt.notify.error("msg")`
//! shows an error toast (highlighted with the error color). UiHost-only.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;

#[lua_module(
    name = "smelt.notify",
    doc = "Status-area notifications. Call `smelt.notify(\"msg\")` for an info toast or `smelt.notify.error(\"msg\")` for an error toast. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let notify_tbl = lua.create_table()?;
    register_ui_fn(
        &notify_tbl,
        "smelt.notify",
        "error",
        "Show an error notification in the status area (highlighted with the error color).",
        &["msg"],
        lua,
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
    notify_tbl.set_metatable(Some(mt))?;
    smelt.set("notify", notify_tbl)?;
    Ok(())
}
