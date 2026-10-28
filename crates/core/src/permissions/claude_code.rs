//! Apply smelt policy to Claude Code's native tools without dispatching them.

use super::{Permissions, ToolOrigin, ToolPath};
use protocol::{AgentMode, PermissionOverrides, ToolEvaluation, ToolMetadata};
use serde_json::Value;
use std::collections::HashMap;

/// Native plan files are artifacts, not workspace edits. Use the same narrow
/// mode/tool/access grants as smelt's saved-plan plugin.
pub fn grant_plan_artifacts(
    permissions: &super::PermissionsHandle,
    sessions_root: &std::path::Path,
    session_id: &str,
) {
    let Ok(directory) = engine::claude_code_plan_directory(sessions_root, session_id) else {
        return;
    };
    let approvals = permissions.approvals();
    let mut approvals = approvals.write().unwrap_or_else(|error| error.into_inner());
    for (tool, access) in [
        ("read_file", super::PathAccess::Read),
        ("write_file", super::PathAccess::Write),
        ("edit_file", super::PathAccess::Write),
    ] {
        approvals.add_session_path_grant(
            AgentMode::parse("plan").expect("built-in mode"),
            tool,
            access,
            directory.clone(),
        );
    }
}

pub fn evaluate(
    permissions: &Permissions,
    cwd: Option<&std::path::Path>,
    mode: AgentMode,
    name: &str,
    args: &HashMap<String, Value>,
    overrides: Option<&PermissionOverrides>,
) -> ToolEvaluation {
    let (tool, args) = engine::claude_code_smelt_tool(name, args).unwrap_or((name, args.clone()));
    let paths: Vec<ToolPath> = match tool {
        "read_file" | "write_file" | "edit_file" => args
            .get("file_path")
            .and_then(Value::as_str)
            .map(ToolPath::file)
            .into_iter()
            .collect(),
        "edit_notebook" => args
            .get("notebook_path")
            .and_then(Value::as_str)
            .map(ToolPath::file)
            .into_iter()
            .collect(),
        "glob" | "grep" => vec![ToolPath::directory(
            args.get("path").and_then(Value::as_str).unwrap_or("."),
        )],
        _ => Vec::new(),
    };
    if cwd.is_some_and(|cwd| !cwd.is_absolute()) {
        return ToolEvaluation {
            decision: protocol::Decision::Deny,
            metadata: Default::default(),
        };
    }
    let mut resolved;
    let policy = if overrides.is_some() || cwd.is_some_and(|cwd| cwd != permissions.active_root) {
        resolved = overrides
            .map(|overrides| permissions.with_overrides(overrides))
            .unwrap_or_else(|| permissions.clone());
        if let Some(cwd) = cwd {
            // Native shells may change cwd. Resolve paths there without adding
            // that directory to smelt's trusted workspace roots.
            resolved.active_root = cwd.into();
        }
        &resolved
    } else {
        permissions
    };
    let outcome =
        policy.evaluate_tool_with_paths_and_approvals(mode, ToolOrigin::Lua, tool, &args, &paths);
    ToolEvaluation {
        decision: outcome.decision,
        metadata: ToolMetadata {
            summary: crate::mcp::args_summary(&args),
            ..Default::default()
        },
    }
}
