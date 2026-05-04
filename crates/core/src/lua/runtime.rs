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

use mlua::prelude::*;

use crate::lua::{
    json_to_lua, LuaShared, TaskCompletion, TaskDriveOutput, TaskEvent, ToolEnv, ToolExecResult,
};

/// Modules embedded in the binary, available via `require("smelt.*")`.
const EMBEDDED_MODULES: &[(&str, &str)] = &[
    (
        "smelt.plugins.plan_mode",
        include_str!("../../../../runtime/lua/smelt/plugins/plan_mode.lua"),
    ),
    (
        "smelt.commands.btw",
        include_str!("../../../../runtime/lua/smelt/commands/btw.lua"),
    ),
    (
        "smelt.plugins.predict",
        include_str!("../../../../runtime/lua/smelt/plugins/predict.lua"),
    ),
    (
        "smelt.tools.ask_user_question",
        include_str!("../../../../runtime/lua/smelt/tools/ask_user_question.lua"),
    ),
    (
        "smelt.commands.export",
        include_str!("../../../../runtime/lua/smelt/commands/export.lua"),
    ),
    (
        "smelt.dialogs.rewind",
        include_str!("../../../../runtime/lua/smelt/dialogs/rewind.lua"),
    ),
    (
        "smelt.plugins.background_commands",
        include_str!("../../../../runtime/lua/smelt/plugins/background_commands.lua"),
    ),
    (
        "smelt.commands.help",
        include_str!("../../../../runtime/lua/smelt/commands/help.lua"),
    ),
    (
        "smelt.plugins.yank_block",
        include_str!("../../../../runtime/lua/smelt/plugins/yank_block.lua"),
    ),
    (
        "smelt.dialogs.permissions",
        include_str!("../../../../runtime/lua/smelt/dialogs/permissions.lua"),
    ),
    (
        "smelt.dialogs.resume",
        include_str!("../../../../runtime/lua/smelt/dialogs/resume.lua"),
    ),
    (
        "smelt.commands.theme",
        include_str!("../../../../runtime/lua/smelt/commands/theme.lua"),
    ),
    (
        "smelt.commands.color",
        include_str!("../../../../runtime/lua/smelt/commands/color.lua"),
    ),
    (
        "smelt.commands.model",
        include_str!("../../../../runtime/lua/smelt/commands/model.lua"),
    ),
    (
        "smelt.commands.settings",
        include_str!("../../../../runtime/lua/smelt/commands/settings.lua"),
    ),
    (
        "smelt.commands.history_search",
        include_str!("../../../../runtime/lua/smelt/commands/history_search.lua"),
    ),
    (
        "smelt.commands.toggles",
        include_str!("../../../../runtime/lua/smelt/commands/toggles.lua"),
    ),
    (
        "smelt.commands.stats",
        include_str!("../../../../runtime/lua/smelt/commands/stats.lua"),
    ),
    (
        "smelt.commands.session",
        include_str!("../../../../runtime/lua/smelt/commands/session.lua"),
    ),
    (
        "smelt.commands.quit",
        include_str!("../../../../runtime/lua/smelt/commands/quit.lua"),
    ),
    (
        "smelt.commands.compact",
        include_str!("../../../../runtime/lua/smelt/commands/compact.lua"),
    ),
    (
        "smelt.commands.reflect",
        include_str!("../../../../runtime/lua/smelt/commands/reflect.lua"),
    ),
    (
        "smelt.commands.simplify",
        include_str!("../../../../runtime/lua/smelt/commands/simplify.lua"),
    ),
    (
        "smelt.commands.custom_commands",
        include_str!("../../../../runtime/lua/smelt/commands/custom_commands.lua"),
    ),
    (
        "smelt.colorschemes.default",
        include_str!("../../../../runtime/lua/smelt/colorschemes/default.lua"),
    ),
    (
        "smelt.tools.glob",
        include_str!("../../../../runtime/lua/smelt/tools/glob.lua"),
    ),
    (
        "smelt.tools.grep",
        include_str!("../../../../runtime/lua/smelt/tools/grep.lua"),
    ),
    (
        "smelt.tools.load_skill",
        include_str!("../../../../runtime/lua/smelt/tools/load_skill.lua"),
    ),
    (
        "smelt.tools.web_search",
        include_str!("../../../../runtime/lua/smelt/tools/web_search.lua"),
    ),
    (
        "smelt.tools.write_file",
        include_str!("../../../../runtime/lua/smelt/tools/write_file.lua"),
    ),
    (
        "smelt.tools.edit_file",
        include_str!("../../../../runtime/lua/smelt/tools/edit_file.lua"),
    ),
    (
        "smelt.tools.read_file",
        include_str!("../../../../runtime/lua/smelt/tools/read_file.lua"),
    ),
    (
        "smelt.tools.notebook_edit",
        include_str!("../../../../runtime/lua/smelt/tools/notebook_edit.lua"),
    ),
    (
        "smelt.tools.web_fetch",
        include_str!("../../../../runtime/lua/smelt/tools/web_fetch.lua"),
    ),
    (
        "smelt.tools.bash",
        include_str!("../../../../runtime/lua/smelt/tools/bash.lua"),
    ),
];

/// Bootstrap Lua chunks loaded at `register_api` time, after the
/// `smelt` global is fully populated but before any plugin or user
/// init.lua runs. Not `require`-able — they extend `smelt` directly.
const BOOTSTRAP_CHUNKS: &[(&str, &str)] = &[
    (
        "smelt/_bootstrap.lua",
        include_str!("../../../../runtime/lua/smelt/_bootstrap.lua"),
    ),
    (
        "smelt/dialog.lua",
        include_str!("../../../../runtime/lua/smelt/dialog.lua"),
    ),
    (
        "smelt/widgets/picker.lua",
        include_str!("../../../../runtime/lua/smelt/widgets/picker.lua"),
    ),
    (
        "smelt/widgets/prompt_picker.lua",
        include_str!("../../../../runtime/lua/smelt/widgets/prompt_picker.lua"),
    ),
    (
        "smelt/cmd.lua",
        include_str!("../../../../runtime/lua/smelt/cmd.lua"),
    ),
    (
        "smelt/dialogs/confirm.lua",
        include_str!("../../../../runtime/lua/smelt/dialogs/confirm.lua"),
    ),
    (
        "smelt/status.lua",
        include_str!("../../../../runtime/lua/smelt/status.lua"),
    ),
    (
        "smelt/modes.lua",
        include_str!("../../../../runtime/lua/smelt/modes.lua"),
    ),
];

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

        out
    }

    /// Call a tool's Lua `render` hook, if registered.
    /// Receives the tool args, the `ToolOutput`, and a `RenderCtx` userdata.
    /// Returns the number of rows added to `out`.
    pub fn render_tool_body(
        &self,
        tool_name: &str,
        args: &HashMap<String, serde_json::Value>,
        output: &crate::transcript_model::ToolOutput,
        width: usize,
        out: &mut crate::content::layout_out::SpanCollector,
    ) -> u16 {
        let render_fn = {
            let handlers = self.shared.tools.lock().unwrap_or_else(|e| e.into_inner());
            let Some(h) = handlers.get(tool_name) else {
                return crate::transcript_present::render_default_output(
                    out,
                    &output.content,
                    output.is_error,
                    width,
                );
            };
            let Some(rh) = h.render.as_ref() else {
                return crate::transcript_present::render_default_output(
                    out,
                    &output.content,
                    output.is_error,
                    width,
                );
            };
            match self.lua.registry_value::<mlua::Function>(&rh.key) {
                Ok(f) => f,
                Err(_) => {
                    return crate::transcript_present::render_default_output(
                        out,
                        &output.content,
                        output.is_error,
                        width,
                    );
                }
            }
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool render: build args: {e}"));
                return crate::transcript_present::render_default_output(
                    out,
                    &output.content,
                    output.is_error,
                    width,
                );
            }
        };

        let output_table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tool render: build output table: {e}"));
                return crate::transcript_present::render_default_output(
                    out,
                    &output.content,
                    output.is_error,
                    width,
                );
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

        let before = out.line_count();
        let ctx = super::render_ctx::RenderCtx::new(out, width);
        let ctx_ud = match self.lua.create_userdata(ctx) {
            Ok(ud) => ud,
            Err(e) => {
                self.record_error(format!("tool render: create ctx: {e}"));
                return crate::transcript_present::render_default_output(
                    out,
                    &output.content,
                    output.is_error,
                    width,
                );
            }
        };

        if let Err(e) = render_fn.call::<()>((args_table, output_table, width, ctx_ud)) {
            self.record_error(format!("tool render `{tool_name}`: {e}"));
        }

        let after = out.line_count();
        (after - before) as u16
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
    for (name, src) in BOOTSTRAP_CHUNKS {
        lua.load(*src).set_name(*name).exec()?;
    }
    Ok(())
}

fn register_embedded_searcher(lua: &Lua) -> LuaResult<()> {
    let searcher = lua.create_function(|lua, module: String| {
        for &(name, source) in EMBEDDED_MODULES {
            if name == module {
                let loader = lua.load(source).set_name(name).into_function()?;
                return Ok(mlua::Value::Function(loader));
            }
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
