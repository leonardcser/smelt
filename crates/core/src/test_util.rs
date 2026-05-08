#![cfg(test)]
//! Test-only helpers. `XDG_STATE_HOME` is process-wide; tests that mutate it
//! must hold the guard from `xdg_state_guard` for the duration of the test.

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
