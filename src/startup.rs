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
    pub cache: smelt_core::state::SessionCache,
}

/// Resolve the active model. Priority: CLI `--model` > `smelt.defaults{model=...}`
/// in init.lua > cached `selected_model` from the previous session > first in
/// config. Returns `None` when the CLI model is absent from resolved models
/// and `--api-base` is also set.
fn resolve_model_reference(
    args: &Args,
    cfg: &smelt_core::config::Config,
    available_models: &[smelt_core::config::ResolvedModel],
    cache: &smelt_core::state::SessionCache,
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
    } else if let Some(default) = cfg.defaults.model.as_deref() {
        pick(default, false)
    } else if let Some(ref cached) = cache.selected_model {
        smelt_core::config::resolve_model_ref(available_models, cached)
            .ok()
            .cloned()
            .or_else(|| available_models.first().cloned())
    } else {
        available_models.first().cloned()
    }
}

/// Resolve all startup configuration: Lua registries, `--set` overrides, model lists, API keys, and defaults.
///
/// `http_client` is reused for Codex / Copilot auth refresh tasks so they don't each
/// rebuild a fresh rustls config + webpki-roots parse.
pub async fn resolve(
    args: &Args,
    cfg: smelt_core::config::Config,
    http_client: &reqwest::Client,
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
        if let Err(e) = cfg.settings.set(key, parsed) {
            eprintln!("error: --set {pair}: {e}");
            std::process::exit(1);
        }
    }

    cfg.inject_oauth_providers();

    let cache = smelt_core::state::SessionCache::load();
    let mut available_models = cfg.resolve_models();

    if cfg.has_codex_provider() {
        let ids = engine::auth::cached_models(engine::auth::AuthProvider::Codex);
        if !ids.is_empty() {
            cfg.inject_codex_models(&mut available_models, &ids);
        }
        let client = http_client.clone();
        tokio::spawn(async move {
            let _ = engine::auth::refresh_models_cache(engine::auth::AuthProvider::Codex, &client)
                .await;
        });
    }

    if cfg.has_copilot_provider() {
        let ids = engine::auth::cached_models(engine::auth::AuthProvider::Copilot);
        if !ids.is_empty() {
            cfg.inject_copilot_models(&mut available_models, &ids);
        }
        let client = http_client.clone();
        tokio::spawn(async move {
            let _ =
                engine::auth::refresh_models_cache(engine::auth::AuthProvider::Copilot, &client)
                    .await;
        });
    }

    let mut startup_auth_error: Option<String> = None;

    let (api_base, api_key, api_key_env, mut provider_type, model, mut model_config) = {
        let resolved = resolve_model_reference(args, &cfg, &available_models, &cache);

        if let Some(r) = resolved {
            let base = args.api_base.clone().unwrap_or_else(|| r.api_base.clone());
            let key_env = args
                .api_key_env
                .clone()
                .unwrap_or_else(|| r.api_key_env.clone());
            let key = match resolve_api_key(&key_env) {
                Ok(key) => key,
                Err(err) => {
                    startup_auth_error = Some(err);
                    String::new()
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
            let key = match resolve_api_key(&key_env) {
                Ok(key) => key,
                Err(err) => {
                    startup_auth_error = Some(err);
                    String::new()
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
                engine::ProviderKind::detect_from_url(&base)
                    .as_config_str()
                    .to_string(),
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

    let mode_override = args
        .mode
        .as_deref()
        .or(cfg.defaults.mode.as_deref())
        .map(|s| {
            AgentMode::parse(s).unwrap_or_else(|| {
                eprintln!("warning: unknown mode '{s}', defaulting to normal");
                AgentMode::Normal
            })
        });

    let mode_cycle = args
        .mode_cycle
        .as_deref()
        .map(AgentMode::parse_list)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| AgentMode::ALL.to_vec());

    let reasoning_effort = args
        .reasoning_effort
        .as_deref()
        .and_then(ReasoningEffort::parse)
        .or_else(|| {
            cfg.defaults
                .reasoning_effort
                .as_deref()
                .and_then(ReasoningEffort::parse)
        })
        .unwrap_or(cache.reasoning_effort);

    let provider_kind = engine::ProviderKind::from_config(&provider_type);
    let mut reasoning_cycle = args
        .reasoning_cycle
        .as_deref()
        .map(ReasoningEffort::parse_list)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| provider_kind.default_reasoning_cycle().to_vec());
    if !reasoning_cycle.contains(&reasoning_effort) {
        reasoning_cycle.push(reasoning_effort);
    }

    let mut settings = cfg.settings.resolve();
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
        cache,
    }
}
