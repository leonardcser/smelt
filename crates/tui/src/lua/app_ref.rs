//! Thread-local pointer slots that let Lua bindings reach the frontend synchronously.
//!
//! Two TLS slots are installed at every Lua-entry boundary by [`install_app_ptr`]:
//! - `APP` — `*mut TuiApp` for UiHost-tier bindings.
//! - `CORE_PTR` — `*mut Core` (installed via `smelt_core::host::install_core_ptr`)
//!   for Host-tier bindings; headless-safe because `HeadlessApp` installs the same slot.
//!
//! Safety invariant: [`install_app_ptr`] borrows `&mut TuiApp` exclusively and returns a
//! drop-guard that restores the previous slots. The borrow is held across the Lua call but
//! is not touched while Lua runs — Rust is blocked on the FFI. [`with_app`] reborrows the
//! raw pointer as a fresh `&mut` for its closure; because Lua is single-threaded and never
//! re-enters a frame holding an active mutable borrow, the reborrow is always sole.
//! [`with_app`] panics if called outside an [`install_app_ptr`] scope.

use std::cell::Cell;
use std::ptr::NonNull;

use crate::app::TuiApp;

thread_local! {
    static APP: Cell<Option<NonNull<TuiApp>>> = const { Cell::new(None) };
}

/// Install `app` as the TLS pointer for the duration of the returned guard.
pub(crate) fn install_app_ptr(app: &mut TuiApp) -> AppPtrGuard {
    let app_ptr = NonNull::from(&mut *app);
    let old_app = APP.with(|cell| cell.replace(Some(app_ptr)));

    let core_guard = smelt_core::host::install_core_ptr(&mut app.core);
    AppPtrGuard {
        old_app,
        _core_guard: core_guard,
    }
}

/// Run `body` with the APP pointer installed for its duration. The
/// equivalent of `let _g = install_app_ptr(app); body(app)`, but reads
/// straight-line at the call site — preferred for one-shot Lua entries
/// like draining lifecycle hooks.
pub(crate) fn with_app_ptr<R>(app: &mut TuiApp, body: impl FnOnce(&mut TuiApp) -> R) -> R {
    let _guard = install_app_ptr(app);
    body(app)
}

/// Drop guard returned by [`install_app_ptr`]. Restores the previous slot on drop.
pub(crate) struct AppPtrGuard {
    old_app: Option<NonNull<TuiApp>>,
    _core_guard: smelt_core::host::CorePtrGuard,
}

impl Drop for AppPtrGuard {
    fn drop(&mut self) {
        APP.with(|cell| cell.set(self.old_app));
    }
}

/// Borrow the installed `&mut TuiApp` for the duration of `f`.
/// Panics if called outside an [`install_app_ptr`] scope.
pub(crate) fn with_app<R>(f: impl FnOnce(&mut TuiApp) -> R) -> R {
    let ptr = APP
        .with(|cell| cell.get())
        .expect("with_app called outside Lua entry");
    // SAFETY: pointer set by `install_app_ptr` which holds `&mut TuiApp` exclusively.
    // Caller does not access it while Lua runs, so this reborrow is sole.
    // Lua is single-threaded; no race possible.
    unsafe { f(ptr.as_ptr().as_mut().expect("app ptr must be non-null")) }
}

/// Non-panicking variant of `with_app`. Returns `None` if no app pointer is installed.
pub fn try_with_app<R>(f: impl FnOnce(&mut TuiApp) -> R) -> Option<R> {
    let ptr = APP.with(|cell| cell.get())?;
    // SAFETY: same contract as `with_app` above.
    Some(unsafe { f(ptr.as_ptr().as_mut().expect("app ptr must be non-null")) })
}

/// Re-export of `smelt_core::host::try_with_core` for TUI-tier bindings that need it.
pub(crate) fn try_with_core<R>(f: impl FnOnce(&mut smelt_core::Core) -> R) -> Option<R> {
    smelt_core::host::try_with_core(f)
}
