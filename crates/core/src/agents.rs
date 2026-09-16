//! Runtime-owned child hosts. Lua supplies tasks and presentation, while each
//! child has independent engine commands, permissions, cwd, usage and history.

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
const MAX_RUNNING: usize = 4;
const MAX_PENDING: usize = 64;

#[derive(Clone, serde::Serialize)]
pub struct AgentInfo {
    pub id: u64,
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

pub struct Child {
    pub info: AgentInfo,
    pub session: crate::session::Session,
    execution: Option<ChildExecution>,
    pub streaming_text: String,
    pub streaming_reasoning: String,
    pub live_tools: Vec<(
        protocol::InvocationId,
        crate::Block,
        crate::transcript_model::ToolState,
    )>,
    pub revision: u64,
    pub history_revision: u64,
}

enum ChildExecution {
    Queued(Box<ChildLaunch>),
    Running(RunningChild),
}

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
    inherited_history_len: usize,
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
    }
}

#[derive(Default)]
pub struct Agents {
    pub children: BTreeMap<u64, Child>,
}

impl Core {
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
    ) -> Result<Vec<AgentInfo>, String> {
        let task = task.unwrap_or_else(|| input.clone());
        if crate::lua::current_subagent().is_some() {
            return Err("subagents cannot create subagents".into());
        }
        if input.trim().is_empty() || task.trim().is_empty() || !(1..=16).contains(&count) {
            return Err("provide a non-empty task and a count between 1 and 16".into());
        }
        let pending = self
            .agents
            .children
            .values()
            .filter(|child| matches!(child.info.status.as_str(), "queued" | "running"))
            .count();
        if pending + count > MAX_PENDING {
            return Err("subagent queue is full".into());
        }
        let snapshot = self.engine.fork_snapshot().map_err(str::to_owned)?;
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
            let policy = self.permissions.snapshot();
            let policy = snapshot.permission_overrides().map_or_else(
                || policy.as_ref().clone(),
                |overrides| policy.with_overrides(overrides),
            );
            let info = AgentInfo {
                id,
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
            self.agents.children.insert(
                id,
                Child {
                    info,
                    session,
                    streaming_text: String::new(),
                    streaming_reasoning: String::new(),
                    live_tools: Vec::new(),
                    revision: 0,
                    history_revision: 0,
                    execution: Some(ChildExecution::Queued(Box::new(ChildLaunch {
                        snapshot: Arc::clone(&snapshot),
                        input: input.clone(),
                        config: self.config.clone(),
                        permissions: crate::permissions::PermissionsHandle::new(policy),
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
        let running = self
            .agents
            .children
            .values()
            .filter(|child| child.info.status == "running")
            .count();
        let queued: Vec<u64> = self
            .agents
            .children
            .iter()
            .filter(|(_, child)| child.info.status == "queued")
            .take(MAX_RUNNING.saturating_sub(running))
            .map(|(id, _)| *id)
            .collect();
        for id in queued {
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
                        inherited_history_len: launch.snapshot.history().len(),
                    }));
                    child.info.status = "running".into();
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
        match event {
            EngineEvent::ToolEvaluationRequest {
                request_id,
                tool_name,
                args,
                ..
            } => {
                let permissions = core.permissions.snapshot();
                let mode = core.config.mode.clone();
                let evaluation = crate::lua::with_subagent(id, || {
                    crate::host::scope_core(core, || {
                        lua.evaluate_tool_call(&tool_name, &args, mode, &permissions)
                    })
                });
                core.engine.send(UiCommand::ToolEvaluationResponse {
                    request_id,
                    evaluation,
                });
            }
            EngineEvent::RequestPermission { request_id, .. } => {
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
                let mode = core.config.mode.clone();
                let artifact_dir = core.sessions.artifact_dir_for(&child.session);
                let now = core.clock.instant_now();
                let result = crate::lua::with_subagent(id, || {
                    crate::host::scope_core(core, || {
                        lua.execute_tool(
                            &tool_name,
                            &args,
                            crate::lua::ToolCallIds {
                                invocation_id,
                                request_id,
                                call_id: &call_id,
                            },
                            crate::lua::ToolEnv {
                                mode,
                                session_id: &child.session.id,
                                artifact_dir: &artifact_dir,
                            },
                            now,
                        )
                    })
                });
                if let ToolExecResult::Immediate {
                    content,
                    is_error,
                    metadata,
                    display_content,
                    attachment,
                } = result
                {
                    core.engine.send(UiCommand::ToolResult {
                        request_id,
                        invocation_id,
                        call_id,
                        content,
                        is_error,
                        metadata,
                        display_content,
                        attachment: attachment.map(|value| *value),
                    });
                }
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
            EngineEvent::TextDelta { delta } => child.streaming_text.push_str(&delta),
            EngineEvent::Text { content } => child.streaming_text = content,
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
                child.info.status = if meta.is_some_and(|meta| meta.interrupted) {
                    "cancelled"
                } else if child.info.error.is_some() {
                    "failed"
                } else {
                    "completed"
                }
                .into();
                child.streaming_text.clear();
                child.streaming_reasoning.clear();
                child.live_tools.clear();
                let suffix = child
                    .session
                    .history
                    .get(run.inherited_history_len..)
                    .unwrap_or_default();
                child.info.result = protocol::history_to_messages(suffix)
                    .iter()
                    .rev()
                    .find(|message| message.role == protocol::Role::Assistant)
                    .and_then(|message| message.content.as_ref())
                    .map(|content| content.text_content().into_owned())
                    .unwrap_or_default();
                lua.cancel_subagent_tasks(id);
                child.release_execution();
                self.start_queued_agents();
            }
            EngineEvent::Shutdown { reason } if child.info.status == "running" => {
                child.info.status = "failed".into();
                child.info.error =
                    Some(reason.unwrap_or_else(|| "subagent engine shut down".into()));
                lua.cancel_subagent_tasks(id);
                child.release_execution();
                self.start_queued_agents();
            }
            _ => {}
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
            let (child_core, mut commands) = core(root.path());
            let mut session = crate::session::Session::new(1, root.path().into());
            session
                .history
                .push(protocol::HistoryItem::user(protocol::Content::text(
                    "child task",
                )));
            parent.agents.children.insert(
                1,
                Child {
                    info: AgentInfo {
                        id: 1,
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
                    execution: Some(ChildExecution::Running(RunningChild {
                        core: Box::new(child_core),
                        inherited_history_len: 0,
                    })),
                    streaming_text: String::new(),
                    streaming_reasoning: String::new(),
                    live_tools: Vec::new(),
                    revision: 0,
                    history_revision: 0,
                },
            );
            let lua = LuaRuntime::new();
            parent.handle_agent_event(&lua, 1, event);
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
