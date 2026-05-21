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
pub const BOOTSTRAP_FILES: &[&str] = &[
    "_bootstrap.lua",
    "dialog.lua",
    "list.lua",
    "session.lua",
    "widgets/picker.lua",
    "widgets/completer.lua",
    "widgets/prompt_picker.lua",
    "cmd.lua",
    "dialogs/confirm.lua",
    "_bar.lua",
    "prompt_bar.lua",
    "statusline.lua",
    "layout.lua",
    "modes.lua",
];

/// Subdirectories whose files are `require`'d at startup as side-effect registrations.
const AUTOLOAD_DIRS: &[&str] = &["tools", "commands", "completers", "plugins", "dialogs"];

/// Subdirectory whose files run during the Early phase under the restricted
/// `smelt` view, BEFORE user `early.lua`. Plugins drop a file here to declare
/// CLI flags (`smelt.cli.register_flag{}`) or opt out of bundled modules
/// (`smelt.builtins.disable{}`).
const EARLY_DIRS: &[&str] = &["early"];

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

    /// Run every bundled `runtime/lua/smelt/early/*.lua` file during the
    /// Early phase under the restricted `smelt` view. Intended for plugins
    /// shipped with smelt that want to declare a CLI flag via
    /// `smelt.cli.register_flag{}` (so argv parsing picks it up) without
    /// forcing every user to edit their `early.lua`. Runs BEFORE
    /// [`Self::load_early_init`] so user code can override.
    pub fn load_bundled_early(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let modules = early_modules();
        if modules.is_empty() {
            return;
        }
        let result = self.with_early_smelt(|this| {
            for name in &modules {
                let code = format!("require('{name}')");
                this.lua
                    .load(&code)
                    .set_name(name.as_str())
                    .exec()
                    .map_err(|e| LuaError::RuntimeError(format!("bundled early {name}: {e}")))?;
            }
            Ok(())
        });
        if let Err(e) = result {
            self.load_error = Some(format!("bundled early init: {e}"));
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
        self.with_early_smelt(|this| this.lua.load(&src).set_name(name).exec())
    }

    /// Swap the global `smelt` for the restricted Early-phase view, set the
    /// phase, run `body`, then restore the full `smelt` regardless of
    /// outcome. The single place that owns the Early-phase smelt-view
    /// contract — every early-phase loader (`run_early_phase`,
    /// `load_bundled_early`) routes through here. Body errors win over
    /// restore errors so a real user mistake isn't masked by table-restore
    /// noise.
    fn with_early_smelt<F, R>(&mut self, body: F) -> LuaResult<R>
    where
        F: FnOnce(&mut Self) -> LuaResult<R>,
    {
        let full_smelt: mlua::Table = self.lua.globals().get("smelt")?;
        let restricted = self.build_early_smelt_view(&full_smelt)?;
        self.lua.globals().set("smelt", restricted)?;
        self.shared.set_phase(crate::lua::Phase::Early);
        let body_result = body(self);
        let restore_result = self.lua.globals().set("smelt", full_smelt);
        match (body_result, restore_result) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
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
        // Push an unnamed loader frame; `smelt.plugin("name")` inside
        // the body opts in to hot-reload survival. Falls back to plain
        // exec when bootstrap hasn't installed `__smelt_with_scope` yet.
        let loader = self.lua.load(&src).set_name("init.lua").into_function()?;
        match wrap_in_scope(&self.lua, loader.clone()) {
            Ok(wrapped) => wrapped.call::<()>(()),
            Err(_) => loader.call::<()>(()),
        }
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
        let name = path.display().to_string();
        let loader = self
            .lua
            .load(&src)
            .set_name(name.as_str())
            .into_function()?;
        // Push an unnamed loader frame; the plugin opts in via
        // `smelt.plugin("name")`. Falls back to plain exec when bootstrap
        // hasn't installed `__smelt_with_scope` yet.
        match wrap_in_scope(&self.lua, loader.clone()) {
            Ok(wrapped) => wrapped.call::<()>(()),
            Err(_) => loader.call::<()>(()),
        }
    }

    /// Snapshot the desired MCP server set as registered through
    /// `smelt.mcp.register`. `/reload` uses this to drive
    /// [`crate::mcp::McpManager::reconcile`].
    pub fn mcp_configs_snapshot(
        &self,
    ) -> std::collections::HashMap<String, crate::mcp::McpServerConfig> {
        self.shared
            .mcp_configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
        let mut settings = crate::config::ResolvedSettings::default();
        let overrides = self
            .shared
            .settings_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (key, value) in overrides.iter() {
            if let Err(e) = settings.set(key, value) {
                eprintln!("settings override: {e}");
            }
        }
        let defaults = self
            .shared
            .defaults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        crate::config::Config {
            providers,
            mcp,
            settings,
            defaults,
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

        let mut initial_args = mlua::MultiValue::new();
        if let Some(a) = arg {
            match self.lua.create_string(&a) {
                Ok(s) => initial_args.push_back(mlua::Value::String(s)),
                Err(e) => {
                    self.record_error(format!("cmd `{name}`: {e}"));
                    return true;
                }
            }
        }

        // Run the handler on the Lua task runtime so it executes inside a
        // coroutine. Yieldable APIs (`smelt.dialog.open`, `smelt.sleep`,
        // `smelt.task.wait`, ...) then work without each command wrapping its
        // body in `smelt.spawn`. Non-yielding handlers finish in the
        // synchronous drive below; yielding handlers park on the runtime and
        // resume on the next main-loop `drive_tasks` tick.
        let spawn_result = {
            let Ok(mut rt) = self.shared.tasks.lock() else {
                self.record_error(format!("cmd `{name}`: task runtime unavailable"));
                return true;
            };
            rt.spawn(
                &self.lua,
                func,
                initial_args,
                TaskCompletion::Command {
                    name: name.to_string(),
                },
            )
        };
        if let Err(e) = spawn_result {
            self.record_error(format!("cmd `{name}`: {e}"));
            return true;
        }
        let _ = self.drive_tasks(Instant::now());
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

    /// Surface an error to the user. Routes through `smelt.notify.error`,
    /// which appends the full body to the persistent message log AND
    /// pops a one-line toast. Falls back to a direct log write if the
    /// Lua surface isn't bound (e.g. an early-boot failure before
    /// `register_api` has run).
    pub fn record_error(&self, msg: String) {
        let routed = self
            .lua
            .globals()
            .get::<mlua::Table>("smelt")
            .and_then(|s| s.get::<mlua::Table>("notify"))
            .and_then(|n| n.get::<mlua::Function>("error"))
            .and_then(|f| f.call::<()>(msg.clone()))
            .is_ok();
        if routed {
            return;
        }
        if let Ok(mut messages) = self.shared.messages.lock() {
            messages.append(crate::messages::MessageKind::Error, "lua".to_string(), msg);
        }
    }

    pub fn has_command(&self, name: &str) -> bool {
        self.shared
            .commands
            .lock()
            .map(|m| m.contains_key(name))
            .unwrap_or(false)
    }

    /// Send+Sync handle to the registered `/command` name set. Worker threads
    /// (parallel block layout, etc.) can clone this to answer "is `foo` a
    /// known command?" without touching the main-thread `APP` pointer or the
    /// `!Send` `LuaHandle`s in `commands`.
    pub fn command_names_handle(&self) -> Arc<std::sync::Mutex<std::collections::HashSet<String>>> {
        Arc::clone(&self.shared.command_names)
    }

    /// Invoke every `smelt.lifecycle.on(event, fn)` callback in registration
    /// order, then drop them. The host calls this at the corresponding phase
    /// — `"ready"` after Lua bootstrap and argv parse, `"shutdown"` after the
    /// TUI tears down but before the process exits. Per-hook errors are
    /// returned so the caller can surface them as in-app notifications without
    /// aborting the remaining hooks.
    ///
    /// `build_ctx` constructs the per-event ctx table fresh inside the Lua
    /// runtime borrow. Pass `|_| Ok(mlua::Value::Nil)` for events that don't
    /// need a ctx.
    pub fn drain_lifecycle_hooks<F>(&mut self, event: &str, build_ctx: F) -> Vec<String>
    where
        F: Fn(&mlua::Lua) -> mlua::Result<mlua::Value>,
    {
        let hooks = self.shared.hooks.lifecycle.drain_for(&self.lua, event);
        let mut errors = Vec::with_capacity(hooks.len());
        for f in hooks {
            let ctx = match build_ctx(&self.lua) {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("lifecycle.{event}: ctx build: {e}"));
                    continue;
                }
            };
            if let Err(e) = f.call::<()>(ctx) {
                errors.push(format!("lifecycle.{event}: {e}"));
            }
        }
        errors
    }

    /// Convenience over [`drain_lifecycle_hooks`] for the `"shutdown"` event:
    /// builds the standard `{ session_id, has_messages }` ctx so the binary
    /// crate doesn't need a direct `mlua` dependency.
    pub fn drain_shutdown_hooks(&mut self, session_id: &str, has_messages: bool) -> Vec<String> {
        let sid = session_id.to_string();
        self.drain_lifecycle_hooks("shutdown", |lua| {
            let t = lua.create_table()?;
            t.set("session_id", sid.as_str())?;
            t.set("has_messages", has_messages)?;
            Ok(mlua::Value::Table(t))
        })
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
        let (src, name) = read_bootstrap_source(rel)?;
        lua.load(&src).set_name(name).exec()?;
    }
    Ok(())
}

/// Resolve a bootstrap-file relative path to its source, walking the
/// same disk-overlay roots as `require()` before falling back to the
/// baked-in [`EMBEDDED_LUA`] snapshot. Lets `dialog.lua`, `cmd.lua`,
/// etc. hot-reload from disk on `/reload` — same dev-loop parity as
/// autoloaded plugins. Returns `(source, chunk_name)`; the chunk name
/// reflects where the source actually came from so Lua tracebacks
/// point at the file you're editing.
fn read_bootstrap_source(rel: &str) -> mlua::Result<(String, String)> {
    for root in module_overlay_roots() {
        let candidate = root.join("smelt").join(rel);
        if let Ok(src) = std::fs::read_to_string(&candidate) {
            let name = candidate.display().to_string();
            return Ok((src, name));
        }
    }
    let file = EMBEDDED_LUA.get_file(rel).ok_or_else(|| {
        LuaError::RuntimeError(format!("missing embedded bootstrap chunk: {rel}"))
    })?;
    let src = file
        .contents_utf8()
        .ok_or_else(|| LuaError::RuntimeError(format!("bootstrap chunk not utf-8: {rel}")))?
        .to_string();
    let name = format!("smelt/{rel}");
    Ok((src, name))
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

/// Bundled `runtime/lua/smelt/early/*.lua` modules to `require` during the
/// Early phase. Sorted lex order for deterministic registration.
pub fn early_modules() -> Vec<String> {
    let mut out = Vec::new();
    for dir_name in EARLY_DIRS {
        let Some(dir) = EMBEDDED_LUA.get_dir(*dir_name) else {
            continue;
        };
        let mut names: Vec<String> = dir
            .files()
            .filter(|f| f.path().extension().and_then(|s| s.to_str()) == Some("lua"))
            .filter_map(|f| f.path().to_str().map(path_to_module))
            .collect();
        names.sort();
        out.extend(names);
    }
    out
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
                // Push an unnamed loader frame so the required module
                // can opt in to hot-reload survival via
                // `smelt.plugin(name)`. Falls back to the unwrapped
                // loader if bootstrap hasn't installed
                // `__smelt_with_scope` yet.
                let wrapped = wrap_in_scope(lua, loader.clone()).unwrap_or(loader);
                return Ok(mlua::Value::Function(wrapped));
            }
        }
        if let Some(source) = modules.get(&module) {
            let loader = lua
                .load(*source)
                .set_name(module.as_str())
                .into_function()?;
            let wrapped = wrap_in_scope(lua, loader.clone()).unwrap_or(loader);
            return Ok(mlua::Value::Function(wrapped));
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

/// Wrap a Lua loader function so its body runs inside a fresh frame
/// pushed onto `__smelt_scope_stack`. The frame starts unnamed; the
/// module body opts in to hot-reload survival via `smelt.plugin(name)`.
/// Implemented by forwarding through the bundled `__smelt_with_scope`
/// helper which handles push/pop + error propagation.
fn wrap_in_scope(lua: &Lua, loader: mlua::Function) -> LuaResult<mlua::Function> {
    let with_scope: mlua::Function = lua.globals().get("__smelt_with_scope")?;
    lua.create_function(move |_, args: mlua::MultiValue| {
        let mut call_args = mlua::MultiValue::new();
        call_args.push_back(mlua::Value::Function(loader.clone()));
        for a in args {
            call_args.push_back(a);
        }
        with_scope.call::<mlua::MultiValue>(call_args)
    })
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

pub fn init_lua_path() -> Option<PathBuf> {
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
                smelt.lifecycle.on_ready(function() end)
                "#,
            )
            .exec()
            .expect("register");
        assert!(rt.shared.commands.lock().unwrap().contains_key("plug_cmd"));
        assert!(!rt.shared.hooks.tool_before.is_empty());
        assert!(!rt.shared.hooks.lifecycle.is_empty());

        rt.shared.clear_lua_handles();
        assert!(rt.shared.commands.lock().unwrap().is_empty());
        assert!(rt.shared.hooks.tool_before.is_empty());
        assert!(rt.shared.hooks.lifecycle.is_empty());
    }

    #[test]
    fn lifecycle_on_ready_fires_in_registration_order_and_drains() {
        let mut rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                LIFECYCLE_LOG = {}
                smelt.lifecycle.on_ready(function() table.insert(LIFECYCLE_LOG, "a") end)
                smelt.lifecycle.on("ready", function() table.insert(LIFECYCLE_LOG, "b") end)
                "#,
            )
            .exec()
            .expect("register hooks");

        let errs = rt.drain_lifecycle_hooks("ready", |_| Ok(mlua::Value::Nil));
        assert!(errs.is_empty(), "no per-hook errors expected, got {errs:?}");
        let log: Vec<String> = rt
            .lua
            .load("return LIFECYCLE_LOG")
            .eval()
            .expect("read log");
        assert_eq!(log, vec!["a".to_string(), "b".to_string()]);

        // Second drain returns nothing — hooks are one-shot.
        let again = rt.drain_lifecycle_hooks("ready", |_| Ok(mlua::Value::Nil));
        assert!(again.is_empty());
        assert!(rt.shared.hooks.lifecycle.is_empty());
    }

    #[test]
    fn lifecycle_unknown_event_drains_to_empty() {
        let mut rt = LuaRuntime::new();
        let errs = rt.drain_lifecycle_hooks("never_emitted", |_| Ok(mlua::Value::Nil));
        assert!(errs.is_empty());
    }

    #[test]
    fn lifecycle_passes_ctx_table_to_hook() {
        let mut rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                LIFECYCLE_CTX = nil
                smelt.lifecycle.on("shutdown", function(ctx) LIFECYCLE_CTX = ctx end)
                "#,
            )
            .exec()
            .expect("register");

        let errs = rt.drain_lifecycle_hooks("shutdown", |lua| {
            let t = lua.create_table()?;
            t.set("session_id", "sess-42")?;
            t.set("has_messages", true)?;
            Ok(mlua::Value::Table(t))
        });
        assert!(errs.is_empty(), "no errors expected, got {errs:?}");
        let id: String = rt
            .lua
            .load("return LIFECYCLE_CTX.session_id")
            .eval()
            .expect("ctx.session_id readable");
        let has: bool = rt
            .lua
            .load("return LIFECYCLE_CTX.has_messages")
            .eval()
            .expect("ctx.has_messages readable");
        assert_eq!(id, "sess-42");
        assert!(has);
    }

    #[test]
    fn lifecycle_hook_error_isolates_per_callback() {
        let mut rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                LIFECYCLE_RAN = 0
                smelt.lifecycle.on_ready(function() error("boom") end)
                smelt.lifecycle.on_ready(function() LIFECYCLE_RAN = LIFECYCLE_RAN + 1 end)
                "#,
            )
            .exec()
            .expect("register");

        let errs = rt.drain_lifecycle_hooks("ready", |_| Ok(mlua::Value::Nil));
        assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
        assert!(
            errs[0].contains("boom"),
            "error message preserved: {}",
            errs[0]
        );
        let ran: i64 = rt.lua.load("return LIFECYCLE_RAN").eval().unwrap();
        assert_eq!(ran, 1, "second hook still ran after first one errored");
    }

    #[test]
    fn bundled_early_modules_run_in_lex_order_with_restricted_smelt() {
        let modules = early_modules();
        assert!(
            modules.contains(&"smelt.early.resume".to_string()),
            "bundled early should include smelt.early.resume, got {modules:?}"
        );
        let mut sorted = modules.clone();
        sorted.sort();
        assert_eq!(modules, sorted, "early modules must be lex-sorted");
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
