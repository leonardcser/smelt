//! `smelt.provider` bindings — config-time provider registration.
//!
//! `smelt.provider.register(name, { type, api_base, api_key_env, models })`
//! stores into `LuaShared.providers`; the startup sequence reads it
//! after `init.lua` runs.
//!
//! Each entry in `models` is either a bare string (just the model name)
//! or a table with the full per-model config: `{ name, temperature,
//! top_p, top_k, min_p, repeat_penalty, tool_calling, input_cost,
//! output_cost, cache_read_cost, cache_write_cost }`.

use mlua::prelude::*;
use std::sync::Arc;

use crate::config::{ModelConfig, ProviderConfig};
use crate::lua::LuaShared;

/// Reject-unknown reader for per-model config tables. Plugins that rely
/// on a typo'd field would otherwise silently lose the override.
fn parse_model_table(t: &mlua::Table) -> LuaResult<ModelConfig> {
    const KNOWN: &[&str] = &[
        "name",
        "temperature",
        "top_p",
        "top_k",
        "min_p",
        "repeat_penalty",
        "tool_calling",
        "input_cost",
        "output_cost",
        "cache_read_cost",
        "cache_write_cost",
    ];
    for pair in t.clone().pairs::<mlua::Value, mlua::Value>() {
        let (key, _) = pair?;
        if let mlua::Value::String(s) = key {
            let k = s.to_string_lossy();
            if !KNOWN.contains(&k.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "smelt.provider.register: unknown model field `{k}`; \
                     known fields are {KNOWN:?}"
                )));
            }
        }
    }
    Ok(ModelConfig {
        name: t.get::<Option<String>>("name")?,
        temperature: t.get::<Option<f64>>("temperature")?,
        top_p: t.get::<Option<f64>>("top_p")?,
        top_k: t.get::<Option<u32>>("top_k")?,
        min_p: t.get::<Option<f64>>("min_p")?,
        repeat_penalty: t.get::<Option<f64>>("repeat_penalty")?,
        tool_calling: t.get::<Option<bool>>("tool_calling")?,
        input_cost: t.get::<Option<f64>>("input_cost")?,
        output_cost: t.get::<Option<f64>>("output_cost")?,
        cache_read_cost: t.get::<Option<f64>>("cache_read_cost")?,
        cache_write_cost: t.get::<Option<f64>>("cache_write_cost")?,
    })
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let tbl = lua.create_table()?;

    tbl.set(
        "register",
        lua.create_function({
            let shared = Arc::clone(shared);
            move |_lua, (name, cfg): (String, mlua::Table)| {
                let provider_type = cfg.get::<Option<String>>("type")?.unwrap_or_default();
                let api_base = cfg.get::<Option<String>>("api_base")?.unwrap_or_default();
                let api_key_env = cfg.get::<Option<String>>("api_key_env")?;
                let models: Vec<ModelConfig> = {
                    let arr: Option<mlua::Table> = cfg.get("models")?;
                    match arr {
                        Some(t) => {
                            let mut out = Vec::new();
                            for i in 1..=t.raw_len() {
                                let val: mlua::Value = t.get(i)?;
                                let mc = match val {
                                    mlua::Value::String(s) => ModelConfig {
                                        name: Some(s.to_string_lossy().to_string()),
                                        ..Default::default()
                                    },
                                    mlua::Value::Table(model_tbl) => parse_model_table(&model_tbl)?,
                                    other => {
                                        return Err(mlua::Error::external(format!(
                                            "smelt.provider.register: each model entry must \
                                             be a string or table, got {}",
                                            other.type_name()
                                        )));
                                    }
                                };
                                out.push(mc);
                            }
                            out
                        }
                        None => Vec::new(),
                    }
                };
                let provider = ProviderConfig {
                    name: Some(name),
                    provider_type: Some(provider_type),
                    api_base: Some(api_base),
                    api_key_env,
                    models,
                };
                let mut providers = shared.providers.lock().unwrap_or_else(|e| e.into_inner());
                providers.retain(|p| {
                    p.name.as_deref() != Some(&provider.name.clone().unwrap_or_default())
                });
                providers.push(provider);
                Ok(())
            }
        })?,
    )?;

    tbl.set(
        "list",
        lua.create_function({
            let shared = Arc::clone(shared);
            move |lua, ()| {
                let providers = shared.providers.lock().unwrap_or_else(|e| e.into_inner());
                let out = lua.create_table()?;
                for (i, p) in providers.iter().enumerate() {
                    let t = lua.create_table()?;
                    t.set("name", p.name.clone())?;
                    t.set("type", p.provider_type.clone())?;
                    t.set("api_base", p.api_base.clone())?;
                    t.set("api_key_env", p.api_key_env.clone())?;
                    let models = lua.create_table()?;
                    for (j, m) in p.models.iter().enumerate() {
                        let row = lua.create_table()?;
                        row.set("name", m.name.clone())?;
                        row.set("temperature", m.temperature)?;
                        row.set("top_p", m.top_p)?;
                        row.set("top_k", m.top_k)?;
                        row.set("min_p", m.min_p)?;
                        row.set("repeat_penalty", m.repeat_penalty)?;
                        row.set("tool_calling", m.tool_calling)?;
                        row.set("input_cost", m.input_cost)?;
                        row.set("output_cost", m.output_cost)?;
                        row.set("cache_read_cost", m.cache_read_cost)?;
                        row.set("cache_write_cost", m.cache_write_cost)?;
                        models.set(j + 1, row)?;
                    }
                    t.set("models", models)?;
                    out.set(i + 1, t)?;
                }
                Ok(out)
            }
        })?,
    )?;

    smelt.set("provider", tbl)?;
    Ok(())
}
