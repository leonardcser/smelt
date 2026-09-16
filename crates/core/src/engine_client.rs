//! Thin wrapper around `EngineHandle` that pauses output while a confirm dialog is open.

use engine::{EngineDisconnected, EngineHandle, EngineOutput};
use protocol::{EngineEvent, UiCommand};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct EngineClient {
    handle: EngineHandle,
    confirms_clear: Arc<AtomicBool>,
}

impl EngineClient {
    pub(crate) fn new(handle: EngineHandle, confirms_clear: Arc<AtomicBool>) -> Self {
        Self {
            handle,
            confirms_clear,
        }
    }

    pub fn enable_forks(&self) -> Result<(), &'static str> {
        self.handle.enable_forks()
    }

    pub fn fork_snapshot(&self) -> Result<Arc<engine::fork::ForkSnapshot>, &'static str> {
        self.handle.fork_snapshot()
    }

    pub fn start_fork(
        &self,
        snapshot: &Arc<engine::fork::ForkSnapshot>,
        id: u64,
        session_id: String,
        task: String,
    ) -> Result<EngineHandle, &'static str> {
        let mut child = self
            .handle
            .start_fork(snapshot, session_id, u64::MAX - id, task)?;
        self.handle.forward_child(id, &mut child);
        Ok(child)
    }

    pub fn send(&self, cmd: UiCommand) {
        self.handle.send(cmd);
    }

    pub fn try_send(&self, cmd: UiCommand) -> Result<(), EngineDisconnected> {
        self.handle.try_send(cmd)
    }

    /// Returns `pending()` when a confirm dialog is open, pausing the engine.
    pub async fn recv(&mut self) -> Option<EngineEvent> {
        if !self.confirms_clear.load(Ordering::Relaxed) {
            std::future::pending().await
        } else {
            self.handle.recv().await
        }
    }

    /// Returns `pending()` when a confirm dialog is open, preserving the
    /// ordering of protocol events and host callbacks.
    pub async fn recv_output(&mut self) -> Option<EngineOutput> {
        if !self.confirms_clear.load(Ordering::Relaxed) {
            std::future::pending().await
        } else {
            self.handle.recv_output().await
        }
    }

    /// Returns `Err(Empty)` when a confirm dialog is open.
    pub fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        if !self.confirms_clear.load(Ordering::Relaxed) {
            Err(mpsc::error::TryRecvError::Empty)
        } else {
            self.handle.try_recv()
        }
    }

    /// Returns `Err(Empty)` when a confirm dialog is open.
    pub fn try_recv_output(&mut self) -> Result<EngineOutput, mpsc::error::TryRecvError> {
        if !self.confirms_clear.load(Ordering::Relaxed) {
            Err(mpsc::error::TryRecvError::Empty)
        } else {
            self.handle.try_recv_output()
        }
    }

    pub(crate) fn injector(&self) -> engine::EventInjector {
        self.handle.injector()
    }
}
