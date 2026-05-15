//! Unified event source for the application loop.
//!
//! Every input the loop reacts to — terminal keys, engine events, Lua
//! wakeups, shell-exec output, virtual-clock ticks, OS signals — flows
//! through [`SourceEvent`] and an [`EventSource`] implementation.
//! Production wraps the live `tokio::select!`; tests iterate a scripted
//! `Vec` via [`ScriptedSource`].

use crossterm::event::Event as TermEvent;
use protocol::EngineEvent;

/// One unit of input to the main loop.
#[derive(Debug, Clone)]
pub enum SourceEvent {
    /// Raw terminal event (key, mouse, paste, focus, resize).
    Term(TermEvent),
    /// Event emitted by the engine task (turn lifecycle, tool calls, etc).
    Engine(EngineEvent),
    /// A Lua-side task pushed work onto the inbox; the loop should
    /// drain pending callbacks and resume parked coroutines.
    LuaWakeup,
    /// Output from a foregrounded shell-exec child process.
    ExecOutput(String),
    /// A foregrounded shell-exec child process exited with this code.
    ExecDone(Option<i32>),
    /// Advance the virtual clock by `ms` milliseconds. Production never
    /// emits this; only scripted sources do.
    Tick(u64),
    /// Terminal window resize (SIGWINCH on Unix). Carries the new
    /// dimensions in cells; production reads them from crossterm.
    Resize { width: u16, height: u16 },
}

/// Async iterator over [`SourceEvent`]s. Returns `None` to signal end of
/// stream (the loop then exits). Implementors are concrete types — the
/// trait is not designed for `dyn`-erasure.
pub trait EventSource: Send {
    fn next(&mut self) -> impl std::future::Future<Output = Option<SourceEvent>> + Send;
}

/// Drain a fixed sequence of events. Used by tests and the scripted
/// replay binary — feeds events in order and reports `None` when the
/// script is exhausted.
pub struct ScriptedSource {
    queue: std::collections::VecDeque<SourceEvent>,
}

impl ScriptedSource {
    pub fn new(events: impl IntoIterator<Item = SourceEvent>) -> Self {
        Self {
            queue: events.into_iter().collect(),
        }
    }

    pub fn push(&mut self, ev: SourceEvent) {
        self.queue.push_back(ev);
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

impl EventSource for ScriptedSource {
    async fn next(&mut self) -> Option<SourceEvent> {
        self.queue.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scripted_source_yields_events_in_order_then_none() {
        let mut src = ScriptedSource::new([SourceEvent::Tick(10), SourceEvent::LuaWakeup]);
        assert!(matches!(src.next().await, Some(SourceEvent::Tick(10))));
        assert!(matches!(src.next().await, Some(SourceEvent::LuaWakeup)));
        assert!(src.next().await.is_none());
    }

    #[tokio::test]
    async fn scripted_source_supports_late_push() {
        let mut src = ScriptedSource::new(std::iter::empty());
        assert!(src.is_empty());
        src.push(SourceEvent::Resize {
            width: 80,
            height: 24,
        });
        assert!(matches!(
            src.next().await,
            Some(SourceEvent::Resize {
                width: 80,
                height: 24
            })
        ));
    }
}
