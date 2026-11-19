//! The facts of one tool call, and the permission check on them.
//!
//! Rust works out what a call needs: its mode, its access and the access each
//! declared path needs. The Lua policy `smelt.permissions.check(call)` only
//! decides on those facts. A call that touches a `deny` path is denied before
//! the policy runs.

use std::path::PathBuf;

use mlua::prelude::*;

use super::api::mode::LuaMode;
use super::api::permissions::{path_rules, resolve};
use super::shared::CallSandbox;
use super::LuaShared;
use crate::modes::Mode;
use crate::permissions::{Access, Intent, PathRules, Rule};

pub struct PermissionCheck {
    pub decision: protocol::Decision,
    pub access: Access,
    /// The path the prompt names, absolute: the first one that needs the
    /// most access. `None` when the tool declares no paths.
    pub target: Option<PermissionTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionTarget {
    pub path: PathBuf,
    pub intent: Intent,
    /// An `ask` rule protects the path.
    pub protected: bool,
    /// What "allow for this session" approves; `None` for a `deny` path.
    pub scope: Option<PathBuf>,
}

/// One path a call declares, with the access it needs.
struct CallPath {
    path: PathBuf,
    intent: Intent,
    access: Access,
}

/// What one tool call needs.
struct Call {
    mode: Mode,
    access: Access,
    paths: Vec<CallPath>,
    /// The first declared path a `deny` rule forbids.
    denied: Option<(PathBuf, Intent)>,
    rules: PathRules,
}

/// Work out what the call of `tool` with `args` needs in `mode`. `access`
/// is set when the caller already knows it: a native tool, or a pending
/// request checked again. Otherwise a Lua tool's `access` and `paths`
/// declarations give it, an MCP tool's read-only flag does, and anything
/// else is `full`.
fn call(
    lua: &Lua,
    shared: &LuaShared,
    tool: &str,
    args: &mlua::Table,
    mode: &protocol::AgentMode,
    access: Option<Access>,
) -> LuaResult<Call> {
    let mode = shared
        .modes
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(mode.as_str())
        .cloned()
        .ok_or_else(|| LuaError::RuntimeError(format!("unknown mode `{}`", mode.as_str())))?;
    let rules = path_rules(shared, mode.name.as_str())?;
    let meta = lua
        .named_registry_value::<mlua::Table>(&format!("__pt_meta_{tool}"))
        .ok();
    let Some(meta) = meta else {
        let access = access.unwrap_or_else(|| {
            crate::host::try_with_core(|core| core.mcp_tool_access(tool))
                .flatten()
                .unwrap_or(Access::Full)
        });
        return Ok(Call {
            mode,
            access,
            paths: Vec::new(),
            denied: None,
            rules,
        });
    };
    let declared: String = match meta.get::<mlua::Value>("access")? {
        mlua::Value::Function(declare) => declare.call((args.clone(), LuaMode(mode.clone())))?,
        value => String::from_lua(value, lua)?,
    };
    let declared = Access::parse(&declared).ok_or_else(|| {
        LuaError::RuntimeError(format!(
            "tool access must be read, write or full, got {declared}"
        ))
    })?;
    let declared_paths = match meta.get::<Option<mlua::Function>>("paths")? {
        Some(paths) => Some(
            paths
                .call::<Option<Vec<String>>>(args.clone())?
                .unwrap_or_default(),
        ),
        None => None,
    };
    let intent = match declared {
        Access::Read => Intent::Read,
        Access::Write | Access::Full => Intent::Write,
    };
    let mut paths = Vec::new();
    let mut denied = None;
    for path in declared_paths.iter().flatten() {
        let path = resolve(shared, path);
        match rules.need(&path, intent) {
            Some(access) => paths.push(CallPath {
                path,
                intent,
                access,
            }),
            None => {
                denied = Some((path, intent));
                break;
            }
        }
    }
    let access = access.unwrap_or_else(|| {
        let most = paths.iter().map(|path| path.access).max();
        match (declared, &declared_paths) {
            (Access::Full, _) => Access::Full,
            // A tool that may change anything it likes is `full`.
            (Access::Write, None) => Access::Full,
            (Access::Read, _) => most.unwrap_or(Access::Read),
            (Access::Write, Some(_)) => most.unwrap_or(Access::Write),
        }
    });
    Ok(Call {
        mode,
        access,
        paths,
        denied,
        rules,
    })
}

/// Decide the call of `tool` with `args` in `mode`: run the policy
/// `smelt.permissions.check` on what the call needs. See [`call`] for
/// `access`.
pub(super) fn check(
    lua: &Lua,
    shared: &LuaShared,
    tool: &str,
    args: mlua::Table,
    mode: &protocol::AgentMode,
    access: Option<Access>,
) -> LuaResult<PermissionCheck> {
    let call = call(lua, shared, tool, &args, mode, access)?;
    let target = |path: PathBuf, intent: Intent| PermissionTarget {
        protected: call.rules.rule(&path) == Some(Rule::Ask),
        scope: call.rules.scope(&path),
        intent,
        path,
    };
    if let Some((path, intent)) = call.denied.clone() {
        return Ok(PermissionCheck {
            decision: protocol::Decision::Deny,
            access: call.access,
            target: Some(target(path, intent)),
        });
    }
    let paths = lua.create_table()?;
    for path in &call.paths {
        let entry = lua.create_table()?;
        entry.set("path", path.path.to_string_lossy())?;
        entry.set(
            "intent",
            match path.intent {
                Intent::Read => "read",
                Intent::Write => "write",
            },
        )?;
        entry.set("access", path.access.as_str())?;
        paths.push(entry)?;
    }
    let table = lua.create_table()?;
    table.set("tool", tool)?;
    table.set("args", args)?;
    table.set("access", call.access.as_str())?;
    table.set("mode", LuaMode(call.mode.clone()))?;
    table.set("paths", paths)?;
    let smelt: mlua::Table = lua.globals().get("smelt")?;
    let policy: mlua::Function = smelt.get::<mlua::Table>("permissions")?.get("check")?;
    let decision = match policy.call::<String>(table)?.as_str() {
        "allow" => protocol::Decision::Allow,
        "ask" => protocol::Decision::Ask,
        "deny" => protocol::Decision::Deny,
        other => {
            return Err(LuaError::RuntimeError(format!(
                "smelt.permissions.check returned {other}, expected allow, ask or deny"
            )))
        }
    };
    // The prompt names the first path that needs the most.
    let named = call
        .paths
        .iter()
        .fold(None::<&CallPath>, |best, path| match best {
            Some(best) if best.access >= path.access => Some(best),
            _ => Some(path),
        });
    Ok(PermissionCheck {
        decision,
        access: call.access,
        target: named.map(|path| target(path.path.clone(), path.intent)),
    })
}

/// The sandbox the commands of the call of `tool` with `args` in `mode` run
/// in: the most the call may do without asking again, which is its own
/// access or its mode's, whichever is more.
pub(super) fn sandbox(
    lua: &Lua,
    shared: &LuaShared,
    tool: &str,
    args: &mlua::Table,
    mode: &protocol::AgentMode,
) -> LuaResult<CallSandbox> {
    let call = call(lua, shared, tool, args, mode, None)?;
    Ok(CallSandbox {
        access: call.access.max(call.mode.access),
        mode: call.mode.name,
    })
}
