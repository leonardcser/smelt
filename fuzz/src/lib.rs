//! Shared types for the smelt fuzz target and the `crash_to_scenario`
//! converter. The on-disk scenario format is a JSON-serialized
//! [`Scenario`] — also the exact shape libFuzzer's `arbitrary` decoder
//! produces, so a crash artifact round-trips into a readable file with no
//! lossy translation.

use arbitrary::Arbitrary;
use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
};
use protocol::{AgentMode, EngineEvent, ToolOutcome};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tui::app::test_harness::{AllocBudget, SourceEvent};

pub use tui::app::test_harness::TestApp;

/// One unit of fuzz input. Each variant either translates to a
/// `SourceEvent` or invokes a harness side channel (`StartTurn`).
#[derive(Arbitrary, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FuzzOp {
    /// Single Unicode codepoint keystroke. Surrogates and out-of-range
    /// values are rewritten to `'?'` on translation.
    KeyUnicode(u32),
    /// Control-modified ASCII letter (`b % 26 + 'a'`).
    KeyCtrl(u8),
    /// Shift-modified ASCII letter.
    KeyShift(u8),
    /// Bare special key chosen by `which % SPECIALS.len()`.
    KeySpecial(u8),
    /// Bracketed paste with arbitrary UTF-8 payload.
    Paste(String),
    /// Advance the virtual clock by `ms` milliseconds.
    Tick(u16),
    /// Wake any pending Lua callbacks.
    LuaWakeup,
    /// Terminal resize, clamped to `[1, 400]` per dimension.
    Resize { w: u16, h: u16 },

    /// Synthesize an active agent turn so subsequent engine events flow
    /// through the active-turn dispatch path.
    StartTurn(u8),

    EngineReady,
    EngineText(String),
    EngineTextDelta(String),
    EngineThinkingDelta(String),
    EngineToolStart {
        call_id: u8,
        tool_name: String,
    },
    EngineToolOutput {
        call_id: u8,
        chunk: String,
    },
    EngineToolFinish {
        call_id: u8,
        is_error: bool,
        content: String,
    },
    ExecOutput(String),
    ExecDone(Option<i32>),
}

#[derive(Arbitrary, Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FuzzMode {
    Normal,
    Plan,
    Apply,
    Yolo,
}

impl From<FuzzMode> for AgentMode {
    fn from(m: FuzzMode) -> Self {
        match m {
            FuzzMode::Normal => AgentMode::Normal,
            FuzzMode::Plan => AgentMode::Plan,
            FuzzMode::Apply => AgentMode::Apply,
            FuzzMode::Yolo => AgentMode::Yolo,
        }
    }
}

/// A full reproducible scenario: initial app config plus the event stream.
/// `FuzzInput` is also what `arbitrary` decodes from libFuzzer bytes, so a
/// crash artifact converts to a `Scenario` JSON via a single
/// `serde_json::to_string_pretty`.
#[derive(Arbitrary, Debug, Clone, Serialize, Deserialize)]
pub struct FuzzInput {
    pub vim: bool,
    pub mode: FuzzMode,
    pub ops: Vec<FuzzOp>,
}

/// Alias clarifying intent at use sites: on-disk JSON is a `Scenario`,
/// the in-memory fuzz input is a `FuzzInput`. Same bytes either way.
pub type Scenario = FuzzInput;

pub const SPECIALS: &[KeyCode] = &[
    KeyCode::Enter,
    KeyCode::Esc,
    KeyCode::Backspace,
    KeyCode::Tab,
    KeyCode::Up,
    KeyCode::Down,
    KeyCode::Left,
    KeyCode::Right,
    KeyCode::Home,
    KeyCode::End,
    KeyCode::PageUp,
    KeyCode::PageDown,
    KeyCode::Delete,
];

pub const MAX_OPS: usize = 256;

/// Slack allowed on top of the post-build interner baseline.
pub const INTERN_SLACK: usize = 64;

const RESIZE_MIN: u16 = 1;
const RESIZE_MAX: u16 = 400;

fn key_event(code: KeyCode, mods: KeyModifiers) -> TermEvent {
    TermEvent::Key(KeyEvent {
        code,
        modifiers: mods,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

fn decode_codepoint(raw: u32) -> char {
    char::from_u32(raw).unwrap_or('?')
}

fn clamp_dim(d: u16) -> u16 {
    d.clamp(RESIZE_MIN, RESIZE_MAX)
}

fn call_id_string(id: u8) -> String {
    format!("call-{id:02x}")
}

/// Cheap snapshot of state needed by event-specific post-checks. Captured
/// before dispatch so a `PostCheck` can compare pre/post without re-deriving
/// what it cares about from scratch.
struct Snapshot {
    agent_running: bool,
    pending: Vec<String>,
    streaming: tui::app::test_harness::StreamingState,
}

impl Snapshot {
    fn capture(app: &TestApp) -> Self {
        Self {
            agent_running: app.agent_running(),
            pending: app.pending_tool_call_ids(),
            streaming: app.streaming_state(),
        }
    }
}

/// Post-dispatch invariant tied to a specific `FuzzOp`. Holds just the
/// payload the check needs (e.g. `call_id`); the pre/post `Snapshot`s carry
/// everything else. Variants gated on `agent_running` self-skip in idle
/// dispatch, where the relevant event arms are no-ops.
enum PostCheck {
    None,
    /// `Text` commits any streaming text buffer.
    TextFlushed,
    /// `ToolStarted` flushes streaming text + thinking and adds `call_id`
    /// to pending.
    ToolStarted { call_id: String },
    /// `ToolFinished` clears `call_id` from pending — but only verifiable
    /// when it was actually present beforehand.
    ToolFinished { call_id: String },
    /// `ExecDone` runs `finalize_exec`, which clears `stream_exec_id`.
    ExecCleared,
}

fn run_check(check: PostCheck, pre: &Snapshot, post: &Snapshot) {
    match check {
        PostCheck::None => {}
        PostCheck::TextFlushed => {
            if pre.agent_running {
                assert!(
                    !post.streaming.text,
                    "EngineEvent::Text left streaming text active: {:?}",
                    post.streaming
                );
            }
        }
        PostCheck::ToolStarted { call_id } => {
            if pre.agent_running {
                assert!(
                    !post.streaming.text && !post.streaming.thinking,
                    "ToolStarted({call_id}) left streaming active: {:?}",
                    post.streaming
                );
                assert!(
                    post.pending.iter().any(|p| p == &call_id),
                    "ToolStarted({call_id}) did not appear in pending: {:?}",
                    post.pending
                );
            }
        }
        PostCheck::ToolFinished { call_id } => {
            if pre.pending.iter().any(|p| p == &call_id) {
                assert!(
                    !post.pending.iter().any(|p| p == &call_id),
                    "ToolFinished({call_id}) left pending entry: {:?}",
                    post.pending
                );
            }
        }
        PostCheck::ExecCleared => {
            assert!(
                !post.streaming.exec,
                "ExecDone left active-exec state: {:?}",
                post.streaming
            );
        }
    }
}

/// Translate a `FuzzOp` into the `SourceEvent` to feed and the post-check
/// to run after. `None` for the event means the op was handled inline (via
/// a harness side channel) and the caller should not feed anything.
fn plan(op: FuzzOp) -> (Option<SourceEvent>, PostCheck) {
    match op {
        FuzzOp::KeyUnicode(raw) => {
            let c = decode_codepoint(raw);
            let ev = SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::NONE));
            (Some(ev), PostCheck::None)
        }
        FuzzOp::KeyCtrl(b) => {
            let c = (b'a' + (b % 26)) as char;
            let ev = SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::CONTROL));
            (Some(ev), PostCheck::None)
        }
        FuzzOp::KeyShift(b) => {
            let c = (b'a' + (b % 26)) as char;
            let ev = SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::SHIFT));
            (Some(ev), PostCheck::None)
        }
        FuzzOp::KeySpecial(which) => {
            let code = SPECIALS[(which as usize) % SPECIALS.len()];
            (
                Some(SourceEvent::Term(key_event(code, KeyModifiers::NONE))),
                PostCheck::None,
            )
        }
        FuzzOp::Paste(s) => (Some(SourceEvent::Term(TermEvent::Paste(s))), PostCheck::None),
        FuzzOp::Tick(ms) => (Some(SourceEvent::Tick(u64::from(ms))), PostCheck::None),
        FuzzOp::LuaWakeup => (Some(SourceEvent::LuaWakeup), PostCheck::None),
        FuzzOp::Resize { w, h } => (
            Some(SourceEvent::Resize {
                width: clamp_dim(w),
                height: clamp_dim(h),
            }),
            PostCheck::None,
        ),
        // Side-channel: not a SourceEvent.
        FuzzOp::StartTurn(_) => (None, PostCheck::None),
        FuzzOp::EngineReady => (Some(SourceEvent::Engine(EngineEvent::Ready)), PostCheck::None),
        FuzzOp::EngineText(s) => (
            Some(SourceEvent::Engine(EngineEvent::Text { content: s })),
            PostCheck::TextFlushed,
        ),
        FuzzOp::EngineTextDelta(s) => (
            Some(SourceEvent::Engine(EngineEvent::TextDelta { delta: s })),
            PostCheck::None,
        ),
        FuzzOp::EngineThinkingDelta(s) => (
            Some(SourceEvent::Engine(EngineEvent::ThinkingDelta { delta: s })),
            PostCheck::None,
        ),
        FuzzOp::EngineToolStart { call_id, tool_name } => {
            let cid = call_id_string(call_id);
            let ev = SourceEvent::Engine(EngineEvent::ToolStarted {
                call_id: cid.clone(),
                tool_name,
                args: HashMap::new(),
            });
            (Some(ev), PostCheck::ToolStarted { call_id: cid })
        }
        FuzzOp::EngineToolOutput { call_id, chunk } => (
            Some(SourceEvent::Engine(EngineEvent::ToolOutput {
                call_id: call_id_string(call_id),
                chunk,
            })),
            PostCheck::None,
        ),
        FuzzOp::EngineToolFinish {
            call_id,
            is_error,
            content,
        } => {
            let cid = call_id_string(call_id);
            let ev = SourceEvent::Engine(EngineEvent::ToolFinished {
                call_id: cid.clone(),
                result: ToolOutcome {
                    content,
                    is_error,
                    metadata: None,
                },
                elapsed_ms: Some(0),
            });
            (Some(ev), PostCheck::ToolFinished { call_id: cid })
        }
        FuzzOp::ExecOutput(s) => (Some(SourceEvent::ExecOutput(s)), PostCheck::None),
        FuzzOp::ExecDone(code) => (Some(SourceEvent::ExecDone(code)), PostCheck::ExecCleared),
    }
}

/// Apply one `FuzzOp` to a `TestApp`. Every op rolls through the same path:
/// pre-snapshot → feed event (or side-channel) → post-snapshot → check →
/// global invariants.
pub fn apply(app: &mut TestApp, op: FuzzOp) {
    // `StartTurn` is the one side channel: it pokes the harness directly
    // instead of going through `feed_one_within_budget`.
    if let FuzzOp::StartTurn(id) = op {
        app.start_turn(u64::from(id));
        app.assert_invariants();
        return;
    }

    let pre = Snapshot::capture(app);
    let (ev, check) = plan(op);
    if let Some(ev) = ev {
        app.feed_one_within_budget(ev, AllocBudget::DEFAULT);
    }
    let post = Snapshot::capture(app);
    run_check(check, &pre, &post);
    app.assert_invariants();
}

/// Build a fresh `TestApp` configured for the scenario's initial state.
/// Bypasses the invariant-only path so visual replay code can advance
/// step-by-step.
pub fn build_app(scenario: &Scenario) -> TestApp {
    TestApp::builder()
        .with_vim(scenario.vim)
        .with_mode(scenario.mode.into())
        .build()
}

/// Apply the first `n` ops from `scenario` to `app`. Used by replay
/// drivers that need to rewind to an earlier step by rebuilding and
/// fast-forwarding.
pub fn apply_n(app: &mut TestApp, scenario: &Scenario, n: usize) {
    let n = n.min(scenario.ops.len()).min(MAX_OPS);
    for op in scenario.ops.iter().take(n).cloned() {
        apply(app, op);
        if app.quit_requested() {
            break;
        }
    }
}

/// Drive a fresh `TestApp` through a scenario from start to finish.
/// Returns when the scenario is exhausted or the app requests quit.
/// Used by the fuzz target itself and by any external replay code that
/// just wants to re-run a scenario to confirm a crash.
pub fn run_scenario(scenario: Scenario) {
    let mut app = TestApp::builder()
        .with_vim(scenario.vim)
        .with_mode(scenario.mode.into())
        .build();
    let theme_baseline = smelt_style::theme::registry_len();
    let ns_baseline = smelt_buffer::buffer::namespace_count();

    let take = scenario.ops.len().min(MAX_OPS);
    for op in scenario.ops.into_iter().take(take) {
        apply(&mut app, op);
        if app.quit_requested() {
            break;
        }
    }

    let theme_end = smelt_style::theme::registry_len();
    let ns_end = smelt_buffer::buffer::namespace_count();
    assert!(
        theme_end <= theme_baseline + INTERN_SLACK,
        "theme registry leaked: {} -> {} (slack {})",
        theme_baseline,
        theme_end,
        INTERN_SLACK
    );
    assert!(
        ns_end <= ns_baseline + INTERN_SLACK,
        "namespace registry leaked: {} -> {} (slack {})",
        ns_baseline,
        ns_end,
        INTERN_SLACK
    );
}
