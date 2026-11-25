//! Read-only projections of Claude-owned tasks. Execution stays in the native session.

use super::native_id;
use super::transcript::Transcript;
use crate::EngineEventSender;
use protocol::{
    AgentCapabilities, AgentInfo, AgentMessageEvent, AgentOwner, AgentStatus, EngineEvent,
    HistoryItem, HistoryNote, ToolOutcome,
};
use serde_json::{json, Value};
use std::collections::HashMap;

const MAX_PENDING_FRAMES: usize = 1024;
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

struct Launch {
    owner: Option<String>,
    title: Option<String>,
    task: Option<String>,
}

struct Child {
    info: AgentInfo,
    transcript: Transcript,
    /// Whether the end of its background work was reported to its owner.
    reported: bool,
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
    /// What agents add to the main agent's transcript.
    deliveries: Vec<Delivery>,
}

/// What an agent adds to the transcript of the agent that launched it.
#[derive(Debug, PartialEq)]
pub(super) enum Delivery {
    /// A background agent's report, when it ends.
    Report(HistoryNote),
    /// What a waited-for agent works on, as a line of the call that waits.
    Activity { call_id: String, line: String },
}

impl Delivery {
    pub(super) fn apply(self, transcript: &mut Transcript) {
        match self {
            Self::Report(note) => transcript.note(HistoryItem::note(note)),
            Self::Activity { call_id, line } => transcript.printed(&call_id, vec![line]),
        }
    }
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
            deliveries: Vec::new(),
        }
    }

    /// What agents added to the main agent's transcript since the last call.
    pub(super) fn deliveries(&mut self) -> Vec<Delivery> {
        std::mem::take(&mut self.deliveries)
    }

    /// Returns true for child frames and task telemetry, including buffered frames.
    pub(super) fn frame(&mut self, frame: &Value, now_ms: u64) -> Result<bool, String> {
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
            .map(|id| native_id(&self.provider, id));
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
                if launch_id.is_none() || launch_id == child.info.call_id.as_deref() {
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
            id: native_id(&self.provider, task),
            ephemeral: self.ephemeral,
            generation,
            parent_agent_id: parent,
            owner: AgentOwner::ClaudeCode,
            title,
            task: prompt,
            status: AgentStatus::Running,
            // Claude Code registers a task in the background unless it says otherwise.
            background: frame["is_backgrounded"] != false,
            call_id: launch_id.map(str::to_owned),
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
                transcript,
                reported: false,
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
                .info
                .call_id
                .as_ref()
                .and_then(|id| self.launches.get(id))
            else {
                continue;
            };
            let owner = launch
                .owner
                .as_ref()
                .map(|id| native_id(&self.provider, id));
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
                .info
                .call_id
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
        if patch["is_backgrounded"] == true {
            child.info.background = true;
        }
        let mut deliveries = Vec::new();
        let launch_id = child.info.call_id.clone();
        if let Some(description) = frame["description"].as_str() {
            if let Some(call_id) = launch_id.clone().filter(|_| {
                frame["subtype"] == "task_progress"
                    && !child.info.background
                    && child.info.activity != description
            }) {
                deliveries.push(Delivery::Activity {
                    call_id,
                    line: description.to_owned(),
                });
            }
            child.info.activity = description.into();
        }
        if let Some(tokens) = frame["usage"]["total_tokens"].as_u64() {
            child.info.total_tokens = Some(tokens);
        }
        // Claude Code reports a task's end in its notification, after the
        // status. Only background work reports to its owner: a foreground
        // agent's report is the result of the call that waits for it.
        if frame["subtype"] == "task_notification"
            && child.info.background
            && child.info.status.is_terminal()
            && !child.reported
        {
            child.reported = true;
            deliveries.push(Delivery::Report(HistoryNote::agent_message(
                AgentMessageEvent::BackgroundAgentEnded {
                    agent_id: child.info.id.clone(),
                    title: child.info.title.clone(),
                    status: child.info.status,
                },
                child
                    .info
                    .error
                    .as_deref()
                    .or(child.info.result.as_deref())
                    .unwrap_or_default(),
            )));
        }
        if child.info != previous {
            self.announce(task);
        }
        let owner = launch_id
            .as_ref()
            .and_then(|id| self.launches.get(id))
            .and_then(|launch| launch.owner.clone());
        for delivery in deliveries {
            self.deliver(owner.as_deref(), delivery);
        }
    }

    /// Adds `delivery` to the transcript of agent `owner`, or of the main
    /// agent when there is no owner.
    fn deliver(&mut self, owner: Option<&str>, delivery: Delivery) {
        match owner {
            None => self.deliveries.push(delivery),
            Some(owner) => {
                if let Some(child) = self.children.get_mut(owner) {
                    delivery.apply(&mut child.transcript);
                }
            }
        }
    }

    /// The request that moves the main agent's foreground agent of call
    /// `call_id` to the background, so it stops waiting for it.
    pub(super) fn background_request(&self, call_id: &str) -> Option<Value> {
        self.children
            .values()
            .any(|child| {
                !child.info.status.is_terminal()
                    && !child.info.background
                    && child.info.call_id.as_deref() == Some(call_id)
                    && self
                        .launches
                        .get(call_id)
                        .is_some_and(|launch| launch.owner.is_none())
            })
            .then(|| json!({"subtype":"background_tasks", "tool_use_id":call_id}))
    }

    fn announce(&self, task: &str) {
        if let Some(child) = self.children.get(task) {
            let _ = self.tx.send(EngineEvent::AgentUpdated {
                session_id: self.session_id.clone(),
                agent: Box::new(child.info.clone()),
            });
        }
    }

    /// The request that stops agent `agent_id`, unless it ended or cannot
    /// stop.
    pub(super) fn stop_request(&self, session_id: &str, agent_id: &str) -> Option<Value> {
        if session_id != self.session_id {
            return None;
        }
        let (task, child) = self
            .children
            .iter()
            .find(|(_, child)| child.info.id == agent_id)?;
        (!child.info.status.is_terminal() && child.info.capabilities.stop)
            .then(|| json!({"subtype":"stop_task", "task_id":task}))
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

    /// Whether a child agent made the call `call_id`.
    pub(super) fn owns_call(&self, call_id: &str) -> bool {
        self.tool_owners.contains_key(call_id)
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

/// Whether Claude Code's tool `name` launches an agent.
pub(super) fn launches(name: &str) -> bool {
    matches!(name, "Agent" | "Task")
}

/// The result of a call that waited for the agent it launched, read from
/// Claude Code's structured result: the agent's report. The model keeps
/// `content`.
pub(super) fn launch_result(content: &str, result: &Value) -> Option<ToolOutcome> {
    let report = result["content"]
        .as_array()?
        .iter()
        .filter_map(|block| block["text"].as_str())
        .filter(|text| !text.trim_start().starts_with("<tools-used-before-summary>"))
        .collect::<Vec<_>>()
        .join("\n");
    let metadata: serde_json::Map<_, _> = [
        ("totalTokens", "tokens"),
        ("totalToolUseCount", "tool_uses"),
    ]
    .into_iter()
    .filter_map(|(key, name)| Some((name.to_owned(), result[key].as_u64()?.into())))
    .collect();
    ToolOutcome::try_from_parts(
        content.into(),
        false,
        (!metadata.is_empty()).then_some(Value::Object(metadata)),
        vec![protocol::ToolDisplayContent::new(
            "report",
            report.trim().to_owned(),
        )],
        None,
    )
    .ok()
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
    fn stop_requests_are_scoped_and_only_a_task_update_ends_the_agent() {
        let (mut agents, _) = registry();
        agents.frame(&start("one", "launch-one"), 0).unwrap();
        agents.frame(&start("two", "launch-two"), 0).unwrap();
        assert!(agents
            .stop_request("other-root", "claude-code:one")
            .is_none());
        assert!(agents.stop_request("root", "unknown").is_none());
        let request = agents.stop_request("root", "claude-code:one").unwrap();
        assert_eq!(request, json!({"subtype":"stop_task", "task_id":"one"}));
        assert_eq!(agents.children["one"].info.status, AgentStatus::Running);
        assert_eq!(agents.children["two"].info.status, AgentStatus::Running);
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_updated", "task_id":"one",
            "patch":{"status":"killed"}}),
                1,
            )
            .unwrap();
        assert_eq!(agents.children["one"].info.status, AgentStatus::Cancelled);
        assert!(!agents.children["one"].info.capabilities.stop);
        assert!(agents.stop_request("root", "claude-code:one").is_none());
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

    fn launch(agents: &mut Agents, launch: &str) {
        agents
            .frame(
                &assistant(None, launch, json!([
                    {"type":"tool_use", "id":launch, "name":"Agent", "input":{"description":"Review", "prompt":"Review"}}
                ])),
                0,
            )
            .unwrap();
    }

    fn started(task: &str, launch: &str, background: bool) -> Value {
        json!({"type":"system", "subtype":"task_started", "task_id":task, "tool_use_id":launch,
            "task_type":"local_agent", "description":"Review", "is_backgrounded":background})
    }

    fn progress(task: &str, description: &str) -> Value {
        json!({"type":"system", "subtype":"task_progress", "task_id":task, "description":description})
    }

    fn notified(task: &str, summary: &str) -> Value {
        json!({"type":"system", "subtype":"task_notification", "task_id":task,
            "status":"completed", "summary":summary})
    }

    #[test]
    fn a_waited_for_agent_reports_its_activity_to_the_waiting_call() {
        let (mut agents, _rx) = registry();
        launch(&mut agents, "call");
        agents.frame(&started("task", "call", false), 1).unwrap();
        agents.frame(&progress("task", "Reading files"), 2).unwrap();
        agents.frame(&progress("task", "Reading files"), 3).unwrap();
        agents.frame(&progress("task", "Running tests"), 4).unwrap();
        let activity = |line: &str| Delivery::Activity {
            call_id: "call".into(),
            line: line.into(),
        };
        assert_eq!(
            agents.deliveries(),
            [activity("Reading files"), activity("Running tests")]
        );
        // Its report is the call's result, not a message of its own.
        agents.frame(&notified("task", "Done"), 5).unwrap();
        assert!(agents.deliveries().is_empty());
    }

    #[test]
    fn a_background_agent_reports_its_end_once() {
        let (mut agents, _rx) = registry();
        launch(&mut agents, "call");
        agents.frame(&started("task", "call", true), 1).unwrap();
        agents.frame(&progress("task", "Reading files"), 2).unwrap();
        assert!(agents.deliveries().is_empty());
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_updated", "task_id":"task",
            "patch":{"status":"completed"}}),
                3,
            )
            .unwrap();
        assert!(
            agents.deliveries().is_empty(),
            "the notification carries the report"
        );
        agents.frame(&notified("task", "All good"), 4).unwrap();
        agents.frame(&notified("task", "All good"), 5).unwrap();
        let [Delivery::Report(report)] = &agents.deliveries()[..] else {
            panic!("one report");
        };
        assert_eq!(report.text(), "background agent Review finished");
        assert_eq!(report.agent_message_body(), Some("All good"));
    }

    #[test]
    fn moving_an_agent_to_the_background_makes_it_report() {
        let (mut agents, _rx) = registry();
        launch(&mut agents, "call");
        agents.frame(&started("task", "call", false), 1).unwrap();
        assert_eq!(
            agents.background_request("call"),
            Some(json!({"subtype":"background_tasks", "tool_use_id":"call"}))
        );
        agents
            .frame(
                &json!({"type":"system", "subtype":"task_updated", "task_id":"task",
            "patch":{"is_backgrounded":true}}),
                2,
            )
            .unwrap();
        assert!(agents.children["task"].info.background);
        assert!(agents.background_request("call").is_none());
        agents.frame(&notified("task", "Late report"), 3).unwrap();
        assert!(matches!(&agents.deliveries()[..], [Delivery::Report(_)]));
    }

    #[test]
    fn only_the_main_agents_waits_move_to_the_background() {
        let (mut agents, _rx) = registry();
        launch(&mut agents, "outer-call");
        agents
            .frame(&started("outer", "outer-call", true), 1)
            .unwrap();
        agents
            .frame(
                &assistant(Some("outer-call"), "nested", json!([
                    {"type":"tool_use", "id":"nested-call", "name":"Agent", "input":{"prompt":"Nested"}}
                ])),
                2,
            )
            .unwrap();
        agents
            .frame(&started("nested", "nested-call", false), 3)
            .unwrap();
        assert!(agents.background_request("nested-call").is_none());
    }

    #[test]
    fn a_foreground_report_shows_the_report_without_claude_codes_frame() {
        let content = "[Subagent hand-back] The report follows:\n  42\nagentId: a83\n<usage>subagent_tokens: 20130</usage>";
        let structured = json!({"status":"completed", "agentId":"a83", "totalTokens":20130, "totalToolUseCount":1,
            "content":[{"type":"text", "text":"<tools-used-before-summary>Bash</tools-used-before-summary>\n"},
                {"type":"text", "text":"The command printed:\n```\n42\n```"}]});
        let result = launch_result(content, &structured).unwrap();
        assert_eq!(result.content, content, "the model keeps the whole result");
        assert_eq!(result.display_content[0].name, "report");
        assert_eq!(
            result.display_content[0].content.as_str(),
            "The command printed:\n```\n42\n```"
        );
        assert_eq!(
            result.metadata,
            Some(json!({"tokens":20130, "tool_uses":1}))
        );
    }
}
