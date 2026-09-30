//! Runtime-owned child hosts. Lua supplies tasks and presentation, while each
//! child has independent engine commands, permissions, cwd, usage and history.

mod archive;

use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use crate::{
    lua::{LuaRuntime, TaskDriveOutput, ToolExecResult},
    Core,
};
use protocol::{EngineEvent, UiCommand};

static NEXT_AGENT: AtomicU64 = AtomicU64::new(1);
pub const DEFAULT_MAX_CONCURRENT: usize = 16;
pub const MAX_PENDING: usize = 64;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AgentOptions {
    pub max_concurrent: usize,
    pub max_cost_usd: Option<f64>,
    pub max_tokens: Option<u64>,
    pub max_requests: Option<u64>,
    pub compact_at_tokens: Option<u32>,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            max_cost_usd: None,
            max_tokens: None,
            max_requests: None,
            compact_at_tokens: None,
        }
    }
}

impl AgentOptions {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_PENDING).contains(&self.max_concurrent) {
            return Err("max_concurrent must be an integer between 1 and 64".into());
        }
        if self
            .max_cost_usd
            .is_some_and(|n| !n.is_finite() || n <= 0.0)
            || self.max_tokens == Some(0)
            || self.max_requests == Some(0)
            || self.compact_at_tokens == Some(0)
        {
            return Err("subagent budgets and compaction thresholds must be positive".into());
        }
        Ok(())
    }
}

fn agent_name(id: u64, session_id: &str) -> String {
    const NAMES: &[&str] = &[
        "alder", "birch", "cedar", "elm", "fir", "hazel", "juniper", "maple", "oak", "pine",
        "rowan", "spruce", "willow", "aspen", "beech", "larch",
    ];
    let index = crate::utils::hash_serializable(&session_id) as usize % NAMES.len();
    format!("{}-{id}", NAMES[index])
}

fn task_title(task: &str) -> String {
    let text = task.split_whitespace().collect::<Vec<_>>().join(" ");
    text.chars().take(80).collect()
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentInfo {
    pub id: u64,
    pub name: String,
    pub activity: String,
    pub started_at_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub requests: u64,
    pub persistence_error: Option<String>,
    pub group: u64,
    pub session_id: String,
    pub parent_id: String,
    pub task: String,
    pub status: String,
    pub result: String,
    pub error: Option<String>,
    pub cost_usd: f64,
    pub usage: protocol::TokenUsage,
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct AgentCardInfo {
    pub id: u64,
    pub session_id: String,
    pub status: String,
    pub activity: String,
    pub started_at_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub cost_usd: f64,
    pub persistence_error: Option<String>,
}

impl From<&AgentInfo> for AgentCardInfo {
    fn from(info: &AgentInfo) -> Self {
        Self {
            id: info.id,
            session_id: info.session_id.clone(),
            status: info.status.clone(),
            activity: info.activity.clone(),
            started_at_ms: info.started_at_ms,
            elapsed_ms: info.elapsed_ms,
            cost_usd: info.cost_usd,
            persistence_error: info.persistence_error.clone(),
        }
    }
}

pub(crate) struct AgentCard {
    info: AgentCardInfo,
    completion: tokio::sync::watch::Receiver<Option<AgentInfo>>,
}

impl AgentCard {
    pub fn snapshot(&self) -> (AgentCardInfo, bool) {
        match self.completion.borrow().as_ref() {
            Some(info) => (AgentCardInfo::from(info), false),
            None => (self.info.clone(), true),
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct AgentOutput {
    id: u64,
    status: String,
    output: String,
    error: Option<String>,
}

pub struct Child {
    pub info: AgentInfo,
    pub session: crate::session::Session,
    inherited_history_len: usize,
    execution: Option<ChildExecution>,
    completion: tokio::sync::watch::Sender<Option<AgentInfo>>,
    pub streaming_text: String,
    pub streaming_reasoning: String,
    pub live_tools: Vec<(
        protocol::InvocationId,
        crate::Block,
        crate::transcript_model::ToolState,
    )>,
    pub revision: u64,
    pub history_revision: u64,
    resume: Option<ChildLaunch>,
    blocker: Option<String>,
    stopped_for_budget: bool,
    compaction: Option<PendingCompaction>,
    archive: Option<archive::AgentArchive>,
}

struct PendingCompaction {
    id: u64,
    reply: tokio::sync::oneshot::Sender<engine::host::HostRequestDecision>,
    first_live_index: usize,
    tokens_before: Option<u32>,
}

enum ChildExecution {
    Queued(Box<ChildLaunch>),
    Running(RunningChild),
}

#[derive(Clone)]
struct ChildLaunch {
    snapshot: Arc<engine::fork::ForkSnapshot>,
    input: String,
    config: crate::runtime_state::RuntimeState,
    permissions: crate::permissions::PermissionsHandle,
    env: Arc<engine::env::RuntimeEnv>,
    lua_generation: u64,
    startup_overrides: crate::StartupOverrides,
    skills: Option<Arc<engine::SkillLoader>>,
    mcp: Option<Arc<crate::mcp::McpManager>>,
}

struct RunningChild {
    core: Box<Core>,
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        self.core.engine.send(UiCommand::Cancel);
        self.core.jobs.clear();
    }
}

impl Child {
    pub(crate) fn core_mut(&mut self) -> Option<&mut Core> {
        match self.execution.as_mut()? {
            ChildExecution::Running(run) => Some(&mut run.core),
            ChildExecution::Queued(_) => None,
        }
    }

    fn core(&self) -> Option<&Core> {
        match self.execution.as_ref()? {
            ChildExecution::Running(run) => Some(&run.core),
            ChildExecution::Queued(_) => None,
        }
    }

    fn release_execution(&mut self) {
        self.execution = None;
        self.revision = self.revision.wrapping_add(1);
        self.compaction = None;
        self.info.elapsed_ms = self
            .info
            .started_at_ms
            .map_or(0, |start| crate::session::now_ms().saturating_sub(start));
        if let Some(archive) = self.archive.take() {
            archive.update(&self.session, &self.info, self.inherited_history_len, 0);
        } else {
            self.completion.send_replace(Some(self.info.clone()));
        }
    }

    pub fn info(&self) -> AgentInfo {
        let mut info = self
            .completion
            .borrow()
            .as_ref()
            .cloned()
            .unwrap_or_else(|| self.info.clone());
        if info.status == "running" {
            info.elapsed_ms = info
                .started_at_ms
                .map_or(0, |start| crate::session::now_ms().saturating_sub(start));
        }
        info
    }

    pub(crate) fn card(&self) -> AgentCard {
        AgentCard {
            info: AgentCardInfo::from(&self.info),
            completion: self.completion.subscribe(),
        }
    }

    pub fn inherited_history_len(&self) -> usize {
        self.inherited_history_len
    }
}

struct AgentRestore {
    records: tokio::sync::oneshot::Receiver<
        Result<Vec<(archive::SavedAgent, crate::session::Session)>, String>,
    >,
    ready: tokio::sync::watch::Receiver<Option<Result<(), String>>>,
}

#[derive(Default)]
pub struct Agents {
    pub children: BTreeMap<u64, Child>,
    options: AgentOptions,
    restoring: BTreeMap<String, AgentRestore>,
    restored: std::collections::BTreeSet<String>,
    parent_usage: BTreeMap<String, (protocol::TokenUsage, f64)>,
}

impl Agents {
    pub fn restore(
        &mut self,
        storage: &crate::session::SessionStorage,
        parent_id: &str,
    ) -> Result<bool, String> {
        if self.restored.contains(parent_id) {
            return Ok(true);
        }
        if let Some(receiver) = self.restoring.get_mut(parent_id) {
            match receiver.records.try_recv() {
                Ok(result) => {
                    self.restoring.remove(parent_id);
                    let restored = result?;
                    for (saved, _) in &restored {
                        NEXT_AGENT.fetch_max(saved.info.id.saturating_add(1), Ordering::Relaxed);
                    }
                    let mut session_ids: std::collections::HashSet<_> = self
                        .children
                        .values()
                        .map(|child| child.info.session_id.clone())
                        .collect();
                    for (mut saved, session) in restored {
                        if !session_ids.insert(saved.info.session_id.clone()) {
                            continue;
                        }
                        if self.children.contains_key(&saved.info.id) {
                            saved.info.id = NEXT_AGENT.fetch_add(1, Ordering::Relaxed);
                        }
                        let completion = tokio::sync::watch::channel(Some(saved.info.clone())).0;
                        self.children.insert(
                            saved.info.id,
                            Child {
                                info: saved.info,
                                session,
                                inherited_history_len: saved.inherited_history_len,
                                execution: None,
                                completion,
                                streaming_text: String::new(),
                                streaming_reasoning: String::new(),
                                live_tools: Vec::new(),
                                revision: 0,
                                history_revision: 0,
                                resume: None,
                                blocker: None,
                                stopped_for_budget: false,
                                compaction: None,
                                archive: None,
                            },
                        );
                    }
                    self.restored.insert(parent_id.to_owned());
                    return Ok(true);
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return Ok(false),
                Err(error) => {
                    self.restoring.remove(parent_id);
                    return Err(error.to_string());
                }
            }
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return Err("subagent restoration requires an async runtime".into());
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let (ready, completion) = tokio::sync::watch::channel(None);
        self.restoring.insert(
            parent_id.to_owned(),
            AgentRestore {
                records: receiver,
                ready: completion,
            },
        );
        let storage = storage.clone();
        let parent_id = parent_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let result =
                archive::load(&storage, &parent_id, || sender.is_closed()).and_then(|saved| {
                    saved
                        .into_iter()
                        .map(|saved| {
                            if sender.is_closed() {
                                return Err("subagent restoration cancelled".into());
                            }
                            let session = storage
                                .load_full_result(&saved.info.session_id)
                                .map_err(|error| error.to_string())?
                                .ok_or_else(|| "subagent transcript is missing".to_owned())?;
                            Ok((saved, session))
                        })
                        .collect()
                });
            let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
            let _ = sender.send(result);
            ready.send_replace(Some(status));
        });
        Ok(false)
    }

    pub(crate) fn restoration_ready(
        &self,
        parent_id: &str,
    ) -> Option<tokio::sync::watch::Receiver<Option<Result<(), String>>>> {
        self.restoring
            .get(parent_id)
            .map(|restore| restore.ready.clone())
    }

    pub(crate) async fn flush(&self, parent_id: &str) -> Result<(), String> {
        let mut receivers: Vec<_> = self
            .children
            .values()
            .filter(|child| child.info.parent_id == parent_id)
            .map(|child| child.completion.subscribe())
            .collect();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut failure = None;
            for receiver in &mut receivers {
                let error = match receiver.wait_for(Option::is_some).await {
                    Ok(info) => info
                        .as_ref()
                        .expect("completed archive")
                        .persistence_error
                        .as_ref()
                        .map(|error| format!("subagent archive: {error}")),
                    Err(_) => Some("subagent archive worker stopped before completion".to_owned()),
                };
                failure = failure.or(error);
            }
            failure.map_or(Ok(()), Err)
        })
        .await
        .map_err(|_| "subagent archive flush deadline elapsed".to_owned())?
    }

    fn pending_count(&self) -> usize {
        self.children
            .values()
            .filter(|child| matches!(child.info.status.as_str(), "queued" | "running"))
            .count()
    }

    pub fn running_count(&self) -> usize {
        self.children
            .values()
            .filter(|child| child.info.status == "running")
            .count()
    }

    pub fn record_parent_usage(
        &mut self,
        parent_id: &str,
        usage: &protocol::TokenUsage,
        cost: Option<f64>,
    ) {
        let total = self.parent_usage.entry(parent_id.to_owned()).or_default();
        total.0.accumulate(usage);
        total.1 += cost.unwrap_or_default();
    }

    pub fn family_totals(&self, parent_id: &str) -> (protocol::TokenUsage, f64) {
        let (mut usage, mut cost) = self.totals(parent_id);
        if let Some((parent_usage, parent_cost)) = self.parent_usage.get(parent_id) {
            usage.accumulate(parent_usage);
            cost += parent_cost;
        }
        (usage, cost)
    }

    pub fn totals(&self, parent_id: &str) -> (protocol::TokenUsage, f64) {
        let mut usage = protocol::TokenUsage::default();
        let mut cost = 0.0;
        for child in self
            .children
            .values()
            .filter(|child| child.info.parent_id == parent_id)
        {
            usage.accumulate(&child.info.usage);
            cost += child.info.cost_usd;
        }
        (usage, cost)
    }

    pub fn resolve(&self, parent_id: &str, target: &serde_json::Value) -> Result<u64, String> {
        let id = self
            .children
            .values()
            .find(|child| {
                child.info.parent_id == parent_id
                    && (target.as_u64() == Some(child.info.id)
                        || target.as_str() == Some(child.info.name.as_str()))
            })
            .map(|child| child.info.id);
        id.ok_or_else(|| {
            format!(
                "unknown subagent: {}",
                target
                    .as_str()
                    .map_or_else(|| target.to_string(), str::to_owned)
            )
        })
    }

    fn budget_reason(&self, parent_id: &str) -> Option<String> {
        if self.options.max_cost_usd.is_none() && self.options.max_tokens.is_none() {
            return None;
        }
        let (_, cost) = self.family_totals(parent_id);
        let tokens: u64 = self
            .children
            .values()
            .filter(|child| child.info.parent_id == parent_id)
            .map(|child| &child.info.usage)
            .chain(self.parent_usage.get(parent_id).map(|(usage, _)| usage))
            .map(|usage| {
                [
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.cache_read_tokens,
                    usage.cache_write_tokens,
                ]
                .into_iter()
                .flatten()
                .map(u64::from)
                .sum::<u64>()
            })
            .sum();
        if self.options.max_cost_usd.is_some_and(|limit| cost >= limit) {
            Some("subagent cost budget reached".into())
        } else if self.options.max_tokens.is_some_and(|limit| tokens >= limit) {
            Some("subagent token budget reached".into())
        } else {
            None
        }
    }

    fn selected(&self, parent_id: &str, ids: &[u64]) -> Result<Vec<&Child>, String> {
        if ids.is_empty() || ids.len() > MAX_PENDING {
            return Err(format!("provide between 1 and {MAX_PENDING} subagent IDs"));
        }
        ids.iter()
            .map(|id| {
                self.children
                    .get(id)
                    .filter(|child| child.info.parent_id == parent_id)
                    .ok_or_else(|| format!("unknown subagent: {id}"))
            })
            .collect()
    }

    pub(crate) fn peek(&self, parent_id: &str, id: u64) -> Result<AgentOutput, String> {
        let selected = self.selected(parent_id, &[id])?;
        let child = selected[0];
        let history = child
            .session
            .history
            .get(child.inherited_history_len..)
            .unwrap_or_default();
        let messages = history.iter().filter_map(|item| match item {
            protocol::HistoryItem::Assistant(step) => {
                step.content.as_ref().map(protocol::Content::text_content)
            }
            _ => None,
        });
        let mut output = crate::output_limit::OutputLimiter::default();
        for (index, text) in messages
            .chain(std::iter::once(std::borrow::Cow::Borrowed(
                child.streaming_text.as_str(),
            )))
            .filter(|text| !text.is_empty())
            .enumerate()
        {
            if index > 0 {
                output.push_line(String::new());
            }
            output.push_text(&text);
        }
        Ok(AgentOutput {
            id,
            status: child.info.status.clone(),
            output: output.format_text_with_notice("[agent output truncated; showing tail]"),
            error: child.info.error.clone(),
        })
    }

    pub(crate) fn completions(
        &self,
        parent_id: &str,
        ids: &[u64],
    ) -> Result<Vec<tokio::sync::watch::Receiver<Option<AgentInfo>>>, String> {
        Ok(self
            .selected(parent_id, ids)?
            .into_iter()
            .map(|child| child.completion.subscribe())
            .collect())
    }
}

impl Core {
    pub(crate) fn evaluate_model_tool(
        &mut self,
        lua: &LuaRuntime,
        name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
    ) -> protocol::ToolEvaluation {
        let permissions = self.permissions.snapshot();
        let mode = self.config.mode.clone();
        crate::host::scope_core(self, || {
            lua.evaluate_tool_call(name, args, mode, &permissions)
        })
    }

    pub(crate) fn dispatch_model_tool(
        &mut self,
        lua: &LuaRuntime,
        session: &crate::session::Session,
        name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
        ids: crate::lua::ToolCallIds<'_>,
    ) {
        let mode = self.config.mode.clone();
        let artifact_dir = self.sessions.artifact_dir_for(session);
        let now = self.clock.instant_now();
        let result = crate::host::scope_core(self, || {
            lua.execute_tool(
                name,
                args,
                ids,
                crate::lua::ToolEnv {
                    mode,
                    session_id: &session.id,
                    artifact_dir: &artifact_dir,
                },
                now,
            )
        });
        if let ToolExecResult::Immediate {
            content,
            is_error,
            metadata,
            display_content,
            attachment,
        } = result
        {
            self.engine.send(UiCommand::ToolResult {
                request_id: ids.request_id,
                invocation_id: ids.invocation_id,
                call_id: ids.call_id.to_owned(),
                content,
                is_error,
                metadata,
                display_content,
                attachment: attachment.map(|value| *value),
            });
        }
    }

    pub fn configure_agents(&mut self, options: AgentOptions) -> Result<(), String> {
        options.validate()?;
        self.agents.options = options;
        Ok(())
    }

    pub fn abandon_agents(&mut self, parent_id: &str, reason: &str) {
        for child in self.agents.children.values_mut().filter(|child| {
            child.info.parent_id == parent_id
                && matches!(child.info.status.as_str(), "queued" | "running")
        }) {
            child.info.status = "cancelled".into();
            child.info.error = Some(reason.into());
            child.release_execution();
        }
    }

    pub fn cancel_agents_for(&mut self, parent_id: &str) {
        let ids: Vec<_> = self
            .agents
            .children
            .values()
            .filter(|child| child.info.parent_id == parent_id)
            .map(|child| child.info.id)
            .collect();
        for id in ids {
            let _ = self.cancel_agent(id);
        }
    }

    pub fn follow_up_agent(
        &mut self,
        parent_id: &str,
        id: u64,
        input: String,
    ) -> Result<AgentInfo, String> {
        if crate::lua::current_subagent().is_some() {
            return Err("subagents cannot assign follow-ups".into());
        }
        if input.trim().is_empty() {
            return Err("follow-up task cannot be empty".into());
        }
        if let Some(reason) = self.agents.budget_reason(parent_id) {
            return Err(reason);
        }
        if self.agents.pending_count() >= MAX_PENDING {
            return Err("subagent queue is full".into());
        }
        let child = self.agents.selected(parent_id, &[id])?[0];
        if child.completion.borrow().is_none() {
            return Err("wait for the subagent to finish before assigning a follow-up".into());
        }
        let session = &child.session;
        if session.id != child.info.session_id || session.parent_id.as_deref() != Some(parent_id) {
            return Err("subagent session identity mismatch".into());
        }
        let mut launch = match &child.resume {
            Some(launch) => launch.clone(),
            None => {
                let snapshot = self.engine.fork_snapshot().map_err(str::to_owned)?;
                let mut permissions = self.permissions.snapshot().fork_session();
                if let Some(overrides) = snapshot.permission_overrides() {
                    permissions = permissions.with_overrides(overrides);
                }
                ChildLaunch {
                    snapshot,
                    input: String::new(),
                    config: self.config.clone(),
                    permissions: crate::permissions::PermissionsHandle::new(permissions),
                    env: Arc::new(
                        self.env.fork(
                            session
                                .cwd
                                .as_ref()
                                .map(std::path::PathBuf::from)
                                .unwrap_or_else(|| self.env.cwd()),
                        ),
                    ),
                    lua_generation: self.lua_generation,
                    startup_overrides: self.startup_overrides.clone(),
                    skills: self.skills.clone(),
                    mcp: self.mcp.clone(),
                }
            }
        };
        let (prefix, start, _) = session.model_history_range();
        launch.snapshot = Arc::new(launch.snapshot.with_history(
            session.model_history(),
            protocol::ModelHistoryCoordinates::projected(prefix.len(), start),
        ));
        launch.input = input.clone();
        launch.lua_generation = self.lua_generation;
        let child = self.agents.children.get_mut(&id).expect("selected child");
        child
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(input)));
        child.info.status = "queued".into();
        child.info.error = None;
        child.info.result.clear();
        child.info.activity = "Follow-up queued".into();
        child.info.persistence_error = None;
        child.info.requests = 0;
        child.info.started_at_ms = None;
        child.info.elapsed_ms = 0;
        child.revision = child.revision.wrapping_add(1);
        child.history_revision = child.history_revision.wrapping_add(1);
        child.blocker = None;
        child.stopped_for_budget = false;
        child.completion.send_replace(None);
        let archive = archive::AgentArchive::new(self.sessions.clone(), child.completion.clone());
        archive.update(
            &child.session,
            &child.info,
            child.inherited_history_len,
            child.session.history.len().saturating_sub(1),
        );
        child.archive = Some(archive);
        child.execution = Some(ChildExecution::Queued(Box::new(launch)));
        self.start_queued_agents();
        Ok(self.agents.children[&id].info())
    }

    pub fn handle_agent_host_call(&mut self, id: u64, call: engine::HostCall) {
        let parent_id = match self.agents.children.get(&id) {
            Some(child) => child.info.parent_id.clone(),
            None => return,
        };
        let budget = self.agents.budget_reason(&parent_id);
        let options = self.agents.options.clone();
        let Some(child) = self.agents.children.get_mut(&id) else {
            return;
        };
        match call {
            engine::HostCall::ProviderResponse { reply, .. } => {
                let _ = reply.send(None);
            }
            engine::HostCall::RequestAudit {
                entry,
                payload_mode,
                ..
            } => {
                if let Some(archive) = &child.archive {
                    archive.audit(entry, payload_mode);
                }
            }
            engine::HostCall::PrepareRequest {
                estimated_tokens,
                reply,
                ..
            } => {
                if let Some(reason) = budget.or_else(|| {
                    options
                        .max_requests
                        .filter(|limit| child.info.requests >= *limit)
                        .map(|_| "subagent request budget reached".into())
                }) {
                    child.blocker = Some(reason);
                    child.stopped_for_budget = true;
                    let _ = reply.send(engine::host::HostRequestDecision::Stop);
                    return;
                }
                child.info.requests += 1;
                let threshold = options.compact_at_tokens.or_else(|| {
                    child
                        .resume
                        .as_ref()
                        .and_then(|launch| {
                            launch.config.context_window.or_else(|| {
                                launch
                                    .config
                                    .active_model()
                                    .and_then(|model| model.config.context_window)
                            })
                        })
                        .map(|window| window.saturating_mul(4) / 5)
                });
                let reply = if threshold.is_some_and(|limit| estimated_tokens >= limit) {
                    match Self::begin_agent_compaction(child, reply, Some(estimated_tokens)) {
                        Ok(()) => return,
                        Err(reply) => reply,
                    }
                } else {
                    reply
                };
                child.info.activity = "Waiting for provider".into();
                let _ = reply.send(engine::host::HostRequestDecision::Continue);
            }
            engine::HostCall::RecoverFromContextLimit { reply, .. } => {
                if let Err(reply) = Self::begin_agent_compaction(child, reply, None) {
                    let _ = reply.send(engine::host::HostRequestDecision::Abort(
                        "subagent context cannot be compacted further".into(),
                    ));
                }
            }
            engine::HostCall::Subagent { .. } => {}
        }
    }

    fn begin_agent_compaction(
        child: &mut Child,
        reply: tokio::sync::oneshot::Sender<engine::host::HostRequestDecision>,
        tokens_before: Option<u32>,
    ) -> Result<(), tokio::sync::oneshot::Sender<engine::host::HostRequestDecision>> {
        let Some(launch) = &child.resume else {
            return Err(reply);
        };
        let Some(core) = child.core() else {
            return Err(reply);
        };
        let first_live_index = child.session.history.len().saturating_sub(6);
        let previous = child
            .session
            .checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.first_live_index);
        if first_live_index <= previous + 1 {
            return Err(reply);
        }
        let (mut summary_history, start, _) = child.session.model_history_range();
        summary_history.extend(
            child.session.history[start..first_live_index]
                .iter()
                .cloned(),
        );
        let mut messages = protocol::history_to_messages(&summary_history);
        messages.push(protocol::Message::user(protocol::Content::text("Produce a concise context checkpoint for this subagent's assignment. Preserve its task, scope, constraints, findings, file changes, exact references, verification, blockers and next steps. Reply with the checkpoint only. Do not use tools.")));
        let id = crate::engine_client::next_agent_request_id();
        core.engine.send(UiCommand::EngineAsk {
            id,
            system: launch.snapshot.system_prompt().to_owned(),
            messages,
            target: Box::new(launch.snapshot.model_target().clone()),
            request_config: launch.snapshot.request_config(),
            response_format: None,
            reasoning_effort: protocol::ReasoningEffort::Off,
            fast_mode: false,
            tools: Vec::new(),
            session_id: child.session.id.clone(),
            persistence: protocol::PersistenceScope::default(),
            stream: false,
            visible_retries: false,
        });
        child.info.activity = "Compacting context".into();
        child.compaction = Some(PendingCompaction {
            id,
            reply,
            first_live_index,
            tokens_before,
        });
        Ok(())
    }

    pub fn cancel_agents(&mut self) {
        let ids: Vec<_> = self.agents.children.keys().copied().collect();
        for id in ids {
            let _ = self.cancel_agent(id);
        }
    }

    pub fn spawn_agents(
        &mut self,
        input: String,
        count: usize,
        task: Option<String>,
        max_concurrent: usize,
    ) -> Result<Vec<AgentInfo>, String> {
        let task = task_title(&task.unwrap_or_else(|| input.clone()));
        if crate::lua::current_subagent().is_some() {
            return Err("subagents cannot create subagents".into());
        }
        if input.trim().is_empty() || task.trim().is_empty() || !(1..=16).contains(&count) {
            return Err("provide a non-empty task and a count between 1 and 16".into());
        }
        if !(1..=MAX_PENDING).contains(&max_concurrent) {
            return Err(format!(
                "max_concurrent must be between 1 and {MAX_PENDING}"
            ));
        }
        self.agents.options.max_concurrent = max_concurrent;
        let pending = self.agents.pending_count();
        if pending + count > MAX_PENDING {
            return Err("subagent queue is full".into());
        }
        let snapshot = self.engine.fork_snapshot().map_err(str::to_owned)?;
        if !self
            .agents
            .restore(&self.sessions, snapshot.parent_session_id())?
        {
            return Err(
                "subagent session catalog restoration is in progress; retry when ready".into(),
            );
        }
        if let Some(reason) = self.agents.budget_reason(snapshot.parent_session_id()) {
            return Err(reason);
        }
        // Freeze one batch policy, then give every child its own session approvals.
        let mut policy = self.permissions.snapshot().fork_session();
        if let Some(overrides) = snapshot.permission_overrides() {
            policy = policy.with_overrides(overrides);
        }
        let group = NEXT_AGENT.fetch_add(count as u64, Ordering::Relaxed);
        let mut ids = Vec::with_capacity(count);
        for offset in 0..count {
            let id = group + offset as u64;
            let mut session =
                crate::session::Session::new(self.env.pid(), snapshot.cwd().to_owned());
            session.parent_id = Some(snapshot.parent_session_id().into());
            session.title = Some(task.clone());
            session.history = snapshot.history().to_vec();
            session
                .history
                .push(protocol::HistoryItem::user(protocol::Content::text(
                    input.clone(),
                )));
            let info = AgentInfo {
                id,
                name: agent_name(id, &session.id),
                activity: "Waiting for an execution slot".into(),
                started_at_ms: None,
                elapsed_ms: 0,
                requests: 0,
                persistence_error: None,
                group,
                session_id: session.id.clone(),
                parent_id: snapshot.parent_session_id().into(),
                task: task.clone(),
                status: "queued".into(),
                result: String::new(),
                error: None,
                cost_usd: 0.0,
                usage: protocol::TokenUsage::default(),
            };
            let completion = tokio::sync::watch::channel(None).0;
            let archive = archive::AgentArchive::new(self.sessions.clone(), completion.clone());
            archive.update(&session, &info, snapshot.history().len(), 0);
            self.agents.children.insert(
                id,
                Child {
                    info,
                    session,
                    inherited_history_len: snapshot.history().len(),
                    completion,
                    resume: None,
                    blocker: None,
                    stopped_for_budget: false,
                    compaction: None,
                    archive: Some(archive),
                    streaming_text: String::new(),
                    streaming_reasoning: String::new(),
                    live_tools: Vec::new(),
                    revision: 0,
                    history_revision: 0,
                    execution: Some(ChildExecution::Queued(Box::new(ChildLaunch {
                        snapshot: Arc::clone(&snapshot),
                        input: input.clone(),
                        config: self.config.clone(),
                        permissions: crate::permissions::PermissionsHandle::new(
                            policy.fork_session(),
                        ),
                        env: Arc::new(self.env.fork(snapshot.cwd().to_owned())),
                        lua_generation: self.lua_generation,
                        startup_overrides: self.startup_overrides.clone(),
                        skills: self.skills.clone(),
                        mcp: self.mcp.clone(),
                    }))),
                },
            );
            ids.push(id);
        }
        self.start_queued_agents();
        Ok(ids
            .into_iter()
            .map(|id| self.agents.children[&id].info.clone())
            .collect())
    }

    fn start_queued_agents(&mut self) {
        let mut running = self.agents.running_count();
        let queued: Vec<u64> = self
            .agents
            .children
            .iter()
            .filter(|(_, child)| child.info.status == "queued")
            .map(|(id, _)| *id)
            .collect();
        for id in queued {
            if running >= self.agents.options.max_concurrent {
                break;
            }
            let child = self
                .agents
                .children
                .get_mut(&id)
                .expect("queued child exists");
            let Some(ChildExecution::Queued(mut launch)) = child.execution.take() else {
                unreachable!("queued child owns launch state");
            };
            if launch.lua_generation != self.lua_generation {
                child.info.status = "cancelled".into();
                child.release_execution();
                continue;
            }
            child.resume = Some((*launch).clone());
            child.info.started_at_ms = Some(crate::session::now_ms());
            child.info.activity = "Starting response".into();
            match self.engine.start_fork(
                &launch.snapshot,
                id,
                child.session.id.clone(),
                launch.input,
            ) {
                Ok(handle) => {
                    launch.config.mode = launch.snapshot.mode();
                    let mut core = Core::new(
                        launch.config,
                        launch.startup_overrides,
                        handle,
                        self.frontend,
                        launch.permissions,
                        Arc::clone(&self.clock),
                        launch.env,
                    );
                    core.lua_generation = launch.lua_generation;
                    core.skills = launch.skills;
                    core.mcp = launch.mcp;
                    child.execution = Some(ChildExecution::Running(RunningChild {
                        core: Box::new(core),
                    }));
                    child.info.status = "running".into();
                    running += 1;
                }
                Err(error) => {
                    child.info.status = "failed".into();
                    child.info.error = Some(error.into());
                    child.release_execution();
                }
            }
        }
    }

    pub fn cancel_agent(&mut self, id: u64) -> Result<(), String> {
        let child = self
            .agents
            .children
            .get_mut(&id)
            .ok_or("unknown subagent")?;
        if child.info.status == "queued" {
            child.info.status = "cancelled".into();
            child.release_execution();
        } else if child.info.status == "running" {
            if let Some(core) = child.core() {
                core.engine.send(UiCommand::Cancel);
            }
        }
        Ok(())
    }

    /// Consume child output before any parent transcript, hooks or accounting.
    pub fn handle_agent_event(&mut self, lua: &LuaRuntime, id: u64, event: EngineEvent) {
        let Some(child) = self.agents.children.get_mut(&id) else {
            return;
        };
        let Some(ChildExecution::Running(run)) = child.execution.as_mut() else {
            return;
        };
        let core = &mut run.core;
        child.revision = child.revision.wrapping_add(1);
        if core.lua_generation != self.lua_generation
            && matches!(
                event,
                EngineEvent::ToolDispatch { .. }
                    | EngineEvent::ToolEvaluationRequest { .. }
                    | EngineEvent::CoreToolResult { .. }
            )
        {
            core.engine.send(UiCommand::Cancel);
            return;
        }
        let first_changed = match &event {
            EngineEvent::HistoryAppended { delta, .. }
            | EngineEvent::HistoryUpdated { update: delta, .. } => Some(delta.first_index.get()),
            EngineEvent::TokenUsage { .. } | EngineEvent::EngineAskResponse { .. } => {
                Some(child.session.history.len())
            }
            _ => None,
        };
        match event {
            EngineEvent::ToolEvaluationRequest {
                request_id,
                tool_name,
                args,
                ..
            } => {
                let evaluation = crate::lua::with_subagent(id, || {
                    core.evaluate_model_tool(lua, &tool_name, &args)
                });
                core.engine.send(UiCommand::ToolEvaluationResponse {
                    request_id,
                    evaluation,
                });
            }
            EngineEvent::RequestPermission { request_id, .. } => {
                child.blocker = Some(
                    "subagent requires explicit approval; delegate this operation to the parent"
                        .into(),
                );
                // No child request may inherit the parent's pending approval.
                core.engine.send(UiCommand::PermissionDecision { request_id, approved: false, message: Some("subagent requires explicit approval; delegate this operation to the parent".into()) });
            }
            EngineEvent::ToolDispatch {
                request_id,
                invocation_id,
                call_id,
                tool_name,
                args,
            } => {
                crate::lua::with_subagent(id, || {
                    core.dispatch_model_tool(
                        lua,
                        &child.session,
                        &tool_name,
                        &args,
                        crate::lua::ToolCallIds {
                            invocation_id,
                            request_id,
                            call_id: &call_id,
                        },
                    )
                });
            }
            EngineEvent::CoreToolResult {
                request_id,
                content,
                is_error,
                metadata,
            } => {
                crate::lua::with_subagent(id, || {
                    crate::host::scope_core(core, || {
                        lua.resolve_core_tool_call(request_id, content, is_error, metadata)
                    })
                });
            }
            EngineEvent::ToolStarted {
                invocation_id,
                call_id,
                tool_name,
                args,
                called_at_ms,
            } => {
                let summary = crate::lua::with_subagent(id, || {
                    crate::host::scope_core(core, || lua.tool_summary(&tool_name, &args))
                });
                child.info.activity = format!("Running {tool_name}");
                child.live_tools.push((
                    invocation_id,
                    crate::Block::ToolCall {
                        call_id,
                        name: tool_name,
                        summary,
                        args: args.into(),
                    },
                    crate::transcript_model::ToolState {
                        status: crate::transcript_model::ToolStatus::Pending,
                        elapsed: None,
                        called_at_ms: Some(called_at_ms),
                        elapsed_active: true,
                        output: None,
                        user_message: None,
                        preview_output: None,
                    },
                ));
            }
            EngineEvent::ToolOutput {
                invocation_id,
                line,
                ..
            } => {
                if let Some((_, _, state)) = child
                    .live_tools
                    .iter_mut()
                    .find(|(key, _, _)| *key == invocation_id)
                {
                    let output = state.output.get_or_insert_with(|| {
                        Box::new(crate::transcript_model::ToolOutput::new(
                            String::new(),
                            false,
                            None,
                        ))
                    });
                    output.content.push_str(&line);
                    output.content.push_str("\n");
                }
            }
            EngineEvent::ToolFinished {
                invocation_id,
                result,
                elapsed_ms,
                ..
            } => {
                if let Some((_, _, state)) = child
                    .live_tools
                    .iter_mut()
                    .find(|(key, _, _)| *key == invocation_id)
                {
                    state.status = if result.is_error {
                        crate::transcript_model::ToolStatus::Err
                    } else {
                        crate::transcript_model::ToolStatus::Ok
                    };
                    state.elapsed = elapsed_ms.map(std::time::Duration::from_millis);
                    state.elapsed_active = false;
                    state.output = Some(Box::new(crate::transcript_model::ToolOutput::new(
                        result.content,
                        result.is_error,
                        result.metadata,
                    )));
                }
            }
            EngineEvent::ReasoningPartDelta {
                delta,
                kind: protocol::ReasoningKind::Raw,
                ..
            } => child.streaming_reasoning.push_str(&delta),
            EngineEvent::Reasoning { content, .. } => child.streaming_reasoning = content,
            EngineEvent::TextDelta { delta } => {
                child.info.activity = "Writing report".into();
                child.streaming_text.push_str(&delta);
            }
            EngineEvent::Text { content } => child.streaming_text = content,
            EngineEvent::EngineAskResponse { id, message, error }
                if child
                    .compaction
                    .as_ref()
                    .is_some_and(|pending| pending.id == id) =>
            {
                if let Some(pending) = child.compaction.take() {
                    let summary = message
                        .and_then(|message| message.content)
                        .map(|content| content.text_content().into_owned())
                        .unwrap_or_default();
                    let end = child.session.history.len();
                    if error.is_none()
                        && child.session.install_context_checkpoint_at_history_index(
                            "compaction".into(),
                            summary,
                            pending.first_live_index,
                            pending.tokens_before,
                            end,
                        )
                    {
                        let (prefix, start, _) = child.session.model_history_range();
                        let coordinates =
                            protocol::ModelHistoryCoordinates::projected(prefix.len(), start);
                        let _ = pending.reply.send(
                            engine::host::HostRequestDecision::replace_model_history(
                                child.session.model_history(),
                                coordinates,
                            ),
                        );
                        child.history_revision = child.history_revision.wrapping_add(1);
                    } else {
                        let _ = pending.reply.send(engine::host::HostRequestDecision::Abort(
                            "subagent context compaction failed".into(),
                        ));
                    }
                }
            }
            EngineEvent::HistoryAppended { delta, .. }
            | EngineEvent::HistoryUpdated { update: delta, .. } => {
                child.history_revision = child.history_revision.wrapping_add(1);
                child.session.history.truncate(delta.first_index.get());
                child.session.history.extend(delta.items);
                child.streaming_text.clear();
                child.streaming_reasoning.clear();
                child.live_tools.clear();
            }
            EngineEvent::TokenUsage {
                usage, cost_usd, ..
            } => {
                child.session.session_usage.accumulate(&usage);
                child.session.session_cost_usd += cost_usd.unwrap_or_default();
                child.info.cost_usd = child.session.session_cost_usd;
                child.info.usage = child.session.session_usage.clone();
            }
            EngineEvent::TurnError { message, .. } => child.info.error = Some(message),
            EngineEvent::TurnComplete { history, meta, .. } => {
                if let Some(delta) = history {
                    child.history_revision = child.history_revision.wrapping_add(1);
                    child.session.history.truncate(delta.first_index.get());
                    child.session.history.extend(delta.items);
                }
                let report = core.agent_report.take();
                child.info.status =
                    if meta.is_some_and(|meta| meta.interrupted) && !child.stopped_for_budget {
                        "cancelled"
                    } else if child.info.error.is_some() {
                        "failed"
                    } else if let Some((status, _)) = &report {
                        status.as_str()
                    } else if child.blocker.is_some() {
                        "blocked"
                    } else {
                        "completed"
                    }
                    .into();
                if child.info.status == "blocked" {
                    child.info.error = child.blocker.clone();
                }
                child.info.activity = child.info.status.clone();
                child.streaming_text.clear();
                child.streaming_reasoning.clear();
                child.live_tools.clear();
                let suffix = child
                    .session
                    .history
                    .get(child.inherited_history_len..)
                    .unwrap_or_default();
                child.info.result = if let Some((_, report)) = report {
                    report
                } else if matches!(child.info.status.as_str(), "completed" | "blocked") {
                    protocol::history_to_messages(suffix)
                        .iter()
                        .rev()
                        .find(|message| message.role == protocol::Role::Assistant)
                        .and_then(|message| message.content.as_ref())
                        .map(|content| content.text_content().into_owned())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                lua.cancel_subagent_tasks(id);
                child.release_execution();
            }
            EngineEvent::Shutdown { reason } if child.info.status == "running" => {
                child.info.status = "failed".into();
                child.info.error =
                    Some(reason.unwrap_or_else(|| "subagent engine shut down".into()));
                lua.cancel_subagent_tasks(id);
                child.release_execution();
            }
            _ => {}
        }
        if let (Some(first_changed), Some(archive)) = (first_changed, &child.archive) {
            archive.update(
                &child.session,
                &child.info,
                child.inherited_history_len,
                first_changed,
            );
        }
        lua.publish_agent(child);
        if child.execution.is_none() {
            self.start_queued_agents();
        }
    }

    pub fn complete_agent_tool(&mut self, output: &TaskDriveOutput) -> bool {
        let TaskDriveOutput::ToolComplete {
            invocation,
            call_id,
            content,
            is_error,
            metadata,
            display_content,
            attachment,
        } = output
        else {
            return false;
        };
        let Some(id) = invocation.subagent_id else {
            return false;
        };
        if let Some(core) = self.agents.children.get(&id).and_then(Child::core) {
            core.engine.send(UiCommand::ToolResult {
                request_id: invocation.request_id,
                invocation_id: invocation.invocation_id,
                call_id: call_id.clone(),
                content: content.clone(),
                is_error: *is_error,
                metadata: metadata.clone(),
                display_content: display_content.clone(),
                attachment: attachment.as_deref().cloned(),
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(root: &std::path::Path) -> (Core, tokio::sync::mpsc::UnboundedReceiver<UiCommand>) {
        let config = crate::config::Config::default();
        let runtime = crate::resolve_runtime(crate::RuntimeInputs {
            config: &config,
            startup: &crate::StartupOverrides::default(),
            available_models: &[],
            registered_modes: &[],
            selections: &crate::RuntimeSelections::default(),
            previous: None,
            headless: true,
        })
        .unwrap();
        let env = engine::env::RuntimeEnv::scripted(
            1,
            root.into(),
            root.join("config"),
            root.join("state"),
            root.join("cache"),
            root.join("data"),
            root.join("runtime"),
            root.into(),
            std::num::NonZeroUsize::new(1).unwrap(),
        );
        let (handle, commands, _) = engine::EngineHandle::for_test();
        (
            Core::new(
                runtime,
                crate::StartupOverrides::default(),
                handle,
                crate::FrontendKind::Headless,
                crate::permissions::PermissionsHandle::new(
                    crate::permissions::Permissions::from_raw(
                        &Default::default(),
                        &Default::default(),
                    ),
                ),
                Arc::new(engine::clock::RealClock),
                Arc::new(env),
            ),
            commands,
        )
    }

    fn add_running_child(
        parent: &mut Core,
        id: u64,
    ) -> tokio::sync::mpsc::UnboundedReceiver<UiCommand> {
        let root = parent.env.cwd();
        parent.agents.restored.insert("parent".into());
        let (child_core, commands) = core(&root);
        let mut session = crate::session::Session::new(1, root);
        session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(
                "child task",
            )));
        parent.agents.children.insert(
            id,
            Child {
                info: AgentInfo {
                    id,
                    name: format!("cedar-{id}"),
                    activity: "Starting response".into(),
                    started_at_ms: None,
                    elapsed_ms: 0,
                    requests: 0,
                    persistence_error: None,
                    group: 1,
                    session_id: session.id.clone(),
                    parent_id: "parent".into(),
                    task: "child task".into(),
                    status: "running".into(),
                    result: String::new(),
                    error: None,
                    cost_usd: 0.0,
                    usage: protocol::TokenUsage::default(),
                },
                session,
                inherited_history_len: 0,
                completion: tokio::sync::watch::channel(None).0,
                execution: Some(ChildExecution::Running(RunningChild {
                    core: Box::new(child_core),
                })),
                streaming_text: String::new(),
                streaming_reasoning: String::new(),
                live_tools: Vec::new(),
                revision: 0,
                history_revision: 0,
                resume: None,
                blocker: None,
                stopped_for_budget: false,
                compaction: None,
                archive: None,
            },
        );
        commands
    }

    fn wait_tool(parent: &mut Core, lua: &LuaRuntime, args: serde_json::Value) -> ToolExecResult {
        let args = serde_json::from_value(args).unwrap();
        let root = parent.env.cwd();
        crate::host::scope_core(parent, || {
            lua.execute_tool(
                "wait_agents",
                &args,
                crate::lua::ToolCallIds {
                    invocation_id: protocol::InvocationId::new(1),
                    request_id: 1,
                    call_id: "wait",
                },
                crate::lua::ToolEnv {
                    mode: protocol::AgentMode::normal(),
                    session_id: "parent",
                    artifact_dir: &root,
                },
                std::time::Instant::now(),
            )
        })
    }

    fn drive_wait(
        parent: &mut Core,
        lua: &LuaRuntime,
        now: std::time::Instant,
    ) -> Vec<TaskDriveOutput> {
        crate::host::scope_core(parent, || {
            lua.pump_task_events();
            lua.drive_tasks(now)
        })
    }

    #[tokio::test]
    async fn subagent_flush_waits_for_every_archive_beyond_model_selection_limit() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        for id in 1..=72 {
            let _commands = add_running_child(&mut parent, id);
            let child = parent.agents.children.get_mut(&id).unwrap();
            child.info.status = "completed".into();
            child.execution = None;
            if id < 72 {
                child.completion.send_replace(Some(child.info.clone()));
            }
        }
        let ids: Vec<_> = parent.agents.children.keys().copied().collect();
        assert!(parent.agents.completions("parent", &ids).is_err());
        let flush = parent.agents.flush("parent");
        tokio::pin!(flush);
        tokio::select! {
            biased;
            result = &mut flush => panic!("flush skipped the last archive: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        let child = &parent.agents.children[&72];
        child.completion.send_replace(Some(child.info.clone()));
        flush.await.unwrap();
        let first = &parent.agents.children[&1];
        let mut info = first.info.clone();
        info.persistence_error = Some("disk full".into());
        first.completion.send_replace(Some(info));
        child.completion.send_replace(None);
        let flush = parent.agents.flush("parent");
        tokio::pin!(flush);
        tokio::select! {
            biased;
            result = &mut flush => panic!("archive failure skipped remaining archives: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        child.completion.send_replace(Some(child.info.clone()));
        assert!(flush.await.unwrap_err().contains("disk full"));
        parent.agents.flush("another parent").await.unwrap();
    }

    #[test]
    fn subagent_restoration_requires_runtime() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        assert_eq!(
            parent
                .agents
                .restore(&parent.sessions, "parent")
                .unwrap_err(),
            "subagent restoration requires an async runtime"
        );
        assert!(!parent.agents.restored.contains("parent"));
        assert!(parent.agents.restoring.is_empty());
    }

    #[test]
    fn subagent_card_snapshot_tracks_archive_completion_without_copying_reports() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let _commands = add_running_child(&mut parent, 1);
        let child = parent.agents.children.get_mut(&1).unwrap();
        child.info.status = "completed".into();
        child.info.result = "Worker report".repeat(1000);
        let card = child.card();
        let (info, pending) = card.snapshot();
        assert!(pending);
        assert_eq!(info.status, "completed");
        assert!(info.persistence_error.is_none());
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("result").is_none());
        assert!(json.get("usage").is_none());
        let mut completed = child.info.clone();
        completed.persistence_error = Some("disk full".into());
        child.completion.send_replace(Some(completed));
        let (info, pending) = card.snapshot();
        assert!(!pending);
        assert_eq!(info.status, "completed");
        assert_eq!(info.persistence_error.as_deref(), Some("disk full"));
    }

    #[test]
    fn subagent_peek_excludes_inherited_context_reasoning_and_non_assistant_messages() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let _commands = add_running_child(&mut parent, 1);
        let message = |text| {
            protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text(text)),
                Some("private reasoning".into()),
                vec![],
            ))
        };
        let child = parent.agents.children.get_mut(&1).unwrap();
        child.session.history = vec![
            message("inherited parent answer"),
            protocol::HistoryItem::user(protocol::Content::text("private task")),
            message("Checked the parser."),
        ];
        child.inherited_history_len = 1;
        child.streaming_text = "Vérifying the boundary".into();
        child.streaming_reasoning = "private in-flight reasoning".into();
        for _ in 0..2 {
            let peek = parent.agents.peek("parent", 1).unwrap();
            assert_eq!(peek.status, "running");
            assert_eq!(peek.output, "Checked the parser.\n\nVérifying the boundary");
            assert!(peek.error.is_none());
        }
        let child = parent.agents.children.get_mut(&1).unwrap();
        assert_eq!(child.streaming_text, "Vérifying the boundary");
        child
            .session
            .history
            .push(message("Vérifying the boundary"));
        child.streaming_text.clear();
        parent.handle_agent_event(
            &LuaRuntime::new(),
            1,
            EngineEvent::TurnComplete {
                turn_id: 1,
                history: None,
                meta: None,
            },
        );
        let peek = parent.agents.peek("parent", 1).unwrap();
        assert_eq!(peek.status, "completed");
        assert_eq!(peek.output, "Checked the parser.\n\nVérifying the boundary");
        assert_eq!(
            parent.agents.children[&1].info.result,
            "Vérifying the boundary"
        );
        assert!(parent.agents.peek("other parent", 1).is_err());
        assert!(parent.agents.peek("parent", 99).is_err());
    }

    #[test]
    fn subagent_peek_retains_partial_output_and_terminal_reasons() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let _commands = add_running_child(&mut parent, 1);
        let _queued_commands = add_running_child(&mut parent, 2);
        parent.agents.children.get_mut(&2).unwrap().info.status = "queued".into();
        let queued = parent.agents.peek("parent", 2).unwrap();
        assert_eq!(queued.status, "queued");
        assert!(queued.output.is_empty());
        parent.cancel_agent(2).unwrap();
        let cancelled = parent.agents.peek("parent", 2).unwrap();
        assert_eq!(cancelled.status, "cancelled");
        assert!(cancelled.output.is_empty());
        parent
            .agents
            .children
            .get_mut(&1)
            .unwrap()
            .session
            .history
            .push(protocol::HistoryItem::assistant(
                protocol::AssistantStep::terminal(
                    Some(protocol::Content::text("Partial findings")),
                    None,
                    vec![],
                ),
            ));
        let lua = LuaRuntime::new();
        parent.handle_agent_event(
            &lua,
            1,
            EngineEvent::TurnError {
                message: "provider unavailable".into(),
                kind: None,
                retry_at_ms: None,
            },
        );
        parent.handle_agent_event(
            &lua,
            1,
            EngineEvent::TurnComplete {
                turn_id: 1,
                history: None,
                meta: None,
            },
        );
        let failed = parent.agents.peek("parent", 1).unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.output, "Partial findings");
        assert_eq!(failed.error.as_deref(), Some("provider unavailable"));
        assert!(parent.agents.children[&1].info.result.is_empty());
    }

    #[test]
    fn subagent_peek_bounds_assistant_output_with_the_shared_process_limiter() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let _commands = add_running_child(&mut parent, 1);
        for text in [
            format!(
                "{}\nlast line",
                "é".repeat(crate::output_limit::DEFAULT_MAX_BYTES)
            ),
            format!(
                "{}last line",
                "older line\n".repeat(crate::output_limit::DEFAULT_MAX_LINES + 1)
            ),
        ] {
            parent.agents.children.get_mut(&1).unwrap().streaming_text = text;
            let peek = parent.agents.peek("parent", 1).unwrap();
            assert!(peek
                .output
                .starts_with("[agent output truncated; showing tail]"));
            assert!(peek.output.ends_with("last line"));
            assert!(peek.output.len() < crate::output_limit::DEFAULT_MAX_BYTES + 512);
            assert!(peek.output.lines().count() <= crate::output_limit::DEFAULT_MAX_LINES + 2);
            assert!(!peek.output.contains('\u{fffd}'));
            assert_eq!(parent.agents.peek("parent", 1).unwrap().output, peek.output);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn subagent_wait_is_indefinite_and_cancellation_only_releases_waiter() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let mut commands = add_running_child(&mut parent, 1);
        let lua = LuaRuntime::new();
        lua.lua
            .load("require('smelt.plugins.subagents')")
            .exec()
            .unwrap();
        assert!(matches!(
            wait_tool(&mut parent, &lua, serde_json::json!({"ids": [1]})),
            ToolExecResult::Pending
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(3600)).await;
        let later = std::time::Instant::now() + std::time::Duration::from_secs(3600);
        assert!(drive_wait(&mut parent, &lua, later).is_empty());
        assert!(
            lua.next_task_wakeup(later).is_none(),
            "wait has no polling timer or watchdog"
        );
        assert_eq!(parent.agents.children[&1].completion.receiver_count(), 1);
        lua.cancel_turn_tasks();
        tokio::task::yield_now().await;
        assert!(drive_wait(&mut parent, &lua, later).is_empty());
        assert_eq!(parent.agents.children[&1].completion.receiver_count(), 0);
        assert_eq!(parent.agents.running_count(), 1);
        assert!(matches!(
            commands.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn subagent_wait_returns_only_final_reports_and_terminal_reasons() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let _commands: Vec<_> = (1..=3)
            .map(|id| add_running_child(&mut parent, id))
            .collect();
        let lua = LuaRuntime::new();
        lua.lua
            .load("require('smelt.plugins.subagents')")
            .exec()
            .unwrap();
        let message = |text| {
            protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text(text)),
                None,
                vec![],
            ))
        };
        for child in parent.agents.children.values_mut() {
            child
                .session
                .history
                .push(message("intermediate commentary, not a final report"));
        }
        parent
            .agents
            .children
            .get_mut(&1)
            .unwrap()
            .session
            .history
            .push(message("final report\nverified"));
        assert!(matches!(
            wait_tool(
                &mut parent,
                &lua,
                serde_json::json!({"ids": [3, 1, 2], "timeout_ms": 0})
            ),
            ToolExecResult::Pending
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(3600)).await;
        assert!(drive_wait(&mut parent, &lua, std::time::Instant::now()).is_empty());
        parent.handle_agent_event(
            &lua,
            2,
            EngineEvent::TurnError {
                message: "provider unavailable".into(),
                kind: None,
                retry_at_ms: None,
            },
        );
        for id in [3, 1, 2] {
            assert!(drive_wait(&mut parent, &lua, std::time::Instant::now()).is_empty());
            parent.handle_agent_event(
                &lua,
                id,
                EngineEvent::TurnComplete {
                    turn_id: id,
                    history: None,
                    meta: (id == 3).then_some(protocol::TurnMeta {
                        interrupted: true,
                        elapsed_ms: 0,
                        avg_tps: None,
                        display_tps: None,
                    }),
                },
            );
            tokio::task::yield_now().await;
        }
        let expected = serde_json::json!([
            {"id":3, "name":"cedar-3", "title":"child task", "status":"cancelled", "error":"subagent was cancelled"},
            {"id":1, "name":"cedar-1", "title":"child task", "status":"completed", "result":"final report\nverified"},
            {"id":2, "name":"cedar-2", "title":"child task", "status":"failed", "error":"provider unavailable"},
        ]);
        for collected in 0..2 {
            if collected == 1 {
                assert!(matches!(
                    wait_tool(&mut parent, &lua, serde_json::json!({"ids": [3, 1, 2]})),
                    ToolExecResult::Pending
                ));
                tokio::task::yield_now().await;
            }
            let outputs = drive_wait(&mut parent, &lua, std::time::Instant::now());
            let [TaskDriveOutput::ToolComplete {
                content,
                is_error,
                display_content,
                ..
            }] = outputs.as_slice()
            else {
                panic!("{outputs:?}")
            };
            assert!(!is_error);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(content).unwrap(),
                expected
            );
            assert_eq!(display_content, &vec![protocol::ToolDisplayContent::new("results",
                "cedar-3 - cancelled\nsubagent was cancelled\n\ncedar-1 - completed\nfinal report\nverified\n\ncedar-2 - failed\nprovider unavailable".into(),
            )]);
        }
        assert!(parent.agents.children[&2].info.result.is_empty());
        assert!(parent.agents.children[&3].info.result.is_empty());
        assert!(parent.engine.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn subagent_wait_reports_runtime_teardown() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let _commands = add_running_child(&mut parent, 1);
        let lua = LuaRuntime::new();
        lua.lua
            .load("require('smelt.plugins.subagents')")
            .exec()
            .unwrap();
        assert!(matches!(
            wait_tool(&mut parent, &lua, serde_json::json!({"ids": [1]})),
            ToolExecResult::Pending
        ));
        tokio::task::yield_now().await;
        parent.agents.children.clear();
        tokio::task::yield_now().await;
        let outputs = drive_wait(&mut parent, &lua, std::time::Instant::now());
        let [TaskDriveOutput::ToolComplete {
            content, is_error, ..
        }] = outputs.as_slice()
        else {
            panic!("{outputs:?}")
        };
        assert!(*is_error);
        assert_eq!(content, "subagent runtime ended");
    }

    #[test]
    fn subagent_wait_validates_ids_and_owner() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let _commands = add_running_child(&mut parent, 1);
        let lua = LuaRuntime::new();
        lua.lua
            .load("require('smelt.plugins.subagents')")
            .exec()
            .unwrap();
        for args in [
            serde_json::json!({"ids": []}),
            serde_json::json!({"ids": vec![1; 65]}),
            serde_json::json!({"ids": [2]}),
        ] {
            assert!(matches!(
                wait_tool(&mut parent, &lua, args),
                ToolExecResult::Immediate { is_error: true, .. }
            ));
        }
        assert!(parent.agents.completions("other parent", &[1]).is_err());
        assert_eq!(parent.agents.children[&1].completion.receiver_count(), 0);
    }

    #[test]
    fn subagent_cancellation_resolves_waiters_and_counts_only_running_children() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let mut commands = add_running_child(&mut parent, 1);
        let _sibling_commands = add_running_child(&mut parent, 2);
        parent.agents.children.get_mut(&2).unwrap().info.status = "queued".into();
        assert_eq!(parent.agents.running_count(), 1);
        let receivers = parent.agents.completions("parent", &[1, 2]).unwrap();
        parent.cancel_agents();
        assert!(matches!(commands.try_recv(), Ok(UiCommand::Cancel)));
        assert_eq!(receivers[1].borrow().as_ref().unwrap().status, "cancelled");
        parent.handle_agent_event(
            &LuaRuntime::new(),
            1,
            EngineEvent::TurnComplete {
                turn_id: 1,
                history: None,
                meta: Some(protocol::TurnMeta {
                    interrupted: true,
                    elapsed_ms: 0,
                    avg_tps: None,
                    display_tps: None,
                }),
            },
        );
        assert_eq!(receivers[0].borrow().as_ref().unwrap().status, "cancelled");
        assert_eq!(parent.agents.running_count(), 0);
        assert!(parent.engine.try_recv().is_err());
    }

    #[tokio::test]
    async fn subagent_archive_flushes_before_completion_and_restores_owner_and_reports() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let _commands = add_running_child(&mut parent, 1);
        let storage = parent.sessions.clone();
        let child = parent.agents.children.get_mut(&1).unwrap();
        let parent_id = "a".repeat(64);
        child.info.parent_id = parent_id.clone();
        child.session.parent_id = Some(parent_id.clone());
        child.archive = Some(archive::AgentArchive::new(
            storage.clone(),
            child.completion.clone(),
        ));
        let mut completion = child.completion.subscribe();
        child.core_mut().unwrap().agent_report = Some((
            "blocked".into(),
            "Need the parent to approve the operation".into(),
        ));
        parent.handle_agent_event(
            &LuaRuntime::new(),
            1,
            EngineEvent::TurnComplete {
                turn_id: 1,
                history: None,
                meta: None,
            },
        );
        let info = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            completion.wait_for(Option::is_some),
        )
        .await
        .unwrap()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone();
        assert_eq!(info.status, "blocked");
        assert!(
            info.persistence_error.is_none(),
            "{:?}",
            info.persistence_error
        );
        let session = storage.load_full(&info.session_id).unwrap();
        assert_eq!(session.history.len(), 1);
        let writer =
            smelt_store::SessionWriter::open(storage.sessions_dir(), info.session_id.clone())
                .unwrap();
        assert_eq!(writer.store_head().unwrap().history_len.get(), 1);
        writer.release().unwrap();
        parent.agents = Agents::default();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !parent.agents.restore(&storage, &parent_id).unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let restored = &parent.agents.children[&1];
        assert_eq!(restored.info().name, "cedar-1");
        assert_eq!(
            restored.info().result,
            "Need the parent to approve the operation"
        );
        assert!(parent
            .agents
            .resolve("other-parent", &serde_json::json!("cedar-1"))
            .is_err());

        parent.agents = Agents::default();
        let _other_commands = add_running_child(&mut parent, 1);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !parent.agents.restore(&storage, &parent_id).unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(parent.agents.children.len(), 2);
        assert_eq!(parent.agents.children[&1].info.parent_id, "parent");
        let restored_id = parent
            .agents
            .resolve(&parent_id, &serde_json::json!("cedar-1"))
            .unwrap();
        assert_ne!(restored_id, 1);
        assert_eq!(
            parent.agents.children[&restored_id].info.session_id,
            info.session_id
        );
        let lua = LuaRuntime::new();
        lua.publish_agent(&parent.agents.children[&1]);
        let environment = crate::lua::module::bundled_chunk_environment(&lua.lua).unwrap();
        lua.lua
            .load(format!(
                "assert(__smelt_internal.agent.__card({:?}) == nil); \
                 assert(__smelt_internal.agent.__card(nil) == nil)",
                info.session_id
            ))
            .set_environment(environment.clone())
            .exec()
            .unwrap();
        lua.publish_agent(&parent.agents.children[&restored_id]);
        lua.lua
            .load(format!(
                "local card = __smelt_internal.agent.__card({:?}); \
                 assert(card.id == {restored_id}); assert(card.status == 'blocked'); \
                 assert(card.session_id == {:?}); \
                 assert(card.result == nil and card.usage == nil and card.task == nil); \
                 assert(__smelt_internal.agent.__card('unknown-session') == nil)",
                info.session_id, info.session_id
            ))
            .set_environment(environment)
            .exec()
            .unwrap();

        parent.agents = Agents::default();
        let _same_parent_commands = add_running_child(&mut parent, 1);
        let active = parent.agents.children.get_mut(&1).unwrap();
        active.info.parent_id = parent_id.clone();
        active.info.name = "birch-1".into();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !parent.agents.restore(&storage, &parent_id).unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            parent.agents.children.len(),
            2,
            "active workers do not hide archives"
        );
        let restored_id = parent
            .agents
            .resolve(&parent_id, &serde_json::json!("cedar-1"))
            .unwrap();
        parent.agents.restored.remove(&parent_id);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !parent.agents.restore(&storage, &parent_id).unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            parent.agents.children.len(),
            2,
            "native identity prevents duplicate restoration"
        );
        assert_eq!(
            parent
                .agents
                .resolve(&parent_id, &serde_json::json!("cedar-1"))
                .unwrap(),
            restored_id
        );
    }

    #[tokio::test]
    async fn subagent_restore_failure_can_be_retried() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let (sender, receiver) = tokio::sync::oneshot::channel();
        parent.agents.restoring.insert(
            "parent".into(),
            AgentRestore {
                records: receiver,
                ready: tokio::sync::watch::channel(None).1,
            },
        );
        assert!(sender.send(Err("archive load failed".into())).is_ok());
        assert_eq!(
            parent
                .agents
                .restore(&parent.sessions, "parent")
                .unwrap_err(),
            "archive load failed"
        );
        assert!(!parent.agents.restored.contains("parent"));
        assert!(!parent.agents.restoring.contains_key("parent"));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        parent.agents.restoring.insert(
            "parent".into(),
            AgentRestore {
                records: receiver,
                ready: tokio::sync::watch::channel(None).1,
            },
        );
        assert!(sender.send(Ok(Vec::new())).is_ok());
        assert!(parent.agents.restore(&parent.sessions, "parent").unwrap());
    }

    #[tokio::test]
    async fn subagent_archive_errors_are_terminal_and_do_not_strand_waiters() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let _commands = add_running_child(&mut parent, 1);
        let invalid_root = root.path().join("not-a-directory");
        std::fs::write(&invalid_root, "occupied").unwrap();
        let child = parent.agents.children.get_mut(&1).unwrap();
        child.archive = Some(archive::AgentArchive::new(
            crate::session::SessionStorage::new(invalid_root),
            child.completion.clone(),
        ));
        let mut completion = child.completion.subscribe();
        let lua = LuaRuntime::new();
        parent.handle_agent_event(
            &lua,
            1,
            EngineEvent::TurnComplete {
                turn_id: 1,
                history: None,
                meta: None,
            },
        );
        let info = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            completion.wait_for(Option::is_some),
        )
        .await
        .unwrap()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone();
        assert_eq!(info.status, "completed");
        assert!(info.persistence_error.is_some());
        let environment = crate::lua::module::bundled_chunk_environment(&lua.lua).unwrap();
        let error = lua
            .lua
            .load(format!(
                "local card = __smelt_internal.agent.__card({:?}); \
                 assert(not card.archive_pending); return card.persistence_error",
                info.session_id
            ))
            .set_environment(environment)
            .eval::<String>()
            .unwrap();
        assert_eq!(Some(error), info.persistence_error);
    }

    #[test]
    fn subagent_budget_counts_parent_and_cached_tokens_without_double_counting_reasoning() {
        let root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(root.path());
        let _commands = add_running_child(&mut parent, 1);
        parent
            .configure_agents(AgentOptions {
                max_tokens: Some(100),
                ..Default::default()
            })
            .unwrap();
        parent.agents.record_parent_usage(
            "parent",
            &protocol::TokenUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(20),
                reasoning_tokens: Some(20),
                ..Default::default()
            },
            Some(1.0),
        );
        parent.agents.children.get_mut(&1).unwrap().info.usage = protocol::TokenUsage {
            cache_read_tokens: Some(60),
            ..Default::default()
        };
        assert!(parent.agents.budget_reason("parent").is_none());
        parent
            .agents
            .children
            .get_mut(&1)
            .unwrap()
            .info
            .usage
            .cache_write_tokens = Some(10);
        assert_eq!(
            parent.agents.budget_reason("parent").as_deref(),
            Some("subagent token budget reached")
        );
        assert!(parent.agents.budget_reason("other-parent").is_none());
    }

    #[test]
    fn cwd_reads_follow_the_active_parent_or_child_host() {
        let parent_root = tempfile::tempdir().unwrap();
        let child_root = tempfile::tempdir().unwrap();
        let (mut parent, _) = core(parent_root.path());
        let (mut child, _) = core(child_root.path());
        let lua = LuaRuntime::new();
        let cwd = || {
            lua.lua
                .load("return smelt.os.cwd()")
                .eval::<String>()
                .unwrap()
        };
        crate::host::scope_core(&mut parent, || {
            assert_eq!(cwd(), parent_root.path().to_string_lossy());
            crate::lua::with_subagent(1, || {
                crate::host::scope_core(&mut child, || {
                    assert_eq!(cwd(), child_root.path().to_string_lossy());
                });
            });
            assert_eq!(cwd(), parent_root.path().to_string_lossy());
        });
    }

    #[test]
    fn terminal_child_events_release_execution_and_preserve_transcript() {
        for (event, status) in [
            (
                EngineEvent::TurnComplete {
                    turn_id: 1,
                    history: None,
                    meta: None,
                },
                "completed",
            ),
            (
                EngineEvent::Shutdown {
                    reason: Some("disconnected".into()),
                },
                "failed",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (mut parent, mut parent_commands) = core(root.path());
            let mut commands = add_running_child(&mut parent, 1);
            let receivers = parent.agents.completions("parent", &[1]).unwrap();
            assert_eq!(parent.agents.running_count(), 1);
            let lua = LuaRuntime::new();
            parent.handle_agent_event(&lua, 1, event);
            assert_eq!(receivers[0].borrow().as_ref().unwrap().status, status);
            assert_eq!(parent.agents.running_count(), 0);
            let child = &parent.agents.children[&1];
            assert_eq!(child.info.status, status);
            assert!(child.execution.is_none());
            assert_eq!(child.session.history.len(), 1);
            assert!(matches!(commands.try_recv(), Ok(UiCommand::Cancel)));
            assert!(matches!(
                commands.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            ));
            parent.handle_agent_event(
                &lua,
                1,
                EngineEvent::TextDelta {
                    delta: "late output".into(),
                },
            );
            assert!(parent.agents.children[&1].streaming_text.is_empty());
            let late_completion = TaskDriveOutput::ToolComplete {
                invocation: crate::lua::ToolInvocationContext {
                    invocation_id: protocol::InvocationId::new(42),
                    request_id: 1,
                    execution_mode: protocol::ToolExecutionMode::Concurrent,
                    subagent_id: Some(1),
                },
                call_id: "late-call".into(),
                content: "late output".into(),
                is_error: false,
                metadata: None,
                display_content: Vec::new(),
                attachment: None,
            };
            assert!(
                parent.complete_agent_tool(&late_completion),
                "late child completions remain child-owned"
            );
            assert!(matches!(
                parent_commands.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
        }
    }
}
