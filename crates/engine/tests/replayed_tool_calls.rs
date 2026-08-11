//! End-to-end coverage for provider-replayed tool-call IDs.

use engine::EngineConfig;
use protocol::{
    AgentMode, Content, Decision, EngineEvent, HistoryItem, ModelConfig, ModelTarget,
    ReasoningEffort, RequestRuntimeConfig, StartTurnPayload, ToolEvaluation, ToolMetadata,
    UiCommand,
};
use smelt_provider::{FunctionSchema, ToolDefinition};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOOL_NAME: &str = "counted_tool";
const TOOL_CALL_ID: &str = "replayed-call";

fn sse(events: &[&str]) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(event);
        body.push_str("\n\n");
    }
    body
}

fn tool_call_sse() -> String {
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

fn terminal_sse() -> String {
    sse(&[
        r#"{"type":"message_start","message":{"id":"done","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}"#,
        r#"{"type":"message_stop"}"#,
    ])
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

async fn run_server(listener: TcpListener, requests: Arc<Mutex<Vec<serde_json::Value>>>) {
    let mut request_index = 0;
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(connection) => connection,
            Err(_) => return,
        };
        let request = read_json_request(&mut sock).await;
        requests.lock().unwrap().push(request);
        let body = if request_index < 2 {
            tool_call_sse()
        } else {
            terminal_sse()
        };
        request_index += 1;
        write_sse_response(&mut sock, &body).await;
    }
}

struct CountingDispatcher {
    executions: Arc<AtomicUsize>,
}

impl engine::tools::ToolDispatcher for CountingDispatcher {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition::new(FunctionSchema {
            name: TOOL_NAME.into(),
            description: "counts executions".into(),
            parameters: serde_json::json!({"type":"object","properties":{}}),
        })]
    }

    fn contains(&self, name: &str) -> bool {
        name == TOOL_NAME
    }

    fn evaluate_tool_call(
        &self,
        _turn_id: u64,
        name: &str,
        _args: &HashMap<String, serde_json::Value>,
        _mode: AgentMode,
        _permission_overrides: Option<&protocol::PermissionOverrides>,
    ) -> Option<ToolEvaluation> {
        self.contains(name).then(|| ToolEvaluation {
            decision: Decision::Allow,
            metadata: ToolMetadata::default(),
        })
    }

    fn dispatch<'a>(
        &'a self,
        name: &str,
        _args: HashMap<String, serde_json::Value>,
        _ctx: &'a engine::tools::ToolContext,
    ) -> Option<engine::tools::ToolFuture<'a>> {
        if !self.contains(name) {
            return None;
        }
        self.executions.fetch_add(1, Ordering::SeqCst);
        Some(Box::pin(async {
            engine::tools::ToolResult::ok("executed")
        }))
    }
}

fn count_tool_results(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(values) => values.iter().map(count_tool_results).sum(),
        serde_json::Value::Object(object) => {
            usize::from(
                object.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
                    && object
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(TOOL_CALL_ID),
            ) + object.values().map(count_tool_results).sum::<usize>()
        }
        _ => 0,
    }
}

fn count_committed_invocations(items: &[HistoryItem]) -> usize {
    items
        .iter()
        .filter_map(|item| match item {
            HistoryItem::Assistant(step) => Some(&step.invocations),
            _ => None,
        })
        .flatten()
        .filter(|invocation| invocation.call_id == TOOL_CALL_ID)
        .count()
}

#[tokio::test(flavor = "current_thread")]
async fn replayed_completed_tool_call_is_not_executed_or_committed_again() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(run_server(listener, Arc::clone(&requests)));
    let executions = Arc::new(AtomicUsize::new(0));

    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        host_callbacks: engine::HostCallbacks::Disabled,
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
    let dispatcher = CountingDispatcher {
        executions: Arc::clone(&executions),
    };
    let mut handle = engine::start(config, Box::new(dispatcher));

    loop {
        match handle.recv().await {
            Some(EngineEvent::Ready) => break,
            Some(_) => {}
            None => panic!("engine closed before Ready"),
        }
    }

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
        tools: Vec::new(),
    })));

    let mut turn_error = None;
    let mut committed_invocations = 0;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match handle.recv().await {
                Some(EngineEvent::HistoryAppended { delta, .. }) => {
                    committed_invocations += count_committed_invocations(&delta.items);
                }
                Some(EngineEvent::TurnError { message, .. }) => turn_error = Some(message),
                Some(EngineEvent::TurnComplete { .. }) => break,
                Some(_) => {}
                None => panic!("engine stopped before turn completion"),
            }
        }
    })
    .await
    .expect("turn did not complete");
    server.abort();

    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(committed_invocations, 1);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(count_tool_results(&requests[0]), 0);
    assert_eq!(count_tool_results(&requests[1]), 1);
    assert!(
        turn_error
            .as_deref()
            .is_some_and(|message| message.contains(TOOL_CALL_ID)),
        "expected replay error naming {TOOL_CALL_ID}, got {turn_error:?}"
    );
}
