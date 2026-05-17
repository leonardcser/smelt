//! `smelt.model` — callable selector for the configured provider/model.
//! `smelt.model()` reads the active key, `smelt.model(v)` switches,
//! `smelt.model.list()` returns the available models.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "model",
        "Model selector. `smelt.model()` reads the active model key, `smelt.model(v)` switches, `smelt.model.list()` returns the available models.",
        Tier::UiHost,
    )?;
    m.fn_(
        "list",
        "Return an array of `{ key, name, provider }` records for every model the active config can switch to.",
        &[],
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
    m.callable(
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
    Ok(())
}
