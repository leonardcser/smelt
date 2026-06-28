//! Host-callback channel - typed RPCs from the engine task to whoever
//! runs Lua (TUI / headless). Each variant carries its own
//! `oneshot::Sender<Reply>` so correlation is "the channel handle"
//! instead of an integer id + lookup table.
//!
//! Why not put these on [`protocol::EngineEvent`] / [`protocol::UiCommand`]?
//! Those types are `Serialize` for JSON output and persistence; an embedded
//! `oneshot::Sender` is neither serializable nor meaningful across process
//! boundaries. Splitting host RPC into its own channel keeps the protocol
//! surface clean and lets us add new RPCs by adding a single enum variant
//! plus a handler arm on the consumer side - no protocol churn, no pending
//! HashMap on the engine, no `*Response` variants on `UiCommand`.

use protocol::Message;
use std::path::PathBuf;
use tokio::sync::oneshot;

#[derive(Debug)]
pub enum HostRequestDecision {
    Continue,
    Replace(Vec<Message>),
    Abort(String),
}

/// One synchronous request from the engine to the host (TUI / headless).
/// The host must `reply.send(...)` exactly once or the engine's awaiting
/// future will resolve with the channel's `Closed` error and fall back
/// to a default; both paths are intentional - dropping a `reply` is a
/// "no-op" signal.
pub enum HostCall {
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
    /// conversation to retry with. `Replace(msgs)` swaps the engine's
    /// `messages` (excluding the system prompt at index 0) and re-runs
    /// the loop; `Continue` (no hook registered, hook returned nil, or hook
    /// failed) aborts the turn with the existing `TurnError`; `Abort(message)`
    /// aborts with a host-provided terminal error.
    RecoverFromContextLimit {
        messages: Vec<Message>,
        reply: oneshot::Sender<HostRequestDecision>,
    },

    /// Append a provider request audit row through the host's session
    /// persistence worker. The worker owns the session database writer, so
    /// audit writes stay ordered with history and metadata saves.
    RequestAudit {
        session_dir: PathBuf,
        entry: Box<protocol::request_log::RequestLogEntry>,
        payload_mode: smelt_store::RequestAuditPayloadMode,
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
        reply: oneshot::Sender<HostRequestDecision>,
    },
}
