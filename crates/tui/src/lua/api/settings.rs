//! `smelt.settings` — typed preferences (bool or number) as direct field
//! access via `__index`/`__newindex`. Writes before app init are stored
//! in `LuaShared.settings_overrides` for later pickup. Unknown keys raise
//! at the access site; type mismatches raise on assignment.

use mlua::prelude::*;
use smelt_core::config::{
    setting_kind, ResolvedSettings, SettingKind, SettingValue, SETTINGS_KEYS,
};
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

fn unknown_key_err(key: &str) -> LuaError {
    let names: Vec<&str> = SETTINGS_KEYS.iter().map(|(k, _)| *k).collect();
    LuaError::external(format!(
        "smelt.settings: unknown key `{key}`; known keys are {names:?}"
    ))
}

fn read_resolved(s: &ResolvedSettings, key: &str) -> Option<SettingValue> {
    Some(match key {
        "vim" => SettingValue::Bool(s.vim),
        "auto_compact" => SettingValue::Bool(s.auto_compact),
        "show_tps" => SettingValue::Bool(s.show_tps),
        "show_tokens" => SettingValue::Bool(s.show_tokens),
        "show_cost" => SettingValue::Bool(s.show_cost),
        "show_prediction" => SettingValue::Bool(s.show_prediction),
        "show_slug" => SettingValue::Bool(s.show_slug),
        "show_thinking" => SettingValue::Bool(s.show_thinking),
        "restrict_to_workspace" => SettingValue::Bool(s.restrict_to_workspace),
        "redact_secrets" => SettingValue::Bool(s.redact_secrets),
        "auto_reload" => SettingValue::Bool(s.auto_reload),
        "compact_threshold" => SettingValue::Number(s.compact_threshold),
        _ => return None,
    })
}

fn write_resolved(s: &mut ResolvedSettings, key: &str, value: &SettingValue) -> bool {
    match (key, value) {
        ("vim", SettingValue::Bool(v)) => s.vim = *v,
        ("auto_compact", SettingValue::Bool(v)) => s.auto_compact = *v,
        ("show_tps", SettingValue::Bool(v)) => s.show_tps = *v,
        ("show_tokens", SettingValue::Bool(v)) => s.show_tokens = *v,
        ("show_cost", SettingValue::Bool(v)) => s.show_cost = *v,
        ("show_prediction", SettingValue::Bool(v)) => s.show_prediction = *v,
        ("show_slug", SettingValue::Bool(v)) => s.show_slug = *v,
        ("show_thinking", SettingValue::Bool(v)) => s.show_thinking = *v,
        ("restrict_to_workspace", SettingValue::Bool(v)) => s.restrict_to_workspace = *v,
        ("redact_secrets", SettingValue::Bool(v)) => s.redact_secrets = *v,
        ("auto_reload", SettingValue::Bool(v)) => s.auto_reload = *v,
        ("compact_threshold", SettingValue::Number(v)) => s.compact_threshold = *v,
        _ => return false,
    }
    true
}

fn setting_to_lua(_lua: &Lua, value: &SettingValue) -> LuaResult<mlua::Value> {
    Ok(match value {
        SettingValue::Bool(b) => mlua::Value::Boolean(*b),
        SettingValue::Number(n) => mlua::Value::Number(*n),
    })
}

fn lua_to_setting(key: &str, value: mlua::Value) -> LuaResult<SettingValue> {
    let kind = setting_kind(key).ok_or_else(|| unknown_key_err(key))?;
    match (kind, value) {
        (SettingKind::Bool, mlua::Value::Boolean(b)) => Ok(SettingValue::Bool(b)),
        (SettingKind::Number, mlua::Value::Number(n)) => Ok(SettingValue::Number(n)),
        (SettingKind::Number, mlua::Value::Integer(i)) => Ok(SettingValue::Number(i as f64)),
        (expected, got) => Err(LuaError::external(format!(
            "smelt.settings.{key}: expected {expected:?}, got {}",
            got.type_name()
        ))),
    }
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
        "Read a preference by `key` from the resolved settings. Raises if the app is not yet initialized or if `key` is not in `SETTINGS_KEYS`.",
        &["_", "key"],
        |lua, (_, key): (mlua::Value, String)| -> LuaResult<mlua::Value> {
            if setting_kind(&key).is_none() {
                return Err(unknown_key_err(&key));
            }
            let v = crate::lua::try_with_app(|app| read_resolved(&app.core.config.settings, &key))
                .flatten();
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
                    if !write_resolved(&mut s, &key, &parsed) {
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
        "Iterate every known settings key and its current resolved value as `(key, value)` pairs. Value type matches the key's declared kind (bool or number).",
        &["_"],
        |lua, _: mlua::Value| -> LuaResult<(mlua::Function, mlua::Value, mlua::Value)> {
            let next = lua.create_function(|lua, (_, prev): (mlua::Value, mlua::Value)| {
                let prev_key = match prev {
                    mlua::Value::String(s) => Some(s.to_string_lossy().to_string()),
                    _ => None,
                };
                let idx = match prev_key {
                    None => 0,
                    Some(k) => match SETTINGS_KEYS.iter().position(|(s, _)| *s == k.as_str()) {
                        Some(i) => i + 1,
                        None => SETTINGS_KEYS.len(),
                    },
                };
                if idx >= SETTINGS_KEYS.len() {
                    return Ok((mlua::Value::Nil, mlua::Value::Nil));
                }
                let (key, _) = SETTINGS_KEYS[idx];
                let value =
                    crate::lua::try_with_app(|app| read_resolved(&app.core.config.settings, key))
                        .flatten();
                let v = match value {
                    Some(ref val) => setting_to_lua(lua, val)?,
                    None => mlua::Value::Nil,
                };
                Ok((mlua::Value::String(lua.create_string(key)?), v))
            })?;
            Ok((next, mlua::Value::Nil, mlua::Value::Nil))
        },
    )?;

    settings_tbl.tbl.set_metatable(Some(mt_tbl))?;
    Ok(())
}
