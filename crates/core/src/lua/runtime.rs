//! Headless-safe Lua runtime. The TUI extends this with UI-specific queues and statusline rendering.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use include_dir::{include_dir, Dir, DirEntry};
use mlua::prelude::*;

use crate::content::block_layout::{BlockLayout, LuaLeaf, SeparatorSpec, TextSpec};
use crate::content::display_safe_text;
use crate::lua::{
    json_to_lua, LuaShared, TaskCompletion, TaskDriveOutput, TaskEvent, ToolEnv, ToolExecResult,
    TranscriptGroupSpec,
};
use crate::permissions::{PathTargetKind, ToolPath};
use crate::transcript_model::{Block, BlockId, ToolOutput, ToolOutputRef, ToolState, ToolStatus};

const DEFAULT_TOOL_TIMEOUT_MS: u64 = 30_000;
const MAX_TOOL_TIMEOUT_MS: u64 = 600_000;

pub struct ShutdownHookContext<'a> {
    pub session_id: &'a str,
    pub has_messages: bool,
    pub ephemeral: bool,
}

/// Embedded `runtime/lua/smelt/` tree; every `.lua` file is `require`-able as `smelt.<path>`.
static EMBEDDED_LUA: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../runtime/lua/smelt");

/// Embedded `runtime/skills/` tree; extracted alongside Lua built-ins so
/// built-in skills have real on-disk locations for `/skills` and `load_skill`.
static EMBEDDED_SKILLS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../runtime/skills");

/// Lua chunks that only depend on Host-tier APIs and are available in every runtime.
pub const HOST_BOOTSTRAP_FILES: &[&str] = &[
    "_bootstrap.lua",
    "transcript/defaults.lua",
    "transcript.lua",
];

/// Lua chunks that depend on UiHost APIs and are loaded when the TUI runtime is active.
pub const UI_BOOTSTRAP_FILES: &[&str] = &[
    "dialog.lua",
    "list.lua",
    "session.lua",
    "widgets/picker.lua",
    "widgets/completer.lua",
    "widgets/prompt_picker.lua",
    "cmd.lua",
    "dialogs/confirm.lua",
    "_bar.lua",
    "tips.lua",
    "prompt_bar.lua",
    "statusline.lua",
    "layout.lua",
    "modes.lua",
];

/// Lua chunks executed at bootstrap time, in order: primitives before consumers.
pub const BOOTSTRAP_FILES: &[&str] = &[
    "_bootstrap.lua",
    "transcript/defaults.lua",
    "transcript.lua",
    "dialog.lua",
    "list.lua",
    "session.lua",
    "widgets/picker.lua",
    "widgets/completer.lua",
    "widgets/prompt_picker.lua",
    "cmd.lua",
    "label_value.lua",
    "dialogs/confirm.lua",
    "_bar.lua",
    "tips.lua",
    "prompt_bar.lua",
    "statusline.lua",
    "layout.lua",
    "modes.lua",
];

/// Subdirectories whose files are `require`'d at startup as side-effect registrations.
const AUTOLOAD_DIRS: &[&str] = &["tools", "commands", "completers", "plugins", "dialogs"];

/// Lua helper for filesystem-backed markdown slash commands. The regular
/// autoloader skips it; startup calls it explicitly after built-in commands
/// so `override: true` in command frontmatter can replace built-ins.
const CUSTOM_COMMANDS_MODULE: &str = "smelt.commands.custom_commands";

/// Subdirectory whose files run during the Early phase under the restricted
/// `smelt` view, BEFORE user `early.lua`. Plugins drop a file here to declare
/// CLI flags (`smelt.cli.register_flag{}`) or opt out of bundled modules
/// (`smelt.builtins.disable{}`).
const EARLY_DIRS: &[&str] = &["early"];

/// Bundled plugins that ship with smelt but are NOT autoloaded. Users opt in by
/// calling `require("smelt.plugins.<name>")` from their `init.lua`. Exposed
/// so the `gen-lua-docs` xtask can emit an opt-in vs autoload table in the
/// `customize` skill.
pub const OPTIONAL_PLUGINS: &[&str] = &[
    "smelt.plugins.which_key",
    "smelt.plugins.inspect",
    "smelt.plugins.lsp",
];

/// Command metadata used by command-line completion UIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCompletionItem {
    pub name: String,
    pub description: Option<String>,
}

/// Outcome of dispatching a keymap chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeymapResult {
    /// Handler ran and returned truthy or nothing.
    Consumed,
    /// Handler ran and returned `false`; key falls through.
    PassThrough,
    NoBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapPrefix {
    pub mode: String,
    pub chord: String,
    pub suffix: String,
    pub description: Option<String>,
}

fn keymap_mode_char(mode: &str) -> Option<&'static str> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "" | "*" | "any" | "all" => Some(""),
        "n" | "normal" => Some("n"),
        "i" | "insert" => Some("i"),
        "v" | "visual" | "visual_line" | "visualline" => Some("v"),
        _ => match mode {
            "Normal" => Some("n"),
            "Insert" => Some("i"),
            "Visual" | "VisualLine" => Some("v"),
            _ => None,
        },
    }
}

fn dispatch_mode_char(current_mode: Option<&str>) -> Option<&'static str> {
    current_mode.map(|mode| keymap_mode_char(mode).unwrap_or("n"))
}

fn query_mode_char(current_mode: Option<&str>) -> &'static str {
    current_mode.and_then(keymap_mode_char).unwrap_or("n")
}

fn keymap_mode_matches(binding_mode: &str, active_mode: &str) -> bool {
    binding_mode.is_empty() || binding_mode == active_mode
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootstrapMode {
    Host,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolVisibility {
    Interactive,
    Headless,
}

impl ToolVisibility {
    fn includes(self, headless: bool) -> bool {
        match self {
            Self::Interactive => true,
            Self::Headless => headless,
        }
    }
}

#[derive(Clone)]
struct LuaLaunchInputs {
    disabled_modules: std::collections::HashSet<String>,
    cli_flag_specs: Vec<crate::lua::CliFlagSpec>,
    cli_flag_values: HashMap<String, crate::lua::CliFlagValue>,
}

#[derive(Clone)]
struct LuaLoadPaths {
    config_dir: PathBuf,
    runtime_override: Option<PathBuf>,
    development_runtime: Option<PathBuf>,
    project_cwd: Option<PathBuf>,
    data_runtime: PathBuf,
}

impl LuaLoadPaths {
    fn from_process() -> Self {
        let development_runtime = if cfg!(debug_assertions) {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("runtime")
                .join("lua");
            path.is_dir().then_some(path)
        } else {
            None
        };
        Self {
            config_dir: crate::config::config_dir(),
            runtime_override: std::env::var_os("SMELT_RUNTIME_DIR").map(PathBuf::from),
            development_runtime,
            project_cwd: std::env::current_dir().ok(),
            data_runtime: engine::data_dir().join("runtime"),
        }
    }

    fn for_target_cwd(&self, cwd: Option<&std::path::Path>) -> Self {
        let mut paths = self.clone();
        paths.project_cwd = cwd.map(std::path::Path::to_path_buf);
        paths
    }

    fn module_overlay_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(path) = &self.runtime_override {
            roots.push(path.clone());
        }
        if let Some(path) = &self.development_runtime {
            roots.push(path.clone());
        }
        if let Some(cwd) = &self.project_cwd {
            roots.push(cwd.join(".smelt").join("runtime"));
        }
        roots.push(self.data_runtime.clone());
        roots
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LuaLoadFailureLocation {
    pub phase: &'static str,
    pub path: Option<PathBuf>,
}

/// Headless-safe Lua runtime.
pub struct LuaRuntime {
    pub lua: Lua,
    pub load_error: Option<String>,
    load_failure_location: Option<LuaLoadFailureLocation>,
    shared: Arc<LuaShared>,
    init_lua_path: Option<PathBuf>,
    bootstrap_mode: BootstrapMode,
    load_paths: LuaLoadPaths,
    launch_inputs: Option<LuaLaunchInputs>,
    load_warnings: Vec<String>,
    loaded_files: Arc<Mutex<Vec<PathBuf>>>,
    candidate_commits: Vec<mlua::RegistryKey>,
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
        #[allow(clippy::arc_with_non_send_sync)]
        let shared = Arc::new(LuaShared::default());
        Self::with_shared_and_paths(shared, LuaLoadPaths::from_process(), true)
    }

    pub fn with_shared(shared: Arc<LuaShared>) -> Self {
        Self::with_shared_and_paths(shared, LuaLoadPaths::from_process(), true)
    }

    fn with_shared_and_paths(
        shared: Arc<LuaShared>,
        load_paths: LuaLoadPaths,
        load_bootstrap: bool,
    ) -> Self {
        shared.set_project_cwd(load_paths.project_cwd.as_deref());
        let lua = Lua::new();
        let load_error = Self::register_api(&lua, &shared)
            .err()
            .map(|error| error.to_string());
        let load_failure_location = load_error.as_ref().map(|_| LuaLoadFailureLocation {
            phase: "api_registration",
            path: None,
        });
        let mut runtime = Self {
            lua,
            load_error,
            load_failure_location,
            shared,
            init_lua_path: None,
            bootstrap_mode: BootstrapMode::Host,
            load_paths,
            launch_inputs: None,
            load_warnings: Vec::new(),
            loaded_files: Arc::new(Mutex::new(Vec::new())),
            candidate_commits: Vec::new(),
        };

        if runtime.load_error.is_none() {
            let roots = runtime.load_paths.module_overlay_roots();
            if let Err(error) = register_module_searcher_with_roots(
                &runtime.lua,
                roots,
                Some(Arc::clone(&runtime.loaded_files)),
            ) {
                runtime.set_load_error(
                    "module_searcher",
                    None,
                    format!("embedded searcher: {error}"),
                );
            }
        }
        runtime.snapshot_native_modules();
        if load_bootstrap {
            runtime.load_bootstrap();
        }
        runtime
    }

    pub fn fresh_with_shared(
        &self,
        shared: Arc<LuaShared>,
        target_cwd: Option<&std::path::Path>,
    ) -> Self {
        Self::with_shared_and_paths(shared, self.load_paths.for_target_cwd(target_cwd), false)
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn load_failure_location(&self) -> Option<&LuaLoadFailureLocation> {
        self.load_failure_location.as_ref()
    }

    fn set_load_error(&mut self, phase: &'static str, path: Option<PathBuf>, message: String) {
        self.load_error = Some(message);
        self.load_failure_location = Some(LuaLoadFailureLocation { phase, path });
    }

    pub fn set_init_lua_path(&mut self, path: PathBuf) {
        self.init_lua_path = Some(path);
    }

    pub fn enable_ui_bootstrap(&mut self) {
        self.bootstrap_mode = BootstrapMode::Full;
    }

    pub fn load_full_bootstrap(&mut self) {
        self.bootstrap_mode = BootstrapMode::Full;
        self.load_bootstrap();
    }

    #[doc(hidden)]
    pub fn set_load_paths_for_harness(
        &mut self,
        config_dir: PathBuf,
        runtime_override: Option<PathBuf>,
        target_cwd: Option<PathBuf>,
    ) {
        self.load_paths.config_dir = config_dir;
        self.load_paths.runtime_override = runtime_override;
        self.load_paths.project_cwd = target_cwd;
        self.shared
            .set_project_cwd(self.load_paths.project_cwd.as_deref());
    }

    fn current_launch_inputs(&self) -> LuaLaunchInputs {
        LuaLaunchInputs {
            disabled_modules: self
                .shared
                .disabled_modules
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            cli_flag_specs: self
                .shared
                .cli_flag_specs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            cli_flag_values: self
                .shared
                .cli_flag_values
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        }
    }

    pub fn freeze_launch_inputs(&mut self) {
        self.launch_inputs = Some(self.current_launch_inputs());
    }

    /// Copy immutable launch inputs into a fresh candidate generation.
    pub fn inherit_launch_inputs(&mut self, committed: &Self) {
        self.init_lua_path = committed.init_lua_path.clone();
        self.bootstrap_mode = committed.bootstrap_mode;
        let launch = committed
            .launch_inputs
            .clone()
            .unwrap_or_else(|| committed.current_launch_inputs());
        *self
            .shared
            .disabled_modules
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = launch.disabled_modules.clone();
        *self
            .shared
            .cli_flag_specs
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = launch.cli_flag_specs.clone();
        *self
            .shared
            .cli_flag_values
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = launch.cli_flag_values.clone();
        self.launch_inputs = Some(launch);
    }

    pub fn take_load_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.load_warnings)
    }

    pub fn configured_init_lua_path(&self) -> Option<&std::path::Path> {
        self.init_lua_path.as_deref()
    }

    pub fn loaded_config_files(&self) -> Vec<PathBuf> {
        let mut files = self
            .loaded_files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        files.sort();
        files.dedup();
        files
    }

    fn record_loaded_file(&self, path: PathBuf) {
        self.loaded_files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(path);
    }

    pub fn load_manifest_roots(&self, target_cwd: Option<&std::path::Path>) -> Vec<PathBuf> {
        let mut roots = self.load_paths.module_overlay_roots();
        roots.push(self.load_paths.config_dir.clone());
        if let Some(cwd) = target_cwd {
            roots.push(cwd.join(".smelt"));
        }
        roots.sort();
        roots.dedup();
        roots
    }

    fn load_bootstrap(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let files = match self.bootstrap_mode {
            BootstrapMode::Host => HOST_BOOTSTRAP_FILES,
            BootstrapMode::Full => BOOTSTRAP_FILES,
        };
        let roots = self.load_paths.module_overlay_roots();
        if let Err(error) =
            load_bootstrap_group_with_roots(&self.lua, files, &roots, Some(&self.loaded_files))
        {
            self.set_load_error("bootstrap", None, format!("bootstrap: {error}"));
        }
    }

    pub fn load_user_config(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        let path = self
            .init_lua_path
            .clone()
            .unwrap_or_else(|| self.load_paths.config_dir.join("init.lua"));
        if path.exists() {
            if let Err(error) = self.load_init(&path) {
                let label = self
                    .init_lua_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "~/.config/smelt/init.lua".to_string());
                self.set_load_error("user", Some(path), format!("{label}: {error}"));
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
            self.set_load_error("early", None, format!("bundled early init: {e}"));
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
        let path = self.load_paths.config_dir.join("early.lua");
        if !path.exists() {
            return;
        }
        if let Err(e) = self.run_early_phase(&path, "early.lua") {
            self.set_load_error(
                "early",
                Some(path.clone()),
                format!("{}: {e}", path.display()),
            );
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
            self.set_load_error(
                "early",
                Some(path.clone()),
                format!("{}: {e}", path.display()),
            );
        }
    }

    fn run_early_phase(&mut self, path: &std::path::Path, name: &str) -> LuaResult<()> {
        self.record_loaded_file(path.to_path_buf());
        let src = std::fs::read_to_string(path)
            .map_err(|e| LuaError::RuntimeError(format!("read {name}: {e}")))?;
        self.with_early_smelt(|this| this.lua.load(&src).set_name(name).exec())
    }

    /// Swap the global `smelt` for the restricted Early-phase view, set the
    /// phase, run `body`, then restore the full `smelt` regardless of
    /// outcome. The single place that owns the Early-phase smelt-view
    /// contract - every early-phase loader (`run_early_phase`,
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
    /// errors with "attempt to call nil" - loud, immediate, traceable.
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
            if let Err(e) = self.require_module(&name) {
                self.set_load_error("autoload", None, format!("autoload {name}: {e}"));
                return;
            }
        }
        self.load_global_commands();
    }

    fn load_global_commands(&mut self) {
        self.load_command_dir("register_global", "global commands");
    }

    fn load_project_commands(&mut self) {
        self.load_command_dir("register_project", "project commands");
    }

    fn load_command_dir(&mut self, function_name: &str, label: &str) {
        if self.disabled_modules().contains(CUSTOM_COMMANDS_MODULE) {
            return;
        }
        let result: LuaResult<()> = (|| {
            let module: mlua::Table = self
                .lua
                .load(format!("return require('{CUSTOM_COMMANDS_MODULE}')"))
                .set_name(CUSTOM_COMMANDS_MODULE)
                .eval()?;
            let register: mlua::Function = module.get(function_name)?;
            register.call(())
        })();
        if let Err(e) = result {
            self.set_load_error("commands", None, format!("{label}: {e}"));
        }
    }

    fn require_module(&self, name: &str) -> LuaResult<()> {
        let code = format!("require('{name}')");
        self.lua.load(&code).set_name(name).exec()
    }

    /// Flush dirty `smelt.state.persistent` entries before reload clears the
    /// timers that would otherwise perform debounced saves.
    pub fn flush_persistent_state(&self) -> Option<String> {
        let result: LuaResult<()> = (|| {
            let smelt: mlua::Table = self.lua.globals().get("smelt")?;
            let flush: mlua::Function = smelt.get("__flush_persistent_state")?;
            flush.call(())
        })();
        result.err().map(|e| e.to_string())
    }

    /// Clear every Lua-owned registry, wipe non-stdlib `package.loaded`,
    /// re-run bootstrap (idempotent), then re-run autoload → user init →
    /// global plugins → project config. After loading, sweep stale
    /// `smelt.state` slots no plugin touched this cycle. `early.lua` is
    /// skipped (CLI flags and `builtins.disable` are startup-only).
    /// Returns any load error.
    pub fn reload(&mut self, cwd: Option<&std::path::Path>) -> Option<String> {
        self.reload_inner(cwd, None, false)
    }

    /// Load a fresh generation while carrying JSON-compatible `smelt.state`
    /// values from the committed runtime. Lua functions, userdata, and handles
    /// remain generation-owned and are intentionally not transferred.
    pub fn reload_with_state(
        &mut self,
        cwd: Option<&std::path::Path>,
        state: Option<&serde_json::Value>,
    ) -> Option<String> {
        self.reload_inner(cwd, state, true)
    }

    fn reload_inner(
        &mut self,
        cwd: Option<&std::path::Path>,
        state: Option<&serde_json::Value>,
        candidate: bool,
    ) -> Option<String> {
        self.load_error = None;
        self.load_failure_location = None;
        self.load_warnings.clear();
        self.loaded_files
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.candidate_commits.clear();
        let launch = candidate.then(|| {
            self.launch_inputs
                .clone()
                .unwrap_or_else(|| self.current_launch_inputs())
        });
        self.clear_for_reload();
        if candidate {
            self.shared
                .disabled_modules
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
            self.shared
                .cli_flag_specs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
        }
        if candidate {
            if let Err(error) = self.install_candidate_effect_guards() {
                self.set_load_error(
                    "candidate_guards",
                    None,
                    format!("candidate effect guards: {error}"),
                );
                return self.load_error.clone();
            }
        }
        self.load_bootstrap();
        if self.load_error.is_some() {
            return self.load_error.clone();
        }
        if candidate {
            if let Err(error) = self.install_candidate_effect_guards() {
                self.set_load_error(
                    "candidate_guards",
                    None,
                    format!("candidate effect guards: {error}"),
                );
                return self.load_error.clone();
            }
            self.load_bundled_early();
            self.load_early_init();
            if let Some(cwd) = cwd {
                self.load_project_early_init(cwd);
            }
            if self.load_error.is_some() {
                return self.load_error.clone();
            }

            let candidate_disabled = self
                .shared
                .disabled_modules
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let candidate_specs = self
                .shared
                .cli_flag_specs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let launch = launch.as_ref().expect("candidate launch inputs");
            if candidate_disabled != launch.disabled_modules
                || candidate_specs != launch.cli_flag_specs
            {
                self.load_warnings.push(
                    "early.lua launch declarations changed; restart smelt to apply CLI flag or builtin module changes"
                        .to_string(),
                );
            }
            *self
                .shared
                .disabled_modules
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = launch.disabled_modules.clone();
            let candidate_names: std::collections::HashSet<&str> = candidate_specs
                .iter()
                .map(|spec| spec.name.as_str())
                .collect();
            let mut values = launch.cli_flag_values.clone();
            values.retain(|name, _| candidate_names.contains(name.as_str()));
            *self
                .shared
                .cli_flag_values
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = values;
        }
        if let Some(state) = state {
            if let Err(error) = self.install_state_snapshot(state) {
                self.set_load_error(
                    "state_restore",
                    None,
                    format!("restore smelt.state: {error}"),
                );
                return self.load_error.clone();
            }
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

    pub fn state_snapshot(&self) -> serde_json::Value {
        let value = self
            .lua
            .load(
                r#"
                local seen = {}
                local function snapshot(value)
                    local kind = type(value)
                    if kind == "nil" or kind == "boolean" or kind == "number" or kind == "string" then
                        return value
                    end
                    if kind ~= "table" or seen[value] then
                        return nil
                    end
                    seen[value] = true
                    local copy = {}
                    for key, item in pairs(value) do
                        local key_kind = type(key)
                        if key_kind == "string" or key_kind == "number" then
                            local copied = snapshot(item)
                            if copied ~= nil then copy[key] = copied end
                        end
                    end
                    seen[value] = nil
                    return copy
                end
                return snapshot(rawget(_G, "__smelt_state__")) or {}
                "#,
            )
            .eval::<mlua::Value>();
        value
            .ok()
            .and_then(|value| crate::lua::lua_to_serde(&self.lua, &value))
            .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
    }

    fn install_state_snapshot(&self, state: &serde_json::Value) -> LuaResult<()> {
        let value = crate::lua::json_to_lua(&self.lua, state)?;
        self.lua.globals().set("__smelt_state__", value)
    }

    /// Guard APIs whose immediate effects cannot be part of candidate
    /// evaluation. Generation-owned registrations and reads remain available;
    /// guarded functions become live automatically when the generation commits.
    fn install_candidate_effect_guards(&mut self) -> LuaResult<()> {
        const BLOCKED_NAMESPACES: &[&str] = &["smelt.clipboard"];
        const BLOCKED_FUNCTIONS: &[&str] = &[
            "io.output",
            "io.popen",
            "io.tmpfile",
            "os.execute",
            "os.exit",
            "os.remove",
            "os.rename",
            "os.setlocale",
            "os.tmpname",
            "smelt.auth.managed_usage",
            "smelt.auth.request",
            "smelt.cmd.run",
            "smelt.engine.ask",
            "smelt.engine.ask_inherited",
            "smelt.engine.cancel",
            "smelt.engine.reload",
            "smelt.engine.reload_when_idle",
            "smelt.engine.submit_command",
            "smelt.engine.submit_command_continuation",
            "smelt.events.emit",
            "smelt.files.accept",
            "smelt.files.rescan",
            "smelt.grep.run",
            "smelt.http.cache.write",
            "smelt.http.get",
            "smelt.http.post",
            "smelt.inspect.__open_url",
            "smelt.inspect.__start",
            "smelt.inspect.__stop",
            "smelt.messages.append",
            "smelt.messages.clear",
            "smelt.messages.mark_read",
            "smelt.metrics.perf.clear",
            "smelt.metrics.perf.set_enabled",
            "smelt.fs.copy",
            "smelt.fs.file_state.record_read",
            "smelt.fs.file_state.record_read_with_mtime",
            "smelt.fs.file_state.record_write",
            "smelt.fs.mkdir",
            "smelt.fs.mkdir_all",
            "smelt.fs.mkdir_all_async",
            "smelt.fs.remove_dir",
            "smelt.fs.remove_dir_all",
            "smelt.fs.remove_file",
            "smelt.fs.rename",
            "smelt.fs.try_flock",
            "smelt.fs.write",
            "smelt.fs.write_async",
            "smelt.mode.cycle",
            "smelt.mode.set",
            "smelt.model.set",
            "smelt.notebook.apply_edit",
            "smelt.notebook.apply_edit_async",
            "smelt.os.open_url",
            "smelt.os.open_url_if_available",
            "smelt.os.set_cwd",
            "smelt.os.setenv",
            "smelt.os.unsetenv",
            "smelt.permissions.grant_session",
            "smelt.permissions.sync",
            "smelt.process.detach_foreground",
            "smelt.process.kill",
            "smelt.process.read_output",
            "smelt.process.run",
            "smelt.process.run_streaming",
            "smelt.process.spawn_bg",
            "smelt.process.stop",
            "smelt.prompt.cursor",
            "smelt.prompt.replace_range",
            "smelt.prompt.set_text",
            "smelt.quit",
            "smelt.reasoning.cycle",
            "smelt.reasoning.set",
            "smelt.session._rewind_active_turn_if_clean",
            "smelt.session.checkpoint",
            "smelt.session.context_note",
            "smelt.session.delete",
            "smelt.session.enter_worktree",
            "smelt.session.fork",
            "smelt.session.load",
            "smelt.session.reset",
            "smelt.session.rewind_to",
            "smelt.session.set_title_for_history",
            "smelt.session.switch_cwd",
            "smelt.session.title.set",
            "smelt.signal.set",
            "smelt.search.clear",
            "smelt.state.__save",
            "smelt.terminal.bell",
            "smelt.terminal.osc9_notify",
            "smelt.trust.mark",
            "smelt.vim.set_mode",
            "smelt.work.busy",
        ];

        let namespaces = self.lua.create_table()?;
        for (index, path) in BLOCKED_NAMESPACES.iter().enumerate() {
            namespaces.set(index + 1, *path)?;
        }
        let functions = self.lua.create_table()?;
        for (index, path) in BLOCKED_FUNCTIONS.iter().enumerate() {
            functions.set(index + 1, *path)?;
        }
        let install = self
            .lua
            .load(
                r#"
                return function(blocked_namespaces, blocked_functions)
                    local state = { committed = false }
                    local seen = {}

                    local function resolve(path)
                        local value = _G
                        local owner = nil
                        local key = nil
                        for part in string.gmatch(path, "[^.]+") do
                            owner = value
                            key = part
                            if type(owner) ~= "table" then return nil, nil end
                            value = rawget(owner, key)
                            if value == nil then return nil, nil end
                        end
                        return owner, key
                    end

                    local function guard(owner, key, path)
                        local original = rawget(owner, key)
                        if type(original) ~= "function" then return end
                        rawset(owner, key, function(...)
                            if not state.committed then
                                error(path .. " is unavailable while loading a Lua candidate", 2)
                            end
                            return original(...)
                        end)
                    end

                    local function guard_tree(value, path)
                        if type(value) ~= "table" or seen[value] then return end
                        seen[value] = true
                        for key, child in pairs(value) do
                            local child_path = path .. "." .. tostring(key)
                            if type(child) == "function" then
                                guard(value, key, child_path)
                            elseif type(child) == "table" then
                                guard_tree(child, child_path)
                            end
                        end
                    end

                    local candidate_cwd = smelt.os.cwd()
                    local path_is_absolute = smelt.path.is_absolute
                    local path_join = smelt.path.join
                    local function candidate_path(path)
                        if state.committed or type(path) ~= "string" then return path end
                        if path_is_absolute(path) then return path end
                        return path_join(candidate_cwd, path)
                    end

                    if io and type(io.open) == "function" then
                        local original_open = io.open
                        io.open = function(path, mode)
                            if not state.committed then
                                mode = mode or "r"
                                if string.find(mode, "[wa+]") then
                                    error("io.open is read-only while loading a Lua candidate", 2)
                                end
                                path = candidate_path(path)
                            end
                            return original_open(path, mode)
                        end
                    end
                    if io and type(io.lines) == "function" then
                        local original_lines = io.lines
                        io.lines = function(path, ...)
                            if path == nil then return original_lines() end
                            return original_lines(candidate_path(path), ...)
                        end
                    end
                    if io and type(io.input) == "function" then
                        local original_input = io.input
                        io.input = function(path)
                            if path == nil then return original_input() end
                            return original_input(candidate_path(path))
                        end
                    end
                    if type(loadfile) == "function" then
                        local original_loadfile = loadfile
                        loadfile = function(path, ...)
                            return original_loadfile(candidate_path(path), ...)
                        end
                    end
                    if type(dofile) == "function" then
                        local original_dofile = dofile
                        dofile = function(path)
                            return original_dofile(candidate_path(path))
                        end
                    end

                    for _, path in ipairs(blocked_namespaces) do
                        local owner, key = resolve(path)
                        if owner then guard_tree(rawget(owner, key), path) end
                    end
                    for _, path in ipairs(blocked_functions) do
                        local owner, key = resolve(path)
                        if owner then guard(owner, key, path) end
                    end
                    return function() state.committed = true end
                end
                "#,
            )
            .eval::<mlua::Function>()?;
        let commit: mlua::Function = install.call((namespaces, functions))?;
        self.candidate_commits
            .push(self.lua.create_registry_value(commit)?);
        Ok(())
    }

    pub fn commit_candidate(&self) -> LuaResult<()> {
        if self.candidate_commits.is_empty() {
            return Err(LuaError::RuntimeError(
                "Lua candidate effect guards are unavailable".to_string(),
            ));
        }
        self.shared
            .activate_generation_resources()
            .map_err(LuaError::external)?;
        for key in &self.candidate_commits {
            let commit = self.lua.registry_value::<mlua::Function>(key)?;
            commit.call::<()>(())?;
        }
        self.mark_running();
        Ok(())
    }

    /// Retire a committed runtime after its replacement has loaded and
    /// validated. External holders of the shared registries observe an empty
    /// retired generation instead of dangling Lua handles.
    pub fn retire(&mut self) {
        if let Ok(mut tasks) = self.shared.tasks.lock() {
            tasks.cancel_and_clear();
        }
        if let Ok(mut queue) = self.shared.task_inbox.lock() {
            queue.clear();
        }
        if let Ok(mut queue) = self.shared.json_inbox.lock() {
            queue.clear();
        }
        self.shared.clear_for_reload();
    }

    /// **Single ledger** of every Lua-side surface wiped at the top of a
    /// `/reload` cycle. Add new `LuaShared` registries here - `reload()`
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
        self.shared.clear_for_reload();
        self.wipe_loaded_modules();
    }

    pub fn load_init(&mut self, path: &std::path::Path) -> LuaResult<()> {
        self.record_loaded_file(path.to_path_buf());
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
        let dir = self.load_paths.config_dir.join("plugins");
        for path in lua_files_in(&dir) {
            if let Err(e) = self.load_plugin_file(&path) {
                self.set_load_error(
                    "plugin",
                    Some(path.clone()),
                    format!("{}: {e}", path.display()),
                );
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
        self.load_project_commands();
        if self.load_error.is_some() {
            return state;
        }
        for path in lua_files_in(&smelt_dir.join("plugins")) {
            if let Err(e) = self.load_plugin_file(&path) {
                self.set_load_error(
                    "plugin",
                    Some(path.clone()),
                    format!("{}: {e}", path.display()),
                );
                return state;
            }
        }
        let init = smelt_dir.join("init.lua");
        if init.exists() {
            if let Err(e) = self.load_init(&init) {
                self.set_load_error(
                    "project",
                    Some(init.clone()),
                    format!("{}: {e}", init.display()),
                );
            }
        }
        state
    }

    fn load_plugin_file(&mut self, path: &std::path::Path) -> LuaResult<()> {
        self.record_loaded_file(path.to_path_buf());
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
        let lsp = self.lsp_config_snapshot();
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
        let remember = self
            .shared
            .remember
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        crate::config::Config {
            providers,
            mcp,
            lsp,
            settings,
            defaults,
            remember,
        }
    }

    pub fn permission_rules_snapshot(&self) -> Option<crate::permissions::rules::RawPerms> {
        self.shared
            .permission_rules
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn lsp_config_snapshot(&self) -> crate::lsp::LspConfig {
        self.shared
            .lsp_config
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn default_shell_snapshot(&self) -> Option<crate::lua::DefaultShell> {
        self.shared
            .default_shell
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn tool_defaults(&self) -> crate::permissions::rules::ToolDefaults {
        self.shared
            .tool_defaults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn run_command(&self, name: &str, arg: Option<String>) -> bool {
        self.run_command_with_queue_target(name, arg, crate::lua::CommandQueueTarget::Turn)
    }

    pub fn run_command_with_queue_target(
        &self,
        name: &str,
        arg: Option<String>,
        queue_target: crate::lua::CommandQueueTarget,
    ) -> bool {
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
                    queue_target,
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
            let mode_char = dispatch_mode_char(current_mode);
            let entry = mode_char
                .and_then(|mc| map.get(&(mc.to_string(), chord.to_string())))
                .or_else(|| map.get(&(String::new(), chord.to_string())));
            let Some(entry) = entry else {
                return KeymapResult::NoBinding;
            };
            let Ok(f) = self.lua.registry_value::<mlua::Function>(&entry.handle.key) else {
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

    /// Returns true if a registered chord would be considered for this mode.
    /// Does not execute the handler, so pass-through callbacks are still treated
    /// as bindings by conservative callers.
    pub fn chord_has_binding(&self, chord: &str, current_mode: Option<&str>) -> bool {
        let Ok(map) = self.shared.keymaps.lock() else {
            return false;
        };
        let mode_char = dispatch_mode_char(current_mode);
        mode_char
            .map(|mc| map.contains_key(&(mc.to_string(), chord.to_string())))
            .unwrap_or(false)
            || map.contains_key(&(String::new(), chord.to_string()))
    }

    /// Returns true if `sequence` is a strict prefix of a registered chord (exact match excluded).
    pub fn chord_has_longer(&self, sequence: &str, current_mode: Option<&str>) -> bool {
        let Ok(map) = self.shared.keymaps.lock() else {
            return false;
        };
        let mode_char = query_mode_char(current_mode);
        for (m, chord) in map.keys() {
            if keymap_mode_matches(m, mode_char)
                && chord.len() > sequence.len()
                && crate::keymap::chord_sequence_starts_with(chord, sequence)
            {
                return true;
            }
        }
        false
    }

    /// Return effective registered chords that strictly extend `sequence` in `current_mode`.
    pub fn keymap_prefixes(&self, sequence: &str, current_mode: Option<&str>) -> Vec<KeymapPrefix> {
        let Ok(map) = self.shared.keymaps.lock() else {
            return Vec::new();
        };
        let mode_char = query_mode_char(current_mode);
        let mut rows: Vec<KeymapPrefix> = map
            .iter()
            .filter_map(|((mode, chord), entry)| {
                if !keymap_mode_matches(mode, mode_char)
                    || chord.len() <= sequence.len()
                    || !crate::keymap::chord_sequence_starts_with(chord, sequence)
                {
                    return None;
                }
                Some(KeymapPrefix {
                    mode: mode.clone(),
                    chord: chord.clone(),
                    suffix: chord[sequence.len()..].to_string(),
                    description: entry.description.clone(),
                })
            })
            .collect();

        rows.sort_by(|a, b| {
            a.chord
                .cmp(&b.chord)
                .then_with(|| a.mode.is_empty().cmp(&b.mode.is_empty()))
                .then_with(|| a.description.cmp(&b.description))
        });
        rows.dedup_by(|a, b| a.chord == b.chord);
        rows.sort_by(|a, b| {
            a.suffix
                .cmp(&b.suffix)
                .then_with(|| a.description.cmp(&b.description))
                .then_with(|| a.mode.cmp(&b.mode))
        });
        rows
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
    /// order, then drop them. The host calls this at the corresponding phase:
    /// `"ready"` after Lua bootstrap and argv parse, `"shutdown"` after the
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
    /// builds the standard shutdown context table so callers don't need a direct
    /// `mlua` dependency.
    pub fn drain_shutdown_hooks(&mut self, ctx: ShutdownHookContext<'_>) -> Vec<String> {
        let sid = ctx.session_id.to_string();
        self.drain_lifecycle_hooks("shutdown", |lua| {
            let t = lua.create_table()?;
            t.set("session_id", sid.as_str())?;
            t.set("has_messages", ctx.has_messages)?;
            t.set("ephemeral", ctx.ephemeral)?;
            Ok(mlua::Value::Table(t))
        })
    }

    pub fn command_busy_behavior(&self, name: &str) -> Option<crate::lua::CommandBusyBehavior> {
        self.shared.commands.lock().ok()?.get(name).map(|c| c.busy)
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

    pub fn command_completion_items(&self) -> Vec<CommandCompletionItem> {
        self.shared
            .commands
            .lock()
            .map(|m| {
                let mut v: Vec<CommandCompletionItem> = m
                    .iter()
                    .filter(|(_, cmd)| !cmd.hidden)
                    .map(|(name, cmd)| CommandCompletionItem {
                        name: name.clone(),
                        description: cmd.description.clone(),
                    })
                    .collect();
                v.sort_by(|a, b| a.name.cmp(&b.name));
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

    pub fn cancel_turn_tasks(&self) {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return;
        };
        rt.cancel_scope(&self.lua, super::task::TaskScope::Turn);
    }

    pub fn set_wakeup_sender(&self, tx: tokio::sync::mpsc::UnboundedSender<()>) {
        let _ = self.shared.wakeup_tx.set(tx);
    }

    pub fn next_task_wakeup(&self, now: Instant) -> Option<Instant> {
        let Ok(rt) = self.shared.tasks.lock() else {
            return None;
        };
        rt.next_wakeup(now)
    }

    fn take_next_ready_task(&self, now: Instant) -> Result<Option<super::task::LuaTask>, ()> {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return Err(());
        };
        Ok(rt.take_next_ready(now))
    }

    fn step_task_outside_runtime_lock(
        &self,
        task: super::task::LuaTask,
        now: Instant,
        outs: &mut Vec<TaskDriveOutput>,
    ) -> Result<(), ()> {
        // Keep the task mutex unlocked while resuming Lua; task code can re-enter
        // the runtime via `smelt.spawn`, cancellation handles, or task helpers.
        if let Some(parked) = crate::lua::step_task_owned(&self.lua, task, now, outs) {
            let Ok(mut rt) = self.shared.tasks.lock() else {
                return Err(());
            };
            rt.put_back(parked);
        }
        Ok(())
    }

    pub fn drive_tasks(&self, now: Instant) -> Vec<TaskDriveOutput> {
        let mut outs = Vec::new();
        loop {
            let task = match self.take_next_ready_task(now) {
                Ok(Some(task)) => task,
                Ok(None) => break,
                Err(()) => return Vec::new(),
            };
            if self
                .step_task_outside_runtime_lock(task, now, &mut outs)
                .is_err()
            {
                return Vec::new();
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

    pub fn has_tool(&self, tool_name: &str) -> bool {
        self.shared
            .tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(tool_name)
    }

    pub fn tool_available_for(&self, tool_name: &str, visibility: ToolVisibility) -> bool {
        self.has_tool(tool_name) && visibility.includes(self.tool_headless_enabled(tool_name))
    }

    fn tool_headless_enabled(&self, tool_name: &str) -> bool {
        self.lua
            .named_registry_value::<mlua::Table>(&format!("__pt_meta_{tool_name}"))
            .ok()
            .map(|meta| Self::meta_headless_enabled(&meta))
            .unwrap_or(true)
    }

    fn meta_headless_enabled(meta: &mlua::Table) -> bool {
        meta.get::<mlua::Value>("headless")
            .ok()
            .and_then(|v| v.as_boolean())
            .unwrap_or(true)
    }

    pub fn system_prompt_fragments(&self) -> Vec<String> {
        crate::lua::api::agent::system_prompt_fragments(&self.lua)
    }

    pub fn tool_defs(
        &self,
        _mode: protocol::AgentMode,
        visibility: ToolVisibility,
    ) -> Vec<protocol::ToolDef> {
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
                let headless = Self::meta_headless_enabled(&meta_table);
                if !visibility.includes(headless) {
                    continue;
                }
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
                    headless,
                });
            }
        }
        defs
    }

    fn lua_value_to_tool_path(value: mlua::Value) -> Option<ToolPath> {
        match value {
            mlua::Value::String(s) => Some(ToolPath::unknown(s.to_string_lossy())),
            mlua::Value::Table(t) => {
                let path = t
                    .get::<Option<String>>("path")
                    .ok()
                    .flatten()
                    .or_else(|| t.get::<Option<String>>(1).ok().flatten())?;
                let kind = t
                    .get::<Option<String>>("kind")
                    .ok()
                    .flatten()
                    .or_else(|| t.get::<Option<String>>("target_kind").ok().flatten())
                    .unwrap_or_else(|| "unknown".to_string())
                    .trim()
                    .to_ascii_lowercase();
                let target_kind = match kind.as_str() {
                    "file" => PathTargetKind::File,
                    "dir" | "directory" => PathTargetKind::Directory,
                    _ => PathTargetKind::Unknown,
                };
                Some(ToolPath { path, target_kind })
            }
            _ => None,
        }
    }

    /// Call a tool's `paths_for_workspace(args)` callback; returns touched paths for boundary checks.
    pub fn tool_paths_for_workspace(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Vec<ToolPath> {
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
                .sequence_values::<mlua::Value>()
                .filter_map(|r| r.ok().and_then(Self::lua_value_to_tool_path))
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => {
                self.record_error(format!("tool paths_for_workspace `{tool_name}`: {e}"));
                Vec::new()
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

    pub fn tool_preview_output(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Option<ToolOutputRef> {
        let preview_output_fn = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let h = handlers.get(tool_name)?;
            let rh = h.preview_output.as_ref()?;
            self.lua.registry_value::<mlua::Function>(&rh.key).ok()?
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool preview_output: build args: {e}"));
                return None;
            }
        };

        let _perf = smelt_perf::perf::begin("lua:tool");
        let result: mlua::Value = match preview_output_fn.call(args_table) {
            Ok(v) => v,
            Err(e) => {
                self.record_error(format!("tool preview_output `{tool_name}`: {e}"));
                return None;
            }
        };

        match result {
            mlua::Value::Nil => None,
            mlua::Value::Table(table) => {
                Some(Box::new(Self::tool_output_from_lua_table(&self.lua, table)))
            }
            other => {
                self.record_error(format!(
                    "tool preview_output `{tool_name}`: expected result table or nil, got {}",
                    other.type_name()
                ));
                None
            }
        }
    }

    fn tool_output_from_lua_table(lua: &Lua, result: mlua::Table) -> ToolOutput {
        let content: String = result.get("content").unwrap_or_default();
        let is_error: bool = result.get("is_error").unwrap_or(false);
        let metadata = result
            .get::<mlua::Value>("metadata")
            .ok()
            .and_then(|v| crate::lua::lua_to_serde::<serde_json::Value>(lua, &v));
        ToolOutput {
            content,
            is_error,
            metadata,
        }
    }

    /// Invoke the tool's `summary(args)` Lua hook. The hook may return:
    ///   * `nil` / no value - empty summary (no header text)
    ///   * a `string` - wrapped as a single plain span (each `\n`-line one row)
    ///   * a table of `{ {span, span}, {span, span} }` - multi-line styled output;
    ///     span shape matches `buf:styled` (`{ text, syntax?, style? }`).
    pub fn tool_summary(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> protocol::StyledLines {
        self.tool_summary_with_context(tool_name, args, true)
    }

    pub fn tool_summary_with_context(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        final_args: bool,
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
        let ctx_table = match self.lua.create_table() {
            Ok(t) => {
                if let Err(e) = t.set("final", final_args) {
                    self.record_error(format!("tool summary: build context: {e}"));
                    return protocol::StyledLines::empty();
                }
                t
            }
            Err(e) => {
                self.record_error(format!("tool summary: build context: {e}"));
                return protocol::StyledLines::empty();
            }
        };
        let _perf = smelt_perf::perf::begin("lua:tool");
        match func.call::<mlua::Value>((args_table, ctx_table)) {
            Ok(v) => match crate::lua::styled_lines_from_lua(v, "tool summary") {
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

    pub fn evaluate_tool_metadata(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> protocol::ToolMetadata {
        let mut out = protocol::ToolMetadata::default();

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
                self.record_error(format!("tool metadata: build args: {e}"));
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
                Ok(Some(s)) => out.preflight_error = Some(s),
                Ok(None) => {}
                Err(e) => self.record_error(format!("tool hook preflight: {e}")),
            }
        }

        out.summary = self.tool_summary(tool_name, args);
        out
    }

    pub fn transcript_renderer_generation(&self) -> u64 {
        self.shared
            .transcript_renderer_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn transcript_renderer_cache_key(&self) -> Option<u64> {
        let key = self
            .shared
            .transcript_renderer_cache_key
            .load(std::sync::atomic::Ordering::Acquire);
        (key != 0).then_some(key)
    }

    pub fn transcript_settings_cache_key(&self) -> Option<u64> {
        let transcript = self
            .lua
            .globals()
            .get::<Option<mlua::Table>>("smelt")
            .ok()
            .flatten()
            .and_then(|smelt| smelt.get::<Option<mlua::Table>>("settings").ok().flatten())
            .and_then(|settings| {
                settings
                    .get::<Option<mlua::Table>>("transcript")
                    .ok()
                    .flatten()
            })?;
        Some(crate::utils::hash_serializable(
            &crate::lua::api::lua_table_to_json(&self.lua, &transcript),
        ))
    }

    pub fn transcript_group_generation(&self) -> u64 {
        self.shared
            .transcript_groups_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn transcript_group_cache_key(&self) -> Option<u64> {
        let key = self
            .shared
            .transcript_groups_cache_key
            .load(std::sync::atomic::Ordering::Acquire);
        (key != 0).then_some(key)
    }

    pub fn transcript_group_specs(&self) -> Vec<TranscriptGroupSpec> {
        self.shared
            .transcript_groups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .specs()
    }

    pub fn render_transcript_group_layout(
        &self,
        name: &str,
        group: &serde_json::Value,
        view_state: crate::transcript_model::ViewState,
    ) -> BlockLayout {
        let render_fn = {
            let registry = self
                .shared
                .transcript_groups
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(entry) = registry.entries.get(name) else {
                return fallback_transcript_group_layout(name);
            };
            match self.lua.registry_value::<mlua::Function>(&entry.render.key) {
                Ok(f) => f,
                Err(e) => {
                    self.record_error(format!(
                        "transcript group render `{name}`: renderer handle: {e}"
                    ));
                    return fallback_transcript_group_layout(name);
                }
            }
        };

        let group_table = match transcript_group_to_lua_table(&self.lua, name, group) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!(
                    "transcript group render `{name}`: build group: {e}"
                ));
                return fallback_transcript_group_layout(name);
            }
        };
        let ctx_table = match transcript_render_ctx_to_lua_table(
            &self.lua,
            view_state,
            self.transcript_renderer_generation(),
        ) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("transcript group render `{name}`: build ctx: {e}"));
                return fallback_transcript_group_layout(name);
            }
        };

        let result: mlua::Value = match render_fn.call((group_table, ctx_table)) {
            Ok(v) => v,
            Err(e) => {
                self.record_error(format!("transcript group render `{name}`: {e}"));
                return fallback_transcript_group_layout(name);
            }
        };
        transcript_layout_from_lua_value(
            self,
            result,
            &format!("transcript group render `{name}`"),
            || fallback_transcript_group_layout(name),
        )
    }

    /// Call the root transcript renderer and return its layout tree. Renderer
    /// errors, nil returns, invalid return types, or missing renderers use a
    /// small Rust fallback layout so transcript projection stays usable.
    pub fn render_transcript_layout(
        &self,
        id: BlockId,
        index: usize,
        block: &Block,
        state: Option<&ToolState>,
        view_state: crate::transcript_model::ViewState,
    ) -> BlockLayout {
        let render_fn = {
            let slot = self
                .shared
                .transcript_renderer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(handle) = slot.as_ref() else {
                return fallback_transcript_layout(block, state, view_state);
            };
            match self.lua.registry_value::<mlua::Function>(&handle.key) {
                Ok(f) => f,
                Err(e) => {
                    self.record_error(format!("transcript render: renderer handle: {e}"));
                    return fallback_transcript_layout(block, state, view_state);
                }
            }
        };

        let block_table = match transcript_block_to_lua_table(&self.lua, id, index, block, state) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("transcript render: build block: {e}"));
                return fallback_transcript_layout(block, state, view_state);
            }
        };
        let ctx_table = match transcript_render_ctx_to_lua_table(
            &self.lua,
            view_state,
            self.transcript_renderer_generation(),
        ) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("transcript render: build ctx: {e}"));
                return fallback_transcript_layout(block, state, view_state);
            }
        };

        let result: mlua::Value = match render_fn.call((block_table, ctx_table)) {
            Ok(v) => v,
            Err(e) => {
                self.record_error(format!(
                    "transcript render `{}` #{index}: {e}",
                    transcript_block_kind(block)
                ));
                return fallback_transcript_layout(block, state, view_state);
            }
        };

        let label = format!(
            "transcript render `{}` #{index}",
            transcript_block_kind(block)
        );
        transcript_layout_from_lua_value(self, result, &label, || {
            fallback_transcript_layout(block, state, view_state)
        })
    }

    fn args_to_lua_table(
        &self,
        args: &HashMap<String, serde_json::Value>,
    ) -> mlua::Result<mlua::Table> {
        args_to_lua_table(&self.lua, args)
    }

    fn tool_timeout_ms(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Option<u64> {
        let meta = self
            .lua
            .named_registry_value::<mlua::Table>(&format!("__pt_meta_{tool_name}"))
            .ok();
        let default_ms = meta
            .as_ref()
            .and_then(|m| m.get::<Option<u64>>("watchdog_timeout_ms").ok().flatten())
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_MS);
        let max_ms = meta
            .as_ref()
            .and_then(|m| {
                m.get::<Option<u64>>("watchdog_max_timeout_ms")
                    .ok()
                    .flatten()
            })
            .unwrap_or(MAX_TOOL_TIMEOUT_MS)
            .max(1);
        let arg_name = meta
            .as_ref()
            .and_then(|m| {
                m.get::<Option<String>>("watchdog_timeout_arg")
                    .ok()
                    .flatten()
            })
            .unwrap_or_else(|| "timeout_ms".to_string());
        let arg_scale_ms = meta
            .as_ref()
            .and_then(|m| {
                m.get::<Option<u64>>("watchdog_timeout_arg_scale_ms")
                    .ok()
                    .flatten()
            })
            .unwrap_or(1)
            .max(1);
        let grace_ms = meta
            .as_ref()
            .and_then(|m| m.get::<Option<u64>>("watchdog_grace_ms").ok().flatten())
            .unwrap_or(0);

        let requested_ms = (!arg_name.is_empty())
            .then(|| args.get(&arg_name).and_then(serde_json::Value::as_u64))
            .flatten()
            .map(|n| n.saturating_mul(arg_scale_ms).saturating_add(grace_ms));
        let timeout_ms = requested_ms.unwrap_or(default_ms);
        (timeout_ms > 0).then(|| timeout_ms.min(max_ms))
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
                            metadata: None,
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
    /// the result. Pending/yielding tools currently bypass this path -
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
                    metadata: None,
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
                        metadata: None,
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
                    metadata: None,
                };
            }
        };

        let ctx_table = match build_tool_ctx(&self.lua, call_id, mode, session_id, session_dir) {
            Ok(t) => t,
            Err(e) => {
                return ToolExecResult::Immediate {
                    content: format!("tool ctx table: {e}"),
                    is_error: true,
                    metadata: None,
                };
            }
        };

        if let Some(result) = self.run_before_hooks(tool_name, &mut args_table, &ctx_table) {
            return result;
        }

        // Keep clones for `run_after_hooks` - `mlua::Table` is Rc-backed
        // internally, so this is cheap and the originals are consumed by
        // the task-spawn MultiValue below.
        let args_for_after = args_table.clone();
        let ctx_for_after = ctx_table.clone();

        let mut initial = mlua::MultiValue::new();
        initial.push_back(mlua::Value::Table(args_table));
        initial.push_back(mlua::Value::Table(ctx_table));

        let timeout_ms = self.tool_timeout_ms(tool_name, args);
        let deadline = timeout_ms.map(|ms| super::task::TaskDeadline {
            at: now + std::time::Duration::from_millis(ms),
            label_ms: ms,
            paused_at: None,
        });
        let task_opt = {
            let mut rt = match self.shared.tasks.lock() {
                Ok(g) => g,
                Err(_) => {
                    return ToolExecResult::Immediate {
                        content: "task runtime poisoned".into(),
                        is_error: true,
                        metadata: None,
                    };
                }
            };
            let task_id = match rt.spawn_scoped(
                &self.lua,
                func,
                initial,
                TaskCompletion::ToolResult {
                    request_id,
                    call_id: call_id.to_string(),
                },
                super::task::TaskScope::Turn,
                deadline,
            ) {
                Ok(id) => id,
                Err(e) => {
                    return ToolExecResult::Immediate {
                        content: format!("tool spawn: {e}"),
                        is_error: true,
                        metadata: None,
                    };
                }
            };
            rt.take_task(task_id)
        };
        // Single-step the freshly-spawned task: if the handler yields, callers
        // get `Pending` and the task is parked for the next `drive_tasks` tick.
        let mut outputs = Vec::new();
        if let Some(task) = task_opt {
            if self
                .step_task_outside_runtime_lock(task, now, &mut outputs)
                .is_err()
            {
                return ToolExecResult::Immediate {
                    content: "task runtime poisoned".into(),
                    is_error: true,
                    metadata: None,
                };
            }
        }

        let mut immediate: Option<(String, bool, Option<serde_json::Value>)> = None;
        for out in outputs {
            match out {
                TaskDriveOutput::ToolComplete {
                    request_id: rid,
                    call_id: cid,
                    content,
                    is_error,
                    metadata,
                } if rid == request_id && cid == call_id => {
                    immediate = Some((content, is_error, metadata));
                }
                TaskDriveOutput::ToolComplete { .. } => {}
                TaskDriveOutput::Error(msg) => self.record_error(msg),
            }
        }
        match immediate {
            Some((mut content, mut is_error, metadata)) => {
                self.run_after_hooks(
                    tool_name,
                    &args_for_after,
                    &ctx_for_after,
                    &mut content,
                    &mut is_error,
                );
                ToolExecResult::Immediate {
                    content,
                    is_error,
                    metadata,
                }
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

fn transcript_block_kind(block: &Block) -> &'static str {
    block.kind()
}

fn view_state_label(view_state: crate::transcript_model::ViewState) -> &'static str {
    match view_state {
        crate::transcript_model::ViewState::Expanded => "expanded",
        crate::transcript_model::ViewState::Peek => "peek",
        crate::transcript_model::ViewState::Collapsed => "collapsed",
        crate::transcript_model::ViewState::TrimmedHead { .. } => "trimmed_head",
        crate::transcript_model::ViewState::TrimmedTail { .. } => "trimmed_tail",
    }
}

fn tool_status_label(status: ToolStatus) -> &'static str {
    status.label()
}

fn transcript_render_ctx_to_lua_table(
    lua: &Lua,
    view_state: crate::transcript_model::ViewState,
    renderer_generation: u64,
) -> LuaResult<mlua::Table> {
    let ctx = lua.create_table()?;
    ctx.set("view_state", view_state_label(view_state))?;
    ctx.set("renderer_generation", renderer_generation)?;
    ctx.set("surface", "transcript")?;

    let configured_limits = transcript_limits_table(lua)?;
    let default_rows = crate::content::block_layout::DEFAULT_TOOL_BLOCK_ROWS;
    let tool_rows =
        transcript_limit(configured_limits.as_ref(), "tool_rows")?.unwrap_or(default_rows);

    let limits = lua.create_table()?;
    limits.set(
        "tool_header_rows",
        transcript_limit(configured_limits.as_ref(), "tool_header_rows")?.unwrap_or(tool_rows),
    )?;
    limits.set(
        "tool_body_rows",
        transcript_limit(configured_limits.as_ref(), "tool_body_rows")?.unwrap_or(tool_rows),
    )?;
    limits.set(
        "tool_output_rows",
        transcript_limit(configured_limits.as_ref(), "tool_output_rows")?.unwrap_or(tool_rows),
    )?;
    limits.set(
        "collapsed_error_rows",
        transcript_limit(configured_limits.as_ref(), "collapsed_error_rows")?.unwrap_or(4),
    )?;
    limits.set(
        "thinking_peek_rows",
        transcript_limit(configured_limits.as_ref(), "thinking_peek_rows")?.unwrap_or(4),
    )?;
    limits.set(
        "thinking_peek_head_rows",
        transcript_limit(configured_limits.as_ref(), "thinking_peek_head_rows")?.unwrap_or(1),
    )?;
    ctx.set("limits", limits)?;
    Ok(ctx)
}

fn transcript_limits_table(lua: &Lua) -> LuaResult<Option<mlua::Table>> {
    let globals = lua.globals();
    let Some(smelt) = globals.get::<Option<mlua::Table>>("smelt")? else {
        return Ok(None);
    };
    let Some(settings) = smelt.get::<Option<mlua::Table>>("settings")? else {
        return Ok(None);
    };
    let Some(transcript) = settings.get::<Option<mlua::Table>>("transcript")? else {
        return Ok(None);
    };
    transcript.get::<Option<mlua::Table>>("limits")
}

fn transcript_limit(limits: Option<&mlua::Table>, key: &str) -> LuaResult<Option<u16>> {
    let Some(limits) = limits else {
        return Ok(None);
    };
    let value = limits.get::<mlua::Value>(key)?;
    match value {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Integer(value) if value >= 1 => {
            Ok(Some(value.min(i64::from(u16::MAX)) as u16))
        }
        mlua::Value::Number(value) if value.is_finite() && value >= 1.0 => {
            Ok(Some(value.floor().min(f64::from(u16::MAX)) as u16))
        }
        _ => Err(mlua::Error::external(format!(
            "smelt.settings.transcript.limits.{key}: expected a positive number"
        ))),
    }
}

fn transcript_group_to_lua_table(
    lua: &Lua,
    name: &str,
    group: &serde_json::Value,
) -> LuaResult<mlua::Table> {
    let value = json_to_lua(lua, group)?;
    let mlua::Value::Table(table) = value else {
        return Err(mlua::Error::external("group must encode to a Lua table"));
    };
    table.set(
        "kind",
        table
            .get::<Option<String>>("kind")?
            .unwrap_or_else(|| "group".into()),
    )?;
    table.set(
        "name",
        table
            .get::<Option<String>>("name")?
            .unwrap_or_else(|| name.to_string()),
    )?;
    Ok(table)
}

fn transcript_layout_from_lua_value(
    runtime: &LuaRuntime,
    result: mlua::Value,
    label: &str,
    fallback: impl FnOnce() -> BlockLayout,
) -> BlockLayout {
    match result {
        mlua::Value::UserData(ud) => match ud.borrow::<crate::lua::api::layout::LuaBlockLayout>() {
            Ok(layout) => layout.0.clone(),
            Err(e) => {
                runtime.record_error(format!("{label}: expected smelt.layout value: {e}"));
                fallback()
            }
        },
        mlua::Value::Nil => {
            runtime.record_error(format!(
                "{label}: returned nil; use smelt.layout.empty() to hide a node"
            ));
            fallback()
        }
        other => {
            runtime.record_error(format!(
                "{label}: expected smelt.layout value, got {}",
                other.type_name()
            ));
            fallback()
        }
    }
}

fn transcript_block_to_lua_table(
    lua: &Lua,
    id: BlockId,
    index: usize,
    block: &Block,
    state: Option<&ToolState>,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("id", id.0)?;
    t.set("index", index)?;
    t.set("kind", transcript_block_kind(block))?;

    match block {
        Block::User { text, image_labels } => {
            t.set("text", text.as_str())?;
            t.set(
                "user_lines",
                crate::lua::serde_to_lua(lua, &user_styled_lines(text, image_labels))?,
            )?;
            let labels = lua.create_table_with_capacity(image_labels.len(), 0)?;
            for (i, label) in image_labels.iter().enumerate() {
                labels.set(i + 1, label.as_str())?;
            }
            t.set("image_labels", labels)?;
        }
        Block::Mode {
            text,
            icon,
            hl_group,
        } => {
            t.set("text", text.as_str())?;
            t.set("icon", icon.as_str())?;
            t.set("hl_group", hl_group.as_str())?;
        }
        Block::ProcessStatus { text, event } => {
            t.set("text", text.as_str())?;
            set_process_status_event_lua_fields(lua, &t, event.as_ref())?;
        }
        Block::Thinking {
            title,
            summary_titles,
            content,
            kind,
        } => {
            t.set("title", title.as_deref())?;
            t.set(
                "summary_titles",
                crate::lua::serde_to_lua(lua, summary_titles)?,
            )?;
            t.set("content", content.as_str())?;
            t.set(
                "reasoning_kind",
                match kind {
                    protocol::ReasoningKind::Summary => "summary",
                    protocol::ReasoningKind::Raw => "raw",
                },
            )?;
            t.set(
                "thinking_summary",
                thinking_fallback_summary(title.as_deref(), content),
            )?;
        }
        Block::Text { content } => {
            t.set("content", content.as_str())?;
        }
        Block::CodeLine { content, lang } => {
            t.set("content", content.as_str())?;
            t.set("lang", lang.as_str())?;
        }
        Block::ToolDraft {
            stream_id,
            call_id,
            name,
            summary,
            args,
            raw_arguments,
            finished,
        } => {
            t.set("stream_id", stream_id.as_str())?;
            if let Some(call_id) = call_id {
                t.set("call_id", call_id.as_str())?;
            }
            t.set("name", name.as_str())?;
            t.set("summary", crate::lua::serde_to_lua(lua, summary)?)?;
            t.set("summary_text", summary.as_plain_text())?;
            t.set("args", args_to_lua_table(lua, args)?)?;
            t.set("raw_arguments", raw_arguments.as_str())?;
            t.set("status", "drafting")?;
            t.set("status_hl", "SmeltToolPending")?;
            t.set("draft", true)?;
            t.set("draft_finished", *finished)?;
        }
        Block::ToolCall {
            call_id,
            name,
            summary,
            args,
        } => {
            t.set("call_id", call_id.as_str())?;
            t.set("name", name.as_str())?;
            t.set("summary", crate::lua::serde_to_lua(lua, summary)?)?;
            t.set("summary_text", summary.as_plain_text())?;
            t.set("args", args_to_lua_table(lua, args)?)?;
            let status = state.map(|s| s.status).unwrap_or(ToolStatus::Pending);
            t.set("status", tool_status_label(status))?;
            t.set("status_hl", tool_status_hl(status))?;
            let elapsed_table = lua.create_table()?;
            elapsed_table.set("call_id", call_id.as_str())?;
            elapsed_table.set("status", tool_status_label(status))?;
            if let Some(elapsed) = state.and_then(|s| s.elapsed) {
                elapsed_table.set("secs", elapsed.as_secs())?;
            }
            t.set("elapsed", elapsed_table)?;
            if let Some(elapsed) = state.and_then(|s| s.elapsed) {
                let secs = elapsed.as_secs();
                t.set("elapsed_secs", secs)?;
                if let Some(text) = tool_elapsed_text(status, secs) {
                    t.set("elapsed_text", text)?;
                }
            }
            if let Some(message) = state.and_then(|s| s.user_message.as_deref()) {
                t.set("user_message", message)?;
            }
            if let Some(output) = state.and_then(|s| s.preview_output.as_deref()) {
                t.set("preview_output", tool_output_to_lua_table(lua, output)?)?;
            }
            if let Some(output) = state.and_then(|s| s.output.as_deref()) {
                t.set("output", tool_output_to_lua_table(lua, output)?)?;
            }
        }
        Block::Exec { command, output } => {
            t.set("command", command.as_str())?;
            t.set(
                "command_spans",
                crate::lua::serde_to_lua(lua, &exec_command_spans(command))?,
            )?;
            t.set("output", output.as_str())?;
        }
        Block::Compacted { summary } | Block::CompactionPreview { summary } => {
            t.set("summary", summary.as_str())?;
        }
    }

    Ok(t)
}

fn set_process_status_event_lua_fields(
    lua: &Lua,
    table: &mlua::Table,
    event: Option<&protocol::ProcessStatusEvent>,
) -> LuaResult<()> {
    let Some(event) = event else {
        return Ok(());
    };
    for (field, value) in event.snapshot_json_fields() {
        table.set(field, crate::lua::json_to_lua(lua, &value)?)?;
    }
    Ok(())
}

fn args_to_lua_table(
    lua: &Lua,
    args: &HashMap<String, serde_json::Value>,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    for (key, value) in args {
        t.set(key.as_str(), json_to_lua(lua, value)?)?;
    }
    Ok(t)
}

fn tool_output_to_lua_table(
    lua: &Lua,
    output: &crate::transcript_model::ToolOutput,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("content", output.content.as_str())?;
    t.set("is_error", output.is_error)?;
    if let Some(metadata) = &output.metadata {
        t.set("metadata", json_to_lua(lua, metadata)?)?;
    }
    Ok(t)
}

fn user_styled_lines(text: &str, image_labels: &[String]) -> protocol::StyledLines {
    let lines = user_display_lines(text);
    let command_token_chars = lines
        .first()
        .and_then(|line| crate::commands::registered_command_token(line))
        .map(|token| token.chars().count())
        .unwrap_or(0);
    protocol::StyledLines(
        lines
            .iter()
            .enumerate()
            .map(|(line_idx, line)| {
                let command_prefix_chars = if line_idx == 0 {
                    command_token_chars
                } else {
                    0
                };
                user_line_spans(line, image_labels, command_prefix_chars)
            })
            .collect(),
    )
}

fn user_display_lines(text: &str) -> Vec<String> {
    let all_lines: Vec<String> = text
        .lines()
        .map(|line| display_safe_text(&line.replace('\t', "    ")))
        .collect();
    let start = all_lines
        .iter()
        .position(|line| !line.is_empty())
        .unwrap_or(0);
    let end = all_lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |idx| idx + 1);
    all_lines[start..end]
        .iter()
        .map(|line| line.trim_end().to_string())
        .collect()
}

fn user_line_spans(
    text: &str,
    image_labels: &[String],
    command_prefix_chars: usize,
) -> Vec<protocol::StyledSpan> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut i = 0usize;

    let flush_plain = |out: &mut Vec<protocol::StyledSpan>, plain: &mut String| {
        if !plain.is_empty() {
            out.push(protocol::StyledSpan {
                text: std::mem::take(plain),
                bold: true,
                ..Default::default()
            });
        }
    };
    let push_accent = |out: &mut Vec<protocol::StyledSpan>, text: String| {
        if !text.is_empty() {
            out.push(protocol::StyledSpan {
                text,
                fg: Some("SmeltAccent".into()),
                bold: true,
                ..Default::default()
            });
        }
    };

    let take = command_prefix_chars.min(len);
    if take > 0 {
        push_accent(&mut out, chars[..take].iter().collect());
        i = take;
    }

    while i < len {
        if chars[i] == '[' {
            let remaining: String = chars[i..].iter().collect();
            if let Some(label) = image_labels
                .iter()
                .find(|label| remaining.starts_with(label.as_str()))
            {
                flush_plain(&mut out, &mut plain);
                push_accent(&mut out, label.clone());
                i += label.chars().count();
                continue;
            }
        }

        if let Some((token, end)) = crate::content::selection::try_at_ref(&chars, i) {
            flush_plain(&mut out, &mut plain);
            push_accent(&mut out, token);
            i = end;
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    flush_plain(&mut out, &mut plain);
    out
}

fn exec_command_spans(command: &str) -> Vec<protocol::StyledSpan> {
    vec![
        protocol::StyledSpan {
            text: "!".into(),
            fg: Some("SmeltExecPrefix".into()),
            bold: true,
            ..Default::default()
        },
        protocol::StyledSpan {
            text: display_safe_text(command),
            bold: true,
            ..Default::default()
        },
    ]
}

fn layout_text(content: impl Into<String>, hl_group: Option<&str>, ansi: bool) -> BlockLayout {
    BlockLayout::Leaf(LuaLeaf::Text(TextSpec {
        content: content.into(),
        hl_group: hl_group.map(str::to_string),
        ansi,
    }))
}

fn layout_separator(label: impl Into<String>) -> BlockLayout {
    BlockLayout::Leaf(LuaLeaf::Separator(SeparatorSpec {
        label: vec![protocol::StyledSpan {
            text: label.into(),
            ..Default::default()
        }],
        dim: true,
        selectable: false,
    }))
}

fn fallback_transcript_group_layout(name: &str) -> BlockLayout {
    layout_text(
        format!("{name} group render error"),
        Some("ErrorMsg"),
        false,
    )
}

fn fallback_transcript_layout(
    block: &Block,
    state: Option<&ToolState>,
    view_state: crate::transcript_model::ViewState,
) -> BlockLayout {
    match block {
        Block::User { text, .. } => layout_text(text.clone(), None, false),
        Block::Mode {
            text,
            icon,
            hl_group,
        } => layout_text(format!("{icon}{text}"), Some(hl_group), false),
        Block::ProcessStatus { text, .. } => layout_text(text.clone(), Some("SmeltProcess"), false),
        Block::Thinking {
            title,
            summary_titles,
            content,
            ..
        } => {
            if matches!(view_state, crate::transcript_model::ViewState::Collapsed) {
                layout_text(
                    thinking_fallback_summary(title.as_deref(), content),
                    None,
                    false,
                )
            } else {
                let visible_titles =
                    if matches!(view_state, crate::transcript_model::ViewState::Expanded) {
                        summary_titles.as_slice()
                    } else {
                        &[]
                    };
                let content = crate::transcript_model::thinking_markdown_source(
                    title.as_deref(),
                    visible_titles,
                    content,
                );
                BlockLayout::Gutter {
                    child: Box::new(layout_text(content, None, false)),
                    spec: crate::content::block_layout::GutterSpec {
                        text: "│ ".to_string(),
                        styled: true,
                    },
                }
            }
        }
        Block::Text { content } | Block::CodeLine { content, .. } => {
            layout_text(content.clone(), None, false)
        }
        Block::ToolDraft { name, summary, .. } => {
            let mut header = format!("* {name}");
            let summary_text = summary.as_plain_text();
            if !summary_text.is_empty() {
                header.push(' ');
                header.push_str(&summary_text);
            }
            header.push_str(" (drafting)");
            layout_text(header, Some("SmeltToolPending"), false)
        }
        Block::ToolCall {
            name,
            summary,
            call_id: _,
            args: _,
        } => {
            let status = state.map(|s| s.status).unwrap_or(ToolStatus::Pending);
            let mut items = Vec::new();
            let mut header = format!("* {name}");
            let summary_text = summary.as_plain_text();
            if !summary_text.is_empty() {
                header.push(' ');
                header.push_str(&summary_text);
            }
            if let Some(elapsed) = state.and_then(|s| s.elapsed) {
                if let Some(text) = tool_elapsed_text(status, elapsed.as_secs()) {
                    header.push_str(&format!(" ({text})"));
                }
            }
            items.push(layout_text(header, Some(tool_status_hl(status)), false));
            if let Some(message) = state.and_then(|s| s.user_message.as_deref()) {
                items.push(BlockLayout::Gutter {
                    child: Box::new(layout_text(message.to_string(), None, false)),
                    spec: crate::content::block_layout::GutterSpec {
                        text: "  ".to_string(),
                        styled: false,
                    },
                });
            }
            if !matches!(status, ToolStatus::Denied) {
                if let Some(output) = state.and_then(|s| s.output.as_deref()) {
                    if !output.content.is_empty() {
                        items.push(BlockLayout::Gutter {
                            child: Box::new(layout_text(
                                output.content.clone(),
                                output.is_error.then_some("ErrorMsg"),
                                true,
                            )),
                            spec: crate::content::block_layout::GutterSpec {
                                text: "  ".to_string(),
                                styled: false,
                            },
                        });
                    }
                }
            }
            BlockLayout::Vbox(items)
        }
        Block::Exec { command, output } => {
            let mut items = vec![layout_text(format!("!{command}"), None, false)];
            if !output.is_empty() {
                items.push(layout_text(output.clone(), None, true));
            }
            BlockLayout::Vbox(items)
        }
        Block::Compacted { summary } => {
            let mut items = vec![layout_separator(" compacted ")];
            if !matches!(view_state, crate::transcript_model::ViewState::Collapsed) {
                items.push(layout_text(summary.clone(), None, false));
            }
            BlockLayout::Vbox(items)
        }
        Block::CompactionPreview { summary } => {
            let mut items = vec![layout_separator(" compacting ")];
            if !matches!(view_state, crate::transcript_model::ViewState::Collapsed) {
                items.push(layout_text(summary.clone(), None, false));
            }
            BlockLayout::Vbox(items)
        }
    }
}

fn tool_status_hl(status: ToolStatus) -> &'static str {
    status.hl_group()
}

fn tool_elapsed_text(status: ToolStatus, secs: u64) -> Option<String> {
    (!matches!(status, ToolStatus::Confirm) && secs >= 1).then(|| format_elapsed_secs(secs))
}

fn format_elapsed_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn thinking_fallback_summary(title: Option<&str>, content: &str) -> String {
    if let Some(title) = title.filter(|_| content.trim().is_empty()) {
        return title.to_string();
    }
    let (inferred_label, line_count) = thinking_summary(content);
    let label = title.unwrap_or(&inferred_label);
    let collapsed_lines = if title.is_some() || inferred_label == "thinking" {
        line_count
    } else {
        line_count.saturating_sub(1)
    };
    format!(
        "{label}\n… {} collapsed …",
        pluralize(collapsed_lines, "line", "lines")
    )
}

fn thinking_summary(content: &str) -> (String, usize) {
    let mut label = None;
    let mut lines = 0usize;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines += 1;
        if label.is_none()
            && trimmed.starts_with("**")
            && trimmed.ends_with("**")
            && trimmed.len() > 4
        {
            label = Some(trimmed[2..trimmed.len() - 2].trim().to_string());
        }
    }
    (label.unwrap_or_else(|| "thinking".to_string()), lines)
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

pub fn load_host_bootstrap_chunks(lua: &Lua) -> mlua::Result<()> {
    load_bootstrap_group_with_roots(lua, HOST_BOOTSTRAP_FILES, &module_overlay_roots(), None)
}

pub fn load_ui_bootstrap_chunks(lua: &Lua) -> mlua::Result<()> {
    load_bootstrap_group_with_roots(lua, UI_BOOTSTRAP_FILES, &module_overlay_roots(), None)
}

pub fn load_bootstrap_chunks(lua: &Lua) -> mlua::Result<()> {
    load_bootstrap_group_with_roots(lua, BOOTSTRAP_FILES, &module_overlay_roots(), None)
}

fn load_bootstrap_group_with_roots(
    lua: &Lua,
    files: &[&str],
    roots: &[PathBuf],
    loaded_files: Option<&Arc<Mutex<Vec<PathBuf>>>>,
) -> mlua::Result<()> {
    for rel in files {
        let (source, name, path) = read_bootstrap_source_from_roots(rel, roots)?;
        if let (Some(files), Some(path)) = (loaded_files, path) {
            files
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(path);
        }
        lua.load(&source).set_name(name).exec()?;
    }
    Ok(())
}

/// Extract the embedded `runtime/lua/smelt/` tree to
/// `<XDG_DATA_HOME>/smelt/builtins/lua/smelt/` so the agent (and humans)
/// can inspect the built-in source as worked examples. Versioned by
/// `CARGO_PKG_VERSION`: re-extracts on smelt upgrade, skips otherwise.
///
/// Best-effort. Returns the target directory on success, or the I/O
/// error on failure - callers should log and continue, since the
/// runtime stays fully functional from the embedded copy.
///
/// This is intentionally separate from the user-overlay path
/// (`<XDG_DATA_HOME>/smelt/runtime/`) so user overrides don't get
/// clobbered on upgrade.
pub fn ensure_builtins_extracted() -> std::io::Result<PathBuf> {
    let target = engine::data_dir().join("builtins");
    let version_file = target.join(".version");
    let expected = env!("CARGO_PKG_VERSION");
    if let Ok(found) = std::fs::read_to_string(&version_file) {
        let has_lua = target.join("lua").join("smelt").exists();
        let has_skills = target.join("skills").exists();
        if found.trim() == expected && has_lua && has_skills {
            return Ok(target);
        }
    }
    if target.exists() {
        std::fs::remove_dir_all(&target)?;
    }
    let lua_root = target.join("lua").join("smelt");
    std::fs::create_dir_all(&lua_root)?;
    write_dir_recursive(&EMBEDDED_LUA, &lua_root)?;

    let skills_root = target.join("skills");
    std::fs::create_dir_all(&skills_root)?;
    write_dir_recursive(&EMBEDDED_SKILLS, &skills_root)?;

    std::fs::write(&version_file, expected)?;
    Ok(target)
}

fn write_dir_recursive(dir: &Dir<'_>, target: &std::path::Path) -> std::io::Result<()> {
    for entry in dir.entries() {
        match entry {
            DirEntry::File(f) => {
                let rel = f.path();
                let dest = target.join(rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dest, f.contents())?;
            }
            DirEntry::Dir(d) => {
                let dest = target.join(d.path());
                std::fs::create_dir_all(&dest)?;
                write_dir_recursive(d, target)?;
            }
        }
    }
    Ok(())
}

/// Resolve a bootstrap-file relative path to its source, walking the
/// same disk-overlay roots as `require()` before falling back to the
/// baked-in [`EMBEDDED_LUA`] snapshot. Lets `dialog.lua`, `cmd.lua`,
/// etc. hot-reload from disk on `/reload` - same dev-loop parity as
/// autoloaded plugins. Returns `(source, chunk_name)`; the chunk name
/// reflects where the source actually came from so Lua tracebacks
/// point at the file you're editing.
#[cfg(test)]
fn read_bootstrap_source(rel: &str) -> mlua::Result<(String, String)> {
    let (source, name, _) = read_bootstrap_source_from_roots(rel, &module_overlay_roots())?;
    Ok((source, name))
}

fn read_bootstrap_source_from_roots(
    rel: &str,
    roots: &[PathBuf],
) -> mlua::Result<(String, String, Option<PathBuf>)> {
    for root in roots {
        let candidate = root.join("smelt").join(rel);
        if let Ok(source) = std::fs::read_to_string(&candidate) {
            let name = candidate.display().to_string();
            return Ok((source, name, Some(candidate)));
        }
    }
    let file = EMBEDDED_LUA.get_file(rel).ok_or_else(|| {
        LuaError::RuntimeError(format!("missing embedded bootstrap chunk: {rel}"))
    })?;
    let source = file
        .contents_utf8()
        .ok_or_else(|| LuaError::RuntimeError(format!("bootstrap chunk not utf-8: {rel}")))?
        .to_string();
    Ok((source, format!("smelt/{rel}"), None))
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
            .filter(|m| m != CUSTOM_COMMANDS_MODULE)
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

fn register_module_searcher_with_roots(
    lua: &Lua,
    roots: Vec<PathBuf>,
    loaded_files: Option<Arc<Mutex<Vec<PathBuf>>>>,
) -> LuaResult<()> {
    let modules: HashMap<String, &'static str> = embedded_lua_modules().collect();
    let searcher = lua.create_function(move |lua, module: String| {
        let rel = module_to_relpath(&module);
        for root in &roots {
            let path = root.join(&rel);
            if let Ok(source) = std::fs::read_to_string(&path) {
                if let Some(files) = &loaded_files {
                    files
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(path.clone());
                }
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
/// 1. `$SMELT_RUNTIME_DIR` (when set) - explicit dev override.
/// 2. Workspace `runtime/lua/` (debug builds only, when the path exists) -
///    so `cargo run` + `/reload` picks up unbuilt edits to bundled plugins.
/// 3. `.smelt/runtime/` in cwd - project overrides.
/// 4. `<XDG_DATA_HOME>/smelt/runtime/` - user overrides.
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
    fn title_only_thinking_summary_has_no_empty_line_count() {
        assert_eq!(
            thinking_fallback_summary(Some("Checking files"), ""),
            "Checking files"
        );
    }

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
        assert!(modules.contains(&"smelt.commands.mcp".to_string()));
        assert!(modules.contains(&"smelt.plugins.esc_chord".to_string()));
        assert!(modules.contains(&"smelt.plugins.plan_mode".to_string()));
    }

    #[test]
    fn autoload_defers_custom_markdown_commands() {
        let modules = autoload_modules();
        assert!(!modules.contains(&CUSTOM_COMMANDS_MODULE.to_string()));
    }

    #[test]
    fn command_register_requires_explicit_override_for_duplicates() {
        let rt = LuaRuntime::new();
        let (ok, err): (bool, String) = rt
            .lua
            .load(
                r#"
                smelt.cmd.register("same", function() end)
                local ok, err = pcall(smelt.cmd.register, "same", function() end)
                return ok, tostring(err)
                "#,
            )
            .eval()
            .unwrap();
        assert!(!ok);
        assert!(err.contains("override = true"));
    }

    #[test]
    fn command_register_override_replaces_without_old_reg_removing_new_entry() {
        let rt = LuaRuntime::new();
        let names: Vec<String> = rt
            .lua
            .load(
                r#"
                local old = smelt.cmd.register("same", function() end)
                smelt.cmd.register("same", function() end, { override = true })
                old:remove()
                local names = {}
                for _, row in ipairs(smelt.cmd.list()) do names[#names + 1] = row.name end
                return names
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(names, vec!["same"]);
    }

    #[test]
    fn tool_schema_preserves_parameter_property_order() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.tools.register({
                    name = "order_probe",
                    description = "test tool",
                    parameters = {
                        type = "object",
                        properties = {
                            file_path = { type = "string" },
                            content = { type = "string" },
                        },
                        required = { "file_path", "content" },
                    },
                    execute = function(args) return "ok" end,
                })
                "#,
            )
            .exec()
            .unwrap();

        let defs = rt.tool_defs(protocol::AgentMode::normal(), ToolVisibility::Interactive);
        let def = defs
            .iter()
            .find(|def| def.name == "order_probe")
            .expect("registered tool definition");
        let keys: Vec<_> = def.parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();

        assert_eq!(keys, vec!["file_path", "content"]);
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
    fn bootstrap_installs_persistent_state_flush_hook() {
        let rt = LuaRuntime::new();
        let (src, name) = read_bootstrap_source("_bootstrap.lua").unwrap();
        rt.lua.load(&src).set_name(name).exec().unwrap();

        let installed: bool = rt
            .lua
            .load("return type(smelt.__flush_persistent_state) == 'function'")
            .eval()
            .unwrap();
        assert!(installed);
        assert!(rt.flush_persistent_state().is_none());
    }

    fn runtime_without_builtin_groups() -> LuaRuntime {
        let rt = LuaRuntime::new();
        {
            let mut registry = rt
                .shared
                .transcript_groups
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            registry.entries.clear();
            registry.next_order = 0;
        }
        rt.shared
            .transcript_groups_cache_key
            .store(0, std::sync::atomic::Ordering::Release);
        rt.shared
            .transcript_groups_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        rt
    }

    #[test]
    fn process_status_block_table_exposes_typed_event_fields() {
        let lua = Lua::new();
        let block = Block::ProcessStatus {
            text: "background process 42 exited with code 7".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "42",
                Some(7),
            )),
        };

        let table = transcript_block_to_lua_table(&lua, BlockId::new(9), 3, &block, None).unwrap();

        assert_eq!(table.get::<String>("kind").unwrap(), "process_status");
        assert_eq!(
            table.get::<String>("event").unwrap(),
            "background_process_completed"
        );
        assert_eq!(
            table.get::<String>("event_type").unwrap(),
            "background_process_completed"
        );
        assert_eq!(table.get::<String>("process_id").unwrap(), "42");
        assert_eq!(table.get::<i32>("exit_code").unwrap(), 7);
        let event_data: mlua::Table = table.get("event_data").unwrap();
        assert_eq!(
            event_data.get::<String>("event").unwrap(),
            "background_process_completed"
        );
    }
    #[test]
    fn host_runtime_installs_transcript_renderer_api() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load error: {:?}", rt.load_error);
        let has_api: bool = rt
            .lua
            .load(
                r#"
                return type(smelt.transcript.set_renderer) == "function"
                  and type(smelt.transcript.extend_renderer) == "function"
                  and type(smelt.transcript.groups.register) == "function"
                  and type(smelt.transcript.defaults.render) == "function"
                  and type(smelt.transcript.defaults.render_group_child_list) == "function"
                  and type(smelt.transcript.defaults.render_group_children) == "function"
                  and type(smelt.transcript.defaults.group_failure_counts) == "function"
                  and smelt.transcript.get_renderer() ~= nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(has_api);
    }

    #[test]
    fn transcript_settings_cache_key_tracks_transcript_table() {
        let rt = LuaRuntime::new();
        assert_eq!(rt.transcript_settings_cache_key(), None);

        rt.lua
            .load(
                r#"
                smelt.settings = {
                  transcript = {
                    view = { tools = { bash = "collapsed" } },
                    limits = { collapsed_error_rows = 2 },
                  },
                }
                "#,
            )
            .exec()
            .unwrap();
        let before = rt.transcript_settings_cache_key().unwrap();

        rt.lua
            .load("smelt.settings.transcript.limits.collapsed_error_rows = 3")
            .exec()
            .unwrap();
        let after = rt.transcript_settings_cache_key().unwrap();

        assert_ne!(before, after);
    }

    #[test]
    fn transcript_limits_accept_numbers_and_reject_invalid_values() {
        let lua = Lua::new();
        let limits = lua.create_table().unwrap();
        limits.set("integer_rows", 2).unwrap();
        limits.set("float_rows", 2.9).unwrap();
        limits.set("zero_rows", 0).unwrap();
        limits.set("string_rows", "2").unwrap();

        assert_eq!(
            transcript_limit(Some(&limits), "missing_rows").unwrap(),
            None
        );
        assert_eq!(
            transcript_limit(Some(&limits), "integer_rows").unwrap(),
            Some(2)
        );
        assert_eq!(
            transcript_limit(Some(&limits), "float_rows").unwrap(),
            Some(2)
        );

        let zero_err = transcript_limit(Some(&limits), "zero_rows").unwrap_err();
        assert!(zero_err
            .to_string()
            .contains("smelt.settings.transcript.limits.zero_rows"));
        let string_err = transcript_limit(Some(&limits), "string_rows").unwrap_err();
        assert!(string_err
            .to_string()
            .contains("smelt.settings.transcript.limits.string_rows"));
    }

    #[test]
    fn transcript_group_registry_orders_and_replaces_specs() {
        let rt = runtime_without_builtin_groups();
        rt.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "tool_batch",
                  cache_key = "v1",
                  priority = 5,
                  min = 3,
                  default_view = "collapsed",
                  selector = { kind = "tool", name = "read_file", terminal = true },
                  bucket = { "name" },
                  render = function(group, ctx) return smelt.layout.empty() end,
                })
                smelt.transcript.groups.register({
                  name = "low",
                  cache_key = "v1",
                  selector = { kind = "tool" },
                  render = function(group, ctx) return smelt.layout.empty() end,
                })
                smelt.transcript.groups.register({
                  name = "tool_batch",
                  cache_key = "v2",
                  priority = 10,
                  selector = { kind = "tool", names = { "read_file", "grep", "glob" }, terminal = true },
                  render = function(group, ctx) return smelt.layout.empty() end,
                })
                "#,
            )
            .exec()
            .unwrap();

        let specs = rt.transcript_group_specs();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "tool_batch");
        assert_eq!(specs[0].cache_key.as_deref(), Some("v2"));
        assert_eq!(specs[0].priority, 10);
        assert_eq!(specs[0].min, 2);
        assert_eq!(specs[0].selector.kind.as_deref(), Some("tool"));
        assert_eq!(
            specs[0]
                .selector
                .names
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["read_file", "grep", "glob"]
        );
        assert_eq!(specs[0].selector.terminal, Some(true));
        assert_eq!(specs[1].name, "low");
        assert!(rt.transcript_group_generation() >= 3);
        assert!(rt.transcript_group_cache_key().is_some());
    }

    #[test]
    fn transcript_group_selector_rejects_conflicting_field_aliases() {
        let rt = LuaRuntime::new();
        let err = rt
            .lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "processes",
                  selector = {
                    kind = "process_status",
                    event = "background_process_completed",
                    fields = { event_type = "other_event" },
                  },
                  render = function(group, ctx) return smelt.layout.empty() end,
                })
                "#,
            )
            .exec()
            .expect_err("conflicting selector fields should fail");

        assert!(err.to_string().contains("conflicts"));
    }

    #[test]
    fn transcript_group_registration_remove_is_token_scoped() {
        let rt = runtime_without_builtin_groups();
        rt.lua
            .load(
                r#"
                local first = smelt.transcript.groups.register({
                  name = "batch",
                  cache_key = "v1",
                  selector = { kind = "tool" },
                  render = function(group, ctx) return smelt.layout.empty() end,
                })
                smelt.transcript.groups.register({
                  name = "batch",
                  cache_key = "v2",
                  selector = { kind = "tool" },
                  render = function(group, ctx) return smelt.layout.empty() end,
                })
                first:remove()
                "#,
            )
            .exec()
            .unwrap();
        let specs = rt.transcript_group_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].cache_key.as_deref(), Some("v2"));
    }

    #[test]
    fn transcript_group_without_cache_key_opts_out_of_cache() {
        let rt = runtime_without_builtin_groups();
        rt.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "uncached",
                  selector = { kind = "tool" },
                  render = function(group, ctx) return smelt.layout.empty() end,
                })
                "#,
            )
            .exec()
            .unwrap();
        assert_eq!(rt.transcript_group_cache_key(), None);
    }

    #[test]
    fn transcript_group_renderer_can_be_invoked_by_rust() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "batch",
                  cache_key = "v1",
                  selector = { kind = "tool" },
                  render = function(group, ctx)
                    return smelt.layout.text(group.name .. ":" .. tostring(group.count) .. ":" .. tostring(ctx.view_state))
                  end,
                })
                "#,
            )
            .exec()
            .unwrap();

        let layout = rt.render_transcript_group_layout(
            "batch",
            &serde_json::json!({ "count": 3 }),
            crate::transcript_model::ViewState::Expanded,
        );
        match layout {
            BlockLayout::Leaf(LuaLeaf::Text(spec)) => assert_eq!(spec.content, "batch:3:expanded"),
            other => panic!("unexpected layout: {other:?}"),
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
        let baseline_groups = rt.transcript_group_specs().len();
        rt.lua
            .load(
                r#"
                smelt.cmd.register("plug_cmd", function() end)
                smelt.tools.middleware("bash", { before = function(ctx) return ctx end })
                smelt.lifecycle.on_ready(function() end)
                smelt.transcript.groups.register({
                  name = "batch",
                  selector = { kind = "tool" },
                  render = function(group, ctx) return smelt.layout.empty() end,
                })
                "#,
            )
            .exec()
            .expect("register");
        assert!(rt.shared.commands.lock().unwrap().contains_key("plug_cmd"));
        assert!(!rt.shared.hooks.tool_before.is_empty());
        assert!(!rt.shared.hooks.lifecycle.is_empty());
        let specs = rt.transcript_group_specs();
        assert_eq!(specs.len(), baseline_groups + 1);
        assert!(specs.iter().any(|spec| spec.name == "batch"));

        rt.shared.clear_lua_handles();
        assert!(rt.shared.commands.lock().unwrap().is_empty());
        assert!(rt.shared.hooks.tool_before.is_empty());
        assert!(rt.shared.hooks.lifecycle.is_empty());
        assert!(rt.transcript_group_specs().is_empty());
    }

    #[test]
    fn clear_reload_scoped_config_drops_config_registries() {
        let rt = LuaRuntime::new();
        rt.shared.providers.lock().unwrap().push(Default::default());
        rt.shared.mcp_configs.lock().unwrap().insert(
            "srv".into(),
            crate::mcp::McpServerConfig {
                description: String::new(),
                enabled: true,
                transport: crate::mcp::McpTransportConfig::Local {
                    command: vec!["echo".into()],
                    env: Default::default(),
                    timeout: 1,
                },
            },
        );
        *rt.shared.permission_rules.lock().unwrap() =
            Some(crate::permissions::rules::RawPerms::default());
        rt.shared
            .settings_overrides
            .lock()
            .unwrap()
            .insert("vim".into(), crate::config::SettingValue::Bool(true));
        rt.shared.defaults.lock().unwrap().model = Some("model-a".into());
        rt.shared.remember.lock().unwrap().model = false;
        rt.shared.tool_defaults.lock().unwrap().tool_effects.insert(
            "bash".into(),
            crate::permissions::rules::ToolEffectKind::Process,
        );
        *rt.shared.default_shell.lock().unwrap() = Some(crate::lua::DefaultShell {
            program: "zsh".into(),
            args: vec!["-fc".into()],
        });

        rt.shared.clear_reload_scoped_config();

        assert!(rt.shared.providers.lock().unwrap().is_empty());
        assert!(rt.shared.mcp_configs.lock().unwrap().is_empty());
        assert!(rt.shared.permission_rules.lock().unwrap().is_none());
        assert!(rt.shared.settings_overrides.lock().unwrap().is_empty());
        assert!(rt.shared.defaults.lock().unwrap().model.is_none());
        assert!(rt.shared.remember.lock().unwrap().model);
        assert!(rt
            .shared
            .tool_defaults
            .lock()
            .unwrap()
            .tool_effects
            .is_empty());
        assert!(rt.shared.default_shell.lock().unwrap().is_none());
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

        // Second drain returns nothing - hooks are one-shot.
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
    // - the bundled autoload modules need the TUI Lua API to run.

    #[test]
    fn overlay_file_overrides_embedded_module() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("smelt").join("dialogs");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("confirm.lua"), "return { tag = 'overlay' }\n").unwrap();

        let lua = Lua::new();
        let roots = vec![tmp.path().to_path_buf()];
        register_module_searcher_with_roots(&lua, roots, None).unwrap();

        let v: mlua::Table = lua
            .load("return require('smelt.dialogs.confirm')")
            .eval()
            .unwrap();
        let tag: String = v.get("tag").unwrap();
        assert_eq!(tag, "overlay");
    }
}
