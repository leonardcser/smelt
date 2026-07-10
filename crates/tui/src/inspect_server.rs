//! Local HTTP server for the `/inspect` session introspection UI.
//!
//! Serves an embedded single-page application plus a small read-only JSON
//! API backed by `smelt_core::session` and `smelt_store`.

use serde::Serialize;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const INDEX_HTML: &str = include_str!("inspect/index.html");

struct StaticAsset {
    path: &'static str,
    content_type: &'static str,
    body: &'static str,
}

const INSPECT_ASSETS: &[StaticAsset] = &[
    StaticAsset {
        path: "style.css",
        content_type: "text/css; charset=utf-8",
        body: include_str!("inspect/style.css"),
    },
    StaticAsset {
        path: "app.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/app.js"),
    },
    StaticAsset {
        path: "format.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/format.js"),
    },
    StaticAsset {
        path: "markdown.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/markdown.js"),
    },
    StaticAsset {
        path: "json_view.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/json_view.js"),
    },
    StaticAsset {
        path: "render_overview.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/render_overview.js"),
    },
    StaticAsset {
        path: "render_conversation.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/render_conversation.js"),
    },
    StaticAsset {
        path: "render_requests.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/render_requests.js"),
    },
    StaticAsset {
        path: "vendor/marked.min.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/vendor/marked.min.js"),
    },
    StaticAsset {
        path: "vendor/purify.min.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/vendor/purify.min.js"),
    },
    StaticAsset {
        path: "vendor/shiki-core.mjs",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/vendor/shiki-core.mjs"),
    },
    StaticAsset {
        path: "vendor/shiki-json.mjs",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/vendor/shiki-json.mjs"),
    },
    StaticAsset {
        path: "vendor/shiki-github-dark-default.mjs",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/vendor/shiki-github-dark-default.mjs"),
    },
    StaticAsset {
        path: "vendor/shiki-engine-javascript.mjs",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/vendor/shiki-engine-javascript.mjs"),
    },
    StaticAsset {
        path: "vendor/node/process.mjs",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/vendor/node/process.mjs"),
    },
    StaticAsset {
        path: "vendor/node/events.mjs",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/vendor/node/events.mjs"),
    },
    StaticAsset {
        path: "vendor/node/tty.mjs",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/vendor/node/tty.mjs"),
    },
    StaticAsset {
        path: "vendor/node/async_hooks.mjs",
        content_type: "text/javascript; charset=utf-8",
        body: include_str!("inspect/vendor/node/async_hooks.mjs"),
    },
    StaticAsset {
        path: "icons/account.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/account.svg"),
    },
    StaticAsset {
        path: "icons/arrow-right.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/arrow-right.svg"),
    },
    StaticAsset {
        path: "icons/check.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/check.svg"),
    },
    StaticAsset {
        path: "icons/circle-outline.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/circle-outline.svg"),
    },
    StaticAsset {
        path: "icons/comment-discussion.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/comment-discussion.svg"),
    },
    StaticAsset {
        path: "icons/copy.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/copy.svg"),
    },
    StaticAsset {
        path: "icons/dashboard.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/dashboard.svg"),
    },
    StaticAsset {
        path: "icons/database.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/database.svg"),
    },
    StaticAsset {
        path: "icons/debug-alt.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/debug-alt.svg"),
    },
    StaticAsset {
        path: "icons/error.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/error.svg"),
    },
    StaticAsset {
        path: "icons/eye.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/eye.svg"),
    },
    StaticAsset {
        path: "icons/file-code.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/file-code.svg"),
    },
    StaticAsset {
        path: "icons/folder.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/folder.svg"),
    },
    StaticAsset {
        path: "icons/gear.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/gear.svg"),
    },
    StaticAsset {
        path: "icons/history.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/history.svg"),
    },
    StaticAsset {
        path: "icons/hubot.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/hubot.svg"),
    },
    StaticAsset {
        path: "icons/json.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/json.svg"),
    },
    StaticAsset {
        path: "icons/list-filter.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/list-filter.svg"),
    },
    StaticAsset {
        path: "icons/note.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/note.svg"),
    },
    StaticAsset {
        path: "icons/package.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/package.svg"),
    },
    StaticAsset {
        path: "icons/pulse.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/pulse.svg"),
    },
    StaticAsset {
        path: "icons/search.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/search.svg"),
    },
    StaticAsset {
        path: "icons/server.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/server.svg"),
    },
    StaticAsset {
        path: "icons/symbol-event.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/symbol-event.svg"),
    },
    StaticAsset {
        path: "icons/symbol-method.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/symbol-method.svg"),
    },
    StaticAsset {
        path: "icons/terminal.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/terminal.svg"),
    },
    StaticAsset {
        path: "icons/tools.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/tools.svg"),
    },
    StaticAsset {
        path: "icons/warning.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/warning.svg"),
    },
    StaticAsset {
        path: "icons/zap.svg",
        content_type: "image/svg+xml",
        body: include_str!("inspect/icons/zap.svg"),
    },
];

#[derive(Debug, Clone, Serialize)]
struct SessionListItem {
    #[serde(flatten)]
    meta: smelt_core::session::SessionMeta,
    project: Option<String>,
    path_group: Option<String>,
    request_stats: RequestStats,
}

type RequestStats = smelt_store::RequestAuditStats;

#[derive(Debug, Clone, Serialize)]
struct SessionSummary {
    id: String,
    project: Option<String>,
    path_group: Option<String>,
    request_stats: RequestStats,
}

/// Handle to the running inspect server.
pub struct Server {
    local_addr: std::net::SocketAddr,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Server {
    /// Bind a loopback address on an ephemeral port and start serving.
    pub async fn start() -> std::io::Result<Self> {
        Self::start_on_port(None).await
    }

    /// Bind a loopback address on `port` (or an ephemeral port when `None`) and start serving.
    pub async fn start_on_port(port: Option<u16>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0))).await?;
        let local_addr = listener.local_addr()?;
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    accept = listener.accept() => {
                        let Ok((stream, _)) = accept else { continue };
                        tokio::spawn(handle_connection(stream));
                    }
                }
            }
        });

        Ok(Self {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
            handle: Some(handle),
        })
    }

    /// Local URL the UI is served from.
    pub fn url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    /// Stop the accept loop and wait for the task to finish.
    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

async fn handle_connection(mut stream: TcpStream) {
    let mut reader = BufReader::new(&mut stream);
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).await.is_err() {
        return;
    }

    // Drain the remaining headers so the next request on the keep-alive
    // connection starts at a fresh line. We don't parse request bodies.
    let mut header = String::new();
    loop {
        header.clear();
        match reader.read_line(&mut header).await {
            Ok(0) | Err(_) => break,
            Ok(_) if header.trim().is_empty() => break,
            Ok(_) => {}
        }
    }

    let parts: Vec<&str> = first_line.split_whitespace().collect();
    let path = parts.get(1).map_or("/", |p| *p);
    let (status, content_type, body) = route(path).await;

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn route(path: &str) -> (&'static str, &'static str, String) {
    let path = path.split('?').next().unwrap_or(path);
    if let Some(rest) = path.strip_prefix("/api/sessions/") {
        let mut segments = rest.split('/');
        let id = segments.next().unwrap_or("");
        if !is_safe_session_ref(id) {
            return not_found();
        }
        let suffix = segments.next();
        match suffix {
            None | Some("") => session_detail(id).await,
            Some("summary") => session_summary(id).await,
            Some("requests") => match segments.next() {
                Some(request_id) if !request_id.is_empty() => {
                    session_request_payload(id, request_id).await
                }
                _ => session_requests(id).await,
            },
            _ => not_found(),
        }
    } else if path == "/api/sessions" {
        list_sessions().await
    } else if let Some(asset) = path.strip_prefix("/assets/") {
        asset_response(asset)
    } else {
        spa_index()
    }
}

fn spa_index() -> (&'static str, &'static str, String) {
    ("200 OK", "text/html; charset=utf-8", INDEX_HTML.to_string())
}

fn asset_response(path: &str) -> (&'static str, &'static str, String) {
    INSPECT_ASSETS
        .iter()
        .find(|asset| asset.path == path)
        .map(|asset| ("200 OK", asset.content_type, asset.body.to_string()))
        .unwrap_or_else(not_found)
}

fn not_found() -> (&'static str, &'static str, String) {
    (
        "404 Not Found",
        "application/json",
        r#"{"error":"not found"}"#.to_string(),
    )
}

fn server_error(message: &str) -> (&'static str, &'static str, String) {
    (
        "500 Internal Server Error",
        "application/json",
        serde_json::json!({"error": message}).to_string(),
    )
}

async fn list_sessions() -> (&'static str, &'static str, String) {
    let sessions = match tokio::task::spawn_blocking(list_session_items).await {
        Ok(s) => s,
        Err(e) => return server_error(&e.to_string()),
    };
    match serde_json::to_string(&sessions) {
        Ok(body) => ("200 OK", "application/json", body),
        Err(e) => server_error(&e.to_string()),
    }
}

async fn session_detail(id: &str) -> (&'static str, &'static str, String) {
    let id = id.to_string();
    let session = match tokio::task::spawn_blocking(move || {
        crate::app::history::materialize_full_session_result(
            &id,
            crate::app::history::FullSessionMaterializationReason::InspectSessionDetail,
        )
    })
    .await
    {
        Ok(Ok(session)) => session,
        Ok(Err(smelt_core::session::SessionStoreError::SessionNotFound { .. })) => {
            return not_found();
        }
        Ok(Err(err)) => return server_error(&err.to_string()),
        Err(err) => return server_error(&err.to_string()),
    };
    match session {
        Some(session) => match serde_json::to_string(&session) {
            Ok(body) => ("200 OK", "application/json", body),
            Err(e) => server_error(&e.to_string()),
        },
        None => not_found(),
    }
}

async fn session_summary(id: &str) -> (&'static str, &'static str, String) {
    let id = id.to_string();
    let summary = match tokio::task::spawn_blocking(move || build_session_summary(&id)).await {
        Ok(Ok(summary)) => summary,
        Ok(Err(smelt_core::session::SessionStoreError::SessionNotFound { .. })) => {
            return not_found();
        }
        Ok(Err(err)) => return server_error(&err.to_string()),
        Err(err) => return server_error(&err.to_string()),
    };
    match summary {
        Some(summary) => match serde_json::to_string(&summary) {
            Ok(body) => ("200 OK", "application/json", body),
            Err(e) => server_error(&e.to_string()),
        },
        None => not_found(),
    }
}

async fn session_requests(id: &str) -> (&'static str, &'static str, String) {
    let id = id.to_string();
    let requests = match tokio::task::spawn_blocking(move || session_requests_json(&id)).await {
        Ok(result) => result,
        Err(e) => return server_error(&e.to_string()),
    };
    match requests {
        Ok(body) => ("200 OK", "application/json", body),
        Err(e) => server_error(&e),
    }
}

fn session_requests_json(id: &str) -> std::result::Result<String, String> {
    let dir = match session_dir(id) {
        Ok(dir) => dir,
        Err(smelt_core::session::SessionStoreError::SessionNotFound { .. }) => {
            return Ok("[]".to_string());
        }
        Err(err) => return Err(err.to_string()),
    };
    if let Err(err) = smelt_core::session::ensure_session_db_read_only(&dir) {
        if matches!(
            err,
            smelt_core::session::SessionStoreError::MissingDatabase { .. }
        ) {
            return Ok("[]".to_string());
        }
        return Err(err.to_string());
    }
    let db_path = dir.join("session.db");
    let db = smelt_store::SessionReader::open_database(&db_path).map_err(|err| err.to_string())?;
    let attempts = db
        .query_request_attempts(&smelt_store::RequestAuditQuery {
            limit: u32::MAX,
            order: smelt_store::RequestAuditOrder::OldestFirst,
            ..Default::default()
        })
        .map_err(|err| err.to_string())?;
    let values: Vec<serde_json::Value> = attempts.iter().map(request_summary_json).collect();
    serde_json::to_string(&values).map_err(|err| err.to_string())
}

async fn session_request_payload(
    id: &str,
    request_id: &str,
) -> (&'static str, &'static str, String) {
    let id = id.to_string();
    let request_id = request_id.to_string();
    let payload =
        match tokio::task::spawn_blocking(move || request_payload_json(&id, &request_id)).await {
            Ok(result) => result,
            Err(e) => return server_error(&e.to_string()),
        };
    match payload {
        Ok(Some(body)) => ("200 OK", "application/json", body),
        Ok(None) => not_found(),
        Err(e) => server_error(&e),
    }
}

fn request_payload_json(id: &str, request_id: &str) -> std::result::Result<Option<String>, String> {
    let attempt_id = request_id
        .parse::<i64>()
        .map_err(|err| format!("invalid request id: {err}"))?;
    let dir = match session_dir(id) {
        Ok(dir) => dir,
        Err(smelt_core::session::SessionStoreError::SessionNotFound { .. }) => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    if let Err(err) = smelt_core::session::ensure_session_db_read_only(&dir) {
        if matches!(
            err,
            smelt_core::session::SessionStoreError::MissingDatabase { .. }
        ) {
            return Ok(None);
        }
        return Err(err.to_string());
    }
    let db_path = dir.join("session.db");
    let db = smelt_store::SessionReader::open_database(&db_path).map_err(|err| err.to_string())?;
    let Some(payloads) = db
        .request_payloads(attempt_id)
        .map_err(|err| err.to_string())?
    else {
        return Ok(None);
    };
    serde_json::to_string(&serde_json::json!({
        "body": payloads.body,
        "response": payloads.response,
        "error": payloads.error,
    }))
    .map(Some)
    .map_err(|err| err.to_string())
}

fn request_summary_json(entry: &smelt_store::RequestAuditSummary) -> serde_json::Value {
    let elapsed_ms = entry
        .completed_at
        .map(|completed_at| completed_at.saturating_sub(entry.started_at));
    let error = entry.error_summary.as_ref().map(|message| {
        serde_json::json!({
            "message": message,
        })
    });
    let response = entry.response_summary.as_ref().map(|content| {
        serde_json::json!({
            "content": content,
        })
    });
    let mut value = serde_json::json!({
        "id": entry.id,
        "request_id": entry.request_id.clone(),
        "kind": entry.kind.clone(),
        "turn_id": entry.turn_id.clone(),
        "ask_id": entry.ask_id.clone(),
        "timestamp_ms": entry.started_at,
        "provider_kind": entry.provider.clone(),
        "api_base": entry.api_base.clone(),
        "model": entry.model.clone(),
        "url": entry.url.clone(),
        "http_status": entry.http_status,
        "history_len": entry.history_len,
        "prompt_cache_key": entry.prompt_cache_key.clone(),
        "stream": entry.stream,
        "usage": entry.usage.clone(),
        "cost_usd": entry.cost_usd,
        "tokens_per_sec": entry.tokens_per_sec,
        "elapsed_ms": elapsed_ms,
        "attempt": entry.attempt,
        "background": entry.background,
        "raw_body_size": entry.raw_body_size,
        "has_body": entry.body_hash.is_some(),
        "has_response": entry.response_hash.is_some(),
        "has_error": entry.error_hash.is_some(),
        "has_raw_response": entry.response_hash.is_some(),
        "response": response,
        "error": error,
    });
    remove_null_json_fields(&mut value);
    value
}

fn remove_null_json_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|_, child| !child.is_null());
            for child in map.values_mut() {
                remove_null_json_fields(child);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                remove_null_json_fields(child);
            }
        }
        _ => {}
    }
}

fn session_dir(id: &str) -> Result<PathBuf, smelt_core::session::SessionStoreError> {
    smelt_core::session::resolve_prefix(id).map(|id| smelt_core::session::session_dir(&id))
}

pub fn is_safe_session_ref(id: &str) -> bool {
    smelt_core::session_id::SessionPrefix::parse(id).is_ok()
}

fn list_session_items() -> Vec<SessionListItem> {
    smelt_core::session::list_sessions()
        .into_iter()
        .map(|meta| {
            let (project, path_group) = project_labels(meta.cwd.as_deref());
            let request_stats = request_stats_for_session(&meta.id);
            SessionListItem {
                meta,
                project,
                path_group,
                request_stats,
            }
        })
        .collect()
}

fn build_session_summary(
    id: &str,
) -> smelt_core::session::SessionStoreResult<Option<SessionSummary>> {
    let Some(meta) = smelt_core::session::load_meta_result(id)? else {
        return Ok(None);
    };
    let (project, path_group) = project_labels(meta.cwd.as_deref());
    Ok(Some(SessionSummary {
        id: meta.id.clone(),
        project,
        path_group,
        request_stats: request_stats_for_session(&meta.id),
    }))
}

fn project_labels(cwd: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(cwd) = cwd.filter(|s| !s.is_empty()) else {
        return (None, None);
    };
    let path = Path::new(cwd);
    let project = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| Some(cwd.to_string()));
    let path_group = path
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    (project, path_group)
}

fn request_stats_for_session(id: &str) -> RequestStats {
    let Ok(session_dir) = session_dir(id) else {
        return RequestStats::default();
    };
    let db_path = session_dir.join("session.db");
    smelt_store::SessionReader::open_database(&db_path)
        .and_then(|db| db.request_audit_stats())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn fetch(path: &str) -> (String, String) {
        let mut server = Server::start().await.unwrap();
        let url = server.url();
        let mut parts = url.split("//").nth(1).unwrap().split(':');
        let host = parts.next().unwrap();
        let port: u16 = parts.next().unwrap().parse().unwrap();
        let mut stream = TcpStream::connect((host, port)).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        server.stop().await;
        let text = String::from_utf8(buf).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        (head.to_string(), body.to_string())
    }

    #[tokio::test]
    async fn serves_index_html_for_root() {
        let (head, body) = fetch("/").await;
        assert!(head.contains("200 OK"), "expected 200, got: {head}");
        assert!(body.contains("Smelt Inspector"));
    }

    #[tokio::test]
    async fn api_sessions_returns_json_array() {
        let (head, body) = fetch("/api/sessions").await;
        assert!(head.contains("200 OK"), "expected 200, got: {head}");
        assert!(head.contains("application/json"));
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed.is_array());
    }

    #[tokio::test]
    async fn unknown_session_requests_returns_empty_array() {
        let (head, body) = fetch("/api/sessions/0000000000000000000000000000000000000000000000000000000000000000/requests").await;
        assert!(head.contains("200 OK"), "expected 200, got: {head}");
        assert_eq!(body, "[]");
    }

    #[tokio::test]
    async fn unknown_api_path_returns_404() {
        let (head, _) = fetch("/api/sessions/0000000000000000000000000000000000000000000000000000000000000000/unknown").await;
        assert!(head.contains("404 Not Found"), "expected 404, got: {head}");
    }
}
