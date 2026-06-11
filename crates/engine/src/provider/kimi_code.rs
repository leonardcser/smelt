use crate::paths::{cache_dir, state_dir};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use rand::RngExt;

use super::auth_storage::CredStore;
use super::unix_now;

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

pub struct LoginCallbacks<'a> {
    pub on_prompt: &'a (dyn Fn(&str, &str) + Send + Sync),
    pub on_progress: &'a (dyn Fn(&str) + Send + Sync),
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
    CredStore {
        keyring_service: "smelt-kimi-code-auth",
        keyring_user: "default",
        file_path: token_path(),
        env_var: TOKENS_ENV,
    }
}

fn token_path() -> PathBuf {
    state_dir().join("kimi_code_auth.json")
}

impl KimiCodeTokens {
    fn save(&self) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        cred_store().save(&json)
    }

    fn load() -> Option<Self> {
        let json = cred_store().load()?;
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

fn token_from_response(value: &serde_json::Value) -> Result<KimiCodeTokens, String> {
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
        expires_at: unix_now().saturating_add(expires_in),
        scope: value["scope"].as_str().unwrap_or_default().to_string(),
        token_type: value["token_type"].as_str().unwrap_or("Bearer").to_string(),
    })
}

struct OAuthResponse {
    status: u16,
    body: String,
    json: serde_json::Value,
}

async fn post_form(
    client: &reqwest::Client,
    path: &str,
    params: &[(&str, &str)],
) -> Result<OAuthResponse, String> {
    let url = format!("{}{}", oauth_host(), path);
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().copied())
        .finish();
    let resp = apply_default_headers(client.post(&url))
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
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

pub(crate) async fn login(
    client: &reqwest::Client,
    callbacks: &LoginCallbacks<'_>,
) -> Result<KimiCodeTokens, String> {
    let auth = post_form(
        client,
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
    let expires_at = unix_now().saturating_add(auth.json["expires_in"].as_u64().unwrap_or(15 * 60));
    (callbacks.on_prompt)(callback_url, user_code);

    while unix_now() < expires_at {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let resp = post_form(
            client,
            "/api/oauth/token",
            &[
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )
        .await?;

        if resp.status == 200 {
            let tokens = token_from_response(&resp.json)?;
            tokens.save()?;
            let models = fetch_models_with_token(client, &tokens.access_token).await;
            if let Ok(models) = models {
                if !models.is_empty() {
                    save_models_cache(&models);
                }
            }
            return Ok(tokens);
        }

        match resp.json["error"].as_str().unwrap_or_default() {
            "authorization_pending" => {
                (callbacks.on_progress)("Waiting for browser authorization...")
            }
            "slow_down" => {
                interval = interval.saturating_add(5);
                (callbacks.on_progress)("Waiting for browser authorization...")
            }
            "expired_token" => {
                return Err("Kimi Code authorization expired; run login again".to_string())
            }
            "access_denied" => return Err("Kimi Code authorization was denied".to_string()),
            _ => return Err(oauth_error(&resp, "Device token polling failed")),
        }
    }

    Err("Kimi Code authorization expired; run login again".to_string())
}

pub async fn access_token(client: &reqwest::Client) -> Result<String, String> {
    let Some(tokens) = KimiCodeTokens::load() else {
        return Err("not logged in to Kimi Code".to_string());
    };
    if tokens.expires_at > unix_now().saturating_add(60) {
        return Ok(tokens.access_token);
    }

    let resp = post_form(
        client,
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
    let fresh = token_from_response(&resp.json)?;
    let token = fresh.access_token.clone();
    fresh.save()?;
    Ok(token)
}

pub async fn authenticated_request(
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    client: &reqwest::Client,
) -> Result<crate::auth::AuthenticatedResponse, String> {
    let token = access_token(client).await?;
    let url = format!("{}{}", api_base(), crate::auth::authenticated_path(path)?);
    crate::auth::send_authenticated_request(client, method, url, body, |req| {
        apply_default_headers(req).bearer_auth(token)
    })
    .await
}

fn models_cache_path() -> PathBuf {
    cache_dir().join("kimi_code_models.json")
}

pub fn load_cached_model_info() -> Vec<KimiCodeModelInfo> {
    let Ok(data) = std::fs::read_to_string(models_cache_path()) else {
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

fn save_models_cache(models: &[KimiCodeModelInfo]) {
    let cache_path = models_cache_path();
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cache = KimiCodeModelsCache::new(models.to_vec());
    let _ = std::fs::write(
        &cache_path,
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
    token: &str,
) -> Result<Vec<KimiCodeModelInfo>, String> {
    let resp = apply_default_headers(client.get(format!("{}/models", api_base())))
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
    let token = access_token(client).await?;
    let models = fetch_models_with_token(client, &token).await?;
    if !models.is_empty() {
        save_models_cache(&models);
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
}
