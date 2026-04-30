//! `Host`: the Ui-agnostic surface every frontend exposes.
//!
//! `Host` is the seam between code that needs subsystems (tools, Lua
//! bindings, the engine drain) and code that does not need to know
//! whether it's running inside the interactive `TuiApp` or the
//! `HeadlessApp` worker. Both frontends own a `Core`; `Core` impls
//! `Host` and gets reborrowed through this trait at every site that
//! does not touch the compositor.
//!
//! The compositor-bearing surface lives in [`ui::UiHost`] (defined in
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

use super::cells::Cells;
use super::confirms::Confirms;
use super::core::Core;
use super::engine_bridge::EngineBridge;
use super::headless_app::HeadlessApp;
use super::timers::Timers;
use super::TuiApp;
use crate::lua::LuaRuntime;
use crate::session::Session;

/// Ui-agnostic accessors over the subsystems every frontend owns. See
/// the module docs for the split between this trait and `ui::UiHost`.
///
/// `pub(crate)` so the trait visibility matches the most-private
/// return type today (`Confirms`, `EngineBridge`, `Cells`, `Timers`
/// are all `pub(crate)`). The trait stays internal to the `tui`
/// crate; that's where every consumer lives.
pub(crate) trait Host {
    fn clipboard(&mut self) -> &mut ui::Clipboard;
    fn cells(&mut self) -> &mut Cells;
    fn timers(&mut self) -> &mut Timers;
    fn lua(&mut self) -> &mut LuaRuntime;
    fn engine(&mut self) -> &mut EngineBridge;
    fn session(&mut self) -> &mut Session;
    fn confirms(&mut self) -> &mut Confirms;
}

impl Host for Core {
    fn clipboard(&mut self) -> &mut ui::Clipboard {
        &mut self.clipboard
    }
    fn cells(&mut self) -> &mut Cells {
        &mut self.cells
    }
    fn timers(&mut self) -> &mut Timers {
        &mut self.timers
    }
    fn lua(&mut self) -> &mut LuaRuntime {
        &mut self.lua
    }
    fn engine(&mut self) -> &mut EngineBridge {
        &mut self.engine
    }
    fn session(&mut self) -> &mut Session {
        &mut self.session
    }
    fn confirms(&mut self) -> &mut Confirms {
        &mut self.confirms
    }
}

/// Frontend impls delegate to the inner `Core`. The seam is "all
/// frontends carry a `Core`"; the impl is mechanical.
impl Host for TuiApp {
    fn clipboard(&mut self) -> &mut ui::Clipboard {
        self.core.clipboard()
    }
    fn cells(&mut self) -> &mut Cells {
        self.core.cells()
    }
    fn timers(&mut self) -> &mut Timers {
        self.core.timers()
    }
    fn lua(&mut self) -> &mut LuaRuntime {
        self.core.lua()
    }
    fn engine(&mut self) -> &mut EngineBridge {
        self.core.engine()
    }
    fn session(&mut self) -> &mut Session {
        self.core.session()
    }
    fn confirms(&mut self) -> &mut Confirms {
        self.core.confirms()
    }
}

impl Host for HeadlessApp {
    fn clipboard(&mut self) -> &mut ui::Clipboard {
        self.core.clipboard()
    }
    fn cells(&mut self) -> &mut Cells {
        self.core.cells()
    }
    fn timers(&mut self) -> &mut Timers {
        self.core.timers()
    }
    fn lua(&mut self) -> &mut LuaRuntime {
        self.core.lua()
    }
    fn engine(&mut self) -> &mut EngineBridge {
        self.core.engine()
    }
    fn session(&mut self) -> &mut Session {
        self.core.session()
    }
    fn confirms(&mut self) -> &mut Confirms {
        self.core.confirms()
    }
}
