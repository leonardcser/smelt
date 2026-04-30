//! Headless-safe runtime core: subsystems that are independent of the
//! terminal compositor. Lives at `TuiApp.core` and `HeadlessApp.core`;
//! `Core::new(config, engine)` is the only construction path.

use super::{
    app_config::AppConfig, cells, cells::Cells, commands, confirms::Confirms,
    engine_bridge::EngineBridge, timers::Timers,
};
use crate::lua::LuaRuntime;
use crate::session::Session;
use engine::EngineHandle;

pub struct Core {
    /// Connection + behaviour configuration: model / api_base /
    /// api_key_env / provider_type / available_models / model_config /
    /// cli_*overrides / mode / mode_cycle / reasoning_effort /
    /// reasoning_cycle / settings / multi_agent / context_window.
    /// Populated by `TuiApp::new` from CLI args + saved state, then
    /// mutated by user actions (Shift+Tab cycles `mode`, `/model`
    /// rewrites `model`, etc.).
    pub config: AppConfig,
    pub session: Session,
    /// Pending Confirm dialog requests, keyed by Lua-side handle id.
    /// `agent.rs` registers a request before firing
    /// `smelt.confirm.open(handle_id)`; the Lua dialog reads it back
    /// through Rust primitives and resolves it on submit / dismiss.
    pub(crate) confirms: Confirms,
    /// **Single global** clipboard subsystem (kill ring + platform
    /// sink). Vim and emacs yank/paste sites borrow this directly so
    /// the prompt, the transcript, dialog inputs, and any future Lua
    /// tools share one kill ring backed by the same system clipboard.
    pub clipboard: ui::Clipboard,
    /// Scheduled Lua callbacks. `smelt.timer.set` /
    /// `smelt.timer.every` / `smelt.timer.cancel` (and the
    /// `smelt.defer` alias) all route here through `with_app`.
    /// Drained each main-loop iteration via `TuiApp::tick_timers`.
    pub(crate) timers: Timers,
    /// Reactive name → value registry plus a deferred subscriber
    /// queue. Built-in cells declare here at startup; setters
    /// publish via `cells.set(name, value)` and the main loop drains
    /// queued subscribers between event handlers.
    pub(crate) cells: Cells,
    /// Lua runtime — loads `~/.config/smelt/init.lua`, dispatches
    /// user-registered commands / keymaps / autocmds.
    pub lua: LuaRuntime,
    /// Channel surface to the LLM `engine` task (`send` / `recv` /
    /// `try_recv`) plus the shared process / spawned-agent registries
    /// the agent loop drains. P2.d will fold the engine-event drain
    /// into this type.
    pub(crate) engine: EngineBridge,
}

impl Core {
    /// Build the headless-safe core from a populated `AppConfig` and a
    /// fresh `EngineHandle`. Both `TuiApp::new` (TUI) and `HeadlessApp::new`
    /// (one-shot / subagent) call this — the only single source of
    /// truth for the eight subsystem fields' construction.
    pub fn new(config: AppConfig, engine: EngineHandle) -> Self {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_default();
        let cells = cells::build_with_builtins(cells::BuiltinSeeds {
            vim_mode: format!("{:?}", ui::VimMode::Insert),
            agent_mode: config.mode.as_str().to_string(),
            model: config.model.clone(),
            reasoning: config.reasoning_effort.label().to_string(),
            cwd,
            session_title: String::new(),
            branch: String::new(),
        });
        Self {
            config,
            session: Session::new(),
            confirms: Confirms::new(),
            clipboard: ui::Clipboard::new(Box::new(commands::SystemSink)),
            timers: Timers::new(),
            cells,
            lua: LuaRuntime::new(),
            engine: EngineBridge::new(engine),
        }
    }
}
