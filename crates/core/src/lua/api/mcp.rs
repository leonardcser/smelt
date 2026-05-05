//! `smelt.mcp` bindings — config-time MCP server registration.
//!
//! `smelt.mcp.register(name, { type, command, args, env, timeout,
//! enabled })` stores into `LuaShared.mcp_configs`; the startup sequence
//! reads it after `init.lua` runs.
//!
//! `type` defaults to `"local"`. Unknown `type` values, and unknown
//! top-level keys, raise an error so a typo doesn't silently lose
//! config.

use crate::lua::LuaShared;
use crate::mcp::McpServerConfig;
use mlua::prelude::*;
use std::sync::Arc;

const KNOWN: &[&str] = &["type", "command", "args", "env", "timeout", "enabled"];

fn reject_unknown(t: &mlua::Table) -> LuaResult<()> {
    for pair in t.clone().pairs::<mlua::Value, mlua::Value>() {
        let (key, _) = pair?;
        if let mlua::Value::String(s) = key {
            let k = s.to_string_lossy();
            if !KNOWN.contains(&k.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "smelt.mcp.register: unknown field `{k}`; \
                     known fields are {KNOWN:?}"
                )));
            }
        }
    }
    Ok(())
}

fn read_string_array(cfg: &mlua::Table, key: &str) -> LuaResult<Vec<String>> {
    let arr: Option<mlua::Table> = cfg.get(key)?;
    match arr {
        Some(t) => {
            let mut out = Vec::new();
            for i in 1..=t.raw_len() {
                out.push(t.get(i)?);
            }
            Ok(out)
        }
        None => Ok(Vec::new()),
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let tbl = lua.create_table()?;

    tbl.set(
        "register",
        lua.create_function({
            let shared = Arc::clone(shared);
            move |_, (name, cfg): (String, mlua::Table)| {
                reject_unknown(&cfg)?;

                let kind = cfg
                    .get::<Option<String>>("type")?
                    .unwrap_or_else(|| "local".to_string());
                if kind != "local" {
                    return Err(mlua::Error::external(format!(
                        "smelt.mcp.register: unknown type `{kind}`; only `local` is supported"
                    )));
                }

                let mut command = read_string_array(&cfg, "command")?;
                if command.is_empty() {
                    if let Ok(cmd) = cfg.get::<String>("command") {
                        command.push(cmd);
                    }
                }
                let args = read_string_array(&cfg, "args")?;
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
                let timeout = cfg.get::<Option<u64>>("timeout")?.unwrap_or(30000);
                let enabled = cfg.get::<Option<bool>>("enabled")?.unwrap_or(true);

                let mut full_cmd = command;
                full_cmd.extend(args);
                let config = McpServerConfig::Local {
                    command: full_cmd,
                    env,
                    timeout,
                    enabled,
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
