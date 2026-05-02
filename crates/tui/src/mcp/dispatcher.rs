use crate::mcp::{McpManager, McpToolDef};
use engine::provider::{FunctionSchema, ToolDefinition};
use engine::tools::{ToolContext, ToolDispatcher, ToolFuture, ToolResult};
use protocol::ToolHooks;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Dispatches MCP tools to their servers.
pub struct McpDispatcher {
    manager: Arc<McpManager>,
    defs: Vec<McpToolDef>,
}

impl McpDispatcher {
    pub async fn start(configs: &HashMap<String, crate::mcp::McpServerConfig>) -> Option<Self> {
        if configs.is_empty() {
            return None;
        }
        let manager = crate::mcp::McpManager::start(configs).await;
        let defs = manager.tool_defs().await;
        Some(Self { manager, defs })
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

    fn evaluate_hooks(&self, name: &str, _args: &HashMap<String, Value>) -> Option<ToolHooks> {
        let def = self.defs.iter().find(|d| d.qualified_name() == name)?;
        Some(ToolHooks {
            needs_confirm: Some(format!("MCP {}_{}", def.server_name, def.tool_name)),
            ..Default::default()
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
