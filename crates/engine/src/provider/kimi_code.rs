use crate::paths::{cache_dir, state_dir};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rand::RngExt;

use super::{auth_storage::CredStore, unix_now, LoginCallbacks};

pub const API_BASE: &str = "https://api.kimi.com/coding/v1";
const OAUTH_HOST: &str = "https://auth.kimi.com";
const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const TOKENS_ENV: &str = "SMELT_KIMI_CODE_TOKENS";
const MODELS_CACHE_VERSION: u32 = 1;

pub fn is_api_base(api_base: &str) -> bool {
    api_base
        .trim_end_matches('/')
        .contains("api.kimi.com/coding")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KimiCodeModelInfo {
    pub id: String,
    pub context_length: Option<u32>,
    pub supports_reasoning: Option<bool>,
    pub supports_image_in: Option<bool>,
    pub supports_video_in: Option<bool>,
    pub supports_tool_use: Option<bool>,
    pub display_name: Option<String>,
}

impl KimiCodeModelInfo {
    fn from_json(item: &serde_json::Value) -> Option<Self> {
        let id = item["id"].as_str()?.to_string();
        let display_name = item["display_name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Some(Self {
            id,
            context_length: json_u32(&item["context_length"]),
            supports_reasoning: item.get("supports_reasoning").and_then(|v| v.as_bool()),
            supports_image_in: item.get("supports_image_in").and_then(|v| v.as_bool()),
            supports_video_in: item.get("supports_video_in").and_then(|v| v.as_bool()),
            supports_tool_use: item.get("supports_tool_use").and_then(|v| v.as_bool()),
            display_name,
        })
    }

    pub fn matches_name(&self, model: &str) -> bool {
        self.id.eq_ignore_ascii_case(model)
            || self
                .display_name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(model))
    }
}

impl From<KimiCodeModelInfo> for protocol::ModelMetadata {
    fn from(model: KimiCodeModelInfo) -> Self {
        Self {
            id: model.id,
            display_name: model.display_name,
            context_window: model.context_length,
            supports_reasoning: model.supports_reasoning,
        }
    }
}

fn json_u32(value: &serde_json::Value) -> Option<u32> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse::<u64>().ok())
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KimiCodeModelsCache {
    version: u32,
    models: Vec<KimiCodeModelInfo>,
}

impl KimiCodeModelsCache {
    fn new(models: Vec<KimiCodeModelInfo>) -> Self {
        Self {
            version: MODELS_CACHE_VERSION,
            models,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct KimiCodeTokens {
    access_token: String,
    refresh_token: String,
    pub(crate) expires_at: u64,
    #[serde(default)]
    scope: String,
    #[serde(default = "default_bearer")]
    token_type: String,
}

fn default_bearer() -> String {
    "Bearer".to_string()
}

fn cred_store() -> CredStore {
    CredStore::production("smelt-kimi-code-auth", "default", token_path(), TOKENS_ENV)
}

#[derive(Clone)]
struct KimiAuthEnv {
    oauth_host: String,
    api_base: String,
    token_store: CredStore,
    models_cache_path: PathBuf,
    now: fn() -> u64,
}

impl KimiAuthEnv {
    fn production() -> Self {
        Self {
            oauth_host: oauth_host(),
            api_base: api_base(),
            token_store: cred_store(),
            models_cache_path: models_cache_path(),
            now: unix_now,
        }
    }
}

fn token_path() -> PathBuf {
    state_dir().join("kimi_code_auth.json")
}

impl KimiCodeTokens {
    fn save_to(&self, store: &CredStore) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        store.save(&json)
    }

    fn load() -> Option<Self> {
        Self::load_from(&cred_store())
    }

    fn load_from(store: &CredStore) -> Option<Self> {
        let json = store.load()?;
        serde_json::from_str(&json).ok()
    }

    fn delete() {
        cred_store().delete();
    }
}

pub fn is_logged_in() -> bool {
    KimiCodeTokens::load().is_some()
}

pub fn logout() {
    KimiCodeTokens::delete();
}

pub fn api_base() -> String {
    std::env::var("KIMI_CODE_BASE_URL")
        .unwrap_or_else(|_| API_BASE.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn device_id_path() -> PathBuf {
    state_dir().join("kimi_code_device_id")
}

fn create_device_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn device_id() -> String {
    let path = device_id_path();
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }

    let id = create_device_id();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    id
}

fn ascii_header(value: impl AsRef<str>, fallback: &str) -> String {
    let cleaned: String = value
        .as_ref()
        .chars()
        .filter(|ch| ch.is_ascii_graphic() || *ch == ' ')
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

fn default_headers() -> Vec<(&'static str, String)> {
    let version = env!("CARGO_PKG_VERSION");
    // Kimi's coding API expects these device/platform headers for OAuth-backed
    // requests. The User-Agent remains Smelt-owned; this is not Kimi CLI
    // credential storage or User-Agent impersonation.
    vec![
        ("User-Agent", format!("smelt/{version}")),
        ("X-Msh-Platform", "kimi_code_cli".to_string()),
        ("X-Msh-Version", version.to_string()),
        (
            "X-Msh-Device-Name",
            ascii_header(std::env::var("HOSTNAME").unwrap_or_default(), "unknown"),
        ),
        (
            "X-Msh-Device-Model",
            ascii_header(
                format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
                "unknown",
            ),
        ),
        (
            "X-Msh-Os-Version",
            ascii_header(std::env::consts::OS, "unknown"),
        ),
        ("X-Msh-Device-Id", device_id()),
    ]
}

pub(crate) fn apply_default_headers(mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    for (key, value) in default_headers() {
        req = req.header(key, value);
    }
    req
}

fn oauth_host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| std::env::var("KIMI_OAUTH_HOST"))
        .unwrap_or_else(|_| OAUTH_HOST.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn token_from_response_at(value: &serde_json::Value, now: u64) -> Result<KimiCodeTokens, String> {
    let access_token = value["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("OAuth response missing access_token")?
        .to_string();
    let refresh_token = value["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("OAuth response missing refresh_token")?
        .to_string();
    let expires_in = value["expires_in"]
        .as_u64()
        .ok_or("OAuth response missing expires_in")?;
    Ok(KimiCodeTokens {
        access_token,
        refresh_token,
        expires_at: now.saturating_add(expires_in),
        scope: value["scope"].as_str().unwrap_or_default().to_string(),
        token_type: value["token_type"].as_str().unwrap_or("Bearer").to_string(),
    })
}

struct OAuthResponse {
    status: u16,
    body: String,
    json: serde_json::Value,
}

struct OAuthFormRequest {
    url: String,
    body: String,
}

fn build_oauth_form_request(
    env: &KimiAuthEnv,
    path: &str,
    params: &[(&str, &str)],
) -> OAuthFormRequest {
    OAuthFormRequest {
        url: format!("{}{}", env.oauth_host, path),
        body: url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params.iter().copied())
            .finish(),
    }
}

async fn post_form(
    client: &reqwest::Client,
    env: &KimiAuthEnv,
    path: &str,
    params: &[(&str, &str)],
) -> Result<OAuthResponse, String> {
    let spec = build_oauth_form_request(env, path, params);
    let resp = apply_default_headers(client.post(&spec.url))
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(spec.body)
        .send()
        .await
        .map_err(|e| format!("OAuth request failed: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    Ok(OAuthResponse { status, body, json })
}

fn oauth_error(resp: &OAuthResponse, context: &str) -> String {
    let detail = resp.json["error_description"]
        .as_str()
        .or_else(|| resp.json["detail"].as_str())
        .or_else(|| resp.json["message"].as_str())
        .or_else(|| resp.json["error"].as_str())
        .unwrap_or(resp.body.as_str());
    format!("{context} (HTTP {}): {detail}", resp.status)
}

#[derive(Debug, PartialEq, Eq)]
enum DevicePollDecision {
    Authorized,
    Continue { interval: u64 },
    Expired,
    Denied,
}

fn device_poll_decision(resp: &OAuthResponse, interval: u64) -> Result<DevicePollDecision, String> {
    if resp.status == 200 {
        return Ok(DevicePollDecision::Authorized);
    }

    match resp.json["error"].as_str().unwrap_or_default() {
        "authorization_pending" => Ok(DevicePollDecision::Continue { interval }),
        "slow_down" => Ok(DevicePollDecision::Continue {
            interval: interval.saturating_add(5),
        }),
        "expired_token" => Ok(DevicePollDecision::Expired),
        "access_denied" => Ok(DevicePollDecision::Denied),
        _ => Err(oauth_error(resp, "Device token polling failed")),
    }
}

pub(crate) async fn login(
    client: &reqwest::Client,
    callbacks: &LoginCallbacks<'_>,
) -> Result<KimiCodeTokens, String> {
    login_with_env(client, callbacks, &KimiAuthEnv::production()).await
}

async fn login_with_env(
    client: &reqwest::Client,
    callbacks: &LoginCallbacks<'_>,
    env: &KimiAuthEnv,
) -> Result<KimiCodeTokens, String> {
    let auth = post_form(
        client,
        env,
        "/api/oauth/device_authorization",
        &[("client_id", CLIENT_ID)],
    )
    .await?;
    if auth.status != 200 {
        return Err(oauth_error(&auth, "Device authorization failed"));
    }

    let user_code = auth.json["user_code"].as_str().unwrap_or_default();
    let device_code = auth.json["device_code"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("device authorization response missing device_code")?;
    let callback_url = auth.json["verification_uri_complete"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("device authorization response missing verification_uri_complete")?;
    let mut interval = auth.json["interval"].as_u64().unwrap_or(5);
    let expires_at =
        (env.now)().saturating_add(auth.json["expires_in"].as_u64().unwrap_or(15 * 60));
    (callbacks.on_prompt)(callback_url, user_code);

    while (env.now)() < expires_at {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let resp = post_form(
            client,
            env,
            "/api/oauth/token",
            &[
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )
        .await?;

        match device_poll_decision(&resp, interval)? {
            DevicePollDecision::Authorized => {
                let tokens = token_from_response_at(&resp.json, (env.now)())?;
                tokens.save_to(&env.token_store)?;
                let models =
                    fetch_models_with_token(client, &env.api_base, &tokens.access_token).await;
                if let Ok(models) = models {
                    if !models.is_empty() {
                        save_models_cache_to(&env.models_cache_path, &models);
                    }
                }
                return Ok(tokens);
            }
            DevicePollDecision::Continue {
                interval: next_interval,
            } => {
                interval = next_interval;
            }
            DevicePollDecision::Expired => {
                return Err("Kimi Code authorization expired; run login again".to_string());
            }
            DevicePollDecision::Denied => {
                return Err("Kimi Code authorization was denied".to_string())
            }
        }
    }

    Err("Kimi Code authorization expired; run login again".to_string())
}

pub async fn access_token(client: &reqwest::Client) -> Result<String, String> {
    access_token_with_env(client, &KimiAuthEnv::production()).await
}

async fn access_token_with_env(
    client: &reqwest::Client,
    env: &KimiAuthEnv,
) -> Result<String, String> {
    let Some(tokens) = KimiCodeTokens::load_from(&env.token_store) else {
        return Err("not logged in to Kimi Code".to_string());
    };
    if tokens.expires_at > (env.now)().saturating_add(60) {
        return Ok(tokens.access_token);
    }

    let resp = post_form(
        client,
        env,
        "/api/oauth/token",
        &[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", &tokens.refresh_token),
        ],
    )
    .await?;
    if resp.status != 200 {
        return Err(oauth_error(&resp, "Token refresh failed"));
    }
    let fresh = token_from_response_at(&resp.json, (env.now)())?;
    let token = fresh.access_token.clone();
    fresh.save_to(&env.token_store)?;
    Ok(token)
}

pub async fn authenticated_request(
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    client: &reqwest::Client,
) -> Result<crate::auth::AuthenticatedResponse, String> {
    authenticated_request_with_env(method, path, body, client, &KimiAuthEnv::production()).await
}

async fn authenticated_request_with_env(
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    client: &reqwest::Client,
    env: &KimiAuthEnv,
) -> Result<crate::auth::AuthenticatedResponse, String> {
    let token = access_token_with_env(client, env).await?;
    let url = format!("{}{}", env.api_base, crate::auth::authenticated_path(path)?);
    crate::auth::send_authenticated_request(client, method, url, body, |req| {
        apply_default_headers(req).bearer_auth(token)
    })
    .await
}

fn models_cache_path() -> PathBuf {
    cache_dir().join("kimi_code_models.json")
}

pub fn load_cached_model_info() -> Vec<KimiCodeModelInfo> {
    load_cached_model_info_from(&models_cache_path())
}

fn load_cached_model_info_from(path: &Path) -> Vec<KimiCodeModelInfo> {
    let Ok(data) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    if let Ok(cache) = serde_json::from_str::<KimiCodeModelsCache>(&data) {
        if cache.version == MODELS_CACHE_VERSION {
            return cache.models;
        }
    }
    if let Ok(models) = serde_json::from_str::<Vec<KimiCodeModelInfo>>(&data) {
        return models;
    }
    serde_json::from_str::<Vec<String>>(&data)
        .unwrap_or_default()
        .into_iter()
        .map(|id| KimiCodeModelInfo {
            id,
            context_length: None,
            supports_reasoning: None,
            supports_image_in: None,
            supports_video_in: None,
            supports_tool_use: None,
            display_name: None,
        })
        .collect()
}

pub fn load_cached_models() -> Vec<String> {
    load_cached_model_info()
        .into_iter()
        .map(|model| model.id)
        .collect()
}

fn save_models_cache_to(cache_path: &Path, models: &[KimiCodeModelInfo]) {
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cache = KimiCodeModelsCache::new(models.to_vec());
    let _ = std::fs::write(
        cache_path,
        serde_json::to_string_pretty(&cache).unwrap_or_default(),
    );
}

fn parse_models_response(json: &serde_json::Value) -> Vec<KimiCodeModelInfo> {
    json["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(KimiCodeModelInfo::from_json)
        .collect()
}

async fn fetch_models_with_token(
    client: &reqwest::Client,
    api_base: &str,
    token: &str,
) -> Result<Vec<KimiCodeModelInfo>, String> {
    let resp = apply_default_headers(client.get(format!("{api_base}/models")))
        .bearer_auth(token)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("Kimi Code models request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Kimi Code models endpoint rejected credentials (HTTP {status}): {body}"
        ));
    }
    let json = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Kimi Code models response was invalid JSON: {e}"))?;
    Ok(parse_models_response(&json))
}

pub async fn fetch_model_info(client: &reqwest::Client) -> Result<Vec<KimiCodeModelInfo>, String> {
    fetch_model_info_with_env(client, &KimiAuthEnv::production()).await
}

async fn fetch_model_info_with_env(
    client: &reqwest::Client,
    env: &KimiAuthEnv,
) -> Result<Vec<KimiCodeModelInfo>, String> {
    let token = access_token_with_env(client, env).await?;
    let models = fetch_models_with_token(client, &env.api_base, &token).await?;
    if !models.is_empty() {
        save_models_cache_to(&env.models_cache_path, &models);
    }
    Ok(models)
}

pub async fn fetch_models(client: &reqwest::Client) -> Vec<String> {
    match fetch_model_info(client).await {
        Ok(models) => models.into_iter().map(|model| model.id).collect(),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn cached_context_window(model: &str) -> Option<u32> {
    load_cached_model_info()
        .into_iter()
        .find(|info| info.matches_name(model))
        .and_then(|info| info.context_length)
}

pub(crate) fn cached_supports_reasoning(model: &str) -> Option<bool> {
    load_cached_model_info()
        .into_iter()
        .find(|info| info.matches_name(model))
        .and_then(|info| info.supports_reasoning)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_http::spawn_json_response;

    #[test]
    fn parse_models_response_keeps_kimi_metadata() {
        let json = serde_json::json!({"data": [{
            "id": "moonshot-v1",
            "context_length": 131072,
            "supports_reasoning": false,
            "supports_image_in": true,
            "supports_video_in": false,
            "supports_tool_use": true,
            "display_name": "Moonshot V1"
        }]});

        let models = parse_models_response(&json);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "moonshot-v1");
        assert_eq!(models[0].context_length, Some(131_072));
        assert_eq!(models[0].supports_reasoning, Some(false));
        assert_eq!(models[0].supports_image_in, Some(true));
        assert_eq!(models[0].supports_video_in, Some(false));
        assert_eq!(models[0].supports_tool_use, Some(true));
        assert!(models[0].matches_name("Moonshot V1"));
    }

    #[test]
    fn json_u32_accepts_positive_numbers_and_numeric_strings_only() {
        assert_eq!(json_u32(&serde_json::json!(42)), Some(42));
        assert_eq!(json_u32(&serde_json::json!("42")), Some(42));
        assert_eq!(json_u32(&serde_json::json!(0)), None);
        assert_eq!(json_u32(&serde_json::json!("0")), None);
        assert_eq!(json_u32(&serde_json::json!("nope")), None);
        assert_eq!(json_u32(&serde_json::json!(u64::MAX)), None);
    }

    #[test]
    fn token_from_response_requires_core_fields() {
        let missing_access = token_from_response_at(
            &serde_json::json!({
                "refresh_token": "refresh",
                "expires_in": 60
            }),
            100,
        )
        .unwrap_err();
        assert!(missing_access.contains("access_token"));

        let missing_refresh = token_from_response_at(
            &serde_json::json!({
                "access_token": "access",
                "expires_in": 60
            }),
            100,
        )
        .unwrap_err();
        assert!(missing_refresh.contains("refresh_token"));

        let missing_expiry = token_from_response_at(
            &serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh"
            }),
            100,
        )
        .unwrap_err();
        assert!(missing_expiry.contains("expires_in"));
    }

    #[test]
    fn token_from_response_uses_supplied_now_and_defaults_optional_fields() {
        let tokens = token_from_response_at(
            &serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "expires_in": 60
            }),
            1_000,
        )
        .unwrap();

        assert_eq!(tokens.access_token, "access");
        assert_eq!(tokens.refresh_token, "refresh");
        assert_eq!(tokens.scope, "");
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.expires_at, 1_060);
    }

    #[test]
    fn device_poll_decision_classifies_pending_slowdown_and_terminal_errors() {
        let pending = OAuthResponse {
            status: 400,
            body: String::new(),
            json: serde_json::json!({"error": "authorization_pending"}),
        };
        assert_eq!(
            device_poll_decision(&pending, 5).unwrap(),
            DevicePollDecision::Continue { interval: 5 }
        );

        let slow_down = OAuthResponse {
            status: 400,
            body: String::new(),
            json: serde_json::json!({"error": "slow_down"}),
        };
        assert_eq!(
            device_poll_decision(&slow_down, 5).unwrap(),
            DevicePollDecision::Continue { interval: 10 }
        );

        let expired = OAuthResponse {
            status: 400,
            body: String::new(),
            json: serde_json::json!({"error": "expired_token"}),
        };
        assert_eq!(
            device_poll_decision(&expired, 5).unwrap(),
            DevicePollDecision::Expired
        );
    }

    #[test]
    fn oauth_error_prefers_structured_fields_before_raw_body() {
        let resp = OAuthResponse {
            status: 400,
            body: "raw body".into(),
            json: serde_json::json!({"message": "structured message"}),
        };
        assert_eq!(
            oauth_error(&resp, "Refresh failed"),
            "Refresh failed (HTTP 400): structured message"
        );

        let raw = OAuthResponse {
            status: 500,
            body: "server exploded".into(),
            json: serde_json::Value::Null,
        };
        assert_eq!(
            oauth_error(&raw, "Refresh failed"),
            "Refresh failed (HTTP 500): server exploded"
        );
    }

    fn fixed_now() -> u64 {
        1_000
    }

    fn test_token_store(path: PathBuf) -> CredStore {
        CredStore::file_only(path)
    }

    fn test_env(oauth_host: String, token_path: PathBuf, cache_path: PathBuf) -> KimiAuthEnv {
        KimiAuthEnv {
            oauth_host,
            api_base: "http://127.0.0.1:9".to_string(),
            token_store: test_token_store(token_path),
            models_cache_path: cache_path,
            now: fixed_now,
        }
    }

    #[test]
    fn build_oauth_form_request_uses_env_host_and_urlencoded_body() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(
            "https://auth.example".to_string(),
            tmp.path().join("tokens.json"),
            tmp.path().join("models.json"),
        );

        let req = build_oauth_form_request(
            &env,
            "/api/oauth/token",
            &[
                ("client_id", "client-1"),
                ("scope", "a b"),
                ("redirect", "x/y"),
            ],
        );

        assert_eq!(req.url, "https://auth.example/api/oauth/token");
        assert_eq!(req.body, "client_id=client-1&scope=a+b&redirect=x%2Fy");
    }

    #[tokio::test]
    async fn access_token_returns_cached_token_without_refresh_when_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(
            "http://127.0.0.1:9".to_string(),
            tmp.path().join("tokens.json"),
            tmp.path().join("models.json"),
        );
        KimiCodeTokens {
            access_token: "cached-access".to_string(),
            refresh_token: "cached-refresh".to_string(),
            expires_at: fixed_now() + 3_600,
            scope: String::new(),
            token_type: default_bearer(),
        }
        .save_to(&env.token_store)
        .unwrap();

        let token = access_token_with_env(&reqwest::Client::new(), &env)
            .await
            .unwrap();

        assert_eq!(token, "cached-access");
    }

    #[tokio::test]
    async fn access_token_refreshes_expired_cached_token_and_persists_fresh_token() {
        let tmp = tempfile::tempdir().unwrap();
        let (host, task) = spawn_json_response(
            r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":120}"#,
        )
        .await;
        let env = test_env(
            host,
            tmp.path().join("tokens.json"),
            tmp.path().join("models.json"),
        );
        KimiCodeTokens {
            access_token: "expired-access".to_string(),
            refresh_token: "expired-refresh".to_string(),
            expires_at: fixed_now().saturating_sub(1),
            scope: String::new(),
            token_type: default_bearer(),
        }
        .save_to(&env.token_store)
        .unwrap();

        let token = access_token_with_env(&reqwest::Client::new(), &env)
            .await
            .unwrap();

        assert_eq!(token, "fresh-access");
        let request = task.await.unwrap();
        assert!(request.starts_with("POST /api/oauth/token HTTP/1.1"));
        assert!(request.contains("grant_type=refresh_token"), "{request}");
        assert!(
            request.contains("refresh_token=expired-refresh"),
            "{request}"
        );
        let saved = std::fs::read_to_string(&env.token_store.file_path).unwrap();
        assert!(saved.contains("fresh-access"), "{saved}");
        assert!(saved.contains("fresh-refresh"), "{saved}");
    }

    #[test]
    fn load_cached_model_info_from_reads_current_and_legacy_shapes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("models.json");

        std::fs::write(
            &path,
            serde_json::to_string(&KimiCodeModelsCache::new(vec![KimiCodeModelInfo {
                id: "kimi-current".to_string(),
                context_length: Some(128_000),
                supports_reasoning: Some(true),
                supports_image_in: None,
                supports_video_in: None,
                supports_tool_use: None,
                display_name: Some("Kimi Current".to_string()),
            }]))
            .unwrap(),
        )
        .unwrap();
        let current = load_cached_model_info_from(&path);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, "kimi-current");
        assert_eq!(current[0].context_length, Some(128_000));

        std::fs::write(&path, r#"["kimi-a", "kimi-b"]"#).unwrap();
        let legacy_ids: Vec<_> = load_cached_model_info_from(&path)
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(legacy_ids, vec!["kimi-a", "kimi-b"]);

        std::fs::write(
            &path,
            serde_json::json!({
                "version": 999,
                "models": [{"id": "ignored", "context_length": 1}]
            })
            .to_string(),
        )
        .unwrap();
        assert!(load_cached_model_info_from(&path).is_empty());

        std::fs::write(&path, "not json").unwrap();
        assert!(load_cached_model_info_from(&path).is_empty());
    }
}
