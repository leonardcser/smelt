//! `smelt.model` — callable selector for the configured provider/model.
//! `smelt.model()` reads the active key, `smelt.model(v)` switches,
//! `smelt.model.list()` returns the available models.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};

#[lua_module(
    name = "smelt.model",
    doc = "Model selector. `smelt.model()` reads, `smelt.model(v)` switches, `smelt.model.list()` lists available models."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    record_module_doc(
        "smelt.model",
        "Model selector. `smelt.model()` reads the active model key, `smelt.model(v)` switches, `smelt.model.list()` returns the available models.",
    );

    let model_tbl = lua.create_table()?;
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

    // `__call(v?)`: read when no arg, switch when arg.
    let f = lua.create_function(
        |lua, (_tbl, v): (mlua::Table, Option<String>)| -> LuaResult<mlua::Value> {
            match v {
                Some(name) => {
                    crate::lua::with_app(|app| app.apply_model(&name));
                    Ok(mlua::Value::Nil)
                }
                None => {
                    let cur = crate::lua::try_with_app(|app| app.core.config.model.clone())
                        .unwrap_or_default();
                    Ok(mlua::Value::String(lua.create_string(&cur)?))
                }
            }
        },
    )?;
    let mt = lua.create_table()?;
    mt.set("__call", f)?;
    model_tbl.set_metatable(Some(mt))?;

    smelt.set("model", model_tbl)?;
    Ok(())
}
