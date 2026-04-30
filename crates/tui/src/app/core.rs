//! Headless-safe runtime core: subsystems that are independent of the
//! terminal compositor. Lives behind `App.core` until the
//! `TuiApp` / `HeadlessApp` split (a.12b) wraps it directly.

use super::{app_config::AppConfig, cells::Cells, confirms::Confirms, engine_bridge::EngineBridge};
use crate::app::timers::Timers;
use crate::lua::LuaRuntime;
use crate::session::Session;

pub struct Core {
    /// Connection + behaviour configuration: model / api_base /
    /// api_key_env / provider_type / available_models / model_config /
    /// cli_*overrides / mode / mode_cycle / reasoning_effort /
    /// reasoning_cycle / settings / multi_agent / context_window.
    /// Populated by `App::new` from CLI args + saved state, then
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
    /// Drained each main-loop iteration via `App::tick_timers`.
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
