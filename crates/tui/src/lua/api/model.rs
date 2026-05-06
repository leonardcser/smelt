//! `smelt.model` — `get / set / list` over the configured provider/model
//! triple. Mirrors `smelt.mode` / `smelt.reasoning`; lives at top-level so
//! `init.lua`'s `smelt.model.set(name)` reads naturally.

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let model_tbl = lua.create_table()?;

    model_tbl.set("get", app_read!(lua, |app| app.core.config.model.clone()))?;

    model_tbl.set(
        "set",
        lua.create_function(|_, v: String| {
            crate::lua::with_app(|app| app.apply_model(&v));
            Ok(())
        })?,
    )?;

    // `list()` returns an array of `{ key, name, provider }` entries
    // for the available models the user can switch to. Used by the
    // prompt-docked `/model` picker.
    model_tbl.set(
        "list",
        lua.create_function(|lua, ()| {
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
        })?,
    )?;

    smelt.set("model", model_tbl)?;
    Ok(())
}
