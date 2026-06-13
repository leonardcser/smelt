//! Shared harness runtime helpers.

/// Run `body` inside a current-thread Tokio runtime.
///
/// Fuzz scenarios sometimes hit production paths that call `tokio::spawn`
/// (for example Vim shell escapes or Lua task helpers). Entering a runtime
/// lets those paths construct tasks without driving real background work.
pub fn with_current_thread_runtime<T>(label: &str, body: impl FnOnce() -> T) -> T {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| panic!("build tokio runtime for {label}: {e}"));
    let _guard = runtime.enter();
    body()
}
