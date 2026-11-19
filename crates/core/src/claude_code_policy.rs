//! Apply smelt policy to Claude Code's native tools without dispatching them.

use crate::lua::LuaRuntime;
use crate::permissions::Access;
use protocol::{AgentMode, Decision, ToolEvaluation, ToolMetadata};
use serde_json::Value;
use std::collections::HashMap;

/// Decide a native tool call with `smelt.permissions.check`. A native tool
/// that shares a smelt tool's capability uses that tool's access and paths.
/// Native `Bash` runs outside the sandbox, so it and every other native tool
/// count as `full`. Path arguments arrive absolute. Call inside a host scope.
pub fn evaluate(
    lua: &LuaRuntime,
    mode: AgentMode,
    name: &str,
    args: &HashMap<String, Value>,
) -> ToolEvaluation {
    let (tool, args) = engine::claude_code_smelt_tool(name, args).unwrap_or((name, args.clone()));
    let access = (tool == "bash").then_some(Access::Full);
    let (decision, access) = match lua.check_permission(tool, &args, mode, access) {
        Ok(check) => (check.decision, check.access),
        Err(error) => (Decision::Error(error), Access::Full),
    };
    ToolEvaluation {
        decision,
        metadata: ToolMetadata {
            summary: crate::mcp::args_summary(&args),
            access,
            ..Default::default()
        },
    }
}
