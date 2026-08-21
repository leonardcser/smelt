//! HTTP capability. Async transport over `reqwest::Client`; caching and
//! application-specific rate-limit policy belong to the caller. Requests reuse pooled
//! clients, enforce a total timeout and streaming body limit, and can retry
//! transient GET failures when requested.
//!
//! Callers spawn these futures via [`crate::lua::shared::LuaResumeSink`] so a
//! parked Lua coroutine wakes up when the response lands. The Lua runtime is
//! never blocked on the request.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

pub(crate) mod cache;

#[derive(Debug, Default)]
pub(crate) struct Client {
    clients: Mutex<HashMap<usize, reqwest::Client>>,
}

#[derive(Debug)]
pub(crate) enum Error {
    Client(String),
    Transport(reqwest::Error),
    Deadline(Duration),
}

impl Error {
    fn retryable(&self) -> bool {
        matches!(self, Self::Transport(err) if err.is_connect() || err.is_timeout() || err.is_body())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(message) => f.write_str(message),
            Self::Transport(err) => err.fmt(f),
            Self::Deadline(timeout) => {
                write!(f, "request timed out after {} seconds", timeout.as_secs())
            }
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) final_url: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
    pub(crate) truncated: bool,
}

/// Defaults: 30s total timeout, up to 10 redirects, no retries, and a 10 MB body.
#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    pub(crate) timeout: Option<Duration>,
    pub(crate) max_redirects: Option<usize>,
    pub(crate) max_response_bytes: Option<usize>,
    pub(crate) max_retries: Option<usize>,
    pub(crate) headers: HashMap<String, String>,
}

impl Client {
    pub(crate) async fn get(&self, url: &str, opts: &Options) -> Result<Response, Error> {
        let timeout = opts.timeout.unwrap_or(DEFAULT_TIMEOUT);
        tokio::time::timeout(timeout, self.get_with_retries(url, opts))
            .await
            .map_err(|_| Error::Deadline(timeout))?
    }

    async fn get_with_retries(&self, url: &str, opts: &Options) -> Result<Response, Error> {
        let retries = opts.max_retries.unwrap_or(0);
        for attempt in 0..=retries {
            let request = request(self.client(opts)?, reqwest::Method::GET, url, opts);
            match finish(request, opts, attempt < retries).await {
                Ok(resp) if retryable_status(resp.status) && attempt < retries => {
                    tokio::time::sleep(response_retry_delay(&resp, attempt)).await;
                }
                Err(err) if err.retryable() && attempt < retries => {
                    tokio::time::sleep(retry_delay(attempt)).await;
                }
                result => return result,
            }
        }
        unreachable!("retry loop always returns on its final attempt")
    }

    /// Body is sent verbatim. Set `Content-Type` via `opts.headers` when needed.
    pub(crate) async fn post(
        &self,
        url: &str,
        body: Vec<u8>,
        opts: &Options,
    ) -> Result<Response, Error> {
        let timeout = opts.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let request = request(self.client(opts)?, reqwest::Method::POST, url, opts).body(body);
        tokio::time::timeout(timeout, finish(request, opts, false))
            .await
            .map_err(|_| Error::Deadline(timeout))?
    }

    fn client(&self, opts: &Options) -> Result<reqwest::Client, Error> {
        let max_redirects = opts.max_redirects.unwrap_or(10);
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| Error::Client("HTTP client cache lock poisoned".into()))?;
        if let Some(client) = clients.get(&max_redirects) {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(max_redirects))
            .build()
            .map_err(|err| Error::Client(err.to_string()))?;
        clients.insert(max_redirects, client.clone());
        Ok(client)
    }
}

fn request(
    client: reqwest::Client,
    method: reqwest::Method,
    url: &str,
    opts: &Options,
) -> reqwest::RequestBuilder {
    let mut request = client.request(method, url);
    for (key, value) in &opts.headers {
        request = request.header(key, value);
    }
    request
}

async fn finish(
    request: reqwest::RequestBuilder,
    opts: &Options,
    skip_retryable_body: bool,
) -> Result<Response, Error> {
    let mut resp = request.send().await.map_err(Error::Transport)?;
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.to_string(), s.to_string())))
        .collect();
    if skip_retryable_body && retryable_status(status) {
        return Ok(Response {
            status,
            final_url,
            headers,
            body: Vec::new(),
            truncated: false,
        });
    }
    let limit = opts
        .max_response_bytes
        .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES);
    let mut body = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    while let Some(chunk) = resp.chunk().await.map_err(Error::Transport)? {
        let remaining = limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Response {
        status,
        final_url,
        headers,
        body,
        truncated,
    })
}

fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 502 | 503 | 504)
}

fn retry_delay(attempt: usize) -> Duration {
    RETRY_BASE_DELAY.saturating_mul(1 << attempt.min(4))
}

fn response_retry_delay(response: &Response, attempt: usize) -> Duration {
    response
        .headers
        .get("retry-after")
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .map(|delay| delay.min(Duration::from_secs(10)))
        .unwrap_or_else(|| retry_delay(attempt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_unreachable_returns_error() {
        let opts = Options {
            timeout: Some(Duration::from_millis(50)),
            ..Default::default()
        };
        let err = Client::default().get("http://127.0.0.1:1/", &opts).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn get_caps_response_while_streaming() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123456789")
                .await
                .unwrap();
        });

        let response = Client::default()
            .get(
                &format!("http://{address}/"),
                &Options {
                    max_response_bytes: Some(4),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(response.body, b"0123");
        assert!(response.truncated);
    }

    #[test]
    fn retry_after_seconds_overrides_backoff_with_a_cap() {
        let response = Response {
            status: 429,
            final_url: String::new(),
            headers: HashMap::from([("retry-after".into(), "30".into())]),
            body: Vec::new(),
            truncated: false,
        };
        assert_eq!(response_retry_delay(&response, 0), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn get_retries_transient_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for status in ["503 Service Unavailable", "200 OK"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                let _ = socket.read(&mut request).await;
                let response = format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\n\r\nok");
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let response = Client::default()
            .get(
                &format!("http://{address}/"),
                &Options {
                    max_retries: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
    }
}
