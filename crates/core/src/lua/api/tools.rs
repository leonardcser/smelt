//! `smelt.tools` - register/unregister plugin tools and resolve their results to the engine.

use super::{lua_table_to_args, lua_table_to_json};
use crate::lua::doc::Tier;
use crate::lua::hooks::composite_reg;
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use crate::lua::{LuaHandle, LuaShared, ToolHandles};
use lua_doc_derive::{LuaAlias, LuaOpts};
use mlua::prelude::*;
use std::sync::Arc;

/// Decision string accepted by `decide` callbacks and
/// `permission_defaults`. Matches `protocol::Decision::{Allow, Ask, Deny}`
/// - the engine's `Error(_)` variant is not exposed.
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

/// Coarse side-effect classification used by permission policy.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.tools.Effect")]
pub enum LuaToolEffect {
    Read,
    Write,
    Network,
    User,
    Process,
    Config,
    Other,
}

impl From<LuaToolEffect> for crate::permissions::ToolEffectKind {
    fn from(effect: LuaToolEffect) -> Self {
        match effect {
            LuaToolEffect::Read => crate::permissions::ToolEffectKind::Read,
            LuaToolEffect::Write => crate::permissions::ToolEffectKind::Write,
            LuaToolEffect::Network => crate::permissions::ToolEffectKind::Network,
            LuaToolEffect::User => crate::permissions::ToolEffectKind::User,
            LuaToolEffect::Process => crate::permissions::ToolEffectKind::Process,
            LuaToolEffect::Config => crate::permissions::ToolEffectKind::Config,
            LuaToolEffect::Other => crate::permissions::ToolEffectKind::Other,
        }
    }
}

/// Per-mode default decisions installed by `smelt.tools.register`.
/// Keys are mode names and values are `"allow"`, `"ask"`, or `"deny"`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.tools.PermissionDefaults")]
pub struct LuaToolPermissionDefaults {
    /// Per-mode decisions keyed by registered mode name.
    #[lua(rest)]
    pub modes: std::collections::HashMap<String, LuaDecision>,
}

/// Plugin tool definition passed to `smelt.tools.register`.
///
/// `execute` is required; the remaining hooks are optional and are
/// invoked at well-defined points during a tool turn - see the field
/// docs for each callback's contract.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.tools.ToolDef")]
pub struct LuaToolDef {
    /// Tool name; used as the engine-facing identifier.
    pub name: String,
    /// Required handler: `execute(args, ctx)` - returns the tool result.
    pub execute: mlua::Function,
    /// Human-readable description shown to the model.
    #[lua(default)]
    pub description: String,
    /// JSON-schema parameters table passed through to the model.
    pub parameters: Option<mlua::Table>,
    /// Per-mode default decisions.
    pub permission_defaults: Option<LuaToolPermissionDefaults>,
    /// Coarse side-effect classification used by permission policy.
    pub effect: Option<LuaToolEffect>,
    /// Subcommand patterns that auto-allow without prompting.
    #[lua(default)]
    pub default_allow: Vec<String>,
    /// Built-in subpattern parser kind (e.g. `"bash"`).
    pub subpattern_parser: Option<String>,
    /// Agent modes the tool is available in; nil means all modes.
    pub modes: Option<mlua::Table>,
    /// `"concurrent"` (default) or `"sequential"`.
    pub execution_mode: Option<String>,
    /// `summary(args) -> string | styled_lines | nil` - styled label
    /// rendered in the transcript header AND confirm dialog body header.
    /// Plain string is auto-wrapped as one plain span; the styled-lines
    /// form is `{ { { text, syntax?, selectable?, title_suffix?, style? }, ... }, ... }` - same span
    /// shape as `buf:styled` plus optional `selectable = false` for chrome text and
    /// `title_suffix = true` for metadata rendered after the live tool timer.
    pub summary: Option<mlua::Function>,
    /// `approval_patterns(args, ctx) -> string[]` - patterns offered as one-click approvals.
    pub approval_patterns: Option<mlua::Function>,
    /// `preflight(args, ctx) -> table?` - validation hook; nil result skips.
    pub preflight: Option<mlua::Function>,
    /// `paths_for_workspace(args) -> (string|{ path: string, kind?: "file"|"directory"|"unknown" })[]` - paths this invocation will touch.
    pub paths_for_workspace: Option<mlua::Function>,
    /// `preview(args) -> smelt.layout` - pre-execute preview render. The
    /// confirm dialog renders it directly into the preview pane.
    pub preview: Option<mlua::Function>,
    /// `preview_output(args) -> { content, is_error?, metadata? }|nil` - immutable
    /// pending transcript output derived from final streamed arguments before execution.
    pub preview_output: Option<mlua::Function>,
    /// `draft_preview(args, ctx, block, opts) -> smelt.layout|nil` - best-effort
    /// renderer for streamed partial arguments in the transcript.
    pub draft_preview: Option<mlua::Function>,
    /// Outer watchdog deadline for this tool's coroutine, in milliseconds.
    /// This is separate from any timeout the tool implements internally.
    pub watchdog_timeout_ms: Option<u64>,
    /// Maximum watchdog deadline accepted from tool arguments, in milliseconds.
    pub watchdog_max_timeout_ms: Option<u64>,
    /// Tool argument that controls the watchdog deadline. Defaults to `timeout_ms`.
    pub watchdog_timeout_arg: Option<String>,
    /// Multiplier that converts `watchdog_timeout_arg` values to milliseconds.
    /// Use `1000` for second-based arguments.
    pub watchdog_timeout_arg_scale_ms: Option<u64>,
    /// Extra time added when a tool argument sets the watchdog deadline, in milliseconds.
    pub watchdog_grace_ms: Option<u64>,
    /// Whether the tool is available when running headless. Defaults to true.
    /// Set to false for tools that require a UI surface (dialogs, menus,
    /// managed worktree creation, cwd switching).
    #[lua(default)]
    pub headless: Option<bool>,
    /// Replace a core tool of the same name (advanced).
    #[lua(rename = "override", default)]
    pub override_core: bool,
}

fn tool_parameters_json(lua: &Lua, params: &mlua::Table) -> serde_json::Value {
    let mut json = lua_table_to_json(lua, params);
    reorder_schema_properties(&mut json);
    json
}

fn reorder_schema_properties(value: &mut serde_json::Value) {
    let serde_json::Value::Object(obj) = value else {
        return;
    };

    for child in obj.values_mut() {
        reorder_schema_properties(child);
    }

    let required: Vec<String> = obj
        .get("required")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if required.is_empty() {
        return;
    }

    let Some(properties) = obj
        .get_mut("properties")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };

    let mut ordered = serde_json::Map::new();
    for name in required {
        if let Some(value) = properties.remove(&name) {
            ordered.insert(name, value);
        }
    }
    for (name, value) in std::mem::take(properties) {
        ordered.insert(name, value);
    }
    *properties = ordered;
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "tools",
        "Register, unregister, and resolve plugin tools for the engine.",
        Tier::Host,
    )?;
    {
        let s = shared.clone();
        m.fn_(
            "register",
            "Register a plugin tool. See [`smelt.tools.ToolDef`](types.md#smelttoolstooldef) for every supported field; only `name` and `execute` are required. Returns a `Reg` whose `:remove()` unregisters the tool.",
            &["def"],
            move |lua, def: LuaToolDef| -> LuaResult<LuaReg> {
                let name = def.name;
                let execute_handle = LuaHandle::from_func(lua, def.execute)?;

                if let Some(perms) = def.permission_defaults {
                    let mut defaults = s.tool_defaults.lock().unwrap_or_else(|e| e.into_inner());
                    let entry = defaults.tool_decisions.entry(name.clone()).or_default();
                    for (mode, decision) in perms.modes {
                        entry.modes.insert(mode, decision.into());
                    }
                }
                if let Some(effect) = def.effect {
                    let mut defaults = s.tool_defaults.lock().unwrap_or_else(|e| e.into_inner());
                    defaults.tool_effects.insert(name.clone(), effect.into());
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
                    LuaHandle::from_func(lua, f)
                };
                let approval_patterns_handle = def.approval_patterns.map(stash).transpose()?;
                let preflight_handle = def.preflight.map(stash).transpose()?;
                let paths_for_workspace_handle = def.paths_for_workspace.map(stash).transpose()?;
                let preview_handle = def.preview.map(stash).transpose()?;
                let preview_output_handle = def.preview_output.map(stash).transpose()?;
                let has_draft_preview = def.draft_preview.is_some();
                let execution_mode = match def.execution_mode.as_deref() {
                    Some("sequential") => protocol::ToolExecutionMode::Sequential,
                    _ => protocol::ToolExecutionMode::Concurrent,
                };

                let meta = lua.create_table()?;
                meta.set("description", def.description)?;
                if let Some(params) = def.parameters {
                    let params_json = tool_parameters_json(lua, &params);
                    if let Ok(json_str) = serde_json::to_string(&params_json) {
                        meta.set("parameters_json", json_str)?;
                    }
                }
                if let Some(modes) = def.modes {
                    meta.set("modes", modes)?;
                }
                if let Some(mode_str) = def.execution_mode {
                    meta.set("execution_mode", mode_str)?;
                }
                meta.set("hook_approval_patterns", approval_patterns_handle.is_some())?;
                meta.set("hook_preflight", preflight_handle.is_some())?;
                meta.set(
                    "hook_paths_for_workspace",
                    paths_for_workspace_handle.is_some(),
                )?;
                meta.set("hook_preview", preview_handle.is_some())?;
                meta.set("hook_preview_output", preview_output_handle.is_some())?;
                meta.set("hook_draft_preview", has_draft_preview)?;
                meta.set("headless", def.headless.unwrap_or(true))?;
                meta.set("override_core", def.override_core)?;
                if let Some(watchdog_timeout_ms) = def.watchdog_timeout_ms {
                    meta.set("watchdog_timeout_ms", watchdog_timeout_ms)?;
                }
                if let Some(watchdog_max_timeout_ms) = def.watchdog_max_timeout_ms {
                    meta.set("watchdog_max_timeout_ms", watchdog_max_timeout_ms)?;
                }
                if let Some(watchdog_timeout_arg) = def.watchdog_timeout_arg {
                    meta.set("watchdog_timeout_arg", watchdog_timeout_arg)?;
                }
                if let Some(watchdog_timeout_arg_scale_ms) = def.watchdog_timeout_arg_scale_ms {
                    meta.set(
                        "watchdog_timeout_arg_scale_ms",
                        watchdog_timeout_arg_scale_ms,
                    )?;
                }
                if let Some(watchdog_grace_ms) = def.watchdog_grace_ms {
                    meta.set("watchdog_grace_ms", watchdog_grace_ms)?;
                }
                if let Some(summary) = def.summary {
                    meta.set("summary", summary)?;
                }
                lua.set_named_registry_value(&format!("__pt_meta_{name}"), meta)?;

                if let Ok(mut map) = s.tools.lock() {
                    map.insert(
                        name.clone(),
                        ToolHandles {
                            execute: execute_handle,
                            execution_mode,
                            approval_patterns: approval_patterns_handle,
                            preflight: preflight_handle,
                            paths_for_workspace: paths_for_workspace_handle,
                            preview: preview_handle,
                            preview_output: preview_output_handle,
                        },
                    );
                }
                let s_for_reg = s.clone();
                Ok(LuaReg::new(move || {
                    s_for_reg
                        .tools
                        .lock()
                        .map(|mut m| m.remove(&name).is_some())
                        .unwrap_or(false)
                }))
            },
        )?;
    }
    {
        m.fn_(
            "patch",
            "Patch metadata for an already-registered tool without replacing its handler. Supports `description` and `parameters`. Returns a `Reg` whose `:remove()` restores the previous metadata.",
            &["name", "patch"],
            move |lua, (name, patch): (String, mlua::Table)| -> LuaResult<LuaReg> {
                let key = format!("__pt_meta_{name}");
                let meta = lua.named_registry_value::<mlua::Table>(&key).map_err(|_| {
                    LuaError::RuntimeError(format!("tools.patch: no registered tool named `{name}`"))
                })?;

                let old_description: Option<String> = meta.get("description").ok();
                let old_parameters_json: Option<String> = meta
                    .get::<mlua::LuaString>("parameters_json")
                    .ok()
                    .map(|s| s.to_string_lossy().to_string());

                if let Ok(Some(description)) = patch.get::<Option<String>>("description") {
                    meta.set("description", description)?;
                }
                if let Ok(Some(parameters)) = patch.get::<Option<mlua::Table>>("parameters") {
                    if let Ok(json_str) = serde_json::to_string(&lua_table_to_json(lua, &parameters)) {
                        meta.set("parameters_json", json_str)?;
                    }
                }

                let lua = lua.weak();
                Ok(LuaReg::new(move || {
                    let Some(lua) = lua.try_upgrade() else {
                        return false;
                    };
                    let Ok(meta) = lua.named_registry_value::<mlua::Table>(&key) else {
                        return false;
                    };
                    match old_description {
                        Some(description) => {
                            let _ = meta.set("description", description);
                        }
                        None => {
                            let _ = meta.set("description", mlua::Value::Nil);
                        }
                    }
                    match old_parameters_json {
                        Some(parameters_json) => {
                            let _ = meta.set("parameters_json", parameters_json);
                        }
                        None => {
                            let _ = meta.set("parameters_json", mlua::Value::Nil);
                        }
                    }
                    true
                }))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "unregister",
            "Unregister a previously-registered tool by `name`. Returns `true` if a tool was removed, `false` otherwise.",
            &["name"],
            move |_, name: String| -> LuaResult<bool> {
                Ok(s.tools
                    .lock()
                    .map(|mut m| m.remove(&name).is_some())
                    .unwrap_or(false))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "list",
            "Return the names of every registered plugin tool, sorted.",
            &[],
            move |lua, ()| -> LuaResult<mlua::Table> {
                let mut names: Vec<String> = s
                    .tools
                    .lock()
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                names.sort();
                let table = lua.create_table()?;
                for (i, name) in names.iter().enumerate() {
                    table.set(i + 1, name.as_str())?;
                }
                Ok(table)
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "middleware",
            "Register middleware for tool `name`. Pass `\"\"` (empty string) as `name` to match every tool. \
`mw` is a table of `{ before = fn?, after = fn? }`:\n\n\
- `before(args, ctx)` runs synchronously before the tool executes. Return a table to replace `args`; return `{ deny = true, reason = \"...\" }` to short-circuit with an error result. Any other return is no-op.\n\
- `after(args, ctx, result)` runs after the tool completes and may return `{ content, is_error }` to replace the result. NOTE: `after` currently only fires for tools that complete synchronously; yielding tools (most builtins) skip it until the task-runtime path is wired.\n\n\
Hooks fire in registration order; an earlier hook's replacement is visible to later hooks. Returns a `Reg` whose `:remove()` drops this middleware.",
            &["name", "mw"],
            move |lua, (name, mw): (String, mlua::Table)| -> LuaResult<LuaReg> {
                let before_fn: Option<mlua::Function> = mw.get("before").ok();
                let after_fn: Option<mlua::Function> = mw.get("after").ok();
                if before_fn.is_none() && after_fn.is_none() {
                    return Err(LuaError::RuntimeError(
                        "tools.middleware: at least one of `before` or `after` is required"
                            .to_string(),
                    ));
                }
                let mut parts = Vec::with_capacity(2);
                if let Some(f) = before_fn {
                    let id = s.hooks.tool_before.register(lua, f, name.clone())?;
                    parts.push((Arc::clone(&s.hooks.tool_before), id));
                }
                if let Some(f) = after_fn {
                    let id = s.hooks.tool_after.register(lua, f, name.clone())?;
                    parts.push((Arc::clone(&s.hooks.tool_after), id));
                }
                Ok(composite_reg(parts))
            },
        )?;
    }
    m.fn_(
        "resolve",
        "Resolve the pending tool call `call_id` from request `request_id` with `{ content, is_error, metadata? }`. Sends a `ToolResult` back to the engine.",
        &["request_id", "call_id", "result"],
        |lua, (request_id, call_id, result): (u64, String, mlua::Table)| -> LuaResult<()> {
            let content: String = result.get("content").unwrap_or_default();
            let is_error: bool = result.get("is_error").unwrap_or(false);
            let metadata = result
                .get::<mlua::Value>("metadata")
                .ok()
                .and_then(|v| crate::lua::lua_to_serde::<serde_json::Value>(lua, &v));
            crate::host::with_core(|core| {
                core.engine.send(protocol::UiCommand::ToolResult {
                    request_id,
                    call_id,
                    content,
                    is_error,
                    metadata,
                })
            });
            Ok(())
        },
    )?;
    let summary_context = Arc::clone(shared);
    m.fn_(
        "default_summary",
        "Best-effort one-liner summary of a tool call's arguments. Picks a sensible field from `args` in priority order: `questions` (returns `\"N question(s)\"`), `pattern` (optionally suffixed with ` in <display path>`), then the first non-empty `command | file_path | notebook_path | path | url | query | name | id`. Returns `\"\"` if nothing matches. Used as the default `summary` field on tools registered via `smelt.tools.register`.",
        &["args"],
        move |_, args: Option<mlua::Table>| -> LuaResult<String> {
            let Some(args) = args else { return Ok(String::new()) };
            if let Ok(Some(questions)) = args.get::<Option<mlua::Table>>("questions") {
                let n = questions.raw_len();
                if n > 0 {
                    let suffix = if n == 1 { "" } else { "s" };
                    return Ok(format!("{n} question{suffix}"));
                }
            }
            let display_path = |path: &str| {
                crate::path_display::display_path_from(
                    path,
                    &summary_context.evaluation_cwd(),
                    &summary_context.runtime_home(),
                )
            };
            if let Ok(Some(pattern)) = args.get::<Option<String>>("pattern") {
                if !pattern.is_empty() {
                    if let Ok(Some(path)) = args.get::<Option<String>>("path") {
                        if !path.is_empty() && path != "." {
                            return Ok(format!("{pattern} in {}", display_path(&path)));
                        }
                    }
                    return Ok(pattern);
                }
            }
            for key in ["command", "file_path", "notebook_path", "path", "url", "query", "name", "id"] {
                if let Ok(Some(value)) = args.get::<Option<String>>(key) {
                    if value.is_empty() {
                        continue;
                    }
                    if matches!(key, "file_path" | "notebook_path" | "path") {
                        return Ok(display_path(&value));
                    }
                    return Ok(value);
                }
            }
            Ok(String::new())
        },
    )?;
    m.private_fn(
        "__send_call",
        &["request_id", "parent_call_id", "tool_name", "args"],
        |lua,
         (request_id, parent_call_id, tool_name, args): (u64, String, String, mlua::Table)|
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
    Ok(())
}
