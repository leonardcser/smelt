//! Filesystem watcher registry shared between `LuaShared` and the
//! `smelt.fs.watch` Lua API. Kept neutral of the api layer so
//! `shared.rs` doesn't need to depend on `crate::lua::api::*`.

use notify::RecommendedWatcher;
use std::sync::{Arc, Mutex};

/// Per-watcher mutable state shared between the notify thread and the
/// Lua-driven arm/stop calls.
#[derive(Default)]
pub struct WatcherState {
    /// Events queued since the last drain. Drained whole on each arm.
    pub pending: Vec<serde_json::Value>,
    /// External task id waiting for the next event batch. The notify
    /// thread takes this and resolves it as soon as one event arrives.
    pub armed: Option<u64>,
    pub closed: bool,
}

/// Lives in `LuaShared::watchers`. Dropping the entry tears down the OS
/// subscription; the `state` arc is owned by both the notify closure and
/// the Lua API so events can still flow until drop.
pub struct WatcherEntry {
    pub state: Arc<Mutex<WatcherState>>,
    /// RAII handle: dropping the entry stops the OS subscription.
    pub _watcher: RecommendedWatcher,
}
