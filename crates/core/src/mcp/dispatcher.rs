use crate::mcp::{args_summary, McpManager, McpToolDef};
use crate::permissions::ToolOrigin;
use engine::tools::{ToolContext, ToolDispatcher, ToolFuture, ToolResult};
use protocol::{AgentMode, ToolEvaluation, ToolMetadata};
use serde_json::Value;
use smelt_provider::{FunctionSchema, ToolDefinition};
use std::collections::HashMap;
use std::sync::Arc;

pub struct McpDispatcher {
    manager: Arc<McpManager>,
    permissions: Arc<crate::permissions::Permissions>,
}

impl McpDispatcher {
    /// Wrap an existing `McpManager` for use as the engine's tool
    /// dispatcher. Callers typically build the manager via
    /// [`McpManager::start`] and pass a clone of the `Arc` here, keeping
    /// a second clone on `Core` for Lua introspection. The dispatcher
    /// holds no cached tool list - it queries `manager.tool_defs()` on
    /// every access so `/reload`-driven server changes are picked up
    /// immediately.
    pub fn new(
        manager: Arc<McpManager>,
        permissions: Arc<crate::permissions::Permissions>,
    ) -> Self {
        Self {
            manager,
            permissions,
        }
    }

    fn def_for(&self, name: &str) -> Option<McpToolDef> {
        self.manager
            .tool_defs()
            .into_iter()
            .find(|d| d.qualified_name() == name)
    }
}

impl ToolDispatcher for McpDispatcher {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.manager
            .tool_defs()
            .into_iter()
            .map(|d| {
                ToolDefinition::new(FunctionSchema {
                    name: d.qualified_name(),
                    description: d.description.clone(),
                    parameters: d.input_schema.clone(),
                })
            })
            .collect()
    }

    fn contains(&self, name: &str) -> bool {
        self.def_for(name).is_some()
    }

    fn is_visible(&self, name: &str, mode: AgentMode) -> bool {
        self.def_for(name).is_some()
            && self.permissions.check_subcommand(mode, "mcp", name) != protocol::Decision::Deny
    }

    fn evaluate_tool_call(
        &self,
        name: &str,
        args: &HashMap<String, Value>,
        mode: AgentMode,
        permission_overrides: Option<&protocol::PermissionOverrides>,
    ) -> Option<ToolEvaluation> {
        self.def_for(name)?;
        let summary = args_summary(args);
        let permissions;
        let active_permissions = if let Some(overrides) = permission_overrides {
            permissions = self.permissions.with_overrides(overrides);
            &permissions
        } else {
            self.permissions.as_ref()
        };
        let outcome =
            active_permissions.evaluate_tool_with_approvals(mode, ToolOrigin::Mcp, name, args);
        let decision = outcome.decision;
        Some(ToolEvaluation {
            decision,
            metadata: ToolMetadata {
                approval_patterns: Vec::new(),
                preflight_error: None,
                summary,
            },
        })
    }

    fn dispatch<'a>(
        &'a self,
        name: &str,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> Option<ToolFuture<'a>> {
        let def = self.def_for(name)?;
        let manager = Arc::clone(&self.manager);
        let server_name = def.server_name.clone();
        let tool_name = def.tool_name.clone();
        let timeout = def.timeout;
        let args_value = serde_json::to_value(&args).unwrap_or(Value::Object(Default::default()));
        Some(Box::pin(async move {
            match manager
                .call_tool(&server_name, &tool_name, args_value, timeout)
                .await
            {
                Ok(output) => ToolResult::ok(output),
                Err(e) => ToolResult::err(e),
            }
        }))
    }
}
