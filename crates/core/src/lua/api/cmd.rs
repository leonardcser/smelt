//! `smelt.cmd` — register/list slash commands. `run` is added by the TUI after host-API init.

use crate::lua::doc::register_fn;
use crate::lua::lua_type::LuaCallback;
use crate::lua::{LuaHandle, LuaShared, RegisteredCommand};
use lua_doc_derive::{lua_module, LuaOpts};
use mlua::prelude::*;
use std::sync::Arc;

/// Options accepted by `smelt.cmd.register`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.cmd.RegisterOpts")]
pub struct LuaCmdRegisterOpts {
    /// Human-readable description shown in `/help` and the slash-command picker.
    pub desc: Option<String>,
    /// Positional argument labels used for help text and completion hints.
    #[lua(default)]
    pub args: Vec<String>,
    /// If true, the command can be invoked while an agent turn is running. Defaults to `true`.
    pub while_busy: Option<bool>,
    /// If true, busy invocations are queued instead of rejected. Defaults to `false`.
    pub queue_when_busy: Option<bool>,
    /// If true, the command may run before the runtime has finished bootstrapping. Defaults to `false`.
    pub startup_ok: Option<bool>,
    /// If true, the command is hidden from `/help` and the picker (still callable). Defaults to `false`.
    pub hidden: Option<bool>,
}

#[lua_module(
    name = "smelt.cmd",
    doc = "Register and list slash commands. `cmd.run` is injected by the TUI layer so it can access the live app state."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let cmd_tbl = lua.create_table()?;
    {
        let s = shared.clone();
        register_fn(
            &cmd_tbl,
            "smelt.cmd",
            "register",
            "Register a slash command `name` whose `handler` is invoked when the user runs it. `opts` accepts `desc`, `args`, `while_busy` (default `true`), `queue_when_busy` (default `false`), `startup_ok` (default `false`), and `hidden` (default `false`).",
            &["name", "handler", "opts"],
            lua,
            move |lua,
                  (name, handler, opts): (
                String,
                LuaCallback<Option<String>, ()>,
                Option<LuaCmdRegisterOpts>,
            )|
                  -> LuaResult<()> {
                let opts = opts.unwrap_or_default();
                let key = lua.create_registry_value(handler.into_inner())?;
                if let Ok(mut map) = s.commands.lock() {
                    map.insert(
                        name,
                        RegisteredCommand {
                            handle: LuaHandle { key },
                            description: opts.desc,
                            args: opts.args,
                            while_busy: opts.while_busy.unwrap_or(true),
                            queue_when_busy: opts.queue_when_busy.unwrap_or(false),
                            startup_ok: opts.startup_ok.unwrap_or(false),
                            hidden: opts.hidden.unwrap_or(false),
                        },
                    );
                }
                Ok(())
            },
        )?;
    }
    {
        let s = shared.clone();
        register_fn(
            &cmd_tbl,
            "smelt.cmd",
            "list",
            "Return every registered slash command as a Lua array of `{ name, desc, args, while_busy, queue_when_busy, startup_ok, hidden }` rows. Sorted by name.",
            &[],
            lua,
            move |lua, ()| -> LuaResult<mlua::Table> {
                let rows: Vec<(String, Option<String>, Vec<String>, bool, bool, bool, bool)> = s
                    .commands
                    .lock()
                    .map(|m| {
                        let mut rows: Vec<_> = m
                            .iter()
                            .map(|(name, cmd)| {
                                (
                                    name.clone(),
                                    cmd.description.clone(),
                                    cmd.args.clone(),
                                    cmd.while_busy,
                                    cmd.queue_when_busy,
                                    cmd.startup_ok,
                                    cmd.hidden,
                                )
                            })
                            .collect();
                        rows.sort_by(|a, b| a.0.cmp(&b.0));
                        rows
                    })
                    .unwrap_or_default();
                let table = lua.create_table()?;
                for (i, (name, desc, args, while_busy, queue_when_busy, startup_ok, hidden)) in
                    rows.into_iter().enumerate()
                {
                    let row = lua.create_table()?;
                    row.set("name", name)?;
                    if let Some(d) = desc {
                        row.set("desc", d)?;
                    }
                    let args_tbl = lua.create_table()?;
                    for (j, a) in args.iter().enumerate() {
                        args_tbl.set(j + 1, a.as_str())?;
                    }
                    row.set("args", args_tbl)?;
                    row.set("while_busy", while_busy)?;
                    row.set("queue_when_busy", queue_when_busy)?;
                    row.set("startup_ok", startup_ok)?;
                    row.set("hidden", hidden)?;
                    table.set(i + 1, row)?;
                }
                Ok(table)
            },
        )?;
    }
    {
        let s = shared.clone();
        register_fn(
            &cmd_tbl,
            "smelt.cmd",
            "unregister",
            "Drop the slash command `name` from the registry. Returns `true` if a command was removed, `false` if no command with that name existed.",
            &["name"],
            lua,
            move |_, name: String| -> LuaResult<bool> {
                Ok(s.commands
                    .lock()
                    .map(|mut m| m.remove(&name).is_some())
                    .unwrap_or(false))
            },
        )?;
    }
    smelt.set("cmd", cmd_tbl)?;
    Ok(())
}
