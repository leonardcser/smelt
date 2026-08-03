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
    use smelt_provider::copilot::{
        base_headers, base_url_from_token, build_copilot_token_request, client_id,
        github_poll_decision, parse_models_response, GitHubPollDecision, COPILOT_INTEGRATION_ID,
        COPILOT_USER_AGENT, EDITOR_PLUGIN_VERSION, EDITOR_VERSION,
    };

    #[test]
    fn client_id_decodes() {
        let cid = client_id();
        assert!(cid.starts_with("Iv1."));
        assert!(cid.len() > 10);
    }

    #[test]
    fn base_url_from_token_parses_proxy_ep() {
        let token = "tid=abc;exp=9999;proxy-ep=proxy.individual.githubcopilot.com;sku=x";
        assert_eq!(
            base_url_from_token(token).as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
    }

    #[test]
    fn base_url_from_token_handles_enterprise() {
        let token = "tid=abc;proxy-ep=proxy.business.githubcopilot.com";
        assert_eq!(
            base_url_from_token(token).as_deref(),
            Some("https://api.business.githubcopilot.com")
        );
    }

    #[test]
    fn base_url_from_token_returns_none_without_claim() {
        let token = "tid=abc;exp=9999;sku=x";
        assert_eq!(base_url_from_token(token), None);
    }

    #[test]
    fn base_url_from_token_handles_host_without_proxy_prefix() {
        let token = "proxy-ep=direct.example.com";
        assert_eq!(
            base_url_from_token(token).as_deref(),
            Some("https://api.direct.example.com")
        );
    }

    #[test]
    fn base_url_from_token_returns_none_for_empty_token() {
        assert_eq!(base_url_from_token(""), None);
    }

    // ---- CopilotTokens::needs_refresh ----

    fn tokens(expires_at: u64) -> CopilotTokens {
        CopilotTokens {
            refresh_token: "r".into(),
            access_token: "a".into(),
            expires_at,
            api_base: smelt_provider::copilot::DEFAULT_COPILOT_API_BASE.into(),
            last_refresh: 0,
        }
    }

    #[test]
    fn needs_refresh_true_when_within_60s_of_expiry() {
        let t = tokens(1_030);
        assert!(t.needs_refresh_at(1_000));
    }

    #[test]
    fn needs_refresh_true_when_past_expiry() {
        let t = tokens(999);
        assert!(t.needs_refresh_at(1_000));
    }

    #[test]
    fn needs_refresh_false_when_expiry_far_future() {
        let t = tokens(4_600);
        assert!(!t.needs_refresh_at(1_000));
    }

    // ---- base_headers ----

    #[test]
    fn base_headers_includes_expected_metadata() {
        let h = base_headers();
        let kv: std::collections::HashMap<&str, &str> = h.iter().copied().collect();
        assert_eq!(kv.get("User-Agent"), Some(&COPILOT_USER_AGENT));
        assert_eq!(kv.get("Editor-Version"), Some(&EDITOR_VERSION));
        assert_eq!(
            kv.get("Editor-Plugin-Version"),
            Some(&EDITOR_PLUGIN_VERSION)
        );
        assert_eq!(
            kv.get("Copilot-Integration-Id"),
            Some(&COPILOT_INTEGRATION_ID)
        );
    }

    #[test]
    fn github_poll_decision_classifies_success_pending_slowdown_and_errors() {
        assert_eq!(
            github_poll_decision(&serde_json::json!({"access_token": "github-token"}), 5_000),
            GitHubPollDecision::Authorized("github-token".to_string())
        );
        assert_eq!(
            github_poll_decision(
                &serde_json::json!({"error": "authorization_pending"}),
                5_000
            ),
            GitHubPollDecision::Pending
        );
        assert_eq!(
            github_poll_decision(&serde_json::json!({"error": "slow_down"}), 5_000),
            GitHubPollDecision::SlowDown {
                interval_ms: 10_000
            }
        );
        assert_eq!(
            github_poll_decision(
                &serde_json::json!({"error": "slow_down", "interval": 7}),
                5_000
            ),
            GitHubPollDecision::SlowDown { interval_ms: 7_000 }
        );
        assert_eq!(
            github_poll_decision(
                &serde_json::json!({"error": "bad_verification_code", "error_description": "nope"}),
                5_000
            ),
            GitHubPollDecision::Failed(
                "Device flow failed: bad_verification_code: nope".to_string()
            )
        );
    }

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

    #[test]
    fn build_copilot_token_request_uses_env_url_and_bearer_token() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(
            "https://copilot-token.example".to_string(),
            tmp.path().join("tokens.json"),
        );

        let req = build_copilot_token_request(&env.config, "github-token");

        assert_eq!(req.url, "https://copilot-token.example");
        assert_eq!(req.authorization, "Bearer github-token");
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

        let tokens = refresh_tokens_with_env(&reqwest::Client::new(), "github-refresh", &env)
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

    // ---- parse_models_response ----

    fn model(id: &str, capability_type: Option<&str>, picker: Option<bool>) -> serde_json::Value {
        let mut v = serde_json::json!({"id": id, "name": format!("{id} Name")});
        if let Some(c) = capability_type {
            v["capabilities"] = serde_json::json!({"type": c});
        }
        if let Some(p) = picker {
            v["model_picker_enabled"] = serde_json::json!(p);
        }
        v
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

    #[test]
    fn parse_models_returns_none_when_data_array_missing() {
        let v = serde_json::json!({});
        assert!(parse_models_response(&v).is_none());
    }

    #[test]
    fn parse_models_filters_out_non_chat_capabilities() {
        let v = serde_json::json!({"data": [
            model("a", Some("chat"), None),
            model("b", Some("embeddings"), None),
            model("c", Some(""), None),
        ]});
        let ms = parse_models_response(&v).unwrap();
        let ids: Vec<_> = ms.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"c")); // empty capability type is allowed
        assert!(!ids.contains(&"b"));
    }

    #[test]
    fn parse_models_filters_out_disabled_picker_entries() {
        let v = serde_json::json!({"data": [
            model("on", None, Some(true)),
            model("off", None, Some(false)),
        ]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].id, "on");
    }

    #[test]
    fn parse_models_filters_out_policy_disabled_entries() {
        let v = serde_json::json!({"data": [
            {"id": "enabled", "capabilities": {"type": "chat"}, "policy": {"state": "enabled"}},
            {"id": "disabled", "capabilities": {"type": "chat"}, "policy": {"state": "disabled"}},
            {"id": "unconfigured", "capabilities": {"type": "chat"}, "policy": {"state": "unconfigured"}}
        ]});
        let ms = parse_models_response(&v).unwrap();
        let ids: Vec<_> = ms.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["enabled", "unconfigured"]);
        assert_eq!(ms[0].policy_state.as_deref(), Some("enabled"));
    }

    #[test]
    fn parse_models_defaults_picker_to_enabled_when_missing() {
        let v = serde_json::json!({"data": [model("default", None, None)]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms.len(), 1);
    }

    #[test]
    fn parse_models_skips_entries_without_id() {
        let v = serde_json::json!({"data": [
            {"name": "no-id"},
            model("ok", None, None),
        ]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].id, "ok");
    }

    #[test]
    fn parse_models_extracts_context_window_from_max_context_window_tokens() {
        let v = serde_json::json!({"data": [{
            "id": "m", "model_picker_enabled": true,
            "capabilities": {"type": "chat",
                "limits": {"max_context_window_tokens": 128000}}
        }]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms[0].context_window, Some(128000));
    }

    #[test]
    fn parse_models_falls_back_to_max_prompt_tokens_for_context_window() {
        let v = serde_json::json!({"data": [{
            "id": "m", "model_picker_enabled": true,
            "capabilities": {"type": "chat",
                "limits": {"max_prompt_tokens": 8000}}
        }]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms[0].context_window, Some(8000));
    }

    #[test]
    fn parse_models_extracts_max_output_tokens() {
        let v = serde_json::json!({"data": [{
            "id": "m", "model_picker_enabled": true,
            "capabilities": {"type": "chat", "limits": {"max_output_tokens": 4096}}
        }]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms[0].max_output_tokens, Some(4096));
    }

    #[test]
    fn parse_models_sorts_by_id_and_dedups() {
        let v = serde_json::json!({"data": [
            model("zzz", None, None),
            model("aaa", None, None),
            model("aaa", None, None),
            model("mmm", None, None),
        ]});
        let ms = parse_models_response(&v).unwrap();
        let ids: Vec<_> = ms.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["aaa", "mmm", "zzz"]);
    }

    #[test]
    fn parse_models_uses_id_as_name_when_name_missing() {
        let v = serde_json::json!({"data": [{
            "id": "naked-id", "model_picker_enabled": true,
            "capabilities": {"type": "chat"}
        }]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms[0].name, "naked-id");
    }

    #[test]
    fn parse_models_captures_vendor_when_present() {
        let v = serde_json::json!({"data": [{
            "id": "m", "name": "M", "vendor": "openai", "model_picker_enabled": true,
            "capabilities": {"type": "chat"}
        }]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms[0].vendor.as_deref(), Some("openai"));
    }

    #[test]
    fn parse_models_vendor_none_when_absent() {
        let v = serde_json::json!({"data": [model("m", Some("chat"), Some(true))]});
        let ms = parse_models_response(&v).unwrap();
        assert!(ms[0].vendor.is_none());
    }

    #[test]
    fn parse_models_captures_family_and_capability_type() {
        let v = serde_json::json!({"data": [{
            "id": "m", "name": "M", "family": "claude", "model_picker_enabled": true,
            "capabilities": {"type": "chat"}
        }]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms[0].family.as_deref(), Some("claude"));
        assert_eq!(ms[0].capability_type.as_deref(), Some("chat"));
    }

    #[test]
    fn parse_models_infers_family_when_missing() {
        let v = serde_json::json!({"data": [{
            "id": "claude-sonnet-4.6", "name": "Claude Sonnet 4.6", "model_picker_enabled": true,
            "capabilities": {"type": "chat"}
        }]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms[0].family.as_deref(), Some("claude"));
    }

    #[test]
    fn parse_models_captures_supported_reasoning_efforts() {
        let v = serde_json::json!({"data": [{
            "id": "m", "model_picker_enabled": true,
            "supportedReasoningEfforts": ["low", "high"],
            "capabilities": {"type": "chat"}
        }]});
        let ms = parse_models_response(&v).unwrap();
        assert_eq!(ms[0].supported_reasoning_efforts, vec!["low", "high"]);
    }
}
