use crate::provider::ToolDefinition;
use protocol::ToolHooks;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    /// Structured metadata passed through to ToolOutcome for machine-readable data.
    pub metadata: Option<serde_json::Value>,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: None,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: None,
        }
    }
}

/// Context provided to tools during execution. All Tool impls left in
/// engine (MCP adapters) ignore it — kept as a placeholder so the
/// trait signature can grow back if a future engine-side tool needs
/// cancel propagation or other engine facilities.
pub struct ToolContext;

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Resolves and executes tool calls during a turn. The engine
/// never touches tool impls directly — every per-call decision (schema
/// list, hook eval, dispatch, ruleset selection) routes through this
/// trait. The frontend's Lua-tool registry runs through `UiCommand::
/// ToolDispatch`; engine-side this trait is implemented only by
/// `core::mcp::dispatcher::McpDispatcher` (and `EmptyDispatcher` for
/// the no-MCP fallback).
///
/// Lookup, hook evaluation, and dispatch all return `Option` so the
/// engine can synthesise a "tool not found" result when the LLM emits
/// a call for a tool the dispatcher doesn't know.
pub trait ToolDispatcher: Send + Sync {
    /// All tool definitions registered with this dispatcher.
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// True when the named tool exists in this dispatcher.
    fn contains(&self, name: &str) -> bool;

    /// True when the named tool routes through the `mcp` permission
    /// ruleset rather than the per-tool `tools` ruleset.
    fn is_mcp(&self, name: &str) -> bool;

    /// Whether the tool should be visible to the LLM in the given mode.
    /// `false` hides tools whose policy decision is `Deny`.
    fn is_visible(&self, _name: &str, _mode: protocol::AgentMode) -> bool {
        true
    }

    /// Per-call permission hooks. `None` means the tool is unknown.
    /// The dispatcher evaluates policy and returns the final decision.
    fn evaluate_hooks(
        &self,
        name: &str,
        args: &HashMap<String, Value>,
        mode: protocol::AgentMode,
    ) -> Option<ToolHooks>;

    /// Dispatch a tool call. `None` means the tool is unknown; the
    /// engine handles that case by emitting a synthetic error result.
    fn dispatch<'a>(
        &'a self,
        name: &str,
        args: HashMap<String, Value>,
        ctx: &'a ToolContext,
    ) -> Option<ToolFuture<'a>>;
}

/// No-op dispatcher: holds no tools, denies every lookup. Used as the
/// engine's tool surface when no MCP servers are configured. Lua tools
/// route through `UiCommand::ToolDispatch` and don't appear here.
#[derive(Default)]
pub struct EmptyDispatcher;

impl EmptyDispatcher {
    pub fn new() -> Self {
        Self
    }
}

impl ToolDispatcher for EmptyDispatcher {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    fn contains(&self, _name: &str) -> bool {
        false
    }

    fn is_mcp(&self, _name: &str) -> bool {
        false
    }

    fn evaluate_hooks(
        &self,
        _name: &str,
        _args: &HashMap<String, Value>,
        _mode: protocol::AgentMode,
    ) -> Option<ToolHooks> {
        None
    }

    fn dispatch<'a>(
        &'a self,
        _name: &str,
        _args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> Option<ToolFuture<'a>> {
        None
    }
}
