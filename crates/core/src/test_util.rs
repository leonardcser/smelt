#![cfg(test)]
//! Test-only helpers.

use std::path::Path;

pub fn xdg_state_guard() -> smelt_test_support::ProcessEnvironmentGuard {
    smelt_test_support::ProcessEnvironmentGuard::capture()
}

pub fn isolate_xdg_state(tmp: &Path) -> smelt_test_support::ProcessEnvironmentGuard {
    let guard = xdg_state_guard();
    guard.set_var("XDG_STATE_HOME", tmp);
    guard
}

pub struct CompletionControl {
    started: tokio::sync::oneshot::Receiver<()>,
    release: tokio::sync::oneshot::Sender<()>,
}

pub struct ControlledCompletion<T> {
    result: T,
    started: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

pub fn controlled_completion<T>(result: T) -> (CompletionControl, ControlledCompletion<T>) {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    (
        CompletionControl {
            started: started_rx,
            release: release_tx,
        },
        ControlledCompletion {
            result,
            started: started_tx,
            release: release_rx,
        },
    )
}

impl CompletionControl {
    pub async fn wait_started(self) -> tokio::sync::oneshot::Sender<()> {
        let _ = self.started.await;
        self.release
    }
}

impl<T> ControlledCompletion<T> {
    pub async fn complete(self) -> T {
        let _ = self.started.send(());
        let _ = self.release.await;
        self.result
    }
}
