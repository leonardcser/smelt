//! Thin wrapper around `EngineHandle` that gates `recv`/`try_recv` on the confirms-clear flag.

use engine::EngineHandle;
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

    pub(crate) fn injector(&self) -> engine::EventInjector {
        self.handle.injector()
    }
}
