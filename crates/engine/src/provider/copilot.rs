//! GitHub Copilot authentication storage and cached model access.

use crate::log;
use crate::paths::state_dir;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use smelt_provider::copilot::{self, CopilotAuthConfig, CopilotLoginProgress};
use smelt_provider::copilot::{CopilotModel, CopilotTokens};

use smelt_provider::unix_now;

use super::{auth_storage::CredStore, LoginCallbacks};

const COPILOT_TOKENS_ENV: &str = "SMELT_COPILOT_TOKENS";

fn cred_store() -> &'static CredStore {
    static STORE: OnceLock<CredStore> = OnceLock::new();

    STORE.get_or_init(|| {
        CredStore::production(
            "smelt-copilot-auth",
            "default",
            state_dir().join("copilot_auth.json"),
            COPILOT_TOKENS_ENV,
        )
    })
}

#[derive(Clone)]
struct CopilotAuthEnv {
    token_store: CredStore,
    config: CopilotAuthConfig,
}

impl CopilotAuthEnv {
    fn production() -> Self {
        Self {
            token_store: cred_store().clone(),
            config: CopilotAuthConfig::production(unix_now),
        }
    }
}

fn save_tokens_to(tokens: &CopilotTokens, store: &CredStore) -> Result<(), String> {
    let json = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    store.save(&json)
}

fn load_tokens_from(store: &CredStore) -> Option<CopilotTokens> {
    let json = store.load()?;
    serde_json::from_str(&json).ok()
}

pub(crate) fn load_tokens_passive() -> Option<CopilotTokens> {
    let json = cred_store().load_passive()?;
    serde_json::from_str(&json).ok()
}

pub(crate) fn delete_tokens() {
    cred_store().delete();
}

pub(crate) async fn device_code_login(
    client: &reqwest::Client,
    callbacks: &LoginCallbacks<'_>,
) -> Result<CopilotTokens, String> {
    let progress = CopilotLoginProgress {
        on_prompt: callbacks.on_prompt,
        on_progress: callbacks.on_progress,
    };
    let tokens =
        copilot::device_code_login(client, &progress, &CopilotAuthEnv::production().config).await?;
    save_tokens_to(&tokens, cred_store()).map_err(|e| format!("failed to save tokens: {e}"))?;

    (callbacks.on_progress)("Fetching Copilot models…");
    let models =
        match copilot::fetch_available_models(client, &tokens.access_token, &tokens.api_base).await
        {
            Ok(m) => m,
            Err(e) => {
                log::entry(
                    log::Level::Warn,
                    "copilot_fetch_models_failed",
                    &serde_json::json!({ "error": e }),
                );
                Vec::new()
            }
        };
    if !models.is_empty() {
        (callbacks.on_progress)(&format!("Fetched {} Copilot models", models.len()));
        let account_fingerprint = crate::auth::model_cache_fingerprint(
            crate::auth::AuthProvider::Copilot,
            "refresh_token",
            &tokens.refresh_token,
        );
        if let Err(error) = save_models_cache_to(&cache_path(), account_fingerprint, &models) {
            log::entry(
                log::Level::Warn,
                "copilot_models_cache_write_failed",
                &serde_json::json!({ "error": error }),
            );
        }
    }

    Ok(tokens)
}

pub(crate) async fn refresh_tokens(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<CopilotTokens, String> {
    refresh_tokens_with_env(client, refresh_token, &CopilotAuthEnv::production()).await
}

async fn refresh_tokens_with_env(
    client: &reqwest::Client,
    refresh_token: &str,
    env: &CopilotAuthEnv,
) -> Result<CopilotTokens, String> {
    let tokens = copilot::refresh_tokens(client, refresh_token, &env.config).await?;
    save_tokens_to(&tokens, &env.token_store).map_err(|e| format!("failed to save tokens: {e}"))?;
    log::entry(
        log::Level::Debug,
        "copilot_token_refreshed",
        &serde_json::json!({ "expires_at": tokens.expires_at }),
    );
    Ok(tokens)
}

pub(crate) async fn ensure_access_token_full(
    client: &reqwest::Client,
) -> Result<CopilotTokens, String> {
    ensure_access_token_full_with_env(client, &CopilotAuthEnv::production()).await
}

async fn ensure_access_token_full_with_env(
    client: &reqwest::Client,
    env: &CopilotAuthEnv,
) -> Result<CopilotTokens, String> {
    let tokens = load_tokens_from(&env.token_store)
        .ok_or("not logged in to GitHub Copilot; run `smelt auth` first")?;
    if !tokens.needs_refresh_at((env.config.now)()) {
        return Ok(tokens);
    }
    refresh_tokens_with_env(client, &tokens.refresh_token, env).await
}

fn cache_path() -> PathBuf {
    crate::paths::cache_dir().join("copilot_models.json")
}

pub(crate) fn load_cached_models() -> Vec<CopilotModel> {
    let Some(account_fingerprint) =
        crate::auth::model_cache_account_fingerprint(crate::auth::AuthProvider::Copilot)
    else {
        return Vec::new();
    };
    load_cached_models_for(&account_fingerprint)
}

pub(crate) fn load_cached_models_for(account_fingerprint: &str) -> Vec<CopilotModel> {
    load_cached_models_from(&cache_path(), account_fingerprint)
}

fn load_cached_models_from(path: &Path, account_fingerprint: &str) -> Vec<CopilotModel> {
    let models = super::load_managed_model_cache(
        path,
        crate::auth::AuthProvider::Copilot.provider_type(),
        account_fingerprint,
    );
    if !models.is_empty()
        && models
            .iter()
            .all(|m: &CopilotModel| m.policy_state.is_none())
    {
        return Vec::new();
    }
    models
        .into_iter()
        .filter(|m| m.policy_state.as_deref() != Some("disabled"))
        .collect()
}

fn save_models_cache_to(
    path: &Path,
    account_fingerprint: String,
    models: &[CopilotModel],
) -> Result<(), String> {
    super::save_managed_model_cache(
        path,
        crate::auth::AuthProvider::Copilot.provider_type(),
        account_fingerprint,
        models,
    )
}

pub(crate) fn save_models_cache_for(
    account_fingerprint: String,
    models: &[CopilotModel],
) -> Result<(), String> {
    save_models_cache_to(&cache_path(), account_fingerprint, models)
}

pub(crate) async fn fetch_models_fresh(
    client: &reqwest::Client,
) -> Result<Vec<CopilotModel>, String> {
    fetch_models_fresh_with_env(client, &CopilotAuthEnv::production()).await
}

async fn fetch_models_fresh_with_env(
    client: &reqwest::Client,
    env: &CopilotAuthEnv,
) -> Result<Vec<CopilotModel>, String> {
    let tokens = ensure_access_token_full_with_env(client, env).await?;
    copilot::fetch_available_models(client, &tokens.access_token, &tokens.api_base).await
}

pub(crate) fn cached_model(model: &str) -> Option<CopilotModel> {
    load_cached_models().into_iter().find(|m| m.id == model)
}

pub(crate) fn cached_context_window(model: &str) -> Option<u32> {
    cached_model(model).and_then(|m| m.context_window)
}

pub(crate) fn cached_output_tokens(model: &str) -> Option<u32> {
    cached_model(model).and_then(|m| m.max_output_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_http::spawn_json_response;
    use smelt_provider::copilot::parse_models_response;

    fn fixed_now() -> u64 {
        1_000
    }

    fn test_env(token_url: String, token_path: std::path::PathBuf) -> CopilotAuthEnv {
        CopilotAuthEnv {
            token_store: CredStore::file_only(token_path),
            config: CopilotAuthConfig {
                copilot_token_url: token_url,
                now: fixed_now,
            },
        }
    }

    #[tokio::test]
    async fn refresh_tokens_with_env_uses_local_store_and_fixed_clock() {
        let tmp = tempfile::tempdir().unwrap();
        let (token_url, task) = spawn_json_response(
            serde_json::json!({
                "token": "tid=abc;proxy-ep=proxy.business.githubcopilot.com",
                "expires_at": 1_500_u64
            })
            .to_string(),
        )
        .await;
        let env = test_env(token_url, tmp.path().join("tokens.json"));

        let tokens =
            refresh_tokens_with_env(&smelt_provider::http_client(), "github-refresh", &env)
                .await
                .unwrap();

        assert_eq!(tokens.refresh_token, "github-refresh");
        assert_eq!(tokens.expires_at, 1_500);
        assert_eq!(tokens.last_refresh, fixed_now());
        assert_eq!(tokens.api_base, "https://api.business.githubcopilot.com");
        let request = task.await.unwrap();
        assert!(request.starts_with("GET / HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer github-refresh"));
        let saved = std::fs::read_to_string(&env.token_store.file_path).unwrap();
        assert!(saved.contains("github-refresh"), "{saved}");
        assert!(saved.contains("api.business.githubcopilot.com"), "{saved}");
    }

    #[test]
    fn model_cache_rejects_a_different_account() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("models.json");
        let models = parse_models_response(&serde_json::json!({"data": [{
            "id": "account-a-model",
            "policy": {"state": "enabled"}
        }]}))
        .unwrap();
        save_models_cache_to(&path, "account-a".into(), &models).unwrap();

        assert_eq!(load_cached_models_from(&path, "account-a").len(), 1);
        assert!(load_cached_models_from(&path, "account-b").is_empty());
    }
}
