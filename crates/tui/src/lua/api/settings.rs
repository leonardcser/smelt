//! `smelt.settings` — boolean preferences as direct field access via `__index`/`__newindex`.
//! Writes before app init are stored in `LuaShared.settings_overrides` for later pickup.
//! Unknown keys raise at the access site.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::config::{ResolvedSettings, SETTINGS_KEYS};
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};
use std::sync::Arc;

fn known(key: &str) -> bool {
    SETTINGS_KEYS.contains(&key)
}

fn unknown_key_err(key: &str) -> LuaError {
    LuaError::external(format!(
        "smelt.settings: unknown key `{key}`; known keys are {SETTINGS_KEYS:?}"
    ))
}

fn read_resolved(s: &ResolvedSettings, key: &str) -> Option<bool> {
    Some(match key {
        "vim" => s.vim,
        "auto_compact" => s.auto_compact,
        "show_tps" => s.show_tps,
        "show_tokens" => s.show_tokens,
        "show_cost" => s.show_cost,
        "show_prediction" => s.show_prediction,
        "show_slug" => s.show_slug,
        "show_thinking" => s.show_thinking,
        "restrict_to_workspace" => s.restrict_to_workspace,
        "redact_secrets" => s.redact_secrets,
        _ => return None,
    })
}

fn write_resolved(s: &mut ResolvedSettings, key: &str, value: bool) -> bool {
    match key {
        "vim" => s.vim = value,
        "auto_compact" => s.auto_compact = value,
        "show_tps" => s.show_tps = value,
        "show_tokens" => s.show_tokens = value,
        "show_cost" => s.show_cost = value,
        "show_prediction" => s.show_prediction = value,
        "show_slug" => s.show_slug = value,
        "show_thinking" => s.show_thinking = value,
        "restrict_to_workspace" => s.restrict_to_workspace = value,
        "redact_secrets" => s.redact_secrets = value,
        _ => return false,
    }
    true
}

#[lua_module]
pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let settings_tbl = lua.create_table()?;
    record_module_doc("smelt.settings", "Metatable-backed proxy table for boolean preferences. Read and write keys directly (`settings.foo = true`) or iterate with `pairs`. UiHost-only.");
    let mt = lua.create_table()?;

    register_ui_fn(
        &mt,
        "smelt.settings",
        "__index",
        "Read a boolean preference by `key` from the resolved settings. Raises if the app is not yet initialized or if `key` is not in `SETTINGS_KEYS`.",
        &["_", "key"],
        lua,
        |_, (_, key): (mlua::Value, String)| -> LuaResult<bool> {
            if !known(&key) {
                return Err(unknown_key_err(&key));
            }
            let v = crate::lua::try_with_app(|app| read_resolved(&app.core.config.settings, &key))
                .flatten();
            match v {
                Some(b) => Ok(b),
                None => Err(LuaError::external(format!(
                    "smelt.settings.{key}: app not initialized"
                ))),
            }
        },
    )?;

    {
        let shared = Arc::clone(shared);
        register_ui_fn(
            &mt,
            "smelt.settings",
            "__newindex",
            "Write a boolean preference. Persists to the running config when the app is initialized; otherwise stashes the write in `LuaShared.settings_overrides` for pickup at init time. Raises on unknown keys.",
            &["_", "key", "value"],
            lua,
            move |_, (_, key, value): (mlua::Value, String, bool)|  -> LuaResult<()>{
                if !known(&key) {
                    return Err(unknown_key_err(&key));
                }
                let applied = crate::lua::try_with_app(|app| {
                    let mut s = app.core.config.settings.clone();
                    if !write_resolved(&mut s, &key, value) {
                        return false;
                    }
                    app.set_settings(s);
                    true
                })
                .unwrap_or(false);
                if !applied {
                    let mut overrides = shared
                        .settings_overrides
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    overrides.insert(key, value);
                }
                Ok(())
            },
        )?;
    }

    register_ui_fn(
        &mt,
        "smelt.settings",
        "__pairs",
        "Iterate every known settings key and its current resolved value as `(key, boolean)` pairs. Lets `for k, v in pairs(smelt.settings) do ... end` enumerate all preferences.",
        &["_"],
        lua,
        |lua, _: mlua::Value| -> LuaResult<(mlua::Function, mlua::Value, mlua::Value)> {
            let next = lua.create_function(|lua, (_, prev): (mlua::Value, mlua::Value)| {
                let prev_key = match prev {
                    mlua::Value::String(s) => Some(s.to_string_lossy().to_string()),
                    _ => None,
                };
                let idx = match prev_key {
                    None => 0,
                    Some(k) => match SETTINGS_KEYS.iter().position(|s| *s == k.as_str()) {
                        Some(i) => i + 1,
                        None => SETTINGS_KEYS.len(),
                    },
                };
                if idx >= SETTINGS_KEYS.len() {
                    return Ok((mlua::Value::Nil, mlua::Value::Nil));
                }
                let key = SETTINGS_KEYS[idx];
                let value =
                    crate::lua::try_with_app(|app| read_resolved(&app.core.config.settings, key))
                        .flatten();
                let v = match value {
                    Some(b) => mlua::Value::Boolean(b),
                    None => mlua::Value::Nil,
                };
                Ok((mlua::Value::String(lua.create_string(key)?), v))
            })?;
            Ok((next, mlua::Value::Nil, mlua::Value::Nil))
        },
    )?;

    settings_tbl.set_metatable(Some(mt))?;
    smelt.set("settings", settings_tbl)?;
    Ok(())
}
