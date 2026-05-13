//! `smelt.model` — `get / set / list` over the configured provider/model
//! triple. Mirrors `smelt.mode` / `smelt.reasoning`; lives at top-level so
//! `init.lua`'s `smelt.model.set(name)` reads naturally.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;

#[lua_module(
    name = "smelt.model",
    doc = "Get, set, and list the configured provider/model triple. Mirrors smelt.mode and smelt.reasoning."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let model_tbl = lua.create_table()?;
    register_ui_fn(
        &model_tbl,
        "smelt.model",
        "get",
        "Return the active model key (matches an entry in `list()`).",
        &[],
        lua,
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| app.core.config.model.clone()).unwrap_or_default())
        },
    )?;

    register_ui_fn(
        &model_tbl,
        "smelt.model",
        "set",
        "Switch to model `v` by key. Re-resolves the provider/model triple, propagates the change to the engine, and persists it to session config.",
        &["v"],
        lua,
        |_, v: String|  -> LuaResult<()>{
            crate::lua::with_app(|app| app.apply_model(&v));
            Ok(())
        },
    )?;

    // `list()` returns `{key, name, provider}` entries for available models.
    register_ui_fn(
        &model_tbl,
        "smelt.model",
        "list",
        "Return an array of `{ key, name, provider }` records for every model the active config can switch to.",
        &[],
        lua,
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            if let Some(res) = crate::lua::try_with_app(|app| -> LuaResult<()> {
                for (i, m) in app.core.config.available_models.iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("key", m.key.clone())?;
                    entry.set("name", m.model_name.clone())?;
                    entry.set("provider", m.provider_name.clone())?;
                    out.set(i + 1, entry)?;
                }
                Ok(())
            }) {
                res?;
            }
            Ok(out)
        },
    )?;

    smelt.set("model", model_tbl)?;
    Ok(())
}
