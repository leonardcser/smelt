use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    LuaMod::under(
        lua,
        smelt,
        "search",
        "Search controls for the active UI session.",
        Tier::UiHost,
    )?
    .fn_(
        "clear",
        "Clear the active search session and remove search highlights from its target window.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| app.clear_search());
            Ok(())
        },
    )?;

    Ok(())
}
