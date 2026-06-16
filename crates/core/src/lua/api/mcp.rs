//! `smelt.mcp` - config-time MCP server registration. Unknown fields and types raise errors.

use crate::lua::doc::Tier;
use crate::lua::lua_type::{LuaType, LuaTypeTuple};
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use crate::mcp::{McpServerConfig, McpStatus, McpToolDef, McpTransportConfig};
use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use std::sync::Arc;

fn status_to_table(lua: &Lua, status: &McpStatus) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("kind", status.as_str())?;
    match status {
        McpStatus::Connected { since_ms } => {
            t.set("since_ms", *since_ms)?;
        }
        McpStatus::Error { message, at_ms } => {
            t.set("error", message.as_str())?;
            t.set("at_ms", *at_ms)?;
        }
        _ => {}
    }
    Ok(t)
}

fn config_to_table(lua: &Lua, config: &McpServerConfig) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("description", config.description.as_str())?;
    t.set("enabled", config.enabled)?;
    match &config.transport {
        McpTransportConfig::Local {
            command,
            env,
            timeout,
        } => {
            t.set("type", "local")?;
            let cmd_tbl = lua.create_table()?;
            for (i, arg) in command.iter().enumerate() {
                cmd_tbl.set(i + 1, arg.as_str())?;
            }
            t.set("command", cmd_tbl)?;
            let env_tbl = lua.create_table()?;
            for (k, v) in env {
                env_tbl.set(k.as_str(), v.as_str())?;
            }
            t.set("env", env_tbl)?;
            t.set("timeout", *timeout)?;
        }
    }
    Ok(t)
}

fn tool_to_table(lua: &Lua, def: &McpToolDef) -> LuaResult<mlua::Table> {
    let row = lua.create_table()?;
    row.set("server", def.server_name.as_str())?;
    row.set("name", def.tool_name.as_str())?;
    row.set("qualified_name", def.qualified_name())?;
    row.set("description", def.description.as_str())?;
    if let Ok(s) = serde_json::to_string(&def.input_schema) {
        row.set("schema", s)?;
    }
    Ok(row)
}

/// Wrapper that accepts either a single command string or a list of
/// argv strings. The first element is the executable; remaining
/// elements are prepended to `args`.
#[derive(Debug, Default)]
pub struct LuaCommand(pub Vec<String>);

impl FromLua for LuaCommand {
    fn from_lua(value: mlua::Value, _: &Lua) -> LuaResult<Self> {
        match value {
            mlua::Value::Nil => Ok(Self(Vec::new())),
            mlua::Value::String(s) => Ok(Self(vec![s.to_string_lossy().to_string()])),
            mlua::Value::Table(t) => {
                let mut out = Vec::new();
                for i in 1..=t.raw_len() {
                    out.push(t.get(i)?);
                }
                Ok(Self(out))
            }
            other => Err(mlua::Error::external(format!(
                "expected string or list, got {}",
                other.type_name()
            ))),
        }
    }
}

impl LuaType for LuaCommand {
    fn lua_type() -> String {
        "string|string[]".into()
    }
}

impl LuaTypeTuple for LuaCommand {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("arg1");
        format!("{}: {}", name, <Self as LuaType>::lua_type())
    }
}

/// MCP server config accepted by `smelt.mcp.register`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.mcp.Config")]
pub struct LuaMcpConfig {
    /// Server kind. Only `"local"` (the default) is supported.
    #[lua(rename = "type")]
    pub kind: Option<String>,
    /// Executable + leading argv. Either a string (`"my-server"`) or a list (`{"my-server", "--flag"}`).
    #[lua(default)]
    pub command: LuaCommand,
    /// Trailing arguments appended after `command`.
    #[lua(default)]
    pub args: Vec<String>,
    /// Human-readable description shown by `/mcp`.
    pub description: Option<String>,
    /// Extra environment variables to set on the child process.
    #[lua(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Request timeout in milliseconds. Defaults to `30000`.
    pub timeout: Option<u64>,
    /// Whether the server is enabled. Defaults to `true`.
    pub enabled: Option<bool>,
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "mcp",
        "Config-time MCP server registration. Unknown fields raise errors.",
        Tier::Host,
    )?;
    let shared_for_register = Arc::clone(shared);
    m.fn_(
        "register",
        "Declare an MCP server named `name`. See [`smelt.mcp.Config`](types.md#smeltmcpconfig). Returns a `Reg` whose `:remove()` drops the desired-state entry; the next `/reload` reconciles it away.",
        &["name", "cfg"],
        move |_, (name, cfg): (String, LuaMcpConfig)| -> LuaResult<crate::lua::reg::LuaReg> {
            let kind = cfg.kind.as_deref().unwrap_or("local");
            if kind != "local" {
                return Err(mlua::Error::external(format!(
                    "smelt.mcp.register: unknown type `{kind}`; only `local` is supported"
                )));
            }

            let mut full_cmd = cfg.command.0;
            full_cmd.extend(cfg.args);
            let config = McpServerConfig {
                description: cfg.description.unwrap_or_default(),
                enabled: cfg.enabled.unwrap_or(true),
                transport: McpTransportConfig::Local {
                    command: full_cmd,
                    env: cfg.env,
                    timeout: cfg.timeout.unwrap_or(30000),
                },
            };
            if let Ok(mut map) = shared_for_register.mcp_configs.lock() {
                map.insert(name.clone(), config);
            }
            let shared_for_reg = Arc::clone(&shared_for_register);
            Ok(crate::lua::reg::LuaReg::new(move || {
                shared_for_reg
                    .mcp_configs
                    .lock()
                    .map(|mut m| m.remove(&name).is_some())
                    .unwrap_or(false)
            }))
        },
    )?;

    m.fn_(
        "list",
        "Snapshot every declared MCP server. Each row is `{ name, description, server_info, config, status, tool_count, tools }` where `description` is the configured human summary, `server_info` is `{ name, version, instructions }?`, `status` is `{ kind = \"disabled\"|\"connecting\"|\"connected\"|\"error\", since_ms?, error?, at_ms? }`, and `tools` contains `{ server, name, qualified_name, description, schema }` rows. Lifecycle reads are sync - safe to call from a status renderer or keymap.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            let rows = crate::host::try_with_core(|core| -> LuaResult<Vec<mlua::Table>> {
                let Some(ref mgr) = core.mcp else {
                    return Ok(Vec::new());
                };
                let mut rows = Vec::new();
                for server in mgr.servers_snapshot() {
                    let row = lua.create_table()?;
                    row.set("name", server.name.as_str())?;
                    row.set("description", server.description())?;
                    if let Some(info) = server.info() {
                        let info_tbl = lua.create_table()?;
                        info_tbl.set("name", info.name)?;
                        info_tbl.set("version", info.version)?;
                        info_tbl.set("instructions", info.instructions)?;
                        row.set("server_info", info_tbl)?;
                    }
                    row.set("config", config_to_table(lua, &server.config)?)?;
                    row.set("status", status_to_table(lua, &server.status())?)?;
                    let mut tools = server.tools();
                    tools.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
                    row.set("tool_count", tools.len())?;
                    let tools_tbl = lua.create_table()?;
                    for (i, def) in tools.iter().enumerate() {
                        tools_tbl.set(i + 1, tool_to_table(lua, def)?)?;
                    }
                    row.set("tools", tools_tbl)?;
                    rows.push(row);
                }
                rows.sort_by(|a, b| {
                    let an: String = a.get("name").unwrap_or_default();
                    let bn: String = b.get("name").unwrap_or_default();
                    an.cmp(&bn)
                });
                Ok(rows)
            })
            .transpose()?
            .unwrap_or_default();
            for (i, row) in rows.into_iter().enumerate() {
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "tools",
        "Snapshot every discovered MCP tool. Each row is `{ server, name, qualified_name, description, schema }`. When `server` is provided, only that server's tools are returned; otherwise tools from every connected server.",
        &["server"],
        |lua, server: Option<String>| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            let rows = crate::host::try_with_core(|core| -> LuaResult<Vec<mlua::Table>> {
                let Some(ref mgr) = core.mcp else {
                    return Ok(Vec::new());
                };
                let mut rows = Vec::new();
                let defs = match &server {
                    Some(name) => mgr.server(name).map(|s| s.tools()).unwrap_or_default(),
                    None => mgr.tool_defs(),
                };
                for def in defs {
                    rows.push(tool_to_table(lua, &def)?);
                }
                Ok(rows)
            })
            .transpose()?
            .unwrap_or_default();
            for (i, row) in rows.into_iter().enumerate() {
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "status",
        "Return the lifecycle status for server `name`: `\"disabled\"`, `\"connecting\"`, `\"connected\"`, or `\"error\"`. Returns `nil` when no server with that name is declared.",
        &["name"],
        |_, name: String| -> LuaResult<Option<String>> {
            Ok(crate::host::try_with_core(|core| {
                core.mcp
                    .as_ref()
                    .and_then(|mgr| mgr.server(&name))
                    .map(|s| s.status().as_str().to_string())
            })
            .flatten())
        },
    )?;

    Ok(())
}
