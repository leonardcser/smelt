//! The shell commands Claude Code runs through its Bash tool. They run inside
//! its process, so smelt lists them from its task events and reads their
//! output with `get_task_output`.

use super::transport::control;
use protocol::NativeProcess;
use serde_json::{json, Value};
use smelt_buffer::text;
use std::collections::{BTreeMap, HashMap, HashSet};

const OUTPUT_REQUEST: &str = "smelt-output-";

struct Shell {
    process: NativeProcess,
    tool_use_id: Option<String>,
    /// Output bytes already streamed into the tool's transcript block.
    streamed: u64,
    /// The streamed output's unfinished last line.
    partial: String,
}

/// Lines a foreground shell printed since the last read, for its tool call.
pub(super) struct Printed {
    pub tool_use_id: String,
    pub lines: Vec<String>,
}

pub(super) struct Shells {
    provider: String,
    live: BTreeMap<String, Shell>,
    /// Commands of Bash calls whose task has not started yet.
    commands: HashMap<String, String>,
    reading: HashSet<String>,
    changed: bool,
}

impl Shells {
    pub(super) fn new(provider: &str) -> Self {
        Self {
            provider: provider.into(),
            live: BTreeMap::new(),
            commands: HashMap::new(),
            reading: HashSet::new(),
            changed: false,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Returns whether the frame was about a shell.
    pub(super) fn frame(&mut self, frame: &Value, now_ms: u64) -> bool {
        if frame["type"] == "assistant" {
            self.remember_commands(frame);
            return false;
        }
        if frame["type"] != "system" {
            return false;
        }
        let task = frame["task_id"].as_str().unwrap_or_default();
        match frame["subtype"].as_str() {
            Some("task_started") if frame["task_type"] == "local_bash" => {
                let tool_use_id = frame["tool_use_id"].as_str().map(str::to_owned);
                let command = tool_use_id
                    .as_ref()
                    .and_then(|id| self.commands.remove(id))
                    .unwrap_or_else(|| description(frame));
                // A background shell joins the level before it starts.
                let shell = self.track(task, now_ms);
                shell.process.command = command;
                shell.process.background = frame["is_backgrounded"] == true;
                shell.tool_use_id = tool_use_id;
                self.changed = true;
                true
            }
            Some("task_updated") if self.live.contains_key(task) => {
                if let Some(background) = frame["patch"]["is_backgrounded"].as_bool() {
                    self.live.get_mut(task).unwrap().process.background = background;
                    self.changed = true;
                }
                true
            }
            Some("task_progress") if self.live.contains_key(task) => true,
            Some("task_notification") if self.live.remove(task).is_some() => {
                self.reading.remove(task);
                self.changed = true;
                true
            }
            // The full set of background tasks: a missed start or end must not
            // leave the list wrong.
            Some("background_tasks_changed") => {
                let listed: HashMap<&str, &Value> = frame["tasks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|task| task["task_type"] == "local_bash")
                    .filter_map(|task| Some((task["task_id"].as_str()?, task)))
                    .collect();
                let before = self.live.len();
                self.live.retain(|id, shell| {
                    !shell.process.background || listed.contains_key(id.as_str())
                });
                self.changed |= self.live.len() != before;
                for (task, entry) in listed {
                    let shell = self.track(task, now_ms);
                    if shell.process.command.is_empty() {
                        shell.process.command = description(entry);
                    }
                    let moved = !shell.process.background;
                    shell.process.background = true;
                    self.changed |= moved;
                }
                true
            }
            _ => false,
        }
    }

    fn track(&mut self, task: &str, now_ms: u64) -> &mut Shell {
        if !self.live.contains_key(task) {
            self.changed = true;
        }
        self.live.entry(task.into()).or_insert_with(|| Shell {
            process: NativeProcess {
                id: format!("{}:{task}", self.provider),
                command: String::new(),
                started_at_ms: now_ms,
                background: false,
                output: String::new(),
            },
            tool_use_id: None,
            streamed: 0,
            partial: String::new(),
        })
    }

    fn remember_commands(&mut self, frame: &Value) {
        let Some(content) = frame["message"]["content"].as_array() else {
            return;
        };
        for block in content {
            if block["type"] == "tool_use" && block["name"] == "Bash" {
                if let (Some(id), Some(command)) =
                    (block["id"].as_str(), block["input"]["command"].as_str())
                {
                    self.commands.insert(id.into(), command.into());
                }
            }
        }
    }

    /// Reads the output of foreground shells, which streams into their tool
    /// blocks. Background output is read when someone looks at it.
    pub(super) fn reads(&mut self) -> Vec<Value> {
        let foreground: Vec<String> = self
            .live
            .iter()
            .filter(|(_, shell)| !shell.process.background)
            .map(|(task, _)| task.clone())
            .collect();
        foreground
            .into_iter()
            .filter_map(|task| self.read(&task))
            .collect()
    }

    pub(super) fn read_request(&mut self, id: &str) -> Option<Value> {
        let task = self.task(id)?.to_owned();
        self.read(&task)
    }

    fn read(&mut self, task: &str) -> Option<Value> {
        self.reading.insert(task.into()).then(|| {
            control(
                &format!("{OUTPUT_REQUEST}{task}"),
                json!({"subtype":"get_task_output", "task_id":task}),
            )
        })
    }

    /// Takes the answer to an output read, with the lines a foreground shell
    /// printed since the last one.
    pub(super) fn output(&mut self, response: &Value) -> Option<Printed> {
        let task = response["request_id"]
            .as_str()?
            .strip_prefix(OUTPUT_REQUEST)?;
        self.reading.remove(task);
        let shell = self.live.get_mut(task)?;
        let answer = &response["response"];
        let output = answer["output"].as_str()?;
        let total = answer["total_bytes"].as_u64()?;
        if shell.process.output != output {
            shell.process.output = output.into();
            self.changed = true;
        }
        if shell.process.background || total <= shell.streamed {
            return None;
        }
        let tool_use_id = shell.tool_use_id.clone()?;
        // The output holds only its end, so bytes older than it are gone.
        let fresh = usize::try_from(total - shell.streamed).unwrap_or(usize::MAX);
        let mut start = output.len().saturating_sub(fresh);
        if text::snap(output, start) != start {
            start = text::next_char_boundary(output, start);
        }
        shell.streamed = total;
        shell.partial.push_str(&output[start..]);
        let end = shell.partial.rfind('\n')?;
        let rest = shell.partial.split_off(end + 1);
        let complete = std::mem::replace(&mut shell.partial, rest);
        Some(Printed {
            tool_use_id,
            lines: complete.lines().map(str::to_owned).collect(),
        })
    }

    /// The list, when it changed since the last call.
    pub(super) fn changed(&mut self) -> Option<Vec<NativeProcess>> {
        std::mem::take(&mut self.changed).then(|| {
            self.live
                .values()
                .map(|shell| shell.process.clone())
                .collect()
        })
    }

    /// The live task a namespaced process ID names.
    fn task<'a>(&self, id: &'a str) -> Option<&'a str> {
        let task = id.strip_prefix(&self.provider)?.strip_prefix(':')?;
        self.live.contains_key(task).then_some(task)
    }

    pub(super) fn stop_request(&self, id: &str) -> Option<Value> {
        self.task(id).map(stop)
    }

    pub(super) fn stop_all(&self) -> Vec<Value> {
        self.live.keys().map(|task| stop(task)).collect()
    }

    /// Moves each foreground shell to the background, leaving foreground
    /// subagents waited for.
    pub(super) fn background_requests(&self) -> Vec<Value> {
        self.live
            .values()
            .filter(|shell| !shell.process.background)
            .filter_map(|shell| shell.tool_use_id.as_deref())
            .map(|tool_use_id| {
                control(
                    &format!("smelt-background-{tool_use_id}"),
                    json!({"subtype":"background_tasks", "tool_use_id":tool_use_id}),
                )
            })
            .collect()
    }
}

fn stop(task: &str) -> Value {
    control(
        &format!("smelt-stop-{task}"),
        json!({"subtype":"stop_task", "task_id":task}),
    )
}

fn description(frame: &Value) -> String {
    frame["description"].as_str().unwrap_or_default().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(id: &str, command: &str) -> Value {
        json!({"type":"assistant", "message":{"content":[
            {"type":"tool_use", "id":id, "name":"Bash", "input":{"command":command}}
        ]}})
    }

    fn started(task: &str, tool: &str, background: bool) -> Value {
        json!({"type":"system", "subtype":"task_started", "task_id":task, "tool_use_id":tool,
            "task_type":"local_bash", "description":"Described", "is_backgrounded":background})
    }

    fn read(task: &str, output: &str) -> Value {
        json!({"subtype":"success", "request_id":format!("{OUTPUT_REQUEST}{task}"),
            "response":{"output":output, "total_bytes":output.len(), "truncated":false}})
    }

    #[test]
    fn lists_shells_by_their_command_until_they_end() {
        let mut shells = Shells::new("claude-code");
        assert!(!shells.frame(&bash("call", "cargo test"), 0));
        assert!(shells.frame(&started("task", "call", true), 5));
        let list = shells.changed().unwrap();
        assert_eq!(list[0].id, "claude-code:task");
        assert_eq!(list[0].command, "cargo test");
        assert_eq!(list[0].started_at_ms, 5);
        assert!(list[0].background);
        assert!(shells.changed().is_none());
        let ended = json!({"type":"system", "subtype":"task_notification", "task_id":"task", "status":"completed"});
        assert!(shells.frame(&ended, 9));
        assert_eq!(shells.changed().unwrap(), Vec::new());
    }

    #[test]
    fn a_start_after_the_level_names_the_shell_by_its_command() {
        let mut shells = Shells::new("claude-code");
        shells.frame(&bash("call", "npm run dev"), 0);
        let level = json!({"type":"system", "subtype":"background_tasks_changed", "tasks":[
            {"task_id":"task", "task_type":"local_bash", "description":"Start the server"}
        ]});
        shells.frame(&level, 1);
        assert_eq!(shells.changed().unwrap()[0].command, "Start the server");
        shells.frame(&started("task", "call", true), 2);
        let list = shells.changed().unwrap();
        assert_eq!(list[0].command, "npm run dev");
        assert_eq!(list[0].started_at_ms, 1);
        assert_eq!(shells.background_requests().len(), 0);
    }

    #[test]
    fn subagent_tasks_are_not_shells() {
        let mut shells = Shells::new("claude-code");
        let mut agent = started("agent", "call", true);
        agent["task_type"] = json!("local_agent");
        assert!(!shells.frame(&agent, 0));
        let ended = json!({"type":"system", "subtype":"task_notification", "task_id":"agent"});
        assert!(!shells.frame(&ended, 0));
        assert!(shells.is_empty());
    }

    #[test]
    fn the_background_level_corrects_missed_edges() {
        let mut shells = Shells::new("claude-code");
        shells.frame(&started("gone", "call", true), 0);
        shells.frame(&started("foreground", "other", false), 0);
        let level = json!({"type":"system", "subtype":"background_tasks_changed", "tasks":[
            {"task_id":"unseen", "task_type":"local_bash", "description":"npm run dev"},
            {"task_id":"agent", "task_type":"local_agent", "description":"Child"}
        ]});
        assert!(shells.frame(&level, 3));
        let list = shells.changed().unwrap();
        let ids: Vec<_> = list.iter().map(|process| process.id.as_str()).collect();
        assert_eq!(ids, ["claude-code:foreground", "claude-code:unseen"]);
        assert_eq!(list[1].command, "npm run dev");
        // A foreground shell moved to the background joins the level.
        let moved = json!({"type":"system", "subtype":"task_updated", "task_id":"foreground",
            "patch":{"is_backgrounded":true}});
        shells.frame(&moved, 4);
        assert!(shells.changed().unwrap()[0].background);
    }

    #[test]
    fn reads_once_at_a_time_and_streams_complete_foreground_lines() {
        let mut shells = Shells::new("claude-code");
        shells.frame(&bash("call", "make"), 0);
        shells.frame(&started("task", "call", false), 0);
        assert_eq!(shells.reads().len(), 1);
        assert!(shells.reads().is_empty(), "a read is already in flight");
        let printed = shells.output(&read("task", "one\ntw")).unwrap();
        assert_eq!(printed.tool_use_id, "call");
        assert_eq!(printed.lines, ["one"]);
        assert_eq!(shells.changed().unwrap()[0].output, "one\ntw");
        assert_eq!(shells.reads().len(), 1);
        let printed = shells.output(&read("task", "one\ntwo\nthree\n")).unwrap();
        assert_eq!(printed.lines, ["two", "three"]);
        assert!(shells.output(&read("task", "one\ntwo\nthree\n")).is_none());
        assert!(shells
            .output(&json!({"request_id":"smelt-context"}))
            .is_none());
    }

    #[test]
    fn streaming_skips_what_the_tail_lost() {
        let mut shells = Shells::new("claude-code");
        shells.frame(&bash("call", "make"), 0);
        shells.frame(&started("task", "call", false), 0);
        shells.reads();
        let tail = json!({"subtype":"success", "request_id":format!("{OUTPUT_REQUEST}task"),
            "response":{"output":"é\nend\n", "total_bytes":9000, "truncated":true}});
        assert_eq!(shells.output(&tail).unwrap().lines, ["é", "end"]);
    }

    #[test]
    fn background_output_is_read_on_demand_and_not_streamed() {
        let mut shells = Shells::new("claude-code");
        shells.frame(&started("task", "call", true), 0);
        assert!(shells.reads().is_empty());
        assert!(shells.read_request("claude-code:other").is_none());
        let request = shells.read_request("claude-code:task").unwrap();
        assert_eq!(
            request["request"],
            json!({"subtype":"get_task_output", "task_id":"task"})
        );
        assert!(
            shells.read_request("claude-code:task").is_none(),
            "a read is already in flight"
        );
        assert!(shells.output(&read("task", "tick\n")).is_none());
        assert_eq!(shells.changed().unwrap()[0].output, "tick\n");
        assert!(shells.background_requests().is_empty());
    }

    #[test]
    fn backgrounds_each_foreground_shell_by_its_call() {
        let mut shells = Shells::new("claude-code");
        shells.frame(&started("task", "call", false), 0);
        shells.frame(&started("other", "detached", true), 0);
        let requests = shells.background_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]["request"],
            json!({"subtype":"background_tasks", "tool_use_id":"call"})
        );
    }

    #[test]
    fn stops_only_its_own_shells() {
        let mut shells = Shells::new("claude-code");
        shells.frame(&started("task", "call", true), 0);
        assert!(shells.stop_request("codex:task").is_none());
        assert!(shells.stop_request("claude-code:other").is_none());
        let request = shells.stop_request("claude-code:task").unwrap();
        assert_eq!(
            request["request"],
            json!({"subtype":"stop_task", "task_id":"task"})
        );
        assert_eq!(shells.stop_all().len(), 1);
    }
}
