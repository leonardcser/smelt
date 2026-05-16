//! `smelt.mcp` — config-time MCP server registration. Unknown fields and types raise errors.

use crate::lua::doc::register_fn;
use crate::lua::lua_type::{LuaType, LuaTypeTuple};
use crate::lua::LuaShared;
use crate::mcp::{McpServerConfig, McpStatus};
use lua_doc_derive::{lua_module, LuaOpts};
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
    match config {
        McpServerConfig::Local {
            command,
            env,
            timeout,
            enabled,
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
            t.set("enabled", *enabled)?;
        }
    }
    Ok(t)
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
    /// Extra environment variables to set on the child process.
    #[lua(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Request timeout in milliseconds. Defaults to `30000`.
    pub timeout: Option<u64>,
    /// Whether the server is enabled. Defaults to `true`.
    pub enabled: Option<bool>,
}

#[lua_module(
    name = "smelt.mcp",
    doc = "Config-time MCP server registration. Unknown fields raise errors."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let tbl = lua.create_table()?;
    let shared_for_register = Arc::clone(shared);
    register_fn(
        &tbl,
        "smelt.mcp",
        "register",
        "Declare an MCP server named `name`. See [`smelt.mcp.Config`](types.md#smeltmcpconfig).",
        &["name", "cfg"],
        lua,
        move |_, (name, cfg): (String, LuaMcpConfig)| -> LuaResult<()> {
            let kind = cfg.kind.as_deref().unwrap_or("local");
            if kind != "local" {
                return Err(mlua::Error::external(format!(
                    "smelt.mcp.register: unknown type `{kind}`; only `local` is supported"
                )));
            }

            let mut full_cmd = cfg.command.0;
            full_cmd.extend(cfg.args);
            let config = McpServerConfig::Local {
                command: full_cmd,
                env: cfg.env,
                timeout: cfg.timeout.unwrap_or(30000),
                enabled: cfg.enabled.unwrap_or(true),
            };
            if let Ok(mut map) = shared_for_register.mcp_configs.lock() {
                map.insert(name, config);
            }
            Ok(())
        },
    )?;

    register_fn(
        &tbl,
        "smelt.mcp",
        "list",
        "Snapshot every declared MCP server. Each row is `{ name, config, status, tool_count }` where `status` is `{ kind = \"disabled\"|\"connecting\"|\"connected\"|\"error\", since_ms?, error?, at_ms? }`. Lifecycle reads are sync — safe to call from a status renderer or keymap.",
        &[],
        lua,
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            let rows = crate::host::try_with_core(|core| -> LuaResult<Vec<mlua::Table>> {
                let Some(ref mgr) = core.mcp else {
                    return Ok(Vec::new());
                };
                let mut rows = Vec::new();
                for server in mgr.servers() {
                    let row = lua.create_table()?;
                    row.set("name", server.name.as_str())?;
                    row.set("config", config_to_table(lua, &server.config)?)?;
                    row.set("status", status_to_table(lua, &server.status())?)?;
                    row.set("tool_count", server.tools().len())?;
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
    register_fn(
        &tbl,
        "smelt.mcp",
        "tools",
        "Snapshot every discovered MCP tool. Each row is `{ server, name, qualified_name, description, schema }`. When `server` is provided, only that server's tools are returned; otherwise tools from every connected server.",
        &["server"],
        lua,
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
                    let row = lua.create_table()?;
                    row.set("server", def.server_name.as_str())?;
                    row.set("name", def.tool_name.as_str())?;
                    row.set("qualified_name", def.qualified_name())?;
                    row.set("description", def.description.as_str())?;
                    if let Ok(s) = serde_json::to_string(&def.input_schema) {
                        row.set("schema", s)?;
                    }
                    rows.push(row);
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
    register_fn(
        &tbl,
        "smelt.mcp",
        "status",
        "Return the lifecycle status for server `name`: `\"disabled\"`, `\"connecting\"`, `\"connected\"`, or `\"error\"`. Returns `nil` when no server with that name is declared.",
        &["name"],
        lua,
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

    smelt.set("mcp", tbl)?;
    Ok(())
}
