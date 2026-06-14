use crate::log;
use crate::paths::state_dir;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::time::Duration;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
pub const CHATGPT_BACKEND_API_BASE: &str = "https://chatgpt.com/backend-api";
pub const CODEX_API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const OAUTH_PORT: u16 = 1455;
const REFRESH_INTERVAL_SECS: u64 = 8 * 24 * 3600; // 8 days, matching codex

pub(crate) const CODEX_TOKENS_ENV: &str = "SMELT_CODEX_TOKENS";

use super::auth_storage::CredStore;
use super::unix_now;

fn cred_store() -> CredStore {
    CredStore::production(
        "smelt-codex-auth",
        "default",
        state_dir().join("codex_auth.json"),
        CODEX_TOKENS_ENV,
    )
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
            token_store: cred_store(),
            now: unix_now,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CodexTokens {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_at: u64,
    pub(crate) account_id: Option<String>,
    #[serde(default)]
    pub(crate) last_refresh: u64,
}

impl CodexTokens {
    fn needs_refresh_at(&self, now: u64) -> bool {
        now + 60 >= self.expires_at
            || (self.last_refresh > 0 && now - self.last_refresh >= REFRESH_INTERVAL_SECS)
    }

    fn save_to(&self, store: &CredStore) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        store.save(&json)
    }

    pub(crate) fn load() -> Option<Self> {
        Self::load_from(&cred_store())
    }

    fn load_from(store: &CredStore) -> Option<Self> {
        let json = store.load()?;
        serde_json::from_str(&json).ok()
    }

    pub(crate) fn apply_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = req.bearer_auth(&self.access_token);
        if let Some(account_id) = self.account_id.as_deref() {
            req.header("ChatGPT-Account-ID", account_id)
        } else {
            req
        }
    }

    pub(crate) fn delete() {
        cred_store().delete();
    }
}

fn extract_account_id(access_token: &str, id_token: Option<&str>) -> Option<String> {
    for token in id_token.into_iter().chain(std::iter::once(access_token)) {
        if let Some(claims) = parse_jwt_claims(token) {
            if let Some(id) = claims.chatgpt_account_id.or_else(|| {
                claims
                    .auth_ext
                    .as_ref()
                    .and_then(|a| a.chatgpt_account_id.clone())
            }) {
                return Some(id);
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct JwtClaims {
    chatgpt_account_id: Option<String>,
    #[serde(rename = "https://api.openai.com/auth")]
    auth_ext: Option<AuthExt>,
}

#[derive(Deserialize)]
struct AuthExt {
    chatgpt_account_id: Option<String>,
}

fn parse_jwt_claims(token: &str) -> Option<JwtClaims> {
    decode_jwt_payload(token)
}

fn decode_jwt_payload<T: serde::de::DeserializeOwned>(token: &str) -> Option<T> {
    use base64::Engine;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    serde_json::from_slice(&payload).ok()
}

#[derive(Deserialize)]
struct ExpClaim {
    exp: Option<u64>,
}

fn parse_jwt_expiration(token: &str) -> Option<u64> {
    decode_jwt_payload::<ExpClaim>(token).and_then(|c| c.exp)
}

struct PkceCodes {
    verifier: String,
    challenge: String,
}

fn generate_pkce() -> PkceCodes {
    use base64::Engine;
    use rand::RngExt;

    let mut bytes = [0u8; 64];
    rand::rng().fill(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);

    let hash = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);

    PkceCodes {
        verifier,
        challenge,
    }
}

fn generate_state() -> String {
    use base64::Engine;
    use rand::RngExt;

    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

struct TokenRefreshRequest<'a> {
    url: String,
    body: TokenRefreshBody<'a>,
}

#[derive(Serialize)]
struct TokenRefreshBody<'a> {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

struct TokenExchangeRequest {
    url: String,
    body: String,
}

fn token_url(env: &CodexAuthEnv) -> String {
    format!("{}/oauth/token", env.issuer)
}

fn build_refresh_request<'a>(
    env: &CodexAuthEnv,
    refresh_token: &'a str,
) -> TokenRefreshRequest<'a> {
    TokenRefreshRequest {
        url: token_url(env),
        body: TokenRefreshBody {
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
        },
    }
}

fn build_exchange_request(
    env: &CodexAuthEnv,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> TokenExchangeRequest {
    TokenExchangeRequest {
        url: token_url(env),
        body: url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("client_id", CLIENT_ID)
            .append_pair("code_verifier", code_verifier)
            .finish(),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

fn build_authorize_url(redirect_uri: &str, pkce: &PkceCodes, state: &str) -> String {
    let params = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "smelt")
        .finish();
    format!("{ISSUER}/oauth/authorize?{params}")
}

const HTML_SUCCESS: &str = r#"<!doctype html>
<html><head><title>smelt - Authorization Successful</title>
<style>body{font-family:system-ui,sans-serif;display:flex;justify-content:center;
align-items:center;height:100vh;margin:0;background:#131010;color:#f1ecec}
.c{text-align:center;padding:2rem}h1{margin-bottom:1rem}p{color:#b7b1b1}</style>
</head><body><div class="c"><h1>Authorization Successful</h1>
<p>You can close this window and return to smelt.</p></div>
<script>setTimeout(()=>window.close(),2000)</script></body></html>"#;

fn html_error(msg: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><title>Agent - Authorization Failed</title>
<style>body{{font-family:system-ui,sans-serif;display:flex;justify-content:center;
align-items:center;height:100vh;margin:0;background:#131010;color:#f1ecec}}
.c{{text-align:center;padding:2rem}}h1{{color:#fc533a;margin-bottom:1rem}}
p{{color:#b7b1b1}}.e{{color:#ff917b;font-family:monospace;margin-top:1rem;
padding:1rem;background:#3c140d;border-radius:.5rem}}</style>
</head><body><div class="c"><h1>Authorization Failed</h1>
<p>An error occurred during authorization.</p>
<div class="e">{msg}</div></div></body></html>"#
    )
}

/// Run the browser-based OAuth + PKCE flow: starts a local server, opens the browser,
/// waits for the callback, and exchanges the code for tokens.
pub(crate) async fn browser_login(client: &reqwest::Client) -> Result<CodexTokens, String> {
    let pkce = generate_pkce();
    let state = generate_state();
    let redirect_uri = format!("http://localhost:{OAUTH_PORT}/auth/callback");
    let auth_url = build_authorize_url(&redirect_uri, &pkce, &state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", OAUTH_PORT))
        .await
        .map_err(|e| format!("failed to bind port {OAUTH_PORT}: {e}"))?;

    open_browser(&auth_url);

    let (code, received_state) =
        tokio::time::timeout(Duration::from_secs(300), wait_for_callback(&listener))
            .await
            .map_err(|_| "login timed out (5 minutes)".to_string())?
            .map_err(|e| format!("callback error: {e}"))?;

    if received_state != state {
        return Err("state mismatch; potential CSRF attack".into());
    }

    exchange_code(client, &code, &pkce.verifier, &redirect_uri).await
}

async fn wait_for_callback(listener: &tokio::net::TcpListener) -> Result<(String, String), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|e| format!("accept failed: {e}"))?;

    let mut buf = vec![0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");

    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params: std::collections::HashMap<&str, &str> = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .collect();

    let error = params.get("error").copied();
    let error_desc = params.get("error_description").copied();

    if let Some(err) = error {
        let msg = error_desc.unwrap_or(err);
        let body = html_error(msg);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        return Err(msg.to_string());
    }

    let code = params
        .get("code")
        .copied()
        .ok_or("missing authorization code")?
        .to_string();
    let state = params.get("state").copied().unwrap_or("").to_string();

    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML_SUCCESS}",
        HTML_SUCCESS.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;

    Ok((code, state))
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
}

async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<CodexTokens, String> {
    exchange_code_with_env(
        client,
        code,
        code_verifier,
        redirect_uri,
        &CodexAuthEnv::production(),
    )
    .await
}

async fn exchange_code_with_env(
    client: &reqwest::Client,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    env: &CodexAuthEnv,
) -> Result<CodexTokens, String> {
    let spec = build_exchange_request(env, code, code_verifier, redirect_uri);

    let resp = client
        .post(spec.url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(spec.body)
        .send()
        .await
        .map_err(|e| format!("token exchange failed: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token exchange error: {body}"));
    }

    let tokens: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("bad token response: {e}"))?;

    save_token_response_to(tokens, None, &env.token_store, (env.now)())
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
    let previous = CodexTokens::load_from(&env.token_store);
    let spec = build_refresh_request(env, refresh_token);

    let resp = client
        .post(spec.url)
        .header("Content-Type", "application/json")
        .json(&spec.body)
        .send()
        .await
        .map_err(|e| format!("token refresh failed: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(classify_refresh_error(&body));
    }

    let tokens: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("bad refresh response: {e}"))?;

    let result = save_token_response_to(tokens, previous.as_ref(), &env.token_store, (env.now)())?;

    log::entry(
        log::Level::Debug,
        "codex_token_refreshed",
        &serde_json::json!({ "expires_at": result.expires_at }),
    );

    Ok(result)
}

fn classify_refresh_error(body: &str) -> String {
    if body.contains("refresh_token_expired") {
        "your refresh token has expired; run `smelt auth` to sign in again".into()
    } else if body.contains("refresh_token_reused") {
        "your refresh token was already used; run `smelt auth` to sign in again".into()
    } else if body.contains("refresh_token_invalidated") {
        "your refresh token was revoked; run `smelt auth` to sign in again".into()
    } else {
        format!("token refresh error: {body}")
    }
}

fn merge_token_response(
    tokens: TokenResponse,
    previous: Option<&CodexTokens>,
    now: u64,
) -> Result<CodexTokens, String> {
    let access_token = tokens
        .access_token
        .or_else(|| previous.map(|t| t.access_token.clone()))
        .ok_or("missing access_token in OAuth response")?;
    let refresh_token = tokens
        .refresh_token
        .or_else(|| previous.map(|t| t.refresh_token.clone()))
        .ok_or("missing refresh_token in OAuth response")?;

    let expires_at = parse_jwt_expiration(&access_token)
        .or_else(|| tokens.expires_in.map(|s| now + s))
        .unwrap_or(now + 3600);

    let account_id = extract_account_id(&access_token, tokens.id_token.as_deref())
        .or_else(|| previous.and_then(|t| t.account_id.clone()));

    Ok(CodexTokens {
        account_id,
        access_token,
        refresh_token,
        expires_at,
        last_refresh: now,
    })
}

fn save_token_response_to(
    tokens: TokenResponse,
    previous: Option<&CodexTokens>,
    store: &CredStore,
    now: u64,
) -> Result<CodexTokens, String> {
    let result = merge_token_response(tokens, previous, now)?;
    result
        .save_to(store)
        .map_err(|e| format!("failed to save tokens: {e}"))?;
    Ok(result)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CodexModel {
    pub(crate) slug: String,
    pub(crate) display_name: String,
    pub(crate) description: Option<String>,
    pub(crate) context_window: Option<u32>,
}

impl From<CodexModel> for protocol::ModelMetadata {
    fn from(model: CodexModel) -> Self {
        Self {
            id: model.slug,
            display_name: Some(model.display_name),
            context_window: model.context_window,
            supports_reasoning: None,
        }
    }
}

async fn fetch_models(client: &reqwest::Client) -> Result<Vec<CodexModel>, String> {
    let (access_token, account_id) = ensure_access_token(client).await?;

    let version = fetch_codex_version(client)
        .await
        .unwrap_or_else(|_| "0.1.0".into());

    let url = format!("https://chatgpt.com/backend-api/codex/models?client_version={version}");

    let mut req = client
        .get(&url)
        .header("Accept", "application/json")
        .bearer_auth(&access_token);
    if let Some(id) = &account_id {
        req = req.header("ChatGPT-Account-Id", id);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("models request failed: {e}"))?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("models endpoint error: {body}"));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("bad models response: {e}"))?;

    let models = data["models"]
        .as_array()
        .ok_or("missing 'models' key in response")?;

    let mut result: Vec<(i64, CodexModel)> = models
        .iter()
        .filter_map(|m| {
            let slug = m["slug"].as_str()?.to_string();
            let display_name = m["display_name"].as_str().unwrap_or(&slug).to_string();
            let description = m["description"].as_str().map(|s| s.to_string());
            let context_window = m["context_window"].as_u64().map(|v| v as u32);
            let visibility = m["visibility"].as_str().unwrap_or("none");
            let priority = m["priority"].as_i64().unwrap_or(999);

            if visibility != "list" {
                return None;
            }

            Some((
                priority,
                CodexModel {
                    slug,
                    display_name,
                    description,
                    context_window,
                },
            ))
        })
        .collect();

    result.sort_by_key(|(p, _)| *p);

    Ok(result.into_iter().map(|(_, m)| m).collect())
}

pub(crate) fn cached_context_window(model: &str) -> Option<u32> {
    load_cached_models()
        .into_iter()
        .find(|m| m.slug == model)
        .and_then(|m| m.context_window)
}

pub(crate) fn load_cached_models() -> Vec<CodexModel> {
    let cache_path = crate::paths::cache_dir().join("codex_models.json");
    let Ok(data) = std::fs::read_to_string(&cache_path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<CodexModel>>(&data).unwrap_or_default()
}

fn save_models_cache(models: &[CodexModel]) {
    let cache_path = crate::paths::cache_dir().join("codex_models.json");
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &cache_path,
        serde_json::to_string(models).unwrap_or_default(),
    );
}

pub(crate) async fn refresh_models_cache(client: &reqwest::Client) -> Vec<CodexModel> {
    let models = match fetch_models(client).await {
        Ok(m) => m,
        Err(_) => return load_cached_models(),
    };
    if models.is_empty() {
        // The server can return a 200 with an empty list during outages or
        // account transitions. Keep the last known-good cache rather than
        // wiping the model picker.
        return load_cached_models();
    }
    save_models_cache(&models);
    models
}

async fn fetch_codex_version(client: &reqwest::Client) -> Result<String, String> {
    let resp = client
        .get("https://api.github.com/repos/openai/codex/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "smelt")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("github request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err("github API error".into());
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("bad github response: {e}"))?;

    data["name"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or("missing release name".into())
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default, deserialize_with = "deserialize_interval")]
    interval: Option<u64>,
}

fn deserialize_interval<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    use serde::de;
    let v = serde_json::Value::deserialize(deserializer)?;
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => Ok(n.as_u64()),
        serde_json::Value::String(s) => {
            s.trim().parse::<u64>().map(Some).map_err(de::Error::custom)
        }
        _ => Err(de::Error::custom("expected number or string for interval")),
    }
}

#[derive(Deserialize)]
struct DeviceCodePollResponse {
    authorization_code: Option<String>,
    code_verifier: Option<String>,
}

/// Device-code flow for headless environments.
pub(crate) async fn device_code_login(client: &reqwest::Client) -> Result<CodexTokens, String> {
    let body = serde_json::json!({ "client_id": CLIENT_ID });

    let resp = client
        .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.as_u16() == 404 {
        return Err(
            "device code login is not enabled for this server; use browser login instead"
                .to_string(),
        );
    }
    if !status.is_success() {
        return Err(format!("device code error (HTTP {status}): {text}"));
    }

    let dc: DeviceCodeResponse = serde_json::from_str(&text)
        .map_err(|e| format!("bad device code response: {e}\nBody: {text}"))?;

    println!("\n  Open this URL in a browser:\n");
    println!("    {ISSUER}/codex/device\n");
    println!("  Then enter code: {}\n", dc.user_code);

    let interval = Duration::from_secs(dc.interval.unwrap_or(5));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15 * 60);

    loop {
        tokio::time::sleep(interval).await;

        if tokio::time::Instant::now() >= deadline {
            return Err("device code login timed out (15 minutes)".into());
        }

        let poll_body = serde_json::json!({
            "device_auth_id": dc.device_auth_id,
            "user_code": dc.user_code,
        });

        let resp = client
            .post(format!("{ISSUER}/api/accounts/deviceauth/token"))
            .json(&poll_body)
            .send()
            .await
            .map_err(|e| format!("device code poll failed: {e}"))?;

        let poll_status = resp.status();
        if poll_status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let poll: DeviceCodePollResponse = serde_json::from_str(&body)
                .map_err(|e| format!("bad poll response: {e}\nBody: {body}"))?;

            let code = poll
                .authorization_code
                .ok_or("missing authorization_code in poll response")?;
            let verifier = poll
                .code_verifier
                .ok_or("missing code_verifier in poll response")?;

            let redirect_uri = format!("{ISSUER}/deviceauth/callback");
            return exchange_code(client, &code, &verifier, &redirect_uri).await;
        }

        // 403/404 = authorization pending, keep polling. Other errors = bail.
        if poll_status.as_u16() != 403 && poll_status.as_u16() != 404 {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("device auth failed (HTTP {poll_status}): {body}"));
        }
    }
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
    let tokens = CodexTokens::load_from(&env.token_store)
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
        let pkce = PkceCodes {
            verifier: "v".into(),
            challenge: "ch".into(),
        };
        let url = build_authorize_url("http://localhost:1455/auth/callback", &pkce, "STATE");
        assert!(url.starts_with(ISSUER));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
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

    #[test]
    fn build_refresh_request_uses_env_issuer_and_json_body() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(
            "https://issuer.example".to_string(),
            tmp.path().join("tokens.json"),
        );

        let req = build_refresh_request(&env, "refresh-token");

        assert_eq!(req.url, "https://issuer.example/oauth/token");
        let body = serde_json::to_value(&req.body).unwrap();
        assert_eq!(body["client_id"], CLIENT_ID);
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "refresh-token");
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
        previous_tokens().save_to(&env.token_store).unwrap();

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

    // ---- html_error ----

    #[test]
    fn html_error_embeds_message_in_body() {
        let html = html_error("boom");
        assert!(html.contains("boom"));
        assert!(html.contains("Authorization Failed"));
    }

    // ---- classify_refresh_error ----

    #[test]
    fn classify_refresh_error_expired() {
        let s = classify_refresh_error(r#"{"error":"refresh_token_expired"}"#);
        assert!(s.contains("expired"));
        assert!(s.contains("smelt auth"));
    }

    #[test]
    fn classify_refresh_error_reused() {
        let s = classify_refresh_error("refresh_token_reused");
        assert!(s.contains("already used"));
    }

    #[test]
    fn classify_refresh_error_invalidated() {
        let s = classify_refresh_error("refresh_token_invalidated detail");
        assert!(s.contains("revoked"));
    }

    #[test]
    fn classify_refresh_error_unknown_returns_raw_body_prefixed() {
        let s = classify_refresh_error("server is on fire");
        assert!(s.starts_with("token refresh error:"));
        assert!(s.contains("server is on fire"));
    }

    // ---- deserialize_interval ----

    #[test]
    fn deserialize_interval_accepts_number() {
        let r: DeviceCodeResponse = serde_json::from_value(
            serde_json::json!({"device_auth_id":"d","user_code":"u","interval": 7}),
        )
        .unwrap();
        assert_eq!(r.interval, Some(7));
    }

    #[test]
    fn deserialize_interval_accepts_numeric_string() {
        let r: DeviceCodeResponse = serde_json::from_value(
            serde_json::json!({"device_auth_id":"d","user_code":"u","interval":"3"}),
        )
        .unwrap();
        assert_eq!(r.interval, Some(3));
    }

    #[test]
    fn deserialize_interval_handles_null() {
        let r: DeviceCodeResponse = serde_json::from_value(
            serde_json::json!({"device_auth_id":"d","user_code":"u","interval": null}),
        )
        .unwrap();
        assert!(r.interval.is_none());
    }

    #[test]
    fn deserialize_interval_defaults_to_none_when_field_missing() {
        let r: DeviceCodeResponse =
            serde_json::from_value(serde_json::json!({"device_auth_id":"d","user_code":"u"}))
                .unwrap();
        assert!(r.interval.is_none());
    }

    #[test]
    fn deserialize_device_code_response_accepts_usercode_alias() {
        let r: DeviceCodeResponse =
            serde_json::from_value(serde_json::json!({"device_auth_id":"d","usercode":"alt"}))
                .unwrap();
        assert_eq!(r.user_code, "alt");
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
        let resp = TokenResponse {
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
        let resp = TokenResponse {
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
        let resp = TokenResponse {
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
        let resp = TokenResponse {
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
        let resp = TokenResponse {
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
        let p = generate_pkce();
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
        let resp = TokenResponse {
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
        let resp = TokenResponse {
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
        }
        .into();

        assert_eq!(meta.id, "codex-mini");
        assert_eq!(meta.display_name.as_deref(), Some("Codex Mini"));
        assert_eq!(meta.context_window, Some(1234));
        assert_eq!(meta.supports_reasoning, None);
    }

    #[test]
    fn generate_state_produces_nonempty_unique_strings() {
        let a = generate_state();
        let b = generate_state();
        assert!(!a.is_empty());
        assert_ne!(a, b);
    }
}
