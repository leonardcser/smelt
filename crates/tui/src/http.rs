//! HTTP capability — synchronous fetch primitives over `reqwest::blocking`.
//! Pure transport, no policy. Exposed to Lua via
//! `crates/tui/src/lua/api/http.rs` and composed by tools that need to
//! pull a URL.
//!
//! `get` and `post` return a [`Response`] struct with body bytes,
//! status, headers, and the final URL after any redirects. Retry /
//! cassette layers belong to the calling tool. The disk-backed
//! TTL [`cache`] submodule lives here for tools that want to skip
//! repeated fetches; `random_user_agent` provides a pseudo-rotating
//! UA string to soften rate-limit detection on scraped endpoints.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub(crate) mod cache;

/// Result of a single HTTP request.
#[derive(Debug, Clone)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) final_url: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
}

/// Options accepted by [`get`]. Defaults: 30s timeout, follow up to 10
/// redirects, no extra headers.
#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    pub(crate) timeout: Option<Duration>,
    pub(crate) max_redirects: Option<usize>,
    pub(crate) headers: HashMap<String, String>,
}

/// GET `url` with the given options. Errors surface as
/// `reqwest::Error` so callers (Lua bindings, tools) can decide how
/// to format them.
pub(crate) fn get(url: &str, opts: &Options) -> Result<Response, reqwest::Error> {
    let mut request = build_client(opts)?.get(url);
    for (k, v) in &opts.headers {
        request = request.header(k, v);
    }
    finish(request)
}

/// POST `url` with the given body bytes. Defaults match [`get`]:
/// 30s timeout, follow up to 10 redirects, no extra headers. Body is
/// sent verbatim — set `Content-Type` via `opts.headers` when needed.
pub(crate) fn post(url: &str, body: Vec<u8>, opts: &Options) -> Result<Response, reqwest::Error> {
    let mut request = build_client(opts)?.post(url).body(body);
    for (k, v) in &opts.headers {
        request = request.header(k, v);
    }
    finish(request)
}

fn build_client(opts: &Options) -> Result<reqwest::blocking::Client, reqwest::Error> {
    let timeout = opts.timeout.unwrap_or(Duration::from_secs(30));
    let max_redirects = opts.max_redirects.unwrap_or(10);
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(max_redirects))
        .build()
}

fn finish(request: reqwest::blocking::RequestBuilder) -> Result<Response, reqwest::Error> {
    let resp = request.send()?;
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.to_string(), s.to_string())))
        .collect();
    let body = resp.bytes()?.to_vec();
    Ok(Response {
        status,
        final_url,
        headers,
        body,
    })
}

/// Rotating User-Agent picker. 80% round-robin over [`USER_AGENTS`],
/// 20% pseudo-random pick. Useful when scraping endpoints that
/// rate-limit by UA.
pub(crate) fn random_user_agent() -> &'static str {
    let idx = UA_COUNTER.fetch_add(1, Ordering::Relaxed);
    if idx.is_multiple_of(5) {
        let mixed = idx.wrapping_mul(6364136223846793005).wrapping_add(1);
        USER_AGENTS[mixed % USER_AGENTS.len()]
    } else {
        USER_AGENTS[idx % USER_AGENTS.len()]
    }
}

static UA_COUNTER: AtomicUsize = AtomicUsize::new(0);

const USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) Gecko/20100101 Firefox/133.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:133.0) Gecko/20100101 Firefox/133.0",
    "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.2 Safari/605.1.15",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 18_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.2 Mobile/15E148 Safari/604.1",
    "Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:132.0) Gecko/20100101 Firefox/132.0",
    "Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0",
    "Mozilla/5.0 (iPad; CPU OS 18_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.2 Mobile/15E148 Safari/604.1",
    "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 OPR/116.0.0.0",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Network calls aren't available in CI sandboxes; this test only
    /// exercises the option plumbing by calling a localhost address
    /// that's guaranteed to fail. We just check the error path returns
    /// without panicking.
    #[test]
    fn get_unreachable_returns_error() {
        let opts = Options {
            timeout: Some(Duration::from_millis(50)),
            ..Default::default()
        };
        // `127.0.0.1:1` is a reserved low port — connect should refuse.
        let err = get("http://127.0.0.1:1/", &opts);
        assert!(err.is_err());
    }
}
