//! GitHub Copilot authentication and API access.
//! GitHub OAuth device-code → long-lived GitHub token → short-lived Copilot token (~30min TTL).
//! The Copilot token carries a `proxy-ep=` claim from which the API base URL is derived.

use crate::log;
use crate::paths::state_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use super::auth_storage::CredStore;
use super::unix_now;

// VS Code Copilot Chat OAuth client ID, stored base64-encoded to avoid casual grep matches.
const CLIENT_ID_B64: &str = "SXYxLmI1MDdhMDhjODdlY2ZlOTg=";

const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
pub const DEFAULT_COPILOT_API_BASE: &str = "https://api.individual.githubcopilot.com";

const EDITOR_VERSION: &str = "vscode/1.107.0";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

const COPILOT_TOKENS_ENV: &str = "SMELT_COPILOT_TOKENS";

fn cred_store() -> CredStore {
    CredStore {
        keyring_service: "smelt-copilot-auth",
        keyring_user: "default",
        file_path: state_dir().join("copilot_auth.json"),
        env_var: COPILOT_TOKENS_ENV,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CopilotTokens {
    pub(crate) refresh_token: String,
    pub(crate) access_token: String,
    pub(crate) expires_at: u64,
    pub(crate) api_base: String,
    #[serde(default)]
    pub(crate) last_refresh: u64,
}

impl CopilotTokens {
    pub(crate) fn needs_refresh(&self) -> bool {
        let now = unix_now();
        now + 60 >= self.expires_at
    }

    pub(crate) fn save(&self) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        cred_store().save(&json)
    }

    pub(crate) fn load() -> Option<Self> {
        let json = cred_store().load()?;
        serde_json::from_str(&json).ok()
    }

    pub(crate) fn delete() {
        cred_store().delete();
    }
}

fn client_id() -> String {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(CLIENT_ID_B64)
        .expect("hard-coded client ID must decode");
    String::from_utf8(bytes).expect("client ID must be UTF-8")
}

/// Extract the Copilot API base URL from the `proxy-ep=` claim in a Copilot token.
fn base_url_from_token(token: &str) -> Option<String> {
    let proxy_host = token
        .split(';')
        .find_map(|kv| kv.strip_prefix("proxy-ep="))?;
    let api_host = proxy_host.strip_prefix("proxy.").unwrap_or(proxy_host);
    Some(format!("https://api.{}", api_host))
}

pub(crate) fn base_headers() -> [(&'static str, &'static str); 4] {
    [
        ("User-Agent", COPILOT_USER_AGENT),
        ("Editor-Version", EDITOR_VERSION),
        ("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION),
        ("Copilot-Integration-Id", COPILOT_INTEGRATION_ID),
    ]
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    expires_in: u64,
}

pub(crate) struct LoginCallbacks<'a> {
    pub(crate) on_prompt: &'a (dyn Fn(&str, &str) + Send + Sync),
    pub(crate) on_progress: &'a (dyn Fn(&str) + Send + Sync),
}

pub(crate) async fn device_code_login(
    client: &reqwest::Client,
    callbacks: &LoginCallbacks<'_>,
) -> Result<CopilotTokens, String> {
    let cid = client_id();

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &cid)
        .append_pair("scope", "read:user")
        .finish();
    let device_resp = client
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", COPILOT_USER_AGENT)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;

    if !device_resp.status().is_success() {
        let status = device_resp.status();
        let body = device_resp.text().await.unwrap_or_default();
        return Err(format!("device code error (HTTP {status}): {body}"));
    }

    let device: DeviceCodeResponse = device_resp
        .json()
        .await
        .map_err(|e| format!("bad device code response: {e}"))?;

    (callbacks.on_prompt)(&device.verification_uri, &device.user_code);
    open_browser(&device.verification_uri);

    let github_token = poll_for_github_token(
        client,
        &cid,
        &device.device_code,
        device.interval,
        device.expires_in,
    )
    .await?;

    (callbacks.on_progress)("Fetching Copilot token…");
    let (access, expires_at, api_base) = fetch_copilot_token(client, &github_token).await?;

    let tokens = CopilotTokens {
        refresh_token: github_token,
        access_token: access,
        expires_at,
        api_base,
        last_refresh: unix_now(),
    };
    tokens
        .save()
        .map_err(|e| format!("failed to save tokens: {e}"))?;

    (callbacks.on_progress)("Fetching Copilot models…");
    let models = match fetch_available_models(client, &tokens.access_token, &tokens.api_base).await
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
        save_models_cache(&models);
    }

    Ok(tokens)
}

async fn poll_for_github_token(
    client: &reqwest::Client,
    client_id: &str,
    device_code: &str,
    initial_interval: u64,
    expires_in: u64,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires_in.max(1));
    let initial_multiplier = 1.2_f64;
    let slow_down_multiplier = 1.4_f64;
    let mut interval_ms: u64 = initial_interval.max(1) * 1000;
    let mut multiplier = initial_multiplier;
    let mut slow_down_count: u32 = 0;

    loop {
        if tokio::time::Instant::now() >= deadline {
            if slow_down_count > 0 {
                return Err("Device flow timed out after repeated slow_down responses. \
                     This is often caused by clock drift in WSL or VM environments."
                    .into());
            }
            return Err("Device flow timed out".into());
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let wait_ms = ((interval_ms as f64) * multiplier).ceil() as u64;
        let wait = Duration::from_millis(wait_ms).min(remaining);
        tokio::time::sleep(wait).await;

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", client_id)
            .append_pair("device_code", device_code)
            .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:device_code")
            .finish();
        let resp = client
            .post(GITHUB_ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", COPILOT_USER_AGENT)
            .body(body)
            .send()
            .await
            .map_err(|e| format!("token poll failed: {e}"))?;

        let data: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Err(format!("bad token poll response: {e}")),
        };

        if let Some(token) = data.get("access_token").and_then(|v| v.as_str()) {
            return Ok(token.to_string());
        }

        let error = data.get("error").and_then(|v| v.as_str()).unwrap_or("");
        match error {
            "authorization_pending" => continue,
            "slow_down" => {
                slow_down_count += 1;
                if let Some(n) = data.get("interval").and_then(|v| v.as_u64()) {
                    interval_ms = n * 1000;
                } else {
                    interval_ms = interval_ms.saturating_add(5000).max(1000);
                }
                multiplier = slow_down_multiplier;
                continue;
            }
            other => {
                let desc = data
                    .get("error_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let suffix = if desc.is_empty() {
                    String::new()
                } else {
                    format!(": {desc}")
                };
                return Err(format!("Device flow failed: {other}{suffix}"));
            }
        }
    }
}

async fn fetch_copilot_token(
    client: &reqwest::Client,
    github_token: &str,
) -> Result<(String, u64, String), String> {
    let mut req = client
        .get(COPILOT_TOKEN_URL)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {github_token}"));
    for (k, v) in base_headers() {
        req = req.header(k, v);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("copilot token request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("copilot token error (HTTP {status}): {body}"));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("bad copilot token response: {e}"))?;

    let token = data
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or("missing 'token' in copilot response")?
        .to_string();
    let expires_at = data
        .get("expires_at")
        .and_then(|v| v.as_u64())
        .ok_or("missing 'expires_at' in copilot response")?;

    let api_base =
        base_url_from_token(&token).unwrap_or_else(|| DEFAULT_COPILOT_API_BASE.to_string());

    Ok((token, expires_at, api_base))
}

pub(crate) async fn refresh_tokens(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<CopilotTokens, String> {
    let (access, expires_at, api_base) = fetch_copilot_token(client, refresh_token).await?;
    let tokens = CopilotTokens {
        refresh_token: refresh_token.to_string(),
        access_token: access,
        expires_at,
        api_base,
        last_refresh: unix_now(),
    };
    tokens
        .save()
        .map_err(|e| format!("failed to save tokens: {e}"))?;
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
    let tokens =
        CopilotTokens::load().ok_or("not logged in to GitHub Copilot; run `smelt auth` first")?;
    if !tokens.needs_refresh() {
        return Ok(tokens);
    }
    refresh_tokens(client, &tokens.refresh_token).await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CopilotModel {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) vendor: Option<String>,
    pub(crate) family: Option<String>,
    pub(crate) capability_type: Option<String>,
    pub(crate) context_window: Option<u32>,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) policy_state: Option<String>,
    #[serde(default)]
    pub(crate) supported_reasoning_efforts: Vec<String>,
}

impl From<CopilotModel> for protocol::ModelMetadata {
    fn from(model: CopilotModel) -> Self {
        Self {
            id: model.id,
            display_name: Some(model.name),
            context_window: model.context_window,
            supports_reasoning: (!model.supported_reasoning_efforts.is_empty()).then_some(true),
        }
    }
}

async fn fetch_available_models(
    client: &reqwest::Client,
    access_token: &str,
    api_base: &str,
) -> Result<Vec<CopilotModel>, String> {
    let url = format!("{}/models", api_base.trim_end_matches('/'));
    let mut req = client
        .get(&url)
        .header("Accept", "application/json")
        .bearer_auth(access_token);
    for (k, v) in base_headers() {
        req = req.header(k, v);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("models request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("models error (HTTP {status}): {body}"));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("bad models response: {e}"))?;

    parse_models_response(&data).ok_or_else(|| "missing 'data' array in models response".into())
}

/// Filter, sort, and dedup chat-capable models from the `/models` endpoint payload. Pure.
fn infer_family(id: &str, name: &str, vendor: Option<&str>) -> Option<String> {
    let id_lower = id.to_ascii_lowercase();
    let name_lower = name.to_ascii_lowercase();
    if id_lower.starts_with("claude-") || name_lower.contains("claude") {
        return Some("claude".to_string());
    }
    if id_lower.starts_with("gpt-") || name_lower.starts_with("gpt-") {
        return Some("gpt".to_string());
    }
    if id_lower.starts_with("gemini-") || name_lower.contains("gemini") {
        return Some("gemini".to_string());
    }
    if id_lower.starts_with("oswe-") || name_lower.contains("raptor") {
        return Some("oswe".to_string());
    }
    vendor.map(|v| v.to_ascii_lowercase())
}

fn parse_models_response(data: &serde_json::Value) -> Option<Vec<CopilotModel>> {
    let entries = data.get("data").and_then(|v| v.as_array())?;
    let mut out: Vec<CopilotModel> = Vec::with_capacity(entries.len());
    for m in entries {
        let capability_type = m
            .pointer("/capabilities/type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !capability_type.is_empty() && capability_type != "chat" {
            continue;
        }
        let model_picker_enabled = m
            .get("model_picker_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !model_picker_enabled {
            continue;
        }
        let policy_state = m
            .pointer("/policy/state")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if policy_state.as_deref() == Some("disabled") {
            continue;
        }
        let id = match m.get("id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let name = m
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();
        let vendor = m
            .get("vendor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let family = m
            .get("family")
            .and_then(|v| v.as_str())
            .or_else(|| m.get("model_family").and_then(|v| v.as_str()))
            .or_else(|| m.pointer("/capabilities/family").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .or_else(|| infer_family(&id, &name, vendor.as_deref()));
        let capability_type = if capability_type.is_empty() {
            None
        } else {
            Some(capability_type.to_string())
        };
        let supported_reasoning_efforts = m
            .get("supportedReasoningEfforts")
            .or_else(|| m.get("supported_reasoning_efforts"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let context_window = m
            .pointer("/capabilities/limits/max_context_window_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                m.pointer("/capabilities/limits/max_prompt_tokens")
                    .and_then(|v| v.as_u64())
            })
            .map(|v| v as u32);
        let max_output_tokens = m
            .pointer("/capabilities/limits/max_output_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        out.push(CopilotModel {
            id,
            name,
            vendor,
            family,
            capability_type,
            context_window,
            max_output_tokens,
            policy_state,
            supported_reasoning_efforts,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    Some(out)
}

fn cache_path() -> PathBuf {
    crate::paths::cache_dir().join("copilot_models.json")
}

pub(crate) fn load_cached_models() -> Vec<CopilotModel> {
    let Ok(data) = std::fs::read_to_string(cache_path()) else {
        return Vec::new();
    };
    let models: Vec<CopilotModel> = serde_json::from_str(&data).unwrap_or_default();
    if !models.is_empty() && models.iter().all(|m| m.policy_state.is_none()) {
        return Vec::new();
    }
    models
        .into_iter()
        .filter(|m| m.policy_state.as_deref() != Some("disabled"))
        .collect()
}

fn save_models_cache(models: &[CopilotModel]) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, serde_json::to_string(models).unwrap_or_default());
}

pub(crate) async fn refresh_models_cache(client: &reqwest::Client) -> Vec<CopilotModel> {
    let Ok(tokens) = ensure_access_token_full(client).await else {
        return Vec::new();
    };
    let models = match fetch_available_models(client, &tokens.access_token, &tokens.api_base).await
    {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    save_models_cache(&models);
    models
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

fn open_browser(url: &str) {
    use std::process::Stdio;

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            api_base: DEFAULT_COPILOT_API_BASE.into(),
            last_refresh: 0,
        }
    }

    #[test]
    fn needs_refresh_true_when_within_60s_of_expiry() {
        let t = tokens(unix_now() + 30);
        assert!(t.needs_refresh());
    }

    #[test]
    fn needs_refresh_true_when_past_expiry() {
        let t = tokens(unix_now().saturating_sub(1));
        assert!(t.needs_refresh());
    }

    #[test]
    fn needs_refresh_false_when_expiry_far_future() {
        let t = tokens(unix_now() + 3600);
        assert!(!t.needs_refresh());
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
