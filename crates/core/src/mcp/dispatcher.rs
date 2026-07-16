use crate::mcp::{args_summary, McpManager, McpToolDef};
use crate::permissions::ToolOrigin;
use engine::tools::{ToolContext, ToolDispatcher, ToolFuture, ToolResult};
use protocol::{AgentMode, ToolEvaluation, ToolMetadata};
use serde_json::Value;
use smelt_provider::{FunctionSchema, ToolDefinition};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct McpDispatcher {
    manager: Arc<McpManager>,
    permissions: crate::permissions::PermissionsHandle,
    turn_permissions: Mutex<HashMap<u64, Arc<crate::permissions::Permissions>>>,
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
        permissions: crate::permissions::PermissionsHandle,
    ) -> Self {
        Self {
            manager,
            permissions,
            turn_permissions: Mutex::new(HashMap::new()),
        }
    }

    fn def_for(&self, name: &str) -> Option<McpToolDef> {
        self.manager
            .tool_defs()
            .into_iter()
            .find(|d| d.qualified_name() == name)
    }

    fn permissions_for_turn(&self, turn_id: u64) -> Arc<crate::permissions::Permissions> {
        self.turn_permissions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&turn_id)
            .cloned()
            .unwrap_or_else(|| self.permissions.snapshot())
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

    fn begin_turn(&self, turn_id: u64) {
        self.turn_permissions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(turn_id, self.permissions.snapshot());
    }

    fn end_turn(&self, turn_id: u64) {
        self.turn_permissions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&turn_id);
    }

    fn is_visible(&self, turn_id: u64, name: &str, mode: AgentMode) -> bool {
        self.def_for(name).is_some()
            && self
                .permissions_for_turn(turn_id)
                .check_subcommand(mode, "mcp", name)
                != protocol::Decision::Deny
    }

    fn evaluate_tool_call(
        &self,
        turn_id: u64,
        name: &str,
        args: &HashMap<String, Value>,
        mode: AgentMode,
        permission_overrides: Option<&protocol::PermissionOverrides>,
    ) -> Option<ToolEvaluation> {
        self.def_for(name)?;
        let summary = args_summary(args);
        let current = self.permissions_for_turn(turn_id);
        let permissions;
        let active_permissions = if let Some(overrides) = permission_overrides {
            permissions = current.with_overrides(overrides);
            &permissions
        } else {
            current.as_ref()
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
    use std::sync::{Mutex, RwLock};
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
            controller: Mutex::new(Default::default()),
            worker: tokio::sync::Mutex::new(()),
        })
    }

    fn permissions_for_mcp(decision: Decision) -> crate::permissions::Permissions {
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
        crate::permissions::Permissions::from_raw(&raw, &ToolDefaults::default())
    }

    #[test]
    fn dispatcher_observes_replaced_runtime_permissions() {
        let permissions =
            crate::permissions::PermissionsHandle::new(permissions_for_mcp(Decision::Deny));
        let dispatcher = McpDispatcher::new(manager_with_tool(), permissions.clone());
        let mode = AgentMode::normal();
        assert!(!dispatcher.is_visible(0, "demo_read", mode.clone()));

        let live_permissions = permissions.replace(permissions_for_mcp(Decision::Allow));
        assert_eq!(
            live_permissions.check_subcommand(mode.clone(), "mcp", "demo_read"),
            Decision::Allow
        );
        assert!(
            dispatcher.is_visible(0, "demo_read", mode),
            "MCP permission evaluation must observe the replacement used by the live runtime"
        );
    }

    #[test]
    fn dispatcher_pins_static_permissions_for_each_turn() {
        let permissions =
            crate::permissions::PermissionsHandle::new(permissions_for_mcp(Decision::Allow));
        let dispatcher = McpDispatcher::new(manager_with_tool(), permissions.clone());
        let mode = AgentMode::normal();
        dispatcher.begin_turn(7);

        permissions.replace(permissions_for_mcp(Decision::Deny));

        assert!(dispatcher.is_visible(7, "demo_read", mode.clone()));
        assert!(!dispatcher.is_visible(8, "demo_read", mode.clone()));
        dispatcher.end_turn(7);
        assert!(!dispatcher.is_visible(7, "demo_read", mode));
    }
}
