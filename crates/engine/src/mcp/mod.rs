mod tool_adapter;

use crate::log;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::RwLock;

pub(crate) use tool_adapter::McpTool;

/// Configuration for a single MCP server.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum McpServerConfig {
    #[serde(rename = "local")]
    Local {
        command: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default = "default_timeout")]
        timeout: u64,
        #[serde(default = "default_true")]
        enabled: bool,
    },
}

fn default_timeout() -> u64 {
    30000
}

fn default_true() -> bool {
    true
}

/// A discovered MCP tool definition (before wrapping as a Tool trait object).
#[derive(Debug, Clone)]
pub(crate) struct McpToolDef {
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub timeout: Duration,
}

impl McpToolDef {
    fn qualified_name(&self) -> String {
        sanitize_name(&format!("{}_{}", self.server_name, self.tool_name))
    }
}

struct McpConnection {
    client: RunningService<rmcp::RoleClient, ()>,
    tools: Vec<McpToolDef>,
}

/// Manages all MCP server connections and their tools.
pub(crate) struct McpManager {
    connections: RwLock<HashMap<String, McpConnection>>,
}

impl McpManager {
    /// Connect to all configured MCP servers. Servers that fail to connect
    /// are logged and skipped — they don't block the agent from starting.
    pub(crate) async fn start(configs: &HashMap<String, McpServerConfig>) -> Arc<Self> {
        let manager = Arc::new(Self {
            connections: RwLock::new(HashMap::new()),
        });

        let mut handles = Vec::new();
        for (name, config) in configs {
            let name = name.clone();
            let config = config.clone();
            let mgr = Arc::clone(&manager);
            handles.push(tokio::spawn(async move {
                mgr.connect_server(&name, &config).await;
            }));
        }

        for h in handles {
            let _ = h.await;
        }

        manager
    }

    async fn connect_server(&self, name: &str, config: &McpServerConfig) {
        match config {
            McpServerConfig::Local {
                command,
                env,
                timeout,
                enabled,
            } => {
                if !enabled || command.is_empty() {
                    return;
                }

                let timeout_dur = Duration::from_millis(*timeout);

                log::entry(
                    log::Level::Info,
                    "mcp_connecting",
                    &serde_json::json!({"server": name, "command": command}),
                );

                let mut cmd = Command::new(&command[0]);
                cmd.args(&command[1..]);
                for (k, v) in env {
                    cmd.env(k, v);
                }

                let transport = match TokioChildProcess::new(cmd) {
                    Ok(t) => t,
                    Err(e) => {
                        return self
                            .record_failure(name, format!("failed to spawn: {e}"))
                            .await
                    }
                };

                let client = match tokio::time::timeout(timeout_dur, ().serve(transport)).await {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => {
                        return self
                            .record_failure(name, format!("handshake failed: {e}"))
                            .await
                    }
                    Err(_) => {
                        return self
                            .record_failure(name, "connection timed out".into())
                            .await
                    }
                };

                let mcp_tools =
                    match tokio::time::timeout(timeout_dur, client.list_all_tools()).await {
                        Ok(Ok(t)) => t,
                        Ok(Err(e)) => {
                            return self
                                .record_failure(name, format!("list_tools failed: {e}"))
                                .await
                        }
                        Err(_) => {
                            return self
                                .record_failure(name, "list_tools timed out".into())
                                .await
                        }
                    };

                let tool_defs: Vec<McpToolDef> = mcp_tools
                    .into_iter()
                    .map(|t| {
                        let tool_name = t.name.to_string();
                        let input_schema = t.schema_as_json_value();
                        let description = t.description.unwrap_or_default().to_string();
                        McpToolDef {
                            server_name: name.to_string(),
                            tool_name,
                            description,
                            input_schema,
                            timeout: timeout_dur,
                        }
                    })
                    .collect();

                log::entry(
                    log::Level::Info,
                    "mcp_connected",
                    &serde_json::json!({
                        "server": name,
                        "tools": tool_defs.iter().map(|t| t.qualified_name()).collect::<Vec<_>>(),
                    }),
                );

                self.connections.write().await.insert(
                    name.to_string(),
                    McpConnection {
                        client,
                        tools: tool_defs,
                    },
                );
            }
        }
    }

    async fn record_failure(&self, name: &str, msg: String) {
        log::entry(
            log::Level::Warn,
            "mcp_error",
            &serde_json::json!({"server": name, "error": &msg}),
        );
    }

    /// Get all discovered MCP tool definitions across all connected servers.
    pub(crate) async fn tool_defs(&self) -> Vec<McpToolDef> {
        let conns = self.connections.read().await;
        conns.values().flat_map(|c| c.tools.clone()).collect()
    }

    /// Call a tool on the appropriate MCP server.
    /// Acquires the connection lock briefly to clone the client handle,
    /// then releases it before the potentially slow remote call.
    pub(crate) async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<String, String> {
        let client = {
            let conns = self.connections.read().await;
            let conn = conns
                .get(server_name)
                .ok_or_else(|| format!("MCP server '{}' not connected", server_name))?;
            conn.client.clone()
        };

        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Some(obj) = args.as_object() {
            params = params.with_arguments(obj.clone());
        }

        let result = tokio::time::timeout(timeout, client.call_tool(params))
            .await
            .map_err(|_| "MCP tool call timed out".to_string())?
            .map_err(|e| format!("MCP tool call failed: {e}"))?;

        let mut parts: Vec<String> = Vec::new();
        for item in result.content {
            let part = match item.raw {
                rmcp::model::RawContent::Text(text) => text.text,
                rmcp::model::RawContent::Image(img) => {
                    format!("[image: {}]", img.mime_type)
                }
                rmcp::model::RawContent::Resource(res) => match res.resource {
                    rmcp::model::ResourceContents::TextResourceContents { text, .. } => text,
                    rmcp::model::ResourceContents::BlobResourceContents { blob, .. } => {
                        format!("[blob: {} bytes]", blob.len())
                    }
                },
                _ => continue,
            };
            if !part.is_empty() {
                parts.push(part);
            }
        }
        let output = parts.join("\n");

        if result.is_error.unwrap_or(false) {
            Err(output)
        } else {
            Ok(output)
        }
    }

}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
