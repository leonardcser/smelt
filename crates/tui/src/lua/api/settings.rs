//! `smelt.settings` - typed preferences (bool, number, or string) as
//! direct field access via `__index`/`__newindex`. Writes before app
//! init are stored in `LuaShared.settings_overrides` for later pickup.
//! Unknown keys raise at the access site; type mismatches raise on
//! assignment.
//!
//! The schema lives entirely in `smelt_core::config::SETTINGS` - this
//! module reads / writes through that table and adds no per-key code
//! of its own.

use mlua::prelude::*;
use smelt_core::config::{setting_decl, SettingKind, SettingValue, SETTINGS};
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

struct TableSettingDecl {
    key: &'static str,
    init: fn(&Lua) -> LuaResult<mlua::Table>,
}

const TABLE_SETTINGS: &[TableSettingDecl] = &[
    TableSettingDecl {
        key: "notifications",
        init: init_notifications_settings,
    },
    TableSettingDecl {
        key: "transcript",
        init: init_transcript_settings,
    },
];

fn table_setting_decl(key: &str) -> Option<&'static TableSettingDecl> {
    TABLE_SETTINGS.iter().find(|decl| decl.key == key)
}

fn init_notifications_settings(lua: &Lua) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    table.set("turn_end", false)?;
    Ok(table)
}

fn init_transcript_settings(lua: &Lua) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    table.set("view", lua.create_table()?)?;
    table.set("limits", lua.create_table()?)?;
    Ok(table)
}

fn unknown_key_err(key: &str) -> LuaError {
    let mut names: Vec<&str> = SETTINGS.iter().map(|d| d.key).collect();
    names.extend(TABLE_SETTINGS.iter().map(|decl| decl.key));
    LuaError::external(format!(
        "smelt.settings: unknown key `{key}`; known keys are {names:?}"
    ))
}

fn apply_setting(
    shared: &Arc<crate::lua::LuaShared>,
    key: String,
    parsed: SettingValue,
) -> LuaResult<()> {
    shared
        .settings_overrides
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key.clone(), parsed.clone());
    crate::lua::try_with_app(|app| {
        let effective = app
            .core
            .startup_overrides
            .settings
            .get(&key)
            .unwrap_or(&parsed);
        let mut settings = app.core.config.settings.clone();
        if settings.set(&key, effective).is_ok() {
            app.set_settings(settings);
        }
    });
    Ok(())
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
    if matches!(parsed, SettingValue::Number(number) if !number.is_finite()) {
        return Err(LuaError::external(format!(
            "smelt.settings.{key}: number must be finite"
        )));
    }
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
        "Metatable-backed proxy table for preferences. Read and write scalar keys directly (`settings.foo = true`, `settings.compact_threshold = 0.65`) or iterate with `pairs`. Values are typed per the schema; type mismatches raise. `settings.notifications` and `settings.transcript` are Lua tables for plugin preferences. UiHost-only.",
        Tier::UiHost,
    )?;
    for decl in TABLE_SETTINGS {
        settings_tbl.tbl.raw_set(decl.key, (decl.init)(lua)?)?;
    }
    let mt_tbl = lua.create_table()?;
    let mt = LuaMod::extend(lua, mt_tbl.clone(), "smelt.settings", Tier::UiHost);

    {
        let shared = Arc::clone(shared);
        mt.private_fn(
            "__index",
            &["_", "key"],
            move |lua, (_, key): (mlua::Value, String)| -> LuaResult<mlua::Value> {
                let Some(decl) = setting_decl(&key) else {
                    return Err(unknown_key_err(&key));
                };
                let desired = shared
                    .settings_overrides
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&key)
                    .cloned();
                if let Some(value) = desired.or_else(|| {
                    crate::lua::try_with_app(|app| (decl.read)(&app.core.config.settings))
                }) {
                    return setting_to_lua(lua, &value);
                }
                Err(LuaError::external(format!(
                    "smelt.settings.{key}: app not initialized"
                )))
            },
        )?;
    }

    {
        let shared = Arc::clone(shared);
        mt.private_fn(
            "__newindex",
            &["_", "key", "value"],
            move |_, (table, key, value): (mlua::Table, String, mlua::Value)| -> LuaResult<()> {
                if setting_decl(&key).is_none() {
                    if table_setting_decl(&key).is_some() {
                        let mlua::Value::Table(_) = &value else {
                            return Err(LuaError::external(format!(
                                "smelt.settings.{key}: expected table, got {}",
                                value.type_name()
                            )));
                        };
                        table.raw_set(key, value)?;
                        return Ok(());
                    }
                    return Err(unknown_key_err(&key));
                }
                let parsed = lua_to_setting(&key, value)?;
                apply_setting(&shared, key, parsed)
            },
        )?;
    }

    {
        let shared = Arc::clone(shared);
        mt.private_fn(
            "__pairs",
            &["settings"],
            move |lua,
                  settings: mlua::Table|
                  -> LuaResult<(mlua::Function, mlua::Value, mlua::Value)> {
                let shared = Arc::clone(&shared);
                let next =
                    lua.create_function(move |lua, (_, prev): (mlua::Value, mlua::Value)| {
                        let prev_key = match prev {
                            mlua::Value::String(s) => Some(s.to_string_lossy().to_string()),
                            _ => None,
                        };
                        let mut keys: Vec<String> =
                            SETTINGS.iter().map(|decl| decl.key.to_string()).collect();
                        keys.extend(TABLE_SETTINGS.iter().map(|decl| decl.key.to_string()));
                        keys.sort_by_key(|key| {
                            SETTINGS
                                .iter()
                                .position(|decl| decl.key == key.as_str())
                                .unwrap_or(SETTINGS.len())
                        });

                        let idx = match prev_key {
                            None => 0,
                            Some(k) => match keys.iter().position(|key| key == &k) {
                                Some(i) => i + 1,
                                None => keys.len(),
                            },
                        };
                        if idx >= keys.len() {
                            return Ok((mlua::Value::Nil, mlua::Value::Nil));
                        }
                        let key = &keys[idx];
                        if let Some(decl) = setting_decl(key) {
                            let desired = shared
                                .settings_overrides
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .get(key)
                                .cloned();
                            let value = desired.or_else(|| {
                                crate::lua::try_with_app(|app| {
                                    (decl.read)(&app.core.config.settings)
                                })
                            });
                            let value = match value {
                                Some(ref value) => setting_to_lua(lua, value)?,
                                None => mlua::Value::Nil,
                            };
                            return Ok((mlua::Value::String(lua.create_string(key)?), value));
                        }
                        let value = settings.raw_get::<mlua::Value>(key.as_str())?;
                        Ok((mlua::Value::String(lua.create_string(key)?), value))
                    })?;
                Ok((next, mlua::Value::Nil, mlua::Value::Nil))
            },
        )?;
    }

    settings_tbl.fn_(
        "schema",
        "Return the settings schema as an array of `{ key, kind, choices? }` rows. `kind` is `\"bool\"`, `\"number\"`, or `\"string\"`. `choices` is present for `\"string\"` keys with a closed value set. Useful for config tooling and introspection.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            for decl in SETTINGS {
                let row = lua.create_table()?;
                row.set("key", decl.key)?;
                row.set(
                    "kind",
                    match decl.kind {
                        SettingKind::Bool => "bool",
                        SettingKind::Number => "number",
                        SettingKind::String => "string",
                    },
                )?;
                if let Some(choices) = decl.choices {
                    let arr = lua.create_table()?;
                    for (i, c) in choices.iter().enumerate() {
                        arr.set(i + 1, *c)?;
                    }
                    row.set("choices", arr)?;
                }
                out.push(row)?;
            }
            Ok(out)
        },
    )?;

    settings_tbl.tbl.set_metatable(Some(mt_tbl))?;
    Ok(())
}
