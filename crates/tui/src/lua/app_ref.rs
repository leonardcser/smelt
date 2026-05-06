//! Thread-local pointer slots that let Lua bindings reach the
//! frontend synchronously.
//!
//! Three slots are installed at every Rust-side Lua-entry boundary by
//! [`install_app_ptr`]:
//!
//! * `APP_PTR` — `*mut TuiApp`. Concrete-type slot. Used by bindings
//!   that need TuiApp internals not exposed through `Host` /
//!   `UiHost` (e.g. command dispatcher, transcript yank).
//! * `UI_HOST_PTR` — `*mut dyn crate::ui::UiHost`. Trait-object slot.
//!   UiHost-only bindings (`smelt.buf` / `.win` / `.ui` /
//!   `.statusline` / …) reach through here so any frontend that
//!   impls `UiHost` (TuiApp today, story fixtures or future GUI
//!   tomorrow) can serve them without rebinding rewrites.
//! * `CORE_PTR` — `*mut dyn smelt_core::Host`. Installed inside core
//!   via `smelt_core::host::install_core_ptr`. Host-tier bindings
//!   reach through there.
//!
//! Safety contract
//! ---------------
//! `install_app_ptr` is the only way to set the slots; it borrows
//! `&mut TuiApp` briefly to capture the addresses, then returns a drop
//! guard that restores the previous slots on scope exit. Callers hold
//! their `&mut TuiApp` borrow across the subsequent Lua call, but that
//! borrow is *not accessed* while Lua runs — Rust is blocked on the
//! FFI call and the only way to touch TuiApp is through [`with_app`]
//! / [`with_ui_host`] / `with_host`, each of which reborrows the raw
//! pointer as a fresh `&mut` for the duration of its closure. Because
//! Lua is single-threaded inside the TUI event loop and never re-enters
//! a Rust stack frame that is itself holding an active mutable borrow,
//! the reborrow is sole.
//!
//! `with_app` / `with_ui_host` panic if their slot is unset — a
//! defensive check for the "Lua ran from a site that forgot to install
//! the pointer" bug.

use std::cell::Cell;
use std::ptr::NonNull;

use crate::app::TuiApp;
use crate::ui::UiHost;

thread_local! {
    static APP: Cell<Option<NonNull<TuiApp>>> = const { Cell::new(None) };
    static UI_HOST: Cell<Option<NonNull<dyn UiHost>>> = const { Cell::new(None) };
}

/// Install `app` as the TLS pointer for the duration of the returned
/// guard. Typically called at the top of any function that drives Lua
/// (callback invocation, command dispatch, autocmd emission, task
/// resumption).
pub(crate) fn install_app_ptr(app: &mut TuiApp) -> AppPtrGuard {
    let app_ptr = NonNull::from(&mut *app);
    let old_app = APP.with(|cell| cell.replace(Some(app_ptr)));

    let ui_host_ptr: NonNull<dyn UiHost> = {
        let dyn_ref: &mut dyn UiHost = app;
        NonNull::from(dyn_ref)
    };
    let old_ui_host = UI_HOST.with(|cell| cell.replace(Some(ui_host_ptr)));

    let core_guard = smelt_core::host::install_core_ptr(&mut app.core);
    AppPtrGuard {
        old_app,
        old_ui_host,
        core_guard,
    }
}

/// Drop guard returned by [`install_app_ptr`]. Restores the previous
/// slot (usually `None`, but nested installs are supported).
pub(crate) struct AppPtrGuard {
    old_app: Option<NonNull<TuiApp>>,
    old_ui_host: Option<NonNull<dyn UiHost>>,
    #[allow(dead_code)]
    core_guard: smelt_core::host::CorePtrGuard,
}

impl Drop for AppPtrGuard {
    fn drop(&mut self) {
        APP.with(|cell| cell.set(self.old_app));
        UI_HOST.with(|cell| cell.set(self.old_ui_host));
    }
}

/// Borrow the installed `&mut TuiApp` for the duration of `f`. Panics if
/// called outside a [`install_app_ptr`] scope — a Lua binding hit from
/// a context we forgot to wire.
pub(crate) fn with_app<R>(f: impl FnOnce(&mut TuiApp) -> R) -> R {
    let ptr = APP
        .with(|cell| cell.get())
        .expect("with_app called outside Lua entry");
    // SAFETY: the pointer is set only by `install_app_ptr`, which
    // borrows `&mut TuiApp` exclusively. The caller of `install_app_ptr`
    // holds that borrow across subsequent Lua calls but does not
    // access it while Lua runs — so the reborrow here is sole. Lua is
    // single-threaded inside the TUI event loop; no other thread can
    // race.
    unsafe { f(ptr.as_ptr().as_mut().expect("app ptr must be non-null")) }
}

/// Variant that returns `None` if the pointer is unset instead of
/// panicking. Useful from bindings that might be called from a
/// benchmark / test harness without a `TuiApp`.
pub fn try_with_app<R>(f: impl FnOnce(&mut TuiApp) -> R) -> Option<R> {
    let ptr = APP.with(|cell| cell.get())?;
    // SAFETY: same contract as `with_app`.
    Some(unsafe { f(ptr.as_ptr().as_mut().expect("app ptr must be non-null")) })
}

/// Borrow the installed frontend as `&mut dyn smelt_core::Host` for
/// the duration of `f`. Host-tier Lua bindings (cells, timers, engine,
/// clipboard, session, confirms, lua, autocmds) reach through here so
/// they compose without locking the whole frontend struct, and so
/// they stay headless-safe — `HeadlessApp` installs into the same TLS
/// slot inside `smelt_core::host`.
///
/// `pub(crate)` because `Host` itself is `pub(crate)` — neither leaks
/// outside the crate.
#[allow(dead_code)]
pub(crate) fn with_host<R>(f: impl FnOnce(&mut dyn smelt_core::Host) -> R) -> R {
    smelt_core::host::with_host(f)
}

/// `try_` variant of `with_host` that returns `None` instead of
/// panicking when no frontend is installed.
#[allow(dead_code)]
pub(crate) fn try_with_host<R>(f: impl FnOnce(&mut dyn smelt_core::Host) -> R) -> Option<R> {
    smelt_core::host::try_with_host(f)
}

/// Borrow the installed frontend as `&mut dyn UiHost` for the duration
/// of `f`. UiHost-only Lua bindings (`smelt.ui` / `.win` / `.buf` /
/// `.statusline`) reach through here. The slot is decoupled from the
/// concrete `TuiApp` so any future frontend that impls `UiHost` (story
/// fixtures, alternative compositor) can install without binding
/// rewrites.
pub(crate) fn with_ui_host<R>(f: impl FnOnce(&mut dyn UiHost) -> R) -> R {
    let ptr = UI_HOST
        .with(|cell| cell.get())
        .expect("with_ui_host called outside Lua entry");
    // SAFETY: the pointer is set only by `install_app_ptr`, which
    // borrows `&mut TuiApp` exclusively for the duration of the guard.
    // The same single-threaded re-borrow contract as `with_app`
    // applies, but through the `dyn UiHost` vtable.
    unsafe { f(ptr.as_ptr().as_mut().expect("ui_host ptr must be non-null")) }
}

/// `try_` variant of `with_ui_host` that returns `None` instead of
/// panicking when no frontend is installed.
pub(crate) fn try_with_ui_host<R>(f: impl FnOnce(&mut dyn UiHost) -> R) -> Option<R> {
    let ptr = UI_HOST.with(|cell| cell.get())?;
    // SAFETY: same contract as `with_ui_host`.
    Some(unsafe { f(ptr.as_ptr().as_mut().expect("ui_host ptr must be non-null")) })
}
