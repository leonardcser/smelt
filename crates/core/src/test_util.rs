#![cfg(test)]
//! Test-only helpers. `XDG_STATE_HOME` is process-wide; tests that mutate it
//! must hold the guard from `xdg_state_guard` for the duration of the test.

use std::ffi::OsString;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

pub fn xdg_state_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub struct XdgStateGuard {
    _guard: MutexGuard<'static, ()>,
    previous: Option<OsString>,
}

impl Drop for XdgStateGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            std::env::set_var("XDG_STATE_HOME", previous);
        } else {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }
}

pub fn isolate_xdg_state(tmp: &Path) -> XdgStateGuard {
    let guard = xdg_state_guard();
    let previous = std::env::var_os("XDG_STATE_HOME");
    std::env::set_var("XDG_STATE_HOME", tmp);
    XdgStateGuard {
        _guard: guard,
        previous,
    }
}
