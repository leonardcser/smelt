//! `EngineBridge` — owns the `EngineHandle` and is the single
//! Rust-side surface tui uses to talk to the engine.
//!
//! Today it's a thin delegate: `send` / `recv` / `try_recv` /
//! `processes()` / `drain_spawned()`. P2.d folds the
//! `EngineEvent` drain into this type so the call chain becomes
//! `EngineBridge::handle_event(host, ev)` and the TuiApp-level
//! `handle_engine_event` retires.

use engine::{tools, EngineHandle};
use protocol::{EngineEvent, UiCommand};
use tokio::sync::mpsc;

pub(crate) struct EngineBridge {
    handle: EngineHandle,
}

impl EngineBridge {
    pub(crate) fn new(handle: EngineHandle) -> Self {
        Self { handle }
    }

    pub(crate) fn send(&self, cmd: UiCommand) {
        self.handle.send(cmd);
    }

    pub(crate) async fn recv(&mut self) -> Option<EngineEvent> {
        self.handle.recv().await
    }

    pub(crate) fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        self.handle.try_recv()
    }

    pub(crate) fn drain_spawned(&mut self) -> Vec<tools::SpawnedChild> {
        self.handle.drain_spawned()
    }

    pub(crate) fn processes(&self) -> &tools::ProcessRegistry {
        &self.handle.processes
    }
}
