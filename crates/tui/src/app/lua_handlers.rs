//! Operations called by Lua bindings through `with_app`.

use crate::app::TuiApp;
use smelt_core::transcript_model::ConfirmChoice;

impl TuiApp {
    /// Run a slash command as if typed into the cmdline. Forwards `Exec` for shell escapes.
    pub(crate) fn apply_lua_command(&mut self, line: &str) {
        match crate::commands::run_command(self, line) {
            crate::app::CommandAction::Exec(handle) => {
                self.exec = Some(handle);
            }
            crate::app::CommandAction::Continue => {}
        }
    }

    /// `/reload` entry point. Three-phase:
    ///
    /// 1. [`Self::clear_tui_for_reload`] wipes TUI-side caches that hold
    ///    Lua handles or reference resources reload will invalidate.
    /// 2. [`LuaRuntime::reload`] (via its own `clear_for_reload`) wipes
    ///    every `LuaShared` registry, then re-runs bootstrap → autoload
    ///    → init.lua → plugins → state sweep.
    /// 3. [`Self::refresh_agent_inputs`] re-reads AGENTS.md, rebuilds the
    ///    [`engine::SkillLoader`], re-reads the `--system-prompt` file when
    ///    present, then ships the refreshed bundle to the engine task via
    ///    [`protocol::UiCommand::ReloadAgentConfig`].
    ///
    /// Rust-owned UI state (overlays, windows, buffers) is left in place
    /// when named — plugins re-attach via `smelt.state` and the
    /// `opts.name` survival path.
    pub(crate) fn reload_lua(&mut self) {
        self.clear_tui_for_reload();
        let cwd = std::env::current_dir().ok();
        let err = self.lua.reload(cwd.as_deref());
        self.input.command_arg_sources = self.lua.list_command_args();
        self.refresh_agent_inputs();
        self.reconcile_mcp_servers();
        match err {
            Some(e) => self.notify_error(format!("lua reload: {e}")),
            None => self.notify("lua reloaded".into()),
        }
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
    /// or references resources reload will wipe. Add new caches here —
    /// the reload integration tests assert each one is empty/refreshed
    /// after a cycle.
    fn clear_tui_for_reload(&mut self) {
        self.core.cells.clear_lua_subscribers();
        self.core.timers.clear();
        self.paint_registry.clear();
        // Anonymous overlays/wins/bufs from the previous cycle. Named
        // resources (`opts.name = "..."`) survive — plugins recover
        // them by re-passing the same name on re-open.
        let dropped = self.ui.reap_anonymous(smelt_core::lua::LUA_BUF_ID_BASE);
        for id in dropped {
            self.lua.remove_callback(id);
        }
        self.picker_state.clear();
    }

    pub(crate) fn compact_or_notify(&mut self, instructions: Option<String>) {
        if self.core.session.messages.is_empty() {
            self.notify_error("nothing to compact".into());
        } else {
            self.compact_history(instructions);
        }
    }

    /// Rewind to a transcript block, or restore Vim Insert mode when `block_idx` is `None`.
    pub(crate) fn rewind_to_block(&mut self, block_idx: Option<usize>, restore_vim_insert: bool) {
        if let Some(bidx) = block_idx {
            if self.agent.is_some() {
                self.cancel_agent();
                self.agent = None;
            }
            if let Some((text, images)) = self.rewind_to(bidx) {
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.input.restore_from_rewind(&mut pctx, text, images);
            }
            while self.core.engine.try_recv().is_ok() {}
            self.save_session();
        } else if restore_vim_insert {
            let win = self
                .ui
                .win_mut(crate::app::PROMPT_WIN)
                .expect("prompt window");
            self.input
                .set_vim_mode(win, crate::smelt_term::VimMode::Insert);
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
