//! Regression test for GitHub issue #8: orphaned `tool_call_id` produced
//! a "tool_calls must be followed by tool messages" error from
//! Kimi/anthropic-compatible providers on session resume.
//!
//! The root cause was an engine snapshot persisted mid-tool: assistant
//! `tool_calls` were committed to history *before* the matching
//! tool_results existed, and a SIGINT at that moment saved the broken
//! state to disk.
//!
//! The fix introduces `HistoryItem::Assistant(AssistantStep)` with a
//! `Vec<ToolInvocation>` whose entries each carry their own `result`.
//! That makes an unpaired tool_use unrepresentable - there is no
//! intermediate state for the persister to catch. This test verifies
//! that property end-to-end against the real event loop with a fake
//! LLM emitting a tool_use mid-turn.

use engine::EngineConfig;
use protocol::{
    AgentMode, Content, EngineEvent, HistoryItem, ModelConfig, ModelTarget, ReasoningEffort,
    RequestRuntimeConfig, StartTurnPayload, UiCommand,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn canned_sse_response() -> String {
    let events = [
        r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"web_fetch:36","name":"web_fetch","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"url\":\"https://example.com\",\"prompt\":\"x\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":12}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let mut body = String::new();
    for ev in events {
        body.push_str("data: ");
        body.push_str(ev);
        body.push_str("\n\n");
    }
    body
}

/// Returns the list of assistant invocations whose `call_id` is empty - the
/// historical orphan failure mode. With the sum-type history this should
/// always return an empty list.
fn snapshot_has_orphan(history: &[HistoryItem]) -> Option<Vec<String>> {
    let mut orphans = Vec::new();
    for item in history {
        if let HistoryItem::Assistant(turn) = item {
            for inv in &turn.invocations {
                if inv.call_id.is_empty() {
                    orphans.push(format!("{}: empty call_id", inv.name));
                }
            }
        }
    }
    if orphans.is_empty() {
        None
    } else {
        Some(orphans)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn mid_turn_messages_snapshot_never_contains_orphan_tool_call() {
    // ── Fake LLM server ────────────────────────────────────────────────
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stream_started_tx, stream_started_rx) = tokio::sync::oneshot::channel();

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        // Drain request headers.
        let mut buf = [0u8; 8192];
        let mut accumulated = Vec::new();
        loop {
            let n = match sock.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            if n == 0 {
                return;
            }
            accumulated.extend_from_slice(&buf[..n]);
            if accumulated.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        // Best-effort body drain.
        let _ = tokio::time::timeout(Duration::from_millis(20), async {
            loop {
                let mut b = [0u8; 4096];
                if sock.read(&mut b).await.unwrap_or(0) == 0 {
                    break;
                }
            }
        })
        .await;

        let body = canned_sse_response();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(header.as_bytes()).await;
        let _ = sock.write_all(body.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = stream_started_tx.send(());
        // Hold the socket so the engine doesn't see EOF on the next request.
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    // ── Engine ─────────────────────────────────────────────────────────
    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let target = ModelTarget {
        model: "test-model".into(),
        api_base: format!("http://{addr}"),
        api_key: "test-key".into(),
        provider_type: "anthropic-compatible".into(),
        config: ModelConfig {
            max_tokens: Some(4096),
            ..ModelConfig::default()
        },
    };

    let mut handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));
    drop(handle.take_host_rx());

    // Wait for Ready.
    loop {
        match handle.recv().await {
            Some(EngineEvent::Ready) => break,
            Some(_) => continue,
            None => panic!("engine closed before Ready"),
        }
    }

    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("go"), None),
        mode: AgentMode::normal(),
        model_target: target,
        request_config: RequestRuntimeConfig::default(),
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        history: protocol::ModelHistorySource::items(Vec::new()),
        session_id: "sess".into(),
        session_dir: PathBuf::from("/tmp"),
        permission_overrides: None,
        system_prompt: Some("test system".into()),
        tools: Vec::new(),
    })));

    // ── Collect every Messages snapshot + final TurnComplete payload.
    let mut bad_snapshots: Vec<(&'static str, Vec<HistoryItem>, Vec<String>)> = Vec::new();
    let mut all_snapshots: Vec<(&'static str, Vec<HistoryItem>)> = Vec::new();

    // Start the observation window when the response is available. Coverage
    // instrumentation and parallel tests can delay the provider request.
    tokio::time::timeout(Duration::from_secs(10), stream_started_rx)
        .await
        .expect("provider request timed out")
        .expect("fake server closed before streaming");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let recv = tokio::time::timeout(Duration::from_millis(300), handle.recv()).await;
        match recv {
            Ok(Some(EngineEvent::HistoryUpdated { update, .. })) => {
                if let Some(orphans) = snapshot_has_orphan(&update.items) {
                    bad_snapshots.push(("HistoryUpdated", update.items.clone(), orphans));
                }
                all_snapshots.push(("HistoryUpdated", update.items));
            }
            Ok(Some(EngineEvent::HistoryAppended { delta, .. })) => {
                if let Some(orphans) = snapshot_has_orphan(&delta.items) {
                    bad_snapshots.push(("HistoryAppended", delta.items.clone(), orphans));
                }
                all_snapshots.push(("HistoryAppended", delta.items));
            }
            Ok(Some(EngineEvent::TurnComplete { history, .. })) => {
                if let Some(history) = history {
                    if let Some(orphans) = snapshot_has_orphan(&history.items) {
                        bad_snapshots.push(("TurnComplete", history.items.clone(), orphans));
                    }
                    all_snapshots.push(("TurnComplete", history.items));
                }
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    server.abort();

    // Debug dump.
    eprintln!("── observed snapshots ──");
    for (kind, hist) in &all_snapshots {
        eprintln!("[{kind}] {} items:", hist.len());
        for (i, item) in hist.iter().enumerate() {
            match item {
                HistoryItem::System { .. } => eprintln!("  [{i}] system"),
                HistoryItem::User { .. } => eprintln!("  [{i}] user"),
                HistoryItem::Assistant(turn) => eprintln!(
                    "  [{i}] assistant invocations={:?}",
                    turn.invocations
                        .iter()
                        .map(|inv| (inv.call_id.clone(), inv.result.is_error))
                        .collect::<Vec<_>>()
                ),
                HistoryItem::Note(note) => eprintln!("  [{i}] note {:?}", note.kind()),
            }
        }
    }

    assert!(
        bad_snapshots.is_empty(),
        "engine emitted {} snapshot(s) with orphan invocations:\n{}",
        bad_snapshots.len(),
        bad_snapshots
            .iter()
            .map(|(kind, _, orphans)| format!("  {kind}: {:?}", orphans))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    // Sanity: at least one snapshot must include the tool_use the model
    // emitted, otherwise we're not actually exercising the bug surface.
    let saw_invocation = all_snapshots.iter().any(|(_, hist)| {
        hist.iter().any(|item| {
            matches!(item, HistoryItem::Assistant(t) if t.invocations.iter().any(|inv| inv.call_id == "web_fetch:36"))
        })
    });
    assert!(
        saw_invocation,
        "test never observed the web_fetch:36 tool_use - fake server / engine plumbing is broken"
    );
}
