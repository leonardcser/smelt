use crate::mcp::{McpManager, McpToolDef};
use engine::provider::{FunctionSchema, ToolDefinition};
use engine::tools::{ToolContext, ToolDispatcher, ToolFuture, ToolResult};
use protocol::{AgentMode, ToolHooks};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Dispatches MCP tools to their servers.
pub struct McpDispatcher {
    manager: Arc<McpManager>,
    defs: Vec<McpToolDef>,
    permissions: Arc<crate::permissions::Permissions>,
    runtime_approvals: Arc<std::sync::RwLock<crate::permissions::RuntimeApprovals>>,
}

impl McpDispatcher {
    pub async fn start(
        configs: &HashMap<String, crate::mcp::McpServerConfig>,
        permissions: Arc<crate::permissions::Permissions>,
        runtime_approvals: Arc<std::sync::RwLock<crate::permissions::RuntimeApprovals>>,
    ) -> Option<Self> {
        if configs.is_empty() {
            return None;
        }
        let manager = crate::mcp::McpManager::start(configs).await;
        let defs = manager.tool_defs().await;
        Some(Self {
            manager,
            defs,
            permissions,
            runtime_approvals,
        })
    }
}

impl ToolDispatcher for McpDispatcher {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.defs
            .iter()
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
        self.defs.iter().any(|d| d.qualified_name() == name)
    }

    fn is_mcp(&self, _name: &str) -> bool {
        true
    }

    fn is_visible(&self, name: &str, mode: AgentMode) -> bool {
        self.defs.iter().any(|d| d.qualified_name() == name)
            && self.permissions.check_mcp(mode, name) != protocol::Decision::Deny
    }

    fn evaluate_hooks(
        &self,
        name: &str,
        args: &HashMap<String, Value>,
        mode: AgentMode,
    ) -> Option<ToolHooks> {
        let def = self.defs.iter().find(|d| d.qualified_name() == name)?;
        let confirm_message = format!("MCP {}_{}", def.server_name, def.tool_name);
        let mut decision = self.permissions.decide(mode, name, args, true);
        if decision == protocol::Decision::Ask {
            let rt = self.runtime_approvals.read().unwrap();
            if rt.is_auto_approved(&self.permissions, mode, name, args, &confirm_message) {
                decision = protocol::Decision::Allow;
            }
        }
        Some(ToolHooks {
            decision,
            confirm_message: Some(confirm_message),
            approval_patterns: Vec::new(),
        })
    }

    fn dispatch<'a>(
        &'a self,
        name: &str,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> Option<ToolFuture<'a>> {
        let def = self.defs.iter().find(|d| d.qualified_name() == name)?;
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
