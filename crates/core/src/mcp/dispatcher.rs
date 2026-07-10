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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{McpServer, McpServerConfig, McpTransportConfig};
    use crate::permissions::rules::{RawPerms, RawRuleSet, ToolDefaults};
    use protocol::Decision;
    use std::sync::RwLock;
    use std::time::Duration;

    fn manager_with_tool() -> Arc<McpManager> {
        let config = McpServerConfig {
            description: String::new(),
            enabled: true,
            transport: McpTransportConfig::Local {
                command: vec!["unused".into()],
                env: HashMap::new(),
                timeout: 30_000,
            },
        };
        let server = Arc::new(McpServer::new("demo".into(), config));
        server.set_tools(vec![McpToolDef {
            server_name: "demo".into(),
            tool_name: "read".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            timeout: Duration::from_secs(30),
        }]);
        Arc::new(McpManager {
            servers: RwLock::new(HashMap::from([("demo".into(), server)])),
        })
    }

    fn permissions_for_mcp(decision: Decision) -> Arc<crate::permissions::Permissions> {
        let mut raw = RawPerms::default();
        let rules = RawRuleSet {
            allow: (decision == Decision::Allow)
                .then(|| "*".into())
                .into_iter()
                .collect(),
            ask: (decision == Decision::Ask)
                .then(|| "*".into())
                .into_iter()
                .collect(),
            deny: (decision == Decision::Deny)
                .then(|| "*".into())
                .into_iter()
                .collect(),
        };
        raw.default.patterns.insert("mcp".into(), rules);
        Arc::new(crate::permissions::Permissions::from_raw(
            &raw,
            &ToolDefaults::default(),
        ))
    }

    #[test]
    #[ignore = "hot reload refactor characterization"]
    fn dispatcher_observes_replaced_runtime_permissions() {
        let mut live_permissions = permissions_for_mcp(Decision::Deny);
        let dispatcher = McpDispatcher::new(manager_with_tool(), Arc::clone(&live_permissions));
        let mode = AgentMode::normal();
        assert!(!dispatcher.is_visible("demo_read", mode.clone()));

        live_permissions = permissions_for_mcp(Decision::Allow);
        assert_eq!(
            live_permissions.check_subcommand(mode.clone(), "mcp", "demo_read"),
            Decision::Allow
        );
        assert!(
            dispatcher.is_visible("demo_read", mode),
            "MCP permission evaluation must observe the replacement used by the live runtime"
        );
    }
}
