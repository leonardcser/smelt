//! Task + callback lifecycle methods on `LuaRuntime`. Covers Lua
//! callbacks (`register_callback`, `invoke_callback`, `fire_callback`),
//! the parked-task resume API (`resolve_external` + `pump_task_events`),
//! the `LuaTaskRuntime` bridge (`drive_tasks`), and plugin-tool
//! execution (`tool_defs`, `execute_tool`).
//!
//! Headless-safe methods have moved to [`smelt_core::lua::LuaRuntime`];
//! this file only holds TUI-specific callback queueing.

use super::LuaRuntime;

#[cfg(test)]
use std::sync::atomic::Ordering;

impl LuaRuntime {
    /// Register a Lua callable under a fresh u64 id in
    /// `shared.callbacks`. Test-only: production registers callbacks
    /// through [`crate::lua::register_callback_handle`] which writes
    /// directly into `LuaShared.callbacks` without going through this
    /// runtime method.
    #[cfg(test)]
    pub(super) fn register_callback(&self, func: mlua::Function) -> mlua::Result<u64> {
        let key = self.lua.create_registry_value(func)?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut cbs) = self.shared.callbacks.lock() {
            cbs.insert(id, smelt_core::lua::LuaHandle { key });
        }
        Ok(id)
    }

    /// Invoke the Lua function registered under `handle.0` with a
    /// table derived from `payload`. Test-only: production splits this
    /// into [`Self::prepare_invocation`] (called while `&LuaRuntime` is
    /// borrowed) plus a separate `func.call::<()>(payload_table)`
    /// invocation after Rust borrows on `TuiApp` have lapsed (see
    /// `app/lua_bridge.rs::flush_callbacks`). Tests use the merged
    /// path for terseness.
    #[cfg(test)]
    pub(super) fn invoke_callback(
        &self,
        handle: crate::ui::LuaHandle,
        win: crate::ui::WinId,
        payload: &crate::ui::Payload,
    ) {
        if let Some((func, payload_table)) = self.prepare_invocation(handle, win, payload) {
            if let Err(e) = func.call::<()>(payload_table) {
                self.record_error(format!("callback `{}`: {e}", handle.0));
            }
        }
    }

    /// Produce the `mlua::Function` + built payload table for a queued
    /// invocation, **without calling into Lua**. Splitting this out of
    /// [`Self::invoke_callback`] lets the host drain the queue in two
    /// phases: phase 1 prepares each invocation while `&LuaRuntime` is
    /// borrowed (mlua table construction needs it); phase 2 calls them
    /// after all Rust borrows on TuiApp have lapsed (so the Lua body can
    /// reach `&mut TuiApp` through [`crate::lua::with_app`]). Returns
    /// `None` when the handle is already dropped or when payload
    /// construction errored — both recorded as Lua errors.
    pub(crate) fn prepare_invocation(
        &self,
        handle: crate::ui::LuaHandle,
        win: crate::ui::WinId,
        payload: &crate::ui::Payload,
    ) -> Option<(mlua::Function, mlua::Table)> {
        let func = {
            let cbs = self.shared.callbacks.lock().ok()?;
            let h = cbs.get(&handle.0)?;
            self.lua.registry_value::<mlua::Function>(&h.key).ok()?
        };
        let payload_table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("callback payload: {e}"));
                return None;
            }
        };
        if let Err(e) = populate_payload_table(&payload_table, payload) {
            self.record_error(format!("callback payload: {e}"));
            return None;
        }
        if let Err(e) = payload_table.set("win", win.0) {
            self.record_error(format!("callback payload: {e}"));
            return None;
        }
        Some((func, payload_table))
    }

    /// Record a callback error from outside the runtime's usual call
    /// path (e.g. a phase-2 invocation in `TuiApp::drain_lua_invocations`).
    pub(crate) fn record_callback_error(&self, handle_id: u64, err: impl std::fmt::Display) {
        self.record_error(format!("callback `{handle_id}`: {err}"));
    }

    /// Record a pending invocation from inside `ui.dispatch_event` /
    /// `ui.fire_win_event`. The ui dispatcher holds `&mut Ui` while the
    /// callback would fire, so firing Lua immediately would deny Lua
    /// bindings access to `&mut TuiApp` (they'd collide with the ui
    /// borrow). The host drains this queue right after the ui call
    /// returns and invokes each callback with the TLS app pointer
    /// installed, giving Lua bindings sole access to TuiApp state.
    pub(crate) fn queue_invocation(
        &self,
        handle: crate::ui::LuaHandle,
        win: crate::ui::WinId,
        payload: &crate::ui::Payload,
    ) {
        if let Ok(mut q) = self.shared.pending_invocations.lock() {
            q.push(crate::lua::PendingInvocation {
                handle,
                win,
                payload: payload.clone(),
            });
        }
    }

    /// Drain every queued callback invocation. Called by the host after
    /// `ui.dispatch_event` / `ui.fire_win_event` returns, under an
    /// [`crate::lua::install_app_ptr`] scope so each Lua body can reach
    /// `&mut TuiApp` through [`crate::lua::with_app`].
    pub(crate) fn drain_invocations(&self) -> Vec<crate::lua::PendingInvocation> {
        match self.shared.pending_invocations.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => Vec::new(),
        }
    }
}

/// Fill a Lua table with fields from a `crate::ui::Payload` for
/// `LuaRuntime::invoke_callback`.
fn populate_payload_table(table: &mlua::Table, payload: &crate::ui::Payload) -> mlua::Result<()> {
    match payload {
        crate::ui::Payload::None => Ok(()),
        crate::ui::Payload::Key { code, mods } => {
            table.set("code", format!("{code:?}"))?;
            table.set("mods", format!("{mods:?}"))?;
            Ok(())
        }
        crate::ui::Payload::Selection { index } => table.set("index", *index + 1),
        crate::ui::Payload::Text { content } => table.set("text", content.clone()),
    }
}
