//! Lua bindings (Phase D). Wraps the `api::*` surface so users can
//! script smelt from `~/.config/smelt/init.lua`.
//!
//! Current scope:
//! - **D1 bootstrap** — loads `~/.config/smelt/init.lua` at startup
//!   (honouring `XDG_CONFIG_HOME`). Missing files are not errors.
//! - **D2 api shim** — `smelt.version`, `smelt.cmd.register`,
//!   `smelt.keymap.set`, `smelt.au.on` all accept Lua callables and
//!   store them in per-category registries that the app polls on the
//!   tick.
//! - **D3 event dispatch** — every "autocmd-shaped" event flows
//!   through `Cells`. TuiApp publishers call `cells.set_dyn(name,
//!   payload)`; subscribers register via `smelt.au.on(name, fn)` (a
//!   thin alias over `Cells::subscribe_kind`). One observer registry,
//!   no parallel autocmd map.
//! - **D4 user-command + keymap registration** — registration stores
//!   `LuaRef` handles keyed by `(mode, chord)`; mode `"n"` matches
//!   Normal, `"i"` Insert, `"v"` Visual, `""` matches any mode.
//! - **D5 re-entrancy** — pending ops queue defers state mutations
//!   until after the dispatching handler returns. `smelt.defer(ms, fn)`
//!   posts to `pending_timers`; the tick loop fires them when due.
//! - **D6 error UX** — every callable is wrapped in `try_call`;
//!   errors append to `lua_errors` and the app surfaces the first as a
//!   notification on the next tick.

#![allow(clippy::arc_with_non_send_sync)]

mod api;
pub(crate) mod app_ref;
mod tasks;
pub(crate) mod ui_ops;

pub(crate) use app_ref::{
    install_app_ptr, try_with_app, try_with_host, try_with_ui_host, with_app, with_ui_host,
};

pub(crate) use smelt_core::lua::{LuaHandle, TaskDriveOutput, ToolEnv, ToolExecResult};

pub(crate) use smelt_core::lua::StatusSource;

use mlua::prelude::*;

use std::sync::{Arc, Mutex};

/// One Lua-registered `/command` entry. Lives in `LuaShared.commands`
/// so completers (`list_commands`, `is_lua_command`) read the same
/// List all Lua-registered `/commands` as `(name, description)`.
/// Sorted by name. Used by the `/` completer. Reads live via
/// `try_with_app`; returns empty when no app pointer is installed
/// (e.g. early startup).
pub(crate) fn list_commands() -> Vec<(String, Option<String>)> {
    try_with_app(|app| app.lua.list_commands_with_desc()).unwrap_or_default()
}

/// True if `input` (e.g. `/pick-test` or `/pick-test arg`) matches a
/// Lua-registered command name.
pub(crate) fn is_lua_command(input: &str) -> bool {
    let name = input
        .strip_prefix('/')
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("");
    if name.is_empty() {
        return false;
    }
    try_with_app(|app| app.lua.has_command(name)).unwrap_or(false)
}

/// Format a `crossterm::KeyEvent` into an nvim-style chord string
/// (`<C-g>`, `<S-Tab>`, `<M-x>`, printable `j`, etc). Unrecognized
/// chords return `None` so the dispatcher falls through to the normal
/// handlers. This is the lookup key for `smelt.keymap.set(_, chord, fn)`.
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

/// Parse a plugin-facing key spec like `"enter"`, `"esc"`, `"tab"`,
/// `"bs"`, `"space"`, `"up"`, `"c-j"` (ctrl-j), `"a-x"` / `"m-x"`
/// (alt-x), `"s-tab"` (shift-tab), or a single printable char into a
/// [`crate::ui::KeyBind`]. Modifiers separate with `-`; the final token is
/// the key name. Case-insensitive for names and modifiers. Returns
/// `None` for unknown keys — the caller surfaces a Lua error.
pub(crate) fn parse_keybind(spec: &str) -> Option<crate::ui::KeyBind> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let raw = spec.trim();
    if raw.is_empty() {
        return None;
    }
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
                return Some(crate::ui::KeyBind::new(
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
        s if s.chars().count() == 1 => KeyCode::Char(name.chars().next().unwrap()),
        _ => return None,
    };
    Some(crate::ui::KeyBind::new(code, mods))
}

/// Normalize a mode string from a Lua plugin into the canonical
/// single-char form the dispatcher stores (`"n" | "i" | "v" | ""`).
/// Accepts long names (`"normal"`, `"insert"`, `"visual"`), short
/// names (`"n"`, `"i"`, `"v"`), the empty string (mode-independent),
/// or `"any" / "*"` as aliases for "". Case-insensitive. Returns
/// `None` on unknown input so the caller surfaces a Lua error.
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

/// Canonicalize a Lua-supplied chord string into the nvim-angle-bracket
/// form that `chord_string` emits from key events. Accepts plain Lua
/// shorthand (`"c-r"`, `"s-tab"`, `"enter"`) *and* already-canonical
/// (`"<C-r>"`, `"<S-Tab>"`) input. Plain printable chars stay plain
/// (`"j"`). Returns `None` on unknown keys so the caller raises a Lua
/// error at registration.
pub(crate) fn canonicalize_chord(chord: &str) -> Option<String> {
    use crossterm::event::KeyEvent;
    let stripped = chord
        .trim()
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(chord.trim());
    let kb = parse_keybind(stripped)?;
    chord_string(KeyEvent::new(kb.code, kb.mods))
}

/// Parse a Lua-facing window-event name into a [`crate::ui::WinEvent`]. Names
/// match the Neovim-adjacent naming Lua plugins use for autocmd-style
/// hooks. Returns `None` for unknown names so the caller surfaces a
/// Lua error.
pub(crate) fn parse_win_event(name: &str) -> Option<crate::ui::WinEvent> {
    Some(match name {
        "open" => crate::ui::WinEvent::Open,
        "close" => crate::ui::WinEvent::Close,
        "focus" | "focus_gained" => crate::ui::WinEvent::FocusGained,
        "blur" | "focus_lost" => crate::ui::WinEvent::FocusLost,
        "selection_changed" | "select_changed" => crate::ui::WinEvent::SelectionChanged,
        "submit" => crate::ui::WinEvent::Submit,
        "text_changed" | "change" => crate::ui::WinEvent::TextChanged,
        "dismiss" | "cancel" => crate::ui::WinEvent::Dismiss,
        "tick" => crate::ui::WinEvent::Tick,
        _ => return None,
    })
}

/// A Lua callable registered via `smelt.cmd.register` / `smelt.keymap` /
/// `smelt.on`. Stored as a mlua `RegistryKey` so references survive
/// Stash a Lua callable in `shared.callbacks` under a fresh u64 id.
/// Used by every `smelt.win.*` binding that takes a callback — pulls
/// the registry-value + atomic-id + insert dance out of the bindings.
pub(crate) fn register_callback_handle(
    shared: &Arc<LuaShared>,
    lua: &Lua,
    func: mlua::Function,
) -> mlua::Result<u64> {
    let key = lua.create_registry_value(func)?;
    let id = shared
        .next_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut cbs) = shared.callbacks.lock() {
        cbs.insert(id, smelt_core::lua::LuaHandle { key });
    }
    Ok(id)
}

/// Drop the Lua handle id stashed in a displaced `Callback::Lua`, if
/// the option is one. Used wherever a `win_set_keymap` / `win_clear_*`
/// returns the callback that was just replaced or removed.
pub(crate) fn drop_displaced_lua_handle(
    app: &mut crate::app::TuiApp,
    displaced: Option<crate::ui::Callback>,
) {
    if let Some(crate::ui::Callback::Lua(crate::ui::LuaHandle(old))) = displaced {
        app.lua.remove_callback(old);
    }
}

/// A callback invocation recorded by the ui dispatch path while
/// `&mut Ui` is held. Drained by the host TuiApp between ui calls so each
/// Lua fn body runs with the TLS app pointer installed.
pub(crate) struct PendingInvocation {
    pub(crate) handle: crate::ui::LuaHandle,
    pub(crate) win: crate::ui::WinId,
    pub(crate) payload: crate::ui::Payload,
}

/// TUI-specific extension of [`smelt_core::lua::LuaShared`] that adds
/// the `pending_invocations` queue. `Deref`s to the core shared state
/// so existing `self.shared.commands.lock()`-style call sites keep
/// working via autoderef on method calls.
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
    /// Clone the inner `Arc<smelt_core::lua::LuaShared>` so core-side
    /// API modules (which take `Arc<core::LuaShared>`) can capture it
    /// in `'static` Lua closures.
    pub(crate) fn core_arc(&self) -> Arc<smelt_core::lua::LuaShared> {
        Arc::clone(&self.core)
    }
}

/// User-scoped Lua state + any recorded startup error.
/// Wraps [`smelt_core::lua::LuaRuntime`] and adds TUI-specific
/// callback queues and statusline rendering.
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
    ///
    /// *Does not* load plugins or `init.lua` — call
    /// [`LuaRuntime::load_plugins`] after pushing startup snapshots
    /// (available models, settings, history) so plugins that read those
    /// at registration time (e.g. `/model` declaring `args = model_keys`)
    /// see real data.
    pub fn new() -> Self {
        let shared = Arc::new(LuaShared::default());
        let mut core = smelt_core::lua::LuaRuntime::with_shared(shared.core_arc());

        if core.load_error.is_none() {
            if let Err(e) = Self::register_api(&core.lua, &shared) {
                core.load_error = Some(e.to_string());
            }
        }

        Self { core, shared }
    }

    /// Borrow the shared state. Used to clone `Arc<LuaShared>` into
    /// tokio tasks (e.g. streaming subprocess spawn) that need to
    /// post `TaskEvent::ExternalResolvedJson` from outside the main
    /// thread.
    pub(crate) fn shared(&self) -> &Arc<LuaShared> {
        &self.shared
    }

    /// Access the underlying `mlua::Lua` state.
    pub(crate) fn lua(&self) -> &Lua {
        &self.core.lua
    }

    /// Take the load error, if any.
    pub(crate) fn take_load_error(&mut self) -> Option<String> {
        self.core.load_error.take()
    }

    /// Run autoload plugins and `~/.config/smelt/init.lua`. Call
    /// *after* pushing startup snapshots so plugins see populated
    /// `smelt.engine.models()` etc.
    pub(crate) fn load_plugins(&mut self) {
        self.core.load_user_config();
        self.load_autoload();
    }

    /// Run embedded autoload plugins only. Call *after* installing
    /// the TLS app pointer so plugins that read `with_app` work.
    pub(crate) fn load_autoload(&mut self) {
        if self.core.load_error.is_some() {
            return;
        }
        for &name in AUTOLOAD_MODULES {
            let code = format!("require('{name}')");
            if let Err(e) = self.core.lua.load(&code).set_name(name).exec() {
                self.core.load_error = Some(format!("autoload {name}: {e}"));
                return;
            }
        }
    }

    /// Call every registered statusline source and return the combined
    /// item list (appended to Rust-side built-ins at the status-bar
    /// layer). Each source returns either a single item table or a list
    /// of items; empty-text items are skipped.
    ///
    /// The second tuple element is `(source_name, error_msg_or_none)`
    /// for every source, ordered as registered. The caller dedupes
    /// against its own per-source error history so a perpetually-broken
    /// source doesn't spam one toast per frame.
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

    /// Fire `smelt.confirm.open(handle_id)`. Called by the agent loop
    /// when an LLM tool call needs user approval — `agent.rs` registers
    /// the request via `TuiApp::confirms.register` first, then this
    /// method hands the handle to the Lua dialog runner.
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

/// Parse a single-item or list-of-items Lua table into `StatusItem`s
/// and append them to `out`. Empty-text items are skipped.
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
    // Per-item `align_right` wins over the source-level default; falls
    // back to the source's `align` opt when the item omits the field.
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

/// Plugins that must always be active (the user can't opt out via
/// init.lua). These are former Rust built-ins migrated to Lua. Required
/// after the embedded searcher is set up, before user init.lua runs.
const AUTOLOAD_MODULES: &[&str] = &[
    "smelt.tools.ask_user_question",
    "smelt.commands.btw",
    "smelt.commands.export",
    "smelt.dialogs.rewind",
    "smelt.commands.help",
    "smelt.dialogs.permissions",
    "smelt.dialogs.resume",
    "smelt.commands.theme",
    "smelt.commands.color",
    "smelt.commands.model",
    "smelt.commands.settings",
    "smelt.commands.history_search",
    "smelt.commands.toggles",
    "smelt.commands.stats",
    "smelt.commands.session",
    "smelt.commands.quit",
    "smelt.commands.compact",
    "smelt.commands.reflect",
    "smelt.commands.simplify",
    "smelt.commands.custom_commands",
    "smelt.tools.glob",
    "smelt.tools.grep",
    "smelt.tools.load_skill",
    "smelt.tools.web_search",
    "smelt.tools.write_file",
    "smelt.tools.edit_file",
    "smelt.tools.read_file",
    "smelt.tools.notebook_edit",
    "smelt.tools.web_fetch",
    "smelt.tools.bash",
    "smelt.plugins.background_commands",
];

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::lua::api::lua_table_to_json;

    /// Install a Lua-level `smelt.notify` / `smelt.notify_error` stub
    /// that pushes into `_G.test_log` instead of routing through `TuiApp`
    /// (no TuiApp exists in unit tests). Tests that observe handler
    /// behaviour through these calls should call this once at the
    /// start, then read [`drain_notifications`] / [`drain_errors`].
    fn install_test_notify(rt: &LuaRuntime) {
        rt.lua
            .load(
                r#"
                    _G.test_log = {}
                    _G.test_err = {}
                    smelt.notify = function(msg) table.insert(_G.test_log, msg) end
                    smelt.notify_error = function(msg) table.insert(_G.test_err, msg) end
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
            crate::ui::LuaHandle(id),
            crate::ui::WinId(0),
            &crate::ui::Payload::Selection { index: 2 },
        );
        let recorded: u64 = rt.lua.load("return _G.recorded").eval().unwrap();
        // Payload is 0-indexed; Lua gets 1-based.
        assert_eq!(recorded, 3);
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
            crate::ui::LuaHandle(id),
            crate::ui::WinId(0),
            &crate::ui::Payload::Text {
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
            crate::ui::LuaHandle(9999),
            crate::ui::WinId(0),
            &crate::ui::Payload::None,
        );
    }

    /// Regression: every code path that drops a Lua-backed callback
    /// (window close, displaced keymap binding) must funnel through
    /// `remove_callback` to evict the entry from `shared.callbacks`,
    /// otherwise the registry grows unbounded over a long session. Tests
    /// the floor invariant: register inserts, remove evicts, and a
    /// removed handle is a no-op when invoked.
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

        // Invoking the dropped handle must not resurrect the call.
        rt.invoke_callback(
            crate::ui::LuaHandle(id),
            crate::ui::WinId(0),
            &crate::ui::Payload::None,
        );
        let fired: u64 = rt.lua.load("return _G.fired").eval().unwrap();
        assert_eq!(fired, 0);
    }

    #[test]
    fn parse_win_event_covers_common_names() {
        assert!(matches!(
            parse_win_event("submit"),
            Some(crate::ui::WinEvent::Submit)
        ));
        assert!(matches!(
            parse_win_event("text_changed"),
            Some(crate::ui::WinEvent::TextChanged)
        ));
        assert!(matches!(
            parse_win_event("change"),
            Some(crate::ui::WinEvent::TextChanged)
        ));
        assert!(matches!(
            parse_win_event("dismiss"),
            Some(crate::ui::WinEvent::Dismiss)
        ));
        assert!(matches!(
            parse_win_event("tick"),
            Some(crate::ui::WinEvent::Tick)
        ));
        assert!(matches!(
            parse_win_event("focus"),
            Some(crate::ui::WinEvent::FocusGained)
        ));
        assert!(parse_win_event("bogus").is_none());
    }

    // Theme bindings (`smelt.theme.set/get/accent/snapshot`) cross the
    // `with_app` boundary — they read/write through `TuiApp.ui.theme()`.
    // The Lua-side wiring is exercised by integration scenarios; here
    // the role-mapping and error logic is covered directly in
    // `lua::api::tests` against a local `crate::ui::Theme`.

    #[test]
    fn runtime_exposes_api_version() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let version: String = rt.lua.load("return smelt.version").eval().expect("eval");
        assert_eq!(version, crate::api::VERSION);
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
        rt.load_plugins();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("export"),
            "/export should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn autoload_registers_ps_command() {
        let mut rt = LuaRuntime::new();
        rt.load_plugins();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("ps"),
            "/ps should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn autoload_registers_rewind_command() {
        let mut rt = LuaRuntime::new();
        rt.load_plugins();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("rewind"),
            "/rewind should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn autoload_registers_ask_user_question_as_sequential() {
        let mut rt = LuaRuntime::new();
        rt.load_plugins();
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
        assert_eq!(rt.tool_summary("echo_summary", &args), "lua:ok");
    }

    #[test]
    fn dialog_open_outside_task_errors() {
        // Calling `smelt.ui.dialog.open` outside a yieldable coroutine
        // (the runtime file's first guard) must raise. With plugins
        // loaded the Lua wrapper is in place; `isyieldable()` is false
        // at the top level, so the call errors before reaching the
        // Rust `_open` binding.
        let mut rt = LuaRuntime::new();
        rt.load_plugins();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let res: LuaResult<()> = rt.lua.load("smelt.ui.dialog.open({panels = {}})").exec();
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
        match rt.execute_tool("echo", &args, 1, "c1", test_env()) {
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
        match rt.execute_tool("wait_then_yes", &args, 7, "c9", test_env()) {
            ToolExecResult::Pending => {}
            ToolExecResult::Immediate { .. } => panic!("expected pending after yield"),
        }
        // Drive again — the sleep(0) is elapsed, so the task resumes and completes.
        let outs = rt.drive_tasks();
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
        assert!(rt.run_keymap("<C-g>", Some("Normal")));
        assert_eq!(drain_notifications(&rt), vec!["ctrl-g".to_string()]);
        assert!(!rt.run_keymap("<C-g>", Some("Insert")));
        assert!(!rt.run_keymap("<C-x>", Some("Normal")));
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
        assert!(rt.run_keymap("<C-h>", Some("Normal")));
        assert_eq!(drain_notifications(&rt), vec!["any-mode".to_string()]);
        assert!(rt.run_keymap("<C-h>", Some("Insert")));
        assert!(rt.run_keymap("<C-h>", None));
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
            Some(crate::ui::KeyBind::new(KeyCode::Enter, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_keybind("esc"),
            Some(crate::ui::KeyBind::new(KeyCode::Esc, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_keybind("c-j"),
            Some(crate::ui::KeyBind::new(
                KeyCode::Char('j'),
                KeyModifiers::CONTROL
            ))
        );
        assert_eq!(
            parse_keybind("a-x"),
            Some(crate::ui::KeyBind::new(
                KeyCode::Char('x'),
                KeyModifiers::ALT
            ))
        );
        // shift-tab collapses to BackTab without the SHIFT bit so
        // crossterm's event matches lookups done elsewhere.
        assert_eq!(
            parse_keybind("s-tab"),
            Some(crate::ui::KeyBind::new(
                KeyCode::BackTab,
                KeyModifiers::NONE
            ))
        );
        assert_eq!(
            parse_keybind("k"),
            Some(crate::ui::KeyBind::new(
                KeyCode::Char('k'),
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
        // The Ctrl-R class of bug: history_search.lua registers
        // `"normal" + "c-r"` but dispatch uses `"n" + "<C-r>"`.
        // Canonicalization at registration closes the gap.
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
        assert!(rt.run_keymap("<C-r>", Some("Normal")));
        assert!(rt.run_keymap("<C-r>", Some("Insert")));
        assert!(rt.run_keymap("<C-r>", Some("Visual")));
        let msgs = drain_notifications(&rt);
        assert_eq!(msgs.len(), 3);
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
