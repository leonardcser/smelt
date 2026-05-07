#![cfg(test)]
//! Test-only helpers shared across modules.
//!
//! `XDG_STATE_HOME` is a process-wide env var. Multiple tests across
//! `trust::tests` and `lua::runtime::tests` mutate it; cargo runs them
//! on parallel threads inside one process. Anything that reads or
//! writes `XDG_STATE_HOME` from a test must hold the guard returned
//! here for the duration of the test.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

pub fn xdg_state_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn isolate_xdg_state(tmp: &Path) -> MutexGuard<'static, ()> {
    let g = xdg_state_guard();
    std::env::set_var("XDG_STATE_HOME", tmp);
    g
}
