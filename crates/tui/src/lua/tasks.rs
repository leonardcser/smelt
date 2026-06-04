//! TUI-specific callback queueing methods on `LuaRuntime`.

use super::LuaRuntime;

#[cfg(test)]
use std::sync::atomic::Ordering;

impl LuaRuntime {
    /// Register a Lua callable under a fresh u64 id. Test-only - production uses
    /// [`crate::lua::register_callback_handle`] directly.
    #[cfg(test)]
    pub(super) fn register_callback(&self, func: mlua::Function) -> mlua::Result<u64> {
        let handle = smelt_core::lua::LuaHandle::from_func(&self.lua, func)?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut cbs) = self.shared.callbacks.lock() {
            cbs.insert(id, handle);
        }
        Ok(id)
    }

    /// Invoke a registered callback with a payload table. Test-only - production uses
    /// the two-phase [`Self::prepare_invocation`] / call split to avoid borrow conflicts.
    #[cfg(test)]
    pub(super) fn invoke_callback(
        &self,
        handle: crate::smelt_edit::LuaHandle,
        win: crate::smelt_edit::WinId,
        payload: &crate::smelt_edit::Payload,
    ) {
        if let Some((func, payload_table)) = self.prepare_invocation(handle, win, payload) {
            if let Err(e) = func.call::<()>(payload_table) {
                self.record_error(format!("callback `{}`: {e}", handle.0));
            }
        }
    }

    /// Build the `mlua::Function` + payload table for a queued invocation without calling Lua.
    /// Splitting preparation from the call lets the host release all TuiApp borrows before
    /// the Lua body runs (so it can reach `&mut TuiApp` via [`crate::lua::with_app`]).
    /// Returns `None` if the handle is dropped or payload construction fails.
    pub(crate) fn prepare_invocation(
        &self,
        handle: crate::smelt_edit::LuaHandle,
        win: crate::smelt_edit::WinId,
        payload: &crate::smelt_edit::Payload,
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
        if let Err(e) = payload_table.set("leaf", win.0) {
            self.record_error(format!("callback payload: {e}"));
            return None;
        }
        Some((func, payload_table))
    }

    /// Record a callback error from the phase-2 invocation path.
    pub(crate) fn record_callback_error(&self, handle_id: u64, err: impl std::fmt::Display) {
        self.record_error(format!("callback `{handle_id}`: {err}"));
    }

    /// Queue an invocation from inside `ui.dispatch_event` / `ui.fire_win_event`.
    /// The ui dispatcher holds `&mut Ui`, so Lua cannot be called immediately - it would
    /// collide with the borrow. The host drains the queue after the ui call returns.
    pub(crate) fn queue_invocation(
        &self,
        handle: crate::smelt_edit::LuaHandle,
        win: crate::smelt_edit::WinId,
        payload: &crate::smelt_edit::Payload,
    ) {
        if let Ok(mut q) = self.shared.pending_invocations.lock() {
            q.push(crate::lua::PendingInvocation {
                handle,
                win,
                payload: payload.clone(),
            });
        }
    }

    /// Drain all queued invocations. Must be called under an [`crate::lua::install_app_ptr`] scope.
    pub(crate) fn drain_invocations(&self) -> Vec<crate::lua::PendingInvocation> {
        match self.shared.pending_invocations.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => Vec::new(),
        }
    }
}

/// Fill a Lua table with fields derived from `payload`.
fn populate_payload_table(
    table: &mlua::Table,
    payload: &crate::smelt_edit::Payload,
) -> mlua::Result<()> {
    match payload {
        crate::smelt_edit::Payload::None => Ok(()),
        crate::smelt_edit::Payload::Key { code, mods } => {
            table.set("code", format!("{code:?}"))?;
            table.set("mods", format!("{mods:?}"))?;
            Ok(())
        }
        crate::smelt_edit::Payload::Selection { index } => table.set("index", *index + 1),
        crate::smelt_edit::Payload::Text { content } => table.set("text", content.clone()),
        crate::smelt_edit::Payload::Mouse { row, col, button } => {
            table.set("row", *row)?;
            table.set("col", *col)?;
            table.set(
                "button",
                match button {
                    crate::smelt_edit::MouseButton::Left => "left",
                    crate::smelt_edit::MouseButton::Right => "right",
                    crate::smelt_edit::MouseButton::Middle => "middle",
                },
            )?;
            Ok(())
        }
        crate::smelt_edit::Payload::Scroll { top, follow } => {
            table.set("top", *top)?;
            table.set("follow", *follow)?;
            Ok(())
        }
        crate::smelt_edit::Payload::Rect {
            row,
            col,
            width,
            height,
            content_width,
        } => {
            table.set("row", *row)?;
            table.set("col", *col)?;
            table.set("width", *width)?;
            table.set("height", *height)?;
            table.set("content_width", *content_width)?;
            Ok(())
        }
    }
}
