//! `smelt.provider` bindings — config-time provider registration.
//!
//! `smelt.provider.register(name, { type, api_base, api_key_env, models })`
//! stores into `LuaShared.providers`; the startup sequence reads it
//! after `init.lua` runs.

use mlua::prelude::*;
use std::sync::Arc;

use crate::config::ProviderConfig;
use crate::lua::LuaShared;

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
                let models: Vec<String> = {
                    let arr: Option<mlua::Table> = cfg.get("models")?;
                    match arr {
                        Some(t) => {
                            let mut out = Vec::new();
                            for i in 1..=t.raw_len() {
                                out.push(t.get(i)?);
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
                    models: models
                        .into_iter()
                        .map(|m| crate::config::ModelConfig {
                            name: Some(m),
                            ..Default::default()
                        })
                        .collect(),
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
                        models.set(j + 1, m.name.clone())?;
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
