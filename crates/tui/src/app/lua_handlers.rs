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

    /// `/reload` entry point. Clears append-only Lua registries held on
    /// `Core` (cells, timers) and the TUI's `PaintRegistry`, then runs
    /// [`LuaRuntime::reload`] for the `LuaShared` registries + module
    /// re-evaluation. Rust-owned UI state — overlays, windows, buffers
    /// — is left in place; plugins re-attach to it via `smelt.state`.
    pub(crate) fn reload_lua(&mut self) {
        self.core.cells.clear_lua_subscribers();
        self.core.timers.clear();
        self.paint_registry.clear();
        // Tear down anonymous overlays/wins/bufs from the previous cycle.
        // Named resources (`opts.name = "..."`) survive — plugins recover
        // them by re-passing the same name on re-open.
        let dropped = self.ui.reap_anonymous(smelt_core::lua::LUA_BUF_ID_BASE);
        for id in dropped {
            self.lua.remove_callback(id);
        }
        self.picker_state.clear();
        let cwd = std::env::current_dir().ok();
        let err = self.lua.reload(cwd.as_deref());
        match err {
            Some(e) => self.notify_error(format!("lua reload: {e}")),
            None => self.notify("lua reloaded".into()),
        }
        self.input.command_arg_sources = self.lua.list_command_args();
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
