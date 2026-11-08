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

    /// Queue `WinEvent::TextChanged` on `PROMPT_WIN` when the prompt buffer changed.
    /// The returned snapshot lets synchronous input dispatch preserve its cursor
    /// across observational callbacks. Lua-originated edits are pumped after the
    /// current Lua entry returns, when re-entering the runtime is safe.
    pub(crate) fn queue_prompt_text_changed_if_dirty(&mut self) -> Option<(String, usize)> {
        let current_text = self.prompt_buf().source().to_string();
        if !self.prompt.publish_text_if_changed(&current_text) {
            return None;
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
        Some((current_text, cursor_before))
    }

    /// Fire `WinEvent::TextChanged` on `PROMPT_WIN` when the prompt buffer changed.
    pub(crate) fn emit_prompt_text_changed_if_dirty(&mut self) {
        let Some((current_text, cursor_before)) = self.queue_prompt_text_changed_if_dirty() else {
            return;
        };
        self.flush_lua_callbacks();

        // `text_changed` is observational: filter/completer callbacks can edit
        // the prompt explicitly, but a callback that only moves the cursor must
        // not repark the insertion point after every typed character.
        if self.prompt_buf().source() == current_text && self.prompt_win().cpos() != cursor_before {
            let pctx = crate::input::prompt_ctx_mut(&mut self.ui);
            pctx.win.set_cpos(smelt_buffer::text::snap_grapheme(
                pctx.buf.source(),
                cursor_before,
            ));
            pctx.win.clear_selection_anchor();
            pctx.win.clamp_anchors_to_source(pctx.buf.source());
        }
    }

    pub(crate) fn flush_lua_callbacks(&mut self) {
        let work = self.lua.shared().begin_work();
        self.drain_lua_invocations();
        self.drain_lua_commands();
        self.lua.pump_task_events();
        self.finish_lua_work(&work);
    }

    pub(super) fn finish_lua_work(&self, work: &crate::lua::LuaWorkGuard) {
        if work.root && self.has_queued_lua_work() {
            let _ = self.lua.wakeup_sender().send(());
        }
    }

    /// Whether a budgeted pump left signals, callbacks, commands or task resumes queued.
    pub(crate) fn has_queued_lua_work(&self) -> bool {
        self.core.signals.has_pending()
            || self.lua.shared().has_pending_callbacks()
            || self.lua.shared().has_pending_commands()
            || self.lua.core_shared().has_pending_task_events()
    }

    fn drain_lua_commands(&mut self) {
        // Running a command flushes Lua callbacks before returning. A command
        // that queues another command must return to this outer drain instead
        // of recursively starting a fresh drain with a reset work budget.
        if !self.lua.shared().try_begin_command_drain() {
            return;
        }

        let shared = std::sync::Arc::clone(self.lua.shared());
        for _ in 0..crate::lua::LUA_WORK_BATCH {
            if !std::sync::Arc::ptr_eq(&shared, self.lua.shared())
                || !shared.has_pending_commands()
                || !shared.claim_work()
            {
                break;
            }
            match shared.pop_command() {
                Some(crate::lua::DeferredCommand::Line(line)) => self.apply_lua_command(&line),
                Some(crate::lua::DeferredCommand::SubmitPrompt { text, stage }) => {
                    self.submit_prompt(text, stage)
                }
                None => break,
            }
        }
        shared.end_command_drain();
    }

    /// Drive reactive work within one shared callback/time budget. Remaining work
    /// keeps its queue position and wakes the event loop for another pass.
    pub(crate) fn pump_lua(&mut self) {
        if smelt_core::host::host_access_active() {
            return;
        }
        let work = self.lua.shared().begin_work();
        while self.lua.shared().has_work_budget() {
            self.drain_signals_pending();
            self.flush_lua_callbacks();
            if !self.core.signals.has_pending()
                && !self.lua.shared().has_pending_callbacks()
                && !self.lua.shared().has_pending_commands()
                && !self.lua.core_shared().has_pending_task_events()
            {
                break;
            }
        }
        self.finish_lua_work(&work);
    }

    /// Drain invocations queued by `ui.dispatch_event` / `ui.fire_win_event`.
    ///
    /// Prepare each callback before lending the frontend root for its dynamic
    /// extent, so Lua cannot retain host authority after the call returns.
    pub(crate) fn drain_lua_invocations(&mut self) {
        let work = self.lua.shared().begin_work();
        for _ in 0..crate::lua::LUA_WORK_BATCH {
            if !self.lua.shared().has_pending_callbacks() || !self.lua.shared().claim_work() {
                break;
            }
            let Some(inv) = self.lua.pop_invocation() else {
                break;
            };
            let Some((func, payload)) =
                self.lua
                    .prepare_invocation(inv.handle, inv.win, &inv.payload)
            else {
                continue;
            };
            let lua = self.lua.execution();
            crate::lua::scope_app(self, move || {
                let _perf = smelt_perf::perf::begin("lua:event_cb");
                if let Err(error) = func.call::<()>(payload) {
                    lua.record_error(format!("callback `{}`: {error}", inv.handle.0));
                }
            });
        }
        self.finish_lua_work(&work);
    }

    /// Resume Lua tasks that are waiting on a wakeup. A frame painted first
    /// would show the state between two steps of a task, such as no dialog
    /// between a closed dialog and the one the task opens next.
    pub(crate) fn drive_woken_lua_tasks(&mut self) {
        if self.lua.take_wakeups() {
            self.drive_lua_tasks();
        }
    }

    pub(crate) fn drive_lua_tasks(&mut self) {
        let work = self.lua.shared().begin_work();
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
                    display_content,
                    attachment,
                } => {
                    self.complete_lua_tool(
                        invocation,
                        call_id,
                        crate::app::agent::LuaToolCompletion {
                            content,
                            is_error,
                            metadata,
                            display_content,
                            attachment,
                        },
                    );
                }
                crate::lua::TaskDriveOutput::ToolEvaluated { request, result } => {
                    let lua = self.lua.execution();
                    let metadata = crate::lua::scope_app(self, || {
                        lua.finish_tool_evaluation(&request, result)
                    });
                    self.finish_tool_evaluation(*request, metadata);
                }
                crate::lua::TaskDriveOutput::NotifyError(msg) => {
                    self.notify_error(msg);
                }
            }
        }
        self.flush_lua_callbacks();
        self.finish_lua_work(&work);
    }
}
