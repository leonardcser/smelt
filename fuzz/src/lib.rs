//! Shared types for the smelt fuzz target and the `crash_to_scenario`
//! converter. The on-disk scenario format is a JSON-serialized
//! [`Scenario`] — also the exact shape libFuzzer's `arbitrary` decoder
//! produces, so a crash artifact round-trips into a readable file with no
//! lossy translation.

pub mod cache_common;
pub mod lua_loop;
pub mod shrink;
pub use lua_loop::{run_lua_scenario, LuaScenario};

use arbitrary::{Arbitrary, Unstructured};
use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use protocol::UiCommand;
use protocol::{
    AgentMode, Content, EngineAskError, EngineAskErrorKind, EngineEvent, Message, ReasoningBlock,
    TokenUsage, ToolOutcome,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tui::app::test_harness::{Action, AllocBudget, SourceEvent};

pub use tui::app::test_harness::TestApp;

/// Bounded JSON value tree for tool argument fuzzing. Production paths
/// (`evaluate_hooks`, `permissions.decide`, pattern matching in
/// `RequestPermission`) consume `HashMap<String, serde_json::Value>` —
/// feeding them all-empty bags reaches none of that logic. The
/// `Arbitrary` impl synthesises small (≤ 6 keys, depth ≤ 3) trees so
/// each op stays cheap.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArgsBag(pub HashMap<String, serde_json::Value>);

impl ArgsBag {
    pub fn into_map(self) -> HashMap<String, serde_json::Value> {
        self.0
    }
}

impl<'a> Arbitrary<'a> for ArgsBag {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let n = u.int_in_range(0u8..=6)? as usize;
        let mut map = HashMap::with_capacity(n);
        for _ in 0..n {
            let key: String = arb_short_string(u, 16)?;
            let val = arb_json(u, 3)?;
            map.insert(key, val);
        }
        Ok(ArgsBag(map))
    }
}

fn arb_short_string(u: &mut Unstructured<'_>, max_bytes: usize) -> arbitrary::Result<String> {
    let len = u.int_in_range(0..=max_bytes)?;
    let bytes: Vec<u8> = (0..len)
        .map(|_| u.arbitrary::<u8>())
        .collect::<Result<_, _>>()?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn arb_json(u: &mut Unstructured<'_>, depth: u8) -> arbitrary::Result<serde_json::Value> {
    if depth == 0 || u.is_empty() {
        return arb_json_leaf(u);
    }
    match u.int_in_range(0u8..=5)? {
        0 => arb_json_leaf(u),
        1 => Ok(serde_json::Value::String(arb_short_string(u, 16)?)),
        2 => {
            let n = u.int_in_range(0u8..=4)? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(arb_json(u, depth - 1)?);
            }
            Ok(serde_json::Value::Array(v))
        }
        3 => {
            let n = u.int_in_range(0u8..=4)? as usize;
            let mut m = serde_json::Map::with_capacity(n);
            for _ in 0..n {
                m.insert(arb_short_string(u, 16)?, arb_json(u, depth - 1)?);
            }
            Ok(serde_json::Value::Object(m))
        }
        _ => arb_json_leaf(u),
    }
}

fn arb_json_leaf(u: &mut Unstructured<'_>) -> arbitrary::Result<serde_json::Value> {
    match u.int_in_range(0u8..=4)? {
        0 => Ok(serde_json::Value::Null),
        1 => Ok(serde_json::Value::Bool(u.arbitrary()?)),
        2 => Ok(serde_json::json!(u.arbitrary::<i64>()?)),
        3 => {
            let f: f64 = u.arbitrary()?;
            Ok(serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null))
        }
        _ => Ok(serde_json::Value::String(arb_short_string(u, 16)?)),
    }
}

/// Mouse event payload. `kind`/`button`/`mods` index lookup tables
/// declared below so `arbitrary` byte-budget stays tiny.
#[derive(Arbitrary, Debug, Clone, Serialize, Deserialize)]
pub struct MouseFuzz {
    pub kind: u8,
    pub button: u8,
    pub col: u8,
    pub row: u8,
    pub mods: u8,
}

const MOUSE_BUTTONS: &[MouseButton] = &[MouseButton::Left, MouseButton::Right, MouseButton::Middle];

const ENGINE_ASK_ERROR_KINDS: &[EngineAskErrorKind] = &[
    EngineAskErrorKind::Network,
    EngineAskErrorKind::RateLimited,
    EngineAskErrorKind::Quota,
    EngineAskErrorKind::InvalidResponse,
    EngineAskErrorKind::ContextWindow,
    EngineAskErrorKind::Cancelled,
    EngineAskErrorKind::Other,
];

fn decode_mouse_kind(kind: u8, button: u8) -> MouseEventKind {
    let btn = MOUSE_BUTTONS[(button as usize) % MOUSE_BUTTONS.len()];
    match kind % 8 {
        0 => MouseEventKind::Down(btn),
        1 => MouseEventKind::Up(btn),
        2 => MouseEventKind::Drag(btn),
        3 => MouseEventKind::Moved,
        4 => MouseEventKind::ScrollDown,
        5 => MouseEventKind::ScrollUp,
        6 => MouseEventKind::ScrollLeft,
        _ => MouseEventKind::ScrollRight,
    }
}

fn decode_mouse_mods(mods: u8) -> KeyModifiers {
    let mut m = KeyModifiers::NONE;
    if mods & 0b001 != 0 {
        m |= KeyModifiers::CONTROL;
    }
    if mods & 0b010 != 0 {
        m |= KeyModifiers::SHIFT;
    }
    if mods & 0b100 != 0 {
        m |= KeyModifiers::ALT;
    }
    m
}

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
    /// Shift-modified special key — drives shift+arrow / shift+Home/End
    /// selection-extend code paths that plain `KeySpecial` skips.
    KeySpecialShift(u8),
    /// Bracketed paste with arbitrary UTF-8 payload.
    Paste(String),
    /// Mouse event (click/drag/wheel/move). Routes through
    /// `dispatch_terminal_event` → `handle_mouse` so the scrollbar,
    /// click-to-focus, drag-selection, and viewport-pan paths are
    /// reachable from fuzzing.
    Mouse(MouseFuzz),
    /// Advance the virtual clock by `ms` milliseconds.
    Tick(u16),
    /// Wake any pending Lua callbacks.
    LuaWakeup,
    /// Terminal resize, clamped to `[1, 400]` per dimension.
    Resize {
        w: u16,
        h: u16,
    },

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
        args: ArgsBag,
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
        summary: String,
        args: ArgsBag,
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
    /// Emit `ToolDispatch`. Unknown `tool_name`s take the `Immediate
    /// { is_error: true }` path and queue a `ToolResult` UiCommand back
    /// in-step; known tools (autoloaded Lua) may yield Pending and resolve
    /// asynchronously, queueing 0 results this step. Exercises the dispatch
    /// surface and proves `handle_tool_call` is panic-free against arbitrary
    /// names and arg payloads.
    EngineToolDispatch {
        req_id: u8,
        call_id: u8,
        tool_name: String,
        args: ArgsBag,
    },
    /// Emit `ToolHooksRequest`. Active-turn dispatch always queues a
    /// `ToolHooksResponse` UiCommand back; `evaluate_hooks` short-circuits
    /// when no Lua hook is registered.
    EngineToolHooksRequest {
        req_id: u8,
        call_id: u8,
        tool_name: String,
        args: ArgsBag,
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
    /// Side-channel: insert a synthetic image attachment at the prompt
    /// cursor through the real `Input::insert_image` path. Exercises the
    /// attachment_ids ↔ marker invariant under interleaved key + paste
    /// events.
    InsertAttachment {
        label: String,
    },
    /// Side-channel: flip pane focus between Prompt and Content. The Ctrl-W
    /// chord that reaches the same code path requires two keystrokes inside
    /// `PANE_CHORD_WINDOW`, which random fuzz inputs rarely hit.
    TogglePaneFocus,
    /// Emit `ToolArgsDelta`. The TUI accumulates `delta` into a per-call
    /// JSON-fragment buffer, then displays the reconstructed args when
    /// the matching `ToolStarted` arrives. Exercises the streaming-arg
    /// reassembly path against arbitrary out-of-order or orphan deltas.
    EngineToolArgsDelta {
        call_id: String,
        tool_name: String,
        delta: String,
    },
    /// Emit `EngineAskResponse`. Resumes a Lua coroutine that issued a
    /// one-shot `UiCommand::EngineAsk`; no-op when no coroutine is waiting
    /// on the synthesized `id`.
    EngineAskResponse {
        id: u64,
        content: String,
    },
    /// Emit `EngineAskResponse` with a typed error payload (empty content).
    /// Resumes a waiting Lua coroutine with the failure branch so plugins
    /// like compact.lua exercise their `err.kind` dispatch. `kind_idx` is
    /// modded against `ENGINE_ASK_ERROR_KINDS`.
    EngineAskError {
        id: u64,
        kind_idx: u8,
        message: String,
    },
    /// Side channel: invoke the `/reload` pipeline. Stresses named-slot
    /// survival (paint ids, buf/win/overlay NamedSlots) and `on_ready`
    /// re-entry (`ctx.kind = "reload"`) against arbitrary pre-state —
    /// the surface where the `fix(edit): drop named bindings on close`
    /// bug class lives.
    ReloadLua,
    /// Side channel: open a synthetic overlay via `smelt.overlay.new`.
    /// `variant % N` picks the shape (named/anonymous leaf, vbox, leaf
    /// with static measure, leaf with overlay-level keymap). Same
    /// `variant` reuses the same NamedSlot name so the dedup path is
    /// exercised, different variants populate different slots. Targets
    /// the new measure / overlay-keymap / NamedSlots surfaces.
    OpenOverlay {
        variant: u8,
    },
    /// Side channel: install a placeholder on the prompt window with the
    /// given text and accept / dismiss chord set. `variant % N` picks the
    /// chord pair (Tab/Esc, Enter/Esc, Ctrl-N/Ctrl-G, ...) so the routing
    /// branches in `dispatch_placeholder_key` are exercised against random
    /// follow-up keystrokes. The placeholder dispatcher fires on the next
    /// matching key while the buffer is empty.
    InstallPlaceholder {
        text: String,
        variant: u8,
    },
    /// Side channel: clear the prompt placeholder (extmark + opts). Mirrors
    /// `clear_placeholder` so scenarios can drop a placeholder without
    /// going through accept / dismiss.
    ClearPlaceholder,
    /// Side channel: emit `EngineAskResponse` with the smallest pending ask
    /// id, if any. Exercises `lua.fire_ask_callback` and plugin paths that
    /// depend on it (e.g., `/btw`, compaction plugins).
    EngineAskResponsePending {
        content: String,
    },
    /// Side channel: emit `EngineAskResponse` with a typed error payload
    /// against the smallest pending ask id, if any.
    EngineAskErrorPending {
        kind_idx: u8,
        message: String,
    },
    /// Side channel: open an exec block in the transcript so subsequent
    /// `ExecOutput`/`ExecDone` exercise the full lifecycle.
    StartExec {
        command: String,
    },
    /// Side channel: cancel the active turn (or idle background tasks).
    /// Exercises `discard_turn(true)` → flush streaming, send `Cancel`,
    /// `lua.cancel_tasks()`, and the interrupted-transcript path.
    Cancel,
    /// Emit `RequestPermission` with `tool_name = "bash"` so the
    /// permission hook evaluator routes through `bash::parse` and
    /// exercises multi-byte UTF-8 in the shell command parser.
    EngineRequestPermissionBash {
        req_id: u8,
        call_id: u8,
        command: String,
    },
    /// Side channel: push a steer text onto the queued-messages stack.
    /// Exercises the mid-turn steering queue growth path.
    Steer {
        text: String,
    },
    /// Side channel: remove up to `count` queued messages from the front.
    /// Exercises the mid-turn steering queue shrink path.
    Unsteer {
        count: u8,
    },
    /// Side channel: send a `CallCoreTool` UiCommand. Exercises the
    /// tool-call round-trip initiation path (pair with
    /// `EngineCoreToolResult` for the full lifecycle).
    CallCoreTool {
        tool_name: String,
        args: ArgsBag,
    },
    /// Side channel: change the active agent mode. Exercises mode-
    /// dependent rendering and permission rule-sets.
    SetAgentMode {
        mode: FuzzMode,
    },
}

impl FuzzOp {
    /// Short human label used by `play_scenario` for its status line.
    /// Lives next to the enum so adding a variant is one edit, not two —
    /// the per-variant match previously lived in `play_scenario.rs` and
    /// drifted every time a new op landed.
    pub fn label(&self) -> String {
        use FuzzOp::*;
        match self {
            KeyUnicode(c) => format!("key {:?}", char::from_u32(*c).unwrap_or('?')),
            KeyCtrl(b) => format!("ctrl-{}", (b'a' + (b % 26)) as char),
            KeyShift(b) => format!("shift-{}", (b'a' + (b % 26)) as char),
            KeySpecial(_) => "special".into(),
            KeySpecialShift(_) => "shift+special".into(),
            Paste(s) => format!("paste {} chars", s.chars().count()),
            Mouse(m) => format!("mouse k={} b={} {},{}", m.kind, m.button, m.col, m.row),
            Tick(ms) => format!("tick {ms}ms"),
            LuaWakeup => "lua wakeup".into(),
            Resize { w, h } => format!("resize {w}x{h}"),
            StartTurn(id) => format!("start turn {id}"),
            EngineReady => "engine ready".into(),
            EngineText(_) => "engine text".into(),
            EngineTextDelta(_) => "engine text delta".into(),
            EngineThinking(_) => "engine thinking".into(),
            EngineThinkingDelta(_) => "engine thinking delta".into(),
            EngineToolStart { tool_name, .. } => format!("tool start {tool_name}"),
            EngineToolOutput { .. } => "tool output".into(),
            EngineToolFinish { is_error, .. } => {
                if *is_error {
                    "tool error".into()
                } else {
                    "tool done".into()
                }
            }
            ExecOutput(_) => "exec output".into(),
            ExecDone(code) => format!("exec done {code:?}"),
            EngineTurnComplete { msg_count } => format!("turn complete ({msg_count} msgs)"),
            EngineTurnError(_) => "turn error".into(),
            EngineSteered { count, .. } => format!("steered (drain {count})"),
            EngineRetrying { attempt, .. } => format!("retrying (attempt {attempt})"),
            EngineTokenUsage { prompt, .. } => format!("token usage (prompt {prompt})"),
            PushQueuedMessage(_) => "push queued message".into(),
            EngineProcessCompleted { id, .. } => format!("process completed {id}"),
            EngineMessages { msg_count } => format!("messages ({msg_count})"),
            EngineRequestPermission { tool_name, .. } => {
                format!("request permission {tool_name}")
            }
            ApproveFirstConfirm => "approve confirm".into(),
            DenyFirstConfirm { .. } => "deny confirm".into(),
            EngineToolDispatch { tool_name, .. } => format!("tool dispatch {tool_name}"),
            EngineToolHooksRequest { tool_name, .. } => format!("tool hooks {tool_name}"),
            EngineCoreToolResult { .. } => "core tool result".into(),
            EngineShutdown { .. } => "shutdown".into(),
            InsertAttachment { label } => format!("insert attachment {label}"),
            TogglePaneFocus => "toggle pane focus".into(),
            EngineToolArgsDelta { tool_name, .. } => format!("tool args delta {tool_name}"),
            EngineAskResponse { id, .. } => format!("ask response {id}"),
            EngineAskError { id, kind_idx, .. } => format!("ask error {id} k={kind_idx}"),
            ReloadLua => "reload lua".into(),
            OpenOverlay { variant } => format!("open overlay v={variant}"),
            InstallPlaceholder { variant, .. } => format!("install placeholder v={variant}"),
            ClearPlaceholder => "clear placeholder".into(),
            EngineAskResponsePending { .. } => "ask response (pending)".into(),
            EngineAskErrorPending { .. } => "ask error (pending)".into(),
            StartExec { .. } => "start exec".into(),
            Cancel => "cancel".into(),
            EngineRequestPermissionBash { .. } => "request permission (bash)".into(),
            Steer { .. } => "steer".into(),
            Unsteer { count } => format!("unsteer {count}"),
            CallCoreTool { tool_name, .. } => format!("call core tool {tool_name}"),
            SetAgentMode { mode } => format!("set mode {mode:?}"),
        }
    }
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
/// `serde_json::to_string_pretty`. `Arbitrary` is **hand-written** to draw
/// per-scenario [`SwarmWeights`] up front, then sample ops from that
/// distribution (see [`build_fuzz_op`]). JSON serialisation still
/// round-trips losslessly via serde derive — the swarm table itself is
/// generator state and is not persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzInput {
    pub vim: bool,
    pub mode: FuzzMode,
    pub ops: Vec<FuzzOp>,
}

impl<'a> Arbitrary<'a> for FuzzInput {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let vim = u.arbitrary()?;
        let mode = u.arbitrary()?;
        // Swarm testing: each scenario commits to a corner of the op
        // space. Some variants are disabled outright; the rest get
        // wildly skewed weights. Drawing the table here (not per-op)
        // means a single seed goes deep into its chosen corner rather
        // than uniformly bouncing across 43 variants.
        let swarm = SwarmWeights::arbitrary(u, N_FUZZOP_VARIANTS)?;
        let mut ops = Vec::new();
        while !u.is_empty() && ops.len() < MAX_OPS {
            let idx = swarm.pick(u)?;
            ops.push(build_fuzz_op(idx, u)?);
        }
        Ok(FuzzInput { vim, mode, ops })
    }
}

/// Per-scenario swarm weights. Each entry is either `0` (variant
/// disabled for this seed) or a weight in `1..=100` (relative selection
/// weight). At least one entry is guaranteed non-zero. Inspired by
/// TigerBeetle's `random_enum_weights`: per seed, *zero out* most
/// variants and skew the rest wildly so each scenario commits to one
/// shape of workload rather than uniformly spreading thin.
///
/// Weights aren't persisted in the on-disk `Scenario` JSON — they're a
/// per-Arbitrary-draw artifact. Crashed scenarios round-trip through
/// the `ops` vector alone, which is what actually replays.
pub struct SwarmWeights {
    weights: Vec<u32>,
    total: u32,
}

impl SwarmWeights {
    /// Draw a fresh swarm table over `n` variants. One byte per slot:
    /// `byte < 64` disables the variant (25% disable rate); otherwise
    /// the weight is `byte - 63` (range `1..=192`). Single-byte cost
    /// matters — measured coverage A/B showed the prior 2-byte/variant
    /// encoding ate the entire entropy budget of the median 47-byte
    /// libFuzzer seed, leaving nothing for op bytes.
    pub fn arbitrary(u: &mut Unstructured<'_>, n: usize) -> arbitrary::Result<Self> {
        let mut weights = vec![0u32; n];
        let mut total = 0u32;
        for slot in &mut weights {
            let b = u.arbitrary::<u8>().unwrap_or(0);
            if b >= 64 {
                let w = (b - 63) as u32;
                *slot = w;
                total += w;
            }
        }
        if total == 0 {
            // Empty random draw (or exhausted input): fall back to a
            // uniform distribution rather than collapsing to a single
            // variant. Short seeds — common in libFuzzer's early corpus
            // growth — would otherwise only ever fire variant 0.
            weights.fill(1);
            total = n as u32;
        }
        Ok(Self { weights, total })
    }

    /// Sample a variant index, weighted by the table. Consumes a few
    /// bytes per call.
    pub fn pick(&self, u: &mut Unstructured<'_>) -> arbitrary::Result<usize> {
        let pick = u.int_in_range(0u32..=self.total.saturating_sub(1))?;
        let mut acc = 0u32;
        for (i, &w) in self.weights.iter().enumerate() {
            acc = acc.saturating_add(w);
            if pick < acc {
                return Ok(i);
            }
        }
        Ok(self.weights.len() - 1)
    }
}

/// Per-variant builder. The Arbitrary impl picks an index into
/// `FUZZOP_BUILDERS` and calls the corresponding closure to mint the
/// variant with random payload. Centralising this as a slice means
/// adding a new variant is exactly one edit: append a closure here.
/// `N_FUZZOP_VARIANTS` derives from `.len()`, so the index space and the
/// dispatch table cannot drift.
type FuzzOpBuilder = fn(&mut Unstructured<'_>) -> arbitrary::Result<FuzzOp>;

const FUZZOP_BUILDERS: &[FuzzOpBuilder] = &[
    |u| Ok(FuzzOp::KeyUnicode(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeyCtrl(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeyShift(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeySpecial(u.arbitrary()?)),
    |u| Ok(FuzzOp::KeySpecialShift(u.arbitrary()?)),
    |u| Ok(FuzzOp::Paste(u.arbitrary()?)),
    |u| Ok(FuzzOp::Mouse(u.arbitrary()?)),
    |u| Ok(FuzzOp::Tick(u.arbitrary()?)),
    |_| Ok(FuzzOp::LuaWakeup),
    |u| {
        Ok(FuzzOp::Resize {
            w: u.arbitrary()?,
            h: u.arbitrary()?,
        })
    },
    |u| Ok(FuzzOp::StartTurn(u.arbitrary()?)),
    |_| Ok(FuzzOp::EngineReady),
    |u| Ok(FuzzOp::EngineText(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineTextDelta(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineThinking(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineThinkingDelta(u.arbitrary()?)),
    |u| {
        Ok(FuzzOp::EngineToolStart {
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolOutput {
            call_id: u.arbitrary()?,
            chunk: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolFinish {
            call_id: u.arbitrary()?,
            is_error: u.arbitrary()?,
            content: u.arbitrary()?,
        })
    },
    |u| Ok(FuzzOp::ExecOutput(u.arbitrary()?)),
    |u| Ok(FuzzOp::ExecDone(u.arbitrary()?)),
    |u| Ok(FuzzOp::EngineTurnError(u.arbitrary()?)),
    |u| {
        Ok(FuzzOp::EngineSteered {
            text: u.arbitrary()?,
            count: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineRetrying {
            delay_ms: u.arbitrary()?,
            attempt: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineTokenUsage {
            prompt: u.arbitrary()?,
            completion: u.arbitrary()?,
            tps: u.arbitrary()?,
            cost_cents: u.arbitrary()?,
            background: u.arbitrary()?,
        })
    },
    |u| Ok(FuzzOp::PushQueuedMessage(u.arbitrary()?)),
    |u| {
        Ok(FuzzOp::EngineProcessCompleted {
            id: u.arbitrary()?,
            exit_code: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineMessages {
            msg_count: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ApproveFirstConfirm),
    |u| {
        Ok(FuzzOp::DenyFirstConfirm {
            message: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineCoreToolResult {
            req_id: u.arbitrary()?,
            content: u.arbitrary()?,
            is_error: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolHooksRequest {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::InsertAttachment {
            label: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::TogglePaneFocus),
    |u| {
        Ok(FuzzOp::EngineToolArgsDelta {
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            delta: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineAskResponse {
            id: u.arbitrary()?,
            content: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineAskError {
            id: u.arbitrary()?,
            kind_idx: u.arbitrary()?,
            message: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineTurnComplete {
            msg_count: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineToolDispatch {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineRequestPermission {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            tool_name: u.arbitrary()?,
            summary: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineShutdown {
            reason: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ReloadLua),
    |u| {
        Ok(FuzzOp::OpenOverlay {
            variant: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::InstallPlaceholder {
            text: u.arbitrary()?,
            variant: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::ClearPlaceholder),
    |u| {
        Ok(FuzzOp::EngineAskResponsePending {
            content: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::EngineAskErrorPending {
            kind_idx: u.arbitrary()?,
            message: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::StartExec {
            command: u.arbitrary()?,
        })
    },
    |_| Ok(FuzzOp::Cancel),
    |u| {
        Ok(FuzzOp::EngineRequestPermissionBash {
            req_id: u.arbitrary()?,
            call_id: u.arbitrary()?,
            command: u.arbitrary()?,
        })
    },
    |u| Ok(FuzzOp::Steer { text: u.arbitrary()? }),
    |u| Ok(FuzzOp::Unsteer { count: u.arbitrary()? }),
    |u| {
        Ok(FuzzOp::CallCoreTool {
            tool_name: u.arbitrary()?,
            args: u.arbitrary()?,
        })
    },
    |u| {
        Ok(FuzzOp::SetAgentMode {
            mode: u.arbitrary()?,
        })
    },
];

/// Total `FuzzOp` variant count, derived from the dispatch table so it
/// cannot drift.
pub const N_FUZZOP_VARIANTS: usize = FUZZOP_BUILDERS.len();

/// Build one `FuzzOp` by variant index. Indices outside the table never
/// happen because `Arbitrary` for `FuzzOp` always picks within range.
fn build_fuzz_op(idx: usize, u: &mut Unstructured<'_>) -> arbitrary::Result<FuzzOp> {
    FUZZOP_BUILDERS[idx](u)
}

/// Alias clarifying intent at use sites: on-disk JSON is a `Scenario`,
/// the in-memory fuzz input is a `FuzzInput`. Same bytes either way.
pub type Scenario = FuzzInput;

/// Should `run_scenario` render a frame after applying this op? `true`
/// covers anything that changes what would be on screen: input edits,
/// engine stream updates, resize, mouse drag, overlay/dialog motion,
/// reload (re-fires `on_ready`, may open splashes). `false` for ops
/// that only nudge clock or queue-meta state (`Tick`, `EngineReady`,
/// `EngineTokenUsage`) — rendering after those is pure cost. Lifting
/// every `content/*` parser out of 0% coverage is exactly what this
/// classification unlocks; the trade-off is one frame projection per
/// non-trivial op (≈50% of ops, on average).
fn render_trigger(op: &FuzzOp) -> bool {
    !matches!(
        op,
        FuzzOp::Tick(_) | FuzzOp::EngineReady | FuzzOp::EngineTokenUsage { .. }
    )
}

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

/// Curated `(accept, dismiss)` chord pairs for `InstallPlaceholder`.
/// Each variant exercises a different placeholder dispatch path: bare
/// keys, modifier chords, multi-chord accept sets, and the empty-set
/// case where the placeholder is purely cosmetic.
fn placeholder_chord_pair(
    variant: u8,
) -> (
    Vec<tui::smelt_term::KeyBind>,
    Vec<tui::smelt_term::KeyBind>,
) {
    use tui::smelt_term::KeyBind;
    let kb = |code, mods| KeyBind { code, mods };
    let tab = kb(KeyCode::Tab, KeyModifiers::NONE);
    let enter = kb(KeyCode::Enter, KeyModifiers::NONE);
    let esc = kb(KeyCode::Esc, KeyModifiers::NONE);
    let right = kb(KeyCode::Right, KeyModifiers::NONE);
    let ctrl_n = kb(KeyCode::Char('n'), KeyModifiers::CONTROL);
    let ctrl_g = kb(KeyCode::Char('g'), KeyModifiers::CONTROL);
    let ctrl_y = kb(KeyCode::Char('y'), KeyModifiers::CONTROL);
    match variant % 6 {
        0 => (vec![tab], vec![esc]),
        1 => (vec![enter], vec![esc]),
        2 => (vec![ctrl_n], vec![ctrl_g]),
        3 => (vec![tab, right, ctrl_y], vec![esc, ctrl_g]),
        4 => (vec![], vec![esc]),
        _ => (vec![], vec![]),
    }
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

/// Synthesize a deterministic, lightweight history vector for compaction
/// payloads. Rotates through user, terminal assistant, and tool-call
/// assistant turns so `EngineMessages` / `EngineTurnComplete` exercise
/// every `HistoryItem` discriminant — the sum-typed history (commit
/// `b0c54474`) added a third arm and the rebuild-screen path through
/// `restore_screen` must survive each variant.
fn synth_history(count: usize) -> Vec<protocol::HistoryItem> {
    (0..count)
        .map(|i| {
            let body = format!("compacted-{i}");
            let reasoning_blocks = if i % 4 == 0 {
                Vec::new()
            } else {
                vec![ReasoningBlock {
                    provider: "fuzz".to_string(),
                    data: serde_json::Value::Null,
                }]
            };
            match i % 3 {
                0 => protocol::HistoryItem::user(Content::text(body)),
                1 => protocol::HistoryItem::Assistant(protocol::AssistantTurn::terminal(
                    Some(Content::text(body)),
                    None,
                    reasoning_blocks,
                )),
                _ => {
                    let invocation = protocol::ToolInvocation {
                        call_id: format!("synth-call-{i:02}"),
                        name: "synth".to_string(),
                        arguments: "{}".to_string(),
                        result: ToolOutcome {
                            content: format!("synth-result-{i}"),
                            is_error: false,
                            metadata: None,
                        },
                        elapsed_ms: None,
                    };
                    protocol::HistoryItem::Assistant(protocol::AssistantTurn::with_invocations(
                        Some(Content::text(body)),
                        None,
                        reasoning_blocks,
                        vec![invocation],
                    ))
                }
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
    queued_messages: usize,
    working: tui::app::test_harness::WorkingSnapshot,
    session_cost_usd: f64,
    context_tokens: Option<u32>,
    transcript_blocks: usize,
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
            queued_messages: app.queued_message_count(),
            working: app.working_state(),
            session_cost_usd: app.session_cost_usd(),
            context_tokens: app.context_tokens(),
            transcript_blocks: app.transcript_block_count(),
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
        .filter(|a| matches!(a, Action::EngineSend(cmd) if pred(cmd.as_ref())))
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
    ToolStarted {
        call_id: String,
    },
    /// `ToolOutput` for an already-pending `call_id` is a pure append to
    /// that tool's output; the pending entry stays put.
    ToolOutput {
        call_id: String,
    },
    /// `ToolFinished` clears `call_id` from pending — but only verifiable
    /// when it was actually present beforehand.
    ToolFinished {
        call_id: String,
    },
    /// `ExecDone` runs `finalize_exec`, which clears `stream_exec_id`.
    ExecCleared,
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
    Steered {
        count: usize,
    },
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
    /// `RequestPermission` against an active turn lands on exactly one of
    /// three branches: auto-approve (one new `PermissionDecision`,
    /// no new confirm), defer (one new `pending_dialogs` entry, no new
    /// confirm or decision), or register (one new entry in `core.confirms`,
    /// no decision yet).
    PermissionRequested,
    /// Approving / denying a confirm consumes one pending entry and queues
    /// a `PermissionDecision`. Approve never ends the turn; deny without a
    /// message ends it.
    ConfirmResolved {
        approved: bool,
        had_message: bool,
    },
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
    /// `Cancel` side channel ends any active turn and flushes streaming.
    TurnCancelled,
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
                assert!(!post.agent_running, "TurnError did not end the active turn",);
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
        PostCheck::Retrying => {
            if pre.agent_running {
                assert!(
                    post.working.animating,
                    "Retrying on active turn left working idle: {:?}",
                    post.working
                );
                assert!(
                    !post.streaming.text && !post.streaming.thinking,
                    "Retrying left streaming active: {:?}",
                    post.streaming
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
                        if approved {
                            "Approve"
                        } else {
                            "Deny with message"
                        },
                    );
                }
            }
        }
        PostCheck::ToolDispatched => {
            if pre.agent_running {
                let new_results =
                    count_action(new_actions, |c| matches!(c, UiCommand::ToolResult { .. }));
                assert!(
                    new_results <= 1,
                    "ToolDispatch should queue at most one ToolResult in-step (got {new_results})",
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
                assert!(!post.agent_running, "Shutdown did not end the active turn",);
            }
        }
        PostCheck::TurnCancelled => {
            if pre.agent_running {
                assert!(!post.agent_running, "Cancel did not end the active turn",);
            }
            assert!(
                !post.streaming.text && !post.streaming.thinking,
                "Cancel left streaming active: {:?}",
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
        FuzzOp::KeySpecialShift(which) => {
            let code = SPECIALS[(which as usize) % SPECIALS.len()];
            (
                Some(SourceEvent::Term(key_event(code, KeyModifiers::SHIFT))),
                PostCheck::None,
            )
        }
        FuzzOp::Paste(s) => (
            Some(SourceEvent::Term(TermEvent::Paste(s))),
            PostCheck::None,
        ),
        FuzzOp::Mouse(m) => {
            let ev = MouseEvent {
                kind: decode_mouse_kind(m.kind, m.button),
                column: u16::from(m.col),
                row: u16::from(m.row),
                modifiers: decode_mouse_mods(m.mods),
            };
            (
                Some(SourceEvent::Term(TermEvent::Mouse(ev))),
                PostCheck::None,
            )
        }
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
        FuzzOp::EngineReady => (
            Some(SourceEvent::Engine(EngineEvent::Ready)),
            PostCheck::None,
        ),
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
        FuzzOp::EngineToolStart {
            call_id,
            tool_name,
            args,
        } => {
            let cid = call_id_string(call_id);
            let ev = SourceEvent::Engine(EngineEvent::ToolStarted {
                call_id: cid.clone(),
                tool_name,
                args: args.into_map(),
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
                context_tokens: None,
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
        FuzzOp::EngineProcessCompleted { id, exit_code } => {
            let ev = SourceEvent::Engine(EngineEvent::ProcessCompleted { id, exit_code });
            (Some(ev), PostCheck::ProcessCompleted)
        }
        FuzzOp::EngineMessages { .. } => {
            // Needs the live turn_id; handled inline in `apply` before
            // reaching `plan`.
            unreachable!("EngineMessages handled inline in apply()")
        }
        FuzzOp::EngineRequestPermission {
            req_id,
            call_id,
            tool_name,
            summary,
            args,
        } => {
            let ev = SourceEvent::Engine(EngineEvent::RequestPermission {
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: args.into_map(),
                approval_patterns: Vec::new(),
                summary: protocol::style::StyledLines::from_plain(summary),
            });
            (Some(ev), PostCheck::PermissionRequested)
        }
        FuzzOp::EngineRequestPermissionBash {
            req_id,
            call_id,
            command,
        } => {
            let mut args = std::collections::HashMap::new();
            args.insert("command".to_string(), serde_json::Value::String(command));
            let ev = SourceEvent::Engine(EngineEvent::RequestPermission {
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name: "bash".to_string(),
                args,
                approval_patterns: Vec::new(),
                summary: protocol::style::StyledLines::from_plain("bash".to_string()),
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
            args,
        } => {
            let ev = SourceEvent::Engine(EngineEvent::ToolDispatch {
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: args.into_map(),
            });
            (Some(ev), PostCheck::ToolDispatched)
        }
        FuzzOp::EngineToolHooksRequest {
            req_id,
            call_id,
            tool_name,
            args,
        } => {
            let ev = SourceEvent::Engine(EngineEvent::ToolHooksRequest {
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: args.into_map(),
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
        FuzzOp::Cancel
        | FuzzOp::Steer { .. }
        | FuzzOp::Unsteer { .. }
        | FuzzOp::CallCoreTool { .. }
        | FuzzOp::SetAgentMode { .. }
        | FuzzOp::InsertAttachment { .. }
        | FuzzOp::TogglePaneFocus
        | FuzzOp::ReloadLua
        | FuzzOp::OpenOverlay { .. }
        | FuzzOp::InstallPlaceholder { .. }
        | FuzzOp::ClearPlaceholder
        | FuzzOp::EngineAskResponsePending { .. }
        | FuzzOp::EngineAskErrorPending { .. }
        | FuzzOp::StartExec { .. } => {
            unreachable!("side channels handled inline in apply()")
        }
        FuzzOp::EngineToolArgsDelta {
            call_id,
            tool_name,
            delta,
        } => {
            let ev = SourceEvent::Engine(EngineEvent::ToolArgsDelta {
                call_id,
                tool_name,
                delta,
            });
            (Some(ev), PostCheck::None)
        }
        FuzzOp::EngineAskResponse { id, content } => {
            let ev = SourceEvent::Engine(EngineEvent::EngineAskResponse {
                id,
                message: Some(Message::assistant(Some(Content::text(content)), None, None)),
                error: None,
            });
            (Some(ev), PostCheck::None)
        }
        FuzzOp::EngineAskError {
            id,
            kind_idx,
            message,
        } => {
            let kind = ENGINE_ASK_ERROR_KINDS[(kind_idx as usize) % ENGINE_ASK_ERROR_KINDS.len()];
            let ev = SourceEvent::Engine(EngineEvent::EngineAskResponse {
                id,
                message: None,
                error: Some(EngineAskError { kind, message }),
            });
            (Some(ev), PostCheck::None)
        }
    }
}

/// Side channels are `FuzzOp` variants that bypass `feed_one_within_budget`
/// and poke the app directly (host-level pokes the engine never sees). The
/// dispatcher returns `Ok(())` if the op was a side channel and was handled,
/// or `Err(op)` if the op should fall through to the event-feeding path.
/// Owned-data variants take ownership cleanly via this single match.
fn try_dispatch_side_channel(app: &mut TestApp, op: FuzzOp) -> Result<(), FuzzOp> {
    match op {
        FuzzOp::PushQueuedMessage(text) => app.push_queued_message(text),
        FuzzOp::InsertAttachment { label } => app.insert_attachment(label),
        FuzzOp::InstallPlaceholder { text, variant } => {
            let (accept, dismiss) = placeholder_chord_pair(variant);
            app.install_prompt_placeholder(text, accept, dismiss);
        }
        FuzzOp::ClearPlaceholder => app.clear_prompt_placeholder(),
        FuzzOp::StartTurn(id) => app.start_turn(u64::from(id)),
        FuzzOp::TogglePaneFocus => app.toggle_pane_focus(),
        FuzzOp::OpenOverlay { variant } => app.open_synthetic_overlay(variant),
        FuzzOp::ReloadLua => {
            // Targeted reload-survival check: every named slot in the
            // four reload-survival registries (bufs, wins, overlays,
            // paints) that existed before reload must still exist
            // after. New names may be added (the first reload runs
            // `on_ready` which lets bundled plugins register named
            // resources for the first time); anonymous slots get
            // reaped (they don't contribute to these counters).
            // Strictly: post >= pre component-wise. This is the
            // `ce76000e`-class regression detector — cheap and specific.
            let pre = app.named_resource_counts();
            app.reload_lua();
            let post = app.named_resource_counts();
            assert!(
                post.0 >= pre.0 && post.1 >= pre.1 && post.2 >= pre.2 && post.3 >= pre.3,
                "reload dropped a named resource: pre=(bufs,wins,overlays,paints)={:?} post={:?}",
                pre,
                post,
            );
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
        }
        FuzzOp::DenyFirstConfirm { message } => {
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
        }
        FuzzOp::EngineAskResponsePending { content } => {
            if let Some(id) = app.pending_ask_id() {
                let ev = SourceEvent::Engine(EngineEvent::EngineAskResponse {
                    id,
                    message: Some(Message::assistant(Some(Content::text(content)), None, None)),
                    error: None,
                });
                app.feed_one(ev);
            }
        }
        FuzzOp::EngineAskErrorPending { kind_idx, message } => {
            if let Some(id) = app.pending_ask_id() {
                let kind = ENGINE_ASK_ERROR_KINDS
                    [(kind_idx as usize) % ENGINE_ASK_ERROR_KINDS.len()];
                let ev = SourceEvent::Engine(EngineEvent::EngineAskResponse {
                    id,
                    message: None,
                    error: Some(EngineAskError { kind, message }),
                });
                app.feed_one(ev);
            }
        }
        FuzzOp::StartExec { command } => {
            app.start_exec(&command);
        }
        FuzzOp::Cancel => {
            let pre = Snapshot::capture(app);
            app.cancel();
            let post = Snapshot::capture(app);
            let new_actions = app.actions_since(pre.action_count);
            run_check(PostCheck::TurnCancelled, &pre, &post, new_actions);
            check_turn_end_invariants(&pre, &post);
        }
        FuzzOp::Steer { text } => {
            app.steer(&text);
        }
        FuzzOp::Unsteer { count } => {
            app.unsteer(usize::from(count));
        }
        FuzzOp::CallCoreTool { tool_name, args } => {
            app.call_core_tool(&tool_name, args.into_map());
        }
        FuzzOp::SetAgentMode { mode } => {
            app.set_agent_mode(mode.into());
        }
        other => return Err(other),
    }
    Ok(())
}

/// Apply one `FuzzOp` to a `TestApp`. Every op rolls through the same path:
/// pre-snapshot → feed event (or side-channel) → post-snapshot → check →
/// global invariants.
pub fn apply(app: &mut TestApp, op: FuzzOp) {
    let op = match try_dispatch_side_channel(app, op) {
        Ok(()) => {
            app.assert_invariants();
            return;
        }
        Err(op) => op,
    };

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
                history: synth_history(count),
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
            let ev = SourceEvent::Engine(EngineEvent::HistoryUpdated {
                turn_id: id,
                history: synth_history(count),
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
        let render_after = render_trigger(&op);
        apply(&mut app, op);
        if app.quit_requested() {
            break;
        }
        if render_after {
            app.render_silent();
        }
    }
    // Always render once at the end so the final state passes through the
    // projection — covers scenarios that end on a `Tick` and would
    // otherwise skip the renderer entirely.
    app.render_silent();

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
