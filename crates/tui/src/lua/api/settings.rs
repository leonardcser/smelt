//! `smelt.settings` — typed preferences (bool, number, or string) as
//! direct field access via `__index`/`__newindex`. Writes before app
//! init are stored in `LuaShared.settings_overrides` for later pickup.
//! Unknown keys raise at the access site; type mismatches raise on
//! assignment.
//!
//! The schema lives entirely in `smelt_core::config::SETTINGS` — this
//! module reads / writes through that table and adds no per-key code
//! of its own.

use mlua::prelude::*;
use smelt_core::config::{setting_decl, SettingKind, SettingValue, SETTINGS};
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

fn unknown_key_err(key: &str) -> LuaError {
    let names: Vec<&str> = SETTINGS.iter().map(|d| d.key).collect();
    LuaError::external(format!(
        "smelt.settings: unknown key `{key}`; known keys are {names:?}"
    ))
}

fn setting_to_lua(lua: &Lua, value: &SettingValue) -> LuaResult<mlua::Value> {
    Ok(match value {
        SettingValue::Bool(b) => mlua::Value::Boolean(*b),
        SettingValue::Number(n) => mlua::Value::Number(*n),
        SettingValue::String(s) => mlua::Value::String(lua.create_string(s)?),
    })
}

fn lua_to_setting(key: &str, value: mlua::Value) -> LuaResult<SettingValue> {
    let decl = setting_decl(key).ok_or_else(|| unknown_key_err(key))?;
    let parsed = match (decl.kind, value) {
        (SettingKind::Bool, mlua::Value::Boolean(b)) => SettingValue::Bool(b),
        (SettingKind::Number, mlua::Value::Number(n)) => SettingValue::Number(n),
        (SettingKind::Number, mlua::Value::Integer(i)) => SettingValue::Number(i as f64),
        (SettingKind::String, mlua::Value::String(s)) => {
            SettingValue::String(s.to_string_lossy().to_string())
        }
        (expected, got) => {
            return Err(LuaError::external(format!(
                "smelt.settings.{key}: expected {expected:?}, got {}",
                got.type_name()
            )))
        }
    };
    // Validate string choices up front so the error references the Lua
    // call site, not an internal write.
    if let (SettingValue::String(ref s), Some(choices)) = (&parsed, decl.choices) {
        if !choices.contains(&s.as_str()) {
            return Err(LuaError::external(format!(
                "smelt.settings.{key}: '{s}' is not one of {choices:?}"
            )));
        }
    }
    Ok(parsed)
}

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let settings_tbl = LuaMod::under(
        lua,
        smelt,
        "settings",
        "Metatable-backed proxy table for preferences. Read and write keys directly (`settings.foo = true`, `settings.compact_threshold = 0.65`) or iterate with `pairs`. Values are typed per the schema; type mismatches raise. UiHost-only.",
        Tier::UiHost,
    )?;
    let mt_tbl = lua.create_table()?;
    let mt = LuaMod::extend(lua, mt_tbl.clone(), "smelt.settings", Tier::UiHost);

    mt.fn_(
        "__index",
        "Read a preference by `key` from the resolved settings. Raises if the app is not yet initialized or if `key` is not in the schema.",
        &["_", "key"],
        |lua, (_, key): (mlua::Value, String)| -> LuaResult<mlua::Value> {
            let Some(decl) = setting_decl(&key) else {
                return Err(unknown_key_err(&key));
            };
            let v = crate::lua::try_with_app(|app| (decl.read)(&app.core.config.settings));
            match v {
                Some(val) => setting_to_lua(lua, &val),
                None => Err(LuaError::external(format!(
                    "smelt.settings.{key}: app not initialized"
                ))),
            }
        },
    )?;

    {
        let shared = Arc::clone(shared);
        mt.fn_(
            "__newindex",
            "Write a preference. Persists to the running config when the app is initialized; otherwise stashes the write in `LuaShared.settings_overrides` for pickup at init time. Raises on unknown keys or type mismatches.",
            &["_", "key", "value"],
            move |_, (_, key, value): (mlua::Value, String, mlua::Value)| -> LuaResult<()> {
                let parsed = lua_to_setting(&key, value)?;
                let applied = crate::lua::try_with_app(|app| {
                    let mut s = app.core.config.settings.clone();
                    if s.set(&key, &parsed).is_err() {
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
                    overrides.insert(key, parsed);
                }
                Ok(())
            },
        )?;
    }

    mt.fn_(
        "__pairs",
        "Iterate every known settings key and its current resolved value as `(key, value)` pairs. Value type matches the key's declared kind.",
        &["_"],
        |lua, _: mlua::Value| -> LuaResult<(mlua::Function, mlua::Value, mlua::Value)> {
            let next = lua.create_function(|lua, (_, prev): (mlua::Value, mlua::Value)| {
                let prev_key = match prev {
                    mlua::Value::String(s) => Some(s.to_string_lossy().to_string()),
                    _ => None,
                };
                let idx = match prev_key {
                    None => 0,
                    Some(k) => match SETTINGS.iter().position(|d| d.key == k.as_str()) {
                        Some(i) => i + 1,
                        None => SETTINGS.len(),
                    },
                };
                if idx >= SETTINGS.len() {
                    return Ok((mlua::Value::Nil, mlua::Value::Nil));
                }
                let decl = &SETTINGS[idx];
                let value =
                    crate::lua::try_with_app(|app| (decl.read)(&app.core.config.settings));
                let v = match value {
                    Some(ref val) => setting_to_lua(lua, val)?,
                    None => mlua::Value::Nil,
                };
                Ok((mlua::Value::String(lua.create_string(decl.key)?), v))
            })?;
            Ok((next, mlua::Value::Nil, mlua::Value::Nil))
        },
    )?;

    settings_tbl.tbl.set_metatable(Some(mt_tbl))?;
    Ok(())
}
