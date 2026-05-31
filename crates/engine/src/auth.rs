//! Authentication façade for provider login/logout and cached model lists.

use crate::provider;

/// Which OAuth-based provider to authenticate with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProvider {
    Codex,
    Copilot,
}

/// Login method for providers that support multiple flows.
#[derive(Debug, Clone, Copy)]
pub enum LoginMethod {
    /// Opens a browser for the redirect flow (Codex only).
    Browser,
    /// Device-code flow, shown in-terminal.
    DeviceCode,
}

pub struct LoginProgress<'a> {
    pub on_prompt: &'a (dyn Fn(&str, &str) + Send + Sync),
    pub on_message: &'a (dyn Fn(&str) + Send + Sync),
}

#[derive(Debug, Default, Clone)]
pub struct LoginDetails {
    pub account_id: Option<String>,
    pub api_base: Option<String>,
    pub expires_at: Option<String>,
}

pub async fn login(
    provider: AuthProvider,
    method: LoginMethod,
    client: &reqwest::Client,
    progress: &LoginProgress<'_>,
) -> Result<LoginDetails, String> {
    match provider {
        AuthProvider::Codex => codex_login(method, client).await,
        AuthProvider::Copilot => copilot_login(client, progress).await,
    }
}

pub fn logout(provider: AuthProvider) {
    match provider {
        AuthProvider::Codex => provider::codex::CodexTokens::delete(),
        AuthProvider::Copilot => provider::copilot::CopilotTokens::delete(),
    }
}

/// Return cached model identifiers for a provider (Codex slug, Copilot id).
pub fn cached_models(kind: AuthProvider) -> Vec<String> {
    match kind {
        AuthProvider::Codex => provider::codex::load_cached_models()
            .into_iter()
            .map(|m| m.slug)
            .collect(),
        AuthProvider::Copilot => provider::copilot::load_cached_models()
            .into_iter()
            .map(|m| m.id)
            .collect(),
    }
}

pub fn is_logged_in(provider: AuthProvider) -> bool {
    match provider {
        AuthProvider::Codex => provider::codex::CodexTokens::load().is_some(),
        AuthProvider::Copilot => provider::copilot::CopilotTokens::load().is_some(),
    }
}

#[derive(Debug, Clone)]
pub struct AuthenticatedResponse {
    pub status: u16,
    pub body: String,
}

pub async fn authenticated_request(
    provider: AuthProvider,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    client: &reqwest::Client,
) -> Result<AuthenticatedResponse, String> {
    match provider {
        AuthProvider::Codex => codex_authenticated_request(method, path, body, client).await,
        AuthProvider::Copilot => {
            Err("authenticated Copilot requests are not supported".to_string())
        }
    }
}

async fn codex_authenticated_request(
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    client: &reqwest::Client,
) -> Result<AuthenticatedResponse, String> {
    let tokens = provider::codex::ensure_access_token_full(client).await?;
    let url = format!(
        "{}{}",
        provider::codex::CHATGPT_BACKEND_API_BASE,
        authenticated_path(path)?
    );
    let mut req = match method.to_ascii_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url).body(body.unwrap_or_default()),
        other => return Err(format!("unsupported authenticated request method: {other}")),
    };
    req = tokens
        .apply_headers(req)
        .header("Accept", "application/json");

    let resp = req
        .send()
        .await
        .map_err(|e| format!("authenticated request failed: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok(AuthenticatedResponse { status, body })
}

fn authenticated_path(path: &str) -> Result<String, String> {
    if !path.starts_with('/') || path.starts_with("//") || path.contains("://") {
        return Err("authenticated request path must be an absolute path without a scheme".into());
    }
    Ok(path.to_string())
}

pub async fn refresh_models_cache(kind: AuthProvider, client: &reqwest::Client) -> Vec<String> {
    match kind {
        AuthProvider::Codex => provider::codex::refresh_models_cache(client)
            .await
            .into_iter()
            .map(|m| m.slug)
            .collect(),
        AuthProvider::Copilot => provider::copilot::refresh_models_cache(client)
            .await
            .into_iter()
            .map(|m| m.id)
            .collect(),
    }
}

async fn codex_login(
    method: LoginMethod,
    client: &reqwest::Client,
) -> Result<LoginDetails, String> {
    let tokens = match method {
        LoginMethod::Browser => provider::codex::browser_login(client).await?,
        LoginMethod::DeviceCode => provider::codex::device_code_login(client).await?,
    };
    Ok(LoginDetails {
        account_id: tokens.account_id,
        ..Default::default()
    })
}

async fn copilot_login(
    client: &reqwest::Client,
    progress: &LoginProgress<'_>,
) -> Result<LoginDetails, String> {
    let callbacks = provider::copilot::LoginCallbacks {
        on_prompt: progress.on_prompt,
        on_progress: progress.on_message,
    };
    let tokens = provider::copilot::device_code_login(client, &callbacks).await?;
    Ok(LoginDetails {
        api_base: Some(tokens.api_base),
        expires_at: Some(tokens.expires_at.to_string()),
        ..Default::default()
    })
}
