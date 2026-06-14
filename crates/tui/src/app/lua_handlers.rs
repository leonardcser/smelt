//! Operations called by Lua bindings through `with_app`.

use crate::app::TuiApp;
use smelt_core::transcript_model::ConfirmChoice;

impl TuiApp {
    /// Run a slash command as if typed into the cmdline. Forwards `Exec` for shell escapes.
    /// Accepts bare names (`"btw foo"`) - the leading `/` is added automatically so the
    /// dispatcher's sigil requirement applies only to user-typed prompt input.
    pub(crate) fn apply_lua_command(&mut self, line: &str) {
        let trimmed = line.trim_start();
        let normalized: std::borrow::Cow<str> =
            if trimmed.starts_with('/') || trimmed.starts_with(':') || trimmed.starts_with('!') {
                std::borrow::Cow::Borrowed(line)
            } else {
                std::borrow::Cow::Owned(format!("/{trimmed}"))
            };
        match crate::commands::run_command(self, &normalized) {
            crate::app::CommandAction::Exec(handle) => {
                self.exec = Some(handle);
            }
            crate::app::CommandAction::Continue => {}
        }
    }

    /// `/reload` entry point. Wraps [`Self::bring_up_lua`] (the
    /// shared cold-start + reload pipeline) with a user-facing toast.
    pub(crate) fn reload_lua(&mut self) {
        self.pending_lua_reload = false;
        let err = self.bring_up_lua("reload");
        match err {
            Some(e) => self.notify_error(format!("lua reload: {e}")),
            None => self.notify("lua reloaded".into()),
        }
    }

    pub(crate) fn reload_lua_dismissing_modal(&mut self) {
        if let Some(modal_id) = self.ui.active_modal() {
            self.close_overlay(modal_id);
        }
        self.reload_lua();
    }

    /// Mark Lua config for reload at the next point where no turn or modal can
    /// hold callbacks that the reload would wipe. Returns `true` for a new
    /// request and `false` when one was already pending.
    pub(crate) fn schedule_lua_reload(&mut self) -> bool {
        if self.pending_lua_reload {
            return false;
        }
        self.pending_lua_reload = true;
        true
    }

    pub(crate) fn drain_idle_work(&mut self) -> bool {
        let mut did_work = self.dismiss_expired_notification();
        did_work |= self.try_perform_scheduled_lua_reload();
        did_work
    }

    fn try_perform_scheduled_lua_reload(&mut self) -> bool {
        if !self.pending_lua_reload || !self.can_reload_lua_now() {
            return false;
        }
        self.pending_lua_reload = false;
        self.reload_lua();
        true
    }

    fn can_reload_lua_now(&self) -> bool {
        !self.agent_is_running() && self.ui.active_modal().is_none()
    }

    /// Bring up (or rebuild) the Lua context. Single pipeline shared by
    /// cold start (`kind = "launch"`) and `/reload` (`kind = "reload"`).
    /// Plugin module bodies always run with the host pointer live and
    /// `lifecycle.on("ready")` hooks fire on every bring-up so plugins
    /// can rehydrate from `smelt.state` once per Lua-context init.
    /// Rust-owned UI state (named overlays, wins, bufs, paint slots)
    /// survives - plugins re-attach via `opts.name` and `smelt.state`.
    ///
    /// Phases:
    /// 1. Pending JSON-backed `smelt.state.persistent` writes are flushed
    ///    before timers are cleared so reload cannot drop debounced saves.
    /// 2. [`Self::clear_tui_for_reload`] wipes TUI-side caches that hold
    ///    Lua handles (timers, anonymous paint, anonymous overlays/
    ///    wins/bufs, picker state, busy stack).
    /// 3. [`LuaRuntime::reload`] wipes every `LuaShared` registry then
    ///    re-runs bootstrap → autoload → init.lua → plugins → state sweep.
    /// 4. [`Self::refresh_agent_inputs`] re-reads AGENTS.md, rebuilds the
    ///    [`engine::SkillLoader`], re-reads `--system-prompt` when present,
    ///    ships the refreshed bundle via
    ///    [`protocol::UiCommand::ReloadAgentConfig`].
    /// 5. [`Self::reconcile_mcp_servers`] reconciles MCP server state
    ///    off-thread against the new `smelt.mcp.register` desired set.
    /// 6. `smelt.lifecycle.on("ready", fn)` hooks drain with
    ///    `ctx = { kind }` so hooks that need to distinguish cold start
    ///    from reload can branch on it.
    ///
    /// Must be called inside an `install_app_ptr` scope. Returns the
    /// Lua load error (if any) so the caller can render it however
    /// makes sense for the phase.
    pub(crate) fn bring_up_lua(&mut self, kind: &'static str) -> Option<String> {
        if kind == "reload" {
            if let Some(err) = self.lua.flush_persistent_state() {
                return Some(format!("flush persistent state: {err}"));
            }
        }
        self.clear_tui_for_reload();
        let cwd = std::env::current_dir().ok();
        let err = self.lua.reload(cwd.as_deref());
        if err.is_none() {
            self.reconcile_lua_runtime_config();
        }
        self.refresh_agent_inputs();
        self.reconcile_mcp_servers();
        let hook_errors = self.lua.drain_lifecycle_hooks("ready", move |lua| {
            let t = lua.create_table()?;
            t.set("kind", kind)?;
            Ok::<mlua::Value, mlua::Error>(mlua::Value::Table(t))
        });
        for he in hook_errors {
            self.notify_error(he);
        }
        err
    }

    /// Re-read filesystem-backed inputs that feed the agent's system prompt
    /// and ship them to the engine. Runs as the last step of `/reload` so
    /// the engine sees fresh AGENTS.md / SKILL.md / `--system-prompt` bytes
    /// on the next turn, compaction, mid-turn mode change, or `EngineAsk`.
    pub(crate) fn refresh_agent_inputs(&mut self) {
        let outcome = self.prompt_inputs.refresh();
        self.core.skills = Some(outcome.loader);
        if let Some(err) = outcome.system_prompt_read_error {
            self.notify_error(err);
        }
        self.core
            .engine
            .send(self.prompt_inputs.to_reload_command());
    }

    fn reconcile_lua_runtime_config(&mut self) {
        let modes = self.lua.mode_names();
        if !modes.is_empty() {
            if !self.core.config.cli_mode_cycle_override {
                self.core.config.mode_cycle = modes.clone();
            }
            if !modes.contains(&self.core.config.mode) {
                self.set_mode(protocol::AgentMode::normal(), false);
            }
        }

        let raw_permissions = self.lua.take_permission_rules().unwrap_or_default();
        let tool_defaults = self.lua.tool_defaults();
        let mode_behaviors = self.lua.mode_behaviors();
        let permissions = smelt_core::permissions::Permissions::from_raw_with_mode_behaviors(
            &raw_permissions,
            &tool_defaults,
            mode_behaviors,
        )
        .with_runtime_state_from(self.core.permissions.as_ref());
        self.core.permissions = std::sync::Arc::new(permissions);
    }

    /// Reconcile MCP servers against the post-reload `smelt.mcp.register`
    /// desired-state set. Spawns/stops/restarts servers off-thread; the
    /// TUI continues without blocking on handshakes. The
    /// [`smelt_core::mcp::McpDispatcher`] reads tool defs live from the
    /// manager, so the engine's dispatch path picks up the new server
    /// set without further coordination.
    pub(crate) fn reconcile_mcp_servers(&mut self) {
        let desired = self.lua.mcp_configs_snapshot();
        let Some(manager) = self.core.mcp.clone() else {
            return;
        };
        tokio::spawn(async move {
            manager.reconcile(desired).await;
        });
    }

    /// **Single ledger** of every TUI-side cache that holds Lua handles
    /// or references resources reload will wipe. Add new caches here -
    /// the reload integration tests assert each one is empty/refreshed
    /// after a cycle.
    fn clear_tui_for_reload(&mut self) {
        self.core.cells.clear_lua_subscribers();
        self.core.timers.clear();
        // Window/event keymaps live on the UI tree, including named and
        // built-in windows that survive reload. Drop their Lua handles before
        // the runtime clears the callback registry so stale bindings cannot
        // keep swallowing prompt keys after the new Lua context comes up.
        for handle in self.ui.clear_lua_callbacks() {
            self.lua.remove_callback(handle);
        }
        // Anonymous paint slots get reaped; named slots survive with
        // stable `PaintId`s so overlays/layouts referencing them keep
        // working when the plugin re-registers in module body.
        for handle in self.paint_registry.clear_anonymous() {
            self.lua.remove_callback(handle);
        }
        // `smelt.work.busy` tokens are Reg-managed but reload wipes
        // the closures holding the Reg userdata before `:remove()` runs.
        // Clear here to match the timers/cells idiom and avoid leaking
        // entries until process exit.
        self.busy_stack = crate::app::BusyStack::default();
        // Anonymous overlays/wins/bufs from the previous cycle. Named
        // resources (`opts.name = "..."`) survive - plugins recover
        // them by re-passing the same name on re-open.
        let dropped = self.ui.reap_anonymous(smelt_core::lua::LUA_BUF_ID_BASE);
        for id in dropped {
            self.lua.remove_callback(id);
        }
        self.picker_state.clear();
    }

    /// Rewind to a transcript block, or to before the first turn when `block_idx` is `None`.
    pub(crate) fn rewind_to_block(&mut self, block_idx: Option<usize>, restore_vim_insert: bool) {
        if let Some(bidx) = block_idx {
            if self.agent.is_some() {
                self.cancel_agent();
                self.agent = None;
            }
            if let Some((text, images)) = self.rewind_to(bidx) {
                self.clear_prompt_prediction();
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.input.restore_from_rewind(&mut pctx, text, images);
            }
            while self.core.engine.try_recv().is_ok() {}
            self.save_session();
        } else {
            if self.agent.is_some() {
                self.cancel_agent();
                self.agent = None;
            }
            self.clear_prompt_prediction();
            self.rewind_to_start();
            while self.core.engine.try_recv().is_ok() {}
            self.save_session();
            if restore_vim_insert {
                let win = self
                    .ui
                    .win_mut(crate::app::PROMPT_WIN)
                    .expect("prompt window");
                self.input
                    .set_vim_mode(win, crate::smelt_edit::VimMode::Insert);
            }
        }
    }

    /// Load a saved session by id, refresh screen, and scroll to bottom. Silent no-op on miss.
    pub(crate) fn load_session_by_id(&mut self, id: &str) {
        if let Some(loaded) = smelt_core::session::load(id) {
            self.load_session(loaded);
            self.restore_screen();
            self.finish_transcript_turn();
            self.transcript_win_mut().scroll_to_bottom();
        }
    }

    /// Resolve a Confirm dialog. Cancels the active turn when the choice requires it.
    pub(crate) fn handle_confirm_resolve(
        &mut self,
        choice: ConfirmChoice,
        message: Option<String>,
        request_id: u64,
        call_id: &str,
        tool_name: &str,
    ) {
        let should_cancel = self.resolve_confirm((choice, message), call_id, request_id, tool_name);
        if should_cancel {
            self.finish_turn(true);
            self.agent = None;
        }
    }
}
