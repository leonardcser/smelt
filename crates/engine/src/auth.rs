//! Authentication façade for provider login/logout and cached model lists.

use crate::provider;
use std::ffi::OsString;
use std::process::Stdio;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserStatus {
    can_open: bool,
    reason: Option<&'static str>,
}

impl BrowserStatus {
    pub fn can_open(&self) -> bool {
        self.can_open
    }

    pub fn reason(&self) -> Option<&'static str> {
        self.reason
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserPlatform {
    Macos,
    Linux,
    Windows,
    Other,
}

impl BrowserPlatform {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserCommand {
    program: &'static str,
    args: Vec<String>,
}

pub fn browser_status() -> BrowserStatus {
    browser_status_for(BrowserPlatform::current(), |name| std::env::var_os(name))
}

fn browser_status_for<F>(platform: BrowserPlatform, env: F) -> BrowserStatus
where
    F: Fn(&str) -> Option<OsString>,
{
    let has = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    let has_graphical_display = has("DISPLAY") || has("WAYLAND_DISPLAY");
    let is_ssh = has("SSH_CONNECTION") || has("SSH_CLIENT") || has("SSH_TTY");

    if is_ssh && !has_graphical_display {
        return BrowserStatus {
            can_open: false,
            reason: Some("running over SSH without a graphical display"),
        };
    }

    match platform {
        BrowserPlatform::Linux if !has_graphical_display => BrowserStatus {
            can_open: false,
            reason: Some("DISPLAY or WAYLAND_DISPLAY is not set"),
        },
        BrowserPlatform::Macos | BrowserPlatform::Linux | BrowserPlatform::Windows => {
            BrowserStatus {
                can_open: true,
                reason: None,
            }
        }
        BrowserPlatform::Other => BrowserStatus {
            can_open: false,
            reason: Some("unsupported platform"),
        },
    }
}

fn browser_command_for(platform: BrowserPlatform, url: &str) -> Option<BrowserCommand> {
    match platform {
        BrowserPlatform::Macos => Some(BrowserCommand {
            program: "open",
            args: vec![url.to_string()],
        }),
        BrowserPlatform::Linux => Some(BrowserCommand {
            program: "xdg-open",
            args: vec![url.to_string()],
        }),
        BrowserPlatform::Windows => Some(BrowserCommand {
            program: "cmd",
            args: vec![
                "/C".to_string(),
                "start".to_string(),
                "".to_string(),
                url.to_string(),
            ],
        }),
        BrowserPlatform::Other => None,
    }
}

fn spawn_browser(url: &str) -> Result<(), String> {
    let command = browser_command_for(BrowserPlatform::current(), url)
        .ok_or_else(|| "unsupported platform".to_string())?;
    std::process::Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to launch browser: {e}"))
}

fn present_auth_link(progress: &LoginProgress<'_>, url: &str, code: &str) {
    let status = browser_status();
    present_auth_link_with_opener(progress, url, code, status, spawn_browser);
}

fn present_auth_link_with_opener<F>(
    progress: &LoginProgress<'_>,
    url: &str,
    code: &str,
    status: BrowserStatus,
    opener: F,
) where
    F: FnOnce(&str) -> Result<(), String>,
{
    if status.can_open() {
        match opener(url) {
            Ok(()) => {
                if code.is_empty() {
                    (progress.on_message)("Opened authorization page in your browser.");
                } else {
                    (progress.on_message)(&format!(
                        "Opened authorization page in your browser. Enter code if prompted: {code}"
                    ));
                }
                return;
            }
            Err(err) => {
                (progress.on_message)(&format!("Could not open browser automatically: {err}"));
            }
        }
    } else if let Some(reason) = status.reason() {
        (progress.on_message)(&format!("Browser auto-open unavailable ({reason})."));
    }

    (progress.on_prompt)(url, code);
}

/// Which OAuth-based provider to authenticate with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProvider {
    Codex,
    Copilot,
    KimiCode,
}

pub type AuthModelInfo = protocol::ModelMetadata;

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
        AuthProvider::Codex => provider::codex::CodexTokens::delete(),
        AuthProvider::Copilot => provider::copilot::CopilotTokens::delete(),
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
    match provider {
        AuthProvider::Codex => provider::codex::CodexTokens::load().is_some(),
        AuthProvider::Copilot => provider::copilot::CopilotTokens::load().is_some(),
        AuthProvider::KimiCode => provider::kimi_code::is_logged_in(),
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
        AuthProvider::KimiCode => {
            provider::kimi_code::authenticated_request(method, path, body, client).await
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
        "POST" => client.post(url).body(body.unwrap_or_default()),
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

pub async fn refresh_models_cache(kind: AuthProvider, client: &reqwest::Client) -> Vec<String> {
    refresh_model_info(kind, client)
        .await
        .into_iter()
        .map(|model| model.id)
        .collect()
}

pub async fn refresh_model_info(
    kind: AuthProvider,
    client: &reqwest::Client,
) -> Vec<AuthModelInfo> {
    match kind {
        AuthProvider::Codex => provider::codex::refresh_models_cache(client)
            .await
            .into_iter()
            .map(Into::into)
            .collect(),
        AuthProvider::Copilot => provider::copilot::refresh_models_cache(client)
            .await
            .into_iter()
            .map(Into::into)
            .collect(),
        AuthProvider::KimiCode => provider::kimi_code::fetch_model_info(client)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect(),
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
    use std::ffi::OsString;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn env_with<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    fn status(can_open: bool, reason: Option<&'static str>) -> BrowserStatus {
        BrowserStatus { can_open, reason }
    }

    #[test]
    fn browser_status_detects_linux_display_headless_and_ssh() {
        assert!(!browser_status_for(BrowserPlatform::Linux, env_with(&[])).can_open());
        assert_eq!(
            browser_status_for(BrowserPlatform::Linux, env_with(&[])).reason(),
            Some("DISPLAY or WAYLAND_DISPLAY is not set")
        );
        assert!(browser_status_for(
            BrowserPlatform::Linux,
            env_with(&[("WAYLAND_DISPLAY", "wayland-1")])
        )
        .can_open());

        let ssh = browser_status_for(
            BrowserPlatform::Linux,
            env_with(&[("SSH_CONNECTION", "client server")]),
        );
        assert!(!ssh.can_open());
        assert_eq!(
            ssh.reason(),
            Some("running over SSH without a graphical display")
        );
    }

    #[test]
    fn browser_status_allows_local_macos_and_windows() {
        assert!(browser_status_for(BrowserPlatform::Macos, env_with(&[])).can_open());
        assert!(browser_status_for(BrowserPlatform::Windows, env_with(&[])).can_open());
        assert!(!browser_status_for(BrowserPlatform::Other, env_with(&[])).can_open());
    }

    #[test]
    fn browser_command_matches_platform() {
        assert_eq!(
            browser_command_for(BrowserPlatform::Macos, "https://example.test"),
            Some(BrowserCommand {
                program: "open",
                args: vec!["https://example.test".to_string()]
            })
        );
        assert_eq!(
            browser_command_for(BrowserPlatform::Linux, "https://example.test"),
            Some(BrowserCommand {
                program: "xdg-open",
                args: vec!["https://example.test".to_string()]
            })
        );
        assert_eq!(
            browser_command_for(BrowserPlatform::Windows, "https://example.test"),
            Some(BrowserCommand {
                program: "cmd",
                args: vec![
                    "/C".to_string(),
                    "start".to_string(),
                    "".to_string(),
                    "https://example.test".to_string()
                ]
            })
        );
        assert_eq!(
            browser_command_for(BrowserPlatform::Other, "https://example.test"),
            None
        );
    }

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

        present_auth_link_with_opener(
            &progress,
            "https://example.test/auth",
            "ABCD-1234",
            status(true, None),
            |url| {
                opened.lock().unwrap().push(url.to_string());
                Ok(())
            },
        );

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

        present_auth_link_with_opener(
            &progress,
            "https://example.test/auth",
            "",
            status(false, Some("headless")),
            |_| panic!("opener should not run"),
        );

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

        present_auth_link_with_opener(
            &progress,
            "https://example.test/auth",
            "CODE",
            status(true, None),
            |_| Err("missing opener".to_string()),
        );

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
