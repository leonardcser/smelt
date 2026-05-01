//! Thread-local `&mut TuiApp` access for Lua bindings.
//!
//! mlua closures can't hold `&mut TuiApp` because of the borrow checker,
//! and `Arc<LuaShared>` can only expose what the shared struct carries.
//! To let Lua bindings reach the full TuiApp synchronously — the way
//! Neovim's C API lets Lua reach Vim's globals — we install a
//! `NonNull<TuiApp>` in thread-local storage at every Rust-side Lua-entry
//! boundary, and Lua bindings call [`with_app`] to borrow it.
//!
//! Safety contract
//! ---------------
//! `install_app_ptr` is the only way to set the pointer; it borrows
//! `&mut TuiApp` briefly to capture the address, then returns a drop guard
//! that restores the previous slot on scope exit. Callers hold their
//! `&mut TuiApp` borrow across the subsequent Lua call, but that borrow is
//! *not accessed* while Lua runs — Rust is blocked on the FFI call and
//! the only way to touch TuiApp is through [`with_app`], which reborrows
//! the raw pointer as a fresh `&mut TuiApp` for the duration of its
//! closure. Because Lua is single-threaded inside the TUI event loop
//! and never re-enters a Rust stack frame that is itself holding an
//! active mutable borrow, the reborrow is sole.
//!
//! `with_app` panics if the pointer is unset — a defensive check for
//! the "Lua ran from a site that forgot to install the pointer" bug.

use std::cell::Cell;
use std::ptr::NonNull;

use crate::app::TuiApp;

thread_local! {
    static APP: Cell<Option<NonNull<TuiApp>>> = const { Cell::new(None) };
}

/// Install `app` as the TLS pointer for the duration of the returned
/// guard. Typically called at the top of any function that drives Lua
/// (callback invocation, command dispatch, autocmd emission, task
/// resumption).
pub(crate) fn install_app_ptr(app: &mut TuiApp) -> AppPtrGuard {
    let ptr = NonNull::from(&mut *app);
    let old = APP.with(|cell| cell.replace(Some(ptr)));
    AppPtrGuard { old }
}

/// Drop guard returned by [`install_app_ptr`]. Restores the previous
/// slot (usually `None`, but nested installs are supported).
pub(crate) struct AppPtrGuard {
    old: Option<NonNull<TuiApp>>,
}

impl Drop for AppPtrGuard {
    fn drop(&mut self) {
        APP.with(|cell| cell.set(self.old));
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
pub(crate) fn try_with_app<R>(f: impl FnOnce(&mut TuiApp) -> R) -> Option<R> {
    let ptr = APP.with(|cell| cell.get())?;
    // SAFETY: same contract as `with_app`.
    Some(unsafe { f(ptr.as_ptr().as_mut().expect("app ptr must be non-null")) })
}

/// Borrow the installed frontend as `&mut dyn Host` for the duration
/// of `f`. Host-tier Lua bindings (cells, timers, engine, clipboard,
/// session, confirms, lua, autocmds) reach through here so they
/// compose without locking the whole frontend struct, and so they
/// stay headless-safe once `P2.b.5b` lets `HeadlessApp` install into
/// the same TLS slot. Panics if the pointer is unset (same contract
/// as `with_app`).
///
/// `pub(crate)` because `Host` itself is `pub(crate)` — neither leaks
/// outside the crate.
pub(crate) fn with_host<R>(f: impl FnOnce(&mut dyn crate::app::Host) -> R) -> R {
    with_app(|app| f(app))
}

/// `try_` variant of `with_host` that returns `None` instead of
/// panicking when no frontend is installed.
pub(crate) fn try_with_host<R>(f: impl FnOnce(&mut dyn crate::app::Host) -> R) -> Option<R> {
    try_with_app(|app| f(app))
}

/// Borrow the installed frontend as `&mut dyn ui::UiHost` for the
/// duration of `f`. UiHost-only Lua bindings (`smelt.ui` / `.win` /
/// `.buf` / `.statusline`) reach through here. Once `P2.b.5b` lets
/// `HeadlessApp` install into the TLS slot, this dispatcher will
/// surface a runtime error from headless contexts (today every
/// installed frontend impls `UiHost`).
pub(crate) fn with_ui_host<R>(f: impl FnOnce(&mut dyn ui::UiHost) -> R) -> R {
    with_app(|app| f(app))
}

/// `try_` variant of `with_ui_host` that returns `None` instead of
/// panicking when no frontend is installed.
pub(crate) fn try_with_ui_host<R>(f: impl FnOnce(&mut dyn ui::UiHost) -> R) -> Option<R> {
    try_with_app(|app| f(app))
}
