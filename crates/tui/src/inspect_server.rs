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
    meta: smelt_core::session::SessionListMeta,
    project: Option<String>,
    path_group: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionListPage {
    sessions: Vec<SessionListItem>,
    next_cursor: Option<smelt_core::session::SessionListCursor>,
    catalog: smelt_core::session::SessionCatalogStatus,
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

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown_tx.take() {
            let _ = sender.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl Server {
    /// Bind a loopback address on an ephemeral port and start serving process storage.
    pub async fn start() -> std::io::Result<Self> {
        Self::start_with_storage(smelt_core::session::SessionStorage::new(
            smelt_core::config::state_dir(),
        ))
        .await
    }

    /// Bind a loopback address on an ephemeral port and serve explicit runtime storage.
    pub async fn start_with_storage(
        sessions: smelt_core::session::SessionStorage,
    ) -> std::io::Result<Self> {
        Self::start_on_port_with_storage(None, sessions).await
    }

    /// Bind a loopback address on `port` (or an ephemeral port when `None`) and serve process storage.
    pub async fn start_on_port(port: Option<u16>) -> std::io::Result<Self> {
        Self::start_on_port_with_storage(
            port,
            smelt_core::session::SessionStorage::new(smelt_core::config::state_dir()),
        )
        .await
    }

    /// Bind a loopback address and serve explicit runtime storage.
    pub async fn start_on_port_with_storage(
        port: Option<u16>,
        sessions: smelt_core::session::SessionStorage,
    ) -> std::io::Result<Self> {
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
                        tokio::spawn(handle_connection(stream, sessions.clone()));
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

async fn handle_connection(mut stream: TcpStream, sessions: smelt_core::session::SessionStorage) {
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
    let (status, content_type, body) = route(path, &sessions).await;

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn route(
    path: &str,
    sessions: &smelt_core::session::SessionStorage,
) -> (&'static str, &'static str, String) {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    if let Some(rest) = path.strip_prefix("/api/sessions/") {
        let mut segments = rest.split('/');
        let id = segments.next().unwrap_or("");
        if !is_safe_session_ref(id) {
            return not_found();
        }
        let suffix = segments.next();
        let resource = segments.next();
        if segments.next().is_some() {
            return not_found();
        }
        match (suffix, resource) {
            (None | Some(""), None) => session_detail(sessions, id).await,
            (Some("summary"), None) => session_summary(sessions, id).await,
            (Some("requests"), None | Some("")) => session_requests(sessions, id).await,
            (Some("requests"), Some(request_id)) => {
                session_request_payload(sessions, id, request_id).await
            }
            _ => not_found(),
        }
    } else if path == "/api/sessions" {
        list_sessions(sessions, query).await
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

fn bad_request(message: &str) -> (&'static str, &'static str, String) {
    (
        "400 Bad Request",
        "application/json",
        serde_json::json!({"error": message}).to_string(),
    )
}

fn server_error(message: &str) -> (&'static str, &'static str, String) {
    (
        "500 Internal Server Error",
        "application/json",
        serde_json::json!({"error": message}).to_string(),
    )
}

async fn list_sessions(
    sessions: &smelt_core::session::SessionStorage,
    query: &str,
) -> (&'static str, &'static str, String) {
    let query = match parse_session_list_query(query) {
        Ok(query) => query,
        Err(error) => return bad_request(&error),
    };
    let sessions = sessions.clone();
    let page = match tokio::task::spawn_blocking(move || list_session_items(&sessions, query)).await
    {
        Ok(Ok(page)) => page,
        Ok(Err(smelt_core::session::SessionStoreError::InvalidListQuery { message })) => {
            return bad_request(&message);
        }
        Ok(Err(error)) => return server_error(&error.to_string()),
        Err(error) => return server_error(&error.to_string()),
    };
    match serde_json::to_string(&page) {
        Ok(body) => ("200 OK", "application/json", body),
        Err(error) => server_error(&error.to_string()),
    }
}

fn parse_session_list_query(query: &str) -> Result<smelt_core::session::SessionListQuery, String> {
    let mut limit = 200;
    let mut cursor_updated_at = None;
    let mut cursor_id = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "limit" => {
                limit = value
                    .parse::<u32>()
                    .map_err(|error| format!("invalid session page limit: {error}"))?;
            }
            "cursor_updated_at_ms" => {
                cursor_updated_at = Some(
                    value
                        .parse::<u64>()
                        .map_err(|error| format!("invalid session cursor update time: {error}"))?,
                );
            }
            "cursor_id" => cursor_id = Some(value.to_string()),
            _ => {}
        }
    }
    let cursor = match (cursor_updated_at, cursor_id) {
        (None, None) => None,
        (Some(updated_at_ms), Some(id)) => {
            smelt_core::session_id::SessionId::parse(&id)
                .map_err(|error| format!("invalid session cursor id: {error}"))?;
            Some(smelt_core::session::SessionListCursor { updated_at_ms, id })
        }
        _ => return Err("session cursor requires both update time and id".into()),
    };
    Ok(smelt_core::session::SessionListQuery {
        limit,
        cursor,
        cwd: None,
        availability: Some(smelt_core::session::SessionListAvailability::Available),
    })
}

async fn session_detail(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
) -> (&'static str, &'static str, String) {
    let id = id.to_string();
    let sessions = sessions.clone();
    let session = match tokio::task::spawn_blocking(move || {
        smelt_perf::perf::record_value("session:full_materialized", 1);
        smelt_perf::perf::record_value("inspect:session:detail_load_full", 1);
        sessions.load_full_result(&id)
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

async fn session_summary(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
) -> (&'static str, &'static str, String) {
    let id = id.to_string();
    let sessions = sessions.clone();
    let summary =
        match tokio::task::spawn_blocking(move || build_session_summary(&sessions, &id)).await {
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

async fn session_requests(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
) -> (&'static str, &'static str, String) {
    let id = id.to_string();
    let sessions = sessions.clone();
    let requests =
        match tokio::task::spawn_blocking(move || session_requests_json(&sessions, &id)).await {
            Ok(result) => result,
            Err(e) => return server_error(&e.to_string()),
        };
    match requests {
        Ok(Some(body)) => ("200 OK", "application/json", body),
        Ok(None) => not_found(),
        Err(e) => server_error(&e),
    }
}

fn session_requests_json(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
) -> std::result::Result<Option<String>, String> {
    let dir = match session_dir(sessions, id) {
        Ok(dir) => dir,
        Err(smelt_core::session::SessionStoreError::SessionNotFound { .. }) => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    let query = smelt_store::RequestAuditQuery {
        limit: u32::MAX,
        order: smelt_store::RequestAuditOrder::OldestFirst,
        ..Default::default()
    };
    let attempts = if let Some(reader) =
        smelt_store::LineageSessionReader::try_open_existing(sessions.sessions_dir(), id)
            .map_err(|err| err.to_string())?
    {
        reader
            .query_request_attempts(&query)
            .map_err(|err| err.to_string())?
    } else {
        // COMPAT(session-lineage-v1): inspection remains read-only for
        // previous-format sessions until explicit migration.
        sessions
            .ensure_session_db_read_only(&dir)
            .map_err(|err| err.to_string())?;
        let db_path = dir.join("session.db");
        smelt_store::SessionReader::open_database(&db_path)
            .map_err(|err| err.to_string())?
            .query_request_attempts(&query)
            .map_err(|err| err.to_string())?
    };
    let values: Vec<serde_json::Value> = attempts.iter().map(request_summary_json).collect();
    serde_json::to_string(&values)
        .map(Some)
        .map_err(|err| err.to_string())
}

async fn session_request_payload(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
    request_id: &str,
) -> (&'static str, &'static str, String) {
    let attempt_id = match request_id.parse::<i64>() {
        Ok(attempt_id) if attempt_id > 0 => attempt_id,
        Ok(_) => return bad_request("request id must be positive"),
        Err(error) => return bad_request(&format!("invalid request id: {error}")),
    };
    let id = id.to_string();
    let sessions = sessions.clone();
    let payload =
        match tokio::task::spawn_blocking(move || request_payload_json(&sessions, &id, attempt_id))
            .await
        {
            Ok(result) => result,
            Err(e) => return server_error(&e.to_string()),
        };
    match payload {
        Ok(Some(body)) => ("200 OK", "application/json", body),
        Ok(None) => not_found(),
        Err(e) => server_error(&e),
    }
}

fn request_payload_json(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
    attempt_id: i64,
) -> std::result::Result<Option<String>, String> {
    let dir = match session_dir(sessions, id) {
        Ok(dir) => dir,
        Err(smelt_core::session::SessionStoreError::SessionNotFound { .. }) => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    let payloads = if let Some(reader) =
        smelt_store::LineageSessionReader::try_open_existing(sessions.sessions_dir(), id)
            .map_err(|err| err.to_string())?
    {
        reader
            .request_payloads(attempt_id)
            .map_err(|err| err.to_string())?
    } else {
        // COMPAT(session-lineage-v1): payload inspection remains read-only for
        // previous-format sessions until explicit migration.
        sessions
            .ensure_session_db_read_only(&dir)
            .map_err(|err| err.to_string())?;
        let db_path = dir.join("session.db");
        smelt_store::SessionReader::open_database(&db_path)
            .map_err(|err| err.to_string())?
            .request_payloads(attempt_id)
            .map_err(|err| err.to_string())?
    };
    let Some(payloads) = payloads else {
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

fn session_dir(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
) -> Result<PathBuf, smelt_core::session::SessionStoreError> {
    sessions
        .resolve_prefix(id)
        .map(|id| sessions.session_dir(&id))
}

pub fn is_safe_session_ref(id: &str) -> bool {
    smelt_core::session_id::SessionPrefix::parse(id).is_ok()
}

fn list_session_items(
    sessions: &smelt_core::session::SessionStorage,
    query: smelt_core::session::SessionListQuery,
) -> smelt_core::session::SessionStoreResult<SessionListPage> {
    let page = sessions.list_session_page_result(query)?;
    let sessions = page
        .entries
        .into_iter()
        .filter_map(|entry| match entry.status {
            smelt_core::session::SessionListStatus::Available(meta) => Some(*meta),
            smelt_core::session::SessionListStatus::Upgradeable { .. }
            | smelt_core::session::SessionListStatus::Unavailable(_) => None,
        })
        .map(|meta| {
            let (project, path_group) = project_labels(meta.cwd.as_deref());
            SessionListItem {
                meta,
                project,
                path_group,
            }
        })
        .collect();
    Ok(SessionListPage {
        sessions,
        next_cursor: page.next_cursor,
        catalog: page.catalog,
    })
}

fn build_session_summary(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
) -> smelt_core::session::SessionStoreResult<Option<SessionSummary>> {
    let Some(meta) = sessions.load_meta_result(id)? else {
        return Ok(None);
    };
    let (project, path_group) = project_labels(meta.cwd.as_deref());
    Ok(Some(SessionSummary {
        id: meta.id.clone(),
        project,
        path_group,
        request_stats: request_stats_for_session(sessions, &meta.id),
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

fn request_stats_for_session(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
) -> RequestStats {
    let Ok(session_dir) = session_dir(sessions, id) else {
        return RequestStats::default();
    };
    match smelt_store::LineageSessionReader::try_open_existing(sessions.sessions_dir(), id) {
        Ok(Some(reader)) => reader.request_audit_stats().unwrap_or_default(),
        Ok(None) => {
            // COMPAT(session-lineage-v1): inspector maintenance remains able to
            // summarize the immediately preceding format during migration.
            smelt_store::SessionReader::open_database(session_dir.join("session.db"))
                .and_then(|db| db.request_audit_stats())
                .unwrap_or_default()
        }
        Err(_) => RequestStats::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Content, HistoryItem};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const OLDER_SESSION_ID: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";
    const NEWER_SESSION_ID: &str =
        "2222222222222222222222222222222222222222222222222222222222222222";
    const MISSING_SESSION_ID: &str =
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    struct IsolatedState {
        sessions: smelt_core::session::SessionStorage,
        _directory: tempfile::TempDir,
    }

    impl IsolatedState {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("temporary state directory");
            let sessions = smelt_core::session::SessionStorage::new(directory.path().join("smelt"));
            Self {
                sessions,
                _directory: directory,
            }
        }
    }

    async fn fetch(sessions: &smelt_core::session::SessionStorage, path: &str) -> (String, String) {
        let mut server = Server::start_with_storage(sessions.clone()).await.unwrap();
        let mut stream = TcpStream::connect(server.local_addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        server.stop().await;
        let text = String::from_utf8(buf).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        (head.to_string(), body.to_string())
    }

    fn assert_status(head: &str, expected: &str) {
        assert!(
            head.contains(expected),
            "expected {expected}, got response headers: {head}"
        );
    }

    fn seed_session(
        sessions: &smelt_core::session::SessionStorage,
        id: &str,
        title: &str,
        cwd: &str,
        updated_at_ms: u64,
    ) {
        let mut session = smelt_core::session::Session::new(1, PathBuf::from(cwd));
        session.id = id.to_string();
        session.title = Some(title.to_string());
        session.first_user_message = Some(format!("message for {title}"));
        session.created_at_ms = updated_at_ms.saturating_sub(1);
        session.updated_at_ms = updated_at_ms;
        session
            .history
            .push(HistoryItem::user(Content::text(format!(
                "message for {title}"
            ))));
        sessions
            .save_result(&session)
            .expect("save canonical session");
    }

    fn append_request_audit(sessions: &smelt_core::session::SessionStorage, id: &str) -> i64 {
        let sessions_dir = sessions.sessions_dir();
        let mut writer = smelt_store::OwnedLineageWriter::open_existing(sessions_dir, id)
            .expect("open canonical lineage writer");
        let attempt_id = writer
            .append_request_attempt(
                &protocol::request_log::RequestLogEntry {
                    request_id: 42,
                    kind: "turn".into(),
                    turn_id: Some(7),
                    ask_id: None,
                    history_len: Some(1),
                    timestamp_ms: 1_000,
                    provider_kind: "openai".into(),
                    api_base: "https://api.example.test".into(),
                    model: "model-a".into(),
                    url: "https://api.example.test/v1/chat/completions".into(),
                    http_status: Some(200),
                    body: serde_json::json!({"model": "model-a", "prompt": "hello"}),
                    prompt_cache_key: None,
                    stream: true,
                    system_prompt: None,
                    messages: None,
                    tools: None,
                    response: Some(protocol::request_log::RequestResponse {
                        content: Some("world".into()),
                        reasoning: None,
                        tool_calls: None,
                        raw: Some(serde_json::json!({"id": "response-1"})),
                    }),
                    usage: Some(protocol::TokenUsage {
                        prompt_tokens: Some(10),
                        completion_tokens: Some(5),
                        ..Default::default()
                    }),
                    cost_usd: Some(0.001),
                    tokens_per_sec: Some(20.0),
                    elapsed_ms: Some(250),
                    attempt: 1,
                    error: None,
                    background: false,
                },
                smelt_store::RequestAuditPayloadMode::Full,
            )
            .expect("append request audit");
        writer.release().expect("release canonical session writer");
        attempt_id
    }

    #[tokio::test]
    async fn serves_index_html_and_static_assets() {
        let state = IsolatedState::new();
        let (head, body) = fetch(&state.sessions, "/").await;
        assert_status(&head, "200 OK");
        assert!(body.contains("Smelt Inspector"));

        let (head, body) = fetch(&state.sessions, "/assets/style.css").await;
        assert_status(&head, "200 OK");
        assert!(head.contains("text/css"));
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn session_list_paginates_canonical_sessions_with_stable_cursors() {
        let state = IsolatedState::new();
        seed_session(
            &state.sessions,
            OLDER_SESSION_ID,
            "older",
            "/work/older",
            100,
        );
        seed_session(
            &state.sessions,
            NEWER_SESSION_ID,
            "newer",
            "/work/newer",
            200,
        );

        let (head, body) = fetch(&state.sessions, "/api/sessions?limit=1").await;
        assert_status(&head, "200 OK");
        assert!(head.contains("application/json"));
        let first: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(first["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(first["sessions"][0]["id"], NEWER_SESSION_ID);
        assert_eq!(first["sessions"][0]["project"], "newer");
        assert!(first["catalog"]["state"].is_string());
        let cursor = &first["next_cursor"];
        let next_path = format!(
            "/api/sessions?limit=1&cursor_updated_at_ms={}&cursor_id={}",
            cursor["updated_at_ms"].as_u64().unwrap(),
            cursor["id"].as_str().unwrap()
        );

        let (head, body) = fetch(&state.sessions, &next_path).await;
        assert_status(&head, "200 OK");
        let second: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(second["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(second["sessions"][0]["id"], OLDER_SESSION_ID);
        assert!(second["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn detail_and_summary_are_built_from_canonical_storage() {
        let state = IsolatedState::new();
        seed_session(
            &state.sessions,
            NEWER_SESSION_ID,
            "canonical",
            "/workspace/canonical",
            200,
        );
        append_request_audit(&state.sessions, NEWER_SESSION_ID);

        let (head, body) = fetch(
            &state.sessions,
            &format!("/api/sessions/{NEWER_SESSION_ID}"),
        )
        .await;
        assert_status(&head, "200 OK");
        let detail: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(detail["id"], NEWER_SESSION_ID);
        assert_eq!(detail["title"], "canonical");
        assert_eq!(detail["history"].as_array().unwrap().len(), 1);

        let (head, body) = fetch(
            &state.sessions,
            &format!("/api/sessions/{NEWER_SESSION_ID}/summary"),
        )
        .await;
        assert_status(&head, "200 OK");
        let summary: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(summary["id"], NEWER_SESSION_ID);
        assert_eq!(summary["project"], "canonical");
        assert_eq!(summary["path_group"], "/workspace");
        assert_eq!(summary["request_stats"]["request_count"], 1);
        assert_eq!(summary["request_stats"]["total_prompt_tokens"], 10);
    }

    #[tokio::test]
    async fn request_list_and_payload_round_trip_canonical_audit_data() {
        let state = IsolatedState::new();
        seed_session(
            &state.sessions,
            NEWER_SESSION_ID,
            "requests",
            "/work/requests",
            200,
        );
        let attempt_id = append_request_audit(&state.sessions, NEWER_SESSION_ID);

        let (head, body) = fetch(
            &state.sessions,
            &format!("/api/sessions/{NEWER_SESSION_ID}/requests"),
        )
        .await;
        assert_status(&head, "200 OK");
        let requests: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(requests.as_array().unwrap().len(), 1);
        assert_eq!(requests[0]["id"], attempt_id);
        assert_eq!(requests[0]["request_id"], "42");
        assert_eq!(requests[0]["provider_kind"], "openai");
        assert_eq!(requests[0]["elapsed_ms"], 250);
        assert_eq!(requests[0]["has_body"], true);
        assert_eq!(requests[0]["response"]["content"], "world");

        let (head, body) = fetch(
            &state.sessions,
            &format!("/api/sessions/{NEWER_SESSION_ID}/requests/{attempt_id}"),
        )
        .await;
        assert_status(&head, "200 OK");
        let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(payload["body"]["prompt"], "hello");
        assert_eq!(payload["response"]["raw"]["id"], "response-1");
        assert!(payload["error"].is_null());
    }

    #[tokio::test]
    async fn malformed_queries_and_request_ids_return_bad_request() {
        let state = IsolatedState::new();
        for path in [
            "/api/sessions?limit=not-a-number",
            "/api/sessions?limit=0",
            "/api/sessions?cursor_updated_at_ms=1",
            "/api/sessions?cursor_updated_at_ms=not-a-number&cursor_id=1111111111111111111111111111111111111111111111111111111111111111",
            "/api/sessions?cursor_updated_at_ms=1&cursor_id=invalid",
            "/api/sessions/1111111111111111111111111111111111111111111111111111111111111111/requests/not-a-number",
            "/api/sessions/1111111111111111111111111111111111111111111111111111111111111111/requests/0",
        ] {
            let (head, body) = fetch(&state.sessions, path).await;
            assert_status(&head, "400 Bad Request");
            assert!(serde_json::from_str::<serde_json::Value>(&body).is_ok());
        }
    }

    #[tokio::test]
    async fn invalid_and_missing_resources_return_not_found() {
        let state = IsolatedState::new();
        for path in [
            "/api/sessions/not-a-session",
            "/api/sessions/../summary",
            "/api/sessions/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "/api/sessions/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff/summary",
            "/api/sessions/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff/requests",
            "/api/sessions/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff/requests/1",
            "/api/sessions/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff/requests/1/extra",
        ] {
            let (head, body) = fetch(&state.sessions, path).await;
            assert_status(&head, "404 Not Found");
            assert_eq!(body, r#"{"error":"not found"}"#);
        }
    }

    #[tokio::test]
    async fn corrupt_canonical_storage_is_reported_as_unavailable() {
        let state = IsolatedState::new();
        seed_session(
            &state.sessions,
            NEWER_SESSION_ID,
            "corrupt",
            "/work/corrupt",
            200,
        );
        let lineage = smelt_store::LineageSessionReader::open_existing(
            state.sessions.sessions_dir(),
            NEWER_SESSION_ID,
        )
        .unwrap();
        let database_path = lineage.database_path().to_path_buf();
        drop(lineage);
        std::fs::write(database_path, b"not a sqlite database").unwrap();

        for path in [
            format!("/api/sessions/{NEWER_SESSION_ID}"),
            format!("/api/sessions/{NEWER_SESSION_ID}/summary"),
            format!("/api/sessions/{NEWER_SESSION_ID}/requests"),
            format!("/api/sessions/{NEWER_SESSION_ID}/requests/1"),
        ] {
            let (head, body) = fetch(&state.sessions, &path).await;
            assert_status(&head, "500 Internal Server Error");
            assert!(serde_json::from_str::<serde_json::Value>(&body).is_ok());
        }
    }

    #[tokio::test]
    async fn unknown_api_path_returns_404() {
        let state = IsolatedState::new();
        let (head, _) = fetch(
            &state.sessions,
            &format!("/api/sessions/{MISSING_SESSION_ID}/unknown"),
        )
        .await;
        assert_status(&head, "404 Not Found");
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_releases_the_listener() {
        let state = IsolatedState::new();
        let mut server = Server::start_with_storage(state.sessions).await.unwrap();
        let addr = server.local_addr;
        TcpStream::connect(addr)
            .await
            .expect("inspect listener accepts connections while running");

        server.stop().await;
        server.stop().await;

        let rebound = TcpListener::bind(addr)
            .await
            .expect("stopped inspect server releases its listener");
        drop(rebound);
    }
}
