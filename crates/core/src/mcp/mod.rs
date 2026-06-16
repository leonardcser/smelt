pub mod dispatcher;

use engine::log;
use rmcp::model::{CallToolRequestParams, ServerInfo};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::RwLock;

/// Configuration for a single MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpServerConfig {
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub transport: McpTransportConfig,
}

/// Transport-specific MCP server configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type")]
pub enum McpTransportConfig {
    #[serde(rename = "local")]
    Local {
        command: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default = "default_timeout")]
        timeout: u64,
    },
}

fn default_timeout() -> u64 {
    30000
}

fn default_true() -> bool {
    true
}

/// Server metadata returned by the MCP initialization handshake.
#[derive(Debug, Clone, Default)]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
    pub instructions: String,
}

impl From<&ServerInfo> for McpServerInfo {
    fn from(info: &ServerInfo) -> Self {
        Self {
            name: info.server_info.name.to_string(),
            version: info.server_info.version.to_string(),
            instructions: info.instructions.clone().unwrap_or_default(),
        }
    }
}

/// A discovered MCP tool definition (before wrapping as a Tool trait object).
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub timeout: Duration,
}

impl McpToolDef {
    pub fn qualified_name(&self) -> String {
        sanitize_name(&format!("{}_{}", self.server_name, self.tool_name))
    }
}

pub fn args_summary(args: &HashMap<String, serde_json::Value>) -> protocol::StyledLines {
    if args.is_empty() {
        return protocol::StyledLines::empty();
    }
    let sorted: BTreeMap<&String, &serde_json::Value> = args.iter().collect();
    let text = serde_json::to_string(&sorted).unwrap_or_default();
    if text.is_empty() || text == "{}" {
        protocol::StyledLines::empty()
    } else {
        protocol::StyledLines(vec![vec![protocol::StyledSpan {
            text,
            syntax: Some("json".into()),
            ..Default::default()
        }]])
    }
}

/// Per-server lifecycle state. Updated by `McpServer::connect`; read
/// synchronously by Lua introspection. Carries Unix-epoch timestamps so
/// callers can render "since 12s ago" without holding a clock.
#[derive(Debug, Clone)]
pub enum McpStatus {
    /// `enabled = false` in config; the connector never ran.
    Disabled,
    /// `connect_server` is in flight (between spawn and handshake).
    Connecting,
    /// Handshake + initial tool listing succeeded.
    Connected { since_ms: u64 },
    /// Spawn / handshake / list_tools failed.
    Error { message: String, at_ms: u64 },
}

impl McpStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Connecting => "connecting",
            Self::Connected { .. } => "connected",
            Self::Error { .. } => "error",
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Per-server state. Splits sync-readable status/tools (`std::RwLock`)
/// from the async-only client handle (`tokio::RwLock`). Lua bindings
/// hit the sync locks without entering the tokio runtime; `call_tool`
/// uses the async lock once per invocation.
pub struct McpServer {
    pub name: String,
    pub config: McpServerConfig,
    status: StdRwLock<McpStatus>,
    info: StdRwLock<Option<McpServerInfo>>,
    tools: StdRwLock<Vec<McpToolDef>>,
    client: RwLock<Option<RunningService<rmcp::RoleClient, ()>>>,
}

impl McpServer {
    fn new(name: String, config: McpServerConfig) -> Self {
        let initial = if config.enabled {
            McpStatus::Connecting
        } else {
            McpStatus::Disabled
        };
        Self {
            name,
            config,
            status: StdRwLock::new(initial),
            info: StdRwLock::new(None),
            tools: StdRwLock::new(Vec::new()),
            client: RwLock::new(None),
        }
    }

    pub fn status(&self) -> McpStatus {
        self.status
            .read()
            .map(|s| s.clone())
            .unwrap_or(McpStatus::Disabled)
    }

    pub fn tools(&self) -> Vec<McpToolDef> {
        self.tools.read().map(|t| t.clone()).unwrap_or_default()
    }

    pub fn info(&self) -> Option<McpServerInfo> {
        self.info.read().map(|i| i.clone()).unwrap_or_default()
    }

    pub fn description(&self) -> String {
        self.config.description.clone()
    }

    fn set_status(&self, status: McpStatus) {
        if let Ok(mut s) = self.status.write() {
            *s = status;
        }
    }

    fn set_tools(&self, tools: Vec<McpToolDef>) {
        if let Ok(mut t) = self.tools.write() {
            *t = tools;
        }
    }

    fn set_info(&self, info: Option<McpServerInfo>) {
        if let Ok(mut i) = self.info.write() {
            *i = info;
        }
    }

    async fn record_failure(&self, msg: String) {
        log::entry(
            log::Level::Warn,
            "mcp_error",
            &serde_json::json!({"server": &self.name, "error": &msg}),
        );
        self.set_status(McpStatus::Error {
            message: msg,
            at_ms: now_ms(),
        });
    }

    async fn connect(&self) {
        if !self.config.enabled {
            return;
        }
        let McpTransportConfig::Local {
            command,
            env,
            timeout,
        } = &self.config.transport;
        if command.is_empty() {
            return self.record_failure("missing command".into()).await;
        }

        let timeout_dur = Duration::from_millis(*timeout);

        log::entry(
            log::Level::Info,
            "mcp_connecting",
            &serde_json::json!({"server": &self.name, "command": command}),
        );

        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..]);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let transport = match TokioChildProcess::new(cmd) {
            Ok(t) => t,
            Err(e) => return self.record_failure(format!("failed to spawn: {e}")).await,
        };

        let client = match tokio::time::timeout(timeout_dur, ().serve(transport)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return self.record_failure(format!("handshake failed: {e}")).await,
            Err(_) => return self.record_failure("connection timed out".into()).await,
        };
        self.set_info(client.peer_info().map(McpServerInfo::from));

        let mcp_tools = match tokio::time::timeout(timeout_dur, client.list_all_tools()).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return self.record_failure(format!("list_tools failed: {e}")).await,
            Err(_) => return self.record_failure("list_tools timed out".into()).await,
        };

        let tool_defs: Vec<McpToolDef> = mcp_tools
            .into_iter()
            .map(|t| {
                let tool_name = t.name.to_string();
                let input_schema = t.schema_as_json_value();
                let description = t.description.unwrap_or_default().to_string();
                McpToolDef {
                    server_name: self.name.clone(),
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
                "server": &self.name,
                "tools": tool_defs.iter().map(|t| t.qualified_name()).collect::<Vec<_>>(),
            }),
        );

        self.set_tools(tool_defs);
        *self.client.write().await = Some(client);
        self.set_status(McpStatus::Connected { since_ms: now_ms() });
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<String, String> {
        let peer = {
            let guard = self.client.read().await;
            guard
                .as_ref()
                .map(|svc| svc.peer().clone())
                .ok_or_else(|| format!("MCP server '{}' not connected", self.name))?
        };

        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Some(obj) = args.as_object() {
            params = params.with_arguments(obj.clone());
        }

        let result = tokio::time::timeout(timeout, peer.call_tool(params))
            .await
            .map_err(|_| "MCP tool call timed out".to_string())?
            .map_err(|e| format!("MCP tool call failed: {e}"))?;

        let output = format_call_tool_content(result.content);

        if result.is_error.unwrap_or(false) {
            Err(output)
        } else {
            Ok(output)
        }
    }
}

/// Owns the set of `McpServer`s. Held by both `Core` (for Lua
/// introspection) and `McpDispatcher` (for tool dispatch) via `Arc`.
/// The server map sits behind a `std::sync::RwLock` so Lua callers can
/// snapshot it without entering the tokio runtime; reconcile mutates
/// the map under the same lock and only holds it for the swap, not
/// across `connect()` awaits.
pub struct McpManager {
    servers: StdRwLock<HashMap<String, Arc<McpServer>>>,
}

impl McpManager {
    /// Connect to every configured server concurrently. Returns once
    /// every connector future has resolved (success or failure); status
    /// is queryable via [`McpServer::status`] afterwards.
    pub async fn start(configs: &HashMap<String, McpServerConfig>) -> Arc<Self> {
        let manager = Arc::new(Self {
            servers: StdRwLock::new(HashMap::new()),
        });
        manager.reconcile(configs.clone()).await;
        manager
    }

    /// Snapshot of all servers in arbitrary order. Acquires the read
    /// lock briefly; the returned `Vec` is independent of the live map
    /// so callers can iterate without holding the lock.
    pub fn servers_snapshot(&self) -> Vec<Arc<McpServer>> {
        match self.servers.read() {
            Ok(guard) => guard.values().cloned().collect(),
            Err(poisoned) => poisoned.get_ref().values().cloned().collect(),
        }
    }

    /// Look up one server by registered name. Returns `None` when no
    /// server is registered under that name.
    pub fn server(&self, name: &str) -> Option<Arc<McpServer>> {
        let guard = self
            .servers
            .read()
            .map_err(|e| e.into_inner())
            .unwrap_or_else(|e| e);
        guard.get(name).cloned()
    }

    /// Diff the live server map against a freshly computed desired set
    /// and reconcile in place. New servers spawn connectors; removed
    /// servers are dropped (their `RunningService` shuts down with the
    /// `McpServer`); servers whose config changed are stopped and
    /// replaced. Runs the connectors concurrently and awaits all of
    /// them so callers know reconcile is complete when this returns.
    pub async fn reconcile(self: &Arc<Self>, desired: HashMap<String, McpServerConfig>) {
        let new_servers: Vec<Arc<McpServer>> = {
            let mut guard = match self.servers.write() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.retain(|name, server| match desired.get(name) {
                Some(cfg) => &server.config == cfg,
                None => false,
            });
            let mut spawned = Vec::new();
            for (name, cfg) in desired.into_iter() {
                if guard.contains_key(&name) {
                    continue;
                }
                let server = Arc::new(McpServer::new(name.clone(), cfg));
                guard.insert(name, Arc::clone(&server));
                spawned.push(server);
            }
            spawned
        };

        let mut handles = Vec::new();
        for server in new_servers {
            handles.push(tokio::spawn(async move { server.connect().await }));
        }
        for h in handles {
            let _ = h.await;
        }
    }

    /// Snapshot of every discovered tool across every connected server.
    /// Sorted by `(server_name, tool_name)` so callers see a stable order
    /// despite the underlying `HashMap`.
    pub fn tool_defs(&self) -> Vec<McpToolDef> {
        let mut defs: Vec<McpToolDef> = self
            .servers_snapshot()
            .iter()
            .flat_map(|s| s.tools())
            .collect();
        defs.sort_by(|a, b| {
            (a.server_name.as_str(), a.tool_name.as_str())
                .cmp(&(b.server_name.as_str(), b.tool_name.as_str()))
        });
        defs
    }

    /// Dispatch a tool call to the appropriate server.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<String, String> {
        let server = self
            .server(server_name)
            .ok_or_else(|| format!("MCP server '{}' not connected", server_name))?;
        server.call_tool(tool_name, args, timeout).await
    }
}

/// Flatten an MCP `call_tool` content list into a single text payload.
/// Text and text-resource parts are kept verbatim; image and blob parts
/// become placeholder labels; unknown variants are dropped. Empty parts
/// are skipped so the joined output never contains blank lines.
pub(crate) fn format_call_tool_content(content: Vec<rmcp::model::Content>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in content {
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
    parts.join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{Content, ResourceContents};
    use serde_json::json;

    // ── sanitize_name ────────────────────────────────────────────────

    #[test]
    fn sanitize_name_keeps_alphanumerics_and_underscores() {
        assert_eq!(sanitize_name("foo_bar123"), "foo_bar123");
    }

    #[test]
    fn sanitize_name_replaces_punctuation_with_underscore() {
        assert_eq!(sanitize_name("foo-bar.baz"), "foo_bar_baz");
        assert_eq!(sanitize_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_name("hello world"), "hello_world");
    }

    #[test]
    fn sanitize_name_handles_empty_string() {
        assert_eq!(sanitize_name(""), "");
    }

    #[test]
    fn sanitize_name_preserves_unicode_alphanumerics() {
        assert_eq!(sanitize_name("café_λ"), "café_λ");
    }

    // ── McpToolDef::qualified_name ───────────────────────────────────

    fn tool_def(server: &str, tool: &str) -> McpToolDef {
        McpToolDef {
            server_name: server.into(),
            tool_name: tool.into(),
            description: String::new(),
            input_schema: serde_json::Value::Null,
            timeout: Duration::from_millis(30000),
        }
    }

    #[test]
    fn qualified_name_joins_server_and_tool_with_underscore() {
        let def = tool_def("github", "create_issue");
        assert_eq!(def.qualified_name(), "github_create_issue");
    }

    #[test]
    fn qualified_name_sanitizes_invalid_characters() {
        let def = tool_def("my-server", "tool.name");
        assert_eq!(def.qualified_name(), "my_server_tool_name");
    }

    // ── default helpers ───────────────────────────────────────────────

    #[test]
    fn default_timeout_is_thirty_seconds() {
        assert_eq!(default_timeout(), 30000);
    }

    #[test]
    fn default_true_returns_true() {
        assert!(default_true());
    }

    // ── McpServerConfig deserialization ──────────────────────────────

    #[test]
    fn deserialize_local_config_with_all_fields() {
        let json = json!({
            "type": "local",
            "command": ["node", "server.js"],
            "description": "Node tools",
            "env": {"API_KEY": "secret"},
            "timeout": 5000,
            "enabled": false,
        });
        let config: McpServerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.description, "Node tools");
        assert!(!config.enabled);
        match config.transport {
            McpTransportConfig::Local {
                command,
                env,
                timeout,
            } => {
                assert_eq!(command, vec!["node", "server.js"]);
                assert_eq!(env.get("API_KEY").map(String::as_str), Some("secret"));
                assert_eq!(timeout, 5000);
            }
        }
    }

    #[test]
    fn deserialize_local_config_uses_defaults_when_omitted() {
        let json = json!({
            "type": "local",
            "command": ["mcp-server"],
        });
        let config: McpServerConfig = serde_json::from_value(json).unwrap();
        assert!(config.description.is_empty());
        assert!(config.enabled);
        match config.transport {
            McpTransportConfig::Local {
                command,
                env,
                timeout,
            } => {
                assert_eq!(command, vec!["mcp-server"]);
                assert!(env.is_empty());
                assert_eq!(timeout, 30000);
            }
        }
    }

    #[test]
    fn deserialize_unknown_type_fails() {
        let json = json!({"type": "remote", "url": "https://example.com"});
        assert!(serde_json::from_value::<McpServerConfig>(json).is_err());
    }

    // ── format_call_tool_content ─────────────────────────────────────

    #[test]
    fn format_empty_content_returns_empty_string() {
        assert_eq!(format_call_tool_content(vec![]), "");
    }

    #[test]
    fn format_text_content_passes_through_verbatim() {
        let items = vec![Content::text("hello world")];
        assert_eq!(format_call_tool_content(items), "hello world");
    }

    #[test]
    fn format_image_content_emits_mime_placeholder() {
        let items = vec![Content::image("base64data", "image/png")];
        assert_eq!(format_call_tool_content(items), "[image: image/png]");
    }

    #[test]
    fn format_text_resource_keeps_inner_text() {
        let items = vec![Content::resource(ResourceContents::text(
            "resource body",
            "file:///x",
        ))];
        assert_eq!(format_call_tool_content(items), "resource body");
    }

    #[test]
    fn format_blob_resource_emits_byte_count_placeholder() {
        let items = vec![Content::resource(ResourceContents::blob(
            "abcdefghij",
            "file:///x",
        ))];
        assert_eq!(format_call_tool_content(items), "[blob: 10 bytes]");
    }

    #[test]
    fn format_joins_parts_with_newline() {
        let items = vec![
            Content::text("first"),
            Content::text("second"),
            Content::image("data", "image/jpeg"),
        ];
        assert_eq!(
            format_call_tool_content(items),
            "first\nsecond\n[image: image/jpeg]"
        );
    }

    #[test]
    fn format_skips_empty_text_parts() {
        let items = vec![Content::text(""), Content::text("only"), Content::text("")];
        assert_eq!(format_call_tool_content(items), "only");
    }

    #[test]
    fn format_skips_empty_resource_text_parts() {
        let items = vec![
            Content::resource(ResourceContents::text("", "file:///empty")),
            Content::text("kept"),
        ];
        assert_eq!(format_call_tool_content(items), "kept");
    }
}
