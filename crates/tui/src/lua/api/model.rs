//! `smelt.model` - selector for the configured provider/model.
//! `smelt.model.current()` reads the active key, `smelt.model.set(v)` switches,
//! and `smelt.model.list()` returns the available models.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

fn resolved_input_modalities(app: &crate::app::TuiApp) -> Vec<String> {
    let mut values = app
        .core
        .config
        .model_config
        .input_modalities
        .clone()
        .unwrap_or_default();
    if let Some(catalog) = smelt_provider::catalog::input_modalities(
        &app.core.config.provider_type,
        &app.core.config.api_base,
        &app.core.config.model,
    ) {
        values.extend(catalog);
    }
    if values.is_empty() {
        values.push("text".to_string());
    }
    values.sort();
    values.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    values
}

fn modalities_table(lua: &Lua, values: &[String]) -> LuaResult<mlua::Table> {
    let out = lua.create_table()?;
    for (i, value) in values.iter().enumerate() {
        out.set(i + 1, value.as_str())?;
    }
    Ok(out)
}

fn has_modality(values: &[String], modality: &str) -> bool {
    values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(modality))
}

fn supports_multimodal_tool_results(provider_type: &str) -> bool {
    matches!(
        provider_type,
        "anthropic" | "anthropic-compatible" | "kimi-code"
    )
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "model",
        "Model selector. `smelt.model.current()` reads the active model key, `smelt.model.set(v)` switches, and `smelt.model.list()` returns the available models.",
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
                smelt_provider::resolve_pricing(
                    &app.core.config.model,
                    &app.core.config.provider_type,
                    &app.core.config.api_base,
                    &app.core.config.model_config,
                )
            })
            .unwrap_or(smelt_provider::ResolvedPricing {
                pricing: smelt_provider::ModelPricing {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                source: smelt_provider::PricingSource::None,
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
                smelt_provider::catalog::output_tokens(
                    &app.core.config.provider_type,
                    &app.core.config.api_base,
                    &app.core.config.model,
                )
            })
            .unwrap_or_default())
        },
    )?;

    m.fn_(
        "input_modalities",
        "Resolved input modalities for the active model as an array such as `{ \"text\", \"image\", \"pdf\" }`. Config/provider metadata wins, models.dev fills missing data, and unknown models default to text only.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let values = crate::lua::try_with_app(|app| resolved_input_modalities(app))
                .unwrap_or_else(|| vec!["text".to_string()]);
            let out = modalities_table(lua, &values)?;
            Ok(out)
        },
    )?;

    m.fn_(
        "supports_input",
        "Return whether the active model supports the named input modality, for example `image` or `pdf`.",
        &["modality"],
        |_, modality: String| -> LuaResult<bool> {
            let want = modality.to_ascii_lowercase();
            Ok(crate::lua::try_with_app(|app| {
                resolved_input_modalities(app)
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(&want))
            })
            .unwrap_or(want == "text"))
        },
    )?;

    m.fn_(
        "transport",
        "Return `{ provider_type, api_base, multimodal_tool_results }` for the active model transport.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let (provider_type, api_base) = crate::lua::try_with_app(|app| {
                (app.core.config.provider_type.clone(), app.core.config.api_base.clone())
            })
            .unwrap_or_default();
            let multimodal_tool_results = supports_multimodal_tool_results(&provider_type);
            let out = lua.create_table()?;
            out.set("provider_type", provider_type)?;
            out.set("api_base", api_base)?;
            out.set("multimodal_tool_results", multimodal_tool_results)?;
            Ok(out)
        },
    )?;

    m.fn_(
        "capabilities",
        "Resolved capabilities for the active model/provider as `{ input_modalities, supports_image, supports_pdf, supports_video, supports_reasoning, tool_calling, max_tokens, context_window, transport = { ... }, sources = { ... } }`.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let data = crate::lua::try_with_app(|app| {
                let modalities = resolved_input_modalities(app);
                let provider_type = app.core.config.provider_type.clone();
                let api_base = app.core.config.api_base.clone();
                let model = app.core.config.model.clone();
                let catalog = smelt_provider::catalog::lookup(&provider_type, &api_base, &model);
                let max_tokens = app
                    .core
                    .config
                    .model_config
                    .max_tokens
                    .or_else(|| catalog.as_ref().and_then(|entry| entry.output_tokens));
                let context_window = app
                    .core
                    .config
                    .model_config
                    .context_window
                    .or(app.core.config.context_window)
                    .or_else(|| catalog.as_ref().and_then(|entry| entry.context_window));
                let supports_reasoning = app
                    .core
                    .config
                    .model_config
                    .supports_reasoning
                    .or_else(|| catalog.as_ref().and_then(|entry| entry.supports_reasoning));
                (
                    modalities,
                    provider_type,
                    api_base,
                    model,
                    max_tokens,
                    context_window,
                    supports_reasoning,
                    app.core.config.model_config.tool_calling.unwrap_or(true),
                    app.core.config.model_config.input_modalities.is_some(),
                    catalog.and_then(|entry| entry.input_modalities).is_some(),
                )
            });
            let (
                modalities,
                provider_type,
                api_base,
                model,
                max_tokens,
                context_window,
                supports_reasoning,
                tool_calling,
                has_config_modalities,
                has_catalog_modalities,
            ) = data.unwrap_or_else(|| {
                (
                    vec!["text".to_string()],
                    String::new(),
                    String::new(),
                    String::new(),
                    None,
                    None,
                    None,
                    true,
                    false,
                    false,
                )
            });

            let t = lua.create_table()?;
            t.set("model", model)?;
            t.set("provider_type", provider_type.clone())?;
            t.set("api_base", api_base.clone())?;
            t.set("input_modalities", modalities_table(lua, &modalities)?)?;
            t.set("supports_text", has_modality(&modalities, "text"))?;
            t.set("supports_image", has_modality(&modalities, "image"))?;
            t.set("supports_pdf", has_modality(&modalities, "pdf"))?;
            t.set("supports_video", has_modality(&modalities, "video"))?;
            t.set("supports_audio", has_modality(&modalities, "audio"))?;
            if let Some(v) = supports_reasoning {
                t.set("supports_reasoning", v)?;
            }
            t.set("tool_calling", tool_calling)?;
            if let Some(v) = max_tokens {
                t.set("max_tokens", v)?;
            }
            if let Some(v) = context_window {
                t.set("context_window", v)?;
            }

            let transport = lua.create_table()?;
            let multimodal_tool_results = supports_multimodal_tool_results(&provider_type);
            transport.set("provider_type", provider_type)?;
            transport.set("api_base", api_base)?;
            transport.set("multimodal_tool_results", multimodal_tool_results)?;
            transport.set("image_tool_results", multimodal_tool_results)?;
            transport.set("pdf_tool_results", multimodal_tool_results)?;
            t.set("transport", transport)?;

            let sources = lua.create_table()?;
            sources.set(
                "input_modalities",
                if has_config_modalities {
                    "config/provider"
                } else if has_catalog_modalities {
                    "models.dev"
                } else {
                    "default"
                },
            )?;
            sources.set(
                "context_window",
                if context_window.is_some() {
                    "resolved"
                } else {
                    "unknown"
                },
            )?;
            sources.set(
                "max_tokens",
                if max_tokens.is_some() {
                    "resolved"
                } else {
                    "unknown"
                },
            )?;
            sources.set(
                "supports_reasoning",
                if supports_reasoning.is_some() {
                    "resolved"
                } else {
                    "unknown"
                },
            )?;
            t.set("sources", sources)?;
            Ok(t)
        },
    )?;

    m.fn_(
        "current",
        "Return the active model key.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| app.core.config.model.clone()).unwrap_or_default())
        },
    )?;
    m.fn_(
        "set",
        "Switch the active model by key.",
        &["name"],
        |_, name: String| -> LuaResult<()> {
            crate::lua::with_app(|app| app.apply_model(&name, true));
            Ok(())
        },
    )?;
    Ok(())
}
