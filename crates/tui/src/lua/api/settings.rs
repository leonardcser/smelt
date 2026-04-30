//! `smelt.settings` bindings — user preference booleans (vim,
//! auto-compact, etc.). `snapshot()` returns the current state as a
//! table; `toggle(key)` flips one by name. Used by `/settings` to
//! build its picker entirely in Lua.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let settings_tbl = lua.create_table()?;
    settings_tbl.set(
        "snapshot",
        lua.create_function(|lua, ()| {
            let t = lua.create_table()?;
            if let Some(res) = crate::lua::try_with_app(|app| -> LuaResult<()> {
                let s = app.settings_state();
                t.set("vim", s.vim)?;
                t.set("auto_compact", s.auto_compact)?;
                t.set("show_tps", s.show_tps)?;
                t.set("show_tokens", s.show_tokens)?;
                t.set("show_cost", s.show_cost)?;
                t.set("show_prediction", s.show_prediction)?;
                t.set("show_slug", s.show_slug)?;
                t.set("show_thinking", s.show_thinking)?;
                t.set("restrict_to_workspace", s.restrict_to_workspace)?;
                t.set("redact_secrets", s.redact_secrets)?;
                Ok(())
            }) {
                res?;
            }
            Ok(t)
        })?,
    )?;
    settings_tbl.set(
        "toggle",
        lua.create_function(|_, v: String| {
            crate::lua::with_app(|app| app.toggle_named_setting(&v));
            Ok(())
        })?,
    )?;
    smelt.set("settings", settings_tbl)?;
    Ok(())
}
