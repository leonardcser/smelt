//! Pause the HTTP agent's output during confirmation. Independent agents keep
//! delivering control events, including cancellation of an open confirmation.

use engine::{EngineDisconnected, EngineHandle, EngineOutput};
use protocol::{EngineEvent, UiCommand};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct EngineClient {
    handle: EngineHandle,
    confirms_clear: Arc<AtomicBool>,
    native_agent: AtomicBool,
}

impl EngineClient {
    pub(crate) fn new(handle: EngineHandle, confirms_clear: Arc<AtomicBool>) -> Self {
        Self {
            handle,
            confirms_clear,
            native_agent: AtomicBool::new(false),
        }
    }

    pub fn send(&self, cmd: UiCommand) {
        let _ = self.try_send(cmd);
    }

    pub fn try_send(&self, cmd: UiCommand) -> Result<(), EngineDisconnected> {
        if let UiCommand::StartTurn(payload) = &cmd {
            // An independent agent can cancel a permission request while its UI is open.
            self.native_agent.store(
                smelt_provider::BackendKind::is_claude_code(&payload.model_target.provider_type),
                Ordering::Relaxed,
            );
        }
        self.handle.try_send(cmd)
    }

    fn output_paused(&self) -> bool {
        !self.native_agent.load(Ordering::Relaxed) && !self.confirms_clear.load(Ordering::Relaxed)
    }

    /// Returns `pending()` while HTTP-agent confirmation pauses output.
    pub async fn recv(&mut self) -> Option<EngineEvent> {
        if self.output_paused() {
            std::future::pending().await
        } else {
            self.handle.recv().await
        }
    }

    /// Receives ordered protocol events and host callbacks unless output is paused.
    pub async fn recv_output(&mut self) -> Option<EngineOutput> {
        if self.output_paused() {
            std::future::pending().await
        } else {
            self.handle.recv_output().await
        }
    }

    /// Returns `Err(Empty)` when a confirm dialog is open.
    pub fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        if self.output_paused() {
            Err(mpsc::error::TryRecvError::Empty)
        } else {
            self.handle.try_recv()
        }
    }

    /// Returns `Err(Empty)` when a confirm dialog is open.
    pub fn try_recv_output(&mut self) -> Result<EngineOutput, mpsc::error::TryRecvError> {
        if self.output_paused() {
            Err(mpsc::error::TryRecvError::Empty)
        } else {
            self.handle.try_recv_output()
        }
    }

    pub(crate) fn injector(&self) -> engine::EventInjector {
        self.handle.injector()
    }
}
