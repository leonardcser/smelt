//! `smelt.config` - read-only introspection of the resolved application
//! configuration: provider, model overrides, sampling params, and window
//! sizing. UiHost-only.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "config",
        "Resolved application configuration introspection. All fields are read-only snapshots of the active config. UiHost-only.",
        Tier::UiHost,
    )?;

    m.fn_(
        "provider_type",
        "Active provider type string, e.g. `\"openai\"`, `\"anthropic\"`, `\"openai-compatible\"`.",
        &[],
        |_, ()| -> LuaResult<Option<String>> {
            Ok(crate::lua::try_with_app(|app| {
                app.core
                    .config
                    .active_model()
                    .map(|model| model.provider_type.clone())
            })
            .flatten())
        },
    )?;

    m.fn_(
        "api_base",
        "Active API base URL.",
        &[],
        |_, ()| -> LuaResult<Option<String>> {
            Ok(crate::lua::try_with_app(|app| {
                app.core
                    .config
                    .active_model()
                    .map(|model| model.api_base.clone())
            })
            .flatten())
        },
    )?;

    m.fn_(
        "api_key_env",
        "Name of the environment variable that supplies the API key for the active provider.",
        &[],
        |_, ()| -> LuaResult<Option<String>> {
            Ok(crate::lua::try_with_app(|app| {
                app.core
                    .config
                    .active_model()
                    .map(|model| model.api_key_env.clone())
            })
            .flatten())
        },
    )?;

    m.fn_(
        "model_config",
        "Resolved model-level sampling, capability, and cost overrides as a table. Fields are `nil` when not explicitly set: `name`, `temperature`, `top_p`, `top_k`, `min_p`, `repeat_penalty`, `tool_calling`, `max_tokens`, `context_window`, `supports_reasoning`, `input_modalities`, `thinking_budgets` (`{ low, medium, high, max }`), `input_cost`, `output_cost`, `cache_read_cost`, `cache_write_cost`.",
        &[],
        |lua, ()| -> LuaResult<Option<mlua::Table>> {
            let Some(cfg) = crate::lua::try_with_app(|app| {
                app.core
                    .config
                    .active_model()
                    .map(|model| model.config.clone())
            })
            .flatten()
            else {
                return Ok(None);
            };
            let t = lua.create_table()?;
            if let Some(v) = cfg.name {
                t.set("name", v)?;
            }
            if let Some(v) = cfg.temperature {
                t.set("temperature", v)?;
            }
            if let Some(v) = cfg.top_p {
                t.set("top_p", v)?;
            }
            if let Some(v) = cfg.top_k {
                t.set("top_k", v)?;
            }
            if let Some(v) = cfg.min_p {
                t.set("min_p", v)?;
            }
            if let Some(v) = cfg.repeat_penalty {
                t.set("repeat_penalty", v)?;
            }
            if let Some(v) = cfg.tool_calling {
                t.set("tool_calling", v)?;
            }
            if let Some(v) = cfg.max_tokens {
                t.set("max_tokens", v)?;
            }
            if let Some(v) = cfg.context_window {
                t.set("context_window", v)?;
            }
            if let Some(v) = cfg.supports_reasoning {
                t.set("supports_reasoning", v)?;
            }
            if let Some(values) = cfg.input_modalities {
                let arr = lua.create_table()?;
                for (i, value) in values.iter().enumerate() {
                    arr.set(i + 1, value.as_str())?;
                }
                t.set("input_modalities", arr)?;
            }
            if let Some(budgets) = cfg.thinking_budgets {
                let b = lua.create_table()?;
                b.set("low", budgets.low)?;
                b.set("medium", budgets.medium)?;
                b.set("high", budgets.high)?;
                b.set("max", budgets.max)?;
                t.set("thinking_budgets", b)?;
            }
            if let Some(v) = cfg.input_cost {
                t.set("input_cost", v)?;
            }
            if let Some(v) = cfg.output_cost {
                t.set("output_cost", v)?;
            }
            if let Some(v) = cfg.cache_read_cost {
                t.set("cache_read_cost", v)?;
            }
            if let Some(v) = cfg.cache_write_cost {
                t.set("cache_write_cost", v)?;
            }
            Ok(Some(t))
        },
    )?;

    m.fn_(
        "context_window",
        "Configured context-window size in tokens for the active model, or `nil` when not declared.",
        &[],
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_app(|app| app.core.config.context_window).unwrap_or_default())
        },
    )?;

    Ok(())
}
