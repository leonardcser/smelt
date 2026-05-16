//! Wire protocol between the engine and the UI.

use crate::content::Content;
use crate::message::{Message, ToolOutcome};
use crate::mode::{AgentMode, ReasoningEffort};
use crate::usage::{ModelConfigOverrides, PermissionOverrides, TokenUsage, TurnMeta};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Auxiliary LLM task — routed to a dedicated auxiliary model when the
/// engine config has one, otherwise falls back to the primary model.
/// `Btw` is the generic escape hatch (plain `/btw` prompts, plugin
/// `engine.ask` calls without a specific task tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuxiliaryTask {
    Title,
    Prediction,
    Compaction,
    #[default]
    Btw,
}

/// How a registered tool interacts with concurrent tool execution.
///
/// `Concurrent` (default): runs alongside other tools via the engine's
/// `pending_tools` channel — good for pure data fetches with no UI.
///
/// `Sequential`: deferred until after every concurrent tool has
/// finished, then dispatched one at a time. Used by tools that open a
/// dialog and await a user reply — the user should see all other tool
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
    /// Hook signals declared by the tool. Each `true` flag tells the
    /// engine to round-trip through `ToolHooksRequest` before
    /// dispatching the tool — to ask the user for permission, run a
    /// preflight check, etc. When all flags are false the engine
    /// dispatches the tool directly (today's behavior, no permission
    /// gate). Tools that touch security-sensitive surfaces
    /// (bash, file mutation) MUST opt in.
    #[serde(default)]
    pub hooks: ToolHookFlags,
}

/// Which permission hooks a tool has registered. Sent with
/// `ToolDef` so the engine knows whether to ask the TUI to
/// evaluate them per-call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolHookFlags {
    #[serde(default)]
    pub confirm_text: bool,
    #[serde(default)]
    pub approval_patterns: bool,
    #[serde(default)]
    pub preflight: bool,
}

impl ToolHookFlags {
    /// True when at least one hook is registered — i.e. the engine must
    /// round-trip through `ToolHooksRequest` before dispatch.
    pub fn any(&self) -> bool {
        self.confirm_text || self.approval_patterns || self.preflight
    }
}

/// Final permission decision for a single tool call, produced by the
/// dispatcher after evaluating hooks and checking policy.
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

/// Result of evaluating a tool's permission hooks for a specific
/// invocation. Returned by the TUI in response to
/// `EngineEvent::ToolHooksRequest` (Lua tools) or by the dispatcher's
/// `evaluate_hooks` (MCP / core tools).
///
/// The `decision` field is authoritative: `Allow` → dispatch,
/// `Ask` → prompt the user, `Deny` → synthetic denial,
/// `Error(msg)` → synthetic error result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolHooks {
    /// Final permission decision.
    #[serde(default)]
    pub decision: Decision,
    /// Confirm dialog message; used when `decision == Ask`.
    /// `None` falls back to the tool name.
    #[serde(default)]
    pub confirm_message: Option<String>,
    /// Approval patterns to offer "always allow" for.
    /// Used when `decision == Ask`.
    #[serde(default)]
    pub approval_patterns: Vec<String>,
    /// One-line human summary of this invocation. Comes from the
    /// tool's `summary(args)` Lua callback. Used by the confirm
    /// dialog header and the engine's `RequestPermission.summary`
    /// — the engine never extracts arg fields by tool name.
    #[serde(default)]
    pub summary: Option<String>,
}

/// Events emitted by the engine. The UI consumes these to update its display.
///
/// Most variants are fire-and-forget. The exception is `RequestPermission`,
/// which carries a `request_id` that the UI must eventually reply to via
/// `UiCommand::PermissionDecision`.
///
/// Event ordering within a turn:
///   Ready → (Thinking* → Text* → ToolStarted → ToolOutput* → ToolFinished)*
///         → TurnComplete | TurnError
///
/// ProcessCompleted can arrive at any time (including between turns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EngineEvent {
    /// Engine has initialized and is ready to accept commands.
    Ready,

    /// Extended thinking / chain-of-thought text.
    Thinking { content: String },

    /// Incremental thinking token from the LLM (streaming delta).
    ThinkingDelta { delta: String },

    /// Streamed assistant text (may arrive in chunks).
    Text { content: String },

    /// Incremental text token from the LLM (streaming delta).
    TextDelta { delta: String },

    /// Incremental tool-call argument fragment streamed from the LLM.
    /// Observers reassemble the JSON; the engine has already accumulated
    /// it internally and will dispatch on the completed call.
    ToolArgsDelta {
        call_id: String,
        tool_name: String,
        delta: String,
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

    /// Engine needs user permission before executing a tool.
    RequestPermission {
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
        confirm_message: String,
        approval_patterns: Vec<String>,
        summary: Option<String>,
    },

    /// Token usage update after an LLM call.
    TokenUsage {
        usage: TokenUsage,
        tokens_per_sec: Option<f64>,
        cost_usd: Option<f64>,
        /// True for background requests (title, compaction, btw, predict)
        /// whose prompt_tokens should not update the displayed context usage.
        #[serde(default)]
        background: bool,
    },

    /// LLM call failed, engine is retrying.
    Retrying { delay_ms: u64, attempt: u32 },

    /// A background process has finished.
    ProcessCompleted { id: String, exit_code: Option<i32> },

    /// Response to `UiCommand::Compact`.
    CompactionComplete { messages: Vec<Message> },

    /// Response to `UiCommand::GenerateTitle`.
    TitleGenerated { title: String, slug: String },

    /// Response to `UiCommand::Btw`.
    BtwResponse { content: String },

    /// Response to a `UiCommand::EngineAsk` request.
    EngineAskResponse { id: u64, content: String },

    /// Snapshot of the engine's message list, sent after each significant step.
    Messages {
        turn_id: u64,
        messages: Vec<Message>,
    },

    /// The agent turn completed successfully.
    TurnComplete {
        turn_id: u64,
        messages: Vec<Message>,
        meta: Option<TurnMeta>,
    },

    /// The agent turn ended with an error.
    TurnError { message: String },

    /// Engine is shutting down.
    Shutdown { reason: Option<String> },

    /// Engine needs the TUI to execute a Lua-defined tool.
    ToolDispatch {
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
    },

    /// Engine asks the TUI to evaluate a tool's permission
    /// hooks (`confirm_text`, `approval_patterns`, `preflight`) for a
    /// specific invocation. The TUI replies with
    /// `UiCommand::ToolHooksResponse`, after which the engine
    /// resumes the standard Allow / Deny / Ask flow.
    ToolHooksRequest {
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

/// Payload for [`UiCommand::StartTurn`]. Boxed at the variant so the
/// enum stays small — the other variants are channel-frequent
/// (`Steer`, `Cancel`, `PermissionDecision`, …) while StartTurn is
/// once-per-turn and carries the full message history + tool list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartTurnPayload {
    pub turn_id: u64,
    pub content: Content,
    pub mode: AgentMode,
    pub model: String,
    pub reasoning_effort: ReasoningEffort,
    pub history: Vec<Message>,
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
    /// Full system prompt assembled by the TUI (from prompt sections).
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

    /// Change the active mode while the engine is running.
    SetAgentMode {
        mode: AgentMode,
        /// Updated system prompt for the new mode (if managed by TUI).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        system_prompt: Option<String>,
        /// Updated tools for the new mode.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        tools: Option<Vec<ToolDef>>,
    },

    /// Change reasoning effort while the engine is running.
    SetReasoningEffort { effort: ReasoningEffort },

    /// Change the model/provider while the engine is running.
    SetModel {
        model: String,
        api_base: String,
        api_key: String,
        provider_type: String,
    },

    /// Compact conversation history.
    Compact {
        history: Vec<Message>,
        instructions: Option<String>,
    },

    /// Generate a title for the session based on the latest user message and
    /// the tail of the assistant's response.
    GenerateTitle {
        last_user_message: String,
        assistant_tail: String,
    },

    /// Ask an ephemeral side question (no tools, not added to history).
    Btw {
        question: String,
        history: Vec<Message>,
        reasoning_effort: ReasoningEffort,
    },

    /// One-shot LLM call initiated by Lua. The engine spawns
    /// a fire-and-forget request routed through `task`'s auxiliary
    /// model (or the primary model when the routing slot is empty) and
    /// returns the response as `EngineAskResponse`.
    EngineAsk {
        id: u64,
        system: String,
        messages: Vec<Message>,
        #[serde(default)]
        task: AuxiliaryTask,
    },

    /// Result of a tool execution (response to `ToolDispatch`).
    ToolResult {
        request_id: u64,
        call_id: String,
        content: String,
        is_error: bool,
    },

    /// Result of evaluating a tool's permission hooks (response
    /// to `EngineEvent::ToolHooksRequest`).
    ToolHooksResponse { request_id: u64, hooks: ToolHooks },

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

    // ---- AuxiliaryTask / ToolExecutionMode defaults + rename ----

    #[test]
    fn auxiliary_task_default_is_btw() {
        assert_eq!(AuxiliaryTask::default(), AuxiliaryTask::Btw);
    }

    #[test]
    fn auxiliary_task_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_value(AuxiliaryTask::Title).unwrap(),
            json!("title")
        );
        assert_eq!(
            serde_json::to_value(AuxiliaryTask::Compaction).unwrap(),
            json!("compaction")
        );
    }

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
    fn tool_hook_flags_any_true_when_confirm_text_set() {
        let f = ToolHookFlags {
            confirm_text: true,
            ..Default::default()
        };
        assert!(f.any());
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
        };
        let v = serde_json::to_value(&t).unwrap();
        assert!(v.get("modes").is_none());
    }

    // ---- ToolHooks ----

    #[test]
    fn tool_hooks_default_decision_is_allow() {
        let h = ToolHooks::default();
        assert_eq!(h.decision, Decision::Allow);
        assert!(h.confirm_message.is_none());
        assert!(h.approval_patterns.is_empty());
        assert!(h.summary.is_none());
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
            content: Content::text("hi"),
            mode: AgentMode::Normal,
            model: "m".into(),
            reasoning_effort: ReasoningEffort::Off,
            history: vec![],
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
        assert!(v.get("model_config_overrides").is_none());
        assert!(v.get("permission_overrides").is_none());
        assert!(v.get("system_prompt").is_none());
        assert!(v.get("tools").is_none());
    }
}
