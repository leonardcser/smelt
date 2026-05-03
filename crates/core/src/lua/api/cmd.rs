//! `smelt.cmd` bindings — register / list slash commands. Plugin
//! authors call `smelt.cmd.register(name, fn, opts)` to add a new
//! `/name`; `list` enumerates the registry.
//!
//! The `run` binding is UiHost-tier and is added by the TUI after
//! `register_host_api` returns.

use crate::lua::{LuaHandle, LuaShared, RegisteredCommand};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let cmd_tbl = lua.create_table()?;
    {
        let s = shared.clone();
        cmd_tbl.set(
            "register",
            lua.create_function(
                move |lua, (name, handler, opts): (String, mlua::Function, Option<mlua::Table>)| {
                    let desc: Option<String> = opts
                        .as_ref()
                        .and_then(|t| t.get::<Option<String>>("desc").ok().flatten());
                    // `args` may be either a Lua array of strings (static) or
                    // omitted. Drives the secondary CommandArg picker that
                    // opens after `/name `.
                    let args: Vec<String> = opts
                        .as_ref()
                        .and_then(|t| t.get::<Option<mlua::Table>>("args").ok().flatten())
                        .map(|t| {
                            let mut v = Vec::new();
                            for pair in t.pairs::<mlua::Value, String>().flatten() {
                                v.push(pair.1);
                            }
                            v
                        })
                        .unwrap_or_default();
                    let while_busy: bool = opts
                        .as_ref()
                        .and_then(|t| t.get::<Option<bool>>("while_busy").ok().flatten())
                        .unwrap_or(true);
                    let queue_when_busy: bool = opts
                        .as_ref()
                        .and_then(|t| t.get::<Option<bool>>("queue_when_busy").ok().flatten())
                        .unwrap_or(false);
                    let startup_ok: bool = opts
                        .as_ref()
                        .and_then(|t| t.get::<Option<bool>>("startup_ok").ok().flatten())
                        .unwrap_or(false);
                    let key = lua.create_registry_value(handler)?;
                    if let Ok(mut map) = s.commands.lock() {
                        map.insert(
                            name,
                            RegisteredCommand {
                                handle: LuaHandle { key },
                                description: desc,
                                args,
                                while_busy,
                                queue_when_busy,
                                startup_ok,
                            },
                        );
                    }
                    Ok(())
                },
            )?,
        )?;
    }
    {
        let s = shared.clone();
        cmd_tbl.set(
            "list",
            lua.create_function(move |lua, ()| {
                let names: Vec<String> = s
                    .commands
                    .lock()
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                let table = lua.create_table()?;
                for (i, name) in names.iter().enumerate() {
                    table.set(i + 1, name.as_str())?;
                }
                Ok(table)
            })?,
        )?;
    }
    smelt.set("cmd", cmd_tbl)?;
    Ok(())
}
