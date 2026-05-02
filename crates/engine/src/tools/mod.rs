use crate::provider::{FunctionSchema, ToolDefinition};
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

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn execute<'a>(&'a self, args: HashMap<String, Value>, ctx: &'a ToolContext) -> ToolFuture<'a>;
    /// Evaluate per-call permission hooks. Returns a `ToolHooks`
    /// carrying:
    /// - `needs_confirm`: confirm-dialog message (None falls back to the
    ///   tool name).
    /// - `approval_patterns`: glob patterns offered as session-level
    ///   "always allow" choices.
    /// - `preflight_error`: pre-execution error that skips the dialog
    ///   and fails the call immediately.
    ///
    /// Mirrors the shape returned by plugin tools through
    /// `ToolHooksRequest` so the engine consumes both paths
    /// uniformly.
    fn evaluate_hooks(&self, _args: &HashMap<String, Value>) -> ToolHooks {
        ToolHooks::default()
    }
}

pub struct ToolEntry {
    pub(crate) tool: Box<dyn Tool>,
    /// MCP tools use the `mcp` permission ruleset rather than the per-tool
    /// `tools` ruleset; tracked here so the trait stays dispatch-only.
    pub(crate) is_mcp: bool,
}

/// Resolves and executes tool calls during a turn. The engine
/// never touches tool impls directly — every per-call decision (schema
/// list, hook eval, dispatch, ruleset selection) routes through this
/// trait. A future tui-side `ToolRuntime` walks a Lua-driven registry
/// behind the same surface.
///
/// Lookup, hook evaluation, and dispatch all return `Option` so the
/// engine can synthesise a "tool not found" result when the LLM emits
/// a call for a tool the dispatcher doesn't know.
///
/// The trait carries no permission policy — the engine returns the
/// unfiltered tool list from `definitions` and applies no mode or
/// permission filtering. That concern moved to `tui::permissions` and
/// Lua-tool hooks in P5.c.
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

impl ToolDispatcher for ToolRegistry {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|e| {
                ToolDefinition::new(FunctionSchema {
                    name: e.tool.name().into(),
                    description: e.tool.description().into(),
                    parameters: e.tool.parameters(),
                })
            })
            .collect()
    }

    fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    fn is_mcp(&self, name: &str) -> bool {
        self.get(name).is_some_and(|e| e.is_mcp)
    }

    fn evaluate_hooks(
        &self,
        name: &str,
        args: &HashMap<String, Value>,
        _mode: protocol::AgentMode,
    ) -> Option<ToolHooks> {
        self.get(name).map(|e| e.tool.evaluate_hooks(args))
    }

    fn dispatch<'a>(
        &'a self,
        name: &str,
        args: HashMap<String, Value>,
        ctx: &'a ToolContext,
    ) -> Option<ToolFuture<'a>> {
        self.get(name).map(|e| e.tool.execute(args, ctx))
    }
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<ToolEntry>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_mcp(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(ToolEntry { tool, is_mcp: true });
    }

    pub fn get(&self, name: &str) -> Option<&ToolEntry> {
        self.tools.iter().find(|e| e.tool.name() == name)
    }
}

pub fn build_tools() -> ToolRegistry {
    ToolRegistry::new()
}
