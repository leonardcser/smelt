//! Per-tick glue between `TuiApp` and the Lua runtime.

use crate::app::TuiApp;

impl TuiApp {
    /// Vim-mode label for the focused window, or `None` for non-vim surfaces.
    pub(crate) fn current_vim_mode_label(&self) -> Option<String> {
        self.focused_vim_mode_label()
    }

    /// Vim mode of the surface the user is currently driving (focused overlay > app pane).
    /// Returns `None` when the focused surface isn't vim-enabled.
    pub(crate) fn focused_vim_mode_label(&self) -> Option<String> {
        self.focused_vim_mode().map(|m| format!("{m:?}"))
    }

    pub(crate) fn focused_vim_mode(&self) -> Option<crate::smelt_edit::VimMode> {
        if let Some(win) = self.ui.focused_window() {
            if win.vim_enabled() {
                return Some(win.vim_mode());
            }
        }
        match self.app_focus {
            crate::app::AppFocus::Content if self.transcript_win().vim_enabled() => {
                Some(self.transcript_win().vim_mode())
            }
            crate::app::AppFocus::Prompt if self.prompt.vim_enabled(self.prompt_win()) => {
                Some(self.prompt.vim_mode(self.prompt_win()))
            }
            _ => None,
        }
    }

    /// Set the vim mode on whichever pane the user is currently driving. No-op if non-vim.
    pub(crate) fn set_focused_vim_mode(&mut self, mode: crate::smelt_edit::VimMode) {
        if let Some(win_id) = self.ui.focus() {
            if let Some(w) = self.ui.win_mut(win_id) {
                if w.vim_enabled() {
                    w.set_vim_mode(mode);
                    return;
                }
            }
        }
        match self.app_focus {
            crate::app::AppFocus::Content if self.transcript_win().vim_enabled() => {
                self.transcript_win_mut().set_vim_mode(mode);
            }
            crate::app::AppFocus::Prompt => {
                let win = self
                    .ui
                    .win_mut(crate::app::PROMPT_WIN)
                    .expect("prompt window");
                self.prompt.set_vim_mode(win, mode);
            }
            _ => {}
        }
    }

    /// Fire `WinEvent::TextChanged` on `PROMPT_WIN` when the prompt buffer changed.
    pub(crate) fn emit_prompt_text_changed_if_dirty(&mut self) {
        let current_text = self.prompt_buf().source().to_string();
        if !self.prompt.publish_text_if_changed(&current_text) {
            return;
        }
        let cursor_before = self.prompt_win().cpos();
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              win: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui.fire_win_event(
            crate::app::PROMPT_WIN,
            crate::smelt_edit::WinEvent::TextChanged,
            crate::smelt_edit::Payload::Text {
                content: current_text.clone(),
            },
            &mut lua_invoke,
        );
        self.flush_lua_callbacks();

        // `text_changed` is observational: filter/completer callbacks can edit
        // the prompt explicitly, but a callback that only moves the cursor must
        // not repark the insertion point after every typed character.
        if self.prompt_buf().source() == current_text && self.prompt_win().cpos() != cursor_before {
            {
                let pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                pctx.win
                    .set_cpos(smelt_buffer::text::snap(pctx.buf.source(), cursor_before));
                pctx.win.clear_selection_anchor();
                pctx.win.clamp_anchors_to_source(pctx.buf.source());
            }
        }
    }

    pub(crate) fn flush_lua_callbacks(&mut self) {
        self.drain_lua_invocations();
        self.drain_lua_commands();
        self.lua.pump_task_events();
    }

    fn drain_lua_commands(&mut self) {
        // Running a command flushes Lua callbacks before returning. A command
        // that queues another command must return to this outer drain instead
        // of recursively starting a fresh drain with a reset work budget.
        if !self.lua.shared().try_begin_command_drain() {
            return;
        }

        let mut remaining = crate::lua::MAX_PENDING_LUA_COMMANDS;
        loop {
            let commands = self.lua.shared().drain_commands();
            if commands.is_empty() {
                break;
            }
            let overflowed = commands.len() > remaining;
            for line in commands.into_iter().take(remaining) {
                self.apply_lua_command(&line);
                remaining -= 1;
            }
            if overflowed || remaining == 0 {
                let queued_more = !self.lua.shared().drain_commands().is_empty();
                if overflowed || queued_more {
                    self.notify_error_sticky(format!(
                        "deferred Lua commands exceeded {} in one flush",
                        crate::lua::MAX_PENDING_LUA_COMMANDS
                    ));
                }
                break;
            }
        }

        self.lua.shared().end_command_drain();
    }

    /// Drive cell subscribers + invocation queue + task inbox to a fixpoint.
    /// Either pass can re-feed the other, so loop until both are quiet.
    pub(crate) fn pump_lua(&mut self) {
        if smelt_core::host::host_access_active() {
            return;
        }
        loop {
            let cells_had_work = self.core.signals.has_pending();
            if cells_had_work {
                self.drain_signals_pending();
            }
            self.flush_lua_callbacks();
            if !self.core.signals.has_pending() {
                break;
            }
        }
    }

    /// Drain invocations queued by `ui.dispatch_event` / `ui.fire_win_event`.
    ///
    /// Two-phase to keep host borrowing explicit: phase 1 collects `(func, payload)` while
    /// `&mut self` is live; phase 2 lends the frontend root only for the callbacks' dynamic
    /// extent, so Lua cannot retain host authority after the calls return.
    pub(crate) fn drain_lua_invocations(&mut self) {
        loop {
            let pending = self.lua.drain_invocations();
            if pending.is_empty() {
                return;
            }
            let prepared: Vec<(mlua::Function, mlua::Table, u64)> = pending
                .into_iter()
                .filter_map(|inv| {
                    let (func, payload) =
                        self.lua
                            .prepare_invocation(inv.handle, inv.win, &inv.payload)?;
                    Some((func, payload, inv.handle.0))
                })
                .collect();
            let lua = self.lua.execution();
            crate::lua::scope_app(self, move || {
                for (func, payload, handle_id) in prepared {
                    let _perf = smelt_perf::perf::begin("lua:event_cb");
                    if let Err(e) = func.call::<()>(payload) {
                        lua.record_error(format!("callback `{handle_id}`: {e}"));
                    }
                }
            });
            // A callback may itself queue further invocations; drain in the same tick.
        }
    }

    pub(crate) fn drive_lua_tasks(&mut self) {
        self.flush_lua_callbacks();
        let now = self.core.clock.instant_now();
        let lua = self.lua.execution();
        let outs = crate::lua::scope_app(self, move || lua.drive_tasks(now));
        // Drain ops pushed before the coroutine yielded so `OpenLuaDialog` sees created buffers.
        self.flush_lua_callbacks();
        for out in outs {
            match out {
                crate::lua::TaskDriveOutput::ToolComplete {
                    invocation,
                    call_id,
                    content,
                    is_error,
                    metadata,
                } => {
                    self.complete_lua_tool(invocation, call_id, content, is_error, metadata);
                }
                crate::lua::TaskDriveOutput::Error(msg) => {
                    self.notify_error(msg);
                }
            }
        }
        self.flush_lua_callbacks();
    }
}
