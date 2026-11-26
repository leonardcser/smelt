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

/// When a successful call leaves its work running in the background, read
/// from Claude Code's structured result, and the result key that names the
/// task its end is reported under.
#[derive(Clone, Copy)]
enum Background {
    /// Every call runs in the background.
    Always(&'static str),
    /// A call runs in the background when its result names the task.
    WhenTask(&'static str),
    /// A call runs in the background when its result says it launched
    /// asynchronously.
    WhenAsync(&'static str),
}

/// What smelt keeps from Claude Code's structured result of a call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ResultShape {
    /// Only the text the model reads: the smelt tool the call shares renders it.
    Text,
    /// The file before and after the edit, as smelt's `edit_file` reports it.
    Edit,
    /// The report of the agent the call waited for.
    AgentReport,
    /// The whole result as `native` metadata, for a presentation that reads it.
    Structured,
}

/// A native tool smelt knows: the smelt tool it relates to, the access a call
/// asks for, how its arguments map to the smelt tool's, and how its result
/// reads.
struct NativeTool {
    name: &'static str,
    smelt: Option<Smelt>,
    /// `None` leaves the access to the smelt tool the call is checked as.
    access: Option<Access>,
    /// Argument renames from the native tool to the smelt tool.
    renames: &'static [(&'static str, &'static str)],
    background: Option<Background>,
    result: ResultShape,
}

impl NativeTool {
    /// A native tool sharing smelt tool `tool`'s capability.
    const fn shared(name: &'static str, tool: &'static str) -> Self {
        Self::of(name, Some(Shared(tool)), None)
    }

    /// A native tool that runs smelt tool `tool`'s capability under its own name.
    const fn runs_as(name: &'static str, tool: &'static str, access: Access) -> Self {
        Self::of(name, Some(RunsAs(tool)), Some(access))
    }

    /// A native tool for a capability smelt tool `tool` owns with another
    /// argument shape.
    const fn owned(name: &'static str, tool: &'static str, access: Access) -> Self {
        Self::of(name, Some(Owned(tool)), Some(access))
    }

    /// A native tool smelt has no tool for, whose calls ask for `access`.
    const fn own(name: &'static str, access: Access) -> Self {
        Self::of(name, None, Some(access))
    }

    const fn of(name: &'static str, smelt: Option<Smelt>, access: Option<Access>) -> Self {
        Self {
            name,
            smelt,
            access,
            renames: &[],
            background: None,
            result: ResultShape::Text,
        }
    }

    const fn access(mut self, access: Access) -> Self {
        self.access = Some(access);
        self
    }

    const fn renames(mut self, renames: &'static [(&'static str, &'static str)]) -> Self {
        self.renames = renames;
        self
    }

    const fn background(mut self, background: Background) -> Self {
        self.background = Some(background);
        self
    }

    const fn result(mut self, result: ResultShape) -> Self {
        self.result = result;
        self
    }
}

use Access::{Full, Read, Write};
use Background::{Always, WhenAsync, WhenTask};
use ResultShape::{AgentReport, Edit, Structured};
use Smelt::{Owned, RunsAs, Shared};

/// The native tools smelt knows. Native shell commands run outside smelt's
/// sandbox, so they need `full`. `TaskOutput` reads existing task output, and
/// `TaskStop` stops an existing task, so neither needs the access required to
/// start a native shell command. `read` tools only read or change state of
/// the Claude Code session itself. Tools that act on their own or reach
/// outside the machine, such as `Agent`, `SendMessage`, `CronCreate` and
/// `PushNotification`, and tools smelt does not know need `full`.
const NATIVE: &[NativeTool] = &[
    NativeTool::shared("Read", "read_file"),
    NativeTool::shared("Write", "write_file").result(Edit),
    NativeTool::shared("Edit", "edit_file").result(Edit),
    NativeTool::shared("MultiEdit", "edit_file").result(Edit),
    NativeTool::shared("NotebookEdit", "edit_notebook"),
    NativeTool::shared("Bash", "bash")
        .access(Full)
        .renames(&[("timeout", "timeout_ms")])
        .background(WhenTask("backgroundTaskId")),
    NativeTool::shared("Glob", "glob"),
    NativeTool::shared("Grep", "grep"),
    NativeTool::shared("WebFetch", "web_fetch"),
    NativeTool::shared("WebSearch", "web_search"),
    NativeTool::shared("AskUserQuestion", "ask_user_question"),
    NativeTool::shared("ExitPlanMode", "present_plan"),
    NativeTool::runs_as("Monitor", "bash", Full).background(Always("taskId")),
    NativeTool::owned("TaskOutput", "read_process_output", Read),
    NativeTool::owned("TaskStop", "stop_process", Write).result(Structured),
    // Claude Code names the tool `Agent`; older versions call it `Task`.
    NativeTool::own("Agent", Full)
        .background(WhenAsync("agentId"))
        .result(AgentReport),
    NativeTool::own("Task", Full)
        .background(WhenAsync("agentId"))
        .result(AgentReport),
    NativeTool::own("SendMessage", Full),
    NativeTool::own("ToolSearch", Read).result(Structured),
    NativeTool::own("Skill", Read),
    NativeTool::own("LSP", Read),
    NativeTool::own("NotebookRead", Read),
    NativeTool::own("BashOutput", Read),
    NativeTool::own("ListMcpResourcesTool", Read),
    NativeTool::own("ReadMcpResourceTool", Read),
    NativeTool::own("TodoWrite", Read),
    NativeTool::own("TaskCreate", Read).result(Structured),
    NativeTool::own("TaskGet", Read).result(Structured),
    NativeTool::own("TaskList", Read).result(Structured),
    NativeTool::own("TaskUpdate", Read),
    NativeTool::own("ListAgents", Read),
    NativeTool::own("CronList", Read).result(Structured),
    NativeTool::own("CronCreate", Full).result(Structured),
    NativeTool::own("CronDelete", Full),
    NativeTool::own("PushNotification", Full).result(Structured),
    NativeTool::own("EnterPlanMode", Read),
    NativeTool::own("ReportFindings", Read),
    NativeTool::own("StructuredOutput", Read),
    NativeTool::own("ScheduleWakeup", Read),
    NativeTool::own("Sleep", Read),
    NativeTool::own("KillShell", Write),
    NativeTool::own("KillBash", Write),
    NativeTool::own("SendToProjectContext", Read),
    NativeTool::own("PowerShell", Full),
    NativeTool::own("Workflow", Full),
    NativeTool::own("ExitWorktree", Full),
    NativeTool::own("RemoteTrigger", Full),
    NativeTool::own("SendToProject", Full),
    NativeTool::own("DesignSync", Full),
    NativeTool::own("Artifact", Full),
];

fn native(name: &str) -> Option<&'static NativeTool> {
    NATIVE.iter().find(|tool| tool.name == name)
}

fn native_equivalents(name: &str) -> Vec<&'static str> {
    NATIVE
        .iter()
        .filter(|native| matches!(native.smelt, Some(Shared(tool) | Owned(tool)) if tool == name))
        .map(|native| native.name)
        .collect()
}

/// The smelt tool sharing a native tool's capability, with the call's
/// arguments renamed to that tool's parameters.
pub fn smelt_tool(
    name: &str,
    args: &HashMap<String, Value>,
) -> Option<(&'static str, HashMap<String, Value>)> {
    let native = native(name)?;
    match native.smelt? {
        Shared(tool) => Some((tool, native.smelt_args(args))),
        RunsAs(_) | Owned(_) => None,
    }
}

/// The smelt tool whose permission rules and summary a native call uses:
/// the one sharing its capability or the one it runs as, with the call's
/// arguments renamed to that tool's parameters.
pub fn checked_as(
    name: &str,
    args: &HashMap<String, Value>,
) -> Option<(&'static str, HashMap<String, Value>)> {
    let native = native(name)?;
    match native.smelt? {
        Shared(tool) | RunsAs(tool) => Some((tool, native.smelt_args(args))),
        Owned(_) => None,
    }
}

impl NativeTool {
    fn smelt_args(&self, args: &HashMap<String, Value>) -> HashMap<String, Value> {
        let mut args: HashMap<String, Value> = args
            .iter()
            .map(|(key, value)| {
                let key = self
                    .renames
                    .iter()
                    .find(|(from, _)| from == key)
                    .map_or(key.as_str(), |(_, to)| to);
                (key.to_owned(), value.clone())
            })
            .collect();
        let background = if matches!(self.background, Some(Always(_))) {
            Some(true)
        } else if self.name == "Bash" {
            args.remove("run_in_background")
                .and_then(|value| value.as_bool())
        } else {
            None
        };
        if let Some(background) = background {
            let mode = if background {
                "background"
            } else {
                "foreground"
            };
            args.insert("run_mode".into(), Value::from(mode));
        }
        args
    }
}

/// Work a successful call left running in the background.
pub(super) struct Detached<'a> {
    /// The task Claude Code reports the work's end under, when the result names it.
    pub(super) task: Option<&'a str>,
}

/// The work a successful call of `name` left running in the background, read
/// from Claude Code's structured result `result`.
pub(super) fn detached<'a>(name: &str, result: &'a Value) -> Option<Detached<'a>> {
    let (runs, key) = match native(name)?.background? {
        Always(key) => (true, key),
        WhenTask(key) => (result[key].is_string(), key),
        WhenAsync(key) => (
            result["isAsync"] == true || result["status"] == "async_launched",
            key,
        ),
    };
    runs.then(|| Detached {
        task: result[key].as_str().filter(|task| !task.is_empty()),
    })
}

/// What smelt keeps from the structured result of a call of `name`.
pub(super) fn result_shape(name: &str) -> ResultShape {
    native(name).map_or(ResultShape::Text, |native| native.result)
}

/// Whether smelt knows the native tool `name`; see [`NATIVE`].
pub fn knows(name: &str) -> bool {
    native(name).is_some()
}

/// The access a call of `name` asks for; see [`NATIVE`].
pub fn native_access(name: &str) -> Option<Access> {
    native(name).map_or(Some(Full), |native| native.access)
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
