//! Apply smelt policy to Claude Code's native tools without dispatching them.

use crate::lua::LuaRuntime;
use crate::permissions::Access;
use protocol::{AgentMode, Decision, ToolEvaluation, ToolMetadata};
use serde_json::Value;
use std::collections::HashMap;

/// Decide a native tool call with `smelt.permissions.check`. A native tool
/// checked as a smelt tool ([`engine::claude_code_checked_as`]) uses that
/// tool's rules and paths. The access comes from
/// [`engine::claude_code_native_access`], and the summary is the one the
/// transcript shows for the call under `provider`'s namespace.
/// Path arguments arrive absolute. Call inside a host scope.
pub fn evaluate(
    lua: &LuaRuntime,
    mode: AgentMode,
    provider: &str,
    name: &str,
    args: &HashMap<String, Value>,
) -> ToolEvaluation {
    let summary = lua.tool_summary(&format!("{provider}/{name}"), args);
    let access = engine::claude_code_native_access(name);
    let (tool, args) =
        engine::claude_code_checked_as(name, args).unwrap_or_else(|| (name, args.clone()));
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
        let evaluation = evaluate(&lua, mode, "claude-code", tool, &HashMap::new());
        (evaluation.decision, evaluation.metadata.access)
    }

    #[test]
    fn native_tools_without_a_smelt_tool_need_their_own_access() {
        assert_eq!(decide("ToolSearch", "read"), (Allow, Access::Read));
        assert_eq!(decide("KillShell", "read"), (Ask, Access::Write));
        assert_eq!(decide("KillShell", "write"), (Allow, Access::Write));
        assert_eq!(decide("TaskOutput", "read"), (Allow, Access::Read));
        assert_eq!(decide("TaskStop", "read"), (Ask, Access::Write));
        assert_eq!(decide("TaskStop", "write"), (Allow, Access::Write));
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
        let evaluation = evaluate(
            &lua,
            AgentMode::parse("write").unwrap(),
            "claude-code",
            "Edit",
            &args,
        );
        assert_eq!(
            evaluation.metadata.summary.as_plain_text(),
            "edit /w/init.lua"
        );
    }

    #[test]
    fn native_tools_of_their_own_use_their_presentation_summary() {
        let lua = LuaRuntime::new();
        let args = HashMap::from([("id".to_string(), Value::from("c1"))]);
        let mode = AgentMode::parse("write").unwrap();
        let evaluation = evaluate(&lua, mode.clone(), "claude-code", "CronDelete", &args);
        assert_eq!(evaluation.metadata.summary.as_plain_text(), "cancel job c1");
        let evaluation = evaluate(&lua, mode, "claude-code", "SomeFutureTool", &args);
        assert_eq!(evaluation.metadata.summary.as_plain_text(), "id c1");
    }

    #[test]
    fn every_native_presentation_is_for_a_tool_smelt_knows() {
        let lua = LuaRuntime::new();
        let names: Vec<String> = lua
            .lua
            .load(
                "local names = {}
                for name in pairs(require('smelt.transcript.native').presentations) do
                  names[#names + 1] = name
                end
                return names",
            )
            .eval()
            .unwrap();
        assert!(!names.is_empty());
        let unknown: Vec<_> = names
            .iter()
            .filter(|name| !engine::claude_code_knows(name))
            .collect();
        assert!(
            unknown.is_empty(),
            "presentations for unknown tools: {unknown:?}"
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
        let evaluation = evaluate(
            &lua,
            AgentMode::parse("write").unwrap(),
            "claude-code",
            "Monitor",
            &args,
        );
        assert_eq!(evaluation.decision, Ask);
        assert_eq!(evaluation.metadata.access, Access::Full);
        assert_eq!(
            evaluation.metadata.summary.as_plain_text(),
            "$ tail -f log &"
        );
    }
}
