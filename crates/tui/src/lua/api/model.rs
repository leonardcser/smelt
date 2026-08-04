//! `smelt.model` - selector for the configured provider/model.
//! `smelt.model.current()` reads the active key, `smelt.model.set(v)` switches,
//! and `smelt.model.list()` returns the available models.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

fn resolved_input_modalities(active: &smelt_core::runtime_state::ActiveModel) -> Vec<String> {
    let mut values = active.config.input_modalities.clone().unwrap_or_default();
    if let Some(catalog) = smelt_provider::catalog::input_modalities(
        &active.provider_type,
        &active.api_base,
        &active.model_name,
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

fn tool_result_capabilities(provider_type: &str, api_base: &str) -> (bool, bool) {
    let descriptor =
        smelt_provider::ProviderKind::from_config_and_url(provider_type, api_base).descriptor();
    (
        descriptor.supports_image_tool_results(),
        descriptor.supports_pdf_tool_results(),
    )
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "model",
        "Model selector. `smelt.model.current()` reads the active model key, `smelt.model.set(v)` switches, and `smelt.model.list()` returns the available models.",
        Tier::UiHost,
    )?;
    let host = LuaMod::extend_supported(lua, m.tbl.clone(), "smelt.model", Tier::Host);
    host.fn_(
        "list",
        "Return an array of `{ key, name, display_name?, provider, api_base, provider_type }` records for every model the active config can switch to.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            let models =
                crate::lua::try_with_runtime_host(|host| host.available_models()).unwrap_or_default();
            for (index, model) in models.into_iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("key", model.key)?;
                entry.set("name", model.model_name)?;
                entry.set("display_name", model.display_name)?;
                entry.set("provider", model.provider_name)?;
                entry.set("api_base", model.api_base)?;
                entry.set("provider_type", model.provider_type)?;
                out.set(index + 1, entry)?;
            }
            Ok(out)
        },
    )?;

    m.fn_(
        "pricing",
        "Resolved pricing for the active model as `{ input, output, cache_read, cache_write, source }`. `source` is one of `\"config override\"`, `\"models.dev\"`, or `\"none\"`. Prices are USD per 1M tokens.",
        &[],
        |lua, ()| -> LuaResult<Option<mlua::Table>> {
            let Some(active) = crate::lua::try_with_runtime_host(|host| host.active_model()).flatten()
            else {
                return Ok(None);
            };
            let resolved = smelt_provider::resolve_pricing(
                &active.model_name,
                &active.provider_type,
                &active.api_base,
                &active.config,
            );
            let t = lua.create_table()?;
            t.set("input", resolved.pricing.input)?;
            t.set("output", resolved.pricing.output)?;
            t.set("cache_read", resolved.pricing.cache_read)?;
            t.set("cache_write", resolved.pricing.cache_write)?;
            t.set("source", resolved.source.label())?;
            Ok(Some(t))
        },
    )?;

    m.fn_(
        "max_tokens",
        "Resolved maximum output tokens for the active model. Returns the config override if set, otherwise falls back to the models.dev catalog value, then to the provider default. Returns `nil` when no limit is known.",
        &[],
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_runtime_host(|host| host.active_model())
                .flatten()
                .and_then(|active| {
                    active.config.max_tokens.or_else(|| {
                        smelt_provider::catalog::output_tokens(
                            &active.provider_type,
                            &active.api_base,
                            &active.model_name,
                        )
                    })
                }))
        },
    )?;

    m.fn_(
        "input_modalities",
        "Resolved input modalities for the active model as an array such as `{ \"text\", \"image\", \"pdf\" }`. Config/provider metadata wins, models.dev fills missing data, and unknown models default to text only.",
        &[],
        |lua, ()| -> LuaResult<Option<mlua::Table>> {
            let Some(active) = crate::lua::try_with_runtime_host(|host| host.active_model()).flatten()
            else {
                return Ok(None);
            };
            let values = resolved_input_modalities(&active);
            Ok(Some(modalities_table(lua, &values)?))
        },
    )?;

    m.fn_(
        "supports_input",
        "Return whether the active model supports the named input modality, for example `image` or `pdf`.",
        &["modality"],
        |_, modality: String| -> LuaResult<Option<bool>> {
            let want = modality.to_ascii_lowercase();
            Ok(crate::lua::try_with_runtime_host(|host| host.active_model())
                .flatten()
                .map(|active| has_modality(&resolved_input_modalities(&active), &want)))
        },
    )?;

    m.fn_(
        "transport",
        "Return `{ provider_type, api_base, api_key_env, multimodal_tool_results, image_tool_results, pdf_tool_results }` for the active model transport. Prefer the modality-specific fields; `multimodal_tool_results` is their aggregate. `api_key_env` is the environment variable name, never its value.",
        &[],
        |lua, ()| -> LuaResult<Option<mlua::Table>> {
            let Some(active) = crate::lua::try_with_runtime_host(|host| host.active_model()).flatten()
            else {
                return Ok(None);
            };
            let (image_tool_results, pdf_tool_results) =
                tool_result_capabilities(&active.provider_type, &active.api_base);
            let out = lua.create_table()?;
            out.set("provider_type", active.provider_type)?;
            out.set("api_base", active.api_base)?;
            out.set("api_key_env", active.api_key_env)?;
            out.set(
                "multimodal_tool_results",
                image_tool_results || pdf_tool_results,
            )?;
            out.set("image_tool_results", image_tool_results)?;
            out.set("pdf_tool_results", pdf_tool_results)?;
            Ok(Some(out))
        },
    )?;

    m.fn_(
        "capabilities",
        "Resolved capabilities for the active model/provider as `{ input_modalities, supports_image, supports_pdf, supports_video, supports_reasoning, tool_calling, max_tokens, context_window, transport = { ... }, sources = { ... } }`.",
        &[],
        |lua, ()| -> LuaResult<Option<mlua::Table>> {
            let Some((active, runtime_context_window)) = crate::lua::try_with_runtime_host(|host| {
                host.active_model()
                    .map(|active| (active, host.context_window()))
            })
            .flatten()
            else {
                return Ok(None);
            };
            let modalities = resolved_input_modalities(&active);
            let catalog = smelt_provider::catalog::lookup(
                &active.provider_type,
                &active.api_base,
                &active.model_name,
            );
            let max_tokens = active
                .config
                .max_tokens
                .or_else(|| catalog.as_ref().and_then(|entry| entry.output_tokens));
            let context_window = active
                .config
                .context_window
                .or(runtime_context_window)
                .or_else(|| catalog.as_ref().and_then(|entry| entry.context_window));
            let supports_reasoning = active
                .config
                .supports_reasoning
                .or_else(|| catalog.as_ref().and_then(|entry| entry.supports_reasoning));
            let data = (
                modalities,
                active.provider_type,
                active.api_base,
                active.api_key_env,
                active.model_name,
                max_tokens,
                context_window,
                supports_reasoning,
                active.config.tool_calling(),
                active.config.input_modalities.is_some(),
                catalog.and_then(|entry| entry.input_modalities).is_some(),
            );
            let (
                modalities,
                provider_type,
                api_base,
                api_key_env,
                model,
                max_tokens,
                context_window,
                supports_reasoning,
                tool_calling,
                has_config_modalities,
                has_catalog_modalities,
            ) = data;

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
            let (image_tool_results, pdf_tool_results) =
                tool_result_capabilities(&provider_type, &api_base);
            transport.set("provider_type", provider_type)?;
            transport.set("api_base", api_base)?;
            transport.set("api_key_env", api_key_env)?;
            transport.set(
                "multimodal_tool_results",
                image_tool_results || pdf_tool_results,
            )?;
            transport.set("image_tool_results", image_tool_results)?;
            transport.set("pdf_tool_results", pdf_tool_results)?;
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
            Ok(Some(t))
        },
    )?;

    m.fn_(
        "current",
        "Return the active model key, or `nil` when no active model exists.",
        &[],
        |_, ()| -> LuaResult<Option<String>> {
            Ok(
                crate::lua::try_with_runtime_host(|host| host.active_model())
                    .flatten()
                    .map(|model| model.key),
            )
        },
    )?;
    m.fn_(
        "status",
        "Return `{ current, requested, availability, reason?, providers }` for model selection. `availability` is `available`, `stale_catalog`, `unavailable`, `pending`, or `none`; unavailable reasons are stable snake-case strings. Each managed provider reports sanitized `authenticated`, `status`, `error`, `request_id`, `auth_revision`, and `desired_revision` fields.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            if let Some(status) = crate::lua::try_with_runtime_host(|host| host.model_status()) {
                out.set("current", status.current)?;
                out.set("requested", status.requested)?;
                out.set("availability", status.availability)?;
                out.set("reason", status.reason)?;
                let provider_status = lua.create_table()?;
                for provider in status.providers {
                    let row = lua.create_table()?;
                    row.set("authenticated", provider.authenticated)?;
                    row.set("status", provider.status)?;
                    row.set("error", provider.error)?;
                    row.set("request_id", provider.request_id)?;
                    row.set("auth_revision", provider.auth_revision)?;
                    row.set("desired_revision", provider.desired_revision)?;
                    provider_status.set(provider.name, row)?;
                }
                out.set("providers", provider_status)?;
            } else {
                out.set("availability", "none")?;
            }
            Ok(out)
        },
    )?;
    m.live_only_fn(
        "set",
        "Switch the active model by key. Errors when the name cannot be resolved.",
        &["name"],
        |_, name: String| -> LuaResult<()> {
            crate::lua::try_with_runtime_host(|host| host.apply_model_ref(&name))
                .ok_or_else(|| LuaError::external("smelt.model.set: app not initialized"))?
                .map_err(LuaError::external)
        },
    )?;
    Ok(())
}
