use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

// ── Wire protocol types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireMessage {
    Message {
        from_id: String,
        from_slug: String,
        message: String,
    },
    Query {
        from_id: String,
        question: String,
    },
    QueryResult {
        answer: String,
    },
    QueryError {
        error: String,
    },
    PermissionCheck {
        from_id: String,
        tool_name: String,
        args: std::collections::HashMap<String, serde_json::Value>,
        confirm_message: String,
        approval_patterns: Vec<String>,
        summary: Option<String>,
    },
    PermissionVerdict {
        approved: bool,
        message: Option<String>,
    },
}

// ── Incoming message (delivered to the engine) ──────────────────────────────

#[derive(Debug)]
pub enum IncomingMessage {
    Message {
        from_id: String,
        from_slug: String,
        message: String,
    },
    Query {
        from_id: String,
        question: String,
        reply_tx: tokio::sync::oneshot::Sender<String>,
    },
    PermissionCheck {
        from_id: String,
        tool_name: String,
        args: std::collections::HashMap<String, serde_json::Value>,
        confirm_message: String,
        approval_patterns: Vec<String>,
        summary: Option<String>,
        reply_tx: tokio::sync::oneshot::Sender<PermissionReply>,
    },
}

#[derive(Debug)]
pub struct PermissionReply {
    pub approved: bool,
    pub message: Option<String>,
}

/// Payload for `send_permission_check`.
pub struct PermissionCheckRequest<'a> {
    pub from_id: &'a str,
    pub tool_name: &'a str,
    pub args: &'a std::collections::HashMap<String, serde_json::Value>,
    pub confirm_message: &'a str,
    pub approval_patterns: &'a [String],
    pub summary: Option<&'a str>,
}

// ── Socket listener ─────────────────────────────────────────────────────────

/// Start listening on a Unix domain socket. Returns the socket path and a
/// receiver for incoming messages. The listener runs in a background tokio task.
pub fn start_listener(
    pid: u32,
) -> std::io::Result<(PathBuf, mpsc::UnboundedReceiver<IncomingMessage>)> {
    let dir = crate::paths::state_dir().join("sockets");
    std::fs::create_dir_all(&dir)?;

    let socket_path = dir.join(format!("{pid}.sock"));

    // Clean up stale socket from a previous run.
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)?;
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(accept_loop(listener, tx));

    Ok((socket_path, rx))
}

async fn accept_loop(listener: UnixListener, tx: mpsc::UnboundedSender<IncomingMessage>) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(_) => break,
        };
        let tx = tx.clone();
        tokio::spawn(handle_connection(stream, tx));
    }
}

async fn write_response(writer: &mut OwnedWriteHalf, msg: &WireMessage) {
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = writer.write_all(json.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
        let _ = writer.flush().await;
    }
}

async fn handle_connection(stream: UnixStream, tx: mpsc::UnboundedSender<IncomingMessage>) {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    if buf_reader.read_line(&mut line).await.is_err() {
        return;
    }

    let msg: WireMessage = match serde_json::from_str(line.trim()) {
        Ok(m) => m,
        Err(_) => return,
    };

    match msg {
        WireMessage::Message {
            from_id,
            from_slug,
            message,
        } => {
            let _ = tx.send(IncomingMessage::Message {
                from_id,
                from_slug,
                message,
            });
        }
        WireMessage::Query { from_id, question } => {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(IncomingMessage::Query {
                from_id,
                question,
                reply_tx,
            });

            // Wait for the answer (btw-style call runs in the background).
            let response =
                match tokio::time::timeout(std::time::Duration::from_secs(30), reply_rx).await {
                    Ok(Ok(answer)) => WireMessage::QueryResult { answer },
                    Ok(Err(_)) => WireMessage::QueryError {
                        error: "query handler dropped".into(),
                    },
                    Err(_) => WireMessage::QueryError {
                        error: "query timed out".into(),
                    },
                };

            write_response(&mut writer, &response).await;
        }
        WireMessage::PermissionCheck {
            from_id,
            tool_name,
            args,
            confirm_message,
            approval_patterns,
            summary,
        } => {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(IncomingMessage::PermissionCheck {
                from_id,
                tool_name,
                args,
                confirm_message,
                approval_patterns,
                summary,
                reply_tx,
            });

            let response =
                match tokio::time::timeout(std::time::Duration::from_secs(600), reply_rx).await {
                    Ok(Ok(reply)) => WireMessage::PermissionVerdict {
                        approved: reply.approved,
                        message: reply.message,
                    },
                    Ok(Err(_)) => WireMessage::PermissionVerdict {
                        approved: false,
                        message: Some("permission handler dropped".into()),
                    },
                    Err(_) => WireMessage::PermissionVerdict {
                        approved: false,
                        message: Some("permission check timed out".into()),
                    },
                };

            write_response(&mut writer, &response).await;
        }
        // Responses are only sent by the listener, never received.
        WireMessage::QueryResult { .. }
        | WireMessage::QueryError { .. }
        | WireMessage::PermissionVerdict { .. } => {}
    }
}

// ── Socket client ───────────────────────────────────────────────────────────

async fn connect(socket_path: &Path) -> Result<UnixStream, String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        UnixStream::connect(socket_path),
    )
    .await
    .map_err(|_| "connection timed out".to_string())?
    .map_err(|e| format!("connection failed: {e}"))
}

async fn send_and_recv(
    stream: UnixStream,
    msg: &WireMessage,
    timeout_secs: u64,
) -> Result<WireMessage, String> {
    let mut buf = serde_json::to_string(msg).map_err(|e| e.to_string())?;
    buf.push('\n');

    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(buf.as_bytes())
        .await
        .map_err(|e| format!("write failed: {e}"))?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        buf_reader.read_line(&mut line),
    )
    .await
    .map_err(|_| format!("timed out ({timeout_secs}s)"))?
    .map_err(|e| format!("read failed: {e}"))?;

    serde_json::from_str(line.trim()).map_err(|e| format!("parse error: {e}"))
}

async fn fire_and_forget(stream: UnixStream, msg: &WireMessage) -> Result<(), String> {
    let mut buf = serde_json::to_string(msg).map_err(|e| e.to_string())?;
    buf.push('\n');

    let (_, mut writer) = stream.into_split();
    writer
        .write_all(buf.as_bytes())
        .await
        .map_err(|e| format!("write failed: {e}"))
}

/// Send a message to a target agent. Fire-and-forget.
pub async fn send_message(
    socket_path: &Path,
    from_id: &str,
    from_slug: &str,
    message: &str,
) -> Result<(), String> {
    let stream = connect(socket_path).await?;
    fire_and_forget(
        stream,
        &WireMessage::Message {
            from_id: from_id.to_string(),
            from_slug: from_slug.to_string(),
            message: message.to_string(),
        },
    )
    .await
}

/// Send a query to a target agent and wait for the answer.
pub(crate) async fn send_query(
    socket_path: &Path,
    from_id: &str,
    question: &str,
) -> Result<String, String> {
    let stream = connect(socket_path).await?;
    let response = send_and_recv(
        stream,
        &WireMessage::Query {
            from_id: from_id.to_string(),
            question: question.to_string(),
        },
        30,
    )
    .await?;

    match response {
        WireMessage::QueryResult { answer } => Ok(answer),
        WireMessage::QueryError { error } => Err(error),
        _ => Err("unexpected response type".into()),
    }
}

/// Send a permission check to the parent and wait for the verdict.
pub async fn send_permission_check(
    socket_path: &Path,
    req: &PermissionCheckRequest<'_>,
) -> Result<PermissionReply, String> {
    let stream = connect(socket_path).await?;
    // Client timeout shorter than server's 600s to avoid race.
    let response = send_and_recv(
        stream,
        &WireMessage::PermissionCheck {
            from_id: req.from_id.to_string(),
            tool_name: req.tool_name.to_string(),
            args: req.args.clone(),
            confirm_message: req.confirm_message.to_string(),
            approval_patterns: req.approval_patterns.to_vec(),
            summary: req.summary.map(|s| s.to_string()),
        },
        590,
    )
    .await?;

    match response {
        WireMessage::PermissionVerdict { approved, message } => {
            Ok(PermissionReply { approved, message })
        }
        _ => Err("unexpected response type".into()),
    }
}

/// Clean up the socket file for this PID.
pub(crate) fn cleanup_socket(pid: u32) {
    let path = crate::paths::state_dir()
        .join("sockets")
        .join(format!("{pid}.sock"));
    let _ = std::fs::remove_file(path);
}
