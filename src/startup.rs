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

fn validate_api_key(key_env: &str) -> Result<(), String> {
    resolve_api_key(key_env).map(drop)
}

pub fn resolve_project_cwd<I, S>(args: I, mut cwd: std::path::PathBuf) -> std::path::PathBuf
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let bootstrap = scan_bootstrap_args(args);
    if let Some(ref requested) = bootstrap.worktree {
        enter_startup_worktree(&mut cwd, requested, bootstrap.worktree_root.as_deref());
    }
    cwd
}

fn enter_startup_worktree(cwd: &mut std::path::PathBuf, requested: &str, root: Option<&str>) {
    let requested = requested.trim();
    let root = root.map(std::path::PathBuf::from);
    let spec = smelt_core::worktree::WorktreeSpec {
        name: (!requested.is_empty()).then_some(requested),
        base: None,
        root: root.as_deref(),
    };
    match smelt_core::worktree::enter_or_create(cwd, spec) {
        Ok(info) => {
            if let Err(e) = std::env::set_current_dir(&info.path) {
                eprintln!(
                    "error: failed to enter worktree {}: {e}",
                    info.path.display()
                );
                std::process::exit(1);
            }
            *cwd = std::env::current_dir().unwrap_or(info.path);
        }
        Err(e) => {
            eprintln!("error: --worktree: {e}");
            std::process::exit(1);
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BootstrapArgs {
    worktree: Option<String>,
    worktree_root: Option<String>,
}

fn scan_bootstrap_args<I, S>(args: I) -> BootstrapArgs
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut worktree = None;
    let mut worktree_root = None;
    let mut after_separator = false;
    let mut iter = args.into_iter().map(Into::into).skip(1).peekable();
    while let Some(arg) = iter.next() {
        let s = arg.to_string_lossy();
        if matches!(s.as_ref(), "--help" | "-h" | "--version" | "-v") {
            return BootstrapArgs::default();
        }
        if s == "--" {
            after_separator = true;
            continue;
        }
        if after_separator {
            continue;
        }
        if s == "--set" {
            if let Some(next) = iter.next() {
                let next = next.to_string_lossy();
                if let Some(value) = next.strip_prefix("worktree_root=") {
                    worktree_root = Some(value.to_string());
                }
            }
            continue;
        }
        if let Some(value) = s.strip_prefix("--set=worktree_root=") {
            worktree_root = Some(value.to_string());
            continue;
        }
        if s == "--worktree" || s == "-w" {
            let value = iter
                .peek()
                .and_then(|next| {
                    let next = next.to_string_lossy();
                    (!next.starts_with('-')).then(|| next.to_string())
                })
                .unwrap_or_default();
            if worktree.is_none() {
                worktree = Some(value);
            }
            continue;
        }
        if let Some(value) = s.strip_prefix("--worktree=") {
            if worktree.is_none() {
                worktree = Some(value.to_string());
            }
            continue;
        }
        if let Some(value) = s.strip_prefix("-w") {
            if !value.is_empty() && worktree.is_none() {
                worktree = Some(value.to_string());
            }
        }
    }
    BootstrapArgs {
        worktree,
        worktree_root,
    }
}

/// Fully resolved startup parameters, produced by [`resolve`] before the engine starts.
pub struct ResolvedStartup {
    pub runtime: smelt_core::RuntimeState,
    pub startup_overrides: smelt_core::StartupOverrides,
    pub startup_auth_error: Option<String>,
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

fn managed_model_metadata_needs_refresh(
    provider: engine::auth::AuthProvider,
    models: &[smelt_core::config::DynamicModel],
    refresh_if_empty: bool,
) -> bool {
    (refresh_if_empty && models.is_empty())
        || (provider == engine::auth::AuthProvider::Codex
            && models
                .iter()
                .any(|model| model.supports_fast_mode.is_none()))
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

    let cached_models = engine::auth::cached_model_info(provider);
    let client = http_client.clone();
    inject_managed_provider_models_with_refresh(
        cfg,
        available_models,
        provider,
        refresh_if_empty,
        cached_models,
        move || {
            let client = client.clone();
            async move { engine::auth::refresh_model_info(provider, &client).await }
        },
    )
    .await;
}

async fn inject_managed_provider_models_with_refresh<F, Fut>(
    cfg: &smelt_core::config::Config,
    available_models: &mut Vec<smelt_core::config::ResolvedModel>,
    provider: engine::auth::AuthProvider,
    refresh_if_empty: bool,
    mut models: Vec<engine::auth::AuthModelInfo>,
    refresh: F,
) where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Vec<engine::auth::AuthModelInfo>> + Send + 'static,
{
    if !has_managed_provider(cfg, provider) {
        return;
    }

    if managed_model_metadata_needs_refresh(provider, &models, refresh_if_empty) {
        models = refresh().await;
    }
    if !models.is_empty() {
        inject_managed_models(cfg, available_models, provider, &models);
    }

    tokio::spawn(async move {
        let _ = refresh().await;
    });
}

/// Resolve all startup configuration through the shared pure runtime resolver.
///
/// `http_client` is reused for managed-provider cache/refresh work so startup
/// does not rebuild a rustls client for each provider.
pub async fn resolve(
    args: &Args,
    cfg: smelt_core::config::Config,
    http_client: &reqwest::Client,
    registered_modes: &[AgentMode],
) -> ResolvedStartup {
    let mut cfg = cfg;
    cfg.inject_oauth_providers();

    let mut settings = std::collections::HashMap::new();
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
                Ok(number) if number.is_finite() => {
                    smelt_core::config::SettingValue::Number(number)
                }
                _ => {
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
        let mut validation = cfg.settings.clone();
        if let Err(error) = validation.set(key, &parsed) {
            eprintln!("error: --set {pair}: {error}");
            std::process::exit(1);
        }
        settings.insert(key.to_string(), parsed);
    }

    let parse_mode = |value: &str| {
        AgentMode::parse(value)
            .filter(|mode| registered_modes.is_empty() || registered_modes.contains(mode))
            .unwrap_or_else(|| {
                eprintln!("warning: invalid or unregistered mode '{value}', defaulting to normal");
                AgentMode::normal()
            })
    };
    let mode = args.mode.as_deref().map(parse_mode);
    let mode_cycle = args.mode_cycle.as_deref().and_then(|items| {
        let modes = AgentMode::parse_list(items)
            .into_iter()
            .filter(|mode| registered_modes.is_empty() || registered_modes.contains(mode))
            .collect::<Vec<_>>();
        (!modes.is_empty()).then_some(modes)
    });
    let reasoning_effort = args.reasoning_effort.as_deref().map(|value| {
        ReasoningEffort::parse(value).unwrap_or_else(|| {
            eprintln!("warning: invalid reasoning effort '{value}', defaulting to off");
            ReasoningEffort::Off
        })
    });
    let reasoning_cycle = args
        .reasoning_cycle
        .as_deref()
        .map(ReasoningEffort::parse_list)
        .filter(|cycle| !cycle.is_empty());
    let request_audit_env = std::env::var("SMELT_REQUEST_AUDIT").ok().map(|value| {
        protocol::RequestAuditMode::parse(&value).unwrap_or_else(|| {
            eprintln!("warning: invalid request audit mode {value:?}, defaulting to summary");
            protocol::RequestAuditMode::Summary
        })
    });
    let startup_overrides = smelt_core::StartupOverrides {
        model: args.model.clone(),
        api_base: args.api_base.clone(),
        api_key_env: args.api_key_env.clone(),
        provider_type: args.r#type.clone(),
        mode,
        mode_cycle,
        reasoning_effort,
        reasoning_cycle,
        model_config: protocol::ModelConfigOverrides {
            temperature: args.temperature,
            top_p: args.top_p,
            top_k: args.top_k,
            tool_calling: args.no_tool_calling.then_some(false),
            ..Default::default()
        },
        settings,
        request_audit_env,
    };

    let recent = smelt_core::state::Recent::load();
    let selections = smelt_core::RuntimeSelections {
        model: recent.selected_model.clone(),
        model_source: smelt_core::ModelSelectionSource::Remembered,
        mode: recent.mode(),
        reasoning_effort: recent.reasoning_effort,
    };
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
        false,
    )
    .await;
    inject_managed_provider_models(
        &cfg,
        &mut available_models,
        engine::auth::AuthProvider::KimiCode,
        http_client,
        false,
    )
    .await;

    let mut runtime = smelt_core::resolve_runtime(smelt_core::RuntimeInputs {
        config: &cfg,
        startup: &startup_overrides,
        available_models: &available_models,
        registered_modes,
        selections: &selections,
        previous: None,
        headless: args.headless,
    })
    .unwrap_or_else(|error| {
        eprintln!("error: {error}");
        std::process::exit(1);
    });

    let startup_auth_error = runtime.active_model().and_then(|model| {
        let provider_kind = smelt_provider::ProviderKind::from_config_and_url(
            &model.provider_type,
            &model.api_base,
        );
        (provider_kind != smelt_provider::ProviderKind::KimiCode)
            .then(|| validate_api_key(&model.api_key_env).err())
            .flatten()
    });
    if startup_auth_error.is_some() {
        if let Some(active) = runtime.active_model_mut() {
            active.availability = smelt_core::ModelAvailability::Unavailable {
                reason: smelt_core::ModelUnavailableReason::MissingCredentials,
            };
        }
    }

    ResolvedStartup {
        runtime,
        startup_overrides,
        startup_auth_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ModelRefreshControl {
        started: tokio::sync::oneshot::Receiver<()>,
        release: tokio::sync::oneshot::Sender<()>,
        completed: tokio::sync::oneshot::Receiver<()>,
    }

    #[derive(Clone)]
    struct ControlledModelRefresh {
        result: Vec<engine::auth::AuthModelInfo>,
        started: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
        release: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
        completed: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    }

    fn controlled_model_refresh(
        result: Vec<engine::auth::AuthModelInfo>,
    ) -> (ModelRefreshControl, ControlledModelRefresh) {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        (
            ModelRefreshControl {
                started: started_rx,
                release: release_tx,
                completed: completed_rx,
            },
            ControlledModelRefresh {
                result,
                started: std::sync::Arc::new(std::sync::Mutex::new(Some(started_tx))),
                release: std::sync::Arc::new(tokio::sync::Mutex::new(Some(release_rx))),
                completed: std::sync::Arc::new(std::sync::Mutex::new(Some(completed_tx))),
            },
        )
    }

    impl ModelRefreshControl {
        async fn release(self) {
            let _ = self.started.await;
            let _ = self.release.send(());
            let _ = self.completed.await;
        }
    }

    impl ControlledModelRefresh {
        async fn complete(self) -> Vec<engine::auth::AuthModelInfo> {
            if let Some(tx) = self.started.lock().unwrap().take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.release.lock().await.take() {
                let _ = rx.await;
            }
            if let Some(tx) = self.completed.lock().unwrap().take() {
                let _ = tx.send(());
            }
            self.result
        }
    }

    fn bootstrap(args: &[&str]) -> BootstrapArgs {
        scan_bootstrap_args(args.iter().copied())
    }

    #[test]
    fn bootstrap_args_detect_worktree_forms() {
        assert_eq!(
            bootstrap(&["smelt", "--worktree"]).worktree,
            Some("".into())
        );
        assert_eq!(
            bootstrap(&["smelt", "--worktree", "feature"]).worktree,
            Some("feature".into())
        );
        assert_eq!(
            bootstrap(&["smelt", "--worktree=feature"]).worktree,
            Some("feature".into())
        );
        assert_eq!(
            bootstrap(&["smelt", "-w", "feature"]).worktree,
            Some("feature".into())
        );
        assert_eq!(
            bootstrap(&["smelt", "-wfeature"]).worktree,
            Some("feature".into())
        );
    }

    #[test]
    fn bootstrap_args_detect_worktree_root_set_override() {
        assert_eq!(
            bootstrap(&[
                "smelt",
                "--set",
                "worktree_root=/tmp/worktrees",
                "--worktree"
            ])
            .worktree_root,
            Some("/tmp/worktrees".into())
        );
        assert_eq!(
            bootstrap(&[
                "smelt",
                "--set=worktree_root=.agent/worktrees",
                "--worktree"
            ])
            .worktree_root,
            Some(".agent/worktrees".into())
        );
    }

    #[test]
    fn bootstrap_args_ignore_worktree_for_help_version_and_separator() {
        assert_eq!(
            bootstrap(&["smelt", "--help", "--worktree=x"]),
            BootstrapArgs::default()
        );
        assert_eq!(
            bootstrap(&["smelt", "--worktree=x", "-v"]),
            BootstrapArgs::default()
        );
        assert_eq!(
            bootstrap(&["smelt", "--", "--worktree=x"]),
            BootstrapArgs::default()
        );
    }

    fn dynamic_model(supports_fast_mode: Option<bool>) -> smelt_core::config::DynamicModel {
        smelt_core::config::DynamicModel {
            id: "gpt-test".into(),
            display_name: None,
            context_window: None,
            supports_reasoning: None,
            supports_fast_mode,
            input_modalities: None,
        }
    }

    #[test]
    fn legacy_codex_cache_requires_metadata_refresh() {
        assert!(managed_model_metadata_needs_refresh(
            engine::auth::AuthProvider::Codex,
            &[dynamic_model(None)],
            false,
        ));
        assert!(!managed_model_metadata_needs_refresh(
            engine::auth::AuthProvider::Codex,
            &[dynamic_model(Some(false))],
            false,
        ));
        assert!(!managed_model_metadata_needs_refresh(
            engine::auth::AuthProvider::Copilot,
            &[dynamic_model(None)],
            false,
        ));
    }

    #[tokio::test]
    #[ignore = "hot reload refactor characterization"]
    async fn background_managed_model_refresh_updates_the_running_catalog() {
        let cfg = smelt_core::config::Config {
            providers: vec![smelt_core::config::ProviderConfig {
                name: Some("codex".into()),
                provider_type: Some("codex".into()),
                api_base: Some("https://example.invalid".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let cached = protocol::ModelMetadata {
            id: "cached-model".into(),
            display_name: None,
            context_window: None,
            supports_reasoning: None,
            supports_fast_mode: None,
            input_modalities: None,
        };
        let refreshed = protocol::ModelMetadata {
            id: "fresh-model".into(),
            display_name: None,
            context_window: Some(128_000),
            supports_reasoning: Some(true),
            supports_fast_mode: Some(true),
            input_modalities: Some(vec!["text".into()]),
        };
        let (control, refresh) = controlled_model_refresh(vec![refreshed]);

        let mut catalog = Vec::new();
        inject_managed_provider_models_with_refresh(
            &cfg,
            &mut catalog,
            engine::auth::AuthProvider::Codex,
            false,
            vec![cached],
            move || refresh.clone().complete(),
        )
        .await;

        control.release().await;

        assert!(
            catalog.iter().any(|model| model.key == "codex/fresh-model"),
            "a completed background refresh must update the running model catalog"
        );
    }
}
