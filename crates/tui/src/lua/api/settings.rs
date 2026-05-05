//! `smelt.settings` — boolean preferences exposed as direct field
//! access. Reads (`local v = smelt.settings.vim`) hit the live
//! [`ResolvedSettings`]; writes (`smelt.settings.vim = true`) toggle
//! the live setting at runtime, or — when the app isn't live yet
//! (config-time `init.lua`) — store the value into
//! `LuaShared.settings_overrides` for `Config::from_lua_shared` to
//! pick up.
//!
//! Unknown keys raise an error at the access site so a typo can't
//! silently lose an override. The known set lives in
//! [`smelt_core::config::SETTINGS_KEYS`] and matches the field names
//! on [`smelt_core::state::ResolvedSettings`].

use mlua::prelude::*;
use smelt_core::config::SETTINGS_KEYS;
use smelt_core::state::ResolvedSettings;
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

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let settings_tbl = lua.create_table()?;
    let mt = lua.create_table()?;

    mt.set(
        "__index",
        lua.create_function(|_, (_, key): (mlua::Value, String)| {
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
        })?,
    )?;

    mt.set(
        "__newindex",
        lua.create_function({
            let shared = Arc::clone(shared);
            move |_, (_, key, value): (mlua::Value, String, bool)| {
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
            }
        })?,
    )?;

    mt.set(
        "__pairs",
        lua.create_function(|lua, _: mlua::Value| {
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
                let value = crate::lua::try_with_app(|app| {
                    read_resolved(&app.core.config.settings, key)
                })
                .flatten();
                let v = match value {
                    Some(b) => mlua::Value::Boolean(b),
                    None => mlua::Value::Nil,
                };
                Ok((mlua::Value::String(lua.create_string(key)?), v))
            })?;
            Ok((next, mlua::Value::Nil, mlua::Value::Nil))
        })?,
    )?;

    settings_tbl.set_metatable(Some(mt))?;
    smelt.set("settings", settings_tbl)?;
    Ok(())
}
