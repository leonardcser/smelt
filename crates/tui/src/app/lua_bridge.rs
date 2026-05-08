//! Per-tick glue between `TuiApp` and the Lua runtime.

use crate::app::TuiApp;

impl TuiApp {
    /// Vim-mode label for the focused window, or `None` for non-vim surfaces.
    pub(crate) fn current_vim_mode_label(&self) -> Option<String> {
        if let Some(win) = self.ui.focused_window() {
            if win.vim_enabled {
                return Some(format!("{:?}", self.vim_mode));
            }
        }
        let has_vim = match self.app_focus {
            crate::app::AppFocus::Content => self.transcript_window.vim_enabled,
            crate::app::AppFocus::Prompt => self.input.vim_enabled(),
        };
        has_vim.then(|| format!("{:?}", self.vim_mode))
    }

    /// Fire `WinEvent::TextChanged` on `PROMPT_WIN` when the prompt buffer changed.
    pub(crate) fn emit_prompt_text_changed_if_dirty(&mut self) {
        let current_text = self.input.source.clone();
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
                let _perf = smelt_core::perf::begin("lua:event_cb");
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
        let outs = self.lua.drive_tasks();
        // Drain ops pushed before the coroutine yielded so `OpenLuaDialog` sees created buffers.
        self.flush_lua_callbacks();
        for out in outs {
            match out {
                crate::lua::TaskDriveOutput::ToolComplete {
                    request_id,
                    call_id,
                    content,
                    is_error,
                } => {
                    self.core.engine.send(protocol::UiCommand::ToolResult {
                        request_id,
                        call_id,
                        content,
                        is_error,
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
