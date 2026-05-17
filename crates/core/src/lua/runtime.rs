//! Headless-safe Lua runtime. The TUI extends this with UI-specific queues and statusline rendering.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use include_dir::{include_dir, Dir, DirEntry};
use mlua::prelude::*;

use crate::lua::{
    json_to_lua, LuaShared, TaskCompletion, TaskDriveOutput, TaskEvent, ToolEnv, ToolExecResult,
};

/// Embedded `runtime/lua/smelt/` tree; every `.lua` file is `require`-able as `smelt.<path>`.
static EMBEDDED_LUA: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../runtime/lua/smelt");

/// Lua chunks executed at `register_api` time, in order: primitives before consumers.
const BOOTSTRAP_FILES: &[&str] = &[
    "_bootstrap.lua",
    "dialog.lua",
    "widgets/picker.lua",
    "widgets/prompt_picker.lua",
    "cmd.lua",
    "dialogs/confirm.lua",
    "status.lua",
    "modes.lua",
];

/// Subdirectories whose files are `require`'d at startup as side-effect registrations.
const AUTOLOAD_DIRS: &[&str] = &["tools", "commands", "plugins", "dialogs"];

/// Bundled plugins that ship with smelt but are NOT autoloaded. Users opt in by
/// calling `require("smelt.plugins.<name>")` from their `init.lua`.
const OPTIONAL_PLUGINS: &[&str] = &[
    "smelt.plugins.background_commands",
    "smelt.plugins.plan_mode",
];

/// Outcome of dispatching a keymap chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeymapResult {
    /// Handler ran and returned truthy or nothing.
    Consumed,
    /// Handler ran and returned `false`; key falls through.
    PassThrough,
    NoBinding,
}

/// Context passed to a tool's `render(args, output, ctx)` hook.
pub struct ToolRenderCtx<'a> {
    pub width: usize,
    pub summary: &'a str,
    /// `"pending" | "ok" | "err" | "denied" | "confirm"`
    pub status: &'a str,
    /// `None` while the call is still running.
    pub elapsed_secs: Option<u64>,
    /// `None` for synthetic renders (preview / dialog title).
    pub call_id: Option<&'a str>,
}

/// Headless-safe Lua runtime.
pub struct LuaRuntime {
    pub lua: Lua,
    pub load_error: Option<String>,
    shared: Arc<LuaShared>,
    init_lua_path: Option<PathBuf>,
}

impl Default for LuaRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaRuntime {
    /// Build a fresh runtime and register the `smelt` global with
    /// Host-tier APIs only.
    pub fn new() -> Self {
        let lua = Lua::new();
        #[allow(clippy::arc_with_non_send_sync)]
        let shared = Arc::new(LuaShared::default());

        let load_error = Self::register_api(&lua, &shared)
            .err()
            .map(|e| e.to_string());

        let mut rt = Self {
            lua,
            load_error,
            shared,
            init_lua_path: None,
        };

        if rt.load_error.is_none() {
            if let Err(e) = register_embedded_searcher(&rt.lua) {
                rt.load_error = Some(format!("embedded searcher: {e}"));
            }
        }
        rt.snapshot_native_modules();

        rt
    }

    pub fn with_shared(shared: Arc<LuaShared>) -> Self {
        let lua = Lua::new();
        let load_error = Self::register_api(&lua, &shared)
            .err()
            .map(|e| e.to_string());

        let mut rt = Self {
            lua,
            load_error,
            shared,
            init_lua_path: None,
        };

        if rt.load_error.is_none() {
            if let Err(e) = register_embedded_searcher(&rt.lua) {
                rt.load_error = Some(format!("embedded searcher: {e}"));
            }
        }
        rt.snapshot_native_modules();

        rt
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn set_init_lua_path(&mut self, path: PathBuf) {
        self.init_lua_path = Some(path);
    }

    pub fn load_user_config(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let path = self.init_lua_path.clone().or_else(init_lua_path);
        if let Some(path) = path {
            if path.exists() {
                if let Err(e) = self.load_init(&path) {
                    let label = self
                        .init_lua_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "~/.config/smelt/init.lua".to_string());
                    self.load_error = Some(format!("{label}: {e}"));
                }
            }
        }
    }

    /// Evaluate `~/.config/smelt/early.lua` if present. Intended to run
    /// BEFORE [`autoload_modules`] so a user can call
    /// `smelt.builtins.disable{}` / `smelt.cli.register_flag{}` and
    /// have them take effect on the upcoming auto-load and argv parse.
    /// During evaluation the global `smelt` table is swapped for a
    /// restricted view exposing only the phase-zero namespaces
    /// (`builtins`, `cli`, `phase`, `provider`). Calls to any other
    /// namespace error with a clear message. The full `smelt` is
    /// restored afterward. No-op when the file is missing.
    pub fn load_early_init(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let Some(path) = early_lua_path() else {
            return;
        };
        if !path.exists() {
            return;
        }
        if let Err(e) = self.run_early_phase(&path, "early.lua") {
            self.load_error = Some(format!("{}: {e}", path.display()));
        }
    }

    /// Evaluate `.smelt/early.lua` if present and the project is trusted.
    /// Companion to [`Self::load_early_init`] for project-scoped early
    /// config. No-op when the file is missing or the project is not
    /// trusted.
    pub fn load_project_early_init(&mut self, cwd: &std::path::Path) {
        if self.load_error.is_some() {
            return;
        }
        let state = crate::trust::project_trust_state(cwd);
        if !matches!(state, crate::trust::TrustState::Trusted { .. }) {
            return;
        }
        let path = cwd.join(".smelt").join("early.lua");
        if !path.exists() {
            return;
        }
        if let Err(e) = self.run_early_phase(&path, ".smelt/early.lua") {
            self.load_error = Some(format!("{}: {e}", path.display()));
        }
    }

    fn run_early_phase(&mut self, path: &std::path::Path, name: &str) -> LuaResult<()> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| LuaError::RuntimeError(format!("read {name}: {e}")))?;
        // Save full smelt, install restricted view, eval, restore.
        let full_smelt: mlua::Table = self.lua.globals().get("smelt")?;
        let restricted = self.build_early_smelt_view(&full_smelt)?;
        self.lua.globals().set("smelt", restricted)?;
        self.shared.set_phase(crate::lua::Phase::Early);
        let result = self.lua.load(&src).set_name(name).exec();
        self.lua.globals().set("smelt", full_smelt)?;
        result
    }

    /// Build the restricted `smelt` table seen by `early.lua`. Exposes
    /// only the phase-zero namespaces; access to anything else returns
    /// nil, so calling e.g. `smelt.cmd.register(...)` from early.lua
    /// errors with "attempt to call nil" — loud, immediate, traceable.
    fn build_early_smelt_view(&self, full: &mlua::Table) -> LuaResult<mlua::Table> {
        const ALLOWED: &[&str] = &["builtins", "cli", "phase", "provider"];
        let view = self.lua.create_table()?;
        for ns in ALLOWED {
            if let Ok(v) = full.get::<mlua::Value>(*ns) {
                view.set(*ns, v)?;
            }
        }
        Ok(view)
    }

    /// Snapshot of the modules a user has disabled via `smelt.builtins.disable`.
    pub fn disabled_modules(&self) -> std::collections::HashSet<String> {
        self.shared
            .disabled_modules
            .lock()
            .map(|set| set.clone())
            .unwrap_or_default()
    }

    /// Promote the boot phase. The host loop should call:
    /// 1. nothing (`Early` is the default at construction)
    /// 2. `mark_init()` after `early.lua` has run and before autoload
    /// 3. `mark_running()` once the agent loop is live
    pub fn mark_init(&self) {
        self.shared.set_phase(crate::lua::Phase::Init);
    }
    pub fn mark_running(&self) {
        self.shared.set_phase(crate::lua::Phase::Running);
    }
    pub fn current_phase(&self) -> crate::lua::Phase {
        self.shared.phase()
    }

    /// Stash the stdlib keys in `package.loaded` so `/reload` knows what
    /// not to wipe. Called once from the constructor.
    fn snapshot_native_modules(&self) {
        let names: std::collections::HashSet<String> = (|| -> LuaResult<_> {
            let package: mlua::Table = self.lua.globals().get("package")?;
            let loaded: mlua::Table = package.get("loaded")?;
            let mut out = std::collections::HashSet::new();
            for (k, _) in loaded.pairs::<String, mlua::Value>().flatten() {
                out.insert(k);
            }
            Ok(out)
        })()
        .unwrap_or_default();
        if let Ok(mut set) = self.shared.native_module_names.lock() {
            *set = names;
        }
    }

    /// Nil out every `package.loaded` entry outside the stdlib snapshot
    /// so the next `require()` re-runs the module body.
    fn wipe_loaded_modules(&self) {
        let snapshot = match self.shared.native_module_names.lock() {
            Ok(s) => s.clone(),
            Err(_) => return,
        };
        let _ = (|| -> LuaResult<()> {
            let package: mlua::Table = self.lua.globals().get("package")?;
            let loaded: mlua::Table = package.get("loaded")?;
            let mut to_drop = Vec::new();
            for (k, _) in loaded.pairs::<String, mlua::Value>().flatten() {
                if !snapshot.contains(&k) {
                    to_drop.push(k);
                }
            }
            for k in to_drop {
                loaded.set(k, mlua::Value::Nil)?;
            }
            Ok(())
        })();
    }

    /// Move phase from `Early` to `Init`, then `require` every autoload
    /// module (modulo `smelt.builtins.disable{}` opt-outs from
    /// `early.lua`). First failure short-circuits onto `load_error`.
    pub fn load_autoload(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        self.mark_init();
        let disabled = self.disabled_modules();
        for name in autoload_modules_filtered(&disabled) {
            let code = format!("require('{name}')");
            if let Err(e) = self.lua.load(&code).set_name(name.as_str()).exec() {
                self.load_error = Some(format!("autoload {name}: {e}"));
                return;
            }
        }
    }

    /// Clear every Lua-owned registry, wipe non-stdlib `package.loaded`,
    /// re-run bootstrap (idempotent), then re-run autoload → user init →
    /// global plugins → project config. After loading, sweep stale
    /// `smelt.state` slots no plugin touched this cycle. `early.lua` is
    /// skipped (CLI flags and `builtins.disable` are startup-only).
    /// Returns any load error.
    pub fn reload(&mut self, cwd: Option<&std::path::Path>) -> Option<String> {
        self.load_error = None;
        self.clear_for_reload();
        if let Err(e) = load_bootstrap_chunks(&self.lua) {
            self.load_error = Some(format!("bootstrap: {e}"));
            return self.load_error.clone();
        }
        self.load_autoload();
        self.load_user_config();
        self.load_global_plugins();
        if let Some(cwd) = cwd {
            let _ = self.load_project_config(cwd);
        }
        let _ = self.lua.load("smelt.__sweep_state()").exec();
        self.load_error.clone()
    }

    /// **Single ledger** of every Lua-side surface wiped at the top of a
    /// `/reload` cycle. Add new `LuaShared` registries here — `reload()`
    /// is the only caller, and the matching reload-survival test asserts
    /// every clearable surface is empty after calling this.
    ///
    /// Order matters: cancel in-flight tasks *before* clearing handles so
    /// no parked coroutine resumes with stale registry keys; drain inboxes
    /// before re-running modules so the new cycle starts with an empty
    /// resume queue.
    fn clear_for_reload(&mut self) {
        if let Ok(mut tasks) = self.shared.tasks.lock() {
            tasks.cancel_and_clear();
        }
        if let Ok(mut q) = self.shared.task_inbox.lock() {
            q.clear();
        }
        if let Ok(mut q) = self.shared.json_inbox.lock() {
            q.clear();
        }
        self.shared.clear_lua_handles();
        self.wipe_loaded_modules();
    }

    pub fn load_init(&mut self, path: &std::path::Path) -> LuaResult<()> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| LuaError::RuntimeError(format!("read init.lua: {e}")))?;
        self.lua.load(&src).set_name("init.lua").exec()
    }

    pub fn load_global_plugins(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let dir = crate::config::config_dir().join("plugins");
        for path in lua_files_in(&dir) {
            if let Err(e) = self.load_plugin_file(&path) {
                self.load_error = Some(format!("{}: {e}", path.display()));
                return;
            }
        }
    }

    /// Load `.smelt/init.lua` and `.smelt/plugins/*.lua`, gated by trust. Returns the trust state.
    pub fn load_project_config(&mut self, cwd: &std::path::Path) -> crate::trust::TrustState {
        let state = crate::trust::project_trust_state(cwd);
        if !matches!(state, crate::trust::TrustState::Trusted { .. }) {
            return state;
        }
        if self.load_error.is_some() {
            return state;
        }
        let smelt_dir = cwd.join(".smelt");
        for path in lua_files_in(&smelt_dir.join("plugins")) {
            if let Err(e) = self.load_plugin_file(&path) {
                self.load_error = Some(format!("{}: {e}", path.display()));
                return state;
            }
        }
        let init = smelt_dir.join("init.lua");
        if init.exists() {
            if let Err(e) = self.load_init(&init) {
                self.load_error = Some(format!("{}: {e}", init.display()));
            }
        }
        state
    }

    fn load_plugin_file(&self, path: &std::path::Path) -> LuaResult<()> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| LuaError::RuntimeError(format!("read {}: {e}", path.display())))?;
        self.lua
            .load(&src)
            .set_name(path.display().to_string())
            .exec()
    }

    pub fn to_config(&self) -> crate::config::Config {
        let providers = self
            .shared
            .providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mcp = self
            .shared
            .mcp_configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut settings = crate::config::SettingsConfig::default();
        let overrides = self
            .shared
            .settings_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (key, value) in overrides.iter() {
            if let Err(e) = settings.set_bool(key, *value) {
                eprintln!("settings override: {e}");
            }
        }
        crate::config::Config {
            providers,
            mcp,
            settings,
            ..Default::default()
        }
    }

    pub fn take_permission_rules(&self) -> Option<crate::permissions::rules::RawPerms> {
        self.shared
            .permission_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    pub fn tool_defaults(&self) -> crate::permissions::rules::ToolDefaults {
        self.shared
            .tool_defaults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn run_command(&self, name: &str, arg: Option<String>) -> bool {
        let func = {
            let Ok(map) = self.shared.commands.lock() else {
                return false;
            };
            let Some(entry) = map.get(name) else {
                return false;
            };
            let Ok(f) = self.lua.registry_value::<mlua::Function>(&entry.handle.key) else {
                return false;
            };
            f
        };
        let _perf = smelt_perf::perf::begin("lua:cmd");
        let result: LuaResult<()> = match arg {
            Some(a) => func.call::<()>(a),
            None => func.call::<()>(()),
        };
        if let Err(e) = result {
            self.record_error(format!("cmd `{name}`: {e}"));
        }
        true
    }

    /// Dispatch a keymap chord to any Lua-registered handler.
    /// `chord_ctx` is passed as a table for multi-key sequences; `None` for single-key.
    pub fn run_keymap(
        &self,
        chord: &str,
        current_mode: Option<&str>,
        chord_ctx: Option<&[(&str, String)]>,
    ) -> KeymapResult {
        let func = {
            let Ok(map) = self.shared.keymaps.lock() else {
                return KeymapResult::NoBinding;
            };
            let mode_char = current_mode.map(|m| match m {
                "Normal" => "n",
                "Insert" => "i",
                "Visual" => "v",
                _ => "n",
            });
            let handle = mode_char
                .and_then(|mc| map.get(&(mc.to_string(), chord.to_string())))
                .or_else(|| map.get(&(String::new(), chord.to_string())));
            let Some(handle) = handle else {
                return KeymapResult::NoBinding;
            };
            let Ok(f) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
                return KeymapResult::NoBinding;
            };
            f
        };
        let result = match chord_ctx {
            Some(pairs) => {
                let ctx = match self.lua.create_table() {
                    Ok(t) => t,
                    Err(e) => {
                        self.record_error(format!("keymap `{chord}`: {e}"));
                        return KeymapResult::Consumed;
                    }
                };
                for (k, v) in pairs {
                    if let Err(e) = ctx.set(*k, v.as_str()) {
                        self.record_error(format!("keymap `{chord}`: {e}"));
                        return KeymapResult::Consumed;
                    }
                }
                let _perf = smelt_perf::perf::begin("lua:keymap");
                func.call::<mlua::Value>(ctx)
            }
            None => {
                let _perf = smelt_perf::perf::begin("lua:keymap");
                func.call::<mlua::Value>(())
            }
        };
        match result {
            Ok(mlua::Value::Boolean(false)) => KeymapResult::PassThrough,
            Ok(_) => KeymapResult::Consumed,
            Err(e) => {
                self.record_error(format!("keymap `{chord}`: {e}"));
                KeymapResult::Consumed
            }
        }
    }

    /// Returns true if `sequence` is a strict prefix of a registered chord (exact match excluded).
    pub fn chord_has_longer(&self, sequence: &str, current_mode: Option<&str>) -> bool {
        let Ok(map) = self.shared.keymaps.lock() else {
            return false;
        };
        let mode_char = match current_mode {
            Some("Normal") => "n",
            Some("Insert") => "i",
            Some("Visual") => "v",
            _ => "n",
        };
        for (m, chord) in map.keys() {
            if (m == mode_char || m.is_empty())
                && chord.len() > sequence.len()
                && chord.starts_with(sequence)
            {
                return true;
            }
        }
        false
    }

    pub fn cycle_mode(&self) {
        let result: mlua::Result<()> = (|| {
            let smelt: mlua::Table = self.lua.globals().get("smelt")?;
            let mode: mlua::Table = smelt.get("mode")?;
            let cycle: mlua::Function = mode.get("cycle")?;
            cycle.call::<()>(())
        })();
        if let Err(e) = result {
            self.record_error(format!("smelt.mode.cycle: {e}"));
        }
    }

    pub fn cycle_reasoning(&self) {
        let result: mlua::Result<()> = (|| {
            let smelt: mlua::Table = self.lua.globals().get("smelt")?;
            let reasoning: mlua::Table = smelt.get("reasoning")?;
            let cycle: mlua::Function = reasoning.get("cycle")?;
            cycle.call::<()>(())
        })();
        if let Err(e) = result {
            self.record_error(format!("smelt.reasoning.cycle: {e}"));
        }
    }

    /// Log to the persistent message store and surface a one-line summary via `smelt.notify.error`.
    pub fn record_error(&self, msg: String) {
        let summary = msg.lines().next().unwrap_or("").to_string();
        if let Ok(mut messages) = self.shared.messages.lock() {
            messages.append(crate::messages::MessageKind::Error, "lua".to_string(), msg);
        }
        if let Ok(smelt) = self.lua.globals().get::<mlua::Table>("smelt") {
            if let Ok(notify) = smelt.get::<mlua::Table>("notify") {
                if let Ok(func) = notify.get::<mlua::Function>("error") {
                    let _ = func.call::<()>(summary);
                }
            }
        }
    }

    pub fn has_command(&self, name: &str) -> bool {
        self.shared
            .commands
            .lock()
            .map(|m| m.contains_key(name))
            .unwrap_or(false)
    }

    pub fn command_blocks_while_busy(&self, name: &str) -> Option<bool> {
        self.shared
            .commands
            .lock()
            .ok()?
            .get(name)
            .map(|c| !c.while_busy)
    }

    pub fn command_queues_when_busy(&self, name: &str) -> bool {
        self.shared
            .commands
            .lock()
            .ok()
            .and_then(|m| m.get(name).map(|c| c.queue_when_busy))
            .unwrap_or(false)
    }

    pub fn command_startup_ok(&self, name: &str) -> Option<bool> {
        self.shared
            .commands
            .lock()
            .ok()?
            .get(name)
            .map(|c| c.startup_ok)
    }

    pub fn command_names(&self) -> Vec<String> {
        self.shared
            .commands
            .lock()
            .map(|m| {
                let mut v: Vec<String> = m.keys().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    pub fn list_commands_with_desc(&self) -> Vec<(String, Option<String>)> {
        let mut items: Vec<(String, Option<String>)> = self
            .shared
            .commands
            .lock()
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| !v.hidden)
                    .map(|(k, v)| (k.clone(), v.description.clone()))
                    .collect()
            })
            .unwrap_or_default();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        items
    }

    pub fn list_command_args(&self) -> Vec<(String, Vec<String>)> {
        let mut items: Vec<(String, Vec<String>)> = self
            .shared
            .commands
            .lock()
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| !v.args.is_empty())
                    .map(|(k, v)| (format!("/{k}"), v.args.clone()))
                    .collect()
            })
            .unwrap_or_default();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        items
    }

    pub fn fire_callback(&self, id: u64, content: &str) {
        let handle = {
            let Ok(mut cbs) = self.shared.callbacks.lock() else {
                return;
            };
            match cbs.remove(&id) {
                Some(h) => h,
                None => return,
            }
        };
        let Ok(func) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
            return;
        };
        let _perf = smelt_perf::perf::begin("lua:ask_cb");
        if let Err(e) = func.call::<()>(content.to_string()) {
            self.record_error(format!("ask callback: {e}"));
        }
    }

    pub fn remove_callback(&self, id: u64) {
        if let Ok(mut cbs) = self.shared.callbacks.lock() {
            cbs.remove(&id);
        }
    }

    pub fn resolve_external(&self, external_id: u64, value: mlua::Value) -> bool {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return false;
        };
        rt.resolve_external(external_id, value)
    }

    pub fn resolve_core_tool_call(
        &self,
        request_id: u64,
        content: String,
        is_error: bool,
        metadata: Option<serde_json::Value>,
    ) {
        let table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tools.call result table: {e}"));
                return;
            }
        };
        if let Err(e) = table.set("content", content) {
            self.record_error(format!("tools.call result.content: {e}"));
            return;
        }
        if let Err(e) = table.set("is_error", is_error) {
            self.record_error(format!("tools.call result.is_error: {e}"));
            return;
        }
        if let Some(meta) = metadata {
            match json_to_lua(&self.lua, &meta) {
                Ok(v) => {
                    let _ = table.set("metadata", v);
                }
                Err(e) => self.record_error(format!("tools.call result.metadata: {e}")),
            }
        }
        self.resolve_external(request_id, mlua::Value::Table(table));
    }

    pub fn pump_task_events(&self) {
        let json_pending: Vec<(u64, serde_json::Value)> = {
            let Ok(mut inbox) = self.shared.json_inbox.lock() else {
                return;
            };
            std::mem::take(&mut *inbox)
        };
        if !json_pending.is_empty() {
            if let Ok(mut main) = self.shared.task_inbox.lock() {
                for (external_id, value) in json_pending {
                    main.push(TaskEvent::ExternalResolvedJson { external_id, value });
                }
            }
        }
        let events: Vec<TaskEvent> = {
            let Ok(mut inbox) = self.shared.task_inbox.lock() else {
                return;
            };
            std::mem::take(&mut *inbox)
        };
        for ev in events {
            match ev {
                TaskEvent::ExternalResolved { external_id, value } => {
                    let v = self.lua.registry_value(&value).unwrap_or(mlua::Value::Nil);
                    self.resolve_external(external_id, v);
                }
                TaskEvent::ExternalResolvedJson { external_id, value } => {
                    let v = json_to_lua(&self.lua, &value).unwrap_or(mlua::Value::Nil);
                    self.resolve_external(external_id, v);
                }
            }
        }
    }

    pub fn cancel_tasks(&self) {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return;
        };
        rt.cancel_all(&self.lua);
    }

    pub fn drive_tasks(&self, now: Instant) -> Vec<TaskDriveOutput> {
        let mut outs = Vec::new();
        // Step one task at a time without holding the `tasks` mutex across the
        // resume. Re-entrant `smelt.spawn` / `Reg::remove()` calls from inside
        // the coroutine acquire the lock synchronously instead of deadlocking.
        loop {
            let task = {
                let Ok(mut rt) = self.shared.tasks.lock() else {
                    return Vec::new();
                };
                rt.take_next_ready(now)
            };
            let Some(task) = task else { break };
            if let Some(parked) = crate::lua::step_task_owned(&self.lua, task, now, &mut outs) {
                let Ok(mut rt) = self.shared.tasks.lock() else {
                    return Vec::new();
                };
                rt.put_back(parked);
            }
        }
        let mut forward = Vec::with_capacity(outs.len());
        for out in outs {
            match out {
                TaskDriveOutput::ToolComplete { .. } => forward.push(out),
                TaskDriveOutput::Error(msg) => self.record_error(msg),
            }
        }
        forward
    }

    pub fn tool_defs(&self, _mode: protocol::AgentMode) -> Vec<protocol::ToolDef> {
        let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<String> = handlers.keys().cloned().collect();
        drop(handlers);
        names.sort();
        let mut defs = Vec::new();
        for name in &names {
            if let Ok(meta_table) = self
                .lua
                .named_registry_value::<mlua::Table>(&format!("__pt_meta_{name}"))
            {
                let description: String = meta_table.get("description").unwrap_or_default();
                let parameters: serde_json::Value = meta_table
                    .get::<mlua::String>("parameters_json")
                    .ok()
                    .and_then(|s| serde_json::from_str(&s.to_string_lossy()).ok())
                    .unwrap_or(serde_json::json!({"type": "object", "properties": {}}));
                let modes: Option<Vec<protocol::AgentMode>> =
                    meta_table.get::<mlua::Table>("modes").ok().map(|t| {
                        t.sequence_values::<String>()
                            .filter_map(|r| r.ok())
                            .filter_map(|s| protocol::AgentMode::parse(&s))
                            .collect()
                    });
                let execution_mode = meta_table
                    .get::<String>("execution_mode")
                    .ok()
                    .and_then(|s| match s.as_str() {
                        "sequential" => Some(protocol::ToolExecutionMode::Sequential),
                        "concurrent" => Some(protocol::ToolExecutionMode::Concurrent),
                        _ => None,
                    })
                    .unwrap_or_default();
                let hooks = protocol::ToolHookFlags {
                    approval_patterns: meta_table.get("hook_approval_patterns").unwrap_or(false),
                    preflight: meta_table.get("hook_preflight").unwrap_or(false),
                };
                let override_core: bool = meta_table.get("override_core").unwrap_or(false);
                defs.push(protocol::ToolDef {
                    name: name.clone(),
                    description,
                    parameters,
                    modes,
                    execution_mode,
                    hooks,
                    override_core,
                });
            }
        }
        defs
    }

    /// Call a tool's `paths_for_workspace(args)` callback; returns touched paths for boundary checks.
    pub fn tool_paths_for_workspace(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Vec<String> {
        let func = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(h) = handlers.get(tool_name) else {
                return Vec::new();
            };
            let Some(rh) = h.paths_for_workspace.as_ref() else {
                return Vec::new();
            };
            match self.lua.registry_value::<mlua::Function>(&rh.key) {
                Ok(f) => f,
                Err(_) => return Vec::new(),
            }
        };
        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool paths_for_workspace: build args: {e}"));
                return Vec::new();
            }
        };
        let _perf = smelt_perf::perf::begin("lua:tool");
        match func.call::<Option<mlua::Table>>(args_table) {
            Ok(Some(t)) => t
                .sequence_values::<String>()
                .filter_map(|r| r.ok())
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => {
                self.record_error(format!("tool paths_for_workspace `{tool_name}`: {e}"));
                Vec::new()
            }
        }
    }

    /// Run a tool's `decide(args, mode)` callback; returns `None` to fall through to generic permissions.
    pub fn tool_decide(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        mode: protocol::AgentMode,
    ) -> Option<protocol::Decision> {
        let func = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let h = handlers.get(tool_name)?;
            let rh = h.decide.as_ref()?;
            self.lua.registry_value::<mlua::Function>(&rh.key).ok()?
        };
        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool decide: build args: {e}"));
                return None;
            }
        };
        let mode_str = mode.as_str();
        let _perf = smelt_perf::perf::begin("lua:tool");
        match func.call::<String>((args_table, mode_str)) {
            Ok(label) => match label.as_str() {
                "allow" => Some(protocol::Decision::Allow),
                "ask" => Some(protocol::Decision::Ask),
                "deny" => Some(protocol::Decision::Deny),
                other => {
                    self.record_error(format!(
                        "tool decide `{tool_name}`: unknown decision `{other}`"
                    ));
                    None
                }
            },
            Err(e) => {
                self.record_error(format!("tool decide `{tool_name}`: {e}"));
                None
            }
        }
    }

    pub fn tool_has_preview(&self, tool_name: &str) -> bool {
        let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
        handlers.get(tool_name).is_some_and(|h| h.preview.is_some())
    }

    /// Run a tool's `preview(args)` callback and return the composed `BlockLayout` tree.
    /// `None` if the tool registered no preview, the call failed, or the return value
    /// wasn't a `smelt.layout` userdata.
    pub fn render_tool_preview(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Option<crate::content::block_layout::BlockLayout> {
        let preview_fn = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let h = handlers.get(tool_name)?;
            let rh = h.preview.as_ref()?;
            self.lua.registry_value::<mlua::Function>(&rh.key).ok()?
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool preview: build args: {e}"));
                return None;
            }
        };

        let _perf = smelt_perf::perf::begin("lua:tool");
        let result: mlua::Value = match preview_fn.call(args_table) {
            Ok(v) => v,
            Err(e) => {
                self.record_error(format!("tool preview `{tool_name}`: {e}"));
                return None;
            }
        };

        match result {
            mlua::Value::Nil => None,
            mlua::Value::UserData(ud) => {
                match ud.borrow::<crate::lua::api::layout::LuaBlockLayout>() {
                    Ok(layout) => Some(layout.0.clone()),
                    Err(e) => {
                        self.record_error(format!(
                            "tool preview `{tool_name}`: expected smelt.layout value: {e}"
                        ));
                        None
                    }
                }
            }
            _ => {
                self.record_error(format!(
                    "tool preview `{tool_name}`: expected smelt.layout value or nil"
                ));
                None
            }
        }
    }

    /// Invoke the tool's `summary(args)` Lua hook. The hook may return:
    ///   * `nil` / no value — empty summary (no header text)
    ///   * a `string` — wrapped as a single plain span (each `\n`-line one row)
    ///   * a table of `{ {span, span}, {span, span} }` — multi-line styled output;
    ///     span shape matches `buf:styled` (`{ text, syntax?, style? }`).
    pub fn tool_summary(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> protocol::StyledLines {
        let meta = match self
            .lua
            .named_registry_value::<mlua::Table>(&format!("__pt_meta_{tool_name}"))
        {
            Ok(meta) => meta,
            Err(_) => return protocol::StyledLines::empty(),
        };
        let func = match meta.get::<mlua::Function>("summary") {
            Ok(func) => func,
            Err(_) => return protocol::StyledLines::empty(),
        };
        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool summary: build args: {e}"));
                return protocol::StyledLines::empty();
            }
        };
        let _perf = smelt_perf::perf::begin("lua:tool");
        match func.call::<mlua::Value>(args_table) {
            Ok(v) => match decode_styled_lines(v) {
                Ok(lines) => lines,
                Err(e) => {
                    self.record_error(format!("tool summary `{tool_name}`: {e}"));
                    protocol::StyledLines::empty()
                }
            },
            Err(e) => {
                self.record_error(format!("tool summary `{tool_name}`: {e}"));
                protocol::StyledLines::empty()
            }
        }
    }

    pub fn evaluate_hooks(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> protocol::ToolHooks {
        let mut out = protocol::ToolHooks::default();

        let (approval_patterns_fn, preflight_fn) = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(h) = handlers.get(tool_name) else {
                return out;
            };
            let ap = h
                .approval_patterns
                .as_ref()
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok());
            let pf = h
                .preflight
                .as_ref()
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok());
            (ap, pf)
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool hooks: build args: {e}"));
                return out;
            }
        };

        if let Some(func) = approval_patterns_fn {
            let _perf = smelt_perf::perf::begin("lua:tool");
            match func.call::<Option<mlua::Table>>(args_table.clone()) {
                Ok(Some(t)) => {
                    out.approval_patterns = t
                        .sequence_values::<String>()
                        .filter_map(|r| r.ok())
                        .collect();
                }
                Ok(None) => {}
                Err(e) => self.record_error(format!("tool hook approval_patterns: {e}")),
            }
        }
        if let Some(func) = preflight_fn {
            let _perf = smelt_perf::perf::begin("lua:tool");
            match func.call::<Option<String>>(args_table) {
                Ok(Some(s)) => out.decision = protocol::Decision::Error(s),
                Ok(None) => {}
                Err(e) => self.record_error(format!("tool hook preflight: {e}")),
            }
        }

        out.summary = self.tool_summary(tool_name, args);
        out
    }

    /// Call a tool's `render(args, output, ctx)` hook and return the composed `BlockLayout` tree.
    pub fn render_tool_layout(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        output: Option<&crate::transcript_model::ToolOutput>,
        ctx: ToolRenderCtx<'_>,
    ) -> Option<crate::content::block_layout::BlockLayout> {
        let render_fn = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let h = handlers.get(tool_name)?;
            let rh = h.render.as_ref()?;
            self.lua.registry_value::<mlua::Function>(&rh.key).ok()?
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool render: build args: {e}"));
                return None;
            }
        };

        let output_table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool render: build output table: {e}"));
                return None;
            }
        };
        if let Some(out) = output {
            let _ = output_table.set("content", out.content.clone());
            let _ = output_table.set("is_error", out.is_error);
            if let Some(meta) = &out.metadata {
                match json_to_lua(&self.lua, meta) {
                    Ok(v) => {
                        let _ = output_table.set("metadata", v);
                    }
                    Err(e) => self.record_error(format!("tool render: metadata: {e}")),
                }
            }
        }

        let ctx_table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool render: build ctx: {e}"));
                return None;
            }
        };
        let _ = ctx_table.set("width", ctx.width);
        let _ = ctx_table.set("summary", ctx.summary);
        let _ = ctx_table.set("status", ctx.status);
        if let Some(secs) = ctx.elapsed_secs {
            let _ = ctx_table.set("elapsed_secs", secs);
        }
        if let Some(cid) = ctx.call_id {
            let _ = ctx_table.set("call_id", cid);
        }

        let result: mlua::Value = match render_fn.call((args_table, output_table, ctx_table)) {
            Ok(v) => v,
            Err(e) => {
                self.record_error(format!("tool render `{tool_name}`: {e}"));
                return None;
            }
        };

        match result {
            mlua::Value::Nil => None,
            mlua::Value::UserData(ud) => {
                match ud.borrow::<crate::lua::api::layout::LuaBlockLayout>() {
                    Ok(layout) => Some(layout.0.clone()),
                    Err(e) => {
                        self.record_error(format!(
                            "tool render `{tool_name}`: expected smelt.layout value: {e}"
                        ));
                        None
                    }
                }
            }
            _ => {
                self.record_error(format!(
                    "tool render `{tool_name}`: expected smelt.layout value or nil"
                ));
                None
            }
        }
    }

    fn args_to_lua_table(
        &self,
        args: &HashMap<String, serde_json::Value>,
    ) -> mlua::Result<mlua::Table> {
        let t = self.lua.create_table()?;
        for (k, v) in args {
            if let Ok(lua_val) = json_to_lua(&self.lua, v) {
                let _ = t.set(k.as_str(), lua_val);
            }
        }
        Ok(t)
    }

    /// Run all `tools.middleware{before=...}` hooks for `tool_name`, in
    /// registration order. Hooks registered with `name = ""` match every
    /// tool. Each handler receives `(args, ctx)`; returning a table
    /// replaces `args`; returning `{ deny = true, reason }` short-circuits
    /// with an error result. Returns `Some(deny_result)` on deny; `None`
    /// otherwise (and `args` is rewritten in-place when applicable).
    fn run_before_hooks(
        &self,
        tool_name: &str,
        args: &mut mlua::Table,
        ctx: &mlua::Table,
    ) -> Option<ToolExecResult> {
        let funcs = self
            .shared
            .hooks
            .tool_before
            .snapshot_for(&self.lua, tool_name);
        for func in funcs {
            let result: mlua::Result<mlua::Value> = func.call((args.clone(), ctx.clone()));
            match result {
                Ok(mlua::Value::Table(t)) => {
                    let deny: bool = t.get("deny").unwrap_or(false);
                    if deny {
                        let reason: String = t
                            .get("reason")
                            .unwrap_or_else(|_| format!("tool `{tool_name}` denied by middleware"));
                        return Some(ToolExecResult::Immediate {
                            content: reason,
                            is_error: true,
                        });
                    }
                    *args = t;
                }
                Ok(_) => {}
                Err(e) => {
                    self.record_error(format!("tools.middleware before `{tool_name}`: {e}"));
                }
            }
        }
        None
    }

    /// Run all `tools.middleware{after=...}` hooks for `tool_name` against
    /// a synchronous tool result. Each handler receives `(args, ctx, result)`
    /// where `result` is `{ content, is_error }`. A returned table replaces
    /// the result. Pending/yielding tools currently bypass this path —
    /// only Immediate results go through `run_after_hooks`.
    fn run_after_hooks(
        &self,
        tool_name: &str,
        args: &mlua::Table,
        ctx: &mlua::Table,
        content: &mut String,
        is_error: &mut bool,
    ) {
        let funcs = self
            .shared
            .hooks
            .tool_after
            .snapshot_for(&self.lua, tool_name);
        if funcs.is_empty() {
            return;
        }
        let Ok(result_tbl) = self.lua.create_table() else {
            return;
        };
        let _ = result_tbl.set("content", content.as_str());
        let _ = result_tbl.set("is_error", *is_error);
        let mut current = result_tbl;
        for func in funcs {
            let res: mlua::Result<mlua::Value> =
                func.call((args.clone(), ctx.clone(), current.clone()));
            match res {
                Ok(mlua::Value::Table(t)) => {
                    current = t;
                }
                Ok(_) => {}
                Err(e) => {
                    self.record_error(format!("tools.middleware after `{tool_name}`: {e}"));
                }
            }
        }
        if let Ok(c) = current.get::<String>("content") {
            *content = c;
        }
        if let Ok(e) = current.get::<bool>("is_error") {
            *is_error = e;
        }
    }

    pub fn execute_tool(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        request_id: u64,
        call_id: &str,
        env: ToolEnv<'_>,
        now: Instant,
    ) -> ToolExecResult {
        let ToolEnv {
            mode,
            session_id,
            session_dir,
        } = env;
        let func = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(handle) = handlers.get(tool_name) else {
                return ToolExecResult::Immediate {
                    content: format!("no tool registered: {tool_name}"),
                    is_error: true,
                };
            };
            match self
                .lua
                .registry_value::<mlua::Function>(&handle.execute.key)
            {
                Ok(f) => f,
                Err(_) => {
                    return ToolExecResult::Immediate {
                        content: format!("tool handler not found: {tool_name}"),
                        is_error: true,
                    };
                }
            }
        };

        let mut args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                return ToolExecResult::Immediate {
                    content: format!("tool arg table: {e}"),
                    is_error: true,
                };
            }
        };

        let ctx_table = match build_tool_ctx(&self.lua, call_id, mode, session_id, session_dir) {
            Ok(t) => t,
            Err(e) => {
                return ToolExecResult::Immediate {
                    content: format!("tool ctx table: {e}"),
                    is_error: true,
                };
            }
        };

        if let Some(result) = self.run_before_hooks(tool_name, &mut args_table, &ctx_table) {
            return result;
        }

        // Keep clones for `run_after_hooks` — `mlua::Table` is Rc-backed
        // internally, so this is cheap and the originals are consumed by
        // the task-spawn MultiValue below.
        let args_for_after = args_table.clone();
        let ctx_for_after = ctx_table.clone();

        let mut initial = mlua::MultiValue::new();
        initial.push_back(mlua::Value::Table(args_table));
        initial.push_back(mlua::Value::Table(ctx_table));

        let mut rt = match self.shared.tasks.lock() {
            Ok(g) => g,
            Err(_) => {
                return ToolExecResult::Immediate {
                    content: "task runtime poisoned".into(),
                    is_error: true,
                };
            }
        };
        if let Err(e) = rt.spawn(
            &self.lua,
            func,
            initial,
            TaskCompletion::ToolResult {
                request_id,
                call_id: call_id.to_string(),
            },
        ) {
            return ToolExecResult::Immediate {
                content: format!("tool spawn: {e}"),
                is_error: true,
            };
        }
        // Single-step the freshly-spawned task: if the handler yields, callers
        // get `Pending` and the task is parked for the next `drive_tasks` tick.
        // (The general drive loop is fixed-point, but at tool entry we want
        // "yielded at all → Pending" so async handlers don't get coalesced.)
        let task_opt = rt.take_next_ready(now);
        let mut outputs = Vec::new();
        if let Some(task) = task_opt {
            if let Some(parked) = crate::lua::step_task_owned(&self.lua, task, now, &mut outputs) {
                rt.put_back(parked);
            }
        }
        drop(rt);

        let mut immediate: Option<(String, bool)> = None;
        for out in outputs {
            match out {
                TaskDriveOutput::ToolComplete {
                    request_id: rid,
                    call_id: cid,
                    content,
                    is_error,
                } if rid == request_id && cid == call_id => {
                    immediate = Some((content, is_error));
                }
                TaskDriveOutput::ToolComplete { .. } => {}
                TaskDriveOutput::Error(msg) => self.record_error(msg),
            }
        }
        match immediate {
            Some((mut content, mut is_error)) => {
                self.run_after_hooks(
                    tool_name,
                    &args_for_after,
                    &ctx_for_after,
                    &mut content,
                    &mut is_error,
                );
                ToolExecResult::Immediate { content, is_error }
            }
            None => ToolExecResult::Pending,
        }
    }

    fn register_api(lua: &Lua, shared: &Arc<LuaShared>) -> LuaResult<()> {
        let smelt = lua.create_table()?;
        let smelt_keymap = lua.create_table()?;

        crate::lua::api::register_host_api(lua, &smelt, &smelt_keymap, shared)?;

        lua.globals().set("smelt", smelt)?;
        lua.globals().set("smelt_keymap", smelt_keymap)?;

        Ok(())
    }
}

/// Decode a Lua return value into `protocol::StyledLines`.
///
/// Accepted shapes:
///   * `nil` — empty
///   * `string` — wrapped as one or more plain-text lines (split on `\n`)
///   * `table` — must be a 2D sequence: outer list is lines, each line is a
///     list of span tables of shape `{ text, syntax?, style? = { hl?, dim?,
///     bold?, italic?, fg?, bg? } }`. Mirrors `buf:styled`.
fn decode_styled_lines(value: mlua::Value) -> Result<protocol::StyledLines, String> {
    use protocol::{StyledLines, StyledSpan};
    match value {
        mlua::Value::Nil => Ok(StyledLines::empty()),
        mlua::Value::String(s) => Ok(StyledLines::from_plain(s.to_string_lossy())),
        mlua::Value::Table(t) => {
            let mut lines: Vec<Vec<StyledSpan>> = Vec::new();
            for line_val in t.sequence_values::<mlua::Value>() {
                let line_val = line_val.map_err(|e| format!("decode line: {e}"))?;
                let line_tbl = match line_val {
                    mlua::Value::Table(l) => l,
                    mlua::Value::Nil => {
                        lines.push(Vec::new());
                        continue;
                    }
                    other => {
                        return Err(format!("expected line table, got {}", other.type_name()));
                    }
                };
                let mut spans: Vec<StyledSpan> = Vec::new();
                for span_val in line_tbl.sequence_values::<mlua::Table>() {
                    let span_tbl = span_val.map_err(|e| format!("decode span: {e}"))?;
                    let style_tbl = span_tbl
                        .get::<Option<mlua::Table>>("style")
                        .map_err(|e| format!("decode span style: {e}"))?;
                    let (hl, dim, bold, italic, fg, bg) = if let Some(s) = style_tbl {
                        (
                            s.get::<Option<String>>("hl").ok().flatten(),
                            s.get::<Option<bool>>("dim").ok().flatten().unwrap_or(false),
                            s.get::<Option<bool>>("bold")
                                .ok()
                                .flatten()
                                .unwrap_or(false),
                            s.get::<Option<bool>>("italic")
                                .ok()
                                .flatten()
                                .unwrap_or(false),
                            s.get::<Option<String>>("fg").ok().flatten(),
                            s.get::<Option<String>>("bg").ok().flatten(),
                        )
                    } else {
                        (None, false, false, false, None, None)
                    };
                    spans.push(StyledSpan {
                        text: span_tbl
                            .get::<Option<String>>("text")
                            .map_err(|e| format!("decode span text: {e}"))?
                            .unwrap_or_default(),
                        syntax: span_tbl
                            .get::<Option<String>>("syntax")
                            .map_err(|e| format!("decode span syntax: {e}"))?,
                        hl,
                        fg,
                        bg,
                        dim,
                        bold,
                        italic,
                    });
                }
                lines.push(spans);
            }
            Ok(StyledLines(lines))
        }
        other => Err(format!(
            "expected nil | string | table, got {}",
            other.type_name()
        )),
    }
}

pub fn load_bootstrap_chunks(lua: &Lua) -> mlua::Result<()> {
    for rel in BOOTSTRAP_FILES {
        let file = EMBEDDED_LUA.get_file(rel).ok_or_else(|| {
            LuaError::RuntimeError(format!("missing embedded bootstrap chunk: {rel}"))
        })?;
        let src = file
            .contents_utf8()
            .ok_or_else(|| LuaError::RuntimeError(format!("bootstrap chunk not utf-8: {rel}")))?;
        let name = format!("smelt/{rel}");
        lua.load(src).set_name(name).exec()?;
    }
    Ok(())
}

fn embedded_lua_modules() -> impl Iterator<Item = (String, &'static str)> {
    fn walk(dir: &'static Dir<'static>, out: &mut Vec<(String, &'static str)>) {
        for entry in dir.entries() {
            match entry {
                DirEntry::File(f) => {
                    let path = f.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("lua") {
                        continue;
                    }
                    let Some(rel) = path.to_str() else { continue };
                    let module = path_to_module(rel);
                    if let Some(src) = f.contents_utf8() {
                        out.push((module, src));
                    }
                }
                DirEntry::Dir(d) => walk(d, out),
            }
        }
    }
    let mut out = Vec::new();
    walk(&EMBEDDED_LUA, &mut out);
    out.into_iter()
}

fn path_to_module(rel: &str) -> String {
    let trimmed = rel.strip_suffix(".lua").unwrap_or(rel);
    let dotted = trimmed.replace('/', ".");
    format!("smelt.{dotted}")
}

/// Modules to `require` at startup; sorted per directory, bootstrap files excluded.
///
/// `disabled` lets callers (typically the TUI runtime after `early.lua`
/// has run) skip a user-opted-out module. Pass an empty set for
/// no-filter behavior.
pub fn autoload_modules_filtered(disabled: &std::collections::HashSet<String>) -> Vec<String> {
    let bootstrap_modules: std::collections::HashSet<String> =
        BOOTSTRAP_FILES.iter().map(|p| path_to_module(p)).collect();
    let mut out = Vec::new();
    for dir_name in AUTOLOAD_DIRS {
        let Some(dir) = EMBEDDED_LUA.get_dir(*dir_name) else {
            continue;
        };
        let mut names: Vec<String> = dir
            .files()
            .filter(|f| f.path().extension().and_then(|s| s.to_str()) == Some("lua"))
            .filter_map(|f| f.path().to_str().map(path_to_module))
            .filter(|m| !bootstrap_modules.contains(m))
            .filter(|m| !OPTIONAL_PLUGINS.contains(&m.as_str()))
            .filter(|m| !disabled.contains(m))
            .collect();
        names.sort();
        out.extend(names);
    }
    out
}

/// Backwards-compatible wrapper that applies no user filter. Prefer
/// `autoload_modules_filtered` when a `LuaShared::disabled_modules` set
/// is available.
pub fn autoload_modules() -> Vec<String> {
    autoload_modules_filtered(&std::collections::HashSet::new())
}

fn register_embedded_searcher(lua: &Lua) -> LuaResult<()> {
    register_module_searcher_with_roots(lua, module_overlay_roots())
}

fn register_module_searcher_with_roots(lua: &Lua, roots: Vec<PathBuf>) -> LuaResult<()> {
    let modules: HashMap<String, &'static str> = embedded_lua_modules().collect();
    let searcher = lua.create_function(move |lua, module: String| {
        let rel = module_to_relpath(&module);
        for root in &roots {
            let path = root.join(&rel);
            if let Ok(source) = std::fs::read_to_string(&path) {
                let name = path.display().to_string();
                let loader = lua.load(source).set_name(name).into_function()?;
                return Ok(mlua::Value::Function(loader));
            }
        }
        if let Some(source) = modules.get(&module) {
            let loader = lua.load(*source).set_name(module).into_function()?;
            return Ok(mlua::Value::Function(loader));
        }
        Ok(mlua::Value::String(lua.create_string(format!(
            "\n\tno embedded module '{module}'"
        ))?))
    })?;

    let package: mlua::Table = lua.globals().get("package")?;
    let searchers: mlua::Table = package.get("searchers")?;
    let len = searchers.raw_len();
    searchers.raw_set(len + 1, searcher)?;
    Ok(())
}

fn module_to_relpath(module: &str) -> PathBuf {
    let mut path = PathBuf::from(module.replace('.', "/"));
    path.set_extension("lua");
    path
}

/// Override search roots in priority order:
/// 1. `$SMELT_RUNTIME_DIR` (when set) — explicit dev override.
/// 2. Workspace `runtime/lua/` (debug builds only, when the path exists) —
///    so `cargo run` + `/reload` picks up unbuilt edits to bundled plugins.
/// 3. `.smelt/runtime/` in cwd — project overrides.
/// 4. `<XDG_DATA_HOME>/smelt/runtime/` — user overrides.
fn module_overlay_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = std::env::var_os("SMELT_RUNTIME_DIR") {
        roots.push(PathBuf::from(dir));
    }
    #[cfg(debug_assertions)]
    {
        let dev_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("runtime")
            .join("lua");
        if dev_root.is_dir() {
            roots.push(dev_root);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join(".smelt").join("runtime"));
    }
    roots.push(engine::data_dir().join("runtime"));
    roots
}

fn lua_files_in(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("lua"))
        .collect();
    out.sort();
    out
}

fn init_lua_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("smelt").join("init.lua"))
}

fn early_lua_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("smelt").join("early.lua"))
}

fn build_tool_ctx(
    lua: &Lua,
    call_id: &str,
    mode: protocol::AgentMode,
    session_id: &str,
    session_dir: &std::path::Path,
) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;
    t.set("call_id", call_id.to_string())?;
    t.set("mode", mode.as_str())?;
    t.set("session_id", session_id.to_string())?;
    t.set("session_dir", session_dir.to_string_lossy().into_owned())?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_relpath_translates_dots_to_slashes() {
        assert_eq!(
            module_to_relpath("smelt.dialogs.confirm"),
            PathBuf::from("smelt/dialogs/confirm.lua")
        );
        assert_eq!(module_to_relpath("smelt"), PathBuf::from("smelt.lua"));
    }

    #[test]
    fn path_to_module_translates_slashes_to_dots() {
        assert_eq!(
            path_to_module("dialogs/confirm.lua"),
            "smelt.dialogs.confirm"
        );
        assert_eq!(path_to_module("modes.lua"), "smelt.modes");
    }

    #[test]
    fn autoload_covers_tools_commands_plugins() {
        let modules = autoload_modules();
        assert!(modules.contains(&"smelt.tools.bash".to_string()));
        assert!(modules.contains(&"smelt.commands.btw".to_string()));
        assert!(modules.contains(&"smelt.plugins.esc_chord".to_string()));
    }

    #[test]
    fn autoload_excludes_optional_plugins() {
        let modules = autoload_modules();
        for optional in OPTIONAL_PLUGINS {
            assert!(
                !modules.contains(&optional.to_string()),
                "optional plugin must not be autoloaded: {optional}"
            );
        }
        for optional in OPTIONAL_PLUGINS {
            let rel = optional.strip_prefix("smelt.").unwrap().replace('.', "/") + ".lua";
            assert!(
                EMBEDDED_LUA.get_file(&rel).is_some(),
                "optional plugin must still be embedded so users can require it: {optional}"
            );
        }
    }

    #[test]
    fn embedded_lua_includes_bootstrap_files() {
        for rel in BOOTSTRAP_FILES {
            assert!(
                EMBEDDED_LUA.get_file(rel).is_some(),
                "bootstrap file missing from embedded tree: {rel}"
            );
        }
    }

    #[test]
    fn project_config_skipped_when_untrusted() {
        let tmp = tempfile::tempdir().unwrap();
        let smelt_dir = tmp.path().join(".smelt");
        std::fs::create_dir_all(&smelt_dir).unwrap();
        std::fs::write(smelt_dir.join("init.lua"), "PROJECT_LOADED = true\n").unwrap();

        let state = tempfile::tempdir().unwrap();
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let mut rt = LuaRuntime::new();
        let trust = rt.load_project_config(tmp.path());
        assert!(matches!(trust, crate::trust::TrustState::Untrusted { .. }));
        let loaded: bool = rt.lua.load("return PROJECT_LOADED == true").eval().unwrap();
        assert!(!loaded, "project init.lua must not run when untrusted");
    }

    #[test]
    fn project_config_runs_after_mark_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        let smelt_dir = tmp.path().join(".smelt");
        std::fs::create_dir_all(smelt_dir.join("plugins")).unwrap();
        std::fs::write(smelt_dir.join("init.lua"), "PROJECT_INIT = true\n").unwrap();
        std::fs::write(
            smelt_dir.join("plugins").join("a.lua"),
            "PROJECT_PLUGIN = true\n",
        )
        .unwrap();

        let state = tempfile::tempdir().unwrap();
        let _g = crate::test_util::isolate_xdg_state(state.path());
        crate::trust::mark_trusted(tmp.path()).unwrap();

        let mut rt = LuaRuntime::new();
        let trust = rt.load_project_config(tmp.path());
        assert!(matches!(trust, crate::trust::TrustState::Trusted { .. }));
        let init_ran: bool = rt.lua.load("return PROJECT_INIT == true").eval().unwrap();
        let plugin_ran: bool = rt.lua.load("return PROJECT_PLUGIN == true").eval().unwrap();
        assert!(init_ran, "project init.lua must run after trust");
        assert!(plugin_ran, "project plugins/*.lua must run after trust");
    }

    #[test]
    fn clear_lua_handles_drops_every_registry() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.cmd.register("plug_cmd", function() end)
                smelt.tools.middleware("bash", { before = function(ctx) return ctx end })
                "#,
            )
            .exec()
            .expect("register");
        assert!(rt.shared.commands.lock().unwrap().contains_key("plug_cmd"));
        assert!(!rt.shared.hooks.tool_before.is_empty());

        rt.shared.clear_lua_handles();
        assert!(rt.shared.commands.lock().unwrap().is_empty());
        assert!(rt.shared.hooks.tool_before.is_empty());
    }

    #[test]
    fn wipe_loaded_modules_keeps_native_stdlib() {
        let rt = LuaRuntime::new();
        rt.lua
            .load("package.loaded['user_mod'] = { v = 1 }")
            .exec()
            .unwrap();
        let stdlib_before: bool = rt
            .lua
            .load("return package.loaded['string'] ~= nil")
            .eval()
            .unwrap();
        assert!(stdlib_before, "stdlib must be in package.loaded");

        rt.wipe_loaded_modules();
        let stdlib_after: bool = rt
            .lua
            .load("return package.loaded['string'] ~= nil")
            .eval()
            .unwrap();
        assert!(stdlib_after, "stdlib must survive wipe");
        let user_after: bool = rt
            .lua
            .load("return package.loaded['user_mod'] ~= nil")
            .eval()
            .unwrap();
        assert!(!user_after, "user module must be wiped");
    }

    // End-to-end `reload()` lives in `tui::lua::tests::reload_clears_tui_surfaces`
    // — the bundled autoload modules need the TUI Lua API to run.

    #[test]
    fn overlay_file_overrides_embedded_module() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("smelt").join("dialogs");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("confirm.lua"), "return { tag = 'overlay' }\n").unwrap();

        let lua = Lua::new();
        let roots = vec![tmp.path().to_path_buf()];
        register_module_searcher_with_roots(&lua, roots).unwrap();

        let v: mlua::Table = lua
            .load("return require('smelt.dialogs.confirm')")
            .eval()
            .unwrap();
        let tag: String = v.get("tag").unwrap();
        assert_eq!(tag, "overlay");
    }
}
