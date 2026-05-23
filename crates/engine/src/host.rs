//! Host-callback channel — typed RPCs from the engine task to whoever
//! runs Lua (TUI / headless). Each variant carries its own
//! `oneshot::Sender<Reply>` so correlation is "the channel handle"
//! instead of an integer id + lookup table.
//!
//! Why not put these on [`protocol::EngineEvent`] / [`protocol::UiCommand`]?
//! Those types are `Serialize` for JSON output and persistence; an embedded
//! `oneshot::Sender` is neither serializable nor meaningful across process
//! boundaries. Splitting host RPC into its own channel keeps the protocol
//! surface clean and lets us add new RPCs by adding a single enum variant
//! plus a handler arm on the consumer side — no protocol churn, no pending
//! HashMap on the engine, no `*Response` variants on `UiCommand`.

use protocol::{AgentMode, Message, ToolHooks, ToolOutcome};
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::oneshot;

/// One synchronous request from the engine to the host (TUI / headless).
/// The host must `reply.send(...)` exactly once or the engine's awaiting
/// future will resolve with the channel's `Closed` error and fall back
/// to a default; both paths are intentional — dropping a `reply` is a
/// "no-op" signal.
pub enum HostCall {
    /// Execute a Lua-defined tool. Engine awaits the `ToolOutcome`.
    DispatchTool {
        call_id: String,
        tool_name: String,
        args: HashMap<String, Value>,
        reply: oneshot::Sender<ToolOutcome>,
    },

    /// Evaluate a Lua tool's permission hooks
    /// (`summary`, `approval_patterns`, `preflight`).
    EvalHooks {
        call_id: String,
        tool_name: String,
        args: HashMap<String, Value>,
        mode: AgentMode,
        reply: oneshot::Sender<ToolHooks>,
    },

    /// Surface an Ask-tier tool decision to the user.
    /// `reply` carries `(approved, optional_reason_message)`.
    AskPermission {
        call_id: String,
        tool_name: String,
        args: HashMap<String, Value>,
        description: String,
        reply: oneshot::Sender<PermissionDecision>,
    },

    /// Run `smelt.provider.middleware{on_request=...}` hooks against
    /// the chat history about to be sent. `Some(msgs)` replaces the
    /// engine's slice; `None` leaves it untouched. Hooks see the full
    /// `Vec<Message>` including the system prompt at index 0.
    ProviderRequest {
        messages: Vec<Message>,
        reply: oneshot::Sender<Option<Vec<Message>>>,
    },

    /// Run `smelt.provider.middleware{on_response=...}` hooks against
    /// the assembled assistant message. `Some(msg)` replaces it before
    /// it's pushed to history; `None` keeps the original.
    ProviderResponse {
        message: Message,
        reply: oneshot::Sender<Option<Message>>,
    },

    /// Engine hit a context-window error mid-turn. The host's registered
    /// recovery hook (`smelt.engine.on_context_limit`) is invoked with
    /// the conversation up to that point and returns a shorter
    /// conversation to retry with. `Some(msgs)` swaps the engine's
    /// `messages` (excluding the system prompt at index 0) and re-runs
    /// the loop; `None` (no hook registered, hook returned nil, or hook
    /// failed) aborts the turn with the existing `TurnError`.
    RecoverFromContextLimit {
        messages: Vec<Message>,
        reply: oneshot::Sender<Option<Vec<Message>>>,
    },

    /// Engine is about to send a model request. The host may replace
    /// the conversation before the request is
    /// sent. `messages` excludes the system prompt; `estimated_tokens`
    /// is a conservative estimate for the exact message slice the
    /// engine is about to sample with, used to catch drift since the
    /// provider's last reported prompt-token count.
    PrepareRequest {
        messages: Vec<Message>,
        estimated_tokens: u32,
        reply: oneshot::Sender<Option<Vec<Message>>>,
    },
}

/// Outcome of an `AskPermission` round-trip.
#[derive(Debug, Clone)]
pub struct PermissionDecision {
    pub approved: bool,
    pub message: Option<String>,
}
