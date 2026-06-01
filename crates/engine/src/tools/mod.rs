use crate::provider::ToolDefinition;
use protocol::ToolEvaluation;
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

    fn is_visible(&self, _name: &str, _mode: protocol::AgentMode) -> bool {
        true
    }

    /// `None` means the tool is unknown.
    fn evaluate_tool_call(
        &self,
        name: &str,
        args: &HashMap<String, Value>,
        mode: protocol::AgentMode,
        permission_overrides: Option<&protocol::PermissionOverrides>,
    ) -> Option<ToolEvaluation>;

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

    fn evaluate_tool_call(
        &self,
        _name: &str,
        _args: &HashMap<String, Value>,
        _mode: protocol::AgentMode,
        _permission_overrides: Option<&protocol::PermissionOverrides>,
    ) -> Option<ToolEvaluation> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_ok_marks_is_error_false() {
        let r = ToolResult::ok("done");
        assert_eq!(r.content, "done");
        assert!(!r.is_error);
        assert!(r.metadata.is_none());
    }

    #[test]
    fn tool_result_err_marks_is_error_true() {
        let r = ToolResult::err("boom");
        assert_eq!(r.content, "boom");
        assert!(r.is_error);
        assert!(r.metadata.is_none());
    }

    #[test]
    fn empty_dispatcher_definitions_returns_empty_vec() {
        let d = EmptyDispatcher::new();
        assert!(d.definitions().is_empty());
    }

    #[test]
    fn empty_dispatcher_contains_returns_false_for_any_name() {
        let d = EmptyDispatcher;
        assert!(!d.contains("anything"));
        assert!(!d.contains(""));
    }

    #[test]
    fn empty_dispatcher_default_is_visible_returns_true() {
        // Trait-default is_visible returns true; EmptyDispatcher inherits it.
        assert!(EmptyDispatcher.is_visible("anything", protocol::AgentMode::parse("plan").unwrap()));
    }

    #[test]
    fn empty_dispatcher_evaluate_tool_call_returns_none() {
        let d = EmptyDispatcher;
        let res = d.evaluate_tool_call(
            "name",
            &HashMap::new(),
            protocol::AgentMode::parse("plan").unwrap(),
            None,
        );
        assert!(res.is_none());
    }

    #[test]
    fn empty_dispatcher_dispatch_returns_none() {
        let d = EmptyDispatcher;
        let ctx = ToolContext;
        let res = d.dispatch("name", HashMap::new(), &ctx);
        assert!(res.is_none());
    }
}
