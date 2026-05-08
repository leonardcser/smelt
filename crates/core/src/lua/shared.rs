//! Shared Lua state: the `Arc<LuaShared>` that outlives callbacks and lets tokio tasks post resume payloads.

use super::{LuaHandle, LuaTaskRuntime, TaskEvent};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

pub struct RegisteredCommand {
    pub handle: LuaHandle,
    pub description: Option<String>,
    pub args: Vec<String>,
    /// If false, dispatcher rejects this command while the agent is mid-turn.
    pub while_busy: bool,
    /// If true, silently defers this command until the current turn ends rather than erroring.
    pub queue_when_busy: bool,
    pub startup_ok: bool,
    /// If true, hidden from the completer but still dispatchable by name.
    pub hidden: bool,
}

pub struct StatusSource {
    pub handle: LuaHandle,
    pub default_align_right: bool,
}

pub struct ToolHandles {
    pub execute: LuaHandle,
    pub confirm_text: Option<LuaHandle>,
    pub approval_patterns: Option<LuaHandle>,
    pub preflight: Option<LuaHandle>,
    pub render: Option<LuaHandle>,
    pub paths_for_workspace: Option<LuaHandle>,
    pub preview: Option<LuaHandle>,
    pub decide: Option<LuaHandle>,
}

/// All shared state between Lua closures and the app loop.
pub struct LuaShared {
    pub commands: Mutex<HashMap<String, RegisteredCommand>>,
    pub keymaps: Mutex<HashMap<(String, String), LuaHandle>>,
    /// Vec preserves registration order; re-registering an existing name updates in place.
    pub statusline_sources: Mutex<Vec<(String, StatusSource)>>,
    pub tools: Mutex<HashMap<String, ToolHandles>>,
    pub callbacks: Mutex<HashMap<u64, LuaHandle>>,
    pub next_id: AtomicU64,
    /// Starts at `1 << 32` so Lua-allocated `BufId`s never collide with Rust-side buffers.
    pub next_buf_id: AtomicU64,
    /// Lives on the shared arc (not `LuaTaskRuntime`) so a coroutine inside `drive_tasks`
    /// can mint an id without re-entering the `tasks` mutex.
    pub next_external_id: AtomicU64,
    pub tasks: Mutex<LuaTaskRuntime>,
    /// Resume-events for Lua coroutines; pumped each tick.
    pub task_inbox: Mutex<Vec<TaskEvent>>,
    /// Cross-thread inbox: tokio tasks push `(external_id, json)`; main loop drains to `task_inbox`.
    pub json_inbox: Arc<Mutex<Vec<(u64, serde_json::Value)>>>,
    /// Wakes the main loop when a tokio task pushes a JSON payload. Optional for trivial default.
    pub wakeup_tx: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<()>>,
    pub providers: Mutex<Vec<crate::config::ProviderConfig>>,
    pub permission_rules: Mutex<Option<crate::permissions::rules::RawPerms>>,
    pub mcp_configs: Mutex<HashMap<String, crate::mcp::McpServerConfig>>,
    pub settings_overrides: Mutex<HashMap<String, bool>>,
    pub tool_defaults: Mutex<crate::permissions::rules::ToolDefaults>,
    pub messages: Mutex<crate::messages::Messages>,
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
            tool_defaults: Mutex::new(crate::permissions::rules::ToolDefaults::default()),
            messages: Mutex::new(crate::messages::Messages::new()),
        }
    }
}

impl LuaShared {
    /// `Send`-safe handle for pushing JSON resume payloads from tokio tasks.
    /// `Arc<LuaShared>` is `!Send` (owns `mlua::Thread`s); this is the narrowest cross-thread surface.
    pub fn resume_sink(&self) -> LuaResumeSink {
        LuaResumeSink {
            inbox: Arc::clone(&self.json_inbox),
            wakeup: self.wakeup_tx.get().cloned(),
        }
    }
}

/// `Send`-safe handle for resuming a parked Lua coroutine from a tokio task.
#[derive(Clone)]
pub struct LuaResumeSink {
    inbox: Arc<Mutex<Vec<(u64, serde_json::Value)>>>,
    wakeup: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

impl LuaResumeSink {
    pub fn resolve_json(&self, external_id: u64, value: serde_json::Value) {
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.push((external_id, value));
        }
        if let Some(ref tx) = self.wakeup {
            let _ = tx.send(());
        }
    }
}
