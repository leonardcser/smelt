//! `smelt.model` - callable selector for the configured provider/model.
//! `smelt.model()` reads the active key, `smelt.model(v)` switches,
//! `smelt.model.list()` returns the available models.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "model",
        "Model selector. `smelt.model()` reads the active model key, `smelt.model(v)` switches, `smelt.model.list()` returns the available models.",
        Tier::UiHost,
    )?;
    m.fn_(
        "list",
        "Return an array of `{ key, name, provider, api_base, provider_type }` records for every model the active config can switch to.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            if let Some(res) = crate::lua::try_with_app(|app| -> LuaResult<()> {
                for (i, m) in app.core.config.available_models.iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("key", m.key.clone())?;
                    entry.set("name", m.model_name.clone())?;
                    entry.set("provider", m.provider_name.clone())?;
                    entry.set("api_base", m.api_base.clone())?;
                    entry.set("provider_type", m.provider_type.clone())?;
                    out.set(i + 1, entry)?;
                }
                Ok(())
            }) {
                res?;
            }
            Ok(out)
        },
    )?;

    m.fn_(
        "pricing",
        "Resolved pricing for the active model as `{ input, output, cache_read, cache_write, source }`. `source` is one of `\"config override\"`, `\"models.dev\"`, or `\"none\"`. Prices are USD per 1M tokens.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let resolved = crate::lua::try_with_app(|app| {
                engine::pricing::resolve(
                    &app.core.config.model,
                    &app.core.config.provider_type,
                    &app.core.config.api_base,
                    &app.core.config.model_config,
                )
            })
            .unwrap_or(engine::pricing::ResolvedPricing {
                pricing: engine::pricing::ModelPricing {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                source: engine::pricing::PricingSource::None,
            });
            let t = lua.create_table()?;
            t.set("input", resolved.pricing.input)?;
            t.set("output", resolved.pricing.output)?;
            t.set("cache_read", resolved.pricing.cache_read)?;
            t.set("cache_write", resolved.pricing.cache_write)?;
            t.set("source", resolved.source.label())?;
            Ok(t)
        },
    )?;

    m.fn_(
        "max_tokens",
        "Resolved maximum output tokens for the active model. Returns the config override if set, otherwise falls back to the models.dev catalog value, then to the provider default. Returns `nil` when no limit is known.",
        &[],
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_app(|app| {
                // Config override wins.
                if let Some(override_) = app.core.config.model_config.max_tokens {
                    return Some(override_);
                }
                // Fall back to models.dev catalog.
                engine::catalog::output_tokens(
                    &app.core.config.provider_type,
                    &app.core.config.api_base,
                    &app.core.config.model,
                )
            })
            .unwrap_or_default())
        },
    )?;

    // `__call(v?)`: read when no arg, switch when arg.
    m.callable(
        |lua, (_tbl, v): (mlua::Table, Option<String>)| -> LuaResult<mlua::Value> {
            match v {
                Some(name) => {
                    crate::lua::with_app(|app| app.apply_model(&name, true));
                    Ok(mlua::Value::Nil)
                }
                None => {
                    let cur = crate::lua::try_with_app(|app| app.core.config.model.clone())
                        .unwrap_or_default();
                    Ok(mlua::Value::String(lua.create_string(&cur)?))
                }
            }
        },
    )?;
    Ok(())
}
