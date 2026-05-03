//! `EngineClient` — owns the `EngineHandle` and is the single
//! Rust-side surface app uses to talk to the engine.
//!
//! Thin wrapper around `EngineHandle` that gates `recv` / `try_recv`
//! on the confirms-clear flag so the engine pauses while a confirm
//! dialog is open. Event dispatch lives in `crate::app::engine_events`.

use engine::EngineHandle;
use protocol::{EngineEvent, UiCommand};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(crate) struct EngineClient {
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

    pub(crate) fn send(&self, cmd: UiCommand) {
        self.handle.send(cmd);
    }

    /// Returns `pending()` when a confirm dialog is open so the
    /// `select!` branch never resolves — the engine pauses until
    /// `Confirms::is_clear()` is true again.
    pub(crate) async fn recv(&mut self) -> Option<EngineEvent> {
        if !self.confirms_clear.load(Ordering::Relaxed) {
            std::future::pending().await
        } else {
            self.handle.recv().await
        }
    }

    /// Returns `Err(Empty)` when a confirm dialog is open so the
    /// drain loop breaks immediately.
    pub(crate) fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        if !self.confirms_clear.load(Ordering::Relaxed) {
            Err(mpsc::error::TryRecvError::Empty)
        } else {
            self.handle.try_recv()
        }
    }

    /// Cloneable injector for cross-thread tasks that need to push
    /// events into the engine's event stream (e.g. streaming bash
    /// emitting `EngineEvent::ToolOutput` per line).
    pub(crate) fn injector(&self) -> engine::EventInjector {
        self.handle.injector()
    }
}
