//! Shared types for the smelt fuzz target and the `crash_to_scenario`
//! converter. The on-disk scenario format is a JSON-serialized
//! [`Scenario`] — also the exact shape libFuzzer's `arbitrary` decoder
//! produces, so a crash artifact round-trips into a readable file with no
//! lossy translation.

use arbitrary::Arbitrary;
use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
};
use protocol::{AgentMode, Content, EngineEvent, Message, TokenUsage, ToolOutcome};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use protocol::UiCommand;
use tui::app::test_harness::{Action, AllocBudget, SourceEvent};

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
    EngineThinking(String),
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

    /// Side channel: prime the compact epoch so the next
    /// `CompactionComplete` is treated as fresh (apply path) rather than
    /// stale (fast-finish path). Mirrors what a real `compact_history`
    /// call does, without engaging the engine.
    BeginCompaction,
    /// Synthesize a `CompactionComplete` carrying `msg_count` user/
    /// assistant messages. Empty payload exercises the early-return arm.
    EngineCompactionComplete {
        msg_count: u8,
    },
    /// Emit `TurnComplete` against the currently-active turn (when any),
    /// carrying `msg_count` synthetic messages and a zero `TurnMeta`. Idle
    /// dispatch also accepts this and replaces history when non-empty.
    EngineTurnComplete {
        msg_count: u8,
    },
    /// Emit `TurnError`. Active-turn dispatch ends the turn; idle dispatch
    /// surfaces a notification.
    EngineTurnError(String),
    /// Emit `Steered { text, count }`. Active-turn dispatch flushes both
    /// streams and drains up to `count` queued messages.
    EngineSteered {
        text: String,
        count: u8,
    },
    /// Emit `Retrying { delay_ms, attempt }`. Active-turn dispatch puts
    /// the working bar into `TurnPhase::Retrying`.
    EngineRetrying {
        delay_ms: u16,
        attempt: u8,
    },
    /// Emit `TokenUsage`. Active-turn dispatch accumulates cost, updates
    /// `context_tokens`, and re-enters `TurnPhase::Working`.
    EngineTokenUsage {
        prompt: u16,
        completion: u16,
        tps: u16,
        cost_cents: u8,
        background: bool,
    },
    /// Side channel: push a synthetic entry onto `queued_messages` so
    /// `Steered` has something to drain.
    PushQueuedMessage(String),
    /// Side channel: flip `pending_title` so a subsequent `TitleGenerated`
    /// applies instead of being dropped.
    PrimePendingTitle,
    /// Emit `ProcessCompleted { id, exit_code }`. Pushes a transcript
    /// block describing the exit.
    EngineProcessCompleted {
        id: String,
        exit_code: Option<i32>,
    },
    /// Emit `Messages` against the currently-active turn (when any),
    /// carrying `msg_count` synthetic messages. Doesn't end the turn.
    EngineMessages {
        msg_count: u8,
    },
    /// Emit `TitleGenerated`. Applies only when `pending_title` is set.
    EngineTitleGenerated {
        title: String,
        slug: String,
    },
    /// Emit `RequestPermission`. Active-turn dispatch either auto-approves
    /// (queuing a `PermissionDecision`), defers the dialog onto the
    /// `pending_dialogs` queue (when a keystroke landed within
    /// `CONFIRM_DEFER_MS`), or registers a confirm dialog the user
    /// resolves. `request_id` is derived from `req_id` to round-trip
    /// through `PermissionDecision`.
    EngineRequestPermission {
        req_id: u8,
        call_id: u8,
        tool_name: String,
        confirm_message: String,
    },
    /// Side channel: approve the oldest pending confirm. Mirrors what
    /// `lua_handlers::handle_dialog_decision` does with `ConfirmChoice::Yes`.
    /// No-op when no confirm is pending.
    ApproveFirstConfirm,
    /// Side channel: deny the oldest pending confirm with `ConfirmChoice::No`.
    /// When `message` is `None`, denying cancels the agent turn (production
    /// `resolve_confirm` returns `true`); when `Some`, the turn continues.
    DenyFirstConfirm {
        message: Option<String>,
    },
    /// Emit `ToolDispatch`. With no Lua tool registered for `tool_name`,
    /// `execute_tool` returns `Immediate { is_error: true }` and the TUI
    /// sends a `ToolResult` UiCommand back. Exercises the "unknown tool"
    /// error path and proves `handle_tool_call` is panic-free against
    /// arbitrary names and arg payloads.
    EngineToolDispatch {
        req_id: u8,
        call_id: u8,
        tool_name: String,
    },
    /// Emit `ToolHooksRequest`. Active-turn dispatch always queues a
    /// `ToolHooksResponse` UiCommand back; `evaluate_hooks` short-circuits
    /// when no Lua hook is registered.
    EngineToolHooksRequest {
        req_id: u8,
        call_id: u8,
        tool_name: String,
    },
    /// Emit `CoreToolResult`. Active-turn dispatch routes to
    /// `lua.resolve_core_tool_call`, which is a no-op when no pending
    /// coroutine carries the `request_id`. Fuzz value: prove
    /// table-construction and `resolve_external` don't panic on arbitrary
    /// payloads.
    EngineCoreToolResult {
        req_id: u8,
        content: String,
        is_error: bool,
    },
    /// Emit `Shutdown`. Active-turn dispatch yields `SessionControl::Done`
    /// and ends the turn; idle dispatch is a no-op.
    EngineShutdown {
        reason: Option<String>,
    },
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

/// Compress the random `u8` call-id space down to `CALL_ID_BUCKETS` so
/// `ToolStarted` and `ToolFinished` actually pair up under random fuzz
/// inputs. Without this, collisions happen at 1/256 per pair, which the
/// coverage report shows is too rare to ever exercise the matching
/// branches in `handle_engine_event`.
const CALL_ID_BUCKETS: u8 = 8;

fn call_id_string(id: u8) -> String {
    format!("call-{:02x}", id % CALL_ID_BUCKETS)
}

/// Synthesize a deterministic, lightweight message vector for compaction
/// payloads. Alternating user/assistant content keeps the rebuild-screen
/// path through `restore_screen` honest without coupling to engine
/// internals.
fn synth_messages(count: usize) -> Vec<Message> {
    (0..count)
        .map(|i| {
            let body = format!("compacted-{i}");
            if i % 2 == 0 {
                Message::user(Content::text(body))
            } else {
                Message::assistant(Some(Content::text(body)), None, None)
            }
        })
        .collect()
}

/// Cheap snapshot of state needed by event-specific post-checks. Captured
/// before dispatch so a `PostCheck` can compare pre/post without re-deriving
/// what it cares about from scratch.
struct Snapshot {
    agent_running: bool,
    pending: Vec<String>,
    streaming: tui::app::test_harness::StreamingState,
    session_messages: usize,
    compact_epoch_match: bool,
    snapshot_counts: (usize, usize, usize),
    queued_messages: usize,
    working: tui::app::test_harness::WorkingSnapshot,
    session_cost_usd: f64,
    context_tokens: Option<u32>,
    transcript_blocks: usize,
    pending_title: bool,
    session_title: Option<String>,
    session_slug: Option<String>,
    pending_confirms: usize,
    deferred_dialogs: usize,
    /// Length of the harness's action log at capture time. Use
    /// `app.actions_since(snapshot.action_count)` to inspect actions
    /// produced after the snapshot, replacing the previous per-UiCommand
    /// counter fields.
    action_count: usize,
}

impl Snapshot {
    fn capture(app: &TestApp) -> Self {
        Self {
            agent_running: app.agent_running(),
            pending: app.pending_tool_call_ids(),
            streaming: app.streaming_state(),
            session_messages: app.session_message_count(),
            compact_epoch_match: app.compact_epoch_match(),
            snapshot_counts: app.snapshot_counts(),
            queued_messages: app.queued_message_count(),
            working: app.working_state(),
            session_cost_usd: app.session_cost_usd(),
            context_tokens: app.context_tokens(),
            transcript_blocks: app.transcript_block_count(),
            pending_title: app.pending_title(),
            session_title: app.session_title(),
            session_slug: app.session_slug(),
            pending_confirms: app.pending_confirm_count(),
            deferred_dialogs: app.pending_deferred_dialog_count(),
            action_count: app.actions().len(),
        }
    }
}

fn count_action<F>(actions: &[Action], pred: F) -> usize
where
    F: Fn(&UiCommand) -> bool,
{
    actions
        .iter()
        .filter(|a| matches!(a, Action::EngineSend(cmd) if pred(cmd)))
        .count()
}

/// Post-dispatch invariant tied to a specific `FuzzOp`. Holds just the
/// payload the check needs (e.g. `call_id`); the pre/post `Snapshot`s carry
/// everything else. Variants gated on `agent_running` self-skip in idle
/// dispatch, where the relevant event arms are no-ops.
enum PostCheck {
    None,
    /// `Text` commits any streaming text buffer. Cascade: `flush_streaming_text`
    /// also flushes thinking, so both buffers must be empty after.
    TextFlushed,
    /// `Thinking { content }` commits any streaming thinking buffer.
    ThinkingFlushed,
    /// `ToolStarted` flushes streaming text + thinking and adds `call_id`
    /// to pending.
    ToolStarted { call_id: String },
    /// `ToolOutput` for an already-pending `call_id` is a pure append to
    /// that tool's output; the pending entry stays put.
    ToolOutput { call_id: String },
    /// `ToolFinished` clears `call_id` from pending — but only verifiable
    /// when it was actually present beforehand.
    ToolFinished { call_id: String },
    /// `ExecDone` runs `finalize_exec`, which clears `stream_exec_id`.
    ExecCleared,
    /// `CompactionComplete` with `msg_count` messages. When the pre-state
    /// had a matching compact epoch and `msg_count > 0`, the apply path
    /// must replace `session.messages` and drain all snapshot vectors.
    CompactionApplied { msg_count: usize },
    /// `TurnComplete` against the active turn. Non-empty messages replace
    /// session.messages; an active turn ends.
    TurnCompleted {
        msg_count: usize,
        targeted_active: bool,
    },
    /// `TurnError`. When an active turn was running, it ends.
    TurnErrored,
    /// `Steered`. Active-turn dispatch flushes streams and drains up to
    /// `count` queued messages.
    Steered { count: usize },
    /// `Retrying`. Active-turn dispatch moves working into `Retrying`
    /// phase (still animating).
    Retrying,
    /// `TokenUsage`. Accumulates cost monotonically and (when non-background
    /// and prompt > 0) updates `context_tokens`.
    TokenUsageReceived {
        prompt: u32,
        cost_usd: f64,
        background: bool,
    },
    /// `ProcessCompleted` pushes one transcript block describing the exit.
    ProcessCompleted,
    /// `Messages` against the active turn (matching turn_id) replaces
    /// `session.messages` mid-turn; idle dispatch is a no-op.
    MessagesReplaced {
        msg_count: usize,
        targeted_active: bool,
    },
    /// `TitleGenerated` only applies when `pending_title` was set.
    TitleApplied { title: String, slug: String },
    /// `RequestPermission` against an active turn lands on exactly one of
    /// three branches: auto-approve (one new `PermissionDecision`,
    /// no new confirm), defer (one new `pending_dialogs` entry, no new
    /// confirm or decision), or register (one new entry in `core.confirms`,
    /// no decision yet).
    PermissionRequested,
    /// Approving / denying a confirm consumes one pending entry and queues
    /// a `PermissionDecision`. Approve never ends the turn; deny without a
    /// message ends it.
    ConfirmResolved { approved: bool, had_message: bool },
    /// `ToolDispatch` with an unregistered tool produces exactly one
    /// `ToolResult` UiCommand (the synthetic error path). With a real Lua
    /// tool registered it could yield `Pending` instead, but the harness
    /// loads no tools so strict equality holds.
    ToolDispatched,
    /// `ToolHooksRequest` always produces exactly one `ToolHooksResponse`
    /// UiCommand, regardless of whether hooks are registered.
    ToolHooksRequested,
    /// `CoreToolResult` with no pending Lua coroutine is silently dropped;
    /// only the no-panic invariant matters.
    CoreToolResultReceived,
    /// `Shutdown` ends any active turn.
    ShutdownReceived,
}

fn run_check(check: PostCheck, pre: &Snapshot, post: &Snapshot, new_actions: &[Action]) {
    match check {
        PostCheck::None => {}
        PostCheck::TextFlushed => {
            if pre.agent_running {
                assert!(
                    !post.streaming.text && !post.streaming.thinking,
                    "EngineEvent::Text left streaming active: {:?}",
                    post.streaming
                );
            }
        }
        PostCheck::ThinkingFlushed => {
            if pre.agent_running {
                assert!(
                    !post.streaming.thinking,
                    "EngineEvent::Thinking left streaming thinking active: {:?}",
                    post.streaming
                );
            }
        }
        PostCheck::ToolOutput { call_id } => {
            if pre.pending.iter().any(|p| p == &call_id) {
                let count = post.pending.iter().filter(|p| *p == &call_id).count();
                assert!(
                    count == 1,
                    "ToolOutput({call_id}) disturbed pending entry: {count} entries, pre {:?} post {:?}",
                    pre.pending,
                    post.pending
                );
            }
        }
        PostCheck::ToolStarted { call_id } => {
            if pre.agent_running {
                let was_pending = pre.pending.iter().any(|p| p == &call_id);
                if !was_pending {
                    // Fresh ToolStarted flushes streaming text + thinking
                    // before pushing the tool block.
                    assert!(
                        !post.streaming.text && !post.streaming.thinking,
                        "ToolStarted({call_id}) left streaming active: {:?}",
                        post.streaming
                    );
                }
                // Duplicate dispatch must be a no-op on pending; either way,
                // the call_id ends up in `pending` exactly once.
                let count = post.pending.iter().filter(|p| *p == &call_id).count();
                assert!(
                    count == 1,
                    "ToolStarted({call_id}) yields {count} pending entries, expected 1: {:?}",
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
        PostCheck::TurnCompleted {
            msg_count,
            targeted_active,
        } => {
            // Either active-turn dispatch (matching turn_id) or idle
            // dispatch with non-empty messages replaces history.
            let history_path = targeted_active || msg_count > 0;
            if history_path {
                assert_eq!(
                    post.session_messages, msg_count,
                    "TurnComplete did not replace session.messages: post {} (expected {msg_count})",
                    post.session_messages,
                );
            }
            if targeted_active {
                assert!(
                    !post.agent_running,
                    "TurnComplete against active turn did not end it",
                );
            }
        }
        PostCheck::TurnErrored => {
            if pre.agent_running {
                assert!(
                    !post.agent_running,
                    "TurnError did not end the active turn",
                );
            }
        }
        PostCheck::TokenUsageReceived {
            prompt,
            cost_usd,
            background,
        } => {
            // Idle dispatch drops TokenUsage entirely (falls through to
            // the `_ => {}` arm); only the active-turn path mutates
            // session state.
            if pre.agent_running {
                let expected = pre.session_cost_usd + cost_usd;
                assert!(
                    (post.session_cost_usd - expected).abs() < 1e-6,
                    "TokenUsage did not add cost {cost_usd}: pre {} → post {} (expected {expected})",
                    pre.session_cost_usd,
                    post.session_cost_usd,
                );
                if !background && prompt > 0 {
                    assert_eq!(
                        post.context_tokens,
                        Some(prompt),
                        "TokenUsage(prompt={prompt}, background=false) did not set context_tokens",
                    );
                }
            } else {
                assert_eq!(
                    post.session_cost_usd, pre.session_cost_usd,
                    "TokenUsage in idle dispatch should not accumulate cost",
                );
            }
        }
        PostCheck::ProcessCompleted => {
            assert_eq!(
                post.transcript_blocks,
                pre.transcript_blocks + 1,
                "ProcessCompleted did not push exactly one transcript block: pre {} → post {}",
                pre.transcript_blocks,
                post.transcript_blocks,
            );
        }
        PostCheck::MessagesReplaced {
            msg_count,
            targeted_active,
        } => {
            // Only the active-turn arm with matching turn_id calls
            // set_history; idle dispatch is an explicit no-op.
            if targeted_active {
                assert_eq!(
                    post.session_messages, msg_count,
                    "Messages did not replace session.messages: post {} (expected {msg_count})",
                    post.session_messages,
                );
                // Doesn't end the turn.
                assert!(
                    post.agent_running,
                    "Messages on active turn ended it unexpectedly",
                );
            } else {
                assert_eq!(
                    post.session_messages, pre.session_messages,
                    "Messages in idle dispatch should not change session.messages",
                );
            }
        }
        PostCheck::TitleApplied { title, slug } => {
            if pre.pending_title {
                assert_eq!(
                    post.session_title.as_deref(),
                    Some(title.as_str()),
                    "TitleGenerated did not set session.title",
                );
                assert_eq!(
                    post.session_slug.as_deref(),
                    Some(slug.as_str()),
                    "TitleGenerated did not set session.slug",
                );
                assert!(
                    !post.pending_title,
                    "TitleGenerated did not clear pending_title flag",
                );
            } else {
                assert_eq!(
                    post.session_title, pre.session_title,
                    "TitleGenerated without pending_title should not change title",
                );
                assert_eq!(
                    post.session_slug, pre.session_slug,
                    "TitleGenerated without pending_title should not change slug",
                );
            }
        }
        PostCheck::Retrying => {
            if pre.agent_running {
                assert!(
                    post.working.animating,
                    "Retrying on active turn left working idle: {:?}",
                    post.working
                );
            }
        }
        PostCheck::Steered { count } => {
            // Idle dispatch is a no-op for Steered; only enforce on active.
            if pre.agent_running {
                assert!(
                    !post.streaming.text && !post.streaming.thinking,
                    "Steered left streaming active: {:?}",
                    post.streaming
                );
                let drained = count.min(pre.queued_messages);
                assert_eq!(
                    post.queued_messages,
                    pre.queued_messages - drained,
                    "Steered(count={count}) drain mismatch: pre {} → post {} (expected drain {})",
                    pre.queued_messages,
                    post.queued_messages,
                    drained,
                );
            }
        }
        PostCheck::PermissionRequested => {
            // Idle dispatch falls through to the `_ => {}` arm in
            // `handle_idle_engine_event` — no state change. Only enforce
            // the trichotomy when a turn was running.
            if pre.agent_running {
                let new_confirms = post.pending_confirms.saturating_sub(pre.pending_confirms);
                let new_deferred = post.deferred_dialogs.saturating_sub(pre.deferred_dialogs);
                let new_decisions = count_action(new_actions, |c| {
                    matches!(c, UiCommand::PermissionDecision { .. })
                });
                let total = new_confirms + new_deferred + new_decisions;
                assert_eq!(
                    total, 1,
                    "RequestPermission produced {total} effects (confirms={new_confirms}, deferred={new_deferred}, decisions={new_decisions}), expected exactly 1",
                );
            }
        }
        PostCheck::ConfirmResolved {
            approved,
            had_message,
        } => {
            // Side-channel returns false when no confirm was pending —
            // skip in that case.
            if pre.pending_confirms == 0 {
                return;
            }
            assert_eq!(
                post.pending_confirms,
                pre.pending_confirms - 1,
                "Resolve did not remove one confirm: pre {} -> post {}",
                pre.pending_confirms,
                post.pending_confirms,
            );
            let new_decisions = count_action(new_actions, |c| {
                matches!(c, UiCommand::PermissionDecision { .. })
            });
            assert_eq!(
                new_decisions, 1,
                "Resolve did not queue exactly one PermissionDecision (got {new_decisions})",
            );
            // Approve never ends the turn. Deny without a message ends it
            // (resolve_confirm returns true → discard_turn). Deny with a
            // message keeps the turn alive — the user sent steering text
            // instead of stopping.
            if pre.agent_running {
                let should_end = !approved && !had_message;
                if should_end {
                    assert!(
                        !post.agent_running,
                        "Deny without message did not end the turn",
                    );
                } else {
                    assert!(
                        post.agent_running,
                        "{} ended the turn unexpectedly",
                        if approved { "Approve" } else { "Deny with message" },
                    );
                }
            }
        }
        PostCheck::ToolDispatched => {
            if pre.agent_running {
                let new_results =
                    count_action(new_actions, |c| matches!(c, UiCommand::ToolResult { .. }));
                assert_eq!(
                    new_results, 1,
                    "ToolDispatch with no tool registered should queue exactly one ToolResult (got {new_results})",
                );
            }
        }
        PostCheck::ToolHooksRequested => {
            if pre.agent_running {
                let new_responses = count_action(new_actions, |c| {
                    matches!(c, UiCommand::ToolHooksResponse { .. })
                });
                assert_eq!(
                    new_responses, 1,
                    "ToolHooksRequest should queue exactly one ToolHooksResponse (got {new_responses})",
                );
            }
        }
        PostCheck::CoreToolResultReceived => {
            // No-op when no pending coroutine matches the request_id.
            // `assert_invariants` covers the no-panic property.
        }
        PostCheck::ShutdownReceived => {
            if pre.agent_running {
                assert!(
                    !post.agent_running,
                    "Shutdown did not end the active turn",
                );
            }
        }
        PostCheck::CompactionApplied { msg_count } => {
            // The apply path runs only on epoch match. A non-empty payload
            // replaces the conversation and clears snapshot vectors. Idle
            // dispatch also routes non-empty payloads through the apply
            // path, but skip the assertions when we can't tell which arm
            // ran (e.g. an active turn that wasn't compacting).
            if pre.compact_epoch_match && msg_count > 0 {
                assert_eq!(
                    post.session_messages, msg_count,
                    "CompactionComplete did not replace session.messages: pre {} → post {} (expected {msg_count})",
                    pre.session_messages, post.session_messages,
                );
                assert_eq!(
                    post.snapshot_counts,
                    (0, 0, 0),
                    "CompactionComplete did not clear snapshot vectors: {:?}",
                    post.snapshot_counts,
                );
                // apply_compaction calls working.finish(Done): live → None.
                assert!(
                    !post.working.compacting,
                    "CompactionComplete left working in compacting phase: {:?}",
                    post.working,
                );
            }
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
        FuzzOp::EngineThinking(s) => (
            Some(SourceEvent::Engine(EngineEvent::Thinking { content: s })),
            PostCheck::ThinkingFlushed,
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
        FuzzOp::EngineToolOutput { call_id, chunk } => {
            let cid = call_id_string(call_id);
            let ev = SourceEvent::Engine(EngineEvent::ToolOutput {
                call_id: cid.clone(),
                chunk,
            });
            (Some(ev), PostCheck::ToolOutput { call_id: cid })
        }
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
        // Side-channel: not a SourceEvent.
        FuzzOp::BeginCompaction => (None, PostCheck::None),
        FuzzOp::EngineCompactionComplete { msg_count } => {
            let count = usize::from(msg_count);
            let ev = SourceEvent::Engine(EngineEvent::CompactionComplete {
                messages: synth_messages(count),
            });
            (Some(ev), PostCheck::CompactionApplied { msg_count: count })
        }
        FuzzOp::EngineTurnComplete { .. } => {
            // Needs the live turn_id (not accessible here); handled
            // inline in `apply` before reaching `plan`.
            unreachable!("EngineTurnComplete handled inline in apply()")
        }
        FuzzOp::EngineTurnError(message) => {
            let ev = SourceEvent::Engine(EngineEvent::TurnError { message });
            (Some(ev), PostCheck::TurnErrored)
        }
        FuzzOp::EngineSteered { text, count } => {
            let n = usize::from(count);
            let ev = SourceEvent::Engine(EngineEvent::Steered { text, count: n });
            (Some(ev), PostCheck::Steered { count: n })
        }
        FuzzOp::EngineRetrying { delay_ms, attempt } => {
            let ev = SourceEvent::Engine(EngineEvent::Retrying {
                delay_ms: u64::from(delay_ms),
                attempt: u32::from(attempt),
            });
            (Some(ev), PostCheck::Retrying)
        }
        FuzzOp::EngineTokenUsage {
            prompt,
            completion,
            tps,
            cost_cents,
            background,
        } => {
            let prompt = u32::from(prompt);
            let completion = u32::from(completion);
            let cost_usd = f64::from(cost_cents) / 100.0;
            let usage = TokenUsage {
                prompt_tokens: Some(prompt),
                completion_tokens: Some(completion),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            };
            let ev = SourceEvent::Engine(EngineEvent::TokenUsage {
                usage,
                tokens_per_sec: Some(f64::from(tps)),
                cost_usd: Some(cost_usd),
                background,
            });
            (
                Some(ev),
                PostCheck::TokenUsageReceived {
                    prompt,
                    cost_usd,
                    background,
                },
            )
        }
        FuzzOp::PushQueuedMessage(_) => {
            unreachable!("PushQueuedMessage handled inline in apply()")
        }
        FuzzOp::PrimePendingTitle => unreachable!("PrimePendingTitle handled inline in apply()"),
        FuzzOp::EngineProcessCompleted { id, exit_code } => {
            let ev = SourceEvent::Engine(EngineEvent::ProcessCompleted { id, exit_code });
            (Some(ev), PostCheck::ProcessCompleted)
        }
        FuzzOp::EngineMessages { .. } => {
            // Needs the live turn_id; handled inline in `apply` before
            // reaching `plan`.
            unreachable!("EngineMessages handled inline in apply()")
        }
        FuzzOp::EngineTitleGenerated { title, slug } => {
            let ev = SourceEvent::Engine(EngineEvent::TitleGenerated {
                title: title.clone(),
                slug: slug.clone(),
            });
            (Some(ev), PostCheck::TitleApplied { title, slug })
        }
        FuzzOp::EngineRequestPermission {
            req_id,
            call_id,
            tool_name,
            confirm_message,
        } => {
            let ev = SourceEvent::Engine(EngineEvent::RequestPermission {
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: HashMap::new(),
                confirm_message,
                approval_patterns: Vec::new(),
                summary: None,
            });
            (Some(ev), PostCheck::PermissionRequested)
        }
        FuzzOp::ApproveFirstConfirm | FuzzOp::DenyFirstConfirm { .. } => {
            unreachable!("Approve/Deny side channels handled inline in apply()")
        }
        FuzzOp::EngineToolDispatch {
            req_id,
            call_id,
            tool_name,
        } => {
            let ev = SourceEvent::Engine(EngineEvent::ToolDispatch {
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: HashMap::new(),
            });
            (Some(ev), PostCheck::ToolDispatched)
        }
        FuzzOp::EngineToolHooksRequest {
            req_id,
            call_id,
            tool_name,
        } => {
            let ev = SourceEvent::Engine(EngineEvent::ToolHooksRequest {
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: HashMap::new(),
                mode: AgentMode::Normal,
            });
            (Some(ev), PostCheck::ToolHooksRequested)
        }
        FuzzOp::EngineCoreToolResult {
            req_id,
            content,
            is_error,
        } => {
            let ev = SourceEvent::Engine(EngineEvent::CoreToolResult {
                request_id: u64::from(req_id),
                content,
                is_error,
                metadata: None,
            });
            (Some(ev), PostCheck::CoreToolResultReceived)
        }
        FuzzOp::EngineShutdown { reason } => {
            let ev = SourceEvent::Engine(EngineEvent::Shutdown { reason });
            (Some(ev), PostCheck::ShutdownReceived)
        }
    }
}

/// Apply one `FuzzOp` to a `TestApp`. Every op rolls through the same path:
/// pre-snapshot → feed event (or side-channel) → post-snapshot → check →
/// global invariants.
pub fn apply(app: &mut TestApp, op: FuzzOp) {
    // Side channels — pokes that bypass `feed_one_within_budget`. Variants
    // carrying owned data (e.g. PushQueuedMessage(String)) are handled
    // with `if let` so they take ownership; the rest match by reference.
    if let FuzzOp::PushQueuedMessage(text) = op {
        app.push_queued_message(text);
        app.assert_invariants();
        return;
    }
    if let FuzzOp::DenyFirstConfirm { message } = op {
        let pre = Snapshot::capture(app);
        let had_message = message.is_some();
        app.resolve_first_confirm(false, message);
        let post = Snapshot::capture(app);
        let new_actions = app.actions_since(pre.action_count);
        run_check(
            PostCheck::ConfirmResolved {
                approved: false,
                had_message,
            },
            &pre,
            &post,
            new_actions,
        );
        check_turn_end_invariants(&pre, &post);
        app.assert_invariants();
        return;
    }
    match &op {
        FuzzOp::StartTurn(id) => {
            app.start_turn(u64::from(*id));
            app.assert_invariants();
            return;
        }
        FuzzOp::BeginCompaction => {
            app.begin_compaction();
            app.assert_invariants();
            return;
        }
        FuzzOp::PrimePendingTitle => {
            app.prime_pending_title();
            app.assert_invariants();
            return;
        }
        FuzzOp::ApproveFirstConfirm => {
            let pre = Snapshot::capture(app);
            app.resolve_first_confirm(true, None);
            let post = Snapshot::capture(app);
            let new_actions = app.actions_since(pre.action_count);
            run_check(
                PostCheck::ConfirmResolved {
                    approved: true,
                    had_message: false,
                },
                &pre,
                &post,
                new_actions,
            );
            check_turn_end_invariants(&pre, &post);
            app.assert_invariants();
            return;
        }
        _ => {}
    }

    let pre = Snapshot::capture(app);
    // `TurnComplete` is gated on `turn_id` matching the live agent — read
    // it here so the dispatched event hits the active arm whenever a turn
    // is running. Idle dispatch still applies for the `agent_running ==
    // false` case.
    let (ev, check) = match op {
        FuzzOp::EngineTurnComplete { msg_count } => {
            let count = usize::from(msg_count);
            let id = app.current_turn_id().unwrap_or(0);
            let ev = SourceEvent::Engine(EngineEvent::TurnComplete {
                turn_id: id,
                messages: synth_messages(count),
                meta: None,
            });
            (
                Some(ev),
                PostCheck::TurnCompleted {
                    msg_count: count,
                    targeted_active: pre.agent_running,
                },
            )
        }
        FuzzOp::EngineMessages { msg_count } => {
            let count = usize::from(msg_count);
            let id = app.current_turn_id().unwrap_or(0);
            let ev = SourceEvent::Engine(EngineEvent::Messages {
                turn_id: id,
                messages: synth_messages(count),
            });
            (
                Some(ev),
                PostCheck::MessagesReplaced {
                    msg_count: count,
                    targeted_active: pre.agent_running,
                },
            )
        }
        op => plan(op),
    };
    if let Some(ev) = ev {
        app.feed_one_within_budget(ev, AllocBudget::DEFAULT);
    }
    let post = Snapshot::capture(app);
    let new_actions = app.actions_since(pre.action_count);
    run_check(check, &pre, &post, new_actions);
    check_turn_end_invariants(&pre, &post);
    app.assert_invariants();
}

/// Cross-cutting invariants for any op that ends an active turn.
/// `finish_turn` always flushes streaming text + thinking before clearing
/// the agent; if either survives, the flush path was skipped.
fn check_turn_end_invariants(pre: &Snapshot, post: &Snapshot) {
    if pre.agent_running && !post.agent_running {
        assert!(
            !post.streaming.text && !post.streaming.thinking,
            "turn ended with streaming active: {:?}",
            post.streaming
        );
    }
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
    // Vim bang escape (`!cmd<CR>`) spawns a shell via `tokio::spawn`. With
    // no runtime entered, that panics. We use a current-thread runtime
    // that never drives the task queue, so spawn succeeds and the queued
    // shell command never actually runs — keeping fuzz iterations free
    // of real process / fs side effects.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for fuzz harness");
    let _guard = runtime.enter();

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
