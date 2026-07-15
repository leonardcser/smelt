//! `smelt.config` - read-only introspection of the resolved application
//! configuration: provider, model overrides, sampling params, and window
//! sizing. UiHost-only.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

fn revision_status(
    desired_revision: u64,
    observed_revision: u64,
    error: Option<String>,
) -> serde_json::Value {
    let status = if error.is_some() {
        "degraded"
    } else if desired_revision == observed_revision {
        "ready"
    } else {
        "pending"
    };
    serde_json::json!({
        "desired_revision": desired_revision,
        "observed_revision": observed_revision,
        "status": status,
        "error": error,
    })
}

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
        "runtime_status",
        "Return sanitized runtime and reload diagnostics: committed Lua/runtime revisions, pending reload and last failure location, model selection, managed-provider freshness, and MCP/LSP/watcher/context controller convergence. No credential values or Lua source contents are included.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let Some(status) = crate::lua::try_with_app(|app| {
                let controllers = app.runtime_controller_status();
                let model = app.model_status_snapshot();
                let failure = app.lua_reload_failure.as_ref().map(|failure| {
                    serde_json::json!({
                        "phase": failure.location.phase,
                        "path": failure
                            .location
                            .path
                            .as_ref()
                            .map(|path| path.to_string_lossy().into_owned()),
                    })
                });
                let managed_providers = model
                    .providers
                    .iter()
                    .map(|provider| {
                        (
                            provider.name.clone(),
                            serde_json::json!({
                                "authenticated": provider.authenticated,
                                "status": provider.status,
                                "request_id": provider.request_id,
                                "auth_revision": provider.auth_revision,
                                "desired_revision": provider.desired_revision,
                            }),
                        )
                    })
                    .collect::<serde_json::Map<_, _>>();
                let mcp = controllers.mcp.map_or_else(
                    || serde_json::json!({ "status": "unavailable" }),
                    |status| {
                        revision_status(
                            status.desired_revision,
                            status.observed_revision,
                            None,
                        )
                    },
                );
                serde_json::json!({
                    "lua_generation": app.core.lua_generation,
                    "runtime_revision": app.core.config.revision,
                    "reload": {
                        "pending": app.pending_lua_reload,
                        "waiting_for_safe_point": app.pending_lua_reload && !app.can_reload_lua_now(),
                        "failure": failure,
                    },
                    "model": {
                        "current": model.current,
                        "requested": model.requested,
                        "availability": model.availability,
                        "reason": model.reason,
                    },
                    "managed_providers": managed_providers,
                    "controllers": {
                        "mcp": mcp,
                        "lsp": revision_status(
                            controllers.lsp.desired_revision,
                            controllers.lsp.observed_revision,
                            None,
                        ),
                        "watcher": revision_status(
                            controllers.auto_reload.desired_revision,
                            controllers.auto_reload.observed_revision,
                            controllers.auto_reload.error,
                        ),
                        "context_window": revision_status(
                            controllers.context_window.desired_revision,
                            controllers.context_window.observed_revision,
                            controllers.context_window.error,
                        ),
                    },
                })
            }) else {
                return Err(LuaError::external(
                    "smelt.config.runtime_status: app not initialized",
                ));
            };
            match smelt_core::lua::json_to_lua(lua, &status)? {
                mlua::Value::Table(table) => Ok(table),
                _ => Err(LuaError::external(
                    "smelt.config.runtime_status: invalid status shape",
                )),
            }
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
