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

    pub(crate) fn focused_vim_mode(&self) -> Option<crate::smelt_term::VimMode> {
        if let Some(win) = self.ui.focused_window() {
            if win.vim_enabled {
                return Some(win.vim_mode);
            }
        }
        match self.app_focus {
            crate::app::AppFocus::Content if self.transcript_win().vim_enabled => {
                Some(self.transcript_win().vim_mode)
            }
            crate::app::AppFocus::Prompt if self.input.vim_enabled(self.prompt_win()) => {
                Some(self.input.vim_mode(self.prompt_win()))
            }
            _ => None,
        }
    }

    /// Set the vim mode on whichever pane the user is currently driving. No-op if non-vim.
    pub(crate) fn set_focused_vim_mode(&mut self, mode: crate::smelt_term::VimMode) {
        if let Some(win_id) = self.ui.focus() {
            if let Some(w) = self.ui.win_mut(win_id) {
                if w.vim_enabled {
                    w.set_vim_mode(mode);
                    return;
                }
            }
        }
        match self.app_focus {
            crate::app::AppFocus::Content if self.transcript_win().vim_enabled => {
                self.transcript_win_mut().set_vim_mode(mode);
            }
            crate::app::AppFocus::Prompt => {
                let win = self
                    .ui
                    .win_mut(crate::app::PROMPT_WIN)
                    .expect("prompt window");
                self.input.set_vim_mode(win, mode);
            }
            _ => {}
        }
    }

    /// Fire `WinEvent::TextChanged` on `PROMPT_WIN` when the prompt buffer changed.
    pub(crate) fn emit_prompt_text_changed_if_dirty(&mut self) {
        let current_text = self.prompt_buf().source().to_string();
        if self.last_prompt_text == current_text {
            return;
        }
        self.last_prompt_text = current_text.clone();
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_term::LuaHandle,
                              win: crate::smelt_term::WinId,
                              payload: &crate::smelt_term::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui.fire_win_event(
            crate::app::PROMPT_WIN,
            crate::smelt_term::WinEvent::TextChanged,
            crate::smelt_term::Payload::Text {
                content: current_text,
            },
            &mut lua_invoke,
        );
        self.flush_lua_callbacks();
    }

    pub(crate) fn flush_lua_callbacks(&mut self) {
        self.drain_lua_invocations();
        self.lua.pump_task_events();
    }

    /// Drive cell subscribers + invocation queue + task inbox to a fixpoint.
    /// Either pass can re-feed the other, so loop until both are quiet.
    pub(crate) fn pump_lua(&mut self) {
        loop {
            let cells_had_work = self.core.cells.has_pending();
            if cells_had_work {
                self.drain_cells_pending();
            }
            self.flush_lua_callbacks();
            if !self.core.cells.has_pending() {
                break;
            }
        }
    }

    /// Drain invocations queued by `ui.dispatch_event` / `ui.fire_win_event`.
    ///
    /// Two-phase to keep `&mut TuiApp` aliasing clean: phase 1 collects (func, payload) while
    /// `&mut self` is live; phase 2 installs the TLS app pointer and calls each function with no
    /// Rust borrow on self — so Lua bodies that reach back via `with_app` get the sole reborrow.
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
            let _guard = crate::lua::install_app_ptr(self);
            for (func, payload, handle_id) in prepared {
                let _perf = smelt_perf::perf::begin("lua:event_cb");
                if let Err(e) = func.call::<()>(payload) {
                    crate::lua::try_with_app(|app| {
                        app.lua.record_callback_error(handle_id, e);
                    });
                }
            }
            // A callback may itself queue further invocations; drain in the same tick.
        }
    }

    pub(crate) fn drive_lua_tasks(&mut self) {
        self.flush_lua_callbacks();
        let outs = self.lua.drive_tasks(self.core.clock.instant_now());
        // Drain ops pushed before the coroutine yielded so `OpenLuaDialog` sees created buffers.
        self.flush_lua_callbacks();
        for out in outs {
            match out {
                crate::lua::TaskDriveOutput::ToolComplete {
                    request_id,
                    call_id,
                    content,
                    is_error,
                    metadata,
                } => {
                    self.core.engine.send(protocol::UiCommand::ToolResult {
                        request_id,
                        call_id,
                        content,
                        is_error,
                        metadata,
                    });
                }
                crate::lua::TaskDriveOutput::Error(msg) => {
                    self.notify_error(msg);
                }
            }
        }
        self.flush_lua_callbacks();
    }
}
