//! `smelt.tools` — register/unregister plugin tools and resolve their results to the engine.

use super::{lua_table_to_args, lua_table_to_json};
use crate::lua::doc::register_fn;
use crate::lua::{LuaHandle, LuaShared, ToolHandles};
use lua_doc_derive::{lua_module, LuaAlias, LuaOpts};
use mlua::prelude::*;
use std::sync::Arc;

/// Decision string accepted by `decide` callbacks and
/// `permission_defaults`. Matches `protocol::Decision::{Allow, Ask, Deny}`
/// — the engine's `Error(_)` variant is not exposed.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.tools.Decision")]
pub enum LuaDecision {
    Allow,
    Ask,
    Deny,
}

impl From<LuaDecision> for protocol::Decision {
    fn from(d: LuaDecision) -> Self {
        match d {
            LuaDecision::Allow => protocol::Decision::Allow,
            LuaDecision::Ask => protocol::Decision::Ask,
            LuaDecision::Deny => protocol::Decision::Deny,
        }
    }
}

/// Per-mode default decisions installed by `smelt.tools.register`.
/// Each missing field falls through to the host's generic rules.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.tools.PermissionDefaults")]
pub struct LuaToolPermissionDefaults {
    /// Decision applied in normal mode.
    pub normal: Option<LuaDecision>,
    /// Decision applied in plan mode.
    pub plan: Option<LuaDecision>,
    /// Decision applied in apply mode.
    pub apply: Option<LuaDecision>,
    /// Decision applied in yolo mode.
    pub yolo: Option<LuaDecision>,
}

/// Plugin tool definition passed to `smelt.tools.register`.
///
/// `execute` is required; the remaining hooks are optional and are
/// invoked at well-defined points during a tool turn — see the field
/// docs for each callback's contract.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.tools.ToolDef")]
pub struct LuaToolDef {
    /// Tool name; used as the engine-facing identifier.
    pub name: String,
    /// Required handler: `execute(args, ctx)` — returns the tool result.
    pub execute: mlua::Function,
    /// Human-readable description shown to the model.
    #[lua(default)]
    pub description: String,
    /// JSON-schema parameters table passed through to the model.
    pub parameters: Option<mlua::Table>,
    /// Per-mode default decisions.
    pub permission_defaults: Option<LuaToolPermissionDefaults>,
    /// Subcommand patterns that auto-allow without prompting.
    #[lua(default)]
    pub default_allow: Vec<String>,
    /// Built-in subpattern parser kind (e.g. `"bash"`).
    pub subpattern_parser: Option<String>,
    /// Agent modes the tool is available in; nil means all modes.
    pub modes: Option<mlua::Table>,
    /// `"concurrent"` (default) or `"sequential"`.
    pub execution_mode: Option<String>,
    /// `summary(args, result) -> string` — short label for the picker.
    pub summary: Option<mlua::Function>,
    /// `confirm_text(args, ctx) -> string` — prompt body shown in the approval modal.
    pub confirm_text: Option<mlua::Function>,
    /// `approval_patterns(args, ctx) -> string[]` — patterns offered as one-click approvals.
    pub approval_patterns: Option<mlua::Function>,
    /// `preflight(args, ctx) -> table?` — validation hook; nil result skips.
    pub preflight: Option<mlua::Function>,
    /// `render(buf, args, result)` — custom transcript render.
    pub render: Option<mlua::Function>,
    /// `paths_for_workspace(args) -> string[]` — files this invocation will touch.
    pub paths_for_workspace: Option<mlua::Function>,
    /// `preview(buf, args)` — pre-execute preview render.
    pub preview: Option<mlua::Function>,
    /// `decide(args, mode) -> smelt.tools.Decision?` — per-call decision; nil falls through to generic permissions.
    pub decide: Option<mlua::Function>,
    /// Replace a core tool of the same name (advanced).
    #[lua(rename = "override", default)]
    pub override_core: bool,
}

#[lua_module(
    name = "smelt.tools",
    doc = "Register, unregister, and resolve plugin tools for the engine."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let tools_tbl = lua.create_table()?;
    {
        let s = shared.clone();
        register_fn(
            &tools_tbl,
            "smelt.tools",
            "register",
            "Register a plugin tool. See [`smelt.tools.ToolDef`](types.md#smelttoolstooldef) for every supported field; only `name` and `execute` are required.",
            &["def"],
            lua,
            move |lua, def: LuaToolDef| -> LuaResult<()> {
                let name = def.name;
                let key = lua.create_registry_value(def.execute)?;

                if let Some(perms) = def.permission_defaults {
                    let mut defaults = s.tool_defaults.lock().unwrap_or_else(|e| e.into_inner());
                    let entry = defaults.tool_decisions.entry(name.clone()).or_default();
                    entry.normal = perms.normal.map(Into::into);
                    entry.plan = perms.plan.map(Into::into);
                    entry.apply = perms.apply.map(Into::into);
                    entry.yolo = perms.yolo.map(Into::into);
                }
                if !def.default_allow.is_empty() {
                    let mut defaults = s.tool_defaults.lock().unwrap_or_else(|e| e.into_inner());
                    defaults
                        .subcommand_allow
                        .insert(name.clone(), def.default_allow);
                }
                if let Some(kind) = def.subpattern_parser {
                    if let Some(parser) = crate::permissions::builtin_subpattern_parser(&kind) {
                        let mut defaults =
                            s.tool_defaults.lock().unwrap_or_else(|e| e.into_inner());
                        defaults.subpattern_parsers.insert(name.clone(), parser);
                    }
                }

                let stash = |f: mlua::Function| -> LuaResult<LuaHandle> {
                    Ok(LuaHandle {
                        key: lua.create_registry_value(f)?,
                    })
                };
                let confirm_text_handle = def.confirm_text.map(stash).transpose()?;
                let approval_patterns_handle = def.approval_patterns.map(stash).transpose()?;
                let preflight_handle = def.preflight.map(stash).transpose()?;
                let render_handle = def.render.map(stash).transpose()?;
                let paths_for_workspace_handle = def.paths_for_workspace.map(stash).transpose()?;
                let preview_handle = def.preview.map(stash).transpose()?;
                let decide_handle = def.decide.map(stash).transpose()?;

                let meta = lua.create_table()?;
                meta.set("description", def.description)?;
                if let Some(params) = def.parameters {
                    if let Ok(json_str) = serde_json::to_string(&lua_table_to_json(lua, &params)) {
                        meta.set("parameters_json", json_str)?;
                    }
                }
                if let Some(modes) = def.modes {
                    meta.set("modes", modes)?;
                }
                if let Some(mode_str) = def.execution_mode {
                    meta.set("execution_mode", mode_str)?;
                }
                meta.set("hook_confirm_text", confirm_text_handle.is_some())?;
                meta.set("hook_approval_patterns", approval_patterns_handle.is_some())?;
                meta.set("hook_preflight", preflight_handle.is_some())?;
                meta.set("hook_render", render_handle.is_some())?;
                meta.set(
                    "hook_paths_for_workspace",
                    paths_for_workspace_handle.is_some(),
                )?;
                meta.set("hook_preview", preview_handle.is_some())?;
                meta.set("hook_decide", decide_handle.is_some())?;
                meta.set("override_core", def.override_core)?;
                if let Some(summary) = def.summary {
                    meta.set("summary", summary)?;
                }
                lua.set_named_registry_value(&format!("__pt_meta_{name}"), meta)?;

                if let Ok(mut map) = s.tools.lock() {
                    map.insert(
                        name,
                        ToolHandles {
                            execute: LuaHandle { key },
                            confirm_text: confirm_text_handle,
                            approval_patterns: approval_patterns_handle,
                            preflight: preflight_handle,
                            render: render_handle,
                            paths_for_workspace: paths_for_workspace_handle,
                            preview: preview_handle,
                            decide: decide_handle,
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
            &tools_tbl,
            "smelt.tools",
            "unregister",
            "Unregister a previously-registered tool by `name`. No-op if no tool with that name is registered.",
            &["name"],
            lua,
            move |_, name: String| -> LuaResult<()> {
                if let Ok(mut map) = s.tools.lock() {
                    map.remove(&name);
                }
                Ok(())
            },
        )?;
    }
    register_fn(
        &tools_tbl,
        "smelt.tools",
        "resolve",
        "Resolve the pending tool call `call_id` from request `request_id` with `{ content, is_error }`. Sends a `ToolResult` back to the engine.",
        &["request_id", "call_id", "result"],
        lua,
        |_, (request_id, call_id, result): (u64, String, mlua::Table)| -> LuaResult<()> {
            let content: String = result.get("content").unwrap_or_default();
            let is_error: bool = result.get("is_error").unwrap_or(false);
            crate::host::with_core(|core| {
                core.engine.send(protocol::UiCommand::ToolResult {
                    request_id,
                    call_id,
                    content,
                    is_error,
                })
            });
            Ok(())
        },
    )?;
    register_fn(
        &tools_tbl,
        "smelt.tools",
        "__send_call",
        "Internal: forward a tool call invocation to the engine. Used by Lua wrappers to delegate to a core tool.",
        &["request_id", "parent_call_id", "tool_name", "args"],
        lua,
        |lua,
         (request_id, parent_call_id, tool_name, args): (
            u64,
            String,
            String,
            mlua::Table,
        )|
         -> LuaResult<()> {
            let arg_map = lua_table_to_args(lua, &args);
            crate::host::with_core(|core| {
                core.engine.send(protocol::UiCommand::CallCoreTool {
                    request_id,
                    parent_call_id,
                    tool_name,
                    args: arg_map,
                })
            });
            Ok(())
        },
    )?;
    smelt.set("tools", tools_tbl)?;
    Ok(())
}
