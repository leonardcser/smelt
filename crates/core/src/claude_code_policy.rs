//! Apply smelt policy to Claude Code's native tools without dispatching them.

use crate::lua::LuaRuntime;
use crate::permissions::Access;
use protocol::{AgentMode, Decision, ToolEvaluation, ToolMetadata};
use serde_json::Value;
use std::collections::HashMap;

/// Decide a native tool call with `smelt.permissions.check`. A native tool
/// checked as a smelt tool ([`engine::claude_code_checked_as`]) uses that
/// tool's rules, paths and summary. The access comes from
/// [`engine::claude_code_native_access`].
/// Path arguments arrive absolute. Call inside a host scope.
pub fn evaluate(
    lua: &LuaRuntime,
    mode: AgentMode,
    name: &str,
    args: &HashMap<String, Value>,
) -> ToolEvaluation {
    let access = engine::claude_code_native_access(name);
    let (tool, args, summary) = match engine::claude_code_checked_as(name, args) {
        Some((tool, args)) => {
            let summary = Some(lua.tool_summary(tool, &args))
                .filter(|summary| !summary.is_empty())
                .unwrap_or_else(|| crate::mcp::args_summary(&args));
            (tool, args, summary)
        }
        None => (name, args.clone(), crate::mcp::args_summary(args)),
    };
    let (decision, access) = match lua.check_permission(tool, &args, mode, access) {
        Ok(check) => (check.decision, check.access),
        Err(error) => (Decision::Error(error), Access::Full),
    };
    ToolEvaluation {
        decision,
        metadata: ToolMetadata {
            summary,
            access,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Decision::{Allow, Ask};

    fn decide(tool: &str, mode: &str) -> (Decision, Access) {
        let lua = LuaRuntime::new();
        let mode = AgentMode::parse(mode).unwrap();
        let evaluation = evaluate(&lua, mode, tool, &HashMap::new());
        (evaluation.decision, evaluation.metadata.access)
    }

    #[test]
    fn native_tools_without_a_smelt_tool_need_their_own_access() {
        assert_eq!(decide("ToolSearch", "read"), (Allow, Access::Read));
        assert_eq!(decide("KillShell", "read"), (Ask, Access::Write));
        assert_eq!(decide("KillShell", "write"), (Allow, Access::Write));
        assert_eq!(decide("Agent", "write"), (Ask, Access::Full));
        assert_eq!(decide("SomeFutureTool", "write"), (Ask, Access::Full));
    }

    #[test]
    fn native_tools_sharing_a_smelt_tool_use_its_summary() {
        let lua = LuaRuntime::new();
        lua.lua
            .load(
                r#"smelt.tools.register({
                    name = "edit_file",
                    access = "write",
                    summary = function(args) return "edit " .. args.file_path end,
                    execute = function() return "ok" end,
                })"#,
            )
            .exec()
            .unwrap();
        let args = HashMap::from([("file_path".to_string(), Value::from("/w/init.lua"))]);
        let evaluation = evaluate(&lua, AgentMode::parse("write").unwrap(), "Edit", &args);
        assert_eq!(
            evaluation.metadata.summary.as_plain_text(),
            "edit /w/init.lua"
        );
    }

    #[test]
    fn native_monitor_runs_as_bash() {
        let lua = LuaRuntime::new();
        lua.lua
            .load(
                r#"smelt.tools.register({
                    name = "bash",
                    access = "write",
                    summary = function(args)
                        return "$ " .. args.command .. (args.background and " &" or "")
                    end,
                    execute = function() return "ok" end,
                })"#,
            )
            .exec()
            .unwrap();
        let args = HashMap::from([("command".to_string(), Value::from("tail -f log"))]);
        let evaluation = evaluate(&lua, AgentMode::parse("write").unwrap(), "Monitor", &args);
        assert_eq!(evaluation.decision, Ask);
        assert_eq!(evaluation.metadata.access, Access::Full);
        assert_eq!(
            evaluation.metadata.summary.as_plain_text(),
            "$ tail -f log &"
        );
    }
}
