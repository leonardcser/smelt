use crate::provider::ToolDefinition;
use protocol::ToolHooks;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
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

pub struct ToolContext;

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Resolves and executes tool calls. Every per-call decision routes through this
/// trait; the engine never touches tool impls directly.
pub trait ToolDispatcher: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    fn contains(&self, name: &str) -> bool;

    fn is_mcp(&self, name: &str) -> bool;

    fn is_visible(&self, _name: &str, _mode: protocol::AgentMode) -> bool {
        true
    }

    /// `None` means the tool is unknown.
    fn evaluate_hooks(
        &self,
        name: &str,
        args: &HashMap<String, Value>,
        mode: protocol::AgentMode,
    ) -> Option<ToolHooks>;

    fn dispatch<'a>(
        &'a self,
        name: &str,
        args: HashMap<String, Value>,
        ctx: &'a ToolContext,
    ) -> Option<ToolFuture<'a>>;
}

/// No-op dispatcher used when no MCP servers are configured.
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
