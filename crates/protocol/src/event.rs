//! Wire protocol between the engine and the UI.

use crate::content::Content;
use crate::message::{Message, ToolOutcome};
use crate::mode::{AgentMode, ReasoningEffort};
use crate::style::StyledLines;
use crate::usage::{ModelConfigOverrides, PermissionOverrides, TokenUsage, TurnMeta};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Structured JSON output specification mirroring `engine::ResponseFormat`.
/// Lives on the wire so the TUI can forward Lua-provided schemas to the
/// engine without engine depending on the Lua surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskResponseFormat {
    pub name: String,
    pub schema: serde_json::Value,
}

/// Provider connection + model identifier needed for a one-shot
/// `EngineAsk` call. Lives on the wire so the TUI can resolve a
/// Lua-provided model reference before dispatching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskModel {
    pub model: String,
    pub api_base: String,
    pub api_key: String,
    pub provider_type: String,
}

/// Classification of a `smelt.engine.ask` failure. Surfaced to Lua as
/// the `kind` field on the error table so plugins can branch on the
/// failure mode without parsing message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineAskErrorKind {
    Network,
    RateLimited,
    Quota,
    InvalidResponse,
    ContextWindow,
    Cancelled,
    Other,
}

impl EngineAskErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EngineAskErrorKind::Network => "network",
            EngineAskErrorKind::RateLimited => "rate_limited",
            EngineAskErrorKind::Quota => "quota",
            EngineAskErrorKind::InvalidResponse => "invalid_response",
            EngineAskErrorKind::ContextWindow => "context_window",
            EngineAskErrorKind::Cancelled => "cancelled",
            EngineAskErrorKind::Other => "other",
        }
    }
}

/// Typed error payload returned alongside `EngineAskResponse` when the
/// underlying provider call failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineAskError {
    pub kind: EngineAskErrorKind,
    /// Human-readable single-line description (newlines collapsed to spaces).
    pub message: String,
}

/// How a registered tool interacts with concurrent tool execution.
///
/// `Concurrent` (default): runs alongside other tools via the engine's
/// `pending_tools` channel - good for pure data fetches with no UI.
///
/// `Sequential`: deferred until after every concurrent tool has
/// finished, then dispatched one at a time. Used by tools that open a
/// dialog and await a user reply - the user should see all other tool
/// output before the prompt. `ask_user_question` is the canonical
/// example.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    #[default]
    Concurrent,
    Sequential,
}

/// A tool defined in Lua. Sent from TUI to engine so the engine
/// can include it in LLM tool definitions and proxy execution back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    /// When set, the tool is only available in these modes.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub modes: Option<Vec<AgentMode>>,
    #[serde(default)]
    pub execution_mode: ToolExecutionMode,
    /// When `true`, this tool replaces the core Rust tool of the
    /// same name. The engine drops the core definition from the LLM
    /// schema and dispatches calls to Lua instead. When `false`
    /// (default), registering a name that collides with a core tool
    /// is an error reported back to the user.
    #[serde(default)]
    pub override_core: bool,
    /// Hook signals declared by the tool. These are metadata hooks: they
    /// add approval suggestions and preflight validation to the mandatory
    /// permission pipeline. They do not decide whether a tool call is
    /// permission-checked; every model-initiated tool call must pass
    /// through the gate before dispatch.
    #[serde(default)]
    pub hooks: ToolHookFlags,
    /// If false, the tool should be hidden from the model in headless mode.
    /// Defaults to true so existing tools remain visible.
    #[serde(default = "default_true")]
    pub headless: bool,
}

fn default_true() -> bool {
    true
}

/// Which metadata hooks a tool has registered. Sent with `ToolDef` so
/// the engine/host can avoid evaluating optional Lua callbacks that do
/// not exist.
///
/// `summary` is always evaluated regardless of this flag set - it's a
/// display concern, not a permission hook. These flags are not a security
/// boundary and must not be used as the permission gate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolHookFlags {
    #[serde(default)]
    pub approval_patterns: bool,
    #[serde(default)]
    pub preflight: bool,
}

impl ToolHookFlags {
    /// True when at least one optional metadata hook is registered.
    /// This is only an optimization hint; it must not control whether
    /// permission evaluation runs.
    pub fn any(&self) -> bool {
        self.approval_patterns || self.preflight
    }
}

/// Permission decision for a single tool call, produced by central policy
/// evaluation after tool metadata has been collected.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    #[default]
    Allow,
    Ask,
    Deny,
    #[serde(rename = "error")]
    Error(String),
}

/// Tool metadata evaluated for a specific invocation. These values
/// describe the call for display and auto-approval matching; they do not
/// grant permission to execute the tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolMetadata {
    /// Approval patterns to offer "always allow" for.
    /// Used when the central permission decision is `Ask`.
    #[serde(default)]
    pub approval_patterns: Vec<String>,
    /// Preflight validation error reported by the tool before execution.
    /// The central evaluator converts this into `Decision::Error`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub preflight_error: Option<String>,
    /// Styled one-line-or-more summary of this invocation. Comes from
    /// the tool's `summary(args)` Lua callback. Sole source of truth
    /// for the transcript header AND confirm dialog body header - the
    /// engine never extracts arg fields by tool name.
    #[serde(default)]
    pub summary: StyledLines,
}

/// Complete tool evaluation result: tool-provided metadata plus the central
/// permission decision derived from that metadata, mode, origin, and policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolEvaluation {
    #[serde(default)]
    pub decision: Decision,
    #[serde(default)]
    pub metadata: ToolMetadata,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningKind {
    Summary,
    #[default]
    Raw,
}

/// Events emitted by the engine. The UI consumes these to update its display.
///
/// Most variants are fire-and-forget. The exception is `RequestPermission`,
/// which carries a `request_id` that the UI must eventually reply to via
/// `UiCommand::PermissionDecision`.
///
/// Event ordering within a turn:
///   Ready → (ReasoningPart* → Text* → ToolCallDraft* → tool-lifecycle)*
///         → TurnComplete | TurnError
///
/// tool-lifecycle is one of:
///   ToolRejected
///   RequestPermission → approved: ToolStarted → ToolOutput* → ToolFinished
///   RequestPermission → denied: no further engine event
///   ToolStarted → ToolOutput* → ToolFinished
///
/// ProcessCompleted can arrive at any time (including between turns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EngineEvent {
    /// Engine has initialized and is ready to accept commands.
    Ready,

    /// A complete non-streamed reasoning part.
    Reasoning {
        kind: ReasoningKind,
        title: Option<String>,
        content: String,
    },

    /// A provider started a structured reasoning part.
    ReasoningPartStarted { id: String, kind: ReasoningKind },

    /// Incremental reasoning content. Summary deltas may also expose a parsed
    /// thinking title while their transcript body remains buffered.
    ReasoningPartDelta {
        id: String,
        kind: ReasoningKind,
        delta: String,
        title: Option<String>,
    },

    /// A structured reasoning part completed. Raw parts were already streamed;
    /// summary parts are committed from this normalized title and body.
    ReasoningPartFinished {
        id: String,
        kind: ReasoningKind,
        title: Option<String>,
        content: String,
    },

    /// Streamed assistant text (may arrive in chunks).
    Text { content: String },

    /// Incremental text token from the LLM (streaming delta).
    TextDelta { delta: String },

    /// Provider has started streaming a speculative tool call. This is display-only;
    /// execution still waits for the final tool call in the completed LLM response.
    ToolCallDraftStarted {
        stream_id: String,
        call_id: Option<String>,
        tool_name: Option<String>,
    },

    /// Incremental argument fragment for a speculative tool call.
    ToolCallDraftDelta {
        stream_id: String,
        call_id: Option<String>,
        tool_name: Option<String>,
        delta: String,
    },

    /// Provider has finished streaming the speculative tool call arguments.
    ToolCallDraftFinished {
        stream_id: String,
        call_id: String,
        tool_name: String,
        arguments: String,
    },

    /// A queued user message was consumed by the engine.
    Steered { text: String, count: usize },

    /// A tool call has started.
    ToolStarted {
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
    },

    /// Incremental output from a running tool (stdout/stderr lines).
    ToolOutput { call_id: String, chunk: String },

    /// A tool call has finished.
    ToolFinished {
        call_id: String,
        result: ToolOutcome,
        elapsed_ms: Option<u64>,
    },

    /// A tool call failed or was blocked before execution. Carries the full
    /// call shape so the UI can render one terminal block without first showing
    /// a pending or preview state.
    ToolRejected {
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
        summary: StyledLines,
        result: ToolOutcome,
        elapsed_ms: Option<u64>,
    },

    /// Engine needs user permission before executing a tool.
    RequestPermission {
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
        approval_patterns: Vec<String>,
        /// Styled summary of the pending call - both the dialog body
        /// header and any auto-approval pattern matching read this.
        summary: StyledLines,
    },

    /// Token usage update after an LLM call.
    TokenUsage {
        usage: TokenUsage,
        tokens_per_sec: Option<f64>,
        cost_usd: Option<f64>,
        /// True for background requests (title, compaction, btw, predict)
        /// whose token counts should not update displayed context usage.
        #[serde(default)]
        background: bool,
    },

    /// LLM call failed, engine is retrying.
    Retrying { delay_ms: u64, attempt: u32 },

    /// Request audit persistence failed. The turn can continue, but request
    /// inspection for this attempt may be incomplete.
    RequestAuditError { message: String },

    /// A background process has finished.
    ProcessCompleted { id: String, exit_code: Option<i32> },

    /// Incremental text token from a background `UiCommand::EngineAsk` request.
    /// The final `EngineAskResponse` still carries the full assistant message.
    EngineAskDelta { id: u64, delta: String },

    /// Response to a `UiCommand::EngineAsk` request. On success
    /// `error` is `None` and `message` is the assistant reply in the
    /// normal `protocol::Message` shape. On failure `error` carries a
    /// typed classification and `message` is absent.
    EngineAskResponse {
        id: u64,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        message: Option<Message>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        error: Option<EngineAskError>,
    },

    /// Append-only committed history delta. This is the bounded hot path for
    /// normal user, assistant, and note appends; replacements and rewinds still
    /// use `HistoryUpdated` snapshots until they get typed deltas of their own.
    HistoryAppended {
        turn_id: u64,
        /// Public history index where `items` begin in the engine's model-visible history.
        first_index: usize,
        items: Vec<crate::history::HistoryItem>,
    },

    /// Atomic snapshot of the engine's committed history. Fires after each
    /// non-append step that mutates `Vec<HistoryItem>`. By construction every
    /// `HistoryItem::Assistant` carries paired `ToolInvocation`s for the
    /// tool_calls it emitted - there is no in-flight state to worry about.
    HistoryUpdated {
        turn_id: u64,
        /// First public history index that may differ from the previous UI snapshot.
        /// Consumers can treat the prefix below this index as unchanged without
        /// comparing every row in large sessions.
        first_changed_index: usize,
        history: Vec<crate::history::HistoryItem>,
    },

    /// The agent turn completed (successfully or after cancellation).
    TurnComplete {
        turn_id: u64,
        /// First public history index that may differ from the previous UI snapshot.
        first_changed_index: usize,
        /// Final public history snapshot when completion must repair or persist
        /// state that may have missed prior deltas. Normal append-only turns use
        /// `HistoryAppended` and leave this absent.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        history: Option<Vec<crate::history::HistoryItem>>,
        meta: Option<TurnMeta>,
    },

    /// The agent turn ended with an error.
    TurnError {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        kind: Option<EngineAskErrorKind>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        retry_at_ms: Option<u64>,
    },

    /// Engine is shutting down.
    Shutdown { reason: Option<String> },

    /// Engine needs the TUI to execute a Lua-defined tool.
    ToolDispatch {
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
    },

    /// Engine asks the TUI to evaluate a Lua tool's metadata
    /// callbacks (`summary`, `approval_patterns`, `preflight`) for a
    /// specific invocation. The TUI replies with
    /// `UiCommand::ToolEvaluationResponse`, after which the engine
    /// resumes the standard Allow / Deny / Ask flow.
    ToolEvaluationRequest {
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
        mode: AgentMode,
    },

    /// Result of a core-tool side call requested by Lua via
    /// `smelt.tools.call`. Streamed back so the suspended Lua coroutine
    /// can resume with the tool's output.
    CoreToolResult {
        request_id: u64,
        content: String,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        metadata: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StartTurnInput {
    User {
        content: Content,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        display: Option<String>,
    },
    Note {
        note: crate::history::HistoryNote,
    },
}

impl StartTurnInput {
    pub fn user(content: Content, display: Option<String>) -> Self {
        Self::User { content, display }
    }

    pub fn note(note: crate::history::HistoryNote) -> Self {
        Self::Note { note }
    }

    pub fn provider_content(&self) -> Content {
        match self {
            Self::User { content, .. } => content.clone(),
            Self::Note { note } => Content::text(note.to_model_text()),
        }
    }

    pub fn display(&self) -> Option<String> {
        match self {
            Self::User { display, .. } => display.clone(),
            Self::Note { .. } => None,
        }
    }

    pub fn note_ref(&self) -> Option<&crate::history::HistoryNote> {
        match self {
            Self::User { .. } => None,
            Self::Note { note } => Some(note),
        }
    }
}

/// Provider-visible history for a turn. Interactive frontends use the store
/// variant so request start does not clone full session history through the UI
/// channel; tests and headless callers can still provide already materialized
/// items explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModelHistorySource {
    Items(Vec<crate::history::HistoryItem>),
    Store {
        prefix: Vec<crate::history::HistoryItem>,
        first_live_index: usize,
        end_index: usize,
        suffix: Vec<crate::history::HistoryItem>,
    },
}

impl ModelHistorySource {
    pub fn items(items: Vec<crate::history::HistoryItem>) -> Self {
        Self::Items(items)
    }

    pub fn store(
        prefix: Vec<crate::history::HistoryItem>,
        first_live_index: usize,
        end_index: usize,
    ) -> Self {
        Self::Store {
            prefix,
            first_live_index,
            end_index,
            suffix: Vec::new(),
        }
    }

    pub fn store_with_suffix(
        prefix: Vec<crate::history::HistoryItem>,
        first_live_index: usize,
        end_index: usize,
        suffix: Vec<crate::history::HistoryItem>,
    ) -> Self {
        Self::Store {
            prefix,
            first_live_index,
            end_index,
            suffix,
        }
    }

    pub fn requested_len(&self) -> usize {
        match self {
            Self::Items(items) => items.len(),
            Self::Store {
                prefix,
                first_live_index,
                end_index,
                suffix,
            } => end_index
                .saturating_sub(*first_live_index)
                .saturating_add(prefix.len())
                .saturating_add(suffix.len()),
        }
    }
}

impl Default for ModelHistorySource {
    fn default() -> Self {
        Self::Items(Vec::new())
    }
}

/// Payload for [`UiCommand::StartTurn`]. Boxed at the variant so the
/// enum stays small - the other variants are channel-frequent
/// (`Steer`, `Cancel`, `PermissionDecision`, …) while StartTurn carries
/// once-per-turn request metadata and a provider-history source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartTurnPayload {
    pub turn_id: u64,
    pub input: StartTurnInput,
    pub mode: AgentMode,
    pub model: String,
    pub reasoning_effort: ReasoningEffort,
    pub history: ModelHistorySource,
    /// Override API base URL for this turn (uses engine default if None).
    pub api_base: Option<String>,
    /// Override API key for this turn (uses engine default if None).
    pub api_key: Option<String>,
    /// Session ID for plan file storage.
    pub session_id: String,
    /// On-disk directory for this session (date-bucketed).
    pub session_dir: std::path::PathBuf,
    /// Per-turn model parameter overrides (from custom commands).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model_config_overrides: Option<ModelConfigOverrides>,
    /// Per-turn permission overrides (from custom commands or Lua).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub permission_overrides: Option<PermissionOverrides>,
    /// Full system prompt supplied by the frontend.
    /// When present the engine uses this verbatim instead of rendering
    /// its built-in template.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub system_prompt: Option<String>,
    /// Tools registered in Lua. The engine
    /// includes these in the LLM tool definitions and proxies execution
    /// back to the TUI via `ToolDispatch`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<ToolDef>,
}

/// Commands sent from the UI to the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UiCommand {
    /// Start a new agent turn.
    StartTurn(Box<StartTurnPayload>),

    /// Inject a message mid-turn (steering / type-ahead).
    Steer { text: String },

    /// Remove the last `count` steered messages (user unqueued them).
    Unsteer { count: usize },

    /// Reply to a `RequestPermission` event.
    PermissionDecision {
        request_id: u64,
        approved: bool,
        message: Option<String>,
    },

    /// Append an item to the active turn's history before its next LLM request.
    AppendHistoryItem {
        append: crate::history::HistoryAppend,
    },

    /// Change the active agent mode while the engine is running. This is
    /// separate from the synthetic mode-change history note: permission
    /// decisions must observe the runtime mode immediately, even mid-turn.
    SetMode { mode: AgentMode },

    /// Change reasoning effort while the engine is running.
    SetReasoningEffort { effort: ReasoningEffort },

    /// Change the model/provider while the engine is running.
    SetModel {
        model: String,
        api_base: String,
        api_key: String,
        provider_type: String,
    },

    /// Replace cached prompt inputs after `/reload`. Updates
    /// `EngineConfig::instructions`, `EngineConfig::skill_section`, and
    /// `EngineConfig::system_prompt_override` so subsequent turns and
    /// compactions see the refreshed values.
    ReloadAgentConfig {
        instructions: Option<String>,
        skill_section: Option<String>,
        system_prompt_override: Option<String>,
    },

    /// Update the engine's cached cwd after the UI changes the process working directory.
    SetCwd { cwd: String },

    /// One-shot LLM call initiated by Lua. The engine spawns a
    /// fire-and-forget request and returns the response as
    /// `EngineAskResponse`. `model` overrides the primary model when
    /// `Some`; `response_format` enforces a JSON schema when present;
    /// `reasoning_effort` controls effort (defaults to `Off`). Overflow
    /// handling is the caller's responsibility - context-window failures
    /// surface through `EngineAskError { kind = "context_window" }` and
    /// plugins compose retry strategy in Lua.
    EngineAsk {
        id: u64,
        system: String,
        messages: Vec<Message>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        model: Option<AskModel>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        response_format: Option<AskResponseFormat>,
        #[serde(default)]
        reasoning_effort: ReasoningEffort,
        /// Tools to send alongside the request. When this matches the
        /// main session's tool list byte-for-byte, the request shares
        /// the Anthropic prefix cache with the main turn.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools: Vec<ToolDef>,
        /// Stable per-session identifier. Used as OpenAI's
        /// `prompt_cache_key` so EngineAsk requests route to the same
        /// cache shard as the main turn.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        session_id: String,
        /// On-disk directory for this session. The engine writes the SQLite
        /// request audit here for introspection.
        session_dir: PathBuf,
        /// Emit incremental text deltas as `EngineAskDelta` events.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stream: bool,
        /// Surface provider retry events on the main work indicator.
        /// Intended for foreground auxiliary work such as compaction.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        visible_retries: bool,
    },

    /// Result of a tool execution (response to `ToolDispatch`).
    ToolResult {
        request_id: u64,
        call_id: String,
        content: String,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        metadata: Option<serde_json::Value>,
    },

    /// Result of evaluating tool metadata and central permission policy
    /// (response to `EngineEvent::ToolEvaluationRequest`).
    ToolEvaluationResponse {
        request_id: u64,
        evaluation: ToolEvaluation,
    },

    /// Side-call from Lua to a core tool.
    /// The engine runs the named tool and replies with
    /// `EngineEvent::CoreToolResult`. The parent `call_id` is
    /// reused so streamed output (e.g. `ToolOutput`) is grouped under
    /// the visible tool invocation.
    CallCoreTool {
        request_id: u64,
        parent_call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
    },

    /// Cancel the current turn.
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- ToolExecutionMode defaults + rename ----

    #[test]
    fn tool_execution_mode_default_is_concurrent() {
        assert_eq!(ToolExecutionMode::default(), ToolExecutionMode::Concurrent);
    }

    #[test]
    fn tool_execution_mode_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_value(ToolExecutionMode::Sequential).unwrap(),
            json!("sequential")
        );
    }

    // ---- ToolHookFlags::any ----

    #[test]
    fn tool_hook_flags_any_false_when_all_unset() {
        assert!(!ToolHookFlags::default().any());
    }

    #[test]
    fn tool_hook_flags_any_true_when_approval_patterns_set() {
        let f = ToolHookFlags {
            approval_patterns: true,
            ..Default::default()
        };
        assert!(f.any());
    }

    #[test]
    fn tool_hook_flags_any_true_when_preflight_set() {
        let f = ToolHookFlags {
            preflight: true,
            ..Default::default()
        };
        assert!(f.any());
    }

    // ---- Decision ----

    #[test]
    fn decision_default_is_allow() {
        assert_eq!(Decision::default(), Decision::Allow);
    }

    #[test]
    fn decision_serializes_unit_variants_as_snake_case_strings() {
        assert_eq!(
            serde_json::to_value(Decision::Allow).unwrap(),
            json!("allow")
        );
        assert_eq!(serde_json::to_value(Decision::Ask).unwrap(), json!("ask"));
        assert_eq!(serde_json::to_value(Decision::Deny).unwrap(), json!("deny"));
    }

    #[test]
    fn decision_error_variant_serializes_with_rename_tag() {
        let v = serde_json::to_value(Decision::Error("boom".into())).unwrap();
        assert_eq!(v, json!({"error": "boom"}));
    }

    #[test]
    fn decision_error_roundtrips_through_json() {
        let d = Decision::Error("x".into());
        let v = serde_json::to_value(&d).unwrap();
        let back: Decision = serde_json::from_value(v).unwrap();
        assert_eq!(back, Decision::Error("x".into()));
    }

    // ---- ToolDef defaults ----

    #[test]
    fn tool_def_deserialize_defaults_optional_fields() {
        let t: ToolDef = serde_json::from_value(json!({
            "name": "n",
            "description": "d",
            "parameters": {}
        }))
        .unwrap();
        assert!(t.modes.is_none());
        assert_eq!(t.execution_mode, ToolExecutionMode::Concurrent);
        assert!(!t.override_core);
        assert!(!t.hooks.any());
    }

    #[test]
    fn tool_def_skip_serializing_none_modes() {
        let t = ToolDef {
            name: "n".into(),
            description: "d".into(),
            parameters: json!({}),
            modes: None,
            execution_mode: ToolExecutionMode::Concurrent,
            override_core: false,
            hooks: ToolHookFlags::default(),
            headless: true,
        };
        let v = serde_json::to_value(&t).unwrap();
        assert!(v.get("modes").is_none());
    }

    // ---- ToolMetadata ----

    #[test]
    fn tool_metadata_default_is_empty() {
        let h = ToolMetadata::default();
        assert!(h.approval_patterns.is_empty());
        assert!(h.preflight_error.is_none());
        assert!(h.summary.is_empty());
    }

    #[test]
    fn tool_evaluation_default_decision_is_allow() {
        let e = ToolEvaluation::default();
        assert_eq!(e.decision, Decision::Allow);
        assert!(e.metadata.approval_patterns.is_empty());
        assert!(e.metadata.preflight_error.is_none());
        assert!(e.metadata.summary.is_empty());
    }

    // ---- EngineEvent roundtrip sanity ----

    #[test]
    fn engine_event_token_usage_background_defaults_to_false_on_deserialize() {
        let v = json!({
            "TokenUsage": {
                "usage": {},
                "tokens_per_sec": null,
                "cost_usd": null,
            }
        });
        let e: EngineEvent = serde_json::from_value(v).unwrap();
        match e {
            EngineEvent::TokenUsage { background, .. } => assert!(!background),
            _ => panic!("expected TokenUsage"),
        }
    }

    #[test]
    fn engine_event_ready_serializes_as_string_variant() {
        let v = serde_json::to_value(EngineEvent::Ready).unwrap();
        assert_eq!(v, json!("Ready"));
    }

    // ---- StartTurnPayload optional fields ----

    #[test]
    fn start_turn_payload_omits_none_overrides_on_serialize() {
        let p = StartTurnPayload {
            turn_id: 1,
            input: StartTurnInput::user(Content::text("hi"), None),
            mode: AgentMode::normal(),
            model: "m".into(),
            reasoning_effort: ReasoningEffort::Off,
            history: ModelHistorySource::default(),
            api_base: None,
            api_key: None,
            session_id: "s".into(),
            session_dir: std::path::PathBuf::from("/tmp"),
            model_config_overrides: None,
            permission_overrides: None,
            system_prompt: None,
            tools: vec![],
        };
        let v = serde_json::to_value(&p).unwrap();
        assert!(v["input"].get("display").is_none());
        assert!(v.get("model_config_overrides").is_none());
        assert!(v.get("permission_overrides").is_none());
        assert!(v.get("system_prompt").is_none());
        assert!(v.get("tools").is_none());
    }
}
