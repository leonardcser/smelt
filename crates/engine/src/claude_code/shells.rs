//! The shell commands Claude Code runs through its Bash tool. They run inside
//! its process, so smelt lists them from its task events and reads their
//! output with `get_task_output`.

use super::transport::control;
use protocol::{HistoryNote, JobTermination, NativeProcess, ProcessStatusEvent};
use serde_json::{json, Value};
use smelt_buffer::text;
use std::collections::{BTreeMap, HashMap, HashSet};

const OUTPUT_REQUEST: &str = "smelt-output-";
/// The end of a finished background shell's output its note keeps.
const FINISHED_OUTPUT_BYTES: u64 = 64 * 1024;

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
    /// Notes on background shells that finished since the last call.
    finished: Vec<HistoryNote>,
}

impl Shells {
    pub(super) fn new(provider: &str) -> Self {
        Self {
            provider: provider.into(),
            live: BTreeMap::new(),
            commands: HashMap::new(),
            reading: HashSet::new(),
            changed: false,
            finished: Vec::new(),
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
            Some("task_notification") => {
                let Some(shell) = self.live.remove(task) else {
                    return false;
                };
                self.reading.remove(task);
                self.changed = true;
                if shell.process.background {
                    self.finished.push(finished(frame, shell.process));
                }
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

    /// Notes on the background shells that finished since the last call.
    pub(super) fn finished(&mut self) -> Vec<HistoryNote> {
        std::mem::take(&mut self.finished)
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

/// The note on a background shell Claude Code reports finished. It reports
/// how it ended but not its exit code, and keeps the output in a file.
fn finished(frame: &Value, process: NativeProcess) -> HistoryNote {
    let (exit_code, termination) = match frame["status"].as_str() {
        Some("completed") => (Some(0), JobTermination::Exited),
        Some("failed") => (None, JobTermination::Failed),
        Some("stopped" | "killed") => (None, JobTermination::Stopped),
        _ => (None, JobTermination::Exited),
    };
    let output = frame["output_file"]
        .as_str()
        .filter(|path| !path.is_empty())
        .and_then(|path| tail(path, FINISHED_OUTPUT_BYTES))
        .unwrap_or(process.output);
    HistoryNote::process_status_with_output(
        ProcessStatusEvent::background_process_completed(process.id, exit_code, termination),
        output.trim_end(),
    )
}

/// The end of the file at `path`, at most `limit` bytes of whole lines.
fn tail(path: &str, limit: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    // One byte more shows whether the cut starts a line.
    let start = len.saturating_sub(limit + 1);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let output = String::from_utf8_lossy(&bytes).into_owned();
    if start == 0 && len <= limit {
        return Some(output);
    }
    let first = output.find('\n').map_or(output.len(), |end| end + 1);
    Some(text::slice(&output, first..output.len()).to_owned())
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

    fn ended(task: &str, status: &str) -> Value {
        json!({"type":"system", "subtype":"task_notification", "task_id":task, "status":status, "output_file":""})
    }

    #[test]
    fn finished_background_shells_get_a_note_by_how_they_ended() {
        for (status, exit_code, termination) in [
            ("completed", Some(0), JobTermination::Exited),
            ("failed", None, JobTermination::Failed),
            ("stopped", None, JobTermination::Stopped),
            ("killed", None, JobTermination::Stopped),
        ] {
            let mut shells = Shells::new("claude-code");
            shells.frame(&started("task", "call", true), 0);
            shells.frame(&ended("task", status), 0);
            let event = ProcessStatusEvent::background_process_completed(
                "claude-code:task",
                exit_code,
                termination,
            );
            assert_eq!(
                shells.finished(),
                [HistoryNote::process_status_event(event)],
                "{status}"
            );
        }
    }

    #[test]
    fn foreground_shells_end_in_their_tool_block_without_a_note() {
        let mut shells = Shells::new("claude-code");
        shells.frame(&started("task", "call", false), 0);
        shells.frame(&ended("task", "completed"), 0);
        assert!(shells.finished().is_empty());
    }

    #[test]
    fn a_long_output_keeps_its_last_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.output");
        std::fs::write(&path, "first line\nsecond\nthird\n").unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(tail(path, 13).unwrap(), "second\nthird\n");
        assert_eq!(tail(path, 12).unwrap(), "third\n");
        assert_eq!(tail(path, 100).unwrap(), "first line\nsecond\nthird\n");
        assert!(tail("/nonexistent/task.output", 100).is_none());
    }
}
