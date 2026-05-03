//! `Host`: the Ui-agnostic surface every frontend exposes.
//!
//! `Host` is the seam between code that needs subsystems (tools, Lua
//! bindings, the engine drain) and code that does not need to know
//! whether it's running inside the interactive `TuiApp` or the
//! `HeadlessApp` worker. Both frontends own a `Core`; `Core` impls
//! `Host` and gets reborrowed through this trait at every site that
//! does not touch the compositor.
//!
//! The compositor-bearing surface lives in [`crate::ui::UiHost`] (defined in
//! a later sub-phase). `UiHost` does **not** extend `Host` — `ui` can
//! never reference tui-defined types. `TuiApp` impls both traits side
//! by side; `HeadlessApp` impls `Host` only and errors at runtime if a
//! UiHost-only Lua binding is invoked from a headless context.
//!
//! # Methods
//!
//! Each method returns a `&mut` reference to one of `Core`'s
//! subsystems. The names mirror today's `Core` field names so call
//! sites read the same whether they go through the trait or directly.
//!
//! `tools()` will be added when [`P2.a.10`] lands the `ToolRuntime`
//! carve-out — the `Core.tools` slot is vacant today because the
//! `engine::ToolDispatcher` trait shape lives in `P5.a`.
//!
//! [`P2.a.10`]: ../../../refactor/REFACTOR.md

use super::app_config::AppConfig;
use super::cells::Cells;
use super::confirms::Confirms;
use super::engine_client::EngineClient;
use super::headless_app::HeadlessApp;
use super::runtime::{Core, FrontendKind};
use super::timers::Timers;
use crate::session::Session;
use std::sync::Arc;

/// Ui-agnostic accessors over the subsystems every frontend owns. See
/// the module docs for the split between this trait and `crate::ui::UiHost`.
///
/// `pub(crate)` so the trait visibility matches the most-private
/// return type today (`EngineClient`, `Cells`, `Timers`
/// are all `pub(crate)`). The trait stays internal to the `tui`
/// crate; that's where every consumer lives.
pub trait Host {
    fn config(&self) -> &AppConfig;
    fn clipboard(&mut self) -> &mut crate::Clipboard;
    fn cells(&mut self) -> &mut Cells;
    fn timers(&mut self) -> &mut Timers;
    fn engine(&mut self) -> &mut EngineClient;
    fn session(&mut self) -> &mut Session;
    fn files(&mut self) -> &mut crate::fs::FileStateCache;
    fn processes(&mut self) -> &mut crate::process::ProcessRegistry;
    fn skills(&self) -> &Option<Arc<engine::SkillLoader>>;
    fn frontend(&self) -> FrontendKind;
    fn confirms(&mut self) -> &mut Confirms;
}

impl Host for Core {
    fn config(&self) -> &AppConfig {
        &self.config
    }
    fn clipboard(&mut self) -> &mut crate::Clipboard {
        &mut self.clipboard
    }
    fn cells(&mut self) -> &mut Cells {
        &mut self.cells
    }
    fn timers(&mut self) -> &mut Timers {
        &mut self.timers
    }
    fn engine(&mut self) -> &mut EngineClient {
        &mut self.engine
    }
    fn session(&mut self) -> &mut Session {
        &mut self.session
    }
    fn files(&mut self) -> &mut crate::fs::FileStateCache {
        &mut self.files
    }
    fn processes(&mut self) -> &mut crate::process::ProcessRegistry {
        &mut self.processes
    }
    fn skills(&self) -> &Option<Arc<engine::SkillLoader>> {
        &self.skills
    }
    fn frontend(&self) -> FrontendKind {
        self.frontend
    }
    fn confirms(&mut self) -> &mut Confirms {
        &mut self.confirms
    }
}

impl Host for HeadlessApp {
    fn config(&self) -> &AppConfig {
        self.core.config()
    }
    fn clipboard(&mut self) -> &mut crate::Clipboard {
        self.core.clipboard()
    }
    fn cells(&mut self) -> &mut Cells {
        self.core.cells()
    }
    fn timers(&mut self) -> &mut Timers {
        self.core.timers()
    }
    fn engine(&mut self) -> &mut EngineClient {
        self.core.engine()
    }
    fn session(&mut self) -> &mut Session {
        self.core.session()
    }
    fn files(&mut self) -> &mut crate::fs::FileStateCache {
        self.core.files()
    }
    fn processes(&mut self) -> &mut crate::process::ProcessRegistry {
        self.core.processes()
    }
    fn skills(&self) -> &Option<Arc<engine::SkillLoader>> {
        self.core.skills()
    }
    fn frontend(&self) -> FrontendKind {
        self.core.frontend()
    }
    fn confirms(&mut self) -> &mut Confirms {
        self.core.confirms()
    }
}

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

/// Borrow the installed `&mut dyn Host` for the duration of `f`.
/// Panics if called outside an [`install_core_ptr`] scope.
pub fn with_host<R>(f: impl FnOnce(&mut dyn Host) -> R) -> R {
    let ptr = CORE_PTR
        .with(|cell| *cell.borrow())
        .expect("with_host called outside Lua entry");
    // SAFETY: the pointer is set only by `install_core_ptr`, which
    // borrows `&mut Core` exclusively. The caller holds that borrow
    // across subsequent Lua calls but does not access it while Lua runs.
    unsafe { f(&mut *ptr) }
}

/// Variant that returns `None` if the pointer is unset instead of panicking.
pub fn try_with_host<R>(f: impl FnOnce(&mut dyn Host) -> R) -> Option<R> {
    let ptr = CORE_PTR.with(|cell| *cell.borrow())?;
    Some(unsafe { f(&mut *ptr) })
}
