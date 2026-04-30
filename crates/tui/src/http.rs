//! HTTP capability — synchronous fetch primitives over `reqwest::blocking`.
//! Pure transport, no policy. Exposed to Lua via
//! `crates/tui/src/lua/api/http.rs` and composed by tools that need to
//! pull a URL.
//!
//! The shape is deliberately small: `get` returns a `Response` struct
//! with body bytes, status, headers, and the final URL after any
//! redirects. Caching / retry / cassette layers belong to the calling
//! tool, not here. The engine's legacy `web_fetch` tool keeps its own
//! cache + redirect chase until it migrates to Lua in P5.b, at which
//! point the cache layer absorbs into `tui::http::cache`.

use std::collections::HashMap;
use std::time::Duration;

/// Result of a single HTTP request.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub final_url: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    /// Body decoded as UTF-8 (lossy). Convenience for tools that want
    /// text output.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Options accepted by [`get`]. Defaults: 30s timeout, follow up to 10
/// redirects, no extra headers.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub timeout: Option<Duration>,
    pub max_redirects: Option<usize>,
    pub headers: HashMap<String, String>,
}

/// GET `url` with the given options. Errors surface as
/// `reqwest::Error` so callers (Lua bindings, tools) can decide how
/// to format them.
pub fn get(url: &str, opts: &Options) -> Result<Response, reqwest::Error> {
    let timeout = opts.timeout.unwrap_or(Duration::from_secs(30));
    let max_redirects = opts.max_redirects.unwrap_or(10);

    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(max_redirects))
        .build()?;

    let mut request = client.get(url);
    for (k, v) in &opts.headers {
        request = request.header(k, v);
    }
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
