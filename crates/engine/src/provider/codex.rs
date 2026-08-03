use crate::log;
use crate::paths::state_dir;
use std::sync::OnceLock;

const ISSUER: &str = smelt_provider::codex::ISSUER;
#[cfg(test)]
use smelt_provider::codex::REFRESH_INTERVAL_SECS;
use smelt_provider::codex::{CodexModel, CodexTokens};

pub(crate) const CODEX_TOKENS_ENV: &str = "SMELT_CODEX_TOKENS";

use smelt_provider::unix_now;

use super::{auth_storage::CredStore, LoginCallbacks};

fn cred_store() -> &'static CredStore {
    static STORE: OnceLock<CredStore> = OnceLock::new();

    STORE.get_or_init(|| {
        CredStore::production(
            "smelt-codex-auth",
            "default",
            state_dir().join("codex_auth.json"),
            CODEX_TOKENS_ENV,
        )
    })
}

#[derive(Clone)]
struct CodexAuthEnv {
    issuer: String,
    token_store: CredStore,
    now: fn() -> u64,
}

impl CodexAuthEnv {
    fn production() -> Self {
        Self {
            issuer: ISSUER.to_string(),
            token_store: cred_store().clone(),
            now: unix_now,
        }
    }
}

fn save_tokens_to(tokens: &CodexTokens, store: &CredStore) -> Result<(), String> {
    let json = serde_json::to_string_pretty(tokens).map_err(|e| e.to_string())?;
    store.save(&json)
}

pub(crate) fn load_tokens_passive() -> Option<CodexTokens> {
    let json = cred_store().load_passive()?;
    serde_json::from_str(&json).ok()
}

fn load_tokens_from(store: &CredStore) -> Option<CodexTokens> {
    let json = store.load()?;
    serde_json::from_str(&json).ok()
}

pub(crate) fn delete_tokens() {
    cred_store().delete();
}

/// Run the browser-based OAuth + PKCE flow: starts a local server, presents the
/// authorization URL, waits for the callback, exchanges the code for tokens, and saves them.
pub(crate) async fn browser_login(
    client: &reqwest::Client,
    callbacks: &LoginCallbacks<'_>,
) -> Result<CodexTokens, String> {
    let env = CodexAuthEnv::production();
    let progress = smelt_provider::codex::CodexLoginProgress {
        on_prompt: callbacks.on_prompt,
        on_progress: callbacks.on_progress,
    };
    let tokens =
        smelt_provider::codex::browser_login(client, &progress, &env.issuer, env.now).await?;
    save_tokens_to(&tokens, &env.token_store).map_err(|e| format!("failed to save tokens: {e}"))?;
    Ok(tokens)
}

pub(crate) async fn refresh_tokens(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<CodexTokens, String> {
    refresh_tokens_with_env(client, refresh_token, &CodexAuthEnv::production()).await
}

async fn refresh_tokens_with_env(
    client: &reqwest::Client,
    refresh_token: &str,
    env: &CodexAuthEnv,
) -> Result<CodexTokens, String> {
    let previous = load_tokens_from(&env.token_store);
    let result = smelt_provider::codex::refresh_tokens(
        client,
        refresh_token,
        previous.as_ref(),
        &env.issuer,
        (env.now)(),
    )
    .await?;
    save_tokens_to(&result, &env.token_store).map_err(|e| format!("failed to save tokens: {e}"))?;

    log::entry(
        log::Level::Debug,
        "codex_token_refreshed",
        &serde_json::json!({ "expires_at": result.expires_at }),
    );

    Ok(result)
}

pub(crate) fn cached_context_window(model: &str) -> Option<u32> {
    load_cached_models()
        .into_iter()
        .find(|m| m.slug == model)
        .and_then(|m| m.context_window)
}

pub(crate) fn load_cached_models() -> Vec<CodexModel> {
    let Some(account_fingerprint) =
        crate::auth::model_cache_account_fingerprint(crate::auth::AuthProvider::Codex)
    else {
        return Vec::new();
    };
    load_cached_models_for(&account_fingerprint)
}

pub(crate) fn load_cached_models_for(account_fingerprint: &str) -> Vec<CodexModel> {
    super::load_managed_model_cache(
        &crate::paths::cache_dir().join("codex_models.json"),
        crate::auth::AuthProvider::Codex.provider_type(),
        account_fingerprint,
    )
}

pub(crate) fn save_models_cache_for(
    account_fingerprint: String,
    models: &[CodexModel],
) -> Result<(), String> {
    super::save_managed_model_cache(
        &crate::paths::cache_dir().join("codex_models.json"),
        crate::auth::AuthProvider::Codex.provider_type(),
        account_fingerprint,
        models,
    )
}

pub(crate) async fn fetch_models_fresh(
    client: &reqwest::Client,
) -> Result<Vec<CodexModel>, String> {
    let (access_token, account_id) = ensure_access_token(client).await?;
    smelt_provider::codex::fetch_models(client, &access_token, account_id.as_deref()).await
}

/// Device-code flow for headless environments.
pub(crate) async fn device_code_login(
    client: &reqwest::Client,
    callbacks: &LoginCallbacks<'_>,
) -> Result<CodexTokens, String> {
    let env = CodexAuthEnv::production();
    let progress = smelt_provider::codex::CodexLoginProgress {
        on_prompt: callbacks.on_prompt,
        on_progress: callbacks.on_progress,
    };
    let tokens =
        smelt_provider::codex::device_code_login(client, &progress, &env.issuer, env.now).await?;
    save_tokens_to(&tokens, &env.token_store).map_err(|e| format!("failed to save tokens: {e}"))?;
    Ok(tokens)
}

pub(crate) async fn ensure_access_token_full(
    client: &reqwest::Client,
) -> Result<CodexTokens, String> {
    ensure_access_token_full_with_env(client, &CodexAuthEnv::production()).await
}

async fn ensure_access_token_full_with_env(
    client: &reqwest::Client,
    env: &CodexAuthEnv,
) -> Result<CodexTokens, String> {
    let tokens = load_tokens_from(&env.token_store)
        .ok_or("not logged in to Codex; run `smelt auth` first")?;

    if !tokens.needs_refresh_at((env.now)()) {
        return Ok(tokens);
    }

    refresh_tokens_with_env(client, &tokens.refresh_token, env).await
}

async fn ensure_access_token(client: &reqwest::Client) -> Result<(String, Option<String>), String> {
    let tokens = ensure_access_token_full(client).await?;
    Ok((tokens.access_token, tokens.account_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_http::spawn_json_response;
    use base64::Engine;
    use smelt_provider::codex::{
        extract_account_id, merge_token_response, parse_jwt_claims, parse_jwt_expiration,
    };

    fn jwt_with(payload: &serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(payload).unwrap());
        format!("{header}.{body}.sig")
    }

    // ---- needs_refresh ----

    fn tokens(expires_at: u64, last_refresh: u64) -> CodexTokens {
        CodexTokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at,
            account_id: None,
            last_refresh,
        }
    }

    #[test]
    fn needs_refresh_true_when_within_60_seconds_of_expiry() {
        let t = tokens(1_030, 900);
        assert!(t.needs_refresh_at(1_000));
    }

    #[test]
    fn needs_refresh_true_when_past_expiry() {
        let t = tokens(900, 800);
        assert!(t.needs_refresh_at(1_000));
    }

    #[test]
    fn needs_refresh_false_when_expiry_far_and_recent_refresh() {
        let t = tokens(8_200, 940);
        assert!(!t.needs_refresh_at(1_000));
    }

    #[test]
    fn needs_refresh_true_when_last_refresh_older_than_interval() {
        let t = tokens(REFRESH_INTERVAL_SECS + 2_000, 1_000);
        assert!(t.needs_refresh_at(1_000 + REFRESH_INTERVAL_SECS + 1));
    }

    #[test]
    fn needs_refresh_ignores_last_refresh_when_zero() {
        // last_refresh == 0 means never refreshed, the time-based check is skipped.
        let t = tokens(8_200, 0);
        assert!(!t.needs_refresh_at(1_000));
    }

    // ---- parse_jwt_claims ----

    #[test]
    fn parse_jwt_claims_reads_chatgpt_account_id_from_top_level() {
        let jwt = jwt_with(&serde_json::json!({"chatgpt_account_id": "acct-1"}));
        let c = parse_jwt_claims(&jwt).unwrap();
        assert_eq!(c.chatgpt_account_id.as_deref(), Some("acct-1"));
    }

    #[test]
    fn parse_jwt_claims_reads_chatgpt_account_id_from_auth_ext() {
        let jwt = jwt_with(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "from-ext"}
        }));
        let c = parse_jwt_claims(&jwt).unwrap();
        assert_eq!(
            c.auth_ext
                .as_ref()
                .and_then(|a| a.chatgpt_account_id.clone()),
            Some("from-ext".to_string())
        );
    }

    #[test]
    fn parse_jwt_claims_returns_none_when_token_does_not_have_three_segments() {
        assert!(parse_jwt_claims("notajwt").is_none());
        assert!(parse_jwt_claims("a.b").is_none());
    }

    #[test]
    fn parse_jwt_claims_returns_none_when_payload_is_not_base64() {
        assert!(parse_jwt_claims("header.!!!.sig").is_none());
    }

    // ---- extract_account_id ----

    #[test]
    fn extract_account_id_prefers_id_token_over_access_token() {
        let id = jwt_with(&serde_json::json!({"chatgpt_account_id": "from-id"}));
        let access = jwt_with(&serde_json::json!({"chatgpt_account_id": "from-access"}));
        assert_eq!(
            extract_account_id(&access, Some(&id)).as_deref(),
            Some("from-id")
        );
    }

    #[test]
    fn extract_account_id_falls_back_to_access_token_when_id_token_lacks_claim() {
        let id = jwt_with(&serde_json::json!({}));
        let access = jwt_with(&serde_json::json!({"chatgpt_account_id": "from-access"}));
        assert_eq!(
            extract_account_id(&access, Some(&id)).as_deref(),
            Some("from-access")
        );
    }

    #[test]
    fn extract_account_id_returns_none_when_neither_token_has_claim() {
        let id = jwt_with(&serde_json::json!({}));
        let access = jwt_with(&serde_json::json!({}));
        assert!(extract_account_id(&access, Some(&id)).is_none());
    }

    #[test]
    fn extract_account_id_uses_auth_ext_fallback_path() {
        let access = jwt_with(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "ext-acct"}
        }));
        assert_eq!(
            extract_account_id(&access, None).as_deref(),
            Some("ext-acct")
        );
    }

    // ---- build_authorize_url ----

    #[test]
    fn build_authorize_url_includes_all_required_params() {
        let pkce = smelt_provider::codex::PkceCodes {
            verifier: "v".into(),
            challenge: "ch".into(),
        };
        let url = smelt_provider::codex::build_authorize_url(
            ISSUER,
            "http://localhost:1455/auth/callback",
            &pkce,
            "STATE",
        );
        assert!(url.starts_with(ISSUER));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id="));
        assert!(url.contains("code_challenge=ch"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE"));
        assert!(url.contains("originator=smelt"));
        // redirect_uri is url-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    }

    fn fixed_now() -> u64 {
        1_000
    }

    fn test_env(issuer: String, token_path: std::path::PathBuf) -> CodexAuthEnv {
        CodexAuthEnv {
            issuer,
            token_store: CredStore::file_only(token_path),
            now: fixed_now,
        }
    }

    #[tokio::test]
    async fn refresh_tokens_with_env_uses_local_store_and_persists_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let access = jwt_with(&serde_json::json!({
            "exp": 1_500_u64,
            "chatgpt_account_id": "new-acct"
        }));
        let (issuer, task) = spawn_json_response(
            serde_json::json!({
                "access_token": access,
                "expires_in": 120
            })
            .to_string(),
        )
        .await;
        let env = test_env(issuer, tmp.path().join("tokens.json"));
        save_tokens_to(&previous_tokens(), &env.token_store).unwrap();

        let out = refresh_tokens_with_env(&reqwest::Client::new(), "prev-refresh", &env)
            .await
            .unwrap();

        assert_eq!(out.refresh_token, "prev-refresh");
        assert_eq!(out.expires_at, 1_500);
        assert_eq!(out.account_id.as_deref(), Some("new-acct"));
        assert_eq!(out.last_refresh, fixed_now());
        let request = task.await.unwrap();
        assert!(request.starts_with("POST /oauth/token HTTP/1.1"));
        assert!(request.contains(r#""grant_type":"refresh_token""#));
        assert!(request.contains(r#""refresh_token":"prev-refresh""#));
        let saved = std::fs::read_to_string(&env.token_store.file_path).unwrap();
        assert!(saved.contains("prev-refresh"), "{saved}");
        assert!(saved.contains("new-acct"), "{saved}");
    }

    // ---- provider classify_refresh_error ----

    #[test]
    fn provider_classify_refresh_error_expired() {
        let s =
            smelt_provider::codex::classify_refresh_error(r#"{"error":"refresh_token_expired"}"#);
        assert!(s.contains("expired"));
        assert!(s.contains("smelt auth"));
    }

    #[test]
    fn provider_classify_refresh_error_reused() {
        let s = smelt_provider::codex::classify_refresh_error("refresh_token_reused");
        assert!(s.contains("already used"));
    }

    #[test]
    fn provider_classify_refresh_error_invalidated() {
        let s = smelt_provider::codex::classify_refresh_error("refresh_token_invalidated detail");
        assert!(s.contains("revoked"));
    }

    #[test]
    fn provider_classify_refresh_error_unknown_returns_raw_body_prefixed() {
        let s = smelt_provider::codex::classify_refresh_error("server is on fire");
        assert!(s.starts_with("token refresh error:"));
        assert!(s.contains("server is on fire"));
    }

    // ---- parse_jwt_expiration ----

    #[test]
    fn parse_jwt_expiration_reads_exp_claim() {
        let jwt = jwt_with(&serde_json::json!({"exp": 1_700_000_000_u64}));
        assert_eq!(parse_jwt_expiration(&jwt), Some(1_700_000_000));
    }

    #[test]
    fn parse_jwt_expiration_returns_none_without_exp() {
        let jwt = jwt_with(&serde_json::json!({}));
        assert_eq!(parse_jwt_expiration(&jwt), None);
    }

    #[test]
    fn parse_jwt_expiration_returns_none_for_garbage() {
        assert_eq!(parse_jwt_expiration("not-a-jwt"), None);
    }

    // ---- merge_token_response ----

    fn previous_tokens() -> CodexTokens {
        CodexTokens {
            access_token: "prev-access".into(),
            refresh_token: "prev-refresh".into(),
            expires_at: 0,
            account_id: Some("prev-acct".into()),
            last_refresh: 0,
        }
    }

    #[test]
    fn merge_token_response_reuses_previous_refresh_token_when_missing() {
        let access = jwt_with(&serde_json::json!({"exp": 1_700_000_000_u64}));
        let resp = smelt_provider::codex::TokenResponse {
            access_token: Some(access.clone()),
            refresh_token: None,
            id_token: None,
            expires_in: None,
        };
        let out = merge_token_response(resp, Some(&previous_tokens()), 1_000).unwrap();
        assert_eq!(out.access_token, access);
        assert_eq!(out.refresh_token, "prev-refresh");
        assert_eq!(out.account_id.as_deref(), Some("prev-acct"));
        assert_eq!(out.last_refresh, 1_000);
    }

    #[test]
    fn merge_token_response_prefers_jwt_exp_over_expires_in() {
        let jwt_exp = 1_800_000_000_u64;
        let access = jwt_with(&serde_json::json!({"exp": jwt_exp}));
        let resp = smelt_provider::codex::TokenResponse {
            access_token: Some(access),
            refresh_token: Some("new-refresh".into()),
            id_token: None,
            expires_in: Some(60),
        };
        let out = merge_token_response(resp, Some(&previous_tokens()), 1_000).unwrap();
        assert_eq!(out.expires_at, jwt_exp);
        assert_eq!(out.refresh_token, "new-refresh");
    }

    #[test]
    fn merge_token_response_falls_back_to_expires_in_when_jwt_has_no_exp() {
        let access = jwt_with(&serde_json::json!({}));
        let resp = smelt_provider::codex::TokenResponse {
            access_token: Some(access),
            refresh_token: Some("r".into()),
            id_token: None,
            expires_in: Some(120),
        };
        let out = merge_token_response(resp, None, 1_000).unwrap();
        assert_eq!(out.expires_at, 1_120);
    }

    #[test]
    fn merge_token_response_errors_when_no_access_token_anywhere() {
        let resp = smelt_provider::codex::TokenResponse {
            access_token: None,
            refresh_token: Some("r".into()),
            id_token: None,
            expires_in: Some(60),
        };
        let err = merge_token_response(resp, None, 0).unwrap_err();
        assert!(err.contains("access_token"));
    }

    #[test]
    fn merge_token_response_errors_when_no_refresh_token_anywhere() {
        let access = jwt_with(&serde_json::json!({"exp": 1_700_000_000_u64}));
        let resp = smelt_provider::codex::TokenResponse {
            access_token: Some(access),
            refresh_token: None,
            id_token: None,
            expires_in: Some(60),
        };
        let err = merge_token_response(resp, None, 0).unwrap_err();
        assert!(err.contains("refresh_token"));
    }

    // ---- pkce / state helpers (smoke) ----

    #[test]
    fn generate_pkce_produces_nonempty_verifier_and_challenge() {
        let p = smelt_provider::codex::generate_pkce();
        assert!(!p.verifier.is_empty());
        assert!(!p.challenge.is_empty());
        assert_ne!(p.verifier, p.challenge);
    }

    #[test]
    fn parse_jwt_claims_returns_none_when_payload_is_not_json() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"not-json");
        assert!(parse_jwt_claims(&format!("{header}.{body}.sig")).is_none());
    }

    #[test]
    fn apply_headers_includes_bearer_and_optional_account_id() {
        let client = reqwest::Client::new();
        let req = CodexTokens {
            access_token: "access-token".into(),
            refresh_token: "refresh-token".into(),
            expires_at: 0,
            account_id: Some("acct-123".into()),
            last_refresh: 0,
        }
        .apply_headers(client.get("https://example.invalid"))
        .build()
        .unwrap();

        assert_eq!(
            req.headers().get("authorization").unwrap(),
            "Bearer access-token"
        );
        assert_eq!(req.headers().get("ChatGPT-Account-ID").unwrap(), "acct-123");
    }

    #[test]
    fn merge_token_response_prefers_new_account_id_from_id_token() {
        let access = jwt_with(&serde_json::json!({"exp": 1_800_000_000_u64}));
        let id = jwt_with(&serde_json::json!({"chatgpt_account_id": "new-acct"}));
        let resp = smelt_provider::codex::TokenResponse {
            access_token: Some(access),
            refresh_token: Some("new-refresh".into()),
            id_token: Some(id),
            expires_in: None,
        };

        let out = merge_token_response(resp, Some(&previous_tokens()), 1_000).unwrap();

        assert_eq!(out.account_id.as_deref(), Some("new-acct"));
    }

    #[test]
    fn merge_token_response_reuses_previous_access_token_when_missing() {
        let resp = smelt_provider::codex::TokenResponse {
            access_token: None,
            refresh_token: Some("new-refresh".into()),
            id_token: None,
            expires_in: Some(60),
        };

        let out = merge_token_response(resp, Some(&previous_tokens()), 1_000).unwrap();

        assert_eq!(out.access_token, "prev-access");
        assert_eq!(out.refresh_token, "new-refresh");
        assert_eq!(out.expires_at, 1_060);
    }

    #[test]
    fn codex_model_metadata_preserves_display_and_context() {
        let meta: protocol::ModelMetadata = CodexModel {
            slug: "codex-mini".into(),
            display_name: "Codex Mini".into(),
            description: Some("desc".into()),
            context_window: Some(1234),
            supports_fast_mode: Some(true),
        }
        .into();

        assert_eq!(meta.id, "codex-mini");
        assert_eq!(meta.display_name.as_deref(), Some("Codex Mini"));
        assert_eq!(meta.context_window, Some(1234));
        assert_eq!(meta.supports_reasoning, None);
        assert_eq!(meta.supports_fast_mode, Some(true));
    }

    #[test]
    fn generate_state_produces_nonempty_unique_strings() {
        let a = smelt_provider::codex::generate_state();
        let b = smelt_provider::codex::generate_state();
        assert!(!a.is_empty());
        assert_ne!(a, b);
    }
}
