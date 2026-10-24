//! Shared harness runtime helpers.

use std::sync::Once;

/// Directory that never exists, so `PATH` lookups fail immediately.
const EMPTY_PATH: &str = "/nonexistent-smelt-fuzz";

/// Make every child-process spawn fail fast and identically on all machines.
///
/// The app probes `git`, `tput` and `systemd-inhibit` while building and
/// running. Left alone, those cost several milliseconds per spawn (roughly 28
/// processes per scenario), depend on the host's repository and installed
/// tools, and take a real sleep inhibitor on the machine running the fuzzer.
/// Scenarios exercise the "tool unavailable" paths instead.
fn isolate_child_processes() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| std::env::set_var("PATH", EMPTY_PATH));
}

/// Run `body` inside a current-thread Tokio runtime.
///
/// Fuzz scenarios sometimes hit production paths that call `tokio::spawn`
/// (for example Vim shell escapes or Lua task helpers). Entering a runtime
/// lets those paths construct tasks without driving real background work.
pub fn with_current_thread_runtime<T>(label: &str, body: impl FnOnce() -> T) -> T {
    isolate_child_processes();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| panic!("build tokio runtime for {label}: {e}"));
    let _guard = runtime.enter();
    body()
}
