//! Deterministic simulation of the real engine task.
//!
//! A scripted provider answers chat requests in-process, a scripted frontend
//! answers tool evaluation, permission and dispatch requests, and tokio runs
//! with paused virtual time on a single thread. Nothing touches the network,
//! so equal inputs produce equal event traces. Oracles check the event
//! grammar, canonical history deltas, the wire messages sent to the provider,
//! and that a settled engine never leaves a turn open.

use arbitrary::Arbitrary;
use bytes::Bytes;
use engine::clock::VirtualClock;
use engine::{EngineConfig, EngineHandle, HostCallbacks};
use protocol::{
    AgentMode, Content, Decision, EngineEvent, HistoryItem, InvocationId, ModelConfig,
    ModelHistorySource, ModelTarget, PersistenceScope, ReasoningEffort, RequestRuntimeConfig,
    StartTurnInput, StartTurnPayload, ToolDef, ToolEvaluation, ToolExecutionMode, ToolHookFlags,
    ToolMetadata, ToolVisibility, UiCommand,
};
use serde_json::{json, Value};
use smelt_provider::ChatTransport;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant, SystemTime};

const MAX_REPLIES: usize = 8;
const MAX_OPS: usize = 48;
const MAX_CALLS_PER_REPLY: usize = 3;
const MAX_PROVIDER_REQUESTS: usize = 80;
const QUIESCENCE_IDLE_YIELDS: u32 = 48;
const QUIESCENCE_ROUND_LIMIT: u32 = 20_000;
const SETTLE_ROUNDS: u32 = 60;
const SETTLE_STEP: Duration = Duration::from_secs(30);
const TOOL_NAMES: [&str; 4] = [
    "sim_concurrent",
    "sim_sequential",
    "sim_hooked",
    "sim_unknown",
];

#[derive(Arbitrary, Debug, Clone)]
pub struct Input {
    policy: Policy,
    replies: Vec<Reply>,
    ops: Vec<Op>,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
struct Policy {
    evaluation: EvaluationPolicy,
    permission: PermissionPolicy,
    dispatch: DispatchPolicy,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum EvaluationPolicy {
    Allow,
    Ask,
    Deny,
    Error,
    Hold,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum PermissionPolicy {
    Approve,
    Reject,
    Hold,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum DispatchPolicy {
    Succeed,
    Fail,
    Hold,
}

#[derive(Arbitrary, Debug, Clone)]
struct Reply {
    body: ReplyBody,
    gated: bool,
}

#[derive(Arbitrary, Debug, Clone)]
enum ReplyBody {
    Text { words: u8 },
    Tools { calls: Vec<ToolCallSpec> },
    Http { status: HttpFailure },
    Truncated { after_events: u8 },
    Garbage,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
struct ToolCallSpec {
    tool: u8,
    id_reuse: bool,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum HttpFailure {
    BadRequest,
    Unauthorized,
    ContextWindow,
    RateLimited,
    ServerError,
    Overloaded,
}

#[derive(Arbitrary, Debug, Clone)]
enum Op {
    Start { words: u8 },
    Steer { words: u8 },
    Unsteer,
    Cancel,
    Pump { chunks: u8 },
    Resolve { slot: u8, outcome: u8 },
    Advance { millis: u16 },
    SetMode { mode: u8 },
    Settle,
}

/// Run one scenario against the real engine task and check every oracle.
pub fn run(input: Input) {
    isolate_process_environment();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("build simulation runtime");
    runtime.block_on(async move { Sim::new(input).run().await });
}

/// Run a scenario and return the serialized event trace, for determinism checks.
pub fn trace(input: Input) -> Vec<String> {
    isolate_process_environment();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("build simulation runtime");
    runtime.block_on(async move {
        let mut sim = Sim::new(input);
        sim.record_trace = true;
        sim.run().await;
        sim.trace
    })
}

/// The engine writes logs and caches below the user's directories. Point every
/// location at a process-private directory so scenarios never touch real state.
fn isolate_process_environment() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let root = std::env::temp_dir()
            .join("smelt-fuzz-harness")
            .join(format!("engine-sim-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create engine simulation directory");
        for (name, dir) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_RUNTIME_DIR", "runtime"),
        ] {
            let path = root.join(dir);
            std::fs::create_dir_all(&path).expect("create engine simulation path");
            std::env::set_var(name, path);
        }
        std::env::set_var("PATH", "/nonexistent-smelt-fuzz");
    });
}

// ---------------------------------------------------------------------------
// Scripted provider
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Wire {
    replies: VecDeque<Reply>,
    requests: usize,
    bodies: Vec<String>,
    tool_use_counter: usize,
    gates: VecDeque<Gate>,
}

struct Gate {
    tx: tokio::sync::mpsc::UnboundedSender<Result<Bytes, std::io::Error>>,
    chunks: VecDeque<Bytes>,
}

struct ScriptedTransport {
    wire: Arc<Mutex<Wire>>,
}

impl ChatTransport for ScriptedTransport {
    fn send(
        &self,
        request: reqwest::Request,
    ) -> Pin<Box<dyn Future<Output = Result<reqwest::Response, String>> + Send>> {
        let wire = Arc::clone(&self.wire);
        let body: Value = request
            .body()
            .and_then(|body| body.as_bytes())
            .map(|bytes| serde_json::from_slice(bytes).expect("provider request body is JSON"))
            .unwrap_or(Value::Null);
        Box::pin(async move { Ok(wire.lock().unwrap().respond(&body)) })
    }
}

impl Wire {
    fn respond(&mut self, body: &Value) -> reqwest::Response {
        self.requests += 1;
        assert!(
            self.requests <= MAX_PROVIDER_REQUESTS,
            "engine issued more than {MAX_PROVIDER_REQUESTS} provider requests for one scenario"
        );
        check_anthropic_wire(body);
        self.bodies.push(body.to_string());

        let reply = self.replies.pop_front().unwrap_or(Reply {
            body: ReplyBody::Text { words: 1 },
            gated: false,
        });
        let (status, chunks) = self.render(&reply.body);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut gate = Gate {
            tx,
            chunks: chunks.into(),
        };
        if reply.gated && status == 200 {
            self.gates.push_back(gate);
        } else {
            while let Some(chunk) = gate.chunks.pop_front() {
                let _ = gate.tx.send(Ok(chunk));
            }
        }
        let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        let response = http::Response::builder()
            .status(status)
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .expect("build scripted response");
        reqwest::Response::from(response)
    }

    fn render(&mut self, body: &ReplyBody) -> (u16, Vec<Bytes>) {
        match body {
            ReplyBody::Text { words } => (200, sse(text_events(&words_text(*words)))),
            ReplyBody::Tools { calls } => {
                let calls: Vec<(String, String, String)> = calls
                    .iter()
                    .take(MAX_CALLS_PER_REPLY)
                    .map(|spec| {
                        let name = TOOL_NAMES[usize::from(spec.tool) % TOOL_NAMES.len()];
                        let id = if spec.id_reuse {
                            "reused".to_owned()
                        } else {
                            self.tool_use_counter += 1;
                            format!("call-{}", self.tool_use_counter)
                        };
                        (name.to_owned(), id, "{}".to_owned())
                    })
                    .collect();
                (200, sse(tool_events(&calls)))
            }
            ReplyBody::Http { status } => {
                let (code, message) = match status {
                    HttpFailure::BadRequest => (400, "bad request"),
                    HttpFailure::Unauthorized => (401, "invalid api key"),
                    HttpFailure::ContextWindow => {
                        (400, "prompt is too long: maximum context length")
                    }
                    HttpFailure::RateLimited => (429, "rate limited"),
                    HttpFailure::ServerError => (500, "internal error"),
                    HttpFailure::Overloaded => (529, "overloaded"),
                };
                let error =
                    json!({"type": "error", "error": {"type": "api_error", "message": message}});
                (code, vec![Bytes::from(error.to_string())])
            }
            ReplyBody::Truncated { after_events } => {
                let mut events = text_events("cut off mid stream");
                events.truncate(usize::from(*after_events) % events.len());
                (200, sse(events))
            }
            ReplyBody::Garbage => (200, vec![Bytes::from_static(b"data: {not json\n\n")]),
        }
    }

    /// Release up to `chunks` gated chunks, oldest response first, closing
    /// each stream once its last chunk is delivered.
    fn pump(&mut self, mut chunks: usize) {
        while chunks > 0 {
            let Some(gate) = self.gates.front_mut() else {
                return;
            };
            match gate.chunks.pop_front() {
                Some(chunk) => {
                    let _ = gate.tx.send(Ok(chunk));
                    chunks -= 1;
                }
                None => {
                    self.gates.pop_front();
                }
            }
            if self
                .gates
                .front()
                .is_some_and(|gate| gate.chunks.is_empty())
            {
                self.gates.pop_front();
            }
        }
    }

    fn pump_all(&mut self) {
        self.pump(usize::MAX / 2);
    }
}

fn words_text(words: u8) -> String {
    (0..=usize::from(words % 6))
        .map(|index| format!("word{index}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn sse(events: Vec<Value>) -> Vec<Bytes> {
    events
        .into_iter()
        .map(|event| Bytes::from(format!("data: {event}\n\n")))
        .collect()
}

fn message_start() -> Value {
    json!({"type": "message_start", "message": {
        "id": "msg", "type": "message", "role": "assistant", "content": [], "model": "sim",
        "stop_reason": null, "usage": {"input_tokens": 5, "output_tokens": 1}}})
}

fn text_events(text: &str) -> Vec<Value> {
    vec![
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 3}}),
        json!({"type": "message_stop"}),
    ]
}

fn tool_events(calls: &[(String, String, String)]) -> Vec<Value> {
    let mut events = vec![message_start()];
    for (index, (name, id, arguments)) in calls.iter().enumerate() {
        events.push(json!({"type": "content_block_start", "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}));
        events.push(json!({"type": "content_block_delta", "index": index,
            "delta": {"type": "input_json_delta", "partial_json": arguments}}));
        events.push(json!({"type": "content_block_stop", "index": index}));
    }
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 12}}));
    events.push(json!({"type": "message_stop"}));
    events
}

/// Every `tool_use` in an assistant message must be answered by `tool_result`
/// blocks in the very next user message, and no result may be unmatched.
fn check_anthropic_wire(body: &Value) {
    let Some(messages) = body["messages"].as_array() else {
        return;
    };
    let blocks = |message: &Value, kind: &str, key: &str| -> Vec<String> {
        message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == kind)
            .filter_map(|block| block[key].as_str().map(str::to_owned))
            .collect()
    };
    for (index, message) in messages.iter().enumerate() {
        let uses = blocks(message, "tool_use", "id");
        let results = blocks(message, "tool_result", "tool_use_id");
        if message["role"] == "assistant" && !uses.is_empty() {
            // Consecutive user messages are one turn on the wire.
            let mut got: Vec<String> = messages[index + 1..]
                .iter()
                .take_while(|next| next["role"] == "user")
                .flat_map(|next| blocks(next, "tool_result", "tool_use_id"))
                .collect();
            let mut expected = uses.clone();
            expected.sort();
            got.sort();
            assert_eq!(
                expected, got,
                "assistant tool_use blocks are not answered by the following user turn: {body}"
            );
        }
        if !results.is_empty() {
            let assistant = messages[..index]
                .iter()
                .rev()
                .find(|earlier| earlier["role"] == "assistant")
                .map(|earlier| blocks(earlier, "tool_use", "id"))
                .unwrap_or_default();
            for id in &results {
                assert!(
                    assistant.contains(id),
                    "tool_result {id} has no tool_use in the preceding assistant message: {body}"
                );
            }
        }
    }
    if let Some(last) = messages.last() {
        assert_eq!(
            last["role"], "user",
            "request must end with a user message: {body}"
        );
    }
}

// ---------------------------------------------------------------------------
// Simulation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolPhase {
    Started,
    Settled,
}

enum Pending {
    Evaluation {
        request_id: u64,
    },
    Permission {
        request_id: u64,
    },
    Dispatch {
        request_id: u64,
        invocation_id: InvocationId,
        call_id: String,
    },
}

struct Sim {
    policy: Policy,
    ops: Vec<Op>,
    handle: EngineHandle,
    wire: Arc<Mutex<Wire>>,
    clock: Arc<VirtualClock>,
    pending: Vec<Pending>,
    history: Vec<HistoryItem>,
    tools: HashMap<InvocationId, ToolPhase>,
    active_turn: Option<u64>,
    next_turn: u64,
    steer_markers: Vec<String>,
    steered: Vec<String>,
    /// Retractions requested; an idle engine ignores any that arrive late.
    unsteer_requests: usize,
    /// Steers reported and retractions sent during the active turn; a
    /// retraction only ever targets the turn's own steered inputs.
    turn_steers: usize,
    turn_unsteers: usize,
    turn_markers: Vec<String>,
    mode: AgentMode,
    record_trace: bool,
    trace: Vec<String>,
}

impl Sim {
    fn new(input: Input) -> Self {
        let wire = Arc::new(Mutex::new(Wire {
            replies: input.replies.into_iter().take(MAX_REPLIES).collect(),
            ..Wire::default()
        }));
        let clock = Arc::new(VirtualClock::new(
            Instant::now(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        ));
        let config = EngineConfig {
            system_prompt_override: Some("simulated system prompt".into()),
            host_callbacks: HostCallbacks::Disabled,
            transport: Some(Arc::new(ScriptedTransport {
                wire: Arc::clone(&wire),
            })),
            ..EngineConfig::new(std::env::temp_dir(), clock.clone())
        };
        let handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));
        Self {
            policy: input.policy,
            ops: input.ops.into_iter().take(MAX_OPS).collect(),
            handle,
            wire,
            clock,
            pending: Vec::new(),
            history: Vec::new(),
            tools: HashMap::new(),
            active_turn: None,
            next_turn: 1,
            steer_markers: Vec::new(),
            steered: Vec::new(),
            unsteer_requests: 0,
            turn_steers: 0,
            turn_unsteers: 0,
            turn_markers: Vec::new(),
            mode: AgentMode::normal(),
            record_trace: std::env::var_os("SMELT_SIM_TRACE").is_some(),
            trace: Vec::new(),
        }
    }

    async fn run(&mut self) {
        self.quiesce().await;
        for op in std::mem::take(&mut self.ops) {
            self.apply(op).await;
            self.quiesce().await;
        }
        self.settle().await;
    }

    async fn apply(&mut self, op: Op) {
        match op {
            Op::Start { words } => {
                if self.active_turn.is_none() {
                    self.start_turn(words);
                }
            }
            Op::Steer { words } => {
                if self.active_turn.is_some() {
                    let marker = format!("steer-{}", self.steer_markers.len());
                    self.handle.send(UiCommand::Steer {
                        input: StartTurnInput::user(Content::text(format!(
                            "{marker} {}",
                            words_text(words)
                        ))),
                    });
                    self.steer_markers.push(marker);
                }
            }
            Op::Unsteer => {
                if self.active_turn.is_some() && self.turn_unsteers < self.turn_steers {
                    self.handle.send(UiCommand::Unsteer { count: 1 });
                    self.unsteer_requests += 1;
                    self.turn_unsteers += 1;
                }
            }
            Op::Cancel => self.handle.send(UiCommand::Cancel),
            Op::Pump { chunks } => self.wire.lock().unwrap().pump(usize::from(chunks) + 1),
            Op::Resolve { slot, outcome } => {
                if !self.pending.is_empty() {
                    let index = usize::from(slot) % self.pending.len();
                    let pending = self.pending.remove(index);
                    self.resolve(pending, outcome);
                }
            }
            Op::Advance { millis } => self.advance(Duration::from_millis(u64::from(millis))).await,
            Op::SetMode { mode } => {
                let modes = ["normal", "plan", "apply", "yolo"];
                self.mode =
                    AgentMode::parse(modes[usize::from(mode) % modes.len()]).expect("known mode");
                self.handle.send(UiCommand::SetMode {
                    mode: self.mode.clone(),
                });
            }
            Op::Settle => {}
        }
    }

    async fn advance(&mut self, by: Duration) {
        self.clock.advance(by);
        tokio::time::advance(by).await;
    }

    fn start_turn(&mut self, words: u8) {
        let turn_id = self.next_turn;
        self.next_turn += 1;
        self.active_turn = Some(turn_id);
        self.turn_steers = 0;
        self.turn_unsteers = 0;
        let marker = format!("turn-{turn_id}");
        let text = format!("{marker} {}", words_text(words));
        self.turn_markers.push(marker);
        self.handle
            .send(UiCommand::StartTurn(Box::new(StartTurnPayload {
                turn_id,
                input: StartTurnInput::user(Content::text(text)),
                mode: self.mode.clone(),
                model_target: ModelTarget {
                    model: "sim-model".into(),
                    api_base: "http://scripted.invalid".into(),
                    api_key: "key".into(),
                    provider_type: "anthropic-compatible".into(),
                    provider: "sim".into(),
                    config: ModelConfig {
                        max_tokens: Some(4096),
                        context_window: Some(200_000),
                        supports_reasoning: Some(false),
                        ..ModelConfig::default()
                    },
                },
                request_config: RequestRuntimeConfig::default(),
                reasoning_effort: ReasoningEffort::Off,
                fast_mode: false,
                history: ModelHistorySource::items(self.history.clone()),
                session_id: "sim-session".into(),
                sessions_root: std::env::temp_dir(),
                ephemeral: false,
                persistence: PersistenceScope::default(),
                permission_overrides: None,
                native_prompt: None,
                tool_visibility: ToolVisibility::Interactive,
                system_prompt: Some("simulated system prompt".into()),
                tools: sim_tools(),
            })));
    }

    /// Deliver engine events and answer scripted obligations until nothing
    /// changes for several consecutive scheduler turns.
    async fn quiesce(&mut self) {
        let mut idle = 0;
        for _ in 0..QUIESCENCE_ROUND_LIMIT {
            let mut progressed = false;
            while let Ok(event) = self.handle.try_recv() {
                self.absorb(event);
                progressed = true;
            }
            progressed |= self.apply_policies();
            tokio::task::yield_now().await;
            if progressed {
                idle = 0;
            } else {
                idle += 1;
                if idle >= QUIESCENCE_IDLE_YIELDS {
                    return;
                }
            }
        }
        panic!("engine event stream never quiesced");
    }

    /// Release everything that is held and let virtual time pass. A settled
    /// engine must have finished its turn.
    async fn settle(&mut self) {
        for _ in 0..SETTLE_ROUNDS {
            self.wire.lock().unwrap().pump_all();
            for pending in std::mem::take(&mut self.pending) {
                self.resolve(pending, 0);
            }
            self.advance(SETTLE_STEP).await;
            self.quiesce().await;
            if self.active_turn.is_none() && self.pending.is_empty() {
                break;
            }
        }
        assert!(
            self.active_turn.is_none(),
            "turn {:?} never finished after every pending request was answered",
            self.active_turn
        );
        for (invocation, phase) in &self.tools {
            assert_eq!(
                *phase,
                ToolPhase::Settled,
                "tool invocation {invocation:?} never produced an outcome"
            );
        }
        self.check_final_history();
    }

    /// Inputs reach the provider and the canonical history exactly once: every
    /// started turn is recorded once, a steered input is recorded at most once
    /// and only if the engine reported consuming it, and no steered input
    /// disappears without a retraction request.
    fn check_final_history(&self) {
        let history = serde_json::to_string(&self.history).expect("serialize history");
        let occurrences = |marker: &str| history.matches(&format!("\"{marker} ")).count();
        let wire = self.wire.lock().unwrap();
        for marker in &self.turn_markers {
            assert_eq!(
                occurrences(marker),
                1,
                "turn input {marker} appears {} times in canonical history: {history}",
                occurrences(marker)
            );
        }
        let mut missing = 0;
        for marker in &self.steer_markers {
            let steered = self.steered.contains(marker);
            let copies = occurrences(marker);
            let sent_to_provider = wire
                .bodies
                .iter()
                .any(|body| body.contains(&format!("{marker} ")));
            assert!(
                copies <= usize::from(steered),
                "steered input {marker}: reported={steered}, history copies={copies}: {history}"
            );
            missing += usize::from(steered && copies == 0);
            assert!(
                steered || !sent_to_provider,
                "steered input {marker} reached the provider but was never reported as steered"
            );
        }
        assert!(
            missing <= self.unsteer_requests,
            "{missing} steered inputs vanished after {} retractions: {history}",
            self.unsteer_requests
        );
    }

    fn apply_policies(&mut self) -> bool {
        let mut progressed = false;
        let mut held = Vec::new();
        for pending in std::mem::take(&mut self.pending) {
            let policy_outcome = match &pending {
                Pending::Evaluation { .. } => match self.policy.evaluation {
                    EvaluationPolicy::Hold => None,
                    EvaluationPolicy::Allow => Some(0),
                    EvaluationPolicy::Ask => Some(1),
                    EvaluationPolicy::Deny => Some(2),
                    EvaluationPolicy::Error => Some(3),
                },
                Pending::Permission { .. } => match self.policy.permission {
                    PermissionPolicy::Hold => None,
                    PermissionPolicy::Approve => Some(0),
                    PermissionPolicy::Reject => Some(1),
                },
                Pending::Dispatch { .. } => match self.policy.dispatch {
                    DispatchPolicy::Hold => None,
                    DispatchPolicy::Succeed => Some(0),
                    DispatchPolicy::Fail => Some(1),
                },
            };
            match policy_outcome {
                Some(outcome) => {
                    self.resolve(pending, outcome);
                    progressed = true;
                }
                None => held.push(pending),
            }
        }
        self.pending = held;
        progressed
    }

    fn resolve(&mut self, pending: Pending, outcome: u8) {
        match pending {
            Pending::Evaluation { request_id } => {
                let decision = match outcome % 4 {
                    0 => Decision::Allow,
                    1 => Decision::Ask,
                    2 => Decision::Deny,
                    _ => Decision::Error("rejected by simulated preflight".into()),
                };
                self.handle.send(UiCommand::ToolEvaluationResponse {
                    request_id,
                    evaluation: ToolEvaluation {
                        decision,
                        metadata: ToolMetadata::default(),
                    },
                });
            }
            Pending::Permission { request_id } => {
                self.handle.send(UiCommand::PermissionDecision {
                    request_id,
                    approved: outcome.is_multiple_of(2),
                    message: None,
                });
            }
            Pending::Dispatch {
                request_id,
                invocation_id,
                call_id,
            } => {
                self.handle.send(UiCommand::ToolResult {
                    request_id,
                    invocation_id,
                    call_id,
                    content: "simulated output".into(),
                    is_error: outcome % 2 == 1,
                    metadata: None,
                    display_content: Vec::new(),
                    attachment: None,
                });
            }
        }
    }

    fn absorb(&mut self, event: EngineEvent) {
        if self.record_trace {
            let line = serde_json::to_string(&event).expect("serialize engine event");
            if std::env::var_os("SMELT_SIM_TRACE").is_some() {
                eprintln!("event {}", &line[..line.len().min(300)]);
            }
            self.trace.push(line);
        }
        match event {
            EngineEvent::ToolEvaluationRequest { request_id, .. } => {
                self.pending.push(Pending::Evaluation { request_id });
            }
            EngineEvent::RequestPermission { request_id, .. } => {
                self.pending.push(Pending::Permission { request_id });
            }
            EngineEvent::ToolDispatch {
                request_id,
                invocation_id,
                call_id,
                ..
            } => {
                self.pending.push(Pending::Dispatch {
                    request_id,
                    invocation_id,
                    call_id,
                });
            }
            EngineEvent::ToolStarted { invocation_id, .. } => {
                let previous = self.tools.insert(invocation_id, ToolPhase::Started);
                assert!(
                    previous.is_none(),
                    "invocation {invocation_id:?} started twice"
                );
            }
            EngineEvent::ToolFinished { invocation_id, .. } => {
                // A cancel that lands while a request is still held finishes
                // the invocation without a preceding `ToolStarted`.
                let phase = self.tools.insert(invocation_id, ToolPhase::Settled);
                assert!(
                    phase != Some(ToolPhase::Settled),
                    "invocation {invocation_id:?} settled twice"
                );
            }
            EngineEvent::ToolRejected { invocation_id, .. } => {
                let previous = self.tools.insert(invocation_id, ToolPhase::Settled);
                assert!(
                    previous != Some(ToolPhase::Settled),
                    "invocation {invocation_id:?} settled twice"
                );
            }
            EngineEvent::HistoryAppended { turn_id, delta } => {
                self.expect_active(turn_id, "history append");
                self.apply_delta(delta);
            }
            EngineEvent::HistoryUpdated { turn_id, update } => {
                self.expect_active(turn_id, "history update");
                self.apply_delta(update);
            }
            EngineEvent::Steered { turn_id, delta } => {
                self.expect_active(turn_id, "steering");
                let text = serde_json::to_string(&delta.items).expect("serialize steered items");
                for marker in &self.steer_markers {
                    if text.contains(&format!("\"{marker} ")) {
                        assert!(
                            !self.steered.contains(marker),
                            "steered input {marker} was reported twice"
                        );
                        self.steered.push(marker.clone());
                        self.turn_steers += 1;
                    }
                }
                self.apply_delta(delta);
            }
            EngineEvent::TurnComplete {
                turn_id, history, ..
            } => {
                self.expect_active(turn_id, "turn completion");
                if let Some(delta) = history {
                    self.apply_delta(delta);
                }
                self.active_turn = None;
            }
            EngineEvent::TurnPreparationError { turn_id, .. } => {
                self.expect_active(turn_id, "preparation error");
                self.active_turn = None;
            }
            EngineEvent::TurnError { .. } => {
                // An error notice precedes the turn's `TurnComplete`.
                assert!(
                    self.active_turn.is_some(),
                    "turn error arrived with no turn in flight"
                );
            }
            EngineEvent::Shutdown { reason } => {
                panic!("engine shut down unexpectedly: {reason:?}");
            }
            _ => {}
        }
    }

    fn expect_active(&self, turn_id: u64, what: &str) {
        assert_eq!(
            self.active_turn,
            Some(turn_id),
            "{what} for turn {turn_id} while turn {:?} is in flight",
            self.active_turn
        );
    }

    /// Canonical deltas replace the history suffix starting at `first_index`
    /// and must not leave a gap.
    fn apply_delta(&mut self, delta: protocol::CanonicalHistoryDelta) {
        let first = delta.first_index.get();
        assert!(
            first <= self.history.len(),
            "history delta starts at {first} but only {} items exist",
            self.history.len()
        );
        self.history.truncate(first);
        self.history.extend(delta.items);
    }
}

fn sim_tools() -> Vec<ToolDef> {
    let tool = |name: &str, execution_mode, hooks| ToolDef {
        name: name.into(),
        description: "simulated tool".into(),
        parameters: json!({"type": "object", "properties": {}}),
        modes: None,
        execution_mode,
        override_core: false,
        hooks,
        headless: true,
        claude_code: None,
    };
    vec![
        tool(
            TOOL_NAMES[0],
            ToolExecutionMode::Concurrent,
            ToolHookFlags::default(),
        ),
        tool(
            TOOL_NAMES[1],
            ToolExecutionMode::Sequential,
            ToolHookFlags::default(),
        ),
        tool(
            TOOL_NAMES[2],
            ToolExecutionMode::Concurrent,
            ToolHookFlags {
                approval_patterns: true,
                preflight: true,
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use arbitrary::Unstructured;

    fn input(bytes: &[u8]) -> Input {
        Input::arbitrary(&mut Unstructured::new(bytes)).expect("decode simulation input")
    }

    fn lcg_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    fn scripted(policy: Policy, replies: Vec<ReplyBody>, ops: Vec<Op>) -> Input {
        Input {
            policy,
            replies: replies
                .into_iter()
                .map(|body| Reply { body, gated: false })
                .collect(),
            ops,
        }
    }

    const EAGER: Policy = Policy {
        evaluation: EvaluationPolicy::Allow,
        permission: PermissionPolicy::Approve,
        dispatch: DispatchPolicy::Succeed,
    };

    #[test]
    fn plain_turn_completes() {
        run(scripted(
            EAGER,
            vec![ReplyBody::Text { words: 2 }],
            vec![Op::Start { words: 1 }],
        ));
    }

    #[test]
    fn tool_turn_runs_tools_then_finishes() {
        let calls = vec![
            ToolCallSpec {
                tool: 0,
                id_reuse: false,
            },
            ToolCallSpec {
                tool: 1,
                id_reuse: false,
            },
            ToolCallSpec {
                tool: 3,
                id_reuse: false,
            },
        ];
        run(scripted(
            EAGER,
            vec![ReplyBody::Tools { calls }, ReplyBody::Text { words: 0 }],
            vec![Op::Start { words: 0 }],
        ));
    }

    #[test]
    fn held_tool_requests_resolve_during_settle() {
        let policy = Policy {
            evaluation: EvaluationPolicy::Hold,
            permission: PermissionPolicy::Hold,
            dispatch: DispatchPolicy::Hold,
        };
        let calls = vec![ToolCallSpec {
            tool: 2,
            id_reuse: false,
        }];
        run(scripted(
            policy,
            vec![ReplyBody::Tools { calls }],
            vec![Op::Start { words: 0 }, Op::Cancel],
        ));
    }

    #[test]
    fn provider_failures_settle() {
        for status in [
            HttpFailure::BadRequest,
            HttpFailure::Unauthorized,
            HttpFailure::ContextWindow,
            HttpFailure::RateLimited,
            HttpFailure::ServerError,
            HttpFailure::Overloaded,
        ] {
            run(scripted(
                EAGER,
                vec![ReplyBody::Http { status }],
                vec![Op::Start { words: 0 }],
            ));
        }
    }

    #[test]
    fn generated_scenarios_replay_identically() {
        for seed in 0..120 {
            let bytes = lcg_bytes(seed, 256);
            let first = trace(input(&bytes));
            let second = trace(input(&bytes));
            assert_eq!(first, second, "seed {seed} produced different traces");
        }
    }

    #[test]
    fn generated_scenarios_satisfy_every_oracle() {
        let mut failures = Vec::new();
        for seed in 0..400 {
            let bytes = lcg_bytes(seed, 256);
            let scenario = input(&bytes);
            let outcome = std::panic::catch_unwind(|| run(scenario.clone()));
            if let Err(payload) = outcome {
                let message = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_default();
                failures.push(format!("seed {seed}: {message}\n{scenario:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} failing seeds:\n{}",
            failures.len(),
            failures
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n\n")
        );
    }

    #[test]
    fn steering_during_a_tool_round_is_consumed_once() {
        let calls = vec![ToolCallSpec {
            tool: 0,
            id_reuse: false,
        }];
        let mut input = scripted(
            EAGER,
            vec![ReplyBody::Tools { calls }, ReplyBody::Text { words: 1 }],
            vec![
                Op::Start { words: 0 },
                Op::Steer { words: 1 },
                Op::Pump { chunks: 200 },
            ],
        );
        input.replies[0].gated = true;
        let events = trace(input);
        assert!(
            events.iter().any(|line| line.starts_with("{\"Steered\"")),
            "the steered input was never consumed: {events:#?}"
        );
    }

    #[test]
    fn cancel_while_the_response_is_streaming_finishes_the_turn() {
        let mut input = scripted(
            EAGER,
            vec![ReplyBody::Text { words: 5 }],
            vec![Op::Start { words: 0 }, Op::Pump { chunks: 1 }, Op::Cancel],
        );
        input.replies[0].gated = true;
        run(input);
    }

    /// Replay a libFuzzer artifact with its scenario and event trace printed:
    /// `SMELT_SIM_ARTIFACT=<file> cargo test --lib engine_sim -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn replay_artifact() {
        let path = std::env::var("SMELT_SIM_ARTIFACT").expect("set SMELT_SIM_ARTIFACT");
        let scenario = input(&std::fs::read(path).expect("read artifact"));
        eprintln!("{scenario:#?}");
        std::env::set_var("SMELT_SIM_TRACE", "1");
        run(scenario);
    }
}
