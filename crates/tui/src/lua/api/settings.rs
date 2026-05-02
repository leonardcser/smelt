//! `smelt.settings` bindings — user preference booleans (vim,
//! auto-compact, etc.). `snapshot()` returns the current state as a
//! table; `toggle(key)` flips one by name. Used by `/settings` to
//! build its picker entirely in Lua.

use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
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
    settings_tbl.set(
        "set",
        lua.create_function({
            let shared = Arc::clone(shared);
            move |_, (key, value): (String, mlua::Value)| {
                let value_str = match value {
                    mlua::Value::Boolean(b) => b.to_string(),
                    mlua::Value::Integer(i) => i.to_string(),
                    mlua::Value::Number(n) => n.to_string(),
                    mlua::Value::String(s) => s.to_str()?.to_string(),
                    _ => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "unsupported settings value type for {key}"
                        )))
                    }
                };
                let mut overrides = shared
                    .settings_overrides
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                overrides.insert(key, value_str);
                Ok(())
            }
        })?,
    )?;
    smelt.set("settings", settings_tbl)?;
    Ok(())
}
