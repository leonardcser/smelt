//! Thin wrapper around `EngineHandle` that gates `recv`/`try_recv` on the confirms-clear flag.

use engine::{EngineHandle, HostCall};
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

    pub fn send(&self, cmd: UiCommand) {
        self.handle.send(cmd);
    }

    /// Returns `pending()` when a confirm dialog is open, pausing the engine.
    pub async fn recv(&mut self) -> Option<EngineEvent> {
        if !self.confirms_clear.load(Ordering::Relaxed) {
            std::future::pending().await
        } else {
            self.handle.recv().await
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

    /// Move out the host-callback receiver. Caller stores it as a
    /// sibling field so the engine event loop and the host-callback
    /// loop can be polled in the same `tokio::select!` without
    /// borrowing `EngineClient` twice.
    pub fn take_host_rx(&mut self) -> mpsc::UnboundedReceiver<HostCall> {
        self.handle.take_host_rx()
    }

    pub(crate) fn injector(&self) -> engine::EventInjector {
        self.handle.injector()
    }
}
