//! `smelt.mcp` bindings — config-time MCP server registration.
//!
//! `smelt.mcp.register(name, { command, args, env })` stores into
//! `LuaShared.mcp_configs`; the startup sequence reads it after
//! `init.lua` runs.

use mlua::prelude::*;
use std::sync::Arc;

use crate::lua::LuaShared;
use crate::mcp::McpServerConfig;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let tbl = lua.create_table()?;

    tbl.set(
        "register",
        lua.create_function({
            let shared = Arc::clone(shared);
            move |_, (name, cfg): (String, mlua::Table)| {
                let mut command: Vec<String> = {
                    let arr: Option<mlua::Table> = cfg.get("command")?;
                    match arr {
                        Some(t) => {
                            let mut out = Vec::new();
                            for i in 1..=t.raw_len() {
                                out.push(t.get(i)?);
                            }
                            out
                        }
                        None => Vec::new(),
                    }
                };
                if command.is_empty() {
                    if let Ok(cmd) = cfg.get::<String>("command") {
                        command.push(cmd);
                    }
                }
                let args: Vec<String> = {
                    let arr: Option<mlua::Table> = cfg.get("args")?;
                    match arr {
                        Some(t) => {
                            let mut out = Vec::new();
                            for i in 1..=t.raw_len() {
                                out.push(t.get(i)?);
                            }
                            out
                        }
                        None => Vec::new(),
                    }
                };
                command.extend(args);
                let env: std::collections::HashMap<String, String> = {
                    let t: Option<mlua::Table> = cfg.get("env")?;
                    match t {
                        Some(t) => {
                            let mut out = std::collections::HashMap::new();
                            for pair in t.pairs::<String, String>() {
                                let (k, v) = pair?;
                                out.insert(k, v);
                            }
                            out
                        }
                        None => std::collections::HashMap::new(),
                    }
                };
                let timeout: u64 = cfg.get("timeout").unwrap_or(30000);
                let enabled: bool = cfg.get("enabled").unwrap_or(true);
                let config = McpServerConfig::Local {
                    command,
                    env,
                    timeout,
                    enabled,
                };
                let mut configs = shared.mcp_configs.lock().unwrap_or_else(|e| e.into_inner());
                configs.insert(name, config);
                Ok(())
            }
        })?,
    )?;

    tbl.set(
        "list",
        lua.create_function({
            let shared = Arc::clone(shared);
            move |lua, ()| {
                let configs = shared.mcp_configs.lock().unwrap_or_else(|e| e.into_inner());
                let out = lua.create_table()?;
                for (i, (name, _)) in configs.iter().enumerate() {
                    out.set(i + 1, name.clone())?;
                }
                Ok(out)
            }
        })?,
    )?;

    smelt.set("mcp", tbl)?;
    Ok(())
}
