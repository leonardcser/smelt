//! Regression test: Lua/plugin tool evaluation responses must stay queued while
//! the engine is still classifying a parallel tool batch.
//!
//! The fake host runs on a separate OS thread, like the TUI, and replies to
//! `ToolEvaluationRequest` immediately. Those replies share `cmd_rx` with user
//! commands, but they can only be matched once `classify_tools` has built the
//! full pending request-id plan for `execute_concurrent`.

use engine::EngineConfig;
use protocol::{
    AgentMode, Content, EngineEvent, ModelConfig, ModelTarget, ReasoningEffort,
    RequestRuntimeConfig, StartTurnPayload, ToolDef, ToolExecutionMode, ToolHookFlags,
    ToolMetadata, UiCommand,
};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOOL_NAME: &str = "probe_lua_tool";
const TOOL_COUNT: usize = 10_000;

fn model_target(addr: std::net::SocketAddr) -> ModelTarget {
    ModelTarget {
        model: "test-model".into(),
        api_base: format!("http://{addr}"),
        api_key: "test-key".into(),
        provider_type: "anthropic-compatible".into(),
        config: ModelConfig {
            max_tokens: Some(4096),
            ..ModelConfig::default()
        },
    }
}

fn multi_tool_sse() -> String {
    let mut events = Vec::with_capacity(2 + TOOL_COUNT * 3);
    events.push(
        r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#
            .to_string(),
    );
    for i in 0..TOOL_COUNT {
        events.push(format!(
            r#"{{"type":"content_block_start","index":{i},"content_block":{{"type":"tool_use","id":"call-{i}","name":"{TOOL_NAME}","input":{{}}}}}}"#,
        ));
        events.push(format!(
            r#"{{"type":"content_block_delta","index":{i},"delta":{{"type":"input_json_delta","partial_json":"{{}}"}}}}"#,
        ));
        events.push(format!(r#"{{"type":"content_block_stop","index":{i}}}"#));
    }
    events.push(
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":12}}"#
            .to_string(),
    );
    events.push(r#"{"type":"message_stop"}"#.to_string());
    sse(events)
}

fn done_sse() -> String {
    sse(vec![
        r#"{"type":"message_start","message":{"id":"m2","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#.to_string(),
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_string(),
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}"#.to_string(),
        r#"{"type":"content_block_stop","index":0}"#.to_string(),
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}"#.to_string(),
        r#"{"type":"message_stop"}"#.to_string(),
    ])
}

fn single_tool_sse(tool_name: &str) -> String {
    sse(vec![
        r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#.to_string(),
        format!(
            r#"{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"call-1","name":"{tool_name}","input":{{}}}}}}"#,
        ),
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#.to_string(),
        r#"{"type":"content_block_stop","index":0}"#.to_string(),
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":3}}"#.to_string(),
        r#"{"type":"message_stop"}"#.to_string(),
    ])
}

fn sse(events: Vec<String>) -> String {
    let mut body = String::new();
    for ev in events {
        body.push_str("data: ");
        body.push_str(&ev);
        body.push_str("\n\n");
    }
    body
}

async fn run_server(listener: TcpListener) {
    let mut request_count = 0usize;
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        let body = if request_count == 0 {
            multi_tool_sse()
        } else {
            done_sse()
        };
        request_count += 1;
        tokio::spawn(async move {
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
            let header_end = accumulated
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| i + 4)
                .unwrap_or(accumulated.len());
            let headers = String::from_utf8_lossy(&accumulated[..header_end]);
            let content_len = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            });
            if let Some(content_len) = content_len {
                let already_read = accumulated.len().saturating_sub(header_end);
                let mut remaining = content_len.saturating_sub(already_read);
                while remaining > 0 {
                    let n = match sock.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    if n == 0 {
                        return;
                    }
                    remaining = remaining.saturating_sub(n);
                }
            }
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

async fn respond_sse(mut sock: tokio::net::TcpStream, body: String) {
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
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(header.as_bytes()).await;
    let _ = sock.write_all(body.as_bytes()).await;
    let _ = sock.flush().await;
}

async fn run_one_response_server(listener: TcpListener, body: String) {
    let Ok((sock, _)) = listener.accept().await else {
        return;
    };
    respond_sse(sock, body).await;
}

#[derive(Debug)]
struct HostReport {
    evals: usize,
    dispatches: usize,
    completed: bool,
    turn_error: Option<String>,
}

#[tokio::test]
async fn lua_tool_evaluation_error_rejects_without_started_flash() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(run_one_response_server(
        listener,
        single_tool_sse(TOOL_NAME),
    ));

    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        host_callbacks: engine::HostCallbacks::Disabled,
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let target = model_target(addr);

    let mut handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));
    while !matches!(handle.recv().await, Some(EngineEvent::Ready)) {}

    let tool = ToolDef {
        name: TOOL_NAME.into(),
        description: "probe".into(),
        parameters: serde_json::json!({"type":"object","properties":{}}),
        modes: None,
        execution_mode: ToolExecutionMode::Concurrent,
        override_core: false,
        hooks: ToolHookFlags {
            approval_patterns: false,
            preflight: true,
        },
        headless: true,
    };

    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("go")),
        mode: AgentMode::normal(),
        model_target: target,
        request_config: RequestRuntimeConfig::default(),
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        history: protocol::ModelHistorySource::items(Vec::new()),
        session_id: "sess".into(),
        session_dir: PathBuf::from("/tmp"),
        persistence: protocol::PersistenceScope::default(),
        permission_overrides: None,
        system_prompt: Some("test system".into()),
        tools: vec![tool],
    })));

    let mut saw_started = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for ToolRejected"
        );
        match tokio::time::timeout(Duration::from_millis(100), handle.recv()).await {
            Ok(Some(EngineEvent::ToolEvaluationRequest { request_id, .. })) => {
                handle.send(UiCommand::ToolEvaluationResponse {
                    request_id,
                    evaluation: protocol::ToolEvaluation {
                        decision: protocol::Decision::Error("read the file first".into()),
                        metadata: ToolMetadata::default(),
                    },
                });
            }
            Ok(Some(EngineEvent::ToolStarted { .. })) => saw_started = true,
            Ok(Some(EngineEvent::ToolDispatch { .. })) => panic!("rejected tool was dispatched"),
            Ok(Some(EngineEvent::ToolRejected {
                call_id,
                tool_name,
                result,
                ..
            })) => {
                assert!(!saw_started, "ToolStarted arrived before ToolRejected");
                assert_eq!(call_id, "call-1");
                assert_eq!(tool_name, TOOL_NAME);
                assert!(result.is_error);
                assert_eq!(result.content, "read the file first");
                break;
            }
            Ok(Some(EngineEvent::TurnError { message, .. })) => panic!("turn errored: {message}"),
            Ok(Some(_)) | Ok(None) | Err(_) => {}
        }
    }

    server.abort();
}

#[tokio::test]
async fn lua_tool_evaluation_responses_are_not_lost_while_classifying_parallel_calls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(run_server(listener));

    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        host_callbacks: engine::HostCallbacks::Disabled,
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let target = model_target(addr);

    let mut handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));

    let (report_tx, report_rx) = mpsc::channel();
    let host = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("host runtime");
        rt.block_on(async move {
            loop {
                match handle.recv().await {
                    Some(EngineEvent::Ready) => break,
                    Some(_) => continue,
                    None => panic!("engine closed before Ready"),
                }
            }

            let tool = ToolDef {
                name: TOOL_NAME.into(),
                description: "probe".into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
                modes: None,
                execution_mode: ToolExecutionMode::Concurrent,
                override_core: false,
                hooks: ToolHookFlags::default(),
                headless: true,
            };

            handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
                turn_id: 1,
                input: protocol::StartTurnInput::user(Content::text("go")),
                mode: AgentMode::normal(),
                model_target: target,
                request_config: RequestRuntimeConfig::default(),
                reasoning_effort: ReasoningEffort::Off,
                fast_mode: false,
                history: protocol::ModelHistorySource::items(Vec::new()),
                session_id: "sess".into(),
                session_dir: PathBuf::from("/tmp"),
                persistence: protocol::PersistenceScope::default(),
                permission_overrides: Some(protocol::PermissionOverrides {
                    tools: Some(protocol::RuleSetOverride {
                        allow: vec![TOOL_NAME.into()],
                        ask: Vec::new(),
                        deny: Vec::new(),
                    }),
                    subcommands: Default::default(),
                }),
                system_prompt: Some("test system".into()),
                tools: vec![tool],
            })));

            let mut evals = 0usize;
            let mut dispatches = 0usize;
            let mut completed = false;
            let mut turn_error = None;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while tokio::time::Instant::now() < deadline {
                match tokio::time::timeout(Duration::from_millis(50), handle.recv()).await {
                    Ok(Some(EngineEvent::ToolEvaluationRequest { request_id, .. })) => {
                        evals += 1;
                        handle.send(UiCommand::ToolEvaluationResponse {
                            request_id,
                            evaluation: protocol::ToolEvaluation {
                                decision: protocol::Decision::Allow,
                                metadata: ToolMetadata::default(),
                            },
                        });
                    }
                    Ok(Some(EngineEvent::ToolDispatch {
                        request_id,
                        call_id,
                        ..
                    })) => {
                        dispatches += 1;
                        handle.send(UiCommand::ToolResult {
                            request_id,
                            call_id,
                            content: "ok".into(),
                            is_error: false,
                            metadata: None,
                        });
                    }
                    Ok(Some(EngineEvent::TurnError { message, .. })) => {
                        turn_error = Some(message);
                        break;
                    }
                    Ok(Some(EngineEvent::TurnComplete { .. })) => {
                        completed = true;
                        break;
                    }
                    Ok(Some(_)) | Ok(None) | Err(_) => {}
                }
            }

            let _ = report_tx.send(HostReport {
                evals,
                dispatches,
                completed,
                turn_error,
            });
        });
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
    let report = loop {
        if let Ok(report) = report_rx.try_recv() {
            break report;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "host did not finish before deadline"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    server.abort();
    host.join().expect("host thread");

    assert!(
        report.turn_error.is_none(),
        "turn errored before tool evaluation; report={report:?}"
    );
    assert!(
        report.completed,
        "turn did not complete after all tool results; report={report:?}"
    );
    assert_eq!(
        report.evals, TOOL_COUNT,
        "provider/test did not emit all eval requests; report={report:?}"
    );
    assert_eq!(
        report.dispatches,
        TOOL_COUNT,
        "lost {} ToolEvaluationResponse(s) before ToolDispatch; report={report:?}",
        report.evals.saturating_sub(report.dispatches)
    );
}
