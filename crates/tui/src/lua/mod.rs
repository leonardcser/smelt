//! Lua bindings. Wraps the `api::*` surface so users can script smelt
//! from `~/.config/smelt/init.lua`.

#![allow(clippy::arc_with_non_send_sync)]

mod api;
pub(crate) use api::vim::LuaVimMode;
pub mod app_ref;
pub(crate) mod paint;
pub(crate) mod parse;
mod tasks;
pub(crate) mod ui_ops;

pub use app_ref::try_with_app;
pub(crate) use app_ref::{install_app_ptr, try_with_core, with_app, with_app_ptr};

pub(crate) use smelt_core::lua::{LuaHandle, TaskDriveOutput, ToolEnv, ToolExecResult};

pub(crate) use smelt_core::lua::StatusSource;

use mlua::prelude::*;

use std::sync::{Arc, Mutex};

/// List all Lua-registered `/commands` as `(name, description)`.
/// Sorted by name. Returns empty when no app pointer is installed.
pub(crate) fn list_commands() -> Vec<(String, Option<String>)> {
    try_with_app(|app| app.lua.list_commands_with_desc()).unwrap_or_default()
}

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
        // Plain printable char — no angle-bracket wrap.
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

/// Parse a plugin key spec into a [`crate::smelt_term::KeyBind`].
/// Accepts shorthand (`"c-j"`, `"s-tab"`, `"enter"`) and canonical bracket form
/// (`"<C-r>"`, `"<S-Tab>"`). Modifiers separate with `-`; case-insensitive.
/// Returns `None` for unknown keys.
pub(crate) fn parse_keybind(spec: &str) -> Option<crate::smelt_term::KeyBind> {
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
                return Some(crate::smelt_term::KeyBind::new(
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
    Some(crate::smelt_term::KeyBind::new(code, mods))
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
            "v" | "visual" => "v",
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

/// Canonicalize a chord sequence (one or more tokens: `<...>` or single printable chars).
/// Returns the canonical joined form (e.g. `"<Esc><Esc>"`, `"gd"`) or `None` if any
/// token is unknown. Single-token shorthand is tried first before sequence tokenization.
pub(crate) fn canonicalize_chord_sequence(input: &str) -> Option<String> {
    if let Some(single) = canonicalize_chord(input) {
        return Some(single);
    }
    let tokens = tokenize_chord_spec(input)?;
    let mut out = String::new();
    for tok in tokens {
        let canonical = canonicalize_chord(&tok)?;
        out.push_str(&canonical);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
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
    displaced: Option<crate::smelt_term::Callback>,
) {
    if let Some(crate::smelt_term::Callback::Lua(crate::smelt_term::LuaHandle(old))) = displaced {
        app.lua.remove_callback(old);
    }
}

/// Callback invocation queued while `&mut Ui` is held.
/// Drained by TuiApp after the ui call returns, with the TLS app pointer installed.
pub(crate) struct PendingInvocation {
    pub(crate) handle: crate::smelt_term::LuaHandle,
    pub(crate) win: crate::smelt_term::WinId,
    pub(crate) payload: crate::smelt_term::Payload,
}

/// TUI-specific extension of [`smelt_core::lua::LuaShared`] adding the
/// `pending_invocations` queue. `Deref`s to the core shared state.
pub(crate) struct LuaShared {
    pub(crate) core: Arc<smelt_core::lua::LuaShared>,
    pub(crate) pending_invocations: Mutex<Vec<PendingInvocation>>,
}

impl Default for LuaShared {
    fn default() -> Self {
        Self {
            core: Arc::new(smelt_core::lua::LuaShared::default()),
            pending_invocations: Mutex::new(Vec::new()),
        }
    }
}

impl std::ops::Deref for LuaShared {
    type Target = smelt_core::lua::LuaShared;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl LuaShared {
    /// Clone the inner `Arc<smelt_core::lua::LuaShared>` for core API modules.
    pub(crate) fn core_arc(&self) -> Arc<smelt_core::lua::LuaShared> {
        Arc::clone(&self.core)
    }
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
    /// Does not load `init.lua` — call [`LuaRuntime::load_autoload`] after
    /// startup snapshots are available so plugins see real data at registration time.
    pub fn new() -> Self {
        let shared = Arc::new(LuaShared::default());
        let mut core = smelt_core::lua::LuaRuntime::with_shared(shared.core_arc());

        if core.load_error.is_none() {
            if let Err(e) = Self::register_api(&core.lua, &shared) {
                core.load_error = Some(e.to_string());
            }
        }
        if core.load_error.is_none() {
            if let Err(e) = smelt_core::lua::runtime::load_bootstrap_chunks(&core.lua) {
                core.load_error = Some(e.to_string());
            }
        }

        Self { core, shared }
    }

    /// Register the full API surface (host + UiHost) without loading
    /// bootstrap chunks or spinning up the full runtime state.
    /// Used by `gen-lua-docs` to harvest the doc registry.
    pub fn register_for_docs() -> mlua::Result<()> {
        let shared = Arc::new(LuaShared::default());
        let core = smelt_core::lua::LuaRuntime::with_shared(shared.core_arc());
        Self::register_api(&core.lua, &shared)
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
    pub fn drain_shutdown_hooks(&mut self, session_id: &str, has_messages: bool) -> Vec<String> {
        self.core.drain_shutdown_hooks(session_id, has_messages)
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

    /// Call every registered statusline source, returning combined items and per-source errors.
    /// Each source returns a single item or a list; empty-text items are skipped.
    /// The second tuple element is `(source_name, error_or_none)` per source.
    pub(crate) fn tick_statusline(
        &self,
    ) -> (
        Vec<crate::content::status::StatusItem>,
        Vec<(String, Option<String>)>,
    ) {
        let Ok(sources) = self.shared.statusline_sources.lock() else {
            return (Vec::new(), Vec::new());
        };
        let mut items = Vec::new();
        let mut tick_errors: Vec<(String, Option<String>)> = Vec::new();
        for (name, source) in sources.iter() {
            let Ok(func) = self
                .core
                .lua
                .registry_value::<mlua::Function>(&source.handle.key)
            else {
                continue;
            };
            let _perf = smelt_perf::perf::begin("lua:statusline");
            match func.call::<mlua::Value>(()) {
                Ok(mlua::Value::Nil) => {
                    tick_errors.push((name.clone(), None));
                }
                Ok(mlua::Value::Table(t)) => {
                    collect_statusline_items(&t, source.default_align_right, &mut items);
                    tick_errors.push((name.clone(), None));
                }
                Ok(_) => {
                    tick_errors.push((
                        name.clone(),
                        Some(format!("statusline[{name}]: expected table")),
                    ));
                }
                Err(e) => {
                    tick_errors.push((name.clone(), Some(format!("statusline[{name}]: {e}"))));
                }
            }
        }
        (items, tick_errors)
    }

    /// Fire the `smelt.engine.ask` callback registered under `id` with
    /// `(content, err_or_nil)`. The error table mirrors the
    /// `smelt.engine.AskError` shape — `{ kind, message }` strings —
    /// so plugins can branch on the failure mode without parsing text.
    pub(crate) fn fire_ask_callback(
        &self,
        id: u64,
        content: &str,
        error: Option<protocol::EngineAskError>,
    ) {
        let handle = {
            let Ok(mut cbs) = self.shared.callbacks.lock() else {
                return;
            };
            match cbs.remove(&id) {
                Some(h) => h,
                None => return,
            }
        };
        let Ok(func) = self.core.lua.registry_value::<mlua::Function>(&handle.key) else {
            return;
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
        if let Err(e) = func.call::<()>((content.to_string(), err_value)) {
            self.record_error(format!("ask callback: {e}"));
        }
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

fn ansi_color_from_lua(table: &mlua::Table, key: &str) -> Option<smelt_core::style::Color> {
    let val: u8 = table.get(key).ok()?;
    Some(smelt_core::style::Color::AnsiValue(val))
}

/// Parse a single-item or list-of-items Lua table into `StatusItem`s, appending to `out`.
fn collect_statusline_items(
    table: &mlua::Table,
    default_align_right: bool,
    out: &mut Vec<crate::content::status::StatusItem>,
) {
    let looks_like_item = table.contains_key("text").unwrap_or(false);
    if looks_like_item {
        if let Some(item) = statusline_item_from(table, default_align_right) {
            out.push(item);
        }
        return;
    }
    for pair in table.sequence_values::<mlua::Table>() {
        let Ok(entry) = pair else { continue };
        if let Some(item) = statusline_item_from(&entry, default_align_right) {
            out.push(item);
        }
    }
}

fn statusline_item_from(
    entry: &mlua::Table,
    default_align_right: bool,
) -> Option<crate::content::status::StatusItem> {
    let text: String = entry.get("text").ok()?;
    if text.is_empty() {
        return None;
    }
    // Per-item `align_right` wins over source-level default.
    let align_right = if entry.contains_key("align_right").unwrap_or(false) {
        entry.get("align_right").unwrap_or(default_align_right)
    } else {
        default_align_right
    };
    Some(crate::content::status::StatusItem {
        text,
        fg: ansi_color_from_lua(entry, "fg"),
        bg: ansi_color_from_lua(entry, "bg"),
        bold: entry.get("bold").unwrap_or(false),
        priority: entry.get("priority").unwrap_or(0),
        align_right,
        truncatable: entry.get("truncatable").unwrap_or(false),
        group: entry.get("group").unwrap_or(false),
    })
}

impl Default for LuaRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::lua::api::lua_table_to_json;

    /// Stub `smelt.notify` / `smelt.notify.error` to push into `_G.test_log` / `_G.test_err`.
    fn install_test_notify(rt: &LuaRuntime) {
        rt.lua
            .load(
                r#"
                    _G.test_log = {}
                    _G.test_err = {}
                    local mt = getmetatable(smelt.notify) or {}
                    mt.__call = function(_, msg) table.insert(_G.test_log, msg) end
                    setmetatable(smelt.notify, mt)
                    smelt.notify.error = function(msg) table.insert(_G.test_err, msg) end
                "#,
            )
            .exec()
            .expect("install_test_notify");
    }

    fn test_env() -> ToolEnv<'static> {
        static EMPTY_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let p = EMPTY_PATH.get_or_init(std::path::PathBuf::new);
        ToolEnv {
            mode: protocol::AgentMode::Apply,
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
            crate::smelt_term::LuaHandle(id),
            crate::smelt_term::WinId(0),
            &crate::smelt_term::Payload::Selection { index: 2 },
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
            crate::smelt_term::LuaHandle(id),
            crate::smelt_term::WinId(0),
            &crate::smelt_term::Payload::Text {
                content: "hi".into(),
            },
        );
        let t: String = rt.lua.load("return _G.t").eval().unwrap();
        assert_eq!(t, "hi");
    }

    #[test]
    fn invoke_callback_unknown_handle_is_noop() {
        let rt = LuaRuntime::new();
        // Nothing registered under id 9999 — should silently succeed.
        rt.invoke_callback(
            crate::smelt_term::LuaHandle(9999),
            crate::smelt_term::WinId(0),
            &crate::smelt_term::Payload::None,
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
            crate::smelt_term::LuaHandle(id),
            crate::smelt_term::WinId(0),
            &crate::smelt_term::Payload::None,
        );
        let fired: u64 = rt.lua.load("return _G.fired").eval().unwrap();
        assert_eq!(fired, 0);
    }

    // Theme role-mapping and error logic are tested in `lua::api::tests`.

    #[test]
    fn runtime_exposes_api_version_and_app_version() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let api: String = rt
            .lua
            .load("return smelt.api_version")
            .eval()
            .expect("eval");
        assert_eq!(api, crate::lua::api::API_VERSION);
        let app: String = rt.lua.load("return smelt.version").eval().expect("eval");
        assert_eq!(app, crate::lua::api::APP_VERSION);
        assert!(
            !app.is_empty() && app.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "smelt.version should be the program version, got {app:?}"
        );
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
    fn autoload_registers_export_command() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("export"),
            "/export should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn background_commands_plugin_is_opt_in() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            !rt.has_command("ps"),
            "/ps must not be registered until the background_commands plugin is required"
        );
        rt.lua()
            .load(r#"require("smelt.plugins.background_commands")"#)
            .exec()
            .expect("require background_commands");
        assert!(
            rt.has_command("ps"),
            "/ps should be registered once background_commands is loaded"
        );
    }

    #[test]
    fn autoload_registers_rewind_command() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("rewind"),
            "/rewind should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn autoload_registers_ask_user_question_as_sequential() {
        let mut rt = LuaRuntime::new();
        rt.load_autoload();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let defs = rt.tool_defs(protocol::AgentMode::Normal);
        let ask = defs
            .iter()
            .find(|d| d.name == "ask_user_question")
            .expect("ask_user_question should be auto-registered");
        assert_eq!(ask.execution_mode, protocol::ToolExecutionMode::Sequential);
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
            ToolExecResult::Immediate { content, is_error } => {
                assert_eq!(content, "hi world");
                assert!(!is_error);
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
                request_id,
                call_id,
                content,
                is_error,
            } => {
                assert_eq!(*request_id, 7);
                assert_eq!(call_id, "c9");
                assert_eq!(content, "yes");
                assert!(!*is_error);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn notify_queues_for_drain() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load("smelt.notify('hello from lua')")
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
                        smelt.notify("hello world")
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
    fn keymap_register_and_run() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.keymap.set("n", "<C-g>", function()
                        smelt.notify("ctrl-g")
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
        // End-to-end reload across core (cmd) + TUI (keymap, statusline)
        // registries. Catches the case where a new surface is added to
        // `LuaShared` and someone forgets to extend `clear_lua_handles`.
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(
            &init,
            r#"
                smelt.cmd.register("plug_cmd", function() end)
                smelt.keymap.set("n", "<C-g>", function() end)
                smelt.statusline.register("plug_src", function() return {} end)
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
            k.keys().any(|(_, c)| c == "<C-g>")
        };
        assert!(has_user_chord(&shared.keymaps.lock().unwrap()));
        assert!(shared
            .statusline_sources
            .lock()
            .unwrap()
            .iter()
            .any(|(n, _)| n == "plug_src"));

        // Reload to an empty body: the user-registered command, keymap, and
        // statusline source must disappear. Autoload-registered keymaps
        // (e.g. F5/reload, F12/perf_panel) come back, so we only assert the
        // user chord is gone.
        std::fs::write(&init, "").unwrap();
        let err = rt.reload(None);
        assert!(err.is_none(), "reload: {err:?}");

        assert!(!shared.commands.lock().unwrap().contains_key("plug_cmd"));
        assert!(!has_user_chord(&shared.keymaps.lock().unwrap()));
        assert!(!shared
            .statusline_sources
            .lock()
            .unwrap()
            .iter()
            .any(|(n, _)| n == "plug_src"));
    }

    #[test]
    fn keymap_wildcard_mode() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.keymap.set("", "<C-h>", function()
                        smelt.notify("any-mode")
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
            Some(crate::smelt_term::KeyBind::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))
        );
        assert_eq!(
            parse_keybind("esc"),
            Some(crate::smelt_term::KeyBind::new(
                KeyCode::Esc,
                KeyModifiers::NONE
            ))
        );
        assert_eq!(
            parse_keybind("c-j"),
            Some(crate::smelt_term::KeyBind::new(
                KeyCode::Char('j'),
                KeyModifiers::CONTROL
            ))
        );
        assert_eq!(
            parse_keybind("a-x"),
            Some(crate::smelt_term::KeyBind::new(
                KeyCode::Char('x'),
                KeyModifiers::ALT
            ))
        );
        // shift-tab collapses to BackTab with SHIFT removed so crossterm event matches.
        assert_eq!(
            parse_keybind("s-tab"),
            Some(crate::smelt_term::KeyBind::new(
                KeyCode::BackTab,
                KeyModifiers::NONE
            ))
        );
        assert_eq!(
            parse_keybind("k"),
            Some(crate::smelt_term::KeyBind::new(
                KeyCode::Char('k'),
                KeyModifiers::NONE
            ))
        );
        // Canonical bracket form also accepted.
        assert_eq!(
            parse_keybind("<Esc>"),
            Some(crate::smelt_term::KeyBind::new(
                KeyCode::Esc,
                KeyModifiers::NONE
            ))
        );
        assert_eq!(
            parse_keybind("<C-r>"),
            Some(crate::smelt_term::KeyBind::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL
            ))
        );
        assert_eq!(
            parse_keybind("<S-Tab>"),
            Some(crate::smelt_term::KeyBind::new(
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
    fn keymap_accepts_plugin_friendly_spellings() {
        // Canonicalization at registration closes the shorthand → bracket-form gap.
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    for _, mode in ipairs({ "normal", "insert", "visual" }) do
                        smelt.keymap.set(mode, "c-r", function()
                            smelt.notify("history: " .. mode)
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
                    smelt.keymap.set("", "<Esc><Esc>", function() smelt.notify("esc-esc") end)
                    smelt.keymap.set("n", "gd", function() smelt.notify("go-def") end)
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
                        smelt.notify("mode=" .. tostring(ctx.vim_mode_at_chord_start))
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
