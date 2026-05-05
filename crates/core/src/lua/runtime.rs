//! Headless-safe Lua runtime skeleton.
//!
//! Owns the `mlua::Lua` state, the `Arc<LuaShared>` registries, and
//! all methods that do not touch TUI-specific types (`Ui`, `WinId`,
//! `Payload`, etc.).
//!
//! The TUI wraps this in [`tui::lua::LuaRuntime`](crate::tui::lua::LuaRuntime)
//! and adds UI-specific callback queues + statusline rendering.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use include_dir::{include_dir, Dir, DirEntry};
use mlua::prelude::*;

use crate::lua::{
    json_to_lua, LuaShared, TaskCompletion, TaskDriveOutput, TaskEvent, ToolEnv, ToolExecResult,
};

/// Embedded `runtime/lua/smelt/` tree. Every `.lua` file under here is
/// `require`-able as `smelt.<dotted-path>`; the paths in
/// [`BOOTSTRAP_FILES`] additionally run at `register_api` time, and
/// every file under `tools/`, `commands/`, `plugins/` is required at
/// startup.
static EMBEDDED_LUA: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../runtime/lua/smelt");

/// Lua chunks executed at `register_api` time, in this order, after
/// the `smelt` global is fully populated but before any plugin or user
/// init.lua runs. Order is semantic: framework primitives (`_bootstrap`,
/// `dialog`, `cmd`) ship before consumers (`widgets`, `dialogs/confirm`,
/// `status`, `modes`).
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

/// Top-level subdirectories whose `.lua` files are `require`'d at
/// startup. Files within these directories must be self-contained;
/// they register tools, commands, and cell subscribers as a
/// side-effect of being loaded. `dialogs/` is autoloaded because most
/// dialog modules register a slash command at top level; the one
/// exception (`dialogs/confirm.lua`) is loaded earlier via
/// [`BOOTSTRAP_FILES`] and the second `require` is a no-op.
const AUTOLOAD_DIRS: &[&str] = &["tools", "commands", "plugins", "dialogs"];

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

        rt
    }

    /// Build a runtime around an existing `Arc<LuaShared>`.
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

        rt
    }

    /// Read the load error, if any.
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Set a custom path for `init.lua`.
    pub fn set_init_lua_path(&mut self, path: PathBuf) {
        self.init_lua_path = Some(path);
    }

    /// Load `~/.config/smelt/init.lua` (or the override set by
    /// [`set_init_lua_path`]).
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

    pub fn load_init(&mut self, path: &std::path::Path) -> LuaResult<()> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| LuaError::RuntimeError(format!("read init.lua: {e}")))?;
        self.lua.load(&src).set_name("init.lua").exec()
    }

    /// Load every `.lua` file under `<config>/plugins/` (sorted) at
    /// startup. Errors are recorded as load errors but do not stop
    /// the runtime.
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

    /// Load `<cwd>/.smelt/init.lua` and `<cwd>/.smelt/plugins/*.lua`,
    /// gated by [`crate::trust`]. Returns the trust state so the
    /// caller can surface a notification when the project is
    /// untrusted.
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

    /// Build a `Config` from the LuaShared registries populated by
    /// `init.lua`.
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
            let _ = settings.apply(key, value);
        }
        crate::config::Config {
            providers,
            mcp,
            settings,
            ..Default::default()
        }
    }

    /// Take any permission rules registered by Lua config.
    pub fn take_permission_rules(&self) -> Option<crate::permissions::rules::RawPerms> {
        self.shared
            .permission_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Invoke a registered command by name.
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
    pub fn run_keymap(&self, chord: &str, current_mode: Option<&str>) -> bool {
        let func = {
            let Ok(map) = self.shared.keymaps.lock() else {
                return false;
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
                return false;
            };
            let Ok(f) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
                return false;
            };
            f
        };
        if let Err(e) = func.call::<()>(()) {
            self.record_error(format!("keymap `{chord}`: {e}"));
        }
        true
    }

    /// Fire `smelt.mode.cycle()`.
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

    /// Fire `smelt.reasoning.cycle()`.
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

    /// Route an error through `smelt.notify_error` when available.
    pub fn record_error(&self, msg: String) {
        if let Ok(smelt) = self.lua.globals().get::<mlua::Table>("smelt") {
            if let Ok(func) = smelt.get::<mlua::Function>("notify_error") {
                let _ = func.call::<()>(msg);
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
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn list_commands_with_desc(&self) -> Vec<(String, Option<String>)> {
        let mut items: Vec<(String, Option<String>)> = self
            .shared
            .commands
            .lock()
            .map(|m| {
                m.iter()
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

    /// Fire the `on_response` callback for a completed `engine.ask()` call.
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

    pub fn drive_tasks(&self) -> Vec<TaskDriveOutput> {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return Vec::new();
        };
        let outs = rt.drive(&self.lua, Instant::now());
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
        let mut defs = Vec::new();
        for name in handlers.keys() {
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
                    needs_confirm: meta_table.get("hook_needs_confirm").unwrap_or(false),
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

    /// Whether the tool wants its elapsed time displayed in the
    /// transcript header. Read from the `elapsed_visible = true` flag
    /// set on the tool def at registration time.
    pub fn tool_elapsed_visible(&self, tool_name: &str) -> bool {
        let meta = match self
            .lua
            .named_registry_value::<mlua::Table>(&format!("__pt_meta_{tool_name}"))
        {
            Ok(meta) => meta,
            Err(_) => return false,
        };
        meta.get::<bool>("elapsed_visible").unwrap_or(false)
    }

    /// Whether the tool registered a `render_summary` callback. The
    /// caller mints an ephemeral Buffer, runs the callback through
    /// [`render_tool_summary_line`], and replays row 0 into the
    /// transcript / confirm-dialog title.
    pub fn tool_has_render_summary(&self, tool_name: &str) -> bool {
        let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
        handlers
            .get(tool_name)
            .is_some_and(|h| h.render_summary.is_some())
    }

    /// Run a tool's `render_summary` callback against the buffer named
    /// by `buf_id`. Mirrors [`render_tool_body`] but for a single
    /// summary line (transcript header / confirm title). Returns `true`
    /// iff the callback ran successfully.
    pub fn render_tool_summary_line(
        &self,
        tool_name: &str,
        line: &str,
        args: &HashMap<String, serde_json::Value>,
        buf_id: u64,
    ) -> bool {
        let render_fn = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(h) = handlers.get(tool_name) else {
                return false;
            };
            let Some(rh) = h.render_summary.as_ref() else {
                return false;
            };
            match self.lua.registry_value::<mlua::Function>(&rh.key) {
                Ok(f) => f,
                Err(_) => return false,
            }
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool render_summary: build args: {e}"));
                return false;
            }
        };

        if let Err(e) = render_fn.call::<()>((buf_id, line.to_string(), args_table)) {
            self.record_error(format!("tool render_summary `{tool_name}`: {e}"));
            return false;
        }
        true
    }

    pub fn tool_has_render_subhead(&self, tool_name: &str) -> bool {
        let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
        handlers
            .get(tool_name)
            .is_some_and(|h| h.render_subhead.is_some())
    }

    /// Run a tool's `render_subhead` callback against the buffer named
    /// by `buf_id`. The callback paints arbitrary rows below the
    /// summary line (e.g. `web_fetch`'s prompt subline). Returns `true`
    /// iff the callback ran successfully.
    pub fn render_tool_subhead(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        buf_id: u64,
    ) -> bool {
        let render_fn = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(h) = handlers.get(tool_name) else {
                return false;
            };
            let Some(rh) = h.render_subhead.as_ref() else {
                return false;
            };
            match self.lua.registry_value::<mlua::Function>(&rh.key) {
                Ok(f) => f,
                Err(_) => return false,
            }
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool render_subhead: build args: {e}"));
                return false;
            }
        };

        if let Err(e) = render_fn.call::<()>((buf_id, args_table)) {
            self.record_error(format!("tool render_subhead `{tool_name}`: {e}"));
            return false;
        }
        true
    }

    /// Call a tool's `header_suffix(args, ctx)` callback, if registered.
    /// Returns the optional decoration string painted in the row-0 suffix
    /// area (after the elapsed time slot). `ctx.status` is one of
    /// `"pending" | "ok" | "err" | "denied" | "confirm"` so the tool can
    /// branch on lifecycle (e.g. `bash` only emits `(timeout: 2m)` while
    /// pending).
    pub fn tool_header_suffix(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        status: &str,
    ) -> Option<String> {
        let func = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let h = handlers.get(tool_name)?;
            let rh = h.header_suffix.as_ref()?;
            self.lua.registry_value::<mlua::Function>(&rh.key).ok()?
        };
        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool header_suffix: build args: {e}"));
                return None;
            }
        };
        let ctx = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool header_suffix: build ctx: {e}"));
                return None;
            }
        };
        let _ = ctx.set("status", status);
        match func.call::<Option<String>>((args_table, ctx)) {
            Ok(s) => s,
            Err(e) => {
                self.record_error(format!("tool header_suffix `{tool_name}`: {e}"));
                None
            }
        }
    }

    pub fn tool_summary(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> String {
        let meta = match self
            .lua
            .named_registry_value::<mlua::Table>(&format!("__pt_meta_{tool_name}"))
        {
            Ok(meta) => meta,
            Err(_) => return String::new(),
        };
        let func = match meta.get::<mlua::Function>("summary") {
            Ok(func) => func,
            Err(_) => return String::new(),
        };
        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool summary: build args: {e}"));
                return String::new();
            }
        };
        match func.call::<String>(args_table) {
            Ok(summary) => summary,
            Err(e) => {
                self.record_error(format!("tool summary `{tool_name}`: {e}"));
                String::new()
            }
        }
    }

    pub fn evaluate_hooks(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> protocol::ToolHooks {
        let mut out = protocol::ToolHooks::default();

        let (needs_confirm_fn, approval_patterns_fn, preflight_fn) = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(h) = handlers.get(tool_name) else {
                return out;
            };
            let nc = h
                .needs_confirm
                .as_ref()
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok());
            let ap = h
                .approval_patterns
                .as_ref()
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok());
            let pf = h
                .preflight
                .as_ref()
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok());
            (nc, ap, pf)
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool hooks: build args: {e}"));
                return out;
            }
        };

        if let Some(func) = needs_confirm_fn {
            match func.call::<Option<String>>(args_table.clone()) {
                Ok(s) => out.confirm_message = s,
                Err(e) => self.record_error(format!("tool hook needs_confirm: {e}")),
            }
        }
        if let Some(func) = approval_patterns_fn {
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
            match func.call::<Option<String>>(args_table) {
                Ok(Some(s)) => out.decision = protocol::Decision::Error(s),
                Ok(None) => {}
                Err(e) => self.record_error(format!("tool hook preflight: {e}")),
            }
        }

        let summary = self.tool_summary(tool_name, args);
        if !summary.is_empty() {
            out.summary = Some(summary);
        }

        out
    }

    /// Call a tool's Lua `render` hook, if registered.
    /// The hook receives `(args, output, width, buf_id)` and writes its
    /// content into the buffer named by `buf_id` (which the caller has
    /// already created in the UI's buffer registry). Returns `true` iff
    /// the hook ran successfully; the caller decides what fallback to
    /// paint when this returns `false`.
    pub fn render_tool_body(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        output: &crate::transcript_model::ToolOutput,
        width: usize,
        buf_id: u64,
    ) -> bool {
        let render_fn = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(h) = handlers.get(tool_name) else {
                return false;
            };
            let Some(rh) = h.render.as_ref() else {
                return false;
            };
            match self.lua.registry_value::<mlua::Function>(&rh.key) {
                Ok(f) => f,
                Err(_) => return false,
            }
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool render: build args: {e}"));
                return false;
            }
        };

        let output_table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool render: build output table: {e}"));
                return false;
            }
        };
        let _ = output_table.set("content", output.content.clone());
        let _ = output_table.set("is_error", output.is_error);
        if let Some(meta) = &output.metadata {
            match json_to_lua(&self.lua, meta) {
                Ok(v) => {
                    let _ = output_table.set("metadata", v);
                }
                Err(e) => self.record_error(format!("tool render: metadata: {e}")),
            }
        }

        if let Err(e) = render_fn.call::<()>((args_table, output_table, width, buf_id)) {
            self.record_error(format!("tool render `{tool_name}`: {e}"));
            return false;
        }
        true
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

    pub fn execute_tool(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        request_id: u64,
        call_id: &str,
        env: ToolEnv<'_>,
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

        let args_table = match self.args_to_lua_table(args) {
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
        let outputs = rt.drive(&self.lua, Instant::now());
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
            Some((content, is_error)) => ToolExecResult::Immediate { content, is_error },
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

/// Iterate every `.lua` file under [`EMBEDDED_LUA`] as
/// `(module_name, source)` pairs.
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

/// Translate an embedded relative path (`tools/glob.lua`) into a
/// `smelt.*` module name (`smelt.tools.glob`).
fn path_to_module(rel: &str) -> String {
    let trimmed = rel.strip_suffix(".lua").unwrap_or(rel);
    let dotted = trimmed.replace('/', ".");
    format!("smelt.{dotted}")
}

/// Modules to `require` at startup, derived from [`AUTOLOAD_DIRS`].
/// Sorted within each directory for deterministic order. Files
/// already executed in [`BOOTSTRAP_FILES`] are skipped to avoid a
/// re-run during autoload.
pub fn autoload_modules() -> Vec<String> {
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

/// Translate a Lua module name (`smelt.dialogs.confirm`) into a
/// relative file path (`smelt/dialogs/confirm.lua`).
fn module_to_relpath(module: &str) -> PathBuf {
    let mut path = PathBuf::from(module.replace('.', "/"));
    path.set_extension("lua");
    path
}

/// Roots searched for Lua module overrides, in priority order:
/// project-local `.smelt/runtime/`, then user data
/// `<XDG_DATA_HOME>/smelt/runtime/`. The embedded fallback runs last.
fn module_overlay_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join(".smelt").join("runtime"));
    }
    roots.push(engine::data_dir().join("runtime"));
    roots
}

/// Sorted list of `.lua` files directly under `dir`. Missing dir
/// returns empty.
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
        assert!(modules.contains(&"smelt.plugins.background_commands".to_string()));
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
        std::env::set_var("XDG_STATE_HOME", state.path());

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
        std::env::set_var("XDG_STATE_HOME", state.path());
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
