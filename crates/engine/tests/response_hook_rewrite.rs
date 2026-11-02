//! Provider response rewriting and bounded context-limit recovery through host hooks.

use engine::EngineConfig;
use protocol::{
    AgentMode, Content, EngineEvent, Message, ModelConfig, ModelTarget, ReasoningEffort,
    RequestRuntimeConfig, StartTurnPayload, UiCommand,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const STREAMED: &str = "I'll clear the caches.";
const REWRITTEN: &str = "The guard held the command back.";

fn tool_call_sse() -> String {
    let events = [
        r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I'll clear the caches."}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"bash:1","name":"bash","input":{}}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"rm -rf /\"}"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":12}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    events.iter().map(|ev| format!("data: {ev}\n\n")).collect()
}

async fn serve_one(sock: &mut tokio::net::TcpStream, status: u16, body: &str) {
    let mut buf = [0u8; 8192];
    let mut seen = Vec::new();
    let header_end = loop {
        if let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        let n = sock.read(&mut buf).await.unwrap();
        assert_ne!(n, 0, "request ended before its headers");
        seen.extend_from_slice(&buf[..n]);
    };
    let content_length: usize = std::str::from_utf8(&seen[..header_end])
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .expect("request content-length")
        .1
        .trim()
        .parse()
        .unwrap();
    while seen.len() < header_end + content_length {
        let n = sock.read(&mut buf).await.unwrap();
        assert_ne!(n, 0, "request ended before its body");
        seen.extend_from_slice(&buf[..n]);
    }
    let header = format!(
        "HTTP/1.1 {status} Response\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(header.as_bytes()).await.unwrap();
    sock.write_all(body.as_bytes()).await.unwrap();
    sock.flush().await.unwrap();
}

async fn run_hook(
    provider: &str,
    body: String,
    hook: fn(Message) -> Message,
) -> (Vec<EngineEvent>, usize) {
    let (events, requests, _) = run_responses(provider, vec![(200, body)], hook).await;
    (events, requests)
}

async fn run_responses(
    provider: &str,
    responses: Vec<(u16, String)>,
    hook: fn(Message) -> Message,
) -> (Vec<EngineEvent>, usize, usize) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let served = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let index = served.fetch_add(1, Ordering::SeqCst);
            let (status, body) = &responses[index.min(responses.len() - 1)];
            serve_one(&mut sock, *status, body).await;
        }
    });

    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let target = ModelTarget {
        model: "test-model".into(),
        api_base: format!("http://{addr}"),
        api_key: "test-key".into(),
        provider_type: provider.into(),
        provider: provider.into(),
        config: ModelConfig::default(),
    };
    let mut handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                handle.recv().await.expect("engine closed before ready"),
                EngineEvent::Ready
            ) {
                break;
            }
        }
    })
    .await
    .expect("engine startup timed out");
    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("clean up")),
        mode: AgentMode::normal(),
        model_target: target,
        request_config: RequestRuntimeConfig {
            request_audit: protocol::RequestAuditMode::Off,
            ..Default::default()
        },
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        history: protocol::ModelHistorySource::items(Vec::new()),
        session_id: "sess".into(),
        sessions_root: PathBuf::from("/tmp"),
        ephemeral: false,
        persistence: protocol::PersistenceScope::default(),
        permission_overrides: None,
        native_prompt: None,
        tool_visibility: protocol::ToolVisibility::Interactive,
        system_prompt: Some("test system".into()),
        tools: Vec::new(),
    })));

    // Every event after the response draft streamed, in order.
    let mut after_hook = Vec::new();
    let mut hooked = false;
    let mut recoveries = 0;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match handle.recv_output().await {
                Some(engine::EngineOutput::HostCall(engine::HostCall::PrepareRequest {
                    reply,
                    ..
                })) => reply.send(engine::HostRequestDecision::Continue).unwrap(),
                Some(engine::EngineOutput::HostCall(engine::HostCall::ProviderResponse {
                    message,
                    reply,
                    ..
                })) => {
                    hooked = true;
                    reply.send(Some(hook(message))).unwrap();
                }
                Some(engine::EngineOutput::HostCall(
                    engine::HostCall::RecoverFromContextLimit {
                        messages, reply, ..
                    },
                )) => {
                    recoveries += 1;
                    reply
                        .send(engine::HostRequestDecision::replace_canonical_history(
                            messages,
                        ))
                        .unwrap();
                }
                Some(engine::EngineOutput::Event(EngineEvent::TurnComplete { .. })) => break,
                Some(engine::EngineOutput::Event(event))
                    if hooked || matches!(event, EngineEvent::TurnError { .. }) =>
                {
                    after_hook.push(event)
                }
                Some(_) => {}
                None => panic!("engine closed before the turn completed"),
            }
        }
    })
    .await
    .expect("the turn did not finish");
    server.abort();
    (after_hook, requests.load(Ordering::SeqCst), recoveries)
}

fn context_error() -> (u16, String) {
    (400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: context window exceeded"}}"#.into())
}

#[tokio::test(flavor = "current_thread")]
async fn consecutive_context_rejections_allow_only_one_recovery() {
    let (events, requests, recoveries) =
        run_responses("anthropic-compatible", vec![context_error()], |message| {
            message
        })
        .await;
    assert_eq!(requests, 2);
    assert_eq!(recoveries, 1);
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::TurnError {
            kind: Some(protocol::EngineAskErrorKind::ContextWindow),
            ..
        }
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn successful_response_resets_context_recovery_allowance() {
    let (events, requests, recoveries) = run_responses(
        "anthropic-compatible",
        vec![context_error(), (200, tool_call_sse()), context_error()],
        |message| message,
    )
    .await;
    assert_eq!(requests, 4);
    assert_eq!(recoveries, 2);
    assert!(events
        .iter()
        .any(|event| matches!(event, EngineEvent::ResponseDraftAccepted)));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::TurnError {
            kind: Some(protocol::EngineAskErrorKind::ContextWindow),
            ..
        }
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn hook_that_removes_every_tool_call_ends_the_turn_with_its_text() {
    let (after_hook, requests) = run_hook("anthropic-compatible", tool_call_sse(), |message| {
        assert_eq!(message.tool_calls.as_ref().map(Vec::len), Some(1));
        Message::assistant(Some(Content::text(REWRITTEN)), None, None)
    })
    .await;
    assert_eq!(requests, 1, "the model was asked again");
    assert!(!after_hook
        .iter()
        .any(|e| matches!(e, EngineEvent::TurnError { .. })));
    let rejected = after_hook
        .iter()
        .position(|e| matches!(e, EngineEvent::ResponseDraftRejected))
        .expect("the streamed draft was not rejected");
    let text = after_hook
        .iter()
        .position(|e| matches!(e, EngineEvent::Text { content } if content == REWRITTEN))
        .expect("the hook's text was not shown");
    let accepted = after_hook
        .iter()
        .position(|e| matches!(e, EngineEvent::ResponseDraftAccepted))
        .expect("the response was not accepted");
    assert!(rejected < text && text < accepted);
    assert!(!after_hook
        .iter()
        .any(|e| matches!(e, EngineEvent::Text { content } if content == STREAMED)));
}

fn summary_sse() -> String {
    let events = [
        serde_json::json!({"type": "response.reasoning_summary_part.added", "item_id": "r", "summary_index": 0}),
        serde_json::json!({"type": "response.reasoning_summary_text.delta", "item_id": "r", "summary_index": 0, "delta": "**Safety**\nCheck first."}),
        serde_json::json!({"type": "response.reasoning_summary_text.done", "item_id": "r", "summary_index": 0, "text": "**Safety**\nCheck first."}),
        serde_json::json!({"type": "response.output_text.delta", "delta": STREAMED}),
        serde_json::json!({"type": "response.completed", "response": {"usage": {"input_tokens": 5, "output_tokens": 12}}}),
    ];
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn text_rewrite_preserves_streamed_reasoning_summary() {
    let (events, requests) = run_hook("openai", summary_sse(), |mut message| {
        message.content = Some(Content::text(REWRITTEN));
        message
    })
    .await;
    assert_eq!(requests, 1);
    assert!(
        events.iter().any(|event| matches!(event,
            EngineEvent::Reasoning { kind: protocol::ReasoningKind::Summary, title, content }
            if title.as_deref() == Some("Safety") && content == "Check first."
        )),
        "summary structure was lost: {events:?}"
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        EngineEvent::Reasoning {
            kind: protocol::ReasoningKind::Raw,
            ..
        }
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn rewritten_reasoning_replaces_original_summary() {
    let (events, requests) = run_hook("openai", summary_sse(), |mut message| {
        message.reasoning_content = Some("New reasoning.".into());
        message
    })
    .await;
    assert_eq!(requests, 1);
    assert!(events.iter().any(|event| matches!(event,
        EngineEvent::Reasoning { kind: protocol::ReasoningKind::Raw, content, .. }
        if content == "New reasoning."
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        EngineEvent::Reasoning {
            kind: protocol::ReasoningKind::Summary,
            ..
        }
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn removing_tools_without_rewriting_text_rejects_speculative_calls() {
    let (events, requests) = run_hook("anthropic-compatible", tool_call_sse(), |mut message| {
        message.tool_calls = None;
        message
    })
    .await;
    assert_eq!(requests, 1);
    assert!(events
        .iter()
        .any(|event| matches!(event, EngineEvent::ResponseDraftRejected)));
    assert!(!events
        .iter()
        .any(|event| matches!(event, EngineEvent::ToolStarted { .. })));
    assert!(events.iter().any(|event| matches!(event,
        EngineEvent::Text { content } if content == STREAMED
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn unchanged_response_keeps_streamed_output_without_batch_replay() {
    let (events, requests) = run_hook("openai", summary_sse(), |message| message).await;
    assert_eq!(requests, 1);
    assert!(!events.iter().any(|event| matches!(
        event,
        EngineEvent::ResponseDraftRejected
            | EngineEvent::Text { .. }
            | EngineEvent::Reasoning { .. }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EngineEvent::ResponseDraftAccepted))
            .count(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn empty_hook_output_is_retried_then_reported_without_acceptance() {
    let (events, requests) = run_hook("anthropic-compatible", tool_call_sse(), |_| {
        Message::assistant(None, None, None)
    })
    .await;
    assert_eq!(requests, 3, "empty hook output must use bounded retries");
    assert!(events
        .iter()
        .any(|event| matches!(event, EngineEvent::TurnError { .. })));
    assert!(!events
        .iter()
        .any(|event| matches!(event, EngineEvent::ResponseDraftAccepted)));
}
