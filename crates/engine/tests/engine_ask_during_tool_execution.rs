//! Integration coverage for auxiliary engine asks and explicit model switches
//! across in-flight provider requests and concurrent or sequential tool waits.

use engine::EngineConfig;
use protocol::{
    AgentMode, Content, EngineEvent, Message, ModelConfig, ModelTarget, ReasoningEffort,
    RequestRuntimeConfig, StartTurnPayload, ToolDef, ToolExecutionMode, ToolHookFlags,
    ToolMetadata, UiCommand,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOOL_NAME: &str = "needs_engine_ask";
const TOOL_CALL_ID: &str = "needs_engine_ask:1";
const TEST_DEADLINE: Duration = Duration::from_secs(30);

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

async fn read_json_request(sock: &mut tokio::net::TcpStream) -> serde_json::Value {
    let mut buf = [0u8; 8192];
    let mut accumulated = Vec::new();
    let header_end = loop {
        let read = sock.read(&mut buf).await.unwrap();
        assert!(read > 0, "connection closed before request headers");
        accumulated.extend_from_slice(&buf[..read]);
        if let Some(end) = accumulated
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        {
            break end;
        }
    };
    let headers = String::from_utf8_lossy(&accumulated[..header_end]);
    let content_len = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .expect("request content-length");
    while accumulated.len().saturating_sub(header_end) < content_len {
        let read = sock.read(&mut buf).await.unwrap();
        assert!(read > 0, "connection closed before request body");
        accumulated.extend_from_slice(&buf[..read]);
    }
    serde_json::from_slice(&accumulated[header_end..header_end + content_len]).unwrap()
}

async fn write_sse_response(sock: &mut tokio::net::TcpStream, body: &str) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(header.as_bytes()).await.unwrap();
    sock.write_all(body.as_bytes()).await.unwrap();
    sock.flush().await.unwrap();
}

/// Accepts connections in a loop. Request #0 is the main turn (returns
/// a tool_use); every subsequent request is treated as an EngineAsk and
/// gets a plain text response.
async fn run_server(
    listener: TcpListener,
    counter: Arc<AtomicUsize>,
    request_tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
) {
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        let counter = counter.clone();
        let request_tx = request_tx.clone();
        tokio::spawn(async move {
            let _ = request_tx.send(read_json_request(&mut sock).await);
            let request_index = counter.fetch_add(1, Ordering::SeqCst);
            let body = if request_index == 0 {
                primary_turn_sse()
            } else {
                aux_text_sse()
            };
            write_sse_response(&mut sock, &body).await;
        });
    }
}

#[tokio::test(flavor = "current_thread")]
async fn engine_ask_during_tool_execution_is_not_silently_dropped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let req_counter = Arc::new(AtomicUsize::new(0));
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(run_server(listener, req_counter.clone(), request_tx));

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
        headless: true,
    };

    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("go")),
        mode: AgentMode::normal(),
        model_target: target.clone(),
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

    // Pose as the Lua host: on ToolDispatch, simulate a Lua tool that sends
    // EngineAsk and waits for EngineAskResponse before returning ToolResult.
    let ask_id: u64 = 0xDEAD_BEEF;
    let ask_target = ModelTarget {
        model: "ask-model".into(),
        config: ModelConfig {
            max_tokens: Some(333),
            input_cost: Some(4.5),
            ..Default::default()
        },
        ..target.clone()
    };
    let switched_target = ModelTarget {
        model: "switched-model".into(),
        config: ModelConfig {
            max_tokens: Some(777),
            tool_calling: Some(false),
            ..Default::default()
        },
        ..target.clone()
    };
    let mut sent_engine_ask = false;
    let mut got_response = false;
    let mut turn_completed = false;
    let mut tool_dispatch_request_id: Option<u64> = None;

    tokio::time::timeout(TEST_DEADLINE, async {
        loop {
            match handle.recv().await {
                Some(EngineEvent::ToolEvaluationRequest { request_id, .. }) => {
                    handle.send(UiCommand::ToolEvaluationResponse {
                        request_id,
                        evaluation: protocol::ToolEvaluation {
                            decision: protocol::Decision::Allow,
                            metadata: ToolMetadata::default(),
                        },
                    });
                }
                Some(EngineEvent::ToolDispatch {
                    request_id,
                    call_id,
                    ..
                }) => {
                    assert_eq!(call_id, TOOL_CALL_ID);
                    tool_dispatch_request_id = Some(request_id);
                    if !sent_engine_ask {
                        sent_engine_ask = true;
                        handle.send(UiCommand::SetTurnModel {
                            target: Box::new(switched_target.clone()),
                            system_prompt: "switched system".into(),
                        });
                        handle.send(UiCommand::EngineAsk {
                            id: ask_id,
                            system: "sub-system".into(),
                            messages: vec![Message::user(Content::text("sub-q"))],
                            target: Box::new(ask_target.clone()),
                            request_config: RequestRuntimeConfig::default(),
                            response_format: None,
                            reasoning_effort: ReasoningEffort::Off,
                            fast_mode: false,
                            tools: Vec::new(),
                            session_id: "sess".into(),
                            session_dir: std::path::PathBuf::from("/tmp/sess"),
                            persistence: protocol::PersistenceScope::default(),
                            stream: false,
                            visible_retries: false,
                        });
                    }
                }
                Some(EngineEvent::EngineAskResponse { id, .. }) if id == ask_id => {
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
                }
                Some(EngineEvent::TurnComplete { .. }) => {
                    turn_completed = true;
                    break;
                }
                Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
                Some(_) => {}
                None => panic!("engine stopped before turn completion"),
            }
        }
    })
    .await
    .expect("concurrent tool turn completion");

    server.abort();

    assert!(
        sent_engine_ask,
        "test never received ToolDispatch - the test plumbing is broken \
         before the bug under test is exercised"
    );
    assert!(
        got_response,
        "EngineAsk sent during tool execution must produce EngineAskResponse"
    );
    assert!(
        turn_completed,
        "turn did not complete after the tool result"
    );
    let requests: Vec<_> = std::iter::from_fn(|| request_rx.try_recv().ok()).collect();
    let ask_request = requests
        .iter()
        .find(|request| request["model"] == "ask-model")
        .expect("EngineAsk should dispatch its own complete target");
    assert_eq!(ask_request["max_tokens"], 333);
    let switched_request = requests
        .iter()
        .find(|request| request["model"] == "switched-model")
        .expect("the request after concurrent tool execution should use the switched target");
    assert_eq!(switched_request["max_tokens"], 777);
    assert!(
        switched_request
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
        "tool-calling capability must come from the switched target: {switched_request}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_during_in_flight_request_applies_at_next_request_boundary() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (first_request_tx, first_request_rx) = tokio::sync::oneshot::channel();
    let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel();
    let (second_request_tx, second_request_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        first_request_tx
            .send(read_json_request(&mut first).await)
            .unwrap();
        release_first_rx.await.unwrap();
        write_sse_response(&mut first, &aux_text_sse()).await;

        let (mut second, _) = listener.accept().await.unwrap();
        second_request_tx
            .send(read_json_request(&mut second).await)
            .unwrap();
        write_sse_response(&mut second, &aux_text_sse()).await;
    });

    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let original_target = ModelTarget {
        model: "original-model".into(),
        api_base: format!("http://{addr}"),
        api_key: "original-key".into(),
        provider_type: "anthropic-compatible".into(),
        config: ModelConfig {
            max_tokens: Some(111),
            ..Default::default()
        },
    };
    let switched_target = ModelTarget {
        model: "next-model".into(),
        api_key: "next-key".into(),
        config: ModelConfig {
            max_tokens: Some(222),
            supports_reasoning: Some(true),
            ..Default::default()
        },
        ..original_target.clone()
    };
    let mut handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));
    drop(handle.take_host_rx());
    while !matches!(handle.recv().await, Some(EngineEvent::Ready)) {}

    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("first")),
        mode: AgentMode::normal(),
        model_target: original_target,
        request_config: RequestRuntimeConfig {
            request_audit: protocol::RequestAuditMode::Off,
            ..Default::default()
        },
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        history: protocol::ModelHistorySource::items(Vec::new()),
        session_id: "sess".into(),
        session_dir: PathBuf::from("/tmp"),
        persistence: protocol::PersistenceScope::default(),
        permission_overrides: None,
        system_prompt: Some("test system".into()),
        tools: Vec::new(),
    })));

    let first_request = first_request_rx.await.unwrap();
    assert_eq!(first_request["model"], "original-model");
    assert_eq!(first_request["max_tokens"], 111);
    handle.send(UiCommand::SetTurnModel {
        target: Box::new(switched_target),
        system_prompt: "switched system with Lua fragment".into(),
    });
    handle.send(UiCommand::Steer {
        input: protocol::StartTurnInput::user(Content::text("use the new target")),
    });
    release_first_tx.send(()).unwrap();

    let second_request = second_request_rx.await.unwrap();
    assert_eq!(second_request["model"], "next-model");
    assert_eq!(second_request["max_tokens"], 222);
    assert_eq!(
        second_request["system"][0]["text"],
        "switched system with Lua fragment"
    );

    loop {
        match handle.recv().await {
            Some(EngineEvent::TurnComplete { .. }) => break,
            Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
            Some(_) => {}
            None => panic!("engine stopped before turn completion"),
        }
    }
    server.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_during_sequential_tool_wait_applies_to_follow_up_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(run_server(listener, counter, request_tx));
    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let original_target = ModelTarget {
        model: "sequential-original".into(),
        api_base: format!("http://{addr}"),
        api_key: "key".into(),
        provider_type: "anthropic-compatible".into(),
        config: ModelConfig::default(),
    };
    let switched_target = ModelTarget {
        model: "sequential-next".into(),
        config: ModelConfig {
            max_tokens: Some(654),
            ..Default::default()
        },
        ..original_target.clone()
    };
    let mut handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));
    drop(handle.take_host_rx());
    while !matches!(handle.recv().await, Some(EngineEvent::Ready)) {}
    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("go")),
        mode: AgentMode::normal(),
        model_target: original_target,
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
        tools: vec![ToolDef {
            name: TOOL_NAME.into(),
            description: "sequential target switch".into(),
            parameters: serde_json::json!({"type":"object","properties":{}}),
            modes: None,
            execution_mode: ToolExecutionMode::Sequential,
            override_core: false,
            hooks: ToolHookFlags::default(),
            headless: true,
        }],
    })));

    loop {
        match tokio::time::timeout(TEST_DEADLINE, handle.recv())
            .await
            .expect("sequential turn event")
        {
            Some(EngineEvent::ToolEvaluationRequest { request_id, .. }) => {
                handle.send(UiCommand::ToolEvaluationResponse {
                    request_id,
                    evaluation: protocol::ToolEvaluation {
                        decision: protocol::Decision::Allow,
                        metadata: ToolMetadata::default(),
                    },
                });
            }
            Some(EngineEvent::ToolDispatch {
                request_id,
                call_id,
                ..
            }) => {
                handle.send(UiCommand::SetTurnModel {
                    target: Box::new(switched_target.clone()),
                    system_prompt: "sequential switched system".into(),
                });
                handle.send(UiCommand::ToolResult {
                    request_id,
                    call_id,
                    content: "done".into(),
                    is_error: false,
                    metadata: None,
                });
            }
            Some(EngineEvent::TurnComplete { .. }) => break,
            Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
            Some(_) => {}
            None => panic!("engine stopped before turn completion"),
        }
    }
    server.abort();

    let requests: Vec<_> = std::iter::from_fn(|| request_rx.try_recv().ok()).collect();
    let next_request = requests
        .iter()
        .find(|request| request["model"] == "sequential-next")
        .expect("follow-up request should use switched sequential target");
    assert_eq!(next_request["max_tokens"], 654);
    assert_eq!(
        next_request["system"][0]["text"],
        "sequential switched system"
    );
}
