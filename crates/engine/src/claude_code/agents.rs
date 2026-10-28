//! Read-only projections of Claude-owned tasks. Execution stays in the native session.

use super::transcript::Transcript;
use crate::EngineEventSender;
use protocol::{AgentCapabilities, AgentInfo, AgentOwner, AgentStatus, EngineEvent};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

const MAX_PENDING_FRAMES: usize = 1024;
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

struct Launch {
    owner: Option<String>,
    title: Option<String>,
    task: Option<String>,
}

struct Child {
    info: AgentInfo,
    launch_id: Option<String>,
    transcript: Transcript,
}

struct Stop {
    control_id: String,
    deadline: Instant,
}

struct PendingFrame {
    frame: Value,
    received_at_ms: u64,
    bytes: usize,
}

pub(super) struct Agents {
    session_id: String,
    provider: String,
    ephemeral: bool,
    tx: EngineEventSender,
    children: HashMap<String, Child>,
    launches: HashMap<String, Launch>,
    by_launch: HashMap<String, String>,
    by_agent: HashMap<String, String>,
    tool_owners: HashMap<String, String>,
    pending: Vec<PendingFrame>,
    pending_bytes: usize,
    stops: HashMap<String, Stop>,
}

impl Agents {
    pub(super) fn new(
        tx: EngineEventSender,
        session_id: &str,
        provider: &str,
        ephemeral: bool,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            provider: provider.into(),
            ephemeral,
            tx,
            children: HashMap::new(),
            launches: HashMap::new(),
            by_launch: HashMap::new(),
            by_agent: HashMap::new(),
            tool_owners: HashMap::new(),
            pending: Vec::new(),
            pending_bytes: 0,
            stops: HashMap::new(),
        }
    }

    /// Returns true for child frames and task telemetry, including buffered frames.
    pub(super) fn frame(&mut self, frame: &Value, now_ms: u64) -> Result<bool, String> {
        if frame["type"] == "control_response" {
            return Ok(self.stop_response(frame));
        }
        if frame["type"] == "system" {
            match frame["subtype"].as_str() {
                Some("task_started") => {
                    self.started(frame)?;
                    self.replay();
                    return Ok(true);
                }
                Some("task_progress" | "task_notification" | "task_updated") => {
                    self.update(frame);
                    return Ok(true);
                }
                _ => return Ok(false),
            }
        }
        if !matches!(
            frame["type"].as_str(),
            Some("assistant" | "user" | "stream_event")
        ) {
            return Ok(false);
        }
        let parent = frame["parent_tool_use_id"].as_str();
        let agent = frame["agent_id"].as_str();
        if parent.is_none() && agent.is_none() {
            self.remember_tools(frame, None);
            return Ok(false);
        }
        if let Some(task) = self.resolve(frame) {
            self.child_frame(&task, frame, now_ms);
        } else {
            let bytes = frame.to_string().len();
            if self.pending.len() >= MAX_PENDING_FRAMES
                || self.pending_bytes.saturating_add(bytes) > MAX_PENDING_BYTES
            {
                return Err("Claude Code exceeded the unannounced child transcript limit".into());
            }
            self.pending.push(PendingFrame {
                frame: frame.clone(),
                received_at_ms: now_ms,
                bytes,
            });
            self.pending_bytes += bytes;
        }
        Ok(true)
    }

    fn resolve(&self, frame: &Value) -> Option<String> {
        frame["parent_tool_use_id"]
            .as_str()
            .and_then(|id| self.by_launch.get(id))
            .or_else(|| {
                frame["agent_id"]
                    .as_str()
                    .and_then(|id| self.by_agent.get(id))
            })
            .cloned()
    }

    fn remember_tools(&mut self, frame: &Value, owner: Option<&str>) {
        if frame["type"] != "assistant" {
            return;
        }
        let Some(blocks) = frame["message"]["content"].as_array() else {
            return;
        };
        let mut launches_changed = false;
        for block in blocks {
            if block["type"] != "tool_use" {
                continue;
            }
            let Some(id) = block["id"].as_str() else {
                continue;
            };
            if let Some(owner) = owner {
                self.tool_owners.insert(id.into(), owner.into());
            }
            if !matches!(
                block["name"].as_str(),
                Some("Agent" | "Task" | "SendMessage")
            ) {
                continue;
            }
            let std::collections::hash_map::Entry::Vacant(entry) = self.launches.entry(id.into())
            else {
                continue;
            };
            entry.insert(Launch {
                owner: owner.map(str::to_owned),
                title: block["input"]["description"].as_str().map(str::to_owned),
                task: block["input"]["prompt"].as_str().map(str::to_owned),
            });
            launches_changed = true;
        }
        if launches_changed {
            self.refresh_metadata();
        }
    }

    fn started(&mut self, frame: &Value) -> Result<(), String> {
        let Some(task) = frame["task_id"].as_str() else {
            return Ok(());
        };
        // Shell/server tasks use the same telemetry but are not child agents.
        if frame["task_type"]
            .as_str()
            .is_some_and(|kind| !matches!(kind, "local_agent" | "agent" | "subagent"))
        {
            return Ok(());
        }
        let launch_id = frame["tool_use_id"].as_str();
        let launch = launch_id.and_then(|id| self.launches.get(id));
        let parent = launch
            .and_then(|launch| launch.owner.as_ref())
            .map(|id| native_id(id));
        let title = frame["description"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| launch.and_then(|launch| launch.title.as_deref()))
            .unwrap_or("Claude Code agent")
            .to_owned();
        let prompt = frame["prompt"]
            .as_str()
            .or_else(|| launch.and_then(|launch| launch.task.as_deref()))
            .unwrap_or("")
            .to_owned();
        let generation = match self.children.get(task) {
            Some(child) if child.info.status.is_terminal() => {
                if launch_id.is_none() || launch_id == child.launch_id.as_deref() {
                    return Ok(());
                }
                child
                    .info
                    .generation
                    .checked_add(1)
                    .ok_or("Claude Code agent generation overflow")?
            }
            Some(_) => {
                if let Some(id) = launch_id {
                    self.by_launch.insert(id.into(), task.into());
                }
                return Ok(());
            }
            None => 1,
        };
        let info = AgentInfo {
            id: native_id(task),
            ephemeral: self.ephemeral,
            generation,
            parent_agent_id: parent,
            owner: AgentOwner::ClaudeCode,
            title,
            task: prompt,
            status: AgentStatus::Running,
            activity: String::new(),
            result: None,
            error: None,
            capabilities: AgentCapabilities {
                stop: true,
                follow_up: false,
            },
            usage: None,
            total_tokens: None,
            cost_usd: None,
        };
        let transcript = if let Some(child) = self.children.remove(task) {
            let mut transcript = child.transcript;
            transcript.route(self.tx.for_agent(&self.session_id, &info.id, generation));
            transcript
        } else {
            Transcript::new(
                self.tx.for_agent(&self.session_id, &info.id, generation),
                self.provider.clone(),
                0,
                0,
            )
        };
        self.children.insert(
            task.into(),
            Child {
                info,
                launch_id: launch_id.map(str::to_owned),
                transcript,
            },
        );
        if let Some(id) = launch_id {
            self.by_launch.insert(id.into(), task.into());
        }
        self.by_agent.insert(task.into(), task.into());
        if let Some(id) = frame["agent_id"].as_str() {
            self.by_agent.insert(id.into(), task.into());
        }
        self.announce(task);
        Ok(())
    }

    fn child_frame(&mut self, task: &str, frame: &Value, now_ms: u64) {
        if let Some(id) = frame["parent_tool_use_id"].as_str() {
            self.by_launch
                .entry(id.into())
                .or_insert_with(|| task.into());
        }
        if let Some(id) = frame["agent_id"].as_str() {
            self.by_agent.insert(id.into(), task.into());
        }
        self.remember_tools(frame, Some(task));
        let Some(child) = self.children.get_mut(task) else {
            return;
        };
        child.transcript.frame(frame, now_ms);
    }

    fn replay(&mut self) {
        // Replaying an outer child can reveal nested launches whose task_started
        // frame has already arrived. Keep native ordering within each transcript.
        for pending in std::mem::take(&mut self.pending) {
            if let Some(task) = self.resolve(&pending.frame) {
                self.pending_bytes -= pending.bytes;
                self.child_frame(&task, &pending.frame, pending.received_at_ms);
            } else {
                self.pending.push(pending);
            }
        }
    }

    fn refresh_metadata(&mut self) {
        let mut changed = Vec::new();
        for (task, child) in &mut self.children {
            let Some(launch) = child
                .launch_id
                .as_ref()
                .and_then(|id| self.launches.get(id))
            else {
                continue;
            };
            let owner = launch.owner.as_ref().map(|id| native_id(id));
            let prompt = launch.task.as_deref().filter(|s| !s.is_empty());
            if owner != child.info.parent_agent_id
                || (child.info.task.is_empty() && prompt.is_some())
            {
                child.info.parent_agent_id = owner;
                if child.info.task.is_empty() {
                    child.info.task = prompt.unwrap_or_default().into();
                }
                changed.push(task.clone());
            }
        }
        for task in changed {
            self.announce(&task);
        }
    }

    fn update(&mut self, frame: &Value) {
        let Some(task) = frame["task_id"].as_str() else {
            return;
        };
        let Some(child) = self.children.get_mut(task) else {
            return;
        };
        if let Some(launch) = frame["tool_use_id"].as_str() {
            if child
                .launch_id
                .as_deref()
                .is_some_and(|current| current != launch)
            {
                return;
            }
        }
        let previous = child.info.clone();
        let patch = if frame["subtype"] == "task_updated" {
            &frame["patch"]
        } else {
            frame
        };
        let status = match patch["status"].as_str() {
            Some("completed") => Some(AgentStatus::Completed),
            Some("failed") => Some(AgentStatus::Failed),
            Some("stopped" | "killed" | "cancelled") => Some(AgentStatus::Cancelled),
            Some("paused") => Some(AgentStatus::Waiting),
            Some("pending") => Some(AgentStatus::Queued),
            Some("running") => Some(AgentStatus::Running),
            _ => None,
        };
        if child.info.status.is_terminal() && status != Some(child.info.status) {
            return;
        }
        if let Some(status) = status {
            if status.is_terminal() && !child.info.status.is_terminal() {
                child.transcript.finish();
                child.info.capabilities = AgentCapabilities::default();
                child.info.activity.clear();
                if status != AgentStatus::Failed {
                    child.info.error = None;
                }
                self.stops.remove(task);
            }
            child.info.status = status;
            if let Some(summary) = frame["summary"].as_str() {
                if status == AgentStatus::Failed {
                    child.info.error = Some(summary.into());
                } else {
                    child.info.result = Some(summary.into());
                }
            }
        }
        if let Some(description) = frame["description"].as_str() {
            child.info.activity = description.into();
        }
        if let Some(tokens) = frame["usage"]["total_tokens"].as_u64() {
            child.info.total_tokens = Some(tokens);
        }
        if child.info != previous {
            self.announce(task);
        }
    }

    fn announce(&self, task: &str) {
        if let Some(child) = self.children.get(task) {
            let _ = self.tx.send(EngineEvent::AgentUpdated {
                session_id: self.session_id.clone(),
                agent: Box::new(child.info.clone()),
            });
        }
    }

    pub(super) fn stop_request(&mut self, session_id: &str, agent_id: &str) -> Option<Value> {
        if session_id != self.session_id {
            return None;
        }
        let (task, child) = self
            .children
            .iter_mut()
            .find(|(_, child)| child.info.id == agent_id)?;
        if child.info.status.is_terminal()
            || !child.info.capabilities.stop
            || self.stops.contains_key(task)
        {
            return None;
        }
        let control_id = format!("smelt-stop-task-{}", crate::agent::next_request_id());
        let request =
            super::transport::control(&control_id, json!({"subtype":"stop_task", "task_id":task}));
        self.stops.insert(
            task.clone(),
            Stop {
                control_id,
                deadline: Instant::now() + Duration::from_secs(30),
            },
        );
        child.info.activity = "Stop requested; waiting for Claude Code".into();
        child.info.error = None;
        let task = task.clone();
        self.announce(&task);
        Some(request)
    }

    fn stop_response(&mut self, frame: &Value) -> bool {
        let Some(id) = frame["response"]["request_id"].as_str() else {
            return false;
        };
        let Some(task) = self
            .stops
            .iter()
            .find_map(|(task, stop)| (stop.control_id == id).then(|| task.clone()))
        else {
            return false;
        };
        if frame["response"]["subtype"] == "error" {
            self.stops.remove(&task);
            if let Some(child) = self.children.get_mut(&task) {
                child.info.error = Some(format!(
                    "Claude Code could not stop this agent: {}",
                    frame["response"]["error"]
                        .as_str()
                        .unwrap_or("unknown control error")
                ));
                child.info.activity.clear();
                self.announce(&task);
            }
        }
        // A successful control response is not a terminal task outcome.
        true
    }

    pub(super) fn stop_deadline(&self) -> Option<Instant> {
        self.stops.values().map(|stop| stop.deadline).min()
    }

    pub(super) fn expire_stops(&mut self) {
        let expired: Vec<_> = self
            .stops
            .iter()
            .filter(|(_, stop)| stop.deadline <= Instant::now())
            .map(|(task, _)| task.clone())
            .collect();
        for task in expired {
            self.stops.remove(&task);
            if let Some(child) = self.children.get_mut(&task) {
                child.info.error = Some("Claude Code did not confirm this agent's stop. Its outcome is still unknown; retry or cancel the native session.".into());
                child.info.activity.clear();
                self.announce(&task);
            }
        }
    }

    pub(super) fn claim_tool(
        &mut self,
        root: &mut Transcript,
        name: &str,
        args: &HashMap<String, Value>,
    ) -> Result<(String, protocol::InvocationId), String> {
        let mut matches: Vec<_> = root
            .matching_calls(name, args)
            .chain(
                self.children
                    .values()
                    .flat_map(|child| child.transcript.matching_calls(name, args)),
            )
            .take(2)
            .map(str::to_owned)
            .collect();
        match matches.len() {
            0 => Ok(root.claim_call(format!("smelt-mcp-{}", crate::agent::next_request_id()))),
            1 => {
                let id = matches.pop().expect("one matching call");
                Ok(self.transcript_for_call(&id).unwrap_or(root).claim_call(id))
            }
            _ => Err("Claude Code sent indistinguishable concurrent host-tool calls without a tool-use ID. Retry these calls one at a time so their replies can be attributed safely.".into()),
        }
    }

    pub(super) fn transcript_for_call(&mut self, call_id: &str) -> Option<&mut Transcript> {
        let task = self.tool_owners.get(call_id)?;
        self.children
            .get_mut(task)
            .map(|child| &mut child.transcript)
    }

    pub(super) fn has_live(&self) -> bool {
        self.children
            .values()
            .any(|child| !child.info.status.is_terminal())
    }

    pub(super) fn finish(&mut self, error: Option<&str>) {
        let tasks: Vec<_> = self
            .children
            .iter()
            .filter_map(|(task, child)| (!child.info.status.is_terminal()).then_some(task.clone()))
            .collect();
        for task in tasks {
            let child = self.children.get_mut(&task).expect("known child");
            child.transcript.finish();
            child.info.status = if error.is_some() {
                AgentStatus::Failed
            } else {
                AgentStatus::Cancelled
            };
            child.info.error = error.map(str::to_owned);
            child.info.capabilities = AgentCapabilities::default();
            self.announce(&task);
        }
    }
}

fn native_id(task: &str) -> String {
    format!("claude-code:{task}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineOutput, HostCallbacks};
    use serde_json::json;

    fn registry() -> (Agents, tokio::sync::mpsc::UnboundedReceiver<EngineOutput>) {
        let (tx, _, rx) = crate::output_channel(HostCallbacks::Disabled);
        (Agents::new(tx, "root", "claude-code", true), rx)
    }

    fn start(task: &str, launch: &str) -> Value {
        json!({"type":"system", "subtype":"task_started", "task_id":task,
            "tool_use_id":launch, "task_type":"local_agent", "description":task})
    }

    fn assistant(parent: Option<&str>, uuid: &str, content: Value) -> Value {
        json!({"type":"assistant", "uuid":uuid, "parent_tool_use_id":parent,
            "message":{"content":content}})
    }

    fn events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<EngineOutput>) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        while let Ok(output) = rx.try_recv() {
            if let EngineOutput::Event(event) = output {
                events.push(event);
            }
        }
        events
    }

    #[test]
    fn buffers_child_frames_and_resolves_nested_hierarchy_without_parent_output() {
        let (mut agents, mut rx) = registry();
        assert!(!agents.frame(&assistant(None, "launch", json!([
            {"type":"tool_use", "id":"outer-launch", "name":"Agent", "input":{"prompt":"Review"}}
        ])), 0).unwrap());
        let nested_launch = assistant(
            Some("outer-launch"),
            "nested-launch",
            json!([
                {"type":"tool_use", "id":"nested-launch", "name":"Agent", "input":{"prompt":"Nested review"}}
            ]),
        );
        assert!(agents.frame(&nested_launch, 1).unwrap());
        assert!(agents
            .frame(
                &assistant(
                    Some("nested-launch"),
                    "report",
                    json!([
                        {"type":"thinking", "thinking":"Nested reasoning"},
                        {"type":"text", "text":"Nested report"}
                    ])
                ),
                2
            )
            .unwrap());
        let buffered_bytes = agents.pending_bytes;
        agents.frame(&start("nested", "nested-launch"), 3).unwrap();
        assert!(agents.pending_bytes > 0 && agents.pending_bytes < buffered_bytes);
        let unresolved_bytes = agents.pending_bytes;
        agents
            .frame(&start("unrelated", "unrelated-launch"), 3)
            .unwrap();
        assert_eq!(agents.pending_bytes, unresolved_bytes);
        agents.frame(&start("outer", "outer-launch"), 4).unwrap();
        assert!(agents.pending.is_empty());
        assert_eq!(agents.pending_bytes, 0);
        let nested = &agents.children["nested"];
        assert_eq!(
            nested.info.parent_agent_id.as_deref(),
            Some("claude-code:outer")
        );
        assert_eq!(nested.transcript.committed.len(), 1);
        let output = events(&mut rx);
        assert!(output.iter().all(|event| matches!(
            event,
            EngineEvent::AgentUpdated { .. } | EngineEvent::AgentOutput { .. }
        )));
        assert!(output.iter().any(|event| matches!(event,
            EngineEvent::AgentOutput { agent_id, event, .. }
            if agent_id == "claude-code:nested" && matches!(event.as_ref(), EngineEvent::TextDelta { delta } if delta == "Nested report"))));
    }

    #[test]
    fn buffered_child_stream_and_final_message_are_projected_once() {
        let (mut agents, mut rx) = registry();
        for event in [
            json!({"type":"message_start", "message":{"id":"child-message"}}),
            json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"Child streamed report"}}),
        ] {
            agents
                .frame(
                    &json!({"type":"stream_event", "parent_tool_use_id":"launch", "event":event}),
                    0,
                )
                .unwrap();
        }
        agents
            .frame(
                &assistant(
                    Some("launch"),
                    "final",
                    json!([
                        {"type":"text", "text":"Child streamed report"}
                    ]),
                ),
                1,
            )
            .unwrap();
        agents.frame(&start("task", "launch"), 2).unwrap();
        agents.frame(&json!({"type":"system", "subtype":"task_notification", "task_id":"task", "status":"completed"}), 3).unwrap();
        assert_eq!(agents.children["task"].transcript.committed.len(), 1);
        let output = events(&mut rx);
        assert!(output.iter().all(|event| matches!(
            event,
            EngineEvent::AgentOutput { .. } | EngineEvent::AgentUpdated { .. }
        )));
        assert_eq!(output.iter().filter(|event| matches!(event,
            EngineEvent::AgentOutput { event, .. } if matches!(event.as_ref(), EngineEvent::TextDelta { delta } if delta == "Child streamed report"))).count(), 1);
    }

    #[test]
    fn repeated_telemetry_only_publishes_changed_observations() {
        let (mut agents, mut rx) = registry();
        agents.frame(&start("task", "launch"), 0).unwrap();
        let progress = json!({"type":"system", "subtype":"task_progress", "task_id":"task", "description":"Reading", "usage":{"total_tokens":7300}});
        let completed = json!({"type":"system", "subtype":"task_updated", "task_id":"task", "patch":{"status":"completed"}});
        let summary = json!({"type":"system", "subtype":"task_notification", "task_id":"task", "status":"completed", "summary":"Done"});
        for frame in [progress, completed, summary] {
            for _ in 0..3 {
                agents.frame(&frame, 1).unwrap();
            }
        }
        assert_eq!(
            events(&mut rx)
                .iter()
                .filter(|event| matches!(event, EngineEvent::AgentUpdated { .. }))
                .count(),
            4
        );
        assert_eq!(agents.children["task"].info.result.as_deref(), Some("Done"));
        assert_eq!(agents.children["task"].info.total_tokens, Some(7300));
    }

    #[test]
    fn late_launch_metadata_updates_an_already_announced_nested_agent() {
        let (mut agents, _) = registry();
        agents.frame(&start("outer", "outer-launch"), 0).unwrap();
        agents.frame(&start("inner", "inner-launch"), 1).unwrap();
        assert!(agents.children["inner"].info.parent_agent_id.is_none());
        agents.frame(&assistant(Some("outer-launch"), "late-launch", json!([
            {"type":"tool_use", "id":"inner-launch", "name":"Agent", "input":{"prompt":"Late nested prompt"}}
        ])), 2).unwrap();
        assert_eq!(
            agents.children["inner"].info.parent_agent_id.as_deref(),
            Some("claude-code:outer")
        );
        assert_eq!(agents.children["inner"].info.task, "Late nested prompt");
    }

    #[test]
    fn terminal_patches_are_authoritative_and_late_progress_does_not_reopen() {
        let (mut agents, mut rx) = registry();
        agents.frame(&start("task", "launch"), 0).unwrap();
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_progress", "task_id":"task",
            "description":"Reading", "usage":{"total_tokens":7300}}),
                1,
            )
            .unwrap();
        assert_eq!(agents.children["task"].info.total_tokens, Some(7300));
        assert!(agents.children["task"].info.usage.is_none());
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_updated", "task_id":"task",
            "patch":{"status":"killed"}}),
                2,
            )
            .unwrap();
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_progress", "task_id":"task",
            "description":"Stale progress"}),
                3,
            )
            .unwrap();
        agents.frame(&start("task", "launch"), 4).unwrap();
        assert_eq!(agents.children["task"].info.status, AgentStatus::Cancelled);
        assert_eq!(agents.children["task"].info.generation, 1);
        let output = events(&mut rx);
        assert_eq!(
            output
                .iter()
                .filter(|event| matches!(event,
            EngineEvent::AgentUpdated { agent, .. } if agent.status == AgentStatus::Running))
                .count(),
            2
        );
    }

    #[test]
    fn resumed_assignment_preserves_identity_history_and_original_frame_alias() {
        let (mut agents, mut rx) = registry();
        agents.frame(&start("task", "original-launch"), 0).unwrap();
        agents
            .frame(
                &assistant(
                    Some("original-launch"),
                    "first",
                    json!([
                        {"type":"text", "text":"First report"}
                    ]),
                ),
                1,
            )
            .unwrap();
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_notification", "task_id":"task",
            "tool_use_id":"original-launch", "status":"completed", "summary":"Done"}),
                2,
            )
            .unwrap();
        agents.frame(&start("task", "resume-launch"), 3).unwrap();
        agents
            .frame(
                &assistant(
                    Some("original-launch"),
                    "second",
                    json!([
                        {"type":"text", "text":"Second report"}
                    ]),
                ),
                4,
            )
            .unwrap();
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_notification", "task_id":"task",
            "tool_use_id":"original-launch", "status":"failed", "summary":"Stale outcome"}),
                5,
            )
            .unwrap();
        let child = &agents.children["task"];
        assert_eq!(child.info.generation, 2);
        assert_eq!(child.info.status, AgentStatus::Running);
        assert_eq!(child.transcript.committed.len(), 2);
        assert!(events(&mut rx).iter().any(|event| matches!(event,
            EngineEvent::AgentOutput { generation:2, event, .. }
            if matches!(event.as_ref(), EngineEvent::HistoryAppended { delta, .. } if delta.first_index.get() == 1))));
    }

    #[test]
    fn host_tools_are_claimed_from_their_child_and_ambiguous_calls_fail_closed() {
        let (mut agents, _) = registry();
        let mut root = Transcript::new(agents.tx.clone(), "claude-code".into(), 1, 0);
        agents.frame(&start("one", "launch-one"), 0).unwrap();
        agents.frame(&start("two", "launch-two"), 0).unwrap();
        let args: HashMap<_, _> = [("text".into(), json!("same"))].into();
        for child in ["one", "two"] {
            agents.frame(&assistant(Some(&format!("launch-{child}")), child, json!([
                {"type":"tool_use", "id":format!("call-{child}"), "name":"mcp__smelt__fixture_tool", "input":args}
            ])), 1).unwrap();
        }
        assert!(agents
            .claim_tool(&mut root, "fixture_tool", &args)
            .unwrap_err()
            .contains("indistinguishable concurrent"));
        assert!(root.committed.is_empty());
        agents
            .transcript_for_call("call-one")
            .unwrap()
            .claim_call("call-one".into());
        let (call_id, invocation_id) = agents.claim_tool(&mut root, "fixture_tool", &args).unwrap();
        assert_eq!(call_id, "call-two");
        assert_eq!(
            invocation_id,
            agents
                .transcript_for_call("call-two")
                .unwrap()
                .invocation("call-two")
        );
        agents
            .transcript_for_call("call-two")
            .unwrap()
            .retain_result(
                "call-two",
                protocol::ToolOutcome::new(
                    "Rich child result".into(),
                    false,
                    Some(json!({"child":true})),
                ),
            );
        agents
            .frame(
                &json!({"type":"user", "parent_tool_use_id":"launch-two", "message":{"content":[
                    {"type":"tool_result", "tool_use_id":"call-two", "content":"Plain wire result"}
                ]}}),
                2,
            )
            .unwrap();
        let history = serde_json::to_string(&agents.children["two"].transcript.committed).unwrap();
        assert!(history.contains("Rich child result"));
        assert!(history.contains("\"child\":true"));
        assert!(root.committed.is_empty());
    }

    #[test]
    fn ignores_shell_tasks_and_bounds_unknown_child_frames() {
        let (mut agents, mut rx) = registry();
        let mut shell = start("shell", "bash-launch");
        shell["task_type"] = json!("local_bash");
        agents.frame(&shell, 0).unwrap();
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_notification", "task_id":"shell",
            "status":"completed"}),
                1,
            )
            .unwrap();
        assert!(agents.children.is_empty());
        assert!(events(&mut rx).is_empty());
        let frame = assistant(
            Some("unknown"),
            "unknown",
            json!([{ "type":"text", "text":"Unannounced" }]),
        );
        for _ in 0..MAX_PENDING_FRAMES {
            assert!(agents.frame(&frame, 0).unwrap());
        }
        assert!(agents
            .frame(&frame, 0)
            .unwrap_err()
            .contains("unannounced child transcript limit"));
        agents.finish(None);
    }

    #[test]
    fn stop_controls_are_scoped_and_acknowledgement_does_not_fabricate_an_outcome() {
        let (mut agents, mut rx) = registry();
        agents.frame(&start("one", "launch-one"), 0).unwrap();
        agents.frame(&start("two", "launch-two"), 0).unwrap();
        assert!(agents
            .stop_request("other-root", "claude-code:one")
            .is_none());
        assert!(agents.stop_request("root", "unknown").is_none());
        let request = agents.stop_request("root", "claude-code:one").unwrap();
        assert_eq!(request["request"]["subtype"], "stop_task");
        assert_eq!(request["request"]["task_id"], "one");
        assert!(agents.stop_request("root", "claude-code:one").is_none());
        agents
            .frame(
                &json!({"type":"control_response", "response":{"subtype":"success",
            "request_id":request["request_id"], "response":{}}}),
                1,
            )
            .unwrap();
        assert_eq!(agents.children["one"].info.status, AgentStatus::Running);
        assert_eq!(agents.children["two"].info.status, AgentStatus::Running);
        agents.stops.get_mut("one").unwrap().deadline = Instant::now();
        agents.expire_stops();
        assert_eq!(agents.children["one"].info.status, AgentStatus::Running);
        assert!(agents.children["one"]
            .info
            .error
            .as_deref()
            .unwrap()
            .contains("outcome is still unknown"));
        let retry = agents.stop_request("root", "claude-code:one").unwrap();
        agents
            .frame(
                &json!({"type":"control_response", "response":{"subtype":"error",
            "request_id":retry["request_id"], "error":"Unsupported task control"}}),
                2,
            )
            .unwrap();
        assert!(agents.children["one"]
            .info
            .error
            .as_deref()
            .unwrap()
            .contains("Unsupported task control"));
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_updated", "task_id":"one",
            "patch":{"status":"killed"}}),
                3,
            )
            .unwrap();
        assert_eq!(agents.children["one"].info.status, AgentStatus::Cancelled);
        assert!(!agents.children["one"].info.capabilities.stop);
        assert!(agents.stop_request("root", "claude-code:one").is_none());
        assert!(events(&mut rx).iter().all(|event| matches!(
            event,
            EngineEvent::AgentUpdated { .. } | EngineEvent::AgentOutput { .. }
        )));
    }

    #[test]
    fn transport_failure_terminates_only_live_records_and_preserves_known_outcomes() {
        let (mut agents, _) = registry();
        agents.frame(&start("done", "done-launch"), 0).unwrap();
        agents.frame(&start("live", "live-launch"), 0).unwrap();
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_notification", "task_id":"done",
            "status":"completed", "summary":"Known outcome"}),
                1,
            )
            .unwrap();
        agents.finish(Some("Transport failed"));
        assert_eq!(agents.children["done"].info.status, AgentStatus::Completed);
        assert_eq!(
            agents.children["done"].info.result.as_deref(),
            Some("Known outcome")
        );
        assert_eq!(agents.children["live"].info.status, AgentStatus::Failed);
        assert_eq!(
            agents.children["live"].info.error.as_deref(),
            Some("Transport failed")
        );
    }
}
