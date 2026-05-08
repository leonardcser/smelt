//! TLS slot for `&mut Core`, used by Host-tier Lua bindings.
//!
//! Whichever frontend drives Lua installs its `Core` here; bindings reborrow
//! through [`with_core`] / [`try_with_core`]. `HeadlessApp` installs only the
//! Core pointer; UiHost-only bindings return an error from headless context.

use super::runtime::Core;
use std::cell::RefCell;

thread_local! {
    static CORE_PTR: RefCell<Option<*mut Core>> = const { RefCell::new(None) };
}

/// Install `core` as the TLS pointer for the duration of the returned guard.
pub fn install_core_ptr(core: &mut Core) -> CorePtrGuard {
    let ptr: *mut Core = core;
    let old = CORE_PTR.with(|cell| cell.replace(Some(ptr)));
    CorePtrGuard { old }
}

pub struct CorePtrGuard {
    old: Option<*mut Core>,
}

impl Drop for CorePtrGuard {
    fn drop(&mut self) {
        CORE_PTR.with(|cell| *cell.borrow_mut() = self.old);
    }
}

/// Panics if called outside an [`install_core_ptr`] scope.
pub fn with_core<R>(f: impl FnOnce(&mut Core) -> R) -> R {
    let ptr = CORE_PTR
        .with(|cell| *cell.borrow())
        .expect("with_core called outside Lua entry");
    // SAFETY: the pointer is set only by `install_core_ptr`, which
    // borrows `&mut Core` exclusively. The caller holds that borrow
    // across subsequent Lua calls but does not access it while Lua runs.
    unsafe { f(&mut *ptr) }
}

pub fn try_with_core<R>(f: impl FnOnce(&mut Core) -> R) -> Option<R> {
    let ptr = CORE_PTR.with(|cell| *cell.borrow())?;
    Some(unsafe { f(&mut *ptr) })
}
