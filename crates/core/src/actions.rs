//! User actions smelt shows as done the moment the user asks for them. What
//! each pending action predicts is laid over what the engine last reported,
//! until a report shows it, the engine refuses it, or it times out. Only the
//! reports themselves change; the prediction never does.

use protocol::{ActionId, AgentInfo, AgentStatus, UiCommand};
use std::time::{Duration, Instant};

/// How long an action may wait for a report that shows it.
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// What an action asks of its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// It keeps running, and nothing waits for it.
    Background,
    /// It ends.
    Stop,
}

/// Something an action changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A process `Core::processes` lists, by ID.
    Process(String),
    /// Agent `id` of session `parent`.
    Agent { parent: String, id: String },
}

struct Pending {
    id: ActionId,
    target: Target,
    effect: Effect,
    deadline: Instant,
}

impl Pending {
    fn failure(&self, reason: &str) -> String {
        let what = match (self.effect, &self.target) {
            (Effect::Background, _) => "move to background",
            (Effect::Stop, Target::Process(_)) => "stop the process",
            (Effect::Stop, Target::Agent { .. }) => "stop the agent",
        };
        format!("cannot {what}: {reason}")
    }
}

#[derive(Default)]
pub struct Actions {
    next: u64,
    pending: Vec<Pending>,
}

impl Actions {
    /// Starts asking for `effect` on `target`, which lasts until `now` plus
    /// [`ACTION_TIMEOUT`].
    pub fn begin(&mut self, target: Target, effect: Effect, now: Instant) -> ActionId {
        self.next += 1;
        let id = ActionId::new(self.next);
        self.pending.push(Pending {
            id,
            target,
            effect,
            deadline: now + ACTION_TIMEOUT,
        });
        id
    }

    /// The effect the latest pending action on a target `matches` predicts.
    fn effect_where(&self, matches: impl Fn(&Target) -> bool) -> Option<Effect> {
        self.pending
            .iter()
            .rev()
            .find(|action| matches(&action.target))
            .map(|action| action.effect)
    }

    /// The effect a pending action predicts on process `id`.
    pub fn process_effect(&self, id: &str) -> Option<Effect> {
        self.effect_where(|target| matches!(target, Target::Process(process) if process == id))
    }

    /// The effect a pending action predicts on agent `id` of session `parent`.
    pub fn agent_effect(&self, parent: &str, id: &str) -> Option<Effect> {
        self.effect_where(
            |target| matches!(target, Target::Agent { parent: p, id: i } if p == parent && i == id),
        )
    }

    /// Drops each action a report now shows.
    pub fn settle(&mut self, shown: impl Fn(&Target, Effect) -> bool) {
        self.pending
            .retain(|action| !shown(&action.target, action.effect));
    }

    /// Drops action `id`, which the engine refused for `reason`, and returns
    /// what to tell the user. `None` when it is no longer pending.
    pub fn fail(&mut self, id: ActionId, reason: &str) -> Option<String> {
        let at = self.pending.iter().position(|action| action.id == id)?;
        Some(self.pending.remove(at).failure(reason))
    }

    /// Drops the actions no report showed by `now`, and returns what to tell
    /// the user, once for each distinct message.
    pub fn expire(&mut self, now: Instant) -> Vec<String> {
        let reason = format!("not done after {}s", ACTION_TIMEOUT.as_secs());
        let mut messages = Vec::new();
        self.pending.retain(|action| {
            let pending = action.deadline > now;
            let message = (!pending).then(|| action.failure(&reason));
            if let Some(message) = message.filter(|message| !messages.contains(message)) {
                messages.push(message);
            }
            pending
        });
        messages
    }

    /// When the next pending action times out.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|action| action.deadline).min()
    }
}

/// `info` with what pending actions predict on it.
fn predicted_agent(mut info: AgentInfo, effect: Option<Effect>) -> AgentInfo {
    match effect {
        Some(Effect::Background) => info.background = true,
        Some(Effect::Stop) => {
            info.status = AgentStatus::Cancelled;
            info.capabilities.stop = false;
            info.activity.clear();
        }
        None => {}
    }
    info
}

impl crate::Core {
    /// Agent `agent` as the user sees it, with what pending actions predict.
    pub fn agent_view(&self, agent: &crate::agents::Agent) -> AgentInfo {
        let effect = self
            .actions
            .agent_effect(&agent.parent_session_id, &agent.info.id);
        predicted_agent(agent.info.clone(), effect)
    }

    /// Whether `agent` runs, as the user sees it.
    pub(crate) fn agent_runs(&self, agent: &crate::agents::Agent) -> bool {
        !agent.info.status.is_terminal()
            && self
                .actions
                .agent_effect(&agent.parent_session_id, &agent.info.id)
                != Some(Effect::Stop)
    }

    /// Whether native `process` keeps running with nothing waiting for it,
    /// as the user sees it.
    pub(crate) fn native_process_in_background(&self, process: &protocol::NativeProcess) -> bool {
        process.background || self.actions.process_effect(&process.id) == Some(Effect::Background)
    }

    /// Whether the work tool call `call_id` of the native session waits for,
    /// a shell command or an agent, runs in the background, as the user sees
    /// it. Its call shows so until its result arrives.
    pub fn call_in_background(&self, call_id: &str) -> bool {
        let call = Some(call_id);
        self.native_processes
            .iter()
            .filter(|process| process.call_id.as_deref() == call)
            .any(|process| self.native_process_in_background(process))
            || self.native_session.as_deref().is_some_and(|session| {
                self.agents
                    .runs(session)
                    .filter(|agent| agent.info.call_id.as_deref() == call)
                    .any(|agent| {
                        agent.info.background
                            || self.actions.agent_effect(session, &agent.info.id)
                                == Some(Effect::Background)
                    })
            })
    }

    /// Asks agent `id` of session `parent` to stop, and shows it stopped at
    /// once. Returns false when it cannot stop.
    pub fn stop_agent(&mut self, parent: &str, id: &str) -> bool {
        let Some(agent) = self.agents.runs(parent).find(|agent| agent.info.id == id) else {
            return false;
        };
        let info = self.agent_view(agent);
        if !info.capabilities.stop || info.status.is_terminal() {
            return false;
        }
        let target = Target::Agent {
            parent: parent.into(),
            id: id.into(),
        };
        let action = self.begin_action(target, Effect::Stop);
        self.engine.send(UiCommand::StopAgent {
            session_id: parent.into(),
            agent_id: id.into(),
            action,
        });
        true
    }

    pub(crate) fn begin_action(&mut self, target: Target, effect: Effect) -> ActionId {
        let now = self.clock.instant_now();
        self.actions.begin(target, effect, now)
    }

    /// Drops the predictions the latest reports show. Each report settles
    /// them as it arrives: a later one can list the same ID again.
    pub(crate) fn settle_actions(&mut self) {
        if self.actions.pending.is_empty() {
            return;
        }
        let jobs: Vec<String> = self.jobs.list().into_iter().map(|job| job.id).collect();
        let native = &self.native_processes;
        let agents = &self.agents;
        self.actions.settle(|target, effect| match target {
            Target::Process(id) => {
                let process = native.iter().find(|process| &process.id == id);
                match effect {
                    Effect::Stop => process.is_none() && !jobs.contains(id),
                    Effect::Background => process.is_none_or(|process| process.background),
                }
            }
            Target::Agent { parent, id } => agents
                .runs(parent)
                .find(|agent| &agent.info.id == id)
                .is_none_or(|agent| match effect {
                    Effect::Stop => agent.info.status.is_terminal(),
                    Effect::Background => agent.info.background || agent.info.status.is_terminal(),
                }),
        });
    }

    /// Replaces the native session's shell commands with the engine's latest
    /// report.
    pub fn set_native_processes(&mut self, processes: Vec<protocol::NativeProcess>) {
        self.native_processes = processes;
        self.settle_actions();
    }

    /// Drops action `id`, which the engine refused, and returns what to tell
    /// the user.
    pub fn action_failed(&mut self, id: ActionId, error: &str) -> Option<String> {
        self.actions.fail(id, error)
    }

    /// Drops the actions that timed out, and returns what to tell the user.
    /// Local jobs change without a report, so this settles first.
    pub fn expire_actions(&mut self) -> Vec<String> {
        self.settle_actions();
        let now = self.clock.instant_now();
        self.actions.expire(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(id: &str) -> Target {
        Target::Process(id.into())
    }

    #[test]
    fn a_prediction_lasts_until_a_report_shows_it() {
        let mut actions = Actions::default();
        let now = Instant::now();
        actions.begin(process("a"), Effect::Background, now);
        actions.begin(process("b"), Effect::Stop, now);
        assert_eq!(actions.process_effect("a"), Some(Effect::Background));
        actions.settle(|target, _| target == &process("a"));
        assert_eq!(actions.process_effect("a"), None);
        assert_eq!(actions.process_effect("b"), Some(Effect::Stop));
        actions.settle(|_, _| true);
        assert_eq!(actions.deadline(), None);
    }

    #[test]
    fn a_refused_or_unanswered_action_drops_its_prediction_and_says_why() {
        let mut actions = Actions::default();
        let now = Instant::now();
        let refused = actions.begin(process("a"), Effect::Stop, now);
        let later = now + Duration::from_secs(1);
        for id in ["b", "c"] {
            actions.begin(process(id), Effect::Background, later);
        }
        assert_eq!(
            actions.fail(refused, "it ended").as_deref(),
            Some("cannot stop the process: it ended")
        );
        assert_eq!(actions.fail(refused, "again"), None);
        assert_eq!(actions.process_effect("a"), None);
        assert!(actions.expire(now + ACTION_TIMEOUT).is_empty());
        assert_eq!(
            actions.expire(later + ACTION_TIMEOUT),
            ["cannot move to background: not done after 30s"]
        );
        assert_eq!(actions.process_effect("b"), None);
    }
}
