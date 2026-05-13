//! `smelt.mcp` — config-time MCP server registration. Unknown fields and types raise errors.

use crate::lua::doc::register_fn;
use crate::lua::lua_type::{LuaType, LuaTypeTuple};
use crate::lua::LuaShared;
use crate::mcp::McpServerConfig;
use lua_doc_derive::{lua_module, LuaOpts};
use mlua::prelude::*;
use std::sync::Arc;

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

    smelt.set("mcp", tbl)?;
    Ok(())
}
