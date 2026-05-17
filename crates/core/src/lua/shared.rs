//! Shared Lua state: the `Arc<LuaShared>` that outlives callbacks and lets tokio tasks post resume payloads.

use super::hooks::HookRegistry;
use super::{LuaHandle, LuaTaskRuntime, TaskEvent};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// Lua-allocated `BufId`s start here so they can never collide with the
/// Rust-side block-buffer id space (which begins at 1 and grows slowly).
/// `Ui::reap_anonymous` uses this threshold to decide which anonymous
/// bufs are safe to sweep on `/reload`.
pub const LUA_BUF_ID_BASE: u64 = 1u64 << 32;

/// Boot-time phase. Used by Lua API guards to refuse phase-sensitive
/// calls outside their valid window (e.g. `cli.register_flag` only runs
/// in `Early`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// `early.lua` is being evaluated. Only a restricted `smelt` table
    /// is visible; phase-sensitive APIs (`builtins`, `cli`, future
    /// pre-boot interceptors) are usable here.
    Early = 0,
    /// Autoload has run and `init.lua` is being evaluated. Full `smelt`
    /// surface is available.
    Init = 1,
    /// Steady state — the agent is running. Plugins added at this stage
    /// (e.g. from a `/cmd` handler) execute the same way as `Init`.
    Running = 2,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Early => "early",
            Phase::Init => "init",
            Phase::Running => "running",
        }
    }
    fn from_u8(n: u8) -> Self {
        match n {
            0 => Phase::Early,
            1 => Phase::Init,
            _ => Phase::Running,
        }
    }
}

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
    /// Starts at `LUA_BUF_ID_BASE` so Lua-allocated `BufId`s never collide with Rust-side buffers.
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
    /// Bundled `smelt.<dotted>` module names the user has opted out of via
    /// `smelt.builtins.disable{}` in `early.lua`. Read by `autoload_modules`
    /// to skip the matching `require()` calls. Names use the dotted module
    /// form, e.g. `"smelt.tools.web_search"`, `"smelt.commands.compact"`.
    pub disabled_modules: Mutex<HashSet<String>>,
    /// `package.loaded` keys present right after `Lua::new()` — the
    /// stdlib. `/reload` wipes everything outside this set.
    pub native_module_names: Mutex<HashSet<String>>,
    /// CLI flag specs registered from `early.lua` via `smelt.cli.register_flag`.
    /// The main binary reads this after the early phase and re-runs clap with
    /// the merged surface.
    pub cli_flag_specs: Mutex<Vec<CliFlagSpec>>,
    /// CLI flag *values* parsed from argv after `early.lua` declared them.
    /// Populated by the main binary; read by `smelt.cli.get(name)`.
    pub cli_flag_values: Mutex<HashMap<String, CliFlagValue>>,
    /// Every middleware/callback-list surface, grouped under one struct
    /// so the registry pattern is centralized and surfaces like
    /// `tools.middleware` / `provider.middleware` just allocate ids on
    /// the relevant `Arc<HookRegistry>`.
    pub hooks: Hooks,
    /// Default shell + argv for `smelt.process.run`/`run_streaming` when no
    /// explicit shell is given. `None` means hardcoded `sh -c`.
    pub default_shell: Mutex<Option<DefaultShell>>,
    /// Active filesystem watchers registered through `smelt.fs.watch`. The
    /// notify-backed `WatcherEntry` keeps the OS subscription alive; dropping
    /// the entry tears it down. `next_watcher_id` mints fresh ids.
    pub watchers: Mutex<HashMap<u64, crate::lua::watchers::WatcherEntry>>,
    pub next_watcher_id: AtomicU64,
    /// Current boot phase. Phase-sensitive APIs use this to gate their
    /// behavior (refuse-when-late or warn-when-late). Defaults to `Early`
    /// — the runtime promotes it to `Init` before autoload and `Running`
    /// once the agent loop is live.
    phase: AtomicU8,
}

/// Every hook-registry surface bundled into one struct. New middleware
/// streams (e.g. an `agent.middleware`) get a field here rather than a
/// fresh `Mutex<Vec<HookEntry>>` on `LuaShared`. Cheap to clone the
/// inner `Arc`s; consumers grab `Arc::clone(&shared.hooks.tool_before)`
/// when they need a long-lived handle.
#[derive(Default)]
pub struct Hooks {
    /// `tools.middleware{before=...}` registry. Per-tool name; `""`
    /// matches every tool.
    pub tool_before: Arc<HookRegistry>,
    /// `tools.middleware{after=...}` registry. Per-tool name; `""`
    /// matches every tool.
    pub tool_after: Arc<HookRegistry>,
    /// `provider.middleware{on_request=...}` registry. Provider hooks
    /// always use `name = ""`.
    pub provider_request: Arc<HookRegistry>,
    /// `provider.middleware{on_response=...}` registry.
    pub provider_response: Arc<HookRegistry>,
}

/// Spec for a Lua-declared CLI flag. Mirrors the subset of clap we need.
#[derive(Clone, Debug)]
pub struct CliFlagSpec {
    pub name: String,
    pub kind: CliFlagKind,
    pub default: CliFlagValue,
    pub description: Option<String>,
    pub short: Option<char>,
    pub long: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliFlagKind {
    Boolean,
    String,
    Integer,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CliFlagValue {
    Boolean(bool),
    String(String),
    Integer(i64),
    None,
}

/// Default shell used by `smelt.process.run`/`run_streaming` when no
/// per-call override is provided. `program` is the executable; `args`
/// are the leading argv slots before the command string is appended
/// (e.g. `{ "-fc" }`, `{ "-c" }`).
#[derive(Clone, Debug)]
pub struct DefaultShell {
    pub program: String,
    pub args: Vec<String>,
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
            next_buf_id: AtomicU64::new(LUA_BUF_ID_BASE),
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
            disabled_modules: Mutex::new(HashSet::new()),
            native_module_names: Mutex::new(HashSet::new()),
            cli_flag_specs: Mutex::new(Vec::new()),
            cli_flag_values: Mutex::new(HashMap::new()),
            hooks: Hooks::default(),
            default_shell: Mutex::new(None),
            watchers: Mutex::new(HashMap::new()),
            next_watcher_id: AtomicU64::new(1),
            phase: AtomicU8::new(Phase::Early as u8),
        }
    }
}

impl LuaShared {
    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Acquire))
    }
    pub fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Ordering::Release);
    }

    /// Drop every Lua handle from the registries `/reload` repopulates:
    /// commands, keymaps, statusline sources, tools, callbacks, hooks.
    pub fn clear_lua_handles(&self) {
        if let Ok(mut m) = self.commands.lock() {
            m.clear();
        }
        if let Ok(mut m) = self.keymaps.lock() {
            m.clear();
        }
        if let Ok(mut v) = self.statusline_sources.lock() {
            v.clear();
        }
        if let Ok(mut m) = self.tools.lock() {
            m.clear();
        }
        if let Ok(mut m) = self.callbacks.lock() {
            m.clear();
        }
        self.hooks.tool_before.clear();
        self.hooks.tool_after.clear();
        self.hooks.provider_request.clear();
        self.hooks.provider_response.clear();
        if let Ok(mut m) = self.watchers.lock() {
            m.clear();
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

    /// Run `work` on tokio's blocking-thread pool and resolve `task_id`
    /// with the produced JSON payload. The common bridge for sync work
    /// (file I/O, blocking process calls) that needs to surface back to
    /// a yielded Lua coroutine without blocking the main loop.
    pub fn spawn_blocking_resolve<F>(self, task_id: u64, work: F)
    where
        F: FnOnce() -> serde_json::Value + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            let payload = work();
            self.resolve_json(task_id, payload);
        });
    }
}
