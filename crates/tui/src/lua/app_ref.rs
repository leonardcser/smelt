//! Thread-local pointer slots that let Lua bindings reach the
//! frontend synchronously.
//!
//! Two slots are installed at every Rust-side Lua-entry boundary by
//! [`install_app_ptr`]:
//!
//! * `APP` — `*mut TuiApp`. UiHost-tier bindings (`smelt.buf` /
//!   `.win` / `.ui` / `.statusline` / `.theme` / …) reborrow this
//!   to reach the compositor and TuiApp internals.
//! * `CORE_PTR` — `*mut Core`. Installed inside core via
//!   `smelt_core::host::install_core_ptr`. Host-tier bindings
//!   (cells / timers / engine / clipboard / session / confirms /
//!   files / processes / skills / config / frontend) reach
//!   through here so they stay headless-safe — `HeadlessApp`
//!   installs into the same TLS slot.
//!
//! Both slots are concrete pointers, not trait objects. Two earlier
//! DI-shaped seams retired in P10 (the `Host` trait in P10.3.8 and
//! the `UI_HOST` trait-object slot in P10.3.12 — see `P10.md`); they
//! shared the failure mode "DI table over field access with zero
//! behavior". The pattern: don't ship trait-object seams ahead of a
//! concrete second consumer.
//!
//! Safety contract
//! ---------------
//! `install_app_ptr` is the only way to set the slots; it borrows
//! `&mut TuiApp` briefly to capture the addresses, then returns a drop
//! guard that restores the previous slots on scope exit. Callers hold
//! their `&mut TuiApp` borrow across the subsequent Lua call, but that
//! borrow is *not accessed* while Lua runs — Rust is blocked on the
//! FFI call and the only way to touch TuiApp is through [`with_app`]
//! / `with_core`, each of which reborrows the raw pointer as a fresh
//! `&mut` for the duration of its closure. Because Lua is single-
//! threaded inside the TUI event loop and never re-enters a Rust
//! stack frame that is itself holding an active mutable borrow, the
//! reborrow is sole.
//!
//! `with_app` panics if the slot is unset — a defensive check for
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
    let app_ptr = NonNull::from(&mut *app);
    let old_app = APP.with(|cell| cell.replace(Some(app_ptr)));

    let core_guard = smelt_core::host::install_core_ptr(&mut app.core);
    AppPtrGuard {
        old_app,
        _core_guard: core_guard,
    }
}

/// Drop guard returned by [`install_app_ptr`]. Restores the previous
/// slot (usually `None`, but nested installs are supported).
pub(crate) struct AppPtrGuard {
    old_app: Option<NonNull<TuiApp>>,
    _core_guard: smelt_core::host::CorePtrGuard,
}

impl Drop for AppPtrGuard {
    fn drop(&mut self) {
        APP.with(|cell| cell.set(self.old_app));
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

/// `try_` variant of `with_core` that returns `None` instead of
/// panicking when no frontend is installed. Re-export for the
/// `tui/src/lua/api/session.rs` cross-tier helper that sits in tui
/// for historical reasons; native Host-tier bindings under
/// `core/src/lua/api/` reach `smelt_core::host::try_with_core`
/// directly.
pub(crate) fn try_with_core<R>(f: impl FnOnce(&mut smelt_core::Core) -> R) -> Option<R> {
    smelt_core::host::try_with_core(f)
}
