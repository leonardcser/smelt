//! `smelt.cmd` bindings — register / list / run slash commands. Plugin
//! authors call `smelt.cmd.register(name, fn, opts)` to add a new
//! `/name`; `run` invokes by name; `list` enumerates the registry.

use crate::lua::{LuaHandle, LuaShared};
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
                    let key = lua.create_registry_value(handler)?;
                    if let Ok(mut map) = s.commands.lock() {
                        map.insert(
                            name,
                            crate::lua::RegisteredCommand {
                                handle: LuaHandle { key },
                                description: desc,
                                args,
                                while_busy,
                            },
                        );
                    }
                    Ok(())
                },
            )?,
        )?;
    }
    cmd_tbl.set(
        "run",
        lua.create_function(|_, line: String| {
            crate::lua::with_app(|app| app.apply_lua_command(&line));
            Ok(())
        })?,
    )?;
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
