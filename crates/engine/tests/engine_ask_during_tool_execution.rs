//! Regression test: `UiCommand::EngineAsk` must not be silently dropped
//! while the engine is in `execute_concurrent`.
//!
//! Several built-in Lua tools (notably `web_fetch`) park their coroutine
//! on `smelt.engine.ask{...}` while they're still executing. The Lua
//! call lowers to `UiCommand::EngineAsk`. The engine processes that
//! command everywhere *except* inside `execute_concurrent`'s
//! `tokio::select!`, where it falls into the catch-all `_ => {}` arm
//! and is discarded. Result: the EngineAsk never fires its provider
//! request, no `EngineAskResponse` event ever comes back, the Lua
//! coroutine never resumes, the tool never sends `ToolResult`, the
//! turn never completes. A permanent deadlock for any tool that parks
//! on `smelt.engine.ask`.
//!
//! `call_llm` does the right thing at `agent.rs:1868`:
//! `other => { self.handle_background_cmd(other); }`. The fix is to
//! mirror that in `execute_concurrent`'s catch-all.

use engine::{ApiConfig, EngineConfig, ModelConfig};
use protocol::{
    AgentMode, Content, EngineEvent, Message, ReasoningEffort, StartTurnPayload, ToolDef,
    ToolExecutionMode, ToolHookFlags, UiCommand,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOOL_NAME: &str = "needs_engine_ask";
const TOOL_CALL_ID: &str = "needs_engine_ask:1";

fn primary_turn_sse() -> String {
    let block_start = format!(
        r#"{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"{TOOL_CALL_ID}","name":"{TOOL_NAME}","input":{{}}}}}}"#
    );
    let events: Vec<&str> = vec![
        r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        &block_start,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":12}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    sse(&events)
}

fn aux_text_sse() -> String {
    let events: Vec<&str> = vec![
        r#"{"type":"message_start","message":{"id":"m2","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"sub-answer"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    sse(&events)
}

fn sse(events: &[&str]) -> String {
    let mut body = String::new();
    for ev in events {
        body.push_str("data: ");
        body.push_str(ev);
        body.push_str("\n\n");
    }
    body
}

/// Accepts connections in a loop. Request #0 is the main turn (returns
/// a tool_use); every subsequent request is treated as an EngineAsk and
/// gets a plain text response.
async fn run_server(listener: TcpListener, counter: Arc<AtomicUsize>) {
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        let counter = counter.clone();
        tokio::spawn(async move {
            // Drain request headers.
            let mut buf = [0u8; 8192];
            let mut accumulated: Vec<u8> = Vec::new();
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
            // Best-effort drain of any request body.
            let _ = tokio::time::timeout(Duration::from_millis(20), async {
                loop {
                    let mut b = [0u8; 4096];
                    if sock.read(&mut b).await.unwrap_or(0) == 0 {
                        break;
                    }
                }
            })
            .await;

            let n = counter.fetch_add(1, Ordering::SeqCst);
            let body = if n == 0 {
                primary_turn_sse()
            } else {
                aux_text_sse()
            };
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(header.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
            let _ = sock.flush().await;
        });
    }
}

#[tokio::test(flavor = "current_thread")]
async fn engine_ask_during_tool_execution_is_not_silently_dropped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let req_counter = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(run_server(listener, req_counter.clone()));

    let config = EngineConfig {
        api: ApiConfig {
            base: format!("http://{addr}"),
            key: "test-key".into(),
            key_env: "TEST_KEY".into(),
            provider_type: "anthropic-compatible".into(),
            model_config: ModelConfig::default(),
        },
        model: "test-model".into(),
        instructions: None,
        system_prompt_override: Some("test system".into()),
        cwd: PathBuf::from("/tmp"),
        skill_section: None,
        redact_secrets: false,
        cache_ttl_long: false,
        clock: Arc::new(engine::clock::RealClock),
    };

    let mut handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));
    drop(handle.take_host_rx());

    loop {
        match handle.recv().await {
            Some(EngineEvent::Ready) => break,
            Some(_) => continue,
            None => panic!("engine closed before Ready"),
        }
    }

    // Register the tool as a Lua-defined tool with no hooks - classify
    // pushes it straight to `pending_tools` and emits ToolDispatch.
    let tool = ToolDef {
        name: TOOL_NAME.into(),
        description: "calls smelt.engine.ask while executing".into(),
        parameters: serde_json::json!({"type":"object","properties":{}}),
        modes: None,
        execution_mode: ToolExecutionMode::Concurrent,
        override_core: false,
        hooks: ToolHookFlags::default(),
    };

    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        content: Content::text("go"),
        mode: AgentMode::normal(),
        model: "test-model".into(),
        reasoning_effort: ReasoningEffort::Off,
        history: Vec::new(),
        api_base: None,
        api_key: None,
        session_id: "sess".into(),
        session_dir: PathBuf::from("/tmp"),
        model_config_overrides: None,
        permission_overrides: None,
        system_prompt: Some("test system".into()),
        tools: vec![tool],
    })));

    // Pose as the Lua host: on ToolDispatch, simulate web_fetch.lua by
    // firing UiCommand::EngineAsk and waiting for EngineAskResponse
    // before sending ToolResult. With the current `_ => {}` catch-all in
    // execute_concurrent the EngineAsk is dropped, no response ever
    // arrives, and we hit the deadline with `got_response = false`.
    let ask_id: u64 = 0xDEAD_BEEF;
    let mut sent_engine_ask = false;
    let mut got_response = false;
    let mut tool_dispatch_request_id: Option<u64> = None;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let recv = tokio::time::timeout(Duration::from_millis(200), handle.recv()).await;
        match recv {
            Ok(Some(EngineEvent::ToolDispatch {
                request_id,
                call_id,
                ..
            })) => {
                assert_eq!(call_id, TOOL_CALL_ID);
                tool_dispatch_request_id = Some(request_id);
                if !sent_engine_ask {
                    sent_engine_ask = true;
                    handle.send(UiCommand::EngineAsk {
                        id: ask_id,
                        system: "sub-system".into(),
                        messages: vec![Message::user(Content::text("sub-q"))],
                        model: None,
                        response_format: None,
                        reasoning_effort: ReasoningEffort::Off,
                        tools: Vec::new(),
                        session_id: "sess".into(),
                    });
                }
            }
            Ok(Some(EngineEvent::EngineAskResponse { id, .. })) if id == ask_id => {
                got_response = true;
                // Unblock the tool so the engine can complete its turn.
                if let Some(rid) = tool_dispatch_request_id {
                    handle.send(UiCommand::ToolResult {
                        request_id: rid,
                        call_id: TOOL_CALL_ID.into(),
                        content: "tool finished".into(),
                        is_error: false,
                        metadata: None,
                    });
                }
                break;
            }
            Ok(Some(_)) | Ok(None) | Err(_) => {}
        }
    }

    server.abort();

    assert!(
        sent_engine_ask,
        "test never received ToolDispatch - the test plumbing is broken \
         before the bug under test is exercised"
    );
    assert!(
        got_response,
        "EngineAsk sent while a tool was executing produced no EngineAskResponse \
         within the timeout. The command was silently dropped by \
         execute_concurrent's `_ => {{}}` catch-all in crates/engine/src/agent.rs. \
         Any tool that parks on smelt.engine.ask (web_fetch, etc.) will deadlock \
         the turn. Fix: change the catch-all to \
         `other => {{ self.handle_background_cmd(other); }}` to mirror call_llm."
    );
}
