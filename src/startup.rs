use crate::Args;
use protocol::{AgentMode, ReasoningEffort};

/// Read an API key from `key_env`. An empty `key_env` returns an empty key.
pub fn resolve_api_key(key_env: &str) -> Result<String, String> {
    if key_env.is_empty() {
        return Ok(String::new());
    }
    match std::env::var(key_env) {
        Ok(key) => Ok(key),
        Err(std::env::VarError::NotPresent) => Err(format!(
            "environment variable '{key_env}' is not set but is required for API authentication"
        )),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!(
            "environment variable '{key_env}' contains non-Unicode data and cannot be used as an API key"
        )),
    }
}

/// Fully resolved startup parameters, produced by [`resolve`] before the engine starts.
pub struct ResolvedStartup {
    pub cfg: smelt_core::config::Config,
    pub available_models: Vec<smelt_core::config::ResolvedModel>,
    pub api_base: String,
    pub api_key: String,
    pub api_key_env: String,
    pub provider_type: String,
    pub model: String,
    pub model_config: smelt_core::config::ModelConfig,
    pub settings: smelt_core::config::ResolvedSettings,
    pub mode_override: Option<AgentMode>,
    pub mode_cycle: Vec<AgentMode>,
    pub reasoning_effort: ReasoningEffort,
    pub reasoning_cycle: Vec<ReasoningEffort>,
    pub startup_auth_error: Option<String>,
}

/// Resolve the active model. Priority: CLI `--model` > recent
/// `selected_model` (when `smelt.remember.model` is on) >
/// `smelt.defaults{model=...}` in init.lua > first in config. Returns
/// `None` when the CLI model is absent from resolved models and
/// `--api-base` is also set.
fn resolve_model_reference(
    args: &Args,
    cfg: &smelt_core::config::Config,
    available_models: &[smelt_core::config::ResolvedModel],
    recent: &smelt_core::state::Recent,
) -> Option<smelt_core::config::ResolvedModel> {
    let pick = |reference: &str, allow_not_found: bool| match smelt_core::config::resolve_model_ref(
        available_models,
        reference,
    ) {
        Ok(model) => Some(model.clone()),
        Err(smelt_core::config::ResolveModelRefError::NotFound { .. }) if allow_not_found => None,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };

    if let Some(ref cli_model) = args.model {
        pick(cli_model, args.api_base.is_some())
    } else if let Some(picked) = cfg
        .remember
        .model
        .then_some(recent.selected_model.as_deref())
        .flatten()
        .and_then(|key| smelt_core::config::resolve_model_ref(available_models, key).ok())
        .cloned()
    {
        Some(picked)
    } else if let Some(default) = cfg.defaults.model.as_deref() {
        pick(default, false)
    } else {
        available_models.first().cloned()
    }
}

fn has_managed_provider(
    cfg: &smelt_core::config::Config,
    provider: engine::auth::AuthProvider,
) -> bool {
    match provider {
        engine::auth::AuthProvider::Codex => cfg.has_codex_provider(),
        engine::auth::AuthProvider::Copilot => cfg.has_copilot_provider(),
        engine::auth::AuthProvider::KimiCode => cfg.has_kimi_code_provider(),
    }
}

fn inject_managed_models(
    cfg: &smelt_core::config::Config,
    available_models: &mut Vec<smelt_core::config::ResolvedModel>,
    provider: engine::auth::AuthProvider,
    models: &[smelt_core::config::DynamicModel],
) {
    match provider {
        engine::auth::AuthProvider::Codex => cfg.inject_codex_models(available_models, models),
        engine::auth::AuthProvider::Copilot => cfg.inject_copilot_models(available_models, models),
        engine::auth::AuthProvider::KimiCode => {
            cfg.inject_kimi_code_models(available_models, models)
        }
    }
}

async fn inject_managed_provider_models(
    cfg: &smelt_core::config::Config,
    available_models: &mut Vec<smelt_core::config::ResolvedModel>,
    provider: engine::auth::AuthProvider,
    http_client: &reqwest::Client,
    refresh_if_empty: bool,
) {
    if !has_managed_provider(cfg, provider) {
        return;
    }

    let mut models = engine::auth::cached_model_info(provider);
    if refresh_if_empty && models.is_empty() {
        models = engine::auth::refresh_model_info(provider, http_client).await;
    }
    if !models.is_empty() {
        inject_managed_models(cfg, available_models, provider, &models);
    }

    let client = http_client.clone();
    tokio::spawn(async move {
        let _ = engine::auth::refresh_model_info(provider, &client).await;
    });
}

/// Resolve all startup configuration: Lua registries, `--set` overrides, model lists, API keys, and defaults.
///
/// `http_client` is reused for Codex / Copilot auth refresh tasks so they don't each
/// rebuild a fresh rustls config + webpki-roots parse.
pub async fn resolve(
    args: &Args,
    cfg: smelt_core::config::Config,
    http_client: &reqwest::Client,
    registered_modes: &[AgentMode],
) -> ResolvedStartup {
    let mut cfg = cfg;

    for pair in &args.set {
        let Some((key, value)) = pair.split_once('=') else {
            eprintln!("error: --set requires KEY=VALUE format, got '{pair}'");
            std::process::exit(1);
        };
        let parsed = match smelt_core::config::setting_kind(key) {
            Some(smelt_core::config::SettingKind::Bool) => match value {
                "true" => smelt_core::config::SettingValue::Bool(true),
                "false" => smelt_core::config::SettingValue::Bool(false),
                _ => {
                    eprintln!("error: --set {pair}: invalid bool value '{value}' for {key}");
                    std::process::exit(1);
                }
            },
            Some(smelt_core::config::SettingKind::Number) => match value.parse::<f64>() {
                Ok(n) => smelt_core::config::SettingValue::Number(n),
                Err(_) => {
                    eprintln!("error: --set {pair}: invalid number '{value}' for {key}");
                    std::process::exit(1);
                }
            },
            Some(smelt_core::config::SettingKind::String) => {
                smelt_core::config::SettingValue::String(value.to_string())
            }
            None => {
                eprintln!("error: --set {pair}: unknown setting '{key}'");
                std::process::exit(1);
            }
        };
        if let Err(e) = cfg.settings.set(key, &parsed) {
            eprintln!("error: --set {pair}: {e}");
            std::process::exit(1);
        }
    }

    cfg.inject_oauth_providers();

    let recent = smelt_core::state::Recent::load();
    let mut available_models = cfg.resolve_models();

    inject_managed_provider_models(
        &cfg,
        &mut available_models,
        engine::auth::AuthProvider::Codex,
        http_client,
        false,
    )
    .await;
    inject_managed_provider_models(
        &cfg,
        &mut available_models,
        engine::auth::AuthProvider::Copilot,
        http_client,
        true,
    )
    .await;
    inject_managed_provider_models(
        &cfg,
        &mut available_models,
        engine::auth::AuthProvider::KimiCode,
        http_client,
        true,
    )
    .await;

    let mut startup_auth_error: Option<String> = None;

    let (api_base, api_key, api_key_env, mut provider_type, model, mut model_config) = {
        let resolved = resolve_model_reference(args, &cfg, &available_models, &recent);

        if let Some(r) = resolved {
            let base = args.api_base.clone().unwrap_or_else(|| r.api_base.clone());
            let key_env = args
                .api_key_env
                .clone()
                .unwrap_or_else(|| r.api_key_env.clone());
            let provider_kind = engine::ProviderKind::from_config_and_url(&r.provider_type, &base);
            let key = if provider_kind == engine::ProviderKind::KimiCode {
                String::new()
            } else {
                match resolve_api_key(&key_env) {
                    Ok(key) => key,
                    Err(err) => {
                        startup_auth_error = Some(err);
                        String::new()
                    }
                }
            };
            (
                base,
                key,
                key_env,
                r.provider_type.clone(),
                r.model_name.clone(),
                r.config.clone(),
            )
        } else if let Some(base) = args.api_base.clone() {
            let key_env = args.api_key_env.clone().unwrap_or_default();
            let provider_kind = engine::ProviderKind::detect_from_url(&base);
            let key = if provider_kind == engine::ProviderKind::KimiCode {
                String::new()
            } else {
                match resolve_api_key(&key_env) {
                    Ok(key) => key,
                    Err(err) => {
                        startup_auth_error = Some(err);
                        String::new()
                    }
                }
            };
            let Some(model) = args.model.clone() else {
                eprintln!("error: --model is required when using --api-base without a config file");
                std::process::exit(1);
            };
            (
                base.clone(),
                key,
                key_env,
                provider_kind.as_config_str().to_string(),
                model,
                smelt_core::config::ModelConfig::default(),
            )
        } else {
            eprintln!(
                "error: no providers with models registered.\n\
                 Add `smelt.provider.register{{...}}` calls to your init.lua, or use --api-base and --model."
            );
            std::process::exit(1);
        }
    };

    if let Some(ref t) = args.r#type {
        provider_type = t.clone();
    } else if args.api_base.is_some() {
        provider_type = engine::ProviderKind::detect_from_url(&api_base)
            .as_config_str()
            .to_string();
    }

    if let Some(v) = args.temperature {
        model_config.temperature = Some(v);
    }
    if let Some(v) = args.top_p {
        model_config.top_p = Some(v);
    }
    if let Some(v) = args.top_k {
        model_config.top_k = Some(v);
    }
    if args.no_tool_calling {
        model_config.tool_calling = Some(false);
    }

    let parse_mode = |s: &str| {
        AgentMode::parse(s)
            .filter(|mode| registered_modes.is_empty() || registered_modes.contains(mode))
            .unwrap_or_else(|| {
                eprintln!("warning: invalid or unregistered mode '{s}', defaulting to normal");
                AgentMode::normal()
            })
    };
    let mode_override = if let Some(s) = args.mode.as_deref() {
        Some(parse_mode(s))
    } else if cfg.remember.mode {
        recent
            .mode()
            .or_else(|| cfg.defaults.mode.as_deref().map(parse_mode))
    } else {
        cfg.defaults.mode.as_deref().map(parse_mode)
    };

    let mode_cycle = args
        .mode_cycle
        .as_deref()
        .map(|items| {
            AgentMode::parse_list(items)
                .into_iter()
                .filter(|mode| registered_modes.is_empty() || registered_modes.contains(mode))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            if registered_modes.is_empty() {
                AgentMode::default_cycle()
            } else {
                registered_modes.to_vec()
            }
        });

    let default_effort = cfg
        .defaults
        .reasoning_effort
        .as_deref()
        .and_then(ReasoningEffort::parse);
    let reasoning_effort = if let Some(cli) = args
        .reasoning_effort
        .as_deref()
        .and_then(ReasoningEffort::parse)
    {
        cli
    } else if cfg.remember.reasoning_effort && recent.reasoning_effort != ReasoningEffort::Off {
        recent.reasoning_effort
    } else {
        default_effort.unwrap_or(ReasoningEffort::Off)
    };

    let provider_kind = engine::ProviderKind::from_config_and_url(&provider_type, &api_base);
    let mut reasoning_cycle = args
        .reasoning_cycle
        .as_deref()
        .map(ReasoningEffort::parse_list)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| provider_kind.default_reasoning_cycle().to_vec());
    if !reasoning_cycle.contains(&reasoning_effort) {
        reasoning_cycle.push(reasoning_effort);
    }

    let mut settings = cfg.settings.clone();
    if args.headless {
        settings.auto_compact = true;
    }

    ResolvedStartup {
        cfg,
        available_models,
        api_base,
        api_key,
        api_key_env,
        provider_type,
        model,
        model_config,
        settings,
        mode_override,
        mode_cycle,
        reasoning_effort,
        reasoning_cycle,
        startup_auth_error,
    }
}
