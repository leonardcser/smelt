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

use super::cells::Cells;
use super::core::Core;
use super::engine_client::EngineClient;
use super::headless_app::HeadlessApp;
use super::timers::Timers;
use crate::app::TuiApp;
use crate::lua::LuaRuntime;
use crate::session::Session;

/// Ui-agnostic accessors over the subsystems every frontend owns. See
/// the module docs for the split between this trait and `crate::ui::UiHost`.
///
/// `pub(crate)` so the trait visibility matches the most-private
/// return type today (`EngineClient`, `Cells`, `Timers`
/// are all `pub(crate)`). The trait stays internal to the `tui`
/// crate; that's where every consumer lives.
pub(crate) trait Host {
    fn clipboard(&mut self) -> &mut crate::core::Clipboard;
    fn cells(&mut self) -> &mut Cells;
    fn timers(&mut self) -> &mut Timers;
    fn lua(&mut self) -> &mut LuaRuntime;
    fn engine(&mut self) -> &mut EngineClient;
    fn session(&mut self) -> &mut Session;
}

impl Host for Core {
    fn clipboard(&mut self) -> &mut crate::core::Clipboard {
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
    fn engine(&mut self) -> &mut EngineClient {
        &mut self.engine
    }
    fn session(&mut self) -> &mut Session {
        &mut self.session
    }
}

/// Frontend impls delegate to the inner `Core`. The seam is "all
/// frontends carry a `Core`"; the impl is mechanical.
impl Host for TuiApp {
    fn clipboard(&mut self) -> &mut crate::core::Clipboard {
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
    fn engine(&mut self) -> &mut EngineClient {
        self.core.engine()
    }
    fn session(&mut self) -> &mut Session {
        self.core.session()
    }
}

impl Host for HeadlessApp {
    fn clipboard(&mut self) -> &mut crate::core::Clipboard {
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
    fn engine(&mut self) -> &mut EngineClient {
        self.core.engine()
    }
    fn session(&mut self) -> &mut Session {
        self.core.session()
    }
}

/// `UiHost` impl for `TuiApp` — delegates every method to the inner
/// `crate::ui::Ui`. The trait itself lives in `crate::ui::lib`; see its docs for
/// the split between `Host` (Ui-agnostic) and `UiHost` (compositor-
/// bearing). `HeadlessApp` deliberately does **not** impl `UiHost`;
/// UiHost-only Lua bindings raise a runtime error when invoked from
/// a headless context (wired in P2.b.5).
impl crate::ui::UiHost for TuiApp {
    fn ui(&mut self) -> &mut crate::ui::Ui {
        &mut self.ui
    }
    fn set_focus(&mut self, win: crate::ui::WinId) -> bool {
        self.ui.set_focus(win)
    }
    fn buf_create(&mut self, opts: crate::ui::buffer::BufCreateOpts) -> crate::ui::BufId {
        self.ui.buf_create(opts)
    }
    fn buf_mut(&mut self, id: crate::ui::BufId) -> Option<&mut crate::ui::Buffer> {
        self.ui.buf_mut(id)
    }
    fn win_open_split(
        &mut self,
        buf: crate::ui::BufId,
        config: crate::ui::SplitConfig,
    ) -> Option<crate::ui::WinId> {
        self.ui.win_open_split(buf, config)
    }
    fn win_close(&mut self, id: crate::ui::WinId) -> Vec<u64> {
        self.ui.win_close(id)
    }
    fn win_mut(&mut self, id: crate::ui::WinId) -> Option<&mut crate::ui::Window> {
        self.ui.win_mut(id)
    }
    fn overlay_open(&mut self, overlay: crate::ui::Overlay) -> crate::ui::OverlayId {
        self.ui.overlay_open(overlay)
    }
    fn viewport_for(&self, win: crate::ui::WinId) -> Option<crate::ui::WindowViewport> {
        self.ui.win(win).and_then(|w| w.viewport)
    }
    fn rows_for(&mut self, win: crate::ui::WinId) -> Option<Vec<String>> {
        if win == crate::ui::PROMPT_WIN {
            let usable = self.ui.win(win)?.viewport?.content_width as usize;
            let wrap = crate::content::prompt_wrap::PromptWrap::build(&self.input, usable);
            Some(wrap.rows)
        } else if win == crate::ui::TRANSCRIPT_WIN {
            let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
            Some((*rows).clone())
        } else {
            crate::ui::UiHost::rows_for(&mut self.ui, win)
        }
    }
    fn breaks_for(&mut self, win: crate::ui::WinId) -> Option<(Vec<usize>, Vec<usize>)> {
        if win == crate::ui::PROMPT_WIN {
            let usable = self.ui.win(win)?.viewport?.content_width as usize;
            let wrap = crate::content::prompt_wrap::PromptWrap::build(&self.input, usable);
            Some((wrap.soft_breaks, wrap.hard_breaks))
        } else if win == crate::ui::TRANSCRIPT_WIN {
            Some(self.transcript_line_breaks(self.core.config.settings.show_thinking))
        } else {
            crate::ui::UiHost::breaks_for(&mut self.ui, win)
        }
    }
}
