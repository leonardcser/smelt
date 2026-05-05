//! Shared Lua state — the `Arc<LuaShared>` that outlives individual
//! callbacks and lets tokio tasks post resume payloads back to the
//! main thread.

use super::{LuaHandle, LuaTaskRuntime, TaskEvent};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

/// One Lua-registered `/command` entry. Lives in `LuaShared.commands`
/// so completers (`list_commands`, `is_lua_command`) read the same
/// map the dispatcher does — no parallel snapshot.
pub struct RegisteredCommand {
    pub handle: LuaHandle,
    pub description: Option<String>,
    pub args: Vec<String>,
    /// May this command run while the agent is mid-turn? Defaults to
    /// `true`. Plugins like `/compact` / `/fork` / `/resume` set
    /// `while_busy = false` so the dispatcher rejects them with
    /// `cannot run /name while agent is working` instead of queueing.
    pub while_busy: bool,
    /// Should this command queue as a regular message when invoked
    /// while the agent is mid-turn? Defaults to `false`. User-defined
    /// custom commands (which spawn their own turn) opt in so the
    /// dispatcher silently defers them until the current turn ends
    /// instead of erroring or running mid-turn.
    pub queue_when_busy: bool,
    /// May this command be invoked as a startup argument
    /// (`smelt /name`)? Defaults to `false`; plugins that open a UI
    /// useful at launch (`/resume`, `/settings`) opt in.
    pub startup_ok: bool,
}

/// One registered `smelt.statusline.register` entry. `default_align`
/// applies to items the source returns without an explicit
/// `align_right` field; items can still override per-item.
pub struct StatusSource {
    pub handle: LuaHandle,
    pub default_align_right: bool,
}

/// Handles for a plugin tool registered via `smelt.tools.register`.
pub struct ToolHandles {
    pub execute: LuaHandle,
    pub needs_confirm: Option<LuaHandle>,
    pub approval_patterns: Option<LuaHandle>,
    pub preflight: Option<LuaHandle>,
    pub render: Option<LuaHandle>,
    pub render_summary: Option<LuaHandle>,
    pub render_subhead: Option<LuaHandle>,
    pub header_suffix: Option<LuaHandle>,
    pub paths_for_workspace: Option<LuaHandle>,
    pub preview: Option<LuaHandle>,
}

/// All shared state between Lua closures and the app loop.
/// One `Arc<LuaShared>` replaces N separate `Arc<Mutex<…>>` fields.
///
/// The TUI wraps this in a local type that adds
/// `pending_invocations` (UI-specific callback queue).
pub struct LuaShared {
    pub commands: Mutex<HashMap<String, RegisteredCommand>>,
    pub keymaps: Mutex<HashMap<(String, String), LuaHandle>>,
    /// Statusline sources in registration order. A `Vec` (not a
    /// `HashMap`) so the on-screen left-to-right order matches the
    /// order plugins called `smelt.statusline.register`. Re-registering
    /// an existing name updates in place without changing position.
    pub statusline_sources: Mutex<Vec<(String, StatusSource)>>,
    pub tools: Mutex<HashMap<String, ToolHandles>>,
    pub callbacks: Mutex<HashMap<u64, LuaHandle>>,
    pub next_id: AtomicU64,
    /// Separate counter for buffer IDs minted by `smelt.buf.create`.
    /// Starts at `1 << 32` so Lua-allocated `BufId`s never collide with
    /// Rust-side buffers (prompt input, scratch, etc.) that are minted
    /// by `ui.buf_create` from 1.
    pub next_buf_id: AtomicU64,
    /// Lock-free counter for `smelt.task.alloc`. Lives on the
    /// shared arc (not in `LuaTaskRuntime`) so a Lua coroutine running
    /// *inside* `drive_tasks` — which already holds the `tasks` lock —
    /// can mint an id without re-entering the same mutex.
    pub next_external_id: AtomicU64,
    pub tasks: Mutex<LuaTaskRuntime>,
    /// Task-runtime inbox. Dialog callbacks / other UI events that need
    /// to *resume a Lua coroutine* push here instead of through `ops`.
    /// Keeps the reducer's `AppOp` enum free of Lua-task variants; the
    /// Lua module pumps its own inbox each tick.
    pub task_inbox: Mutex<Vec<TaskEvent>>,
    /// Cross-thread JSON inbox mirroring `task_inbox`. tokio tasks
    /// push `(external_id, json)` tuples; the main loop drains
    /// them into `task_inbox` (as `ExternalResolvedJson`) before
    /// pumping. Wrapped in `Arc<Mutex<...>>` so the
    /// `LuaResumeSink` clone the tokio task holds is `Send`.
    pub json_inbox: Arc<Mutex<Vec<(u64, serde_json::Value)>>>,
    /// Sender that wakes the main loop when a tokio task pushes a
    /// JSON resume payload from outside the main thread. The
    /// receiver lives on the host; its `select!` arm flushes the
    /// inbox and renders. Optional so `LuaShared::default()` stays
    /// trivially constructable.
    pub wakeup_tx: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<()>>,
    // ── Config registries (populated by init.lua before engine starts) ───────
    pub providers: Mutex<Vec<crate::config::ProviderConfig>>,
    pub permission_rules: Mutex<Option<crate::permissions::rules::RawPerms>>,
    pub mcp_configs: Mutex<HashMap<String, crate::mcp::McpServerConfig>>,
    pub settings_overrides: Mutex<HashMap<String, String>>,
}

impl Default for LuaShared {
    fn default() -> Self {
        Self {
            commands: Mutex::new(HashMap::new()),
            keymaps: Mutex::new(HashMap::new()),
            statusline_sources: Mutex::new(Vec::new()),
            tools: Mutex::new(HashMap::new()),
            callbacks: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            next_buf_id: AtomicU64::new(1 << 32),
            next_external_id: AtomicU64::new(1),
            tasks: Mutex::new(LuaTaskRuntime::new()),
            task_inbox: Mutex::new(Vec::new()),
            json_inbox: Arc::new(Mutex::new(Vec::new())),
            wakeup_tx: std::sync::OnceLock::new(),
            providers: Mutex::new(Vec::new()),
            permission_rules: Mutex::new(None),
            mcp_configs: Mutex::new(HashMap::new()),
            settings_overrides: Mutex::new(HashMap::new()),
        }
    }
}

impl LuaShared {
    /// Build a `Send`-safe handle that lets a tokio task push a
    /// JSON resume payload and wake the main loop. `Arc<LuaShared>`
    /// itself is `!Send` (it owns `mlua::Thread`s inside `tasks`);
    /// the resume sink is the narrowest cross-thread surface.
    pub fn resume_sink(&self) -> LuaResumeSink {
        LuaResumeSink {
            inbox: Arc::clone(&self.json_inbox),
            wakeup: self.wakeup_tx.get().cloned(),
        }
    }
}

/// Send-safe handle a tokio task uses to resume a parked Lua
/// coroutine from outside the main thread. Stores into a
/// `LuaShared.json_inbox` mirror; the main loop drains that into
/// the runtime's `task_inbox` before pumping.
#[derive(Clone)]
pub struct LuaResumeSink {
    inbox: Arc<Mutex<Vec<(u64, serde_json::Value)>>>,
    wakeup: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

impl LuaResumeSink {
    /// Push a JSON resume payload. Wakes the main loop so the
    /// runtime pumps the inbox on the next iteration.
    pub fn resolve_json(&self, external_id: u64, value: serde_json::Value) {
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.push((external_id, value));
        }
        if let Some(ref tx) = self.wakeup {
            let _ = tx.send(());
        }
    }
}
