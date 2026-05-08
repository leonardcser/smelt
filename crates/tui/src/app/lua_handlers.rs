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
                self.input.restore_from_rewind(text, images);
            }
            while self.core.engine.try_recv().is_ok() {}
            self.save_session();
        } else if restore_vim_insert {
            self.input
                .set_vim_mode(&mut self.vim_mode, crate::smelt_term::VimMode::Insert);
        }
    }

    /// Load a saved session by id, refresh screen, and scroll to bottom. Silent no-op on miss.
    pub(crate) fn load_session_by_id(&mut self, id: &str) {
        if let Some(loaded) = smelt_core::session::load(id) {
            self.load_session(loaded);
            self.restore_screen();
            self.finish_transcript_turn();
            self.transcript_window.scroll_to_bottom();
        }
    }

    pub(crate) fn yank_current_block(&mut self) {
        let abs_row = self.transcript_window.cursor_abs_row();
        if let Some(text) = self.block_text_at_row(abs_row, self.core.config.settings.show_thinking)
        {
            if self.core.clipboard.write(&text).is_ok() {
                self.core.clipboard.kill_ring.record_clipboard_write(text);
            }
            self.notify("block copied".into());
        } else {
            self.notify_error("no block at cursor".into());
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
