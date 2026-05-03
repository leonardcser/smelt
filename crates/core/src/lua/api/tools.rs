//! `smelt.tools` bindings — register / unregister plugin tools and
//! resolve their results back to the engine. `__send_call` is the
//! private dispatch that the `_bootstrap.lua` wrapper around
//! `smelt.tools.call` mints request ids for and yields after.

use super::{lua_table_to_args, lua_table_to_json};
use crate::lua::{LuaHandle, LuaShared, ToolHandles};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let tools_tbl = lua.create_table()?;
    let s = shared.clone();
    let tools_register = lua.create_function(move |lua, def: mlua::Table| {
        let name: String = def.get("name")?;
        let handler: mlua::Function = def.get("execute")?;
        let key = lua.create_registry_value(handler)?;

        // Optional permission hooks. When present, the engine asks
        // the host to evaluate them before deciding Allow / Deny / Ask.
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
        let summary_fn = def.get::<mlua::Function>("summary").ok();

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
        // Hook flag bits — let `tool_defs` build
        // `ToolHookFlags` without reaching back into the
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
        if let Some(summary) = summary_fn {
            meta.set("summary", summary)?;
        }
        lua.set_named_registry_value(&format!("__pt_meta_{name}"), meta)?;

        if let Ok(mut map) = s.tools.lock() {
            map.insert(
                name,
                ToolHandles {
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
                if let Ok(mut map) = s.tools.lock() {
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
                crate::host::with_host(|host| {
                    host.engine().send(protocol::UiCommand::ToolResult {
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
                crate::host::with_host(|host| {
                    host.engine().send(protocol::UiCommand::CallCoreTool {
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
