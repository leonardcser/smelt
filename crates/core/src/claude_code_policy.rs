//! Apply smelt policy to Claude Code's native tools without dispatching them.

use protocol::{AgentMode, PermissionOverrides, ToolEvaluation, ToolMetadata};
use serde_json::Value;
use smelt_permissions::{Permissions, PermissionsHandle, ToolPath};
use std::collections::HashMap;

/// Native plan files are artifacts, not workspace edits.
pub fn grant_plan_artifacts(permissions: &PermissionsHandle, artifact_dir: &std::path::Path) {
    permissions.grant_plan_directory(&engine::claude_code_plan_directory(artifact_dir));
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
    let decision = permissions.evaluate_in_directory(cwd, overrides, mode, tool, &args, &paths);
    ToolEvaluation {
        decision,
        metadata: ToolMetadata {
            summary: crate::mcp::args_summary(&args),
            ..Default::default()
        },
    }
}
