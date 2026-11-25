//! Mode and reasoning effort choices on their way to the engine.
//!
//! A running turn applies each choice at its next request, which comes often
//! when requests are short. The app shows a choice at once but tells the engine
//! only once the user stops switching for [`SETTLE`], so a quick run of
//! switches reaches the agent as one change. A turn that ends first takes the
//! settled choice with it.

use std::time::{Duration, Instant};

use protocol::{AgentMode, ReasoningEffort, UiCommand};

use super::TuiApp;

/// How long a choice made during a turn waits for the next one.
pub(crate) const SETTLE: Duration = Duration::from_millis(400);

/// The choices the engine has not heard of, by the values it still has.
#[derive(Default)]
pub(crate) struct RuntimeSync {
    due: Option<Instant>,
    mode: Option<AgentMode>,
    reasoning_effort: Option<ReasoningEffort>,
}

impl TuiApp {
    /// Tells the engine the mode changed from `previous`, once the choice settles.
    pub(crate) fn sync_mode_to_engine(&mut self, previous: AgentMode) {
        self.runtime_sync.mode.get_or_insert(previous);
        self.schedule_runtime_sync();
    }

    /// Tells the engine the effort changed from `previous`, once the choice settles.
    pub(crate) fn sync_reasoning_effort_to_engine(&mut self, previous: ReasoningEffort) {
        self.runtime_sync.reasoning_effort.get_or_insert(previous);
        self.schedule_runtime_sync();
    }

    /// Drops the mode choice still waiting: the engine set the mode itself.
    pub(crate) fn forget_mode_sync(&mut self) {
        self.runtime_sync.mode = None;
    }

    fn schedule_runtime_sync(&mut self) {
        if self.agent_is_running() {
            self.runtime_sync.due = Some(self.core.clock.instant_now() + SETTLE);
        } else {
            self.flush_runtime_sync();
        }
    }

    /// Sends the choices still waiting, settled or not.
    pub(crate) fn flush_runtime_sync(&mut self) {
        let sync = std::mem::take(&mut self.runtime_sync);
        if sync
            .mode
            .is_some_and(|previous| previous != self.core.config.mode)
        {
            let mode = self.core.config.mode.clone();
            self.core
                .engine
                .send(UiCommand::SetMode { mode: mode.clone() });
            // The note tells the next request of the mode without changing
            // the cached prompt prefix. A running turn takes it at its next
            // request boundary; otherwise it joins the history now.
            let text = self.mode_note_text(&mode);
            self.queue_history_append(crate::app::PendingHistoryAppend::mode_change(
                mode.as_str().to_string(),
                text,
            ));
        }
        if sync
            .reasoning_effort
            .is_some_and(|previous| previous != self.core.config.reasoning_effort)
        {
            self.core.engine.send(UiCommand::SetReasoningEffort {
                effort: self.core.config.reasoning_effort.clone(),
            });
        }
    }

    /// Sends the choices that settled, or that outlived their turn.
    pub(crate) fn flush_settled_runtime_sync(&mut self) -> bool {
        let Some(due) = self.runtime_sync.due else {
            return false;
        };
        if self.agent_is_running() && due > self.core.clock.instant_now() {
            return false;
        }
        self.flush_runtime_sync();
        true
    }

    pub(crate) fn runtime_sync_delay(&self) -> Option<Duration> {
        let due = self.runtime_sync.due?;
        Some(due.saturating_duration_since(self.core.clock.instant_now()))
    }
}
