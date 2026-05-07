//! TLS-installed `&mut Core` slot used by Host-tier Lua bindings.
//!
//! Host-tier bindings (`smelt.cell`, `smelt.timer`, `smelt.fs`, etc.)
//! need to reach the headless-safe runtime state — `Cells`, `Timers`,
//! `Session`, `EngineClient`, `AppConfig`, the file/process registries,
//! the skill loader, the frontend tag, and `Confirms`. Each frontend
//! (`HeadlessApp`, `TuiApp`, future story-app) owns a `Core`; whichever
//! frontend is driving Lua at the moment installs `&mut Core` here, and
//! the bindings reborrow it through [`with_core`] / [`try_with_core`].
//!
//! The compositor-bearing surface lives in `tui::smelt_term::UiHost` (defined in
//! the tui crate). It does not extend any Core-tier trait — `smelt-term`
//! can never reference tui-defined types. `TuiApp` installs both pointers in
//! parallel; `HeadlessApp` installs only the Core pointer and errors at
//! runtime if a UiHost-only Lua binding is invoked from a headless context.

use super::runtime::Core;
use std::cell::RefCell;

thread_local! {
    static CORE_PTR: RefCell<Option<*mut Core>> = const { RefCell::new(None) };
}

/// Install `core` as the TLS pointer for the duration of the returned guard.
/// Typically called at the top of any function that drives Lua callbacks.
pub fn install_core_ptr(core: &mut Core) -> CorePtrGuard {
    let ptr: *mut Core = core;
    let old = CORE_PTR.with(|cell| cell.replace(Some(ptr)));
    CorePtrGuard { old }
}

/// Drop guard returned by [`install_core_ptr`]. Restores the previous slot.
pub struct CorePtrGuard {
    old: Option<*mut Core>,
}

impl Drop for CorePtrGuard {
    fn drop(&mut self) {
        CORE_PTR.with(|cell| *cell.borrow_mut() = self.old);
    }
}

/// Borrow the installed `&mut Core` for the duration of `f`.
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

/// Variant that returns `None` if the pointer is unset instead of panicking.
pub fn try_with_core<R>(f: impl FnOnce(&mut Core) -> R) -> Option<R> {
    let ptr = CORE_PTR.with(|cell| *cell.borrow())?;
    Some(unsafe { f(&mut *ptr) })
}
