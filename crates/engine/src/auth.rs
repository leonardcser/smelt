//! Authentication façade for provider login/logout and cached model lists.

use crate::opener::{self, OpenResult};
use crate::provider;

fn present_auth_link(progress: &LoginProgress<'_>, url: &str, code: &str) {
    present_auth_link_with_opener(progress, url, code, || opener::open_url_if_available(url));
}

fn present_auth_link_with_opener<F>(progress: &LoginProgress<'_>, url: &str, code: &str, opener: F)
where
    F: FnOnce() -> OpenResult,
{
    match opener() {
        OpenResult::Opened => {
            if code.is_empty() {
                (progress.on_message)("Opened authorization page in your browser.");
            } else {
                (progress.on_message)(&format!(
                    "Opened authorization page in your browser. Enter code if prompted: {code}"
                ));
            }
            return;
        }
        OpenResult::Unavailable(reason) => {
            (progress.on_message)(&format!("Browser auto-open unavailable ({reason})."));
        }
        OpenResult::Failed(err) => {
            (progress.on_message)(&format!("Could not open browser automatically: {err}"));
        }
    }

    (progress.on_prompt)(url, code);
}

/// Which OAuth-based provider to authenticate with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AuthProvider {
    Codex,
    Copilot,
    KimiCode,
}

impl AuthProvider {
    pub fn from_provider_type(provider_type: &str) -> Option<Self> {
        match provider_type {
            "codex" => Some(Self::Codex),
            "copilot" => Some(Self::Copilot),
            "kimi-code" => Some(Self::KimiCode),
            _ => None,
        }
    }

    pub const fn provider_type(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::KimiCode => "kimi-code",
        }
    }
}

pub type AuthModelInfo = protocol::ModelMetadata;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedModelsRefreshOutcome {
    Fresh {
        models: Vec<AuthModelInfo>,
        cache_warning: Option<String>,
    },
    CachedFallback {
        models: Vec<AuthModelInfo>,
        warning: String,
    },
    Unauthenticated,
    CredentialsChanged,
    Failed(String),
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
        AuthProvider::Codex => codex_login(method, client, progress).await,
        AuthProvider::Copilot => copilot_login(client, progress).await,
        AuthProvider::KimiCode => kimi_code_login(client, progress).await,
    }
}

pub fn logout(provider: AuthProvider) {
    match provider {
        AuthProvider::Codex => provider::codex::delete_tokens(),
        AuthProvider::Copilot => provider::copilot::delete_tokens(),
        AuthProvider::KimiCode => provider::kimi_code::logout(),
    }
}

/// Return cached model identifiers for a provider (Codex slug, Copilot id).
pub fn cached_models(kind: AuthProvider) -> Vec<String> {
    cached_model_info(kind)
        .into_iter()
        .map(|model| model.id)
        .collect()
}

pub fn cached_model_info(kind: AuthProvider) -> Vec<AuthModelInfo> {
    match kind {
        AuthProvider::Codex => provider::codex::load_cached_models()
            .into_iter()
            .map(Into::into)
            .collect(),
        AuthProvider::Copilot => provider::copilot::load_cached_models()
            .into_iter()
            .map(Into::into)
            .collect(),
        AuthProvider::KimiCode => provider::kimi_code::load_cached_model_info()
            .into_iter()
            .map(Into::into)
            .collect(),
    }
}

pub fn is_logged_in(provider: AuthProvider) -> bool {
    credential_fingerprint(provider).is_some()
}

/// In-memory identity used only to invalidate stale refreshes after credentials
/// change. The hash is never persisted or logged.
pub fn credential_fingerprint(provider: AuthProvider) -> Option<u64> {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match provider {
        AuthProvider::Codex => {
            let tokens = provider::codex::load_tokens()?;
            tokens
                .account_id
                .as_deref()
                .unwrap_or(&tokens.refresh_token)
                .hash(&mut hasher);
        }
        AuthProvider::Copilot => {
            let tokens = provider::copilot::load_tokens()?;
            tokens.refresh_token.hash(&mut hasher);
        }
        AuthProvider::KimiCode => {
            let tokens = provider::kimi_code::load_tokens()?;
            tokens.refresh_token.hash(&mut hasher);
        }
    }
    Some(hasher.finish())
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
        AuthProvider::KimiCode => {
            provider::kimi_code::authenticated_request(method, path, body, client).await
        }
    }
}

pub async fn managed_usage(
    provider: AuthProvider,
    client: &reqwest::Client,
) -> Result<smelt_provider::kimi_code::ManagedUsageReport, String> {
    match provider {
        AuthProvider::KimiCode => provider::kimi_code::managed_usage(client).await,
        AuthProvider::Codex | AuthProvider::Copilot => {
            Err("managed usage is not supported for this provider".to_string())
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
        smelt_provider::codex::CHATGPT_BACKEND_API_BASE,
        authenticated_path(path)?
    );
    send_authenticated_request(client, method, url, body, |req| tokens.apply_headers(req)).await
}

pub(crate) fn authenticated_path(path: &str) -> Result<String, String> {
    if !path.starts_with('/') || path.starts_with("//") || path.contains("://") {
        return Err("authenticated request path must be an absolute path without a scheme".into());
    }
    Ok(path.to_string())
}

pub(crate) async fn send_authenticated_request<F>(
    client: &reqwest::Client,
    method: &str,
    url: String,
    body: Option<Vec<u8>>,
    apply_auth: F,
) -> Result<AuthenticatedResponse, String>
where
    F: FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
{
    let req = match method.to_ascii_uppercase().as_str() {
        "GET" => client.get(url),
        "POST" => client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.unwrap_or_default()),
        other => return Err(format!("unsupported authenticated request method: {other}")),
    };
    let resp = apply_auth(req.header("Accept", "application/json"))
        .send()
        .await
        .map_err(|e| format!("authenticated request failed: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok(AuthenticatedResponse { status, body })
}

pub async fn refresh_model_info_outcome_for(
    kind: AuthProvider,
    client: &reqwest::Client,
    expected_fingerprint: u64,
) -> ManagedModelsRefreshOutcome {
    if credential_fingerprint(kind) != Some(expected_fingerprint) {
        return ManagedModelsRefreshOutcome::CredentialsChanged;
    }
    match kind {
        AuthProvider::Codex => match provider::codex::fetch_models_fresh(client).await {
            Ok(models) => finish_model_refresh(
                kind,
                expected_fingerprint,
                models,
                provider::codex::save_models_cache,
            ),
            Err(error) => model_refresh_failure(kind, expected_fingerprint, error),
        },
        AuthProvider::Copilot => match provider::copilot::fetch_models_fresh(client).await {
            Ok(models) => finish_model_refresh(
                kind,
                expected_fingerprint,
                models,
                provider::copilot::save_models_cache,
            ),
            Err(error) => model_refresh_failure(kind, expected_fingerprint, error),
        },
        AuthProvider::KimiCode => match provider::kimi_code::fetch_models_fresh(client).await {
            Ok(models) => finish_model_refresh(
                kind,
                expected_fingerprint,
                models,
                provider::kimi_code::save_models_cache,
            ),
            Err(error) => model_refresh_failure(kind, expected_fingerprint, error),
        },
    }
}

fn finish_model_refresh<T>(
    kind: AuthProvider,
    expected_fingerprint: u64,
    models: Vec<T>,
    save: impl FnOnce(&[T]) -> Result<(), String>,
) -> ManagedModelsRefreshOutcome
where
    T: Into<AuthModelInfo>,
{
    if credential_fingerprint(kind) != Some(expected_fingerprint) {
        return ManagedModelsRefreshOutcome::CredentialsChanged;
    }
    let cache_warning = save(&models).err();
    ManagedModelsRefreshOutcome::Fresh {
        models: models.into_iter().map(Into::into).collect(),
        cache_warning,
    }
}

fn model_refresh_failure(
    kind: AuthProvider,
    expected_fingerprint: u64,
    error: String,
) -> ManagedModelsRefreshOutcome {
    if credential_fingerprint(kind) != Some(expected_fingerprint) {
        return ManagedModelsRefreshOutcome::CredentialsChanged;
    }
    let models = cached_model_info(kind);
    if models.is_empty() {
        ManagedModelsRefreshOutcome::Failed(error)
    } else {
        ManagedModelsRefreshOutcome::CachedFallback {
            models,
            warning: error,
        }
    }
}

async fn codex_login(
    method: LoginMethod,
    client: &reqwest::Client,
    progress: &LoginProgress<'_>,
) -> Result<LoginDetails, String> {
    let on_prompt = |url: &str, code: &str| {
        present_auth_link(progress, url, code);
    };
    let callbacks = provider::LoginCallbacks {
        on_prompt: &on_prompt,
        on_progress: progress.on_message,
    };
    let tokens = match method {
        LoginMethod::Browser => provider::codex::browser_login(client, &callbacks).await?,
        LoginMethod::DeviceCode => provider::codex::device_code_login(client, &callbacks).await?,
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
    let on_prompt = |url: &str, code: &str| {
        present_auth_link(progress, url, code);
    };
    let callbacks = provider::LoginCallbacks {
        on_prompt: &on_prompt,
        on_progress: progress.on_message,
    };
    let tokens = provider::copilot::device_code_login(client, &callbacks).await?;
    Ok(LoginDetails {
        api_base: Some(tokens.api_base),
        expires_at: Some(tokens.expires_at.to_string()),
        ..Default::default()
    })
}

async fn kimi_code_login(
    client: &reqwest::Client,
    progress: &LoginProgress<'_>,
) -> Result<LoginDetails, String> {
    let on_prompt = |url: &str, code: &str| {
        present_auth_link(progress, url, code);
    };
    let callbacks = provider::LoginCallbacks {
        on_prompt: &on_prompt,
        on_progress: progress.on_message,
    };
    let tokens = provider::kimi_code::login(client, &callbacks).await?;
    Ok(LoginDetails {
        api_base: Some(provider::kimi_code::api_base()),
        expires_at: Some(tokens.expires_at.to_string()),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn present_auth_link_opens_when_browser_available() {
        let prompts = Mutex::new(Vec::<(String, String)>::new());
        let messages = Mutex::new(Vec::<String>::new());
        let opened = Mutex::new(Vec::<String>::new());
        let on_prompt = |url: &str, code: &str| {
            prompts
                .lock()
                .unwrap()
                .push((url.to_string(), code.to_string()));
        };
        let on_message = |msg: &str| messages.lock().unwrap().push(msg.to_string());
        let progress = LoginProgress {
            on_prompt: &on_prompt,
            on_message: &on_message,
        };

        present_auth_link_with_opener(&progress, "https://example.test/auth", "ABCD-1234", || {
            opened
                .lock()
                .unwrap()
                .push("https://example.test/auth".to_string());
            OpenResult::Opened
        });

        assert_eq!(
            *opened.lock().unwrap(),
            vec!["https://example.test/auth".to_string()]
        );
        assert!(prompts.lock().unwrap().is_empty());
        assert_eq!(
            messages.lock().unwrap().as_slice(),
            &["Opened authorization page in your browser. Enter code if prompted: ABCD-1234"]
        );
    }

    #[test]
    fn present_auth_link_shows_prompt_when_browser_unavailable() {
        let prompts = Mutex::new(Vec::<(String, String)>::new());
        let messages = Mutex::new(Vec::<String>::new());
        let on_prompt = |url: &str, code: &str| {
            prompts
                .lock()
                .unwrap()
                .push((url.to_string(), code.to_string()));
        };
        let on_message = |msg: &str| messages.lock().unwrap().push(msg.to_string());
        let progress = LoginProgress {
            on_prompt: &on_prompt,
            on_message: &on_message,
        };

        present_auth_link_with_opener(&progress, "https://example.test/auth", "", || {
            OpenResult::Unavailable("headless")
        });

        assert_eq!(
            prompts.lock().unwrap().as_slice(),
            &[("https://example.test/auth".to_string(), String::new())]
        );
        assert_eq!(
            messages.lock().unwrap().as_slice(),
            &["Browser auto-open unavailable (headless)."]
        );
    }

    #[test]
    fn present_auth_link_falls_back_to_prompt_when_open_fails() {
        let prompts = Mutex::new(Vec::<(String, String)>::new());
        let messages = Mutex::new(Vec::<String>::new());
        let on_prompt = |url: &str, code: &str| {
            prompts
                .lock()
                .unwrap()
                .push((url.to_string(), code.to_string()));
        };
        let on_message = |msg: &str| messages.lock().unwrap().push(msg.to_string());
        let progress = LoginProgress {
            on_prompt: &on_prompt,
            on_message: &on_message,
        };

        present_auth_link_with_opener(&progress, "https://example.test/auth", "CODE", || {
            OpenResult::Failed("missing opener".to_string())
        });

        assert_eq!(
            prompts.lock().unwrap().as_slice(),
            &[("https://example.test/auth".to_string(), "CODE".to_string())]
        );
        assert_eq!(
            messages.lock().unwrap().as_slice(),
            &["Could not open browser automatically: missing opener"]
        );
    }

    #[test]
    fn changed_credentials_prevent_a_stale_refresh_from_writing_cache() {
        let current = credential_fingerprint(AuthProvider::Codex);
        let expected = current.unwrap_or(0).wrapping_add(1);
        let cache_written = std::cell::Cell::new(false);

        let outcome = finish_model_refresh(
            AuthProvider::Codex,
            expected,
            Vec::<protocol::ModelMetadata>::new(),
            |_| {
                cache_written.set(true);
                Ok(())
            },
        );

        assert_eq!(outcome, ManagedModelsRefreshOutcome::CredentialsChanged);
        assert!(!cache_written.get());
    }

    #[test]
    fn authenticated_path_accepts_single_absolute_path() {
        assert_eq!(
            authenticated_path("/backend-api/me").unwrap(),
            "/backend-api/me"
        );
    }

    #[test]
    fn authenticated_path_rejects_relative_double_slash_and_scheme() {
        assert!(authenticated_path("relative").is_err());
        assert!(authenticated_path("//evil.test/path").is_err());
        assert!(authenticated_path("/https://evil.test/path").is_err());
    }

    async fn spawn_one_response(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            loop {
                let mut chunk = [0u8; 1024];
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let req = String::from_utf8_lossy(&buf);
                let Some((headers, body)) = req.split_once("\r\n\r\n") else {
                    continue;
                };
                let content_len = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                if body.len() >= content_len {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&buf).to_string();
            let resp = format!(
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            req
        });
        (format!("http://{addr}/auth-test"), task)
    }

    #[tokio::test]
    async fn send_authenticated_request_sends_get_with_auth_and_accept() {
        let (url, task) = spawn_one_response(r#"{"ok":true}"#).await;
        let client = reqwest::Client::new();

        let resp = send_authenticated_request(&client, "GET", url, None, |req| {
            req.header("Authorization", "Bearer test-token")
        })
        .await
        .unwrap();

        assert_eq!(resp.status, 201);
        assert_eq!(resp.body, r#"{"ok":true}"#);
        let request = task.await.unwrap();
        assert!(request.starts_with("GET /auth-test HTTP/1.1"), "{request}");
        assert!(
            request.contains("authorization: Bearer test-token"),
            "{request}"
        );
        assert!(request.contains("accept: application/json"), "{request}");
    }

    #[tokio::test]
    async fn send_authenticated_request_sends_post_body() {
        let (url, task) = spawn_one_response("done").await;
        let client = reqwest::Client::new();

        let resp =
            send_authenticated_request(&client, "POST", url, Some(b"payload".to_vec()), |req| req)
                .await
                .unwrap();

        assert_eq!(resp.status, 201);
        assert_eq!(resp.body, "done");
        let request = task.await.unwrap();
        assert!(request.starts_with("POST /auth-test HTTP/1.1"), "{request}");
        assert!(
            request.contains("content-type: application/json"),
            "{request}"
        );
        assert!(request.ends_with("payload"), "{request}");
    }

    #[tokio::test]
    async fn send_authenticated_request_rejects_unsupported_method_before_network() {
        let client = reqwest::Client::new();
        let err = send_authenticated_request(
            &client,
            "DELETE",
            "http://127.0.0.1:9/nope".into(),
            None,
            |req| req,
        )
        .await
        .unwrap_err();

        assert!(err.contains("unsupported authenticated request method: DELETE"));
    }
}
