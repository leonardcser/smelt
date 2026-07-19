//! Lua bindings. Wraps the `api::*` surface so users can script smelt
//! from `~/.config/smelt/init.lua`.

#![allow(clippy::arc_with_non_send_sync)]

pub(crate) mod api;
pub(crate) use api::vim::LuaVimMode;
pub use api::{BUILD_SHA, BUILD_TARGET, DISPLAY};
pub mod app_ref;
pub(crate) mod paint;
pub(crate) mod parse;
mod tasks;
pub(crate) mod ui_ops;

pub use app_ref::try_with_app;
pub(crate) use app_ref::{install_app_ptr, try_with_core, with_app, with_app_ptr};

pub(crate) use smelt_core::lua::{LuaHandle, TaskDriveOutput, ToolEnv, ToolExecResult};

use mlua::prelude::*;

use std::sync::{Arc, Mutex};

/// Format a `crossterm::KeyEvent` into an nvim-style chord string
/// (`<C-g>`, `<S-Tab>`, `<M-x>`, printable `j`, etc).
/// The result is the lookup key for `smelt.keymap.set`. Returns `None` for unrecognized chords.
pub(crate) fn chord_string(key: crossterm::event::KeyEvent) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers as M};
    let mods = key.modifiers;
    let has_ctrl = mods.contains(M::CONTROL);
    let has_alt = mods.contains(M::ALT);
    let has_shift = mods.contains(M::SHIFT);
    let base = match key.code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "Tab".to_string(),
        KeyCode::Enter => "CR".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "BS".to_string(),
        KeyCode::Delete => "Del".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Insert => "Insert".to_string(),
        _ => return None,
    };
    let is_named = !matches!(key.code, KeyCode::Char(_));
    if !has_ctrl && !has_alt && (!has_shift || matches!(key.code, KeyCode::Char(_))) && !is_named {
        // Plain printable char - no angle-bracket wrap.
        return Some(base);
    }
    let mut prefix = String::new();
    if has_ctrl {
        prefix.push_str("C-");
    }
    if has_alt {
        prefix.push_str("M-");
    }
    if has_shift && is_named {
        prefix.push_str("S-");
    }
    Some(format!("<{prefix}{base}>"))
}

/// Parse a plugin key spec into a [`crate::smelt_edit::KeyBind`].
/// Accepts shorthand (`"c-j"`, `"s-tab"`, `"enter"`) and canonical bracket form
/// (`"<C-r>"`, `"<S-Tab>"`). Modifiers separate with `-`; case-insensitive.
/// Returns `None` for unknown keys.
pub(crate) fn parse_keybind(spec: &str) -> Option<crate::smelt_edit::KeyBind> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let raw = spec.trim();
    if raw.is_empty() {
        return None;
    }
    let raw = raw
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(raw);
    let (mods, name) = match raw.rsplit_once('-') {
        Some((prefix, name)) => {
            let mut mods = KeyModifiers::NONE;
            for part in prefix.split('-') {
                match part.to_ascii_lowercase().as_str() {
                    "ctrl" | "c" => mods |= KeyModifiers::CONTROL,
                    "alt" | "a" | "meta" | "m" => mods |= KeyModifiers::ALT,
                    "shift" | "s" => mods |= KeyModifiers::SHIFT,
                    _ => return None,
                }
            }
            (mods, name)
        }
        None => (KeyModifiers::NONE, raw),
    };
    let code = match name.to_ascii_lowercase().as_str() {
        "bs" | "backspace" => KeyCode::Backspace,
        "tab" => {
            if mods.contains(KeyModifiers::SHIFT) {
                return Some(crate::smelt_edit::KeyBind::new(
                    KeyCode::BackTab,
                    mods - KeyModifiers::SHIFT,
                ));
            }
            KeyCode::Tab
        }
        "del" | "delete" => KeyCode::Delete,
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        s if s.starts_with('f') && s[1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u8 = s[1..].parse().ok()?;
            if !(1..=12).contains(&n) {
                return None;
            }
            KeyCode::F(n)
        }
        s if s.chars().count() == 1 => KeyCode::Char(name.chars().next().unwrap()),
        _ => return None,
    };
    Some(crate::smelt_edit::KeyBind::new(code, mods))
}

/// Normalize a mode string to the canonical single-char form (`"n"`, `"i"`, `"v"`, `""`).
/// Accepts long names, short names, `"any"`, and `"*"`. Case-insensitive.
/// Returns `None` for unknown input.
pub(crate) fn normalize_mode(mode: &str) -> Option<String> {
    Some(
        match mode.trim().to_ascii_lowercase().as_str() {
            "" | "*" | "any" | "all" => "",
            "n" | "normal" => "n",
            "i" | "insert" => "i",
            "v" | "visual" | "visual_line" | "visualline" => "v",
            _ => return None,
        }
        .to_string(),
    )
}

/// Canonicalize a chord string to the nvim angle-bracket form `chord_string` produces.
/// Accepts shorthand and already-canonical input. Returns `None` for unknown keys.
pub(crate) fn canonicalize_chord(chord: &str) -> Option<String> {
    use crossterm::event::KeyEvent;
    let kb = parse_keybind(chord)?;
    chord_string(KeyEvent::new(kb.code, kb.mods))
}

/// Canonicalize a chord sequence (one or more tokens: `<...>` or single printable chars)
/// and expand `<leader>` tokens to `leader` when provided.
pub(crate) fn canonicalize_chord_sequence_with_leader(
    input: &str,
    leader: Option<&str>,
) -> Option<String> {
    if !input.contains('<') {
        if let Some(single) = canonicalize_chord(input) {
            return Some(single);
        }
    }
    let tokens = tokenize_chord_spec(input)?;
    let mut out = String::new();
    for tok in tokens {
        if tok.eq_ignore_ascii_case("<leader>") {
            out.push_str(leader?);
        } else {
            let canonical = canonicalize_chord(&tok)?;
            out.push_str(&canonical);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Format an internal canonical chord sequence for user-facing introspection.
pub(crate) fn display_chord_sequence(chord: &str) -> String {
    let mut out = String::new();
    let mut chars = chord.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' {
            out.push_str("<space>");
        } else if c == '<' {
            out.push('<');
            for cc in chars.by_ref() {
                out.push(cc);
                if cc == '>' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Canonicalize a single leader key. `<leader>` is not accepted here because
/// leader expansion is only meaningful in registered keymap sequences.
pub(crate) fn canonicalize_leader(input: &str) -> Option<String> {
    if input.trim().eq_ignore_ascii_case("<leader>") {
        return None;
    }
    canonicalize_chord(input)
}

/// Split a chord-spec into individual tokens (`<...>` or single printable chars).
/// Whitespace between tokens is allowed. Returns `None` if the input is malformed.
fn tokenize_chord_spec(input: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '<' {
            let mut tok = String::from('<');
            let mut closed = false;
            for cc in chars.by_ref() {
                tok.push(cc);
                if cc == '>' {
                    closed = true;
                    break;
                }
            }
            if !closed || tok == "<>" {
                return None;
            }
            tokens.push(tok);
        } else {
            tokens.push(c.to_string());
        }
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

/// Mint the next auto-name for a kind ("paint" | "buf" | "win" |
/// "overlay") in the currently active plugin scope. Returns `None`
/// when no module body is on the Lua call stack (e.g. the caller is
/// inside an event-loop callback). Plugin authors get auto-named
/// hot-reload-survivable resources without typing `opts.name = "..."`.
pub(crate) fn auto_name_for_scope(lua: &Lua, kind: &str) -> Option<String> {
    let f: mlua::Function = lua.globals().get("__smelt_auto_name").ok()?;
    f.call::<Option<String>>(kind).ok().flatten()
}

/// Stash a Lua callable in `shared.callbacks` under a fresh u64 id.
/// Used by every `smelt.win.*` binding that takes a callback.
pub(crate) fn register_callback_handle(
    shared: &Arc<LuaShared>,
    lua: &Lua,
    func: mlua::Function,
) -> mlua::Result<u64> {
    let handle = smelt_core::lua::LuaHandle::from_func(lua, func)?;
    let id = shared
        .next_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut cbs) = shared.callbacks.lock() {
        cbs.insert(id, handle);
    }
    Ok(id)
}

/// Drop the Lua handle displaced by a `win_set_keymap` / `win_clear_*` call, if any.
pub(crate) fn drop_displaced_lua_handle(
    app: &mut crate::app::TuiApp,
    displaced: Option<crate::smelt_edit::Callback>,
) {
    if let Some(crate::smelt_edit::Callback::Lua(crate::smelt_edit::LuaHandle(old))) = displaced {
        app.lua.remove_callback(old);
    }
}

/// Callback invocation queued while `&mut Ui` is held.
/// Drained by TuiApp after the ui call returns, with the TLS app pointer installed.
pub(crate) struct PendingInvocation {
    pub(crate) handle: crate::smelt_edit::LuaHandle,
    pub(crate) win: crate::smelt_edit::WinId,
    pub(crate) payload: crate::smelt_edit::Payload,
}

struct TranscriptViewWatcher {
    id: u64,
    callback: LuaHandle,
    last_view_revision: Option<u64>,
}

#[derive(Default)]
pub(crate) struct TranscriptViewWatchers {
    next_id: std::sync::atomic::AtomicU64,
    entries: Mutex<Vec<TranscriptViewWatcher>>,
}

impl TranscriptViewWatchers {
    pub(crate) fn register(&self, lua: &Lua, callback: mlua::Function) -> LuaResult<u64> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let callback = LuaHandle::from_func(lua, callback)?;
        self.entries
            .lock()
            .map_err(|_| mlua::Error::external("transcript view watcher registry poisoned"))?
            .push(TranscriptViewWatcher {
                id,
                callback,
                last_view_revision: None,
            });
        Ok(id)
    }

    pub(crate) fn remove(&self, id: u64) -> bool {
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        entries.len() != before
    }

    pub(crate) fn pending_callbacks(&self, lua: &Lua, view_revision: u64) -> Vec<mlua::Function> {
        let Ok(mut entries) = self.entries.lock() else {
            return Vec::new();
        };
        entries
            .iter_mut()
            .filter(|entry| entry.last_view_revision != Some(view_revision))
            .filter_map(|entry| {
                let callback = lua
                    .registry_value::<mlua::Function>(&entry.callback.key)
                    .ok()?;
                entry.last_view_revision = Some(view_revision);
                Some(callback)
            })
            .collect()
    }
}

/// TUI-specific extension of [`smelt_core::lua::LuaShared`] adding the
/// `pending_invocations` queue. `Deref`s to the core shared state.
pub(crate) struct LuaShared {
    pub(crate) core: Arc<smelt_core::lua::LuaShared>,
    pub(crate) pending_invocations: Mutex<Vec<PendingInvocation>>,
    pub(crate) transcript_view_watchers: Arc<TranscriptViewWatchers>,
    /// Last terminal title declaration made while this generation was a
    /// candidate. `Some(None)` means clear the title at commit.
    pub(crate) staged_terminal_title: Mutex<Option<Option<String>>>,
    pub(crate) staged_notices: Mutex<Vec<(smelt_core::messages::MessageKind, String, String)>>,
}

impl Default for LuaShared {
    fn default() -> Self {
        Self::with_core(Arc::new(smelt_core::lua::LuaShared::default()))
    }
}

impl std::ops::Deref for LuaShared {
    type Target = smelt_core::lua::LuaShared;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl LuaShared {
    fn with_core(core: Arc<smelt_core::lua::LuaShared>) -> Self {
        Self {
            core,
            pending_invocations: Mutex::new(Vec::new()),
            transcript_view_watchers: Arc::new(TranscriptViewWatchers::default()),
            staged_terminal_title: Mutex::new(None),
            staged_notices: Mutex::new(Vec::new()),
        }
    }

    /// Clone the inner `Arc<smelt_core::lua::LuaShared>` for core API modules.
    pub(crate) fn core_arc(&self) -> Arc<smelt_core::lua::LuaShared> {
        Arc::clone(&self.core)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LuaLoadManifest {
    pub roots: Vec<std::path::PathBuf>,
    pub files: Vec<std::path::PathBuf>,
    pub target_cwd: Option<std::path::PathBuf>,
}

#[derive(Clone)]
pub struct ModeDeclarations {
    pub cycle: Vec<protocol::AgentMode>,
    pub behaviors: std::collections::HashMap<String, smelt_core::permissions::ModeBehavior>,
}

#[derive(Clone)]
pub struct PermissionDeclarations {
    pub rules: smelt_core::permissions::rules::RawPerms,
    pub tool_defaults: smelt_core::permissions::rules::ToolDefaults,
}

#[derive(Clone)]
pub struct LuaDesiredState {
    pub config: smelt_core::config::Config,
    pub modes: ModeDeclarations,
    pub permissions: PermissionDeclarations,
    pub default_shell: Option<smelt_core::lua::DefaultShell>,
}

/// TUI-specific Lua runtime. Wraps [`smelt_core::lua::LuaRuntime`] and adds
/// the callback queue and statusline rendering.
pub struct LuaRuntime {
    core: smelt_core::lua::LuaRuntime,
    shared: Arc<LuaShared>,
}

impl std::ops::Deref for LuaRuntime {
    type Target = smelt_core::lua::LuaRuntime;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl std::ops::DerefMut for LuaRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}

impl LuaRuntime {
    /// Build a fresh runtime and register the `smelt` global.
    /// Does not load `init.lua` - call [`LuaRuntime::load_autoload`] after
    /// startup snapshots are available so plugins see real data at registration time.
    pub fn new() -> Self {
        Self::with_shared(Arc::new(LuaShared::default()))
    }

    fn with_shared(shared: Arc<LuaShared>) -> Self {
        let core = smelt_core::lua::LuaRuntime::with_shared(shared.core_arc());
        Self::with_core(core, shared, true)
    }

    fn with_core(
        mut core: smelt_core::lua::LuaRuntime,
        shared: Arc<LuaShared>,
        load_bootstrap: bool,
    ) -> Self {
        if core.load_error.is_none() {
            if let Err(error) = Self::register_api(&core.lua, &shared) {
                core.load_error = Some(error.to_string());
            }
        }
        if core.load_error.is_none() {
            if load_bootstrap {
                core.load_full_bootstrap();
            } else {
                core.enable_ui_bootstrap();
            }
        }
        Self { core, shared }
    }

    /// Build a fresh candidate with generation-local registries and the
    /// committed generation's session-long host services and launch inputs.
    pub fn fresh_candidate(&self, target_cwd: Option<&std::path::Path>) -> Self {
        let core_shared = Arc::new(smelt_core::lua::LuaShared::with_host_services(
            self.shared.host_services(),
        ));
        core_shared.stage_external_effects();
        let shared = Arc::new(LuaShared::with_core(core_shared));
        let core = self.core.fresh_with_shared(shared.core_arc(), target_cwd);
        let mut candidate = Self::with_core(core, shared, false);
        candidate.core.inherit_launch_inputs(&self.core);
        candidate
    }

    #[cfg(any(test, feature = "harness"))]
    pub fn set_load_paths_for_harness(
        &mut self,
        config_dir: std::path::PathBuf,
        runtime_override: Option<std::path::PathBuf>,
        target_cwd: Option<std::path::PathBuf>,
    ) {
        self.core
            .set_load_paths_for_harness(config_dir, runtime_override, target_cwd);
    }

    /// Validate and clone all Lua-owned declarations in one coherent snapshot.
    pub fn snapshot_desired_state(&self) -> Result<LuaDesiredState, String> {
        if let Some(error) = self.load_error() {
            return Err(error.to_string());
        }
        Ok(self.collect_desired_state())
    }

    fn collect_desired_state(&self) -> LuaDesiredState {
        LuaDesiredState {
            config: self.to_config(),
            modes: ModeDeclarations {
                cycle: self.mode_names(),
                behaviors: self.mode_behaviors(),
            },
            permissions: PermissionDeclarations {
                rules: self.permission_rules_snapshot().unwrap_or_default(),
                tool_defaults: self.tool_defaults(),
            },
            default_shell: self.default_shell_snapshot(),
        }
    }

    /// Register the full API surface (host + UiHost) and bundled bootstrap
    /// chunks without spinning up the full runtime state.
    /// Used by `gen-lua-docs` to harvest the doc registry.
    pub fn register_for_docs() -> mlua::Result<()> {
        let shared = Arc::new(LuaShared::default());
        let mut core = smelt_core::lua::LuaRuntime::with_shared(shared.core_arc());
        Self::register_api(&core.lua, &shared)?;
        core.load_full_bootstrap();
        Ok(())
    }

    /// Borrow the shared state (e.g. to clone the `Arc` into tokio tasks).
    pub(crate) fn shared(&self) -> &Arc<LuaShared> {
        &self.shared
    }

    /// Borrow the underlying `smelt_core` shared registry. Used by the
    /// main binary to read `cli_flag_specs` after `early.lua` runs and
    /// write back parsed `cli_flag_values`.
    pub fn core_shared(&self) -> &Arc<smelt_core::lua::LuaShared> {
        &self.shared.core
    }

    pub(crate) fn lua(&self) -> &Lua {
        &self.core.lua
    }

    pub fn into_core(self) -> smelt_core::lua::LuaRuntime {
        self.core
    }

    /// Run every bundled `runtime/lua/smelt/early/*.lua` file. Call BEFORE
    /// [`Self::load_early_init`] so user code can override flag declarations.
    pub fn load_bundled_early(&mut self) {
        self.core.load_bundled_early();
    }

    /// Drain `smelt.lifecycle.on(event, fn)` callbacks for `event`. Returns
    /// per-hook errors so the caller can surface them as notifications;
    /// invocation failures are isolated so one hook can't suppress the rest.
    /// `build_ctx` constructs the per-event ctx table fresh inside the Lua
    /// runtime borrow.
    pub fn drain_lifecycle_hooks<F>(&mut self, event: &str, build_ctx: F) -> Vec<String>
    where
        F: Fn(&mlua::Lua) -> mlua::Result<mlua::Value>,
    {
        self.core.drain_lifecycle_hooks(event, build_ctx)
    }

    /// Forward to [`smelt_core::lua::LuaRuntime::drain_shutdown_hooks`].
    pub fn drain_shutdown_hooks(&mut self, ctx: &crate::app::ShutdownContext) -> Vec<String> {
        self.core
            .drain_shutdown_hooks(smelt_core::lua::ShutdownHookContext {
                session_id: &ctx.session_id,
                has_messages: ctx.has_messages,
                ephemeral: ctx.ephemeral,
            })
    }

    /// Evaluate `~/.config/smelt/early.lua` (if present). Call BEFORE
    /// [`Self::load_autoload`] so user opt-outs take effect.
    pub fn load_early_init(&mut self) {
        self.core.load_early_init();
    }

    /// Evaluate `.smelt/early.lua` (if present and the project is trusted).
    /// Call BEFORE [`Self::load_autoload`].
    pub fn load_project_early_init(&mut self, cwd: &std::path::Path) {
        self.core.load_project_early_init(cwd);
    }

    /// Fire the `smelt.engine.ask` callback registered under `id` with
    /// `(response_or_nil, err_or_nil)`. The success payload mirrors the
    /// provider-shaped assistant `protocol::Message` row; the error table mirrors the
    /// `smelt.engine.AskError` shape - `{ kind, message }` strings -
    /// so plugins can branch on the failure mode without parsing text.
    pub(crate) fn fire_ask_callback(
        &self,
        id: u64,
        message: Option<&protocol::Message>,
        error: Option<protocol::EngineAskError>,
    ) {
        let callbacks = {
            let Ok(mut cbs) = self.shared.ask_callbacks.lock() else {
                return;
            };
            cbs.remove(&id)
        };
        let Some(callbacks) = callbacks else {
            return;
        };
        let Some(handle) = callbacks.response else {
            return;
        };
        let Ok(func) = self.core.lua.registry_value::<mlua::Function>(&handle.key) else {
            return;
        };
        let response_value: mlua::Value = match message {
            Some(msg) => {
                smelt_core::lua::serde_to_lua(&self.core.lua, msg).unwrap_or(mlua::Value::Nil)
            }
            None => mlua::Value::Nil,
        };
        let err_value: mlua::Value = match error {
            None => mlua::Value::Nil,
            Some(e) => match self.core.lua.create_table() {
                Ok(t) => {
                    let _ = t.set("kind", e.kind.as_str());
                    let _ = t.set("message", e.message);
                    mlua::Value::Table(t)
                }
                Err(_) => mlua::Value::Nil,
            },
        };
        let _perf = smelt_perf::perf::begin("lua:ask_cb");
        if let Err(e) = func.call::<()>((response_value, err_value)) {
            self.record_error(format!("ask callback: {e}"));
        }
    }

    pub(crate) fn fire_ask_delta_callback(&self, id: u64, delta: &str) {
        let func = {
            let Ok(cbs) = self.shared.ask_callbacks.lock() else {
                return;
            };
            let Some(callbacks) = cbs.get(&id) else {
                return;
            };
            let Some(handle) = callbacks.delta.as_ref() else {
                return;
            };
            let Ok(func) = self.core.lua.registry_value::<mlua::Function>(&handle.key) else {
                return;
            };
            func
        };
        let _perf = smelt_perf::perf::begin("lua:ask_delta_cb");
        if let Err(e) = func.call::<()>(delta.to_string()) {
            self.record_error(format!("ask delta callback: {e}"));
        }
    }

    pub(crate) fn mode_note(&self, name: &str) -> String {
        let result: mlua::Result<String> = (|| {
            let smelt: mlua::Table = self.core.lua.globals().get("smelt")?;
            let mode: mlua::Table = smelt.get("mode")?;
            let note: mlua::Function = mode.get("note")?;
            note.call(name.to_string())
        })();
        result.unwrap_or_else(|_| format!("now in {name} mode"))
    }

    pub(crate) fn mode_block(
        &self,
        name: Option<&str>,
        note: &str,
    ) -> smelt_core::transcript_model::Block {
        let result: mlua::Result<(String, String, String)> = (|| {
            let smelt: mlua::Table = self.core.lua.globals().get("smelt")?;
            let mode: mlua::Table = smelt.get("mode")?;
            if let Some(name) = name {
                let get: mlua::Function = mode.get("get")?;
                let info: Option<mlua::Table> = get.call(name.to_string())?;
                if let Some(info) = info {
                    let icon = info.get::<Option<String>>("icon")?.unwrap_or_default();
                    let hl_group = info
                        .get::<Option<String>>("hl_group")?
                        .unwrap_or_else(|| "SmeltModeDefault".to_string());
                    return Ok((icon, hl_group, format!("now in {name} mode")));
                }
                return Ok((
                    String::new(),
                    "SmeltModeDefault".to_string(),
                    format!("now in {name} mode"),
                ));
            }
            let list: mlua::Function = mode.get("list")?;
            let rows: mlua::Table = list.call(())?;
            for row in rows.sequence_values::<mlua::Table>() {
                let row = row?;
                if row.get::<Option<String>>("note")?.as_deref() == Some(note) {
                    let name = row.get::<String>("name")?;
                    let icon = row.get::<Option<String>>("icon")?.unwrap_or_default();
                    let hl_group = row
                        .get::<Option<String>>("hl_group")?
                        .unwrap_or_else(|| "SmeltModeDefault".to_string());
                    return Ok((icon, hl_group, format!("now in {name} mode")));
                }
            }
            Ok((
                String::new(),
                "SmeltModeDefault".to_string(),
                note.to_string(),
            ))
        })();
        let (icon, hl_group, text) = result.unwrap_or_else(|_| {
            (
                String::new(),
                "SmeltModeDefault".to_string(),
                note.to_string(),
            )
        });
        smelt_core::transcript_model::Block::Mode {
            text,
            icon,
            hl_group,
        }
    }

    pub fn mode_names(&self) -> Vec<protocol::AgentMode> {
        let result: mlua::Result<Vec<String>> = (|| {
            let smelt: mlua::Table = self.core.lua.globals().get("smelt")?;
            let mode: mlua::Table = smelt.get("mode")?;
            let list: mlua::Function = mode.get("list")?;
            let rows: mlua::Table = list.call(())?;
            let mut names = Vec::new();
            for row in rows.sequence_values::<mlua::Table>() {
                let row = row?;
                names.push(row.get::<String>("name")?);
            }
            Ok(names)
        })();
        result
            .unwrap_or_default()
            .into_iter()
            .filter_map(|name| protocol::AgentMode::parse(&name))
            .collect()
    }

    pub fn mode_behaviors(
        &self,
    ) -> std::collections::HashMap<String, smelt_core::permissions::ModeBehavior> {
        let result: mlua::Result<
            std::collections::HashMap<String, smelt_core::permissions::ModeBehavior>,
        > = (|| {
            let smelt: mlua::Table = self.core.lua.globals().get("smelt")?;
            let mode: mlua::Table = smelt.get("mode")?;
            let behaviors: mlua::Function = mode.get("permission_behaviors")?;
            let table: mlua::Table = behaviors.call(())?;
            let mut out = std::collections::HashMap::new();
            for pair in table.pairs::<String, mlua::Table>() {
                let (name, spec) = pair?;
                let default_decision =
                    match spec.get::<Option<String>>("default_decision")?.as_deref() {
                        Some("allow") => protocol::Decision::Allow,
                        Some("deny") => protocol::Decision::Deny,
                        _ => protocol::Decision::Ask,
                    };
                out.insert(
                    name,
                    smelt_core::permissions::ModeBehavior {
                        default_decision,
                        allow_subcommands_by_default: spec
                            .get("allow_subcommands_by_default")
                            .unwrap_or(false),
                        ask_on_output_redirection: spec
                            .get("ask_on_output_redirection")
                            .unwrap_or(true),
                    },
                );
            }
            Ok(out)
        })();
        result.unwrap_or_default()
    }

    /// Fire `smelt.confirm.open(handle_id)` to hand a pending confirm request to the Lua dialog.
    pub(crate) fn fire_confirm_open(&self, handle_id: u64) {
        let result: mlua::Result<()> = (|| {
            let smelt: mlua::Table = self.core.lua.globals().get("smelt")?;
            let confirm: mlua::Table = smelt.get("confirm")?;
            let open: mlua::Function = confirm.get("open")?;
            open.call::<()>(handle_id)
        })();
        if let Err(e) = result {
            self.record_error(format!("smelt.confirm.open: {e}"));
        }
    }
}

pub struct FailedLuaCandidate {
    pub message: String,
    pub location: smelt_core::lua::LuaLoadFailureLocation,
}

pub struct LuaGeneration {
    pub id: u64,
    runtime: LuaRuntime,
    desired: LuaDesiredState,
    pub manifest: LuaLoadManifest,
    pub project_trust: smelt_core::trust::TrustState,
    warnings: Vec<String>,
}

impl LuaGeneration {
    pub fn initial(
        mut runtime: LuaRuntime,
        target_cwd: Option<&std::path::Path>,
        project_trust: smelt_core::trust::TrustState,
    ) -> Self {
        let manifest = LuaLoadManifest::discover(&runtime, target_cwd);
        let desired = runtime.collect_desired_state();
        let warnings = runtime.take_load_warnings();
        Self {
            id: 0,
            runtime,
            desired,
            manifest,
            project_trust,
            warnings,
        }
    }

    #[cfg(any(test, feature = "harness"))]
    pub fn load_initial_for_harness(
        &mut self,
        target_cwd: Option<&std::path::Path>,
    ) -> Option<String> {
        let error = self.runtime.reload(target_cwd);
        if error.is_none() {
            self.desired = self.runtime.collect_desired_state();
            self.manifest = LuaLoadManifest::discover(&self.runtime, target_cwd);
            self.project_trust = target_cwd.map_or(
                smelt_core::trust::TrustState::NoContent,
                smelt_core::trust::project_trust_state,
            );
            self.runtime.mark_running();
        }
        error
    }

    pub fn load_candidate(
        &self,
        id: u64,
        target_cwd: Option<&std::path::Path>,
        candidate_skills: std::sync::Arc<engine::SkillLoader>,
        wakeup: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Result<Self, FailedLuaCandidate> {
        let state = self.runtime.state_snapshot();
        let mut runtime = self.runtime.fresh_candidate(target_cwd);
        runtime.core_shared().set_candidate_skills(candidate_skills);
        runtime.shared().set_generation_id(id);
        runtime.set_wakeup_sender(wakeup);
        if let Some(message) = runtime.reload_with_state(target_cwd, Some(&state)) {
            let location = runtime.load_failure_location().cloned().unwrap_or(
                smelt_core::lua::LuaLoadFailureLocation {
                    phase: "load",
                    path: None,
                },
            );
            return Err(FailedLuaCandidate { message, location });
        }
        let desired = runtime
            .snapshot_desired_state()
            .map_err(|message| FailedLuaCandidate {
                message,
                location: smelt_core::lua::LuaLoadFailureLocation {
                    phase: "runtime_resolution",
                    path: None,
                },
            })?;
        let manifest = LuaLoadManifest::discover(&runtime, target_cwd);
        let warnings = runtime.take_load_warnings();
        let project_trust = target_cwd.map_or(smelt_core::trust::TrustState::NoContent, |cwd| {
            smelt_core::trust::project_trust_state(cwd)
        });
        Ok(Self {
            id,
            runtime,
            desired,
            manifest,
            project_trust,
            warnings,
        })
    }

    pub fn desired(&self) -> &LuaDesiredState {
        &self.desired
    }

    /// Refresh declarations after a live Lua desired-state write. Candidate
    /// loads set this snapshot once before commit; runtime writes use the same
    /// snapshot path before asking the app to reconcile effects.
    pub fn refresh_desired_state(&mut self) -> Result<(), String> {
        self.desired = self.runtime.snapshot_desired_state()?;
        Ok(())
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn activate(&self) -> Result<(), String> {
        self.runtime
            .commit_candidate()
            .map_err(|error| error.to_string())
    }

    pub fn retire(&mut self) {
        self.runtime.retire();
    }
}

impl std::ops::Deref for LuaGeneration {
    type Target = LuaRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl std::ops::DerefMut for LuaGeneration {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.runtime
    }
}

impl LuaLoadManifest {
    fn discover(runtime: &LuaRuntime, target_cwd: Option<&std::path::Path>) -> Self {
        let mut roots = runtime.load_manifest_roots(target_cwd);
        if let Some(parent) = runtime
            .configured_init_lua_path()
            .and_then(std::path::Path::parent)
        {
            roots.push(parent.to_path_buf());
        }
        roots.sort();
        roots.dedup();

        Self {
            roots,
            files: runtime.loaded_config_files(),
            target_cwd: target_cwd.map(std::path::Path::to_path_buf),
        }
    }
}

impl Default for LuaRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::content::block_layout::{
        BlockLayout, CapKeep, CapMarker, Constraint, LuaLeaf, TextSpec,
    };
    use smelt_core::lua::api::lua_table_to_json;
    use smelt_core::transcript_model::{Block, BlockId, ToolOutput, ToolState, ToolStatus};

    /// Stub `smelt.notify.info` / `smelt.notify.error` to push into `_G.test_log` / `_G.test_err`.
    fn install_test_notify(rt: &LuaRuntime) {
        rt.lua
            .load(
                r#"
                    _G.test_log = {}
                    _G.test_err = {}
                    _G.test_warn = {}
                    smelt.notify.info = function(msg) table.insert(_G.test_log, msg) end
                    smelt.notify.error = function(msg) table.insert(_G.test_err, msg) end
                    smelt.notify.warn = function(msg) table.insert(_G.test_warn, msg) end
                "#,
            )
            .exec()
            .expect("install_test_notify");
    }

    fn test_env() -> ToolEnv<'static> {
        static EMPTY_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let p = EMPTY_PATH.get_or_init(std::path::PathBuf::new);
        ToolEnv {
            mode: protocol::AgentMode::parse("apply").unwrap(),
            session_id: "",
            session_dir: p,
        }
    }

    fn drain_notifications(rt: &LuaRuntime) -> Vec<String> {
        let log: mlua::Table = match rt.lua.globals().get("test_log") {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let out: Vec<String> = log
            .sequence_values::<String>()
            .filter_map(|r| r.ok())
            .collect();
        let _ = rt
            .lua
            .globals()
            .set("test_log", rt.lua.create_table().unwrap());
        out
    }

    fn drain_errors(rt: &LuaRuntime) -> Vec<String> {
        let log: mlua::Table = match rt.lua.globals().get("test_err") {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let out: Vec<String> = log
            .sequence_values::<String>()
            .filter_map(|r| r.ok())
            .collect();
        let _ = rt
            .lua
            .globals()
            .set("test_err", rt.lua.create_table().unwrap());
        out
    }

    fn drain_warnings(rt: &LuaRuntime) -> Vec<String> {
        let log: mlua::Table = match rt.lua.globals().get("test_warn") {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let out: Vec<String> = log
            .sequence_values::<String>()
            .filter_map(|r| r.ok())
            .collect();
        let _ = rt
            .lua
            .globals()
            .set("test_warn", rt.lua.create_table().unwrap());
        out
    }

    #[test]
    fn invoke_callback_runs_registered_fn_with_selection_payload() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                _G.recorded = nil
                _G.test_cb = function(ctx) _G.recorded = ctx.index end
            "#,
            )
            .exec()
            .unwrap();
        let func: mlua::Function = rt.lua.load("test_cb").eval().unwrap();
        let id = rt.register_callback(func).unwrap();
        rt.invoke_callback(
            crate::smelt_edit::LuaHandle(id),
            crate::smelt_edit::WinId(0),
            &crate::smelt_edit::Payload::Selection { index: 2 },
        );
        let recorded: u64 = rt.lua.load("return _G.recorded").eval().unwrap();
        assert_eq!(recorded, 3); // Selection is 0-indexed; Lua gets 1-based.
    }

    #[test]
    fn invoke_callback_text_payload() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                _G.t = nil
                _G.cb = function(ctx) _G.t = ctx.text end
            "#,
            )
            .exec()
            .unwrap();
        let func: mlua::Function = rt.lua.load("cb").eval().unwrap();
        let id = rt.register_callback(func).unwrap();
        rt.invoke_callback(
            crate::smelt_edit::LuaHandle(id),
            crate::smelt_edit::WinId(0),
            &crate::smelt_edit::Payload::Text {
                content: "hi".into(),
            },
        );
        let t: String = rt.lua.load("return _G.t").eval().unwrap();
        assert_eq!(t, "hi");
    }

    #[test]
    fn invoke_callback_unknown_handle_is_noop() {
        let rt = LuaRuntime::new();
        // Nothing registered under id 9999 - should silently succeed.
        rt.invoke_callback(
            crate::smelt_edit::LuaHandle(9999),
            crate::smelt_edit::WinId(0),
            &crate::smelt_edit::Payload::None,
        );
    }

    /// Every code path dropping a Lua callback must call `remove_callback`; otherwise the
    /// registry grows unbounded. Invariant: register inserts, remove evicts, invoke is a no-op.
    #[test]
    fn remove_callback_evicts_handle_from_registry() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                _G.fired = 0
                _G.cb = function() _G.fired = _G.fired + 1 end
            "#,
            )
            .exec()
            .unwrap();
        let func: mlua::Function = rt.lua.load("cb").eval().unwrap();
        let id = rt.register_callback(func).unwrap();
        assert_eq!(rt.shared.callbacks.lock().unwrap().len(), 1);

        rt.remove_callback(id);
        assert!(rt.shared.callbacks.lock().unwrap().is_empty());

        // Dropped handle must not fire.
        rt.invoke_callback(
            crate::smelt_edit::LuaHandle(id),
            crate::smelt_edit::WinId(0),
            &crate::smelt_edit::Payload::None,
        );
        let fired: u64 = rt.lua.load("return _G.fired").eval().unwrap();
        assert_eq!(fired, 0);
    }

    /// `fire_ask_callback` must only look at `ask_callbacks`. A non-ask
    /// handler registered with the same id in the win/overlay/paint map
    /// must NOT fire when an `EngineAskResponse` arrives with that id.
    #[test]
    fn fire_ask_callback_ignores_non_ask_handles() {
        let rt = LuaRuntime::new();
        rt.lua
            .load("_G.fired = 0; _G.cb = function() _G.fired = _G.fired + 1 end")
            .exec()
            .unwrap();
        let func: mlua::Function = rt.lua.load("cb").eval().unwrap();
        // Register in the main callbacks map (win/overlay/paint registry).
        let id = rt.register_callback(func).unwrap();

        // Synthesize an EngineAskResponse for the same id. The shared id
        // counter means a real ask call would never collide with this id,
        // but a buggy engine emitting a stale id used to fire the wrong
        // handler - verify the ask path stays in its own lane.
        let msg =
            protocol::Message::assistant(Some(protocol::Content::text("synthetic")), None, None);
        rt.fire_ask_callback(id, Some(&msg), None);

        let fired: u64 = rt.lua.load("return _G.fired").eval().unwrap();
        assert_eq!(fired, 0, "non-ask handle must not fire on ask response");
    }

    #[test]
    fn fire_ask_delta_callback_streams_until_final_response() {
        let rt = LuaRuntime::new();
        rt.lua
            .load("_G.delta = ''; _G.cb = function(d) _G.delta = _G.delta .. d end")
            .exec()
            .unwrap();
        let func: mlua::Function = rt.lua.load("cb").eval().unwrap();
        let handle = LuaHandle::from_func(&rt.lua, func).unwrap();
        rt.shared.ask_callbacks.lock().unwrap().insert(
            7,
            smelt_core::lua::AskCallbacks {
                response: None,
                delta: Some(handle),
            },
        );

        rt.fire_ask_delta_callback(7, "hel");
        rt.fire_ask_delta_callback(7, "lo");
        let delta: String = rt.lua.load("return _G.delta").eval().unwrap();
        assert_eq!(delta, "hello");

        let msg = protocol::Message::assistant(Some(protocol::Content::text("hello")), None, None);
        rt.fire_ask_callback(7, Some(&msg), None);
        assert!(rt.shared.ask_callbacks.lock().unwrap().is_empty());
    }

    // Theme role-mapping and error logic are tested in `lua::api::tests`.

    #[test]
    fn runtime_exposes_api_version_and_build_identity() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let api: String = rt
            .lua
            .load("return smelt.api_version")
            .eval()
            .expect("eval");
        assert_eq!(api, crate::lua::api::API_VERSION);
        let app: String = rt
            .lua
            .load("return smelt.build.version")
            .eval()
            .expect("eval");
        assert_eq!(app, crate::lua::api::APP_VERSION);
        assert!(
            !app.is_empty() && app.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "smelt.build.version should be the program version, got {app:?}"
        );
        let target: String = rt
            .lua
            .load("return smelt.build.target")
            .eval()
            .expect("eval");
        assert!(!target.is_empty(), "smelt.build.target should be non-empty");
    }

    #[test]
    fn layout_style_api_wraps_child() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let layout = rt
            .lua
            .load(
                r#"
                return smelt.layout.style(smelt.layout.text("styled"), {
                  hl = "SmeltAccent",
                  fg = "SmeltAccent",
                  bg = "SmeltUserBg",
                  dim = true,
                  bold = true,
                  italic = true,
                })
                "#,
            )
            .eval::<mlua::AnyUserData>()
            .expect("eval style layout");
        let layout = layout
            .borrow::<smelt_core::lua::api::layout::LuaBlockLayout>()
            .unwrap();
        let BlockLayout::Style { child, spec } = &layout.0 else {
            panic!("expected style layout, got {:?}", layout.0);
        };
        assert_eq!(spec.hl_group.as_deref(), Some("SmeltAccent"));
        assert_eq!(spec.fg.as_deref(), Some("SmeltAccent"));
        assert_eq!(spec.bg.as_deref(), Some("SmeltUserBg"));
        assert!(spec.dim && spec.bold && spec.italic);
        assert_text_layout(child, "styled");
    }

    #[test]
    fn layout_elapsed_api_builds_dynamic_leaf() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let layout = rt
            .lua
            .load(
                r#"
                return smelt.layout.elapsed({ call_id = "c1", status = "ok", secs = 65 }, {
                  hl = "SmeltToolPending",
                  selectable = false,
                })
                "#,
            )
            .eval::<mlua::AnyUserData>()
            .expect("eval elapsed layout");
        let layout = layout
            .borrow::<smelt_core::lua::api::layout::LuaBlockLayout>()
            .unwrap();
        let BlockLayout::Leaf(LuaLeaf::Elapsed(spec)) = &layout.0 else {
            panic!("expected elapsed layout, got {:?}", layout.0);
        };
        assert_eq!(spec.call_id, "c1");
        assert_eq!(spec.fallback_secs, Some(65));
        assert_eq!(spec.hl_group.as_deref(), Some("SmeltToolPending"));
        assert!(!spec.selectable);
    }

    fn render_transcript_block(
        rt: &LuaRuntime,
        block: &Block,
        state: Option<&ToolState>,
    ) -> BlockLayout {
        rt.render_transcript_layout(
            BlockId::new(7),
            0,
            block,
            state,
            smelt_core::transcript_model::ViewState::Expanded,
        )
    }

    fn assert_text_layout(layout: &BlockLayout, expected: &str) {
        let BlockLayout::Leaf(LuaLeaf::Text(TextSpec { content, .. })) = layout else {
            panic!("expected text layout, got {layout:?}");
        };
        assert_eq!(content, expected);
    }

    fn assert_markdown_layout(layout: &BlockLayout, expected: &str) {
        let BlockLayout::Leaf(LuaLeaf::Markdown(spec)) = layout else {
            panic!("expected markdown layout, got {layout:?}");
        };
        assert_eq!(spec.content, expected);
    }

    fn assert_line_layout(layout: &BlockLayout, expected: &str) {
        let BlockLayout::Leaf(LuaLeaf::Line(spec)) = layout else {
            panic!("expected line layout, got {layout:?}");
        };
        let text: String = spec.spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(text, expected);
    }

    fn tool_block(name: &str, output: &str) -> (Block, ToolState) {
        (
            Block::ToolCall {
                call_id: format!("{name}-call"),
                name: name.into(),
                summary: protocol::StyledLines::from_plain(name),
                args: std::collections::HashMap::new(),
            },
            ToolState {
                status: ToolStatus::Ok,
                elapsed: Some(std::time::Duration::from_secs(65)),
                output: Some(Box::new(ToolOutput {
                    content: output.into(),
                    is_error: false,
                    metadata: None,
                })),
                user_message: None,
                preview_output: None,
            },
        )
    }

    fn assert_tool_body_is_raw_tail(layout: &BlockLayout, expected: &str) {
        let BlockLayout::Vbox(items) = layout else {
            panic!("expected tool vbox, got {layout:?}");
        };
        assert_eq!(items.len(), 2);
        let BlockLayout::Gutter { child, .. } = &items[1] else {
            panic!("expected body gutter, got {:?}", items[1]);
        };
        let BlockLayout::Cap { child, spec } = child.as_ref() else {
            panic!("expected raw output cap, got {child:?}");
        };
        assert_eq!(
            spec.keep,
            CapKeep::Tail {
                marker: Some(CapMarker::Above)
            }
        );
        assert_text_layout(child, expected);
    }

    fn assert_tool_header_has_dynamic_elapsed(layout: &BlockLayout, call_id: &str) {
        let BlockLayout::Vbox(items) = layout else {
            panic!("expected tool vbox, got {layout:?}");
        };
        let (header, prefixed) = match &items[0] {
            BlockLayout::Cap { child, .. } => (child.as_ref(), false),
            BlockLayout::RowPrefix { child, spec } => {
                assert!(
                    spec.rest.iter().any(|span| !span.text.is_empty()),
                    "expected continued header rows to keep prefix indentation"
                );
                let BlockLayout::Cap { child, .. } = child.as_ref() else {
                    panic!("expected capped prefixed header, got {:?}", items[0]);
                };
                (child.as_ref(), true)
            }
            other => panic!("expected capped header, got {other:?}"),
        };
        let BlockLayout::Hbox(items) = header else {
            panic!("expected dynamic elapsed hbox header, got {header:?}");
        };
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].constraint, Constraint::Fit);
        assert_eq!(items[1].constraint, Constraint::Length(2));
        assert_eq!(items[2].constraint, Constraint::Length(8));
        let BlockLayout::Leaf(LuaLeaf::Runs(spec)) = &items[0].layout else {
            panic!("expected runs header leaf, got {:?}", items[0].layout);
        };
        if !prefixed {
            assert!(spec.continuation_indent > 0);
        }
        let BlockLayout::Leaf(LuaLeaf::Elapsed(spec)) = &items[2].layout else {
            panic!("expected elapsed header leaf, got {:?}", items[2].layout);
        };
        assert_eq!(spec.call_id, call_id);
        assert!(!spec.selectable);
    }

    #[test]
    fn raw_process_outputs_use_tail_cap_once() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);

        for name in ["bash", "read_process_output"] {
            let (block, state) = tool_block(name, "one\ntwo\nthree");
            let layout = render_transcript_block(&rt, &block, Some(&state));
            assert_tool_body_is_raw_tail(&layout, "one\ntwo\nthree");
            assert_tool_header_has_dynamic_elapsed(&layout, &format!("{name}-call"));
        }
    }

    #[test]
    fn structured_tool_bodies_are_not_capped_by_default() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        rt.lua
            .load(
                r#"
                local defaults = require("smelt.transcript.defaults")
                defaults.__tool_body_renderers.no_cap_probe = function()
                  return smelt.layout.text("structured body")
                end
                "#,
            )
            .exec()
            .unwrap();

        let (block, state) = tool_block("no_cap_probe", "ignored");
        let layout = render_transcript_block(&rt, &block, Some(&state));
        let BlockLayout::Vbox(items) = layout else {
            panic!("expected tool vbox");
        };
        let BlockLayout::Gutter { child, .. } = &items[1] else {
            panic!("expected body gutter, got {:?}", items[1]);
        };
        assert_text_layout(child, "structured body");
    }

    #[test]
    fn transcript_default_renderer_handles_simple_blocks() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);

        let assistant = Block::Text {
            content: "hello".into(),
        };
        let layout = render_transcript_block(&rt, &assistant, None);
        assert_markdown_layout(&layout, "hello");

        let mode = Block::Mode {
            text: "plan".into(),
            icon: "◇ ".into(),
            hl_group: "SmeltModeDefault".into(),
        };
        let layout = render_transcript_block(&rt, &mode, None);
        assert_line_layout(&layout, "◇ plan");

        let tool = Block::ToolCall {
            call_id: "call-1".into(),
            name: "bash".into(),
            summary: protocol::StyledLines::from_plain("echo hi"),
            args: std::collections::HashMap::new(),
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: Some(std::time::Duration::from_secs(65)),
            output: Some(Box::new(ToolOutput {
                content: "hi".into(),
                is_error: false,
                metadata: None,
            })),
            user_message: Some("done".into()),
            preview_output: None,
        };
        let layout = render_transcript_block(&rt, &tool, Some(&state));
        let BlockLayout::Vbox(items) = layout else {
            panic!("expected tool vbox");
        };
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn transcript_renderer_extension_composes_and_invalidates() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let g0 = rt.transcript_renderer_generation();

        rt.lua
            .load(
                r#"
                local transcript = require("smelt.transcript")
                transcript.set_renderer(function(block, ctx)
                  return smelt.layout.text("base:" .. block.kind .. ":" .. tostring(ctx.renderer_generation ~= nil))
                end)
                _G.transcript_reg = transcript.extend_renderer("test_wrap", function(next, block, ctx)
                  return smelt.layout.gutter(next(block, ctx), { text = "> " })
                end)
            "#,
            )
            .exec()
            .unwrap();
        let g1 = rt.transcript_renderer_generation();
        assert!(g1 > g0);

        let block = Block::User {
            text: "hello".into(),
            image_labels: Vec::new(),
            command: false,
        };
        let layout = render_transcript_block(&rt, &block, None);
        let BlockLayout::Gutter { child, spec } = layout else {
            panic!("expected middleware gutter");
        };
        assert_eq!(spec.text, "> ");
        assert_text_layout(&child, "base:user:true");

        let removed: bool = rt
            .lua
            .load("return _G.transcript_reg:remove()")
            .eval()
            .unwrap();
        assert!(removed);
        let g2 = rt.transcript_renderer_generation();
        assert!(g2 > g1);
        let layout = render_transcript_block(&rt, &block, None);
        assert_text_layout(&layout, "base:user:true");

        rt.lua
            .load("return require('smelt.transcript').invalidate_renderer()")
            .eval::<u64>()
            .unwrap();
        assert!(rt.transcript_renderer_generation() > g2);
    }

    #[test]
    fn transcript_renderer_nil_and_errors_fall_back_but_empty_hides() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        install_test_notify(&rt);
        let block = Block::Text {
            content: "fallback".into(),
        };

        rt.lua
            .load("require('smelt.transcript').set_renderer(function() error('boom') end)")
            .exec()
            .unwrap();
        let layout = render_transcript_block(&rt, &block, None);
        assert_text_layout(&layout, "fallback");
        assert!(drain_errors(&rt).iter().any(|e| e.contains("boom")));

        rt.lua
            .load("require('smelt.transcript').set_renderer(function() return nil end)")
            .exec()
            .unwrap();
        let layout = render_transcript_block(&rt, &block, None);
        assert_text_layout(&layout, "fallback");
        assert!(drain_errors(&rt).iter().any(|e| e.contains("returned nil")));

        rt.lua
            .load("require('smelt.transcript').set_renderer(function() return smelt.layout.empty() end)")
            .exec()
            .unwrap();
        let layout = render_transcript_block(&rt, &block, None);
        assert!(matches!(layout, BlockLayout::Empty));
    }

    #[test]
    fn lua_sequence_tables_serialize_as_json_arrays() {
        let lua = Lua::new();
        let tbl: mlua::Table = lua
            .load(r#"return { "label", "description" }"#)
            .eval()
            .expect("eval");
        let json = lua_table_to_json(&lua, &tbl);
        assert_eq!(
            json,
            serde_json::json!(["label", "description"]),
            "1..N integer keys must become JSON array"
        );

        let obj: mlua::Table = lua
            .load(r#"return { type = "object", properties = {} }"#)
            .eval()
            .expect("eval");
        let json2 = lua_table_to_json(&lua, &obj);
        assert_eq!(json2["type"], serde_json::json!("object"));
    }

    #[test]
    fn session_context_note_noops_before_app_ptr() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);

        rt.lua
            .load("smelt.session.context_note('goal', 'hello')")
            .exec()
            .unwrap();
    }

    #[test]
    fn autoload_registers_process_detach_keymap() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);

        let rows: mlua::Table = rt.lua.load("return smelt.keymap.list()").eval().unwrap();
        let mut found = false;
        for row in rows.sequence_values::<mlua::Table>() {
            let row = row.unwrap();
            let chord: String = row.get("chord").unwrap();
            let desc: Option<String> = row.get("desc").unwrap();
            if chord == "<C-g>" {
                assert_eq!(desc.as_deref(), Some("move running command to background"));
                found = true;
            }
        }
        assert!(found, "process detach keymap should be autoloaded");
    }

    #[test]
    fn autoload_registers_export_and_copy_commands() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("export"),
            "/export should be registered by the autoloaded plugin"
        );
        assert!(
            rt.has_command("copy"),
            "/copy should be registered by the autoloaded plugin"
        );
        assert!(
            rt.has_command("yank"),
            "/yank should be registered as a /copy alias"
        );
    }

    #[test]
    fn autoload_registers_builtin_commands() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        for name in ["brief", "fast", "handoff", "rewind", "worktree", "wt"] {
            assert!(
                rt.has_command(name),
                "/{name} should be registered by autoload"
            );
        }
    }

    #[test]
    fn autoload_registers_context_barrier_tools_as_sequential() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let defs = rt.tool_defs(
            protocol::AgentMode::normal(),
            smelt_core::lua::ToolVisibility::Interactive,
        );
        for name in ["ask_user_question", "enter_worktree", "switch_cwd"] {
            let tool = defs
                .iter()
                .find(|definition| definition.name == name)
                .unwrap_or_else(|| panic!("{name} should be auto-registered"));
            assert_eq!(tool.execution_mode, protocol::ToolExecutionMode::Sequential);
        }
    }

    #[test]
    fn autoload_marks_ui_only_tools_unavailable_headless() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);

        let interactive = rt.tool_defs(
            protocol::AgentMode::normal(),
            smelt_core::lua::ToolVisibility::Interactive,
        );
        let headless = rt.tool_defs(
            protocol::AgentMode::normal(),
            smelt_core::lua::ToolVisibility::Headless,
        );
        let interactive_names: Vec<_> = interactive.iter().map(|d| d.name.as_str()).collect();
        let headless_names: Vec<_> = headless.iter().map(|d| d.name.as_str()).collect();

        for name in ["ask_user_question", "enter_worktree", "switch_cwd"] {
            assert!(
                interactive_names.contains(&name),
                "{name} should be available interactively"
            );
            assert!(
                !headless_names.contains(&name),
                "{name} should be unavailable headless"
            );
        }
        assert!(
            headless_names.contains(&"read_file"),
            "ordinary bundled tools should remain available headless"
        );
    }

    #[test]
    fn autoload_registers_conflicting_file_tools_as_sequential() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let defs = rt.tool_defs(
            protocol::AgentMode::normal(),
            smelt_core::lua::ToolVisibility::Interactive,
        );
        for name in ["edit_file", "edit_notebook"] {
            let tool = defs
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("{name} should be auto-registered"));
            assert_eq!(tool.execution_mode, protocol::ToolExecutionMode::Sequential);
        }
        let write = defs
            .iter()
            .find(|d| d.name == "write_file")
            .expect("write_file should be auto-registered");
        assert_eq!(
            write.execution_mode,
            protocol::ToolExecutionMode::Concurrent
        );
    }

    #[test]
    fn tool_summary_comes_from_lua() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.tools.register({
                  name = "echo_summary",
                  description = "",
                  parameters = { type = "object", properties = {} },
                  summary = function(args) return "lua:" .. (args.label or "") end,
                  execute = function(args) return args.label or "" end,
                })
                "#,
            )
            .exec()
            .unwrap();
        let mut args = std::collections::HashMap::new();
        args.insert("label".into(), serde_json::json!("ok"));
        assert_eq!(
            rt.tool_summary("echo_summary", &args).as_plain_text(),
            "lua:ok"
        );
    }

    #[test]
    fn tool_summary_receives_final_context() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.tools.register({
                  name = "summary_context",
                  description = "",
                  parameters = { type = "object", properties = {} },
                  summary = function(args, ctx) return ctx.final and "final" or "draft" end,
                  execute = function() return "" end,
                })
                "#,
            )
            .exec()
            .unwrap();
        let args = std::collections::HashMap::new();
        assert_eq!(
            rt.tool_summary_with_context("summary_context", &args, false)
                .as_plain_text(),
            "draft"
        );
        assert_eq!(
            rt.tool_summary("summary_context", &args).as_plain_text(),
            "final"
        );
    }

    #[test]
    fn dialog_open_outside_task_errors() {
        // Calling `smelt.dialog.open` outside a yieldable coroutine
        // (the runtime file's first guard) must raise. With plugins
        // loaded the Lua wrapper is in place; `isyieldable()` is false
        // at the top level, so the call errors before reaching the
        // Rust `_open` binding.
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let res: LuaResult<()> = rt.lua.load("smelt.dialog.open({panels = {}})").exec();
        assert!(res.is_err());
    }

    #[test]
    fn tool_runs_as_task_immediate() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.tools.register({
                  name = "echo",
                  description = "",
                  parameters = { type = "object", properties = {} },
                  execute = function(args) return "hi " .. (args.who or "?") end,
                })
                "#,
            )
            .exec()
            .unwrap();
        let mut args = std::collections::HashMap::new();
        args.insert("who".into(), serde_json::json!("world"));
        match rt.execute_tool(
            "echo",
            &args,
            1,
            "c1",
            test_env(),
            std::time::Instant::now(),
        ) {
            ToolExecResult::Immediate {
                content,
                is_error,
                metadata,
            } => {
                assert_eq!(content, "hi world");
                assert!(!is_error);
                assert!(metadata.is_none());
            }
            ToolExecResult::Pending => panic!("expected immediate"),
        }
    }

    #[test]
    fn tool_yield_returns_pending_then_tool_complete() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.tools.register({
                  name = "wait_then_yes",
                  description = "",
                  parameters = { type = "object", properties = {} },
                  execute = function()
                    smelt.sleep(0)
                    return "yes"
                  end,
                })
                "#,
            )
            .exec()
            .unwrap();
        let args = std::collections::HashMap::new();
        match rt.execute_tool(
            "wait_then_yes",
            &args,
            7,
            "c9",
            test_env(),
            std::time::Instant::now(),
        ) {
            ToolExecResult::Pending => {}
            ToolExecResult::Immediate { .. } => panic!("expected pending after yield"),
        }
        // sleep(0) is elapsed; task resumes and completes.
        let outs = rt.drive_tasks(std::time::Instant::now());
        let complete = outs
            .iter()
            .find(|o| matches!(o, TaskDriveOutput::ToolComplete { .. }))
            .expect("expected ToolComplete");
        match complete {
            TaskDriveOutput::ToolComplete {
                invocation,
                call_id,
                content,
                is_error,
                metadata,
            } => {
                assert_eq!(invocation.request_id, 7);
                assert_eq!(call_id, "c9");
                assert_eq!(content, "yes");
                assert!(!*is_error);
                assert!(metadata.is_none());
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_file_tool_accepts_utf8_split_at_sample_boundary() {
        let tmp = tempfile::NamedTempFile::new().expect("tmp");
        let path = tmp.path().to_string_lossy().into_owned();
        let content = format!("{}─\nvisible\n", "a".repeat(8191));
        std::fs::write(&path, content).expect("write fixture");

        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);

        let mut args = std::collections::HashMap::new();
        args.insert("file_path".into(), serde_json::json!(path));
        let result = rt.execute_tool(
            "read_file",
            &args,
            11,
            "call-read-file-utf8-boundary",
            ToolEnv {
                mode: protocol::AgentMode::normal(),
                session_id: "sess",
                session_dir: tmp.path(),
            },
            std::time::Instant::now(),
        );
        assert!(matches!(result, ToolExecResult::Pending));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut completion = None;
        while std::time::Instant::now() < deadline && completion.is_none() {
            rt.pump_task_events();
            completion = rt
                .drive_tasks(std::time::Instant::now())
                .into_iter()
                .find_map(|out| match out {
                    TaskDriveOutput::ToolComplete {
                        invocation,
                        call_id,
                        content,
                        is_error,
                        ..
                    } if invocation.request_id == 11
                        && call_id == "call-read-file-utf8-boundary" =>
                    {
                        Some((content, is_error))
                    }
                    _ => None,
                });
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        let (content, is_error) = completion.expect("read_file completion");
        assert!(!is_error, "{content}");
        assert!(content.contains("   2\tvisible"), "{content}");
    }

    #[test]
    fn notify_queues_for_drain() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load("smelt.notify.info('hello from lua')")
            .exec()
            .expect("exec");
        let msgs = drain_notifications(&rt);
        assert_eq!(msgs, vec!["hello from lua".to_string()]);
        assert!(drain_notifications(&rt).is_empty());
    }

    #[test]
    fn syntax_error_captured_not_panicked() {
        let mut rt = LuaRuntime::new();
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "this is not valid lua @@@").unwrap();
        let err = rt.load_init(tmp.path());
        assert!(err.is_err(), "expected syntax error");
    }

    #[test]
    fn cmd_register_and_run() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.cmd.register("hello", function()
                        smelt.notify.info("hello world")
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        assert!(rt.has_command("hello"));
        assert!(rt.run_command("hello", None));
        assert_eq!(drain_notifications(&rt), vec!["hello world".to_string()]);
        assert!(!rt.run_command("unknown", None));
    }

    #[test]
    fn inspect_command_notifies_with_scoped_notify() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    require("smelt.plugins.inspect")
                    smelt.inspect.url = function() return "http://127.0.0.1:1234" end
                    smelt.inspect.open = function(_) return {} end
                "#,
            )
            .exec()
            .expect("load inspect plugin");

        assert!(rt.run_command("inspect", None));
        assert_eq!(
            drain_notifications(&rt),
            vec!["UI: http://127.0.0.1:1234".to_string()]
        );
        assert!(drain_errors(&rt).is_empty());
    }

    #[test]
    fn upgrade_background_check_stays_quiet_when_fetch_fails() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.settings = {
                      autoupgrade = "notify",
                      autoupgrade_channel = "stable",
                      autoupgrade_interval = 3600,
                    }

                    smelt.banner = smelt.banner or {}
                    smelt.banner.source = function() end

                    smelt.spawn = function(fn)
                      fn()
                      return { remove = function() end }
                    end

                    smelt.tick.every = function(_, fn)
                      _G.upgrade_tick = fn
                      return { remove = function() end }
                    end

                    smelt.process.run = function()
                      return { exit_code = 1, stdout = "", stderr = "" }
                    end

                    smelt.http.get = function()
                      return nil, "network is unreachable"
                    end

                    smelt.state.__save = function() end

                    require("smelt.plugins.upgrade")
                "#,
            )
            .exec()
            .expect("load upgrade plugin");
        rt.lua
            .load("assert(_G.upgrade_tick ~= nil); _G.upgrade_tick()")
            .exec()
            .expect("run captured tick");
        assert!(drain_notifications(&rt).is_empty());
        assert!(drain_warnings(&rt).is_empty());
        assert!(drain_errors(&rt).is_empty());
    }

    #[test]
    fn upgrade_check_command_defers_cleanly_when_fetch_fails() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.settings = {
                      autoupgrade = "notify",
                      autoupgrade_channel = "stable",
                      autoupgrade_interval = 3600,
                    }

                    smelt.banner = smelt.banner or {}
                    smelt.banner.source = function() end

                    smelt.spawn = function(fn)
                      fn()
                      return { remove = function() end }
                    end

                    smelt.tick.every = function(_, fn)
                      _G.upgrade_tick = fn
                      return { remove = function() end }
                    end

                    smelt.process.run = function()
                      return { exit_code = 1, stdout = "", stderr = "" }
                    end

                    smelt.http.get = function()
                      return nil, "network is unreachable"
                    end

                    require("smelt.plugins.upgrade")
                "#,
            )
            .exec()
            .expect("load upgrade plugin");
        assert!(rt.run_command("upgrade", Some("check".to_string())));
        assert_eq!(
            drain_notifications(&rt),
            vec!["checking for upgrades…".to_string()]
        );
        assert_eq!(
            drain_warnings(&rt),
            vec!["autoupgrade: network is unreachable\nretrying later".to_string()]
        );
        assert!(drain_errors(&rt).is_empty());
    }

    #[test]
    fn keymap_register_and_run() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.keymap.set("n", "<C-g>", function()
                        smelt.notify.info("ctrl-g")
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        use smelt_core::lua::runtime::KeymapResult;
        assert_eq!(
            rt.run_keymap("<C-g>", Some("Normal"), None),
            KeymapResult::Consumed
        );
        assert_eq!(drain_notifications(&rt), vec!["ctrl-g".to_string()]);
        assert_eq!(
            rt.run_keymap("<C-g>", Some("Insert"), None),
            KeymapResult::NoBinding
        );
        assert_eq!(
            rt.run_keymap("<C-x>", Some("Normal"), None),
            KeymapResult::NoBinding
        );
    }

    #[test]
    fn reload_clears_tui_surfaces() {
        // End-to-end reload across core (cmd) + TUI (keymap) registries.
        // Catches the case where a new surface is added to `LuaShared` and
        // someone forgets to extend `clear_lua_handles`.
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(
            &init,
            r#"
                smelt.cmd.register("plug_cmd", function() end)
                smelt.keymap.set("n", "<C-o>", function() end)
            "#,
        )
        .unwrap();

        let mut rt = LuaRuntime::new();
        rt.set_init_lua_path(init.clone());
        rt.load_user_config();
        assert!(
            rt.load_error().is_none(),
            "first load: {:?}",
            rt.load_error()
        );

        let shared = rt.shared.clone();
        assert!(shared.commands.lock().unwrap().contains_key("plug_cmd"));
        let has_user_chord = |k: &std::collections::HashMap<(String, String), _>| {
            k.keys().any(|(_, c)| c == "<C-o>")
        };
        assert!(has_user_chord(&shared.keymaps.lock().unwrap()));

        // Reload to an empty body: every user-registered surface disappears.
        // Autoload-registered keymaps (e.g. F5/reload, F12/perf_panel) come
        // back, so we only assert the user chord is gone.
        std::fs::write(&init, "").unwrap();
        let err = rt.reload(None);
        assert!(err.is_none(), "reload: {err:?}");

        assert!(!shared.commands.lock().unwrap().contains_key("plug_cmd"));
        assert!(!has_user_chord(&shared.keymaps.lock().unwrap()));
    }

    #[test]
    fn keymap_wildcard_mode() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.keymap.set("", "<C-h>", function()
                        smelt.notify.info("any-mode")
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        use smelt_core::lua::runtime::KeymapResult;
        assert_eq!(
            rt.run_keymap("<C-h>", Some("Normal"), None),
            KeymapResult::Consumed
        );
        assert_eq!(drain_notifications(&rt), vec!["any-mode".to_string()]);
        assert_eq!(
            rt.run_keymap("<C-h>", Some("Insert"), None),
            KeymapResult::Consumed
        );
        assert_eq!(rt.run_keymap("<C-h>", None, None), KeymapResult::Consumed);
    }

    #[test]
    fn chord_string_formats_nvim_style() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
        let ev = |code, mods| KeyEvent::new(code, mods);
        assert_eq!(
            chord_string(ev(KeyCode::Char('j'), M::NONE)).as_deref(),
            Some("j")
        );
        assert_eq!(
            chord_string(ev(KeyCode::Char('g'), M::CONTROL)).as_deref(),
            Some("<C-g>")
        );
        assert_eq!(
            chord_string(ev(KeyCode::Tab, M::SHIFT)).as_deref(),
            Some("<S-Tab>")
        );
        assert_eq!(
            chord_string(ev(KeyCode::Esc, M::NONE)).as_deref(),
            Some("<Esc>")
        );
        assert_eq!(
            chord_string(ev(KeyCode::Char('x'), M::ALT)).as_deref(),
            Some("<M-x>")
        );
    }

    #[test]
    fn parse_keybind_handles_names_and_modifiers() {
        use crossterm::event::{KeyCode, KeyModifiers};
        assert_eq!(
            parse_keybind("enter"),
            Some(crate::smelt_edit::KeyBind::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))
        );
        assert_eq!(
            parse_keybind("esc"),
            Some(crate::smelt_edit::KeyBind::new(
                KeyCode::Esc,
                KeyModifiers::NONE
            ))
        );
        assert_eq!(
            parse_keybind("c-j"),
            Some(crate::smelt_edit::KeyBind::new(
                KeyCode::Char('j'),
                KeyModifiers::CONTROL
            ))
        );
        assert_eq!(
            parse_keybind("a-x"),
            Some(crate::smelt_edit::KeyBind::new(
                KeyCode::Char('x'),
                KeyModifiers::ALT
            ))
        );
        // shift-tab collapses to BackTab with SHIFT removed so crossterm event matches.
        assert_eq!(
            parse_keybind("s-tab"),
            Some(crate::smelt_edit::KeyBind::new(
                KeyCode::BackTab,
                KeyModifiers::NONE
            ))
        );
        assert_eq!(
            parse_keybind("k"),
            Some(crate::smelt_edit::KeyBind::new(
                KeyCode::Char('k'),
                KeyModifiers::NONE
            ))
        );
        // Canonical bracket form also accepted.
        assert_eq!(
            parse_keybind("<Esc>"),
            Some(crate::smelt_edit::KeyBind::new(
                KeyCode::Esc,
                KeyModifiers::NONE
            ))
        );
        assert_eq!(
            parse_keybind("<C-r>"),
            Some(crate::smelt_edit::KeyBind::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL
            ))
        );
        assert_eq!(
            parse_keybind("<S-Tab>"),
            Some(crate::smelt_edit::KeyBind::new(
                KeyCode::BackTab,
                KeyModifiers::NONE
            ))
        );
        assert_eq!(parse_keybind("bogus"), None);
        assert_eq!(parse_keybind("ctrl-nope"), None);
        assert_eq!(parse_keybind(""), None);
    }

    #[test]
    fn normalize_mode_accepts_long_and_short_names() {
        assert_eq!(normalize_mode("n").as_deref(), Some("n"));
        assert_eq!(normalize_mode("normal").as_deref(), Some("n"));
        assert_eq!(normalize_mode("Normal").as_deref(), Some("n"));
        assert_eq!(normalize_mode("INSERT").as_deref(), Some("i"));
        assert_eq!(normalize_mode("visual").as_deref(), Some("v"));
        assert_eq!(normalize_mode("").as_deref(), Some(""));
        assert_eq!(normalize_mode("*").as_deref(), Some(""));
        assert_eq!(normalize_mode("any").as_deref(), Some(""));
        assert_eq!(normalize_mode("bogus"), None);
    }

    #[test]
    fn canonicalize_chord_folds_all_supported_forms() {
        assert_eq!(canonicalize_chord("c-r").as_deref(), Some("<C-r>"));
        assert_eq!(canonicalize_chord("C-r").as_deref(), Some("<C-r>"));
        assert_eq!(canonicalize_chord("<C-r>").as_deref(), Some("<C-r>"));
        assert_eq!(canonicalize_chord("<c-r>").as_deref(), Some("<C-r>"));
        assert_eq!(canonicalize_chord("enter").as_deref(), Some("<CR>"));
        assert_eq!(canonicalize_chord("<Enter>").as_deref(), Some("<CR>"));
        assert_eq!(canonicalize_chord("esc").as_deref(), Some("<Esc>"));
        assert_eq!(canonicalize_chord("s-tab").as_deref(), Some("<Tab>"));
        assert_eq!(canonicalize_chord("j").as_deref(), Some("j"));
        assert_eq!(canonicalize_chord("bogus"), None);
    }

    #[test]
    fn canonicalize_chord_sequence_expands_leader() {
        assert_eq!(
            canonicalize_chord_sequence_with_leader("<leader>r", Some(" ")).as_deref(),
            Some(" r")
        );
        assert_eq!(
            canonicalize_chord_sequence_with_leader("<Leader><C-r>", Some("\\")).as_deref(),
            Some("\\<C-r>")
        );
        assert_eq!(
            canonicalize_chord_sequence_with_leader("<leader>r", None),
            None
        );
        assert_eq!(canonicalize_leader("<space>").as_deref(), Some(" "));
        assert_eq!(display_chord_sequence(" r"), "<space>r");
        assert_eq!(canonicalize_leader("<leader>"), None);
    }

    #[test]
    fn keymap_leader_applies_to_subsequent_registrations() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.keymap.set_leader("<space>")
                    smelt.keymap.set("n", "<leader>r", function() smelt.notify.info("resume") end)
                "#,
            )
            .exec()
            .expect("exec");
        use smelt_core::lua::runtime::KeymapResult;
        assert_eq!(
            rt.run_keymap(" r", Some("Normal"), None),
            KeymapResult::Consumed
        );
        let rows: mlua::Table = rt.lua.load("return smelt.keymap.list()").eval().unwrap();
        let first: mlua::Table = rows.get(1).unwrap();
        let chord: String = first.get("chord").unwrap();
        assert_eq!(chord, "<space>r");
        let leader: String = rt.lua.load("return smelt.keymap.leader()").eval().unwrap();
        assert_eq!(leader, "<space>");
        assert_eq!(
            rt.run_keymap("\\r", Some("Normal"), None),
            KeymapResult::NoBinding
        );
    }

    #[test]
    fn keymap_leader_resets_on_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(&init, r#"smelt.keymap.set_leader("<space>")"#).unwrap();

        let mut rt = LuaRuntime::new();
        rt.set_init_lua_path(init.clone());
        rt.load_user_config();
        let leader: String = rt.lua.load("return smelt.keymap.leader()").eval().unwrap();
        assert_eq!(leader, "<space>");

        std::fs::write(&init, "").unwrap();
        let err = rt.reload(None);
        assert!(err.is_none(), "reload: {err:?}");
        let leader: String = rt.lua.load("return smelt.keymap.leader()").eval().unwrap();
        assert_eq!(leader, "\\");
    }

    #[test]
    fn keymap_list_includes_optional_description() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                    smelt.keymap.set("n", "gd", function() end, { desc = "go to definition" })
                    smelt.keymap.set("", "?", function() end)
                "#,
            )
            .exec()
            .expect("exec");

        let rows: mlua::Table = rt.lua.load("return smelt.keymap.list()").eval().unwrap();
        let mut found_desc = false;
        let mut found_without_desc = false;
        for row in rows.sequence_values::<mlua::Table>() {
            let row = row.unwrap();
            let chord: String = row.get("chord").unwrap();
            let desc: Option<String> = row.get("desc").unwrap();
            if chord == "gd" {
                assert_eq!(desc.as_deref(), Some("go to definition"));
                found_desc = true;
            }
            if chord == "?" {
                assert_eq!(desc, None);
                found_without_desc = true;
            }
        }
        assert!(found_desc);
        assert!(found_without_desc);
    }

    #[test]
    fn keymap_prefixes_returns_effective_mode_filtered_rows() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                    smelt.keymap.set_leader("<space>")
                    smelt.keymap.set("", "<leader>g", function() end, { desc = "global" })
                    smelt.keymap.set("n", "<leader>r", function() end, { desc = "run" })
                    smelt.keymap.set("i", "<leader>i", function() end, { desc = "insert" })
                "#,
            )
            .exec()
            .expect("exec");

        let rows: mlua::Table = rt
            .lua
            .load(r#"return smelt.keymap.prefixes("<space>", "normal")"#)
            .eval()
            .unwrap();
        let first: mlua::Table = rows.get(1).unwrap();
        let second: mlua::Table = rows.get(2).unwrap();
        assert_eq!(first.get::<String>("suffix").unwrap(), "g");
        assert_eq!(first.get::<String>("desc").unwrap(), "global");
        assert_eq!(second.get::<String>("suffix").unwrap(), "r");
        assert_eq!(second.get::<String>("desc").unwrap(), "run");
        assert_eq!(rows.raw_len(), 2);

        let insert_rows: mlua::Table = rt
            .lua
            .load(r#"return smelt.keymap.prefixes("<space>", "insert")"#)
            .eval()
            .unwrap();
        assert_eq!(insert_rows.raw_len(), 2);
        let insert_second: mlua::Table = insert_rows.get(2).unwrap();
        assert_eq!(insert_second.get::<String>("suffix").unwrap(), "i");
    }

    #[test]
    fn keymap_prefixes_prefers_mode_specific_over_global_same_chord() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                    smelt.keymap.set_leader("<space>")
                    smelt.keymap.set("", "<leader>r", function() end, { desc = "global" })
                    smelt.keymap.set("n", "<leader>r", function() end, { desc = "normal" })
                "#,
            )
            .exec()
            .expect("exec");

        let rows: mlua::Table = rt
            .lua
            .load(r#"return smelt.keymap.prefixes("<space>", "n")"#)
            .eval()
            .unwrap();
        assert_eq!(rows.raw_len(), 1);
        let first: mlua::Table = rows.get(1).unwrap();
        assert_eq!(first.get::<String>("chord").unwrap(), "<space>r");
        assert_eq!(first.get::<String>("suffix").unwrap(), "r");
        assert_eq!(first.get::<String>("mode").unwrap(), "n");
        assert_eq!(first.get::<String>("desc").unwrap(), "normal");
    }

    #[test]
    fn keymap_accepts_plugin_friendly_spellings() {
        // Canonicalization at registration closes the shorthand → bracket-form gap.
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    for _, mode in ipairs({ "normal", "insert", "visual" }) do
                        smelt.keymap.set(mode, "c-r", function()
                            smelt.notify.info("history: " .. mode)
                        end)
                    end
                "#,
            )
            .exec()
            .expect("exec");
        use smelt_core::lua::runtime::KeymapResult;
        assert_eq!(
            rt.run_keymap("<C-r>", Some("Normal"), None),
            KeymapResult::Consumed
        );
        assert_eq!(
            rt.run_keymap("<C-r>", Some("Insert"), None),
            KeymapResult::Consumed
        );
        assert_eq!(
            rt.run_keymap("<C-r>", Some("Visual"), None),
            KeymapResult::Consumed
        );
        let msgs = drain_notifications(&rt);
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn keymap_chord_sequence_registers_canonical_form() {
        // Dispatcher matches against canonical concatenated form.
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.keymap.set("", "<Esc><Esc>", function() smelt.notify.info("esc-esc") end)
                    smelt.keymap.set("n", "gd", function() smelt.notify.info("go-def") end)
                "#,
            )
            .exec()
            .expect("exec");
        use smelt_core::lua::runtime::KeymapResult;
        assert_eq!(
            rt.run_keymap("<Esc><Esc>", Some("Normal"), None),
            KeymapResult::Consumed
        );
        assert_eq!(
            rt.run_keymap("gd", Some("Normal"), None),
            KeymapResult::Consumed
        );
        // Single-key prefix of a multi-key chord must not fire on its own.
        assert_eq!(
            rt.run_keymap("g", Some("Normal"), None),
            KeymapResult::NoBinding
        );
        let msgs = drain_notifications(&rt);
        assert_eq!(msgs, vec!["esc-esc", "go-def"]);
    }

    #[test]
    fn chord_has_longer_detects_prefix_matches() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                    smelt.keymap.set("", "<Esc><Esc>", function() end)
                    smelt.keymap.set("n", "gd", function() end)
                "#,
            )
            .exec()
            .expect("exec");
        assert!(rt.chord_has_longer("<Esc>", Some("Normal")));
        assert!(rt.chord_has_longer("g", Some("Normal")));
        // Exact sequences are not strict prefixes (they fire via `run_keymap`).
        assert!(!rt.chord_has_longer("<Esc><Esc>", Some("Normal")));
        assert!(!rt.chord_has_longer("gd", Some("Normal")));
        assert!(!rt.chord_has_longer("j", Some("Normal")));
        // Mode-specific chord `gd` (Normal only) doesn't surface in Insert.
        assert!(!rt.chord_has_longer("g", Some("Insert")));
        // Global-mode chord surfaces in every mode.
        assert!(rt.chord_has_longer("<Esc>", Some("Insert")));
    }

    #[test]
    fn keymap_chord_handler_receives_ctx_table() {
        // Multi-key handlers receive a context table with state captured at the first key.
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.keymap.set("", "<Esc><Esc>", function(ctx)
                        smelt.notify.info("mode=" .. tostring(ctx.vim_mode_at_chord_start))
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        use smelt_core::lua::runtime::KeymapResult;
        let ctx_pairs: Vec<(&str, String)> =
            vec![("vim_mode_at_chord_start", "insert".to_string())];
        assert_eq!(
            rt.run_keymap("<Esc><Esc>", Some("Normal"), Some(ctx_pairs.as_slice())),
            KeymapResult::Consumed
        );
        let msgs = drain_notifications(&rt);
        assert_eq!(msgs, vec!["mode=insert"]);
    }

    #[test]
    fn keymap_set_errors_on_bad_input() {
        let rt = LuaRuntime::new();
        let err = rt
            .lua
            .load(r#"smelt.keymap.set("bogus", "c-r", function() end)"#)
            .exec()
            .expect_err("should error on unknown mode");
        assert!(format!("{err}").contains("unknown mode"), "err: {err}");
        let err = rt
            .lua
            .load(r#"smelt.keymap.set("n", "c-wtf", function() end)"#)
            .exec()
            .expect_err("should error on unknown chord");
        assert!(format!("{err}").contains("unknown chord"), "err: {err}");
    }

    #[test]
    fn callback_error_surfaces_without_panic() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.cmd.register("broken", function()
                        error("kaboom")
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        assert!(rt.run_command("broken", None));
        let errs = drain_errors(&rt);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("broken"), "err: {}", errs[0]);
    }
}
