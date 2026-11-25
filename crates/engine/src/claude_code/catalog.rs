use protocol::{Access, AgentMode, ToolDef};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};

/// One owner per native capability. Internal UI adapters stay callable by the
/// host without becoming a second model-visible question or planning toolkit.
#[derive(Default)]
pub(super) struct Catalog {
    exposed: HashSet<String>,
    visibility: protocol::ToolVisibility,
    pub(super) disabled_native: Vec<String>,
}

/// How a native tool relates to a smelt tool.
#[derive(Clone, Copy)]
enum Smelt {
    /// The same capability: the call takes the smelt tool's name, permission
    /// rules, summary and renderer.
    Shared(&'static str),
    /// Runs the smelt tool's capability under its own name: the call uses the
    /// smelt tool's permission rules and summary.
    RunsAs(&'static str),
    /// The smelt tool owns the capability with another argument shape.
    Owned(&'static str),
}

use Access::{Full, Read, Write};
use Smelt::{Owned, RunsAs, Shared};

/// The native tools smelt knows, with the access a call asks for. `None`
/// leaves the access to the smelt tool the call is checked as. Native shell
/// commands run outside smelt's sandbox, so they need `full`. `TaskOutput`
/// reads existing task output, and `TaskStop` stops an existing task, so
/// neither needs the access required to start a native shell command.
/// `read` tools only read or change state of the Claude Code session itself.
/// Tools that act on their own or reach outside the machine, such as `Agent`,
/// `SendMessage`, `CronCreate` and `PushNotification`, and tools smelt does
/// not know need `full`.
const NATIVE: &[(&str, Option<Smelt>, Option<Access>)] = &[
    ("Read", Some(Shared("read_file")), None),
    ("Write", Some(Shared("write_file")), None),
    ("Edit", Some(Shared("edit_file")), None),
    ("MultiEdit", Some(Shared("edit_file")), None),
    ("NotebookEdit", Some(Shared("edit_notebook")), None),
    ("Bash", Some(Shared("bash")), Some(Full)),
    ("Glob", Some(Shared("glob")), None),
    ("Grep", Some(Shared("grep")), None),
    ("WebFetch", Some(Shared("web_fetch")), None),
    ("WebSearch", Some(Shared("web_search")), None),
    ("AskUserQuestion", Some(Shared("ask_user_question")), None),
    ("ExitPlanMode", Some(Shared("present_plan")), None),
    ("Monitor", Some(RunsAs("bash")), Some(Full)),
    ("TaskOutput", Some(Owned("read_process_output")), Some(Read)),
    ("TaskStop", Some(Owned("stop_process")), Some(Write)),
    ("ToolSearch", None, Some(Read)),
    ("Skill", None, Some(Read)),
    ("LSP", None, Some(Read)),
    ("NotebookRead", None, Some(Read)),
    ("BashOutput", None, Some(Read)),
    ("ListMcpResourcesTool", None, Some(Read)),
    ("ReadMcpResourceTool", None, Some(Read)),
    ("TodoWrite", None, Some(Read)),
    ("TaskCreate", None, Some(Read)),
    ("TaskGet", None, Some(Read)),
    ("TaskList", None, Some(Read)),
    ("TaskUpdate", None, Some(Read)),
    ("ListAgents", None, Some(Read)),
    ("CronList", None, Some(Read)),
    ("EnterPlanMode", None, Some(Read)),
    ("ReportFindings", None, Some(Read)),
    ("StructuredOutput", None, Some(Read)),
    ("ScheduleWakeup", None, Some(Read)),
    ("Sleep", None, Some(Read)),
    ("KillShell", None, Some(Write)),
    ("KillBash", None, Some(Write)),
];

fn smelt(native: &str) -> Option<Smelt> {
    NATIVE
        .iter()
        .find(|(name, _, _)| *name == native)
        .and_then(|(_, smelt, _)| *smelt)
}

fn native_equivalents(name: &str) -> Vec<&'static str> {
    NATIVE
        .iter()
        .filter(|(_, smelt, _)| matches!(smelt, Some(Shared(tool) | Owned(tool)) if *tool == name))
        .map(|(native, _, _)| *native)
        .collect()
}

/// The smelt tool sharing a native tool's capability, with the call's
/// arguments renamed to that tool's parameters.
pub fn smelt_tool(
    native: &str,
    args: &HashMap<String, Value>,
) -> Option<(&'static str, HashMap<String, Value>)> {
    match smelt(native)? {
        Shared(tool) => Some((tool, smelt_args(native, args))),
        RunsAs(_) | Owned(_) => None,
    }
}

/// The smelt tool whose permission rules and summary a native call uses:
/// the one sharing its capability or the one it runs as, with the call's
/// arguments renamed to that tool's parameters.
pub fn checked_as(
    native: &str,
    args: &HashMap<String, Value>,
) -> Option<(&'static str, HashMap<String, Value>)> {
    match smelt(native)? {
        Shared(tool) | RunsAs(tool) => Some((tool, smelt_args(native, args))),
        Owned(_) => None,
    }
}

fn smelt_args(native: &str, args: &HashMap<String, Value>) -> HashMap<String, Value> {
    let mut args: HashMap<String, Value> = args
        .iter()
        .map(|(key, value)| {
            let key = match (native, key.as_str()) {
                ("Bash", "timeout") => "timeout_ms",
                ("Bash", "run_in_background") => "background",
                _ => key,
            };
            (key.to_owned(), value.clone())
        })
        .collect();
    if native == "Monitor" {
        // A monitor always runs in the background and streams its output.
        args.insert("background".into(), Value::Bool(true));
    }
    args
}

/// Whether a successful call of `native` left its work running in the
/// background, read from Claude Code's structured result.
pub(super) fn continues_in_background(native: &str, result: &Value) -> bool {
    match native {
        "Bash" => result["backgroundTaskId"].is_string(),
        "Monitor" => true,
        "Agent" | "Task" => result["isAsync"] == true || result["status"] == "async_launched",
        _ => false,
    }
}

/// The access a call of `native` asks for; see [`NATIVE`].
pub fn native_access(native: &str) -> Option<Access> {
    NATIVE
        .iter()
        .find(|(name, _, _)| *name == native)
        .map_or(Some(Full), |(_, _, access)| *access)
}

impl Catalog {
    pub(super) fn new(
        tools: &[ToolDef],
        visibility: protocol::ToolVisibility,
    ) -> Result<Self, String> {
        let mut exposed = HashSet::new();
        let mut disabled = BTreeSet::new();
        for tool in tools {
            let equivalents = native_equivalents(&tool.name);
            let Some(policy) = &tool.claude_code else {
                if equivalents.is_empty() {
                    exposed.insert(tool.name.clone());
                }
                continue;
            };
            if !policy.enabled {
                if !policy.replaces.is_empty() {
                    return Err(format!(
                        "Hidden tool {} cannot replace native tools",
                        tool.name
                    ));
                }
                continue;
            }
            if equivalents
                .iter()
                .any(|native| !policy.replaces.iter().any(|name| name == native))
            {
                return Err(format!(
                    "Tool {} duplicates a native capability; claude_code.replaces must include {}",
                    tool.name,
                    equivalents.join(", ")
                ));
            }
            for native in &policy.replaces {
                if native.is_empty()
                    || !native
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    return Err(format!(
                        "Invalid native tool name in {}: {native}",
                        tool.name
                    ));
                }
                if !disabled.insert(native.clone()) {
                    return Err(format!("Multiple smelt tools replace native tool {native}"));
                }
            }
            exposed.insert(tool.name.clone());
        }
        Ok(Self {
            exposed,
            visibility,
            disabled_native: disabled.into_iter().collect(),
        })
    }

    pub(super) fn exposes(&self, tool: &ToolDef, mode: &AgentMode) -> bool {
        self.visibility.includes(tool.headless)
            && self.exposed.contains(&tool.name)
            && tool.modes.as_ref().is_none_or(|modes| modes.contains(mode))
    }

    pub(super) fn owns(&self, native: &str) -> bool {
        self.disabled_native.iter().any(|name| name == native)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build_catalog(tools: &[ToolDef]) -> Result<Catalog, String> {
        Catalog::new(tools, protocol::ToolVisibility::Interactive)
    }

    fn tool(name: &str, policy: serde_json::Value) -> ToolDef {
        serde_json::from_value(
            json!({"name":name,"description":"test","parameters":{},"claude_code":policy}),
        )
        .unwrap()
    }

    #[test]
    fn defaults_are_native_first_without_hiding_custom_capabilities() {
        let tools = vec![
            tool("bash", json!(null)),
            tool("load_skill", json!(null)),
            tool("custom", json!(null)),
        ];
        let catalog = build_catalog(&tools).unwrap();
        assert!(!catalog.exposes(&tools[0], &AgentMode::read()));
        assert!(catalog.exposes(&tools[1], &AgentMode::read()));
        assert!(catalog.exposes(&tools[2], &AgentMode::read()));
        assert!(catalog.disabled_native.is_empty());
    }

    #[test]
    fn headless_visibility_does_not_change_native_ownership() {
        let mut replacement = tool("ui_shell", json!({"replaces":["Bash"]}));
        replacement.headless = false;
        let mut complementary = tool("custom", json!(null));
        complementary.headless = true;
        let tools = [replacement, complementary];
        let catalog = Catalog::new(&tools, protocol::ToolVisibility::Headless).unwrap();
        assert!(catalog.owns("Bash"));
        assert!(!catalog.exposes(&tools[0], &AgentMode::read()));
        assert!(catalog.exposes(&tools[1], &AgentMode::read()));
        let mut conflict = tools[0].clone();
        conflict.name = "other_shell".into();
        conflict.headless = true;
        assert!(Catalog::new(
            &[tools[0].clone(), conflict],
            protocol::ToolVisibility::Headless
        )
        .is_err());
    }

    #[test]
    fn replacements_disable_native_tools_and_conflicts_fail_closed() {
        let replacement = tool("my_shell", json!({"enabled":true,"replaces":["Bash"]}));
        let catalog = build_catalog(std::slice::from_ref(&replacement)).unwrap();
        assert!(catalog.exposes(&replacement, &AgentMode::read()));
        assert!(catalog.owns("Bash"));
        assert!(build_catalog(&[
            replacement,
            tool("other_shell", json!({"enabled":true,"replaces":["Bash"]}))
        ])
        .is_err());
        assert!(build_catalog(&[tool("bash", json!({"enabled":true}))]).is_err());
        assert!(build_catalog(&[tool("edit_file", json!({"replaces":["Edit"]}))]).is_err());
    }
}
