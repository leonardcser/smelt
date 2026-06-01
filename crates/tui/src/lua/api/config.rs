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
        |_, ()| -> LuaResult<String> {
            Ok(
                crate::lua::try_with_app(|app| app.core.config.provider_type.clone())
                    .unwrap_or_default(),
            )
        },
    )?;

    m.fn_(
        "api_base",
        "Active API base URL.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(
                crate::lua::try_with_app(|app| app.core.config.api_base.clone())
                    .unwrap_or_default(),
            )
        },
    )?;

    m.fn_(
        "api_key_env",
        "Name of the environment variable that supplies the API key for the active provider.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(
                crate::lua::try_with_app(|app| app.core.config.api_key_env.clone())
                    .unwrap_or_default(),
            )
        },
    )?;

    m.fn_(
        "model_config",
        "Resolved model-level sampling and cost overrides as a table. Fields are `nil` when not explicitly set: `name`, `temperature`, `top_p`, `top_k`, `min_p`, `repeat_penalty`, `tool_calling`, `max_tokens`, `thinking_budgets` (`{ low, medium, high, max }`), `input_cost`, `output_cost`, `cache_read_cost`, `cache_write_cost`.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let cfg = crate::lua::try_with_app(|app| app.core.config.model_config.clone())
                .unwrap_or_default();
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
            Ok(t)
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
