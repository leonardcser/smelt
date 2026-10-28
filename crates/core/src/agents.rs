//! Agent observations shared by native providers and smelt-owned workers.
//! This registry never launches engines or grants tool permissions.

mod archive;
use protocol::{AgentInfo, EngineEvent};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub struct Agent {
    pub info: AgentInfo,
    pub parent_session_id: String,
    pub session: crate::session::Session,
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

type RestoreResult = Result<Vec<(archive::SavedAgent, crate::session::Session)>, String>;

#[derive(Default)]
pub struct Agents {
    pub children: BTreeMap<(String, String), Agent>,
    archives: BTreeMap<(String, String), archive::Archive>,
    restoring: BTreeMap<String, tokio::sync::oneshot::Receiver<RestoreResult>>,
    restored: std::collections::BTreeSet<String>,
}

impl Agents {
    pub fn archive_error(&self, parent: &str, id: &str) -> Option<String> {
        self.archives
            .get(&(parent.into(), id.into()))
            .and_then(archive::Archive::error)
    }

    pub fn restore(
        &mut self,
        storage: &crate::session::SessionStorage,
        parent: &str,
    ) -> Result<(), String> {
        if self.restored.contains(parent) {
            return Ok(());
        }
        if let Some(receiver) = self.restoring.get_mut(parent) {
            match receiver.try_recv() {
                Ok(result) => {
                    self.restoring.remove(parent);
                    for (saved, session) in result? {
                        self.children
                            .entry((parent.into(), saved.info.id.clone()))
                            .or_insert(Agent {
                                info: saved.info,
                                parent_session_id: saved.parent_session_id,
                                session,
                                streaming_text: String::new(),
                                streaming_reasoning: String::new(),
                                live_tools: Vec::new(),
                                revision: 0,
                                history_revision: 0,
                            });
                    }
                    self.restored.insert(parent.into());
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                Err(error) => {
                    self.restoring.remove(parent);
                    return Err(error.to_string());
                }
            }
        } else {
            let storage = storage.clone();
            let id = parent.to_owned();
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.restoring.insert(parent.into(), rx);
            tokio::task::spawn_blocking(move || {
                let _ = tx.send(archive::load(&storage, &id));
            });
        }
        Ok(())
    }

    pub async fn flush(&self, parent: &str) -> Result<(), String> {
        for ((owner, _), archive) in &self.archives {
            if owner == parent {
                archive.flush().await?;
            }
        }
        Ok(())
    }

    pub fn runs<'a>(&'a self, parent: &'a str) -> impl Iterator<Item = &'a Agent> {
        self.children
            .values()
            .filter(move |agent| agent.parent_session_id == parent)
    }

    pub fn by_session(&self, session_id: &str) -> Option<&Agent> {
        self.children
            .values()
            .find(|agent| agent.session.id == session_id)
    }

    pub fn observe(&mut self, event: &EngineEvent, cwd: &std::path::Path, pid: u32) -> bool {
        match event {
            EngineEvent::AgentUpdated {
                session_id,
                agent: info,
            } => {
                let key = (session_id.clone(), info.id.clone());
                let agent = self.children.entry(key).or_insert_with(|| {
                    let mut session = crate::session::Session::new(pid, cwd.to_owned());
                    let identity = serde_json::to_vec(&(session_id, info.owner, &info.id))
                        .expect("agent identity serializes");
                    session.id = Sha256::digest(identity)
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect();
                    session.parent_id = Some(session_id.clone());
                    session.title = Some(info.title.clone());
                    Agent {
                        info: *info.clone(),
                        parent_session_id: session_id.clone(),
                        session,
                        streaming_text: String::new(),
                        streaming_reasoning: String::new(),
                        live_tools: Vec::new(),
                        revision: 0,
                        history_revision: 0,
                    }
                });
                if info.generation < agent.info.generation
                    || (info.generation == agent.info.generation
                        && agent.info.status.is_terminal()
                        && !info.status.is_terminal())
                {
                    return true;
                }
                agent.info = *info.clone();
                agent.session.title = Some(info.title.clone());
                agent.revision = agent.revision.wrapping_add(1);
                true
            }
            EngineEvent::AgentOutput {
                session_id,
                agent_id,
                generation,
                event,
            } => {
                if let Some(agent) = self
                    .children
                    .get_mut(&(session_id.clone(), agent_id.clone()))
                {
                    if *generation == agent.info.generation {
                        agent.output(event);
                    }
                }
                true
            }
            _ => false,
        }
    }
}

impl Agent {
    fn output(&mut self, event: &EngineEvent) {
        use crate::transcript_model::{ToolOutput, ToolState, ToolStatus};
        self.revision = self.revision.wrapping_add(1);
        match event {
            EngineEvent::TextDelta { delta } => self.streaming_text.push_str(delta),
            EngineEvent::Text { content } => self.streaming_text = content.clone(),
            EngineEvent::ReasoningPartDelta {
                delta,
                kind: protocol::ReasoningKind::Raw,
                ..
            } => self.streaming_reasoning.push_str(delta),
            EngineEvent::Reasoning { content, .. } => self.streaming_reasoning = content.clone(),
            EngineEvent::ToolStarted {
                invocation_id,
                call_id,
                tool_name,
                args,
                called_at_ms,
            } => {
                self.live_tools.push((
                    *invocation_id,
                    crate::Block::ToolCall {
                        call_id: call_id.clone(),
                        name: tool_name.clone(),
                        summary: protocol::StyledLines::default(),
                        args: args.clone().into(),
                    },
                    ToolState {
                        status: ToolStatus::Pending,
                        elapsed: None,
                        called_at_ms: Some(*called_at_ms),
                        elapsed_active: true,
                        output: None,
                        user_message: None,
                        preview_output: None,
                    },
                ));
            }
            EngineEvent::ToolFinished {
                invocation_id,
                result,
                elapsed_ms,
                ..
            } => {
                if let Some((_, _, state)) = self
                    .live_tools
                    .iter_mut()
                    .find(|(id, _, _)| id == invocation_id)
                {
                    state.status = if result.is_error {
                        ToolStatus::Err
                    } else {
                        ToolStatus::Ok
                    };
                    state.elapsed = elapsed_ms.map(std::time::Duration::from_millis);
                    state.elapsed_active = false;
                    state.output = Some(Box::new(ToolOutput::new(
                        result.content.clone(),
                        result.is_error,
                        result.metadata.clone(),
                    )));
                }
            }
            EngineEvent::HistoryAppended { delta, .. }
            | EngineEvent::HistoryUpdated { update: delta, .. } => {
                let index = delta.first_index.get();
                if index <= self.session.history.len() {
                    self.session.history.truncate(index);
                    self.session.history.extend(delta.items.iter().cloned());
                    self.history_revision = self.history_revision.wrapping_add(1);
                    self.streaming_text.clear();
                    self.streaming_reasoning.clear();
                    self.live_tools.clear();
                }
            }
            _ => {}
        }
    }
}

impl crate::Core {
    pub fn observe_agent_event(&mut self, event: &EngineEvent) -> bool {
        if !self.agents.observe(event, &self.env.cwd(), self.env.pid()) {
            return false;
        }
        let key = match event {
            EngineEvent::AgentUpdated { session_id, agent } => {
                Some((session_id.clone(), agent.id.clone()))
            }
            EngineEvent::AgentOutput {
                session_id,
                agent_id,
                event,
                ..
            } if matches!(
                event.as_ref(),
                EngineEvent::HistoryAppended { .. } | EngineEvent::HistoryUpdated { .. }
            ) =>
            {
                Some((session_id.clone(), agent_id.clone()))
            }
            _ => None,
        };
        if let Some(key) = key {
            if let Some(agent) = self
                .agents
                .children
                .get(&key)
                .filter(|agent| !agent.info.ephemeral)
            {
                let archive = self
                    .agents
                    .archives
                    .entry(key)
                    .or_insert_with(|| archive::Archive::new(self.sessions.clone()));
                if !archive.update(agent) {
                    *archive = archive::Archive::new(self.sessions.clone());
                    archive.update(agent);
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn info(generation: u64, status: protocol::AgentStatus) -> AgentInfo {
        AgentInfo {
            id: "child".into(),
            ephemeral: false,
            generation,
            parent_agent_id: None,
            owner: protocol::AgentOwner::ClaudeCode,
            title: "Review".into(),
            task: "Inspect".into(),
            status,
            activity: String::new(),
            result: None,
            error: None,
            capabilities: Default::default(),
            usage: None,
            total_tokens: None,
            cost_usd: None,
        }
    }
    #[tokio::test]
    async fn observation_archive_flushes_and_restores_without_execution_controls() {
        let root = tempfile::tempdir().unwrap();
        let storage = crate::session::SessionStorage::new(root.path().join("smelt"));
        let parent = "a".repeat(64);
        let mut agents = Agents::default();
        let mut record = info(1, protocol::AgentStatus::Running);
        record.capabilities.stop = true;
        agents.observe(
            &EngineEvent::AgentUpdated {
                session_id: parent.clone(),
                agent: Box::new(record),
            },
            root.path(),
            1,
        );
        let child = agents.runs(&parent).next().unwrap();
        let archive = archive::Archive::new(storage.clone());
        archive.update(child);
        archive.flush().await.unwrap();
        let loaded = tokio::task::spawn_blocking(move || archive::load(&storage, &parent))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0.info.status, protocol::AgentStatus::Cancelled);
        assert!(!loaded[0].0.info.capabilities.stop);
        assert_eq!(
            loaded[0].1.parent_id.as_deref(),
            Some("a".repeat(64).as_str())
        );
    }

    #[test]
    fn observations_are_scoped_and_stale_assignments_cannot_reopen_or_write() {
        let mut agents = Agents::default();
        let cwd = std::path::Path::new("/tmp");
        for parent in ["parent", "other"] {
            agents.observe(
                &EngineEvent::AgentUpdated {
                    session_id: parent.into(),
                    agent: Box::new(info(1, protocol::AgentStatus::Running)),
                },
                cwd,
                1,
            );
        }
        assert_ne!(
            agents.runs("parent").next().unwrap().session.id,
            agents.runs("other").next().unwrap().session.id
        );
        agents.observe(
            &EngineEvent::AgentUpdated {
                session_id: "parent".into(),
                agent: Box::new(info(1, protocol::AgentStatus::Completed)),
            },
            cwd,
            1,
        );
        agents.observe(
            &EngineEvent::AgentUpdated {
                session_id: "parent".into(),
                agent: Box::new(info(1, protocol::AgentStatus::Running)),
            },
            cwd,
            1,
        );
        assert_eq!(
            agents.runs("parent").next().unwrap().info.status,
            protocol::AgentStatus::Completed
        );
        agents.observe(
            &EngineEvent::AgentUpdated {
                session_id: "parent".into(),
                agent: Box::new(info(2, protocol::AgentStatus::Running)),
            },
            cwd,
            1,
        );
        for (generation, text) in [(1, "stale"), (2, "current")] {
            agents.observe(
                &EngineEvent::AgentOutput {
                    session_id: "parent".into(),
                    agent_id: "child".into(),
                    generation,
                    event: Box::new(EngineEvent::TextDelta { delta: text.into() }),
                },
                cwd,
                1,
            );
        }
        assert_eq!(
            agents.runs("parent").next().unwrap().streaming_text,
            "current"
        );
    }
}
