//! `smelt.mcp` bindings — config-time MCP server registration.
//!
//! `smelt.mcp.register(name, { command, args, env })` stores into
//! `LuaShared.mcp_configs`; the startup sequence reads it after
//! `init.lua` runs.

use crate::lua::LuaShared;
use crate::mcp::McpServerConfig;
use mlua::prelude::*;
use std::sync::Arc;

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
                let env: std::collections::HashMap<String, String> = {
                    let t: Option<mlua::Table> = cfg.get("env")?;
                    match t {
                        Some(t) => {
                            let mut out = std::collections::HashMap::new();
                            for pair in t.pairs::<String, String>().flatten() {
                                out.insert(pair.0, pair.1);
                            }
                            out
                        }
                        None => std::collections::HashMap::new(),
                    }
                };
                // Merge args into command vec so `command` is the full argv.
                let mut full_cmd = command;
                full_cmd.extend(args);
                let config = McpServerConfig::Local {
                    command: full_cmd,
                    env,
                    timeout: 30,
                    enabled: true,
                };
                if let Ok(mut map) = shared.mcp_configs.lock() {
                    map.insert(name, config);
                }
                Ok(())
            }
        })?,
    )?;

    smelt.set("mcp", tbl)?;
    Ok(())
}
