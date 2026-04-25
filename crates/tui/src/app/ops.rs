//! Deferred-mutation queue.
//!
//! Lua bindings reach `&mut App` directly through
//! [`crate::lua::with_app`] and don't need this. Rust dialog
//! callbacks (currently only the Confirm dialog) fire from inside a
//! `&mut Ui` borrow, so they can't take `&mut App` synchronously —
//! they push a `Deferred` closure here and the host drains it after
//! the UI dispatch returns.
//!
//! The queue is a simple `Vec<Box<dyn FnOnce(&mut App) + Send +
//! 'static>>`. There is no enum dispatch; each closure carries its
//! own logic.

use std::sync::Arc;

use crate::lua::LuaShared;

/// Deferred mutation pushed by a Rust callback that holds `&mut Ui`
/// at fire time. Drained and applied by the app loop after the
/// dispatch returns.
pub type Deferred = Box<dyn FnOnce(&mut crate::app::App) + Send + 'static>;

/// Cloneable push-only handle to the shared deferred queue. Rust
/// dialog callbacks clone this and call [`OpsHandle::push`] from
/// inside their closures to request `&mut App` access. Obtained via
/// `LuaRuntime::ops_handle()`.
#[derive(Clone)]
pub struct OpsHandle(pub(crate) Arc<LuaShared>);

impl OpsHandle {
    /// Queue a closure to run on the next reducer tick with full
    /// `&mut App` access.
    pub fn push<F: FnOnce(&mut crate::app::App) + Send + 'static>(&self, f: F) {
        if let Ok(mut o) = self.0.ops.lock() {
            o.deferred.push(Box::new(f));
        }
    }

    /// Push a task-runtime event (dialog resolution, keymap-fired
    /// callback, …). These drain through `LuaRuntime::pump_task_events`
    /// each tick — they're a separate channel from `Deferred` so the
    /// task lifecycle stays orthogonal to deferred state mutations.
    pub fn push_task_event(&self, ev: crate::lua::TaskEvent) {
        if let Ok(mut inbox) = self.0.task_inbox.lock() {
            inbox.push(ev);
        }
    }

    /// Remove a callback id from `shared.callbacks`. Used by dialog
    /// close paths to clean up `on_press` handles so the registry
    /// doesn't leak.
    pub fn remove_callback(&self, id: u64) {
        if let Ok(mut cbs) = self.0.callbacks.lock() {
            cbs.remove(&id);
        }
    }
}
