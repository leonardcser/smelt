//! The shell commands smelt lists: jobs of its own supervisor, and commands a
//! native agent such as Claude Code runs inside its own process.

use crate::actions::{Effect, Target};
use crate::process::{JobInfo, JobOutput};
use crate::runtime::Core;
use protocol::{BackgroundOutcome, NativeProcess, UiCommand};

/// Where work a tool call left running in the background stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundJobState {
    Running,
    Ended(BackgroundOutcome, Option<std::time::Duration>),
    /// Nothing runs it anymore and nothing reported how it ended, such as a
    /// job of a session that ended.
    Gone,
}

impl Core {
    /// Running jobs, then the native session's shell commands, without
    /// those the user stopped.
    /// Where background job `id` of session `session` stands: a job of
    /// smelt's supervisor, or an agent or shell command of the native
    /// session, whose IDs are namespaced `provider:task`.
    pub fn background_job_state(&self, session: &str, id: &str) -> BackgroundJobState {
        if let Some(state) = self.jobs.job_state(id) {
            return state;
        }
        let native_live = self.native_session.as_deref() == Some(session);
        if let Some(agent) = self.agents.runs(session).find(|agent| agent.info.id == id) {
            if let Some(outcome) = BackgroundOutcome::of_agent(agent.info.status) {
                return BackgroundJobState::Ended(outcome, None);
            }
            return if native_live {
                BackgroundJobState::Running
            } else {
                BackgroundJobState::Gone
            };
        }
        if native_live && self.native_process(id).is_some() {
            return BackgroundJobState::Running;
        }
        BackgroundJobState::Gone
    }

    /// Running jobs, then the native session's shell commands.
    pub fn processes(&self) -> Vec<JobInfo> {
        let mut processes = self.jobs.list();
        processes.retain(|job| self.lists_process(&job.id));
        processes.extend(self.listed_native_processes().map(|process| JobInfo {
            id: process.id.clone(),
            pid: None,
            command: process.command.clone(),
            elapsed_secs: self.native_elapsed_secs(process),
        }));
        processes
    }

    /// Whether `processes` lists process `id`: the user did not stop it.
    fn lists_process(&self, id: &str) -> bool {
        self.actions.process_effect(id) != Some(Effect::Stop)
    }

    fn listed_native_processes(&self) -> impl Iterator<Item = &NativeProcess> {
        self.native_processes
            .iter()
            .filter(|process| self.lists_process(&process.id))
    }

    /// Whether the native session runs work of its own, such as a monitor or
    /// a background agent.
    pub fn native_session_has_background_work(&self) -> bool {
        let Some(session) = self.native_session.as_deref() else {
            return false;
        };
        !self.native_processes.is_empty()
            || self
                .agents
                .runs(session)
                .any(|agent| !agent.info.status.is_terminal())
    }

    /// Whether finished work will still wake the agent: a notifying
    /// background job, or the native session's own background work.
    pub fn has_pending_continuation(&self) -> bool {
        self.jobs.has_pending_continuation() || self.native_session_has_background_work()
    }

    /// How many processes `processes` lists.
    pub fn running_process_count(&self) -> usize {
        self.jobs
            .list()
            .iter()
            .filter(|job| self.lists_process(&job.id))
            .count()
            + self.listed_native_processes().count()
    }

    /// The output of process `id` without draining it. A native shell command
    /// shows what it printed by its last read and is read again.
    pub fn process_output(&self, id: &str) -> Result<JobOutput, String> {
        let Some(process) = self.native_process(id) else {
            return self.jobs.snapshot_output(id);
        };
        self.send_native(|session_id| UiCommand::ReadNativeProcess {
            session_id,
            id: id.into(),
        });
        Ok(JobOutput {
            text: process.output.clone(),
            running: true,
            exit_code: None,
            termination: None,
            elapsed_secs: self.native_elapsed_secs(process),
            pid: None,
        })
    }

    /// Stops process `id` asynchronously, and stops listing it at once.
    pub fn stop_process(&mut self, id: String) {
        let action = self.begin_action(Target::Process(id.clone()), Effect::Stop);
        if self.native_process(&id).is_some() {
            self.send_native(|session_id| UiCommand::StopNativeProcess {
                session_id,
                id,
                action,
            });
            return;
        }
        let supervisor = self.jobs.clone();
        tokio::spawn(async move {
            let _ = supervisor.stop(&id).await;
        });
    }

    /// Leaves the latest foreground job, or else each shell command and
    /// agent the native session's main agent waits for, running in the
    /// background, and shows them there at once. Returns whether it asked
    /// for that.
    pub fn detach_foreground_process(&mut self) -> bool {
        if self.jobs.detach_latest_foreground().requested() {
            return true;
        }
        let Some(session_id) = self.native_session.clone() else {
            return false;
        };
        let work = self.native_foreground_work(&session_id);
        for (target, call_id) in &work {
            let action = self.begin_action(target.clone(), Effect::Background);
            self.engine.send(UiCommand::BackgroundNativeCall {
                session_id: session_id.clone(),
                call_id: call_id.clone(),
                action,
            });
        }
        !work.is_empty()
    }

    /// What the main agent of native session `session` waits for: its shell
    /// commands and the agents it launched, each by its tool call.
    fn native_foreground_work(&self, session: &str) -> Vec<(Target, String)> {
        let processes = self
            .listed_native_processes()
            .filter(|process| {
                process.agent_id.is_none() && !self.native_process_in_background(process)
            })
            .filter_map(|process| {
                let call = process.call_id.clone()?;
                Some((Target::Process(process.id.clone()), call))
            });
        let agents = self
            .agents
            .runs(session)
            .filter(|agent| self.agent_runs(agent))
            .map(|agent| self.agent_view(agent))
            .filter(|agent| !agent.background && agent.parent_agent_id.is_none())
            .filter_map(|agent| {
                let target = Target::Agent {
                    parent: session.into(),
                    id: agent.id,
                };
                Some((target, agent.call_id?))
            });
        processes.chain(agents).collect()
    }

    /// How many agents of session `session` run.
    pub fn running_agent_count(&self, session: &str) -> usize {
        self.agents
            .runs(session)
            .filter(|agent| self.agent_runs(agent))
            .count()
    }

    fn native_process(&self, id: &str) -> Option<&NativeProcess> {
        self.native_processes
            .iter()
            .find(|process| process.id == id)
    }

    fn native_elapsed_secs(&self, process: &NativeProcess) -> u64 {
        engine::clock::unix_time_ms(self.clock.as_ref()).saturating_sub(process.started_at_ms)
            / 1000
    }

    fn send_native(&self, command: impl FnOnce(String) -> UiCommand) -> bool {
        let Some(session_id) = self.native_session.clone() else {
            return false;
        };
        self.engine.send(command(session_id));
        true
    }
}
