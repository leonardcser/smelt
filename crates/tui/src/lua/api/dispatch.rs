//! Registration surfaces — every binding here writes to `LuaShared`
//! state that other Lua-side code reads back later: command handlers,
//! keymap chords, autocmd subscribers, deferred timers, plugin tools,
//! statusline sources, and spawned tasks.

use super::{lua_table_to_args, lua_table_to_json};
use crate::lua::{LuaHandle, LuaShared, PluginToolHandles, TaskCompletion, TaskEvent};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    smelt_keymap: &mlua::Table,
    shared: &Arc<LuaShared>,
) -> LuaResult<()> {
    super::cmd::register(lua, smelt, shared)?;
    super::keymap::register(lua, smelt_keymap, shared)?;
    register_task(lua, smelt, shared)?;
    register_tools(lua, smelt, shared)?;
    register_statusline(lua, smelt, shared)?;
    super::timer::register(lua, smelt)?;
    super::cell::register(lua, smelt)?;
    super::au::register(lua, smelt)?;
    register_spawn(lua, smelt, shared)?;
    Ok(())
}

fn register_task(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let task_tbl = lua.create_table()?;
    {
        let s = shared.clone();
        task_tbl.set(
            "alloc",
            lua.create_function(move |_, ()| {
                Ok(s.next_external_id.fetch_add(1, Ordering::Relaxed))
            })?,
        )?;
    }
    {
        let s = shared.clone();
        task_tbl.set(
            "resume",
            lua.create_function(move |lua, (id, value): (u64, mlua::Value)| {
                let key = lua.create_registry_value(value)?;
                if let Ok(mut inbox) = s.task_inbox.lock() {
                    inbox.push(TaskEvent::ExternalResolved {
                        external_id: id,
                        value: key,
                    });
                }
                Ok(())
            })?,
        )?;
    }
    smelt.set("task", task_tbl)?;
    Ok(())
}

fn register_tools(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let tools_tbl = lua.create_table()?;
    let s = shared.clone();
    let tools_register = lua.create_function(move |lua, def: mlua::Table| {
        let name: String = def.get("name")?;
        let handler: mlua::Function = def.get("execute")?;
        let key = lua.create_registry_value(handler)?;

        // Optional permission hooks. When present, the engine asks
        // the TUI to evaluate them before deciding Allow / Deny / Ask.
        let needs_confirm_handle = def
            .get::<mlua::Function>("needs_confirm")
            .ok()
            .map(|f| lua.create_registry_value(f).map(|key| LuaHandle { key }))
            .transpose()?;
        let approval_patterns_handle = def
            .get::<mlua::Function>("approval_patterns")
            .ok()
            .map(|f| lua.create_registry_value(f).map(|key| LuaHandle { key }))
            .transpose()?;
        let preflight_handle = def
            .get::<mlua::Function>("preflight")
            .ok()
            .map(|f| lua.create_registry_value(f).map(|key| LuaHandle { key }))
            .transpose()?;

        let meta = lua.create_table()?;
        let desc: String = def.get("description").unwrap_or_default();
        meta.set("description", desc)?;
        if let Ok(params) = def.get::<mlua::Table>("parameters") {
            if let Ok(json_str) = serde_json::to_string(&lua_table_to_json(lua, &params)) {
                meta.set("parameters_json", json_str)?;
            }
        }
        if let Ok(modes) = def.get::<mlua::Table>("modes") {
            meta.set("modes", modes)?;
        }
        if let Ok(mode_str) = def.get::<String>("execution_mode") {
            meta.set("execution_mode", mode_str)?;
        }
        // Hook flag bits — let `plugin_tool_defs` build
        // `PluginToolHookFlags` without reaching back into the
        // handles map.
        meta.set("hook_needs_confirm", needs_confirm_handle.is_some())?;
        meta.set("hook_approval_patterns", approval_patterns_handle.is_some())?;
        meta.set("hook_preflight", preflight_handle.is_some())?;
        // override_core: explicit signal that this plugin shadows a
        // core Rust tool of the same name. The engine drops the
        // colliding core definition from the LLM schema and routes
        // dispatch to the plugin.
        let override_core: bool = def.get::<bool>("override").unwrap_or(false);
        meta.set("override_core", override_core)?;
        lua.set_named_registry_value(&format!("__pt_meta_{name}"), meta)?;

        if let Ok(mut map) = s.plugin_tools.lock() {
            map.insert(
                name,
                PluginToolHandles {
                    execute: LuaHandle { key },
                    needs_confirm: needs_confirm_handle,
                    approval_patterns: approval_patterns_handle,
                    preflight: preflight_handle,
                },
            );
        }
        Ok(())
    })?;
    tools_tbl.set("register", tools_register)?;
    {
        let s = shared.clone();
        tools_tbl.set(
            "unregister",
            lua.create_function(move |_, name: String| {
                if let Ok(mut map) = s.plugin_tools.lock() {
                    map.remove(&name);
                }
                Ok(())
            })?,
        )?;
    }
    tools_tbl.set(
        "resolve",
        lua.create_function(
            |_, (request_id, call_id, result): (u64, String, mlua::Table)| {
                let content: String = result.get("content").unwrap_or_default();
                let is_error: bool = result.get("is_error").unwrap_or(false);
                crate::lua::with_host(|host| {
                    host.engine().send(protocol::UiCommand::PluginToolResult {
                        request_id,
                        call_id,
                        content,
                        is_error,
                    })
                });
                Ok(())
            },
        )?,
    )?;
    // Internal: dispatch a `smelt.tools.call` side request to the
    // engine. The Lua wrapper in `_bootstrap.lua` mints `request_id`
    // via `smelt.task.alloc` and yields after this returns.
    tools_tbl.set(
        "__send_call",
        lua.create_function(
            |lua,
             (request_id, parent_call_id, tool_name, args): (
                u64,
                String,
                String,
                mlua::Table,
            )| {
                let arg_map = lua_table_to_args(lua, &args);
                crate::lua::with_app(|app| {
                    app.core.engine.send(protocol::UiCommand::CallCoreTool {
                        request_id,
                        parent_call_id,
                        tool_name,
                        args: arg_map,
                    })
                });
                Ok(())
            },
        )?,
    )?;
    smelt.set("tools", tools_tbl)?;
    Ok(())
}

fn register_statusline(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let statusline_tbl = lua.create_table()?;
    {
        let s = shared.clone();
        statusline_tbl.set(
            "register",
            lua.create_function(
                move |lua, (name, handler, opts): (String, mlua::Function, Option<mlua::Table>)| {
                    let default_align_right = opts
                        .as_ref()
                        .and_then(|t| t.get::<Option<String>>("align").ok().flatten())
                        .map(|s| s == "right")
                        .unwrap_or(false);
                    let key = lua.create_registry_value(handler)?;
                    let source = crate::lua::StatusSource {
                        handle: LuaHandle { key },
                        default_align_right,
                    };
                    if let Ok(mut sources) = s.statusline_sources.lock() {
                        if let Some(existing) = sources.iter_mut().find(|(n, _)| n == &name) {
                            existing.1 = source;
                        } else {
                            sources.push((name, source));
                        }
                    }
                    Ok(())
                },
            )?,
        )?;
    }
    {
        let s = shared.clone();
        statusline_tbl.set(
            "unregister",
            lua.create_function(move |_, name: String| {
                if let Ok(mut sources) = s.statusline_sources.lock() {
                    sources.retain(|(n, _)| n != &name);
                }
                Ok(())
            })?,
        )?;
    }
    smelt.set("statusline", statusline_tbl)?;
    Ok(())
}

fn register_spawn(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let s = shared.clone();
    smelt.set(
        "spawn",
        lua.create_function(move |lua, handler: mlua::Function| {
            if let Ok(mut rt) = s.tasks.lock() {
                rt.spawn(
                    lua,
                    handler,
                    mlua::MultiValue::new(),
                    TaskCompletion::FireAndForget,
                )?;
            }
            Ok(())
        })?,
    )?;
    Ok(())
}
