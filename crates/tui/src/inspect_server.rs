//! Local HTTP server for the `/inspect` session introspection UI.
//!
//! Serves an embedded single-page application plus a small read-only JSON
//! API backed by `smelt_core::session`. The engine writes `requests.jsonl`
//! directly; this module only reads it.

use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const INDEX_HTML: &str = include_str!("inspect.html");

/// Handle to the running inspect server.
pub struct Server {
    local_addr: std::net::SocketAddr,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Server {
    /// Bind a loopback address on an ephemeral port and start serving.
    pub async fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
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
    if let Some(rest) = path.strip_prefix("/api/sessions/") {
        let mut segments = rest.split('/');
        let id = segments.next().unwrap_or("");
        let suffix = segments.next();
        match suffix {
            None | Some("") => session_detail(id).await,
            Some("requests") => session_requests(id).await,
            _ => not_found(),
        }
    } else if path == "/api/sessions" {
        list_sessions().await
    } else {
        spa_index()
    }
}

fn spa_index() -> (&'static str, &'static str, String) {
    ("200 OK", "text/html; charset=utf-8", INDEX_HTML.to_string())
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
    let sessions = match tokio::task::spawn_blocking(smelt_core::session::list_sessions).await {
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
    let session = match tokio::task::spawn_blocking(move || smelt_core::session::load(&id)).await {
        Ok(s) => s,
        Err(e) => return server_error(&e.to_string()),
    };
    match session {
        Some(session) => match serde_json::to_string(&session) {
            Ok(body) => ("200 OK", "application/json", body),
            Err(e) => server_error(&e.to_string()),
        },
        None => not_found(),
    }
}

async fn session_requests(id: &str) -> (&'static str, &'static str, String) {
    let path = session_dir(id).join("requests.jsonl");
    if !path.exists() {
        return ("200 OK", "application/json", "[]".to_string());
    }
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => {
            let lines: Vec<serde_json::Value> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            match serde_json::to_string(&lines) {
                Ok(body) => ("200 OK", "application/json", body),
                Err(e) => server_error(&e.to_string()),
            }
        }
        Err(e) => server_error(&e.to_string()),
    }
}

fn session_dir(id: &str) -> PathBuf {
    engine::state_dir().join("sessions").join(id)
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
