//! Scenario operations: the `FuzzOp` vocabulary and its input decoders.

use crate::*;

/// Bounded JSON value tree for tool argument fuzzing. Production paths
/// (`evaluate_tool_metadata`, `permissions.evaluate_tool`, pattern matching in
/// `RequestPermission`) consume `HashMap<String, serde_json::Value>` -
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
            let key: String = short_string(u, 16)?;
            let val = arb_json(u, 3)?;
            map.insert(key, val);
        }
        Ok(ArgsBag(map))
    }
}

pub(crate) fn arb_json(
    u: &mut Unstructured<'_>,
    depth: u8,
) -> arbitrary::Result<serde_json::Value> {
    if depth == 0 || u.is_empty() {
        return arb_json_leaf(u);
    }
    match u.int_in_range(0u8..=5)? {
        0 => arb_json_leaf(u),
        1 => Ok(serde_json::Value::String(short_string(u, 16)?)),
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
                m.insert(short_string(u, 16)?, arb_json(u, depth - 1)?);
            }
            Ok(serde_json::Value::Object(m))
        }
        _ => arb_json_leaf(u),
    }
}

pub(crate) fn arb_json_leaf(u: &mut Unstructured<'_>) -> arbitrary::Result<serde_json::Value> {
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
        _ => Ok(serde_json::Value::String(short_string(u, 16)?)),
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

#[derive(Arbitrary, Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FuzzJobTermination {
    Exited,
    Signaled,
    #[serde(rename = "oom")]
    OutOfMemory,
    Stopped,
}

impl From<FuzzJobTermination> for protocol::JobTermination {
    fn from(value: FuzzJobTermination) -> Self {
        match value {
            FuzzJobTermination::Exited => Self::Exited,
            FuzzJobTermination::Signaled => Self::Signaled,
            FuzzJobTermination::OutOfMemory => Self::OutOfMemory,
            FuzzJobTermination::Stopped => Self::Stopped,
        }
    }
}

pub(crate) const MOUSE_BUTTONS: &[MouseButton] =
    &[MouseButton::Left, MouseButton::Right, MouseButton::Middle];

pub(crate) const ENGINE_ASK_ERROR_KINDS: &[EngineAskErrorKind] = &[
    EngineAskErrorKind::Network,
    EngineAskErrorKind::RateLimited,
    EngineAskErrorKind::Quota,
    EngineAskErrorKind::InvalidResponse,
    EngineAskErrorKind::ContextWindow,
    EngineAskErrorKind::CyberPolicy,
    EngineAskErrorKind::Cancelled,
    EngineAskErrorKind::Other,
];

pub(crate) fn decode_mouse_kind(kind: u8, button: u8) -> MouseEventKind {
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

pub(crate) fn decode_mouse_mods(mods: u8) -> KeyModifiers {
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
    /// Shift-modified special key - drives shift+arrow / shift+Home/End
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
    /// dispatch also accepts this and merges history when non-empty.
    /// If a compaction checkpoint is installed, the checkpoint prefix is
    /// preserved ahead of the incoming messages.
    EngineTurnComplete {
        msg_count: u8,
    },
    /// Emit `TurnError`. Active-turn dispatch ends the turn; idle dispatch
    /// surfaces a notification.
    EngineTurnError(String),
    /// Emit a canonical steering delta with `count` user items. Active-turn
    /// dispatch commits their transcript rows before acknowledging queued inputs.
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
    /// Side channel: push a synthetic request-queued entry so `Steered`
    /// has something to drain.
    PushQueuedMessage(String),
    /// Deliver supervisor completion through the production job-completion handler.
    JobCompleted {
        id: String,
        exit_code: Option<i32>,
        termination: FuzzJobTermination,
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
    /// Emit `ToolEvaluationRequest`. Active-turn dispatch always queues a
    /// `ToolEvaluationResponse` UiCommand back; `evaluate_tool_metadata` short-circuits
    /// when no Lua hook is registered.
    EngineToolEvaluationRequest {
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
    /// Emit `Shutdown`. Active-turn dispatch ends the turn without starting
    /// queued follow-up input; idle dispatch is a no-op.
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
    /// Emit `ToolCallDraftDelta`. The TUI accumulates `delta` into a per-stream
    /// JSON-fragment buffer, then displays draft args until the matching
    /// `ToolStarted` promotes the block. Exercises the streaming draft path
    /// against arbitrary out-of-order or orphan deltas.
    EngineToolDraftDelta {
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
    /// re-entry (`ctx.kind = "reload"`) against arbitrary pre-state -
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
    /// id, if any. Exercises auxiliary callback dispatch and plugin paths that
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
    /// Side channel: install a prompt `text_changed` callback that attempts
    /// to repark the prompt cursor while typing is in progress.
    InstallPromptCursorTrap {
        variant: u8,
    },
    /// Side channel: register a Lua tool and begin a synthetic custom-command
    /// turn, asserting the outbound `StartTurn` carries Lua tool definitions.
    StartCustomCommand {
        variant: u8,
    },
    /// Side channel: issue a Lua `smelt.engine.ask` request so pending ask
    /// response/error ops exercise callback completion.
    StartEngineAsk {
        question: String,
    },
    /// Side channel: request host-owned reload scheduling. The harness then
    /// offers the same safe-point drain as the main loop, so active-turn
    /// schedules stay pending until a later turn-ending op while idle ones
    /// execute and clear the pending flag immediately.
    ScheduleReload,
    /// Side channel: deterministic regression probe for #15. Exercises typing
    /// and cursor motion after turn completion/error/cancel/reload/traps,
    /// app-level Escape sequences, and stale prompt side-request callbacks.
    ProbePromptCursorAfterTurn {
        variant: u8,
    },
    /// Side channel: drive the real compaction prepare-request + EngineAsk
    /// response path, including active-turn phase preservation.
    ProbeCompactionPrepareRequest {
        variant: u8,
    },
    /// Macro workload: paste a bounded text chunk repeatedly, move the cursor,
    /// and optionally submit. Random single-key streams rarely build large
    /// prompt buffers or hit multi-step edit/submit interactions.
    MacroPromptEdit {
        text: String,
        repetitions: u8,
        movement: u8,
        submit: bool,
    },
    /// Macro workload: create/target an active turn and run a matched
    /// ToolStarted -> ToolOutput -> ToolFinished sequence using one call id.
    /// This reaches paired lifecycle paths far more reliably than unrelated
    /// random engine events.
    MacroToolRoundTrip {
        call_id: u8,
        tool_name: String,
        args: ArgsBag,
        output: String,
        is_error: bool,
    },
    /// Macro workload: stream several text/thinking chunks, then finish or
    /// error the turn. Exercises transcript parser/render with realistic
    /// clustered provider output instead of isolated deltas.
    MacroStreamTurn {
        turn_id: u8,
        text: String,
        thinking: String,
        chunks: u8,
        error: bool,
    },
}

impl FuzzOp {
    /// Short human label used by `play_scenario` for its status line. Keeping
    /// labels next to the operation enum makes new variants a single edit.
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
            JobCompleted { id, .. } => format!("job completed {id}"),
            EngineMessages { msg_count } => format!("messages ({msg_count})"),
            EngineRequestPermission { tool_name, .. } => {
                format!("request permission {tool_name}")
            }
            ApproveFirstConfirm => "approve confirm".into(),
            DenyFirstConfirm { .. } => "deny confirm".into(),
            EngineToolDispatch { tool_name, .. } => format!("tool dispatch {tool_name}"),
            EngineToolEvaluationRequest { tool_name, .. } => format!("tool hooks {tool_name}"),
            EngineCoreToolResult { .. } => "core tool result".into(),
            EngineShutdown { .. } => "shutdown".into(),
            InsertAttachment { label } => format!("insert attachment {label}"),
            TogglePaneFocus => "toggle pane focus".into(),
            EngineToolDraftDelta { tool_name, .. } => format!("tool draft delta {tool_name}"),
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
            InstallPromptCursorTrap { variant } => format!("prompt cursor trap v={variant}"),
            StartCustomCommand { variant } => format!("custom command v={variant}"),
            StartEngineAsk { .. } => "engine ask".into(),
            ScheduleReload => "schedule reload".into(),
            ProbePromptCursorAfterTurn { variant } => format!("prompt cursor probe v={variant}"),
            ProbeCompactionPrepareRequest { variant } => format!("compaction probe v={variant}"),
            MacroPromptEdit {
                repetitions,
                submit,
                ..
            } => format!("macro prompt reps={repetitions} submit={submit}"),
            MacroToolRoundTrip { tool_name, .. } => format!("macro tool round-trip {tool_name}"),
            MacroStreamTurn { chunks, error, .. } => {
                format!("macro stream chunks={chunks} error={error}")
            }
        }
    }
}
