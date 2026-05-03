//! `UiHost` impl for `TuiApp` — delegates every method to the inner
//! `crate::ui::Ui`. The trait itself lives in `crate::ui`; see its docs for
//! the split between `Host` (Ui-agnostic) and `UiHost` (compositor-
//! bearing). `HeadlessApp` deliberately does **not** impl `UiHost`;
//! UiHost-only Lua bindings raise a runtime error when invoked from
//! a headless context.

use crate::app::TuiApp;

impl smelt_core::Host for TuiApp {
    fn config(&self) -> &smelt_core::AppConfig {
        &self.core.config
    }
    fn clipboard(&mut self) -> &mut smelt_core::Clipboard {
        &mut self.core.clipboard
    }
    fn cells(&mut self) -> &mut smelt_core::Cells {
        &mut self.core.cells
    }
    fn timers(&mut self) -> &mut smelt_core::Timers {
        &mut self.core.timers
    }
    fn engine(&mut self) -> &mut smelt_core::EngineClient {
        &mut self.core.engine
    }
    fn session(&mut self) -> &mut smelt_core::Session {
        &mut self.core.session
    }
    fn files(&mut self) -> &mut smelt_core::fs::FileStateCache {
        &mut self.core.files
    }
    fn processes(&mut self) -> &mut smelt_core::process::ProcessRegistry {
        &mut self.core.processes
    }
    fn skills(&self) -> &Option<std::sync::Arc<engine::SkillLoader>> {
        &self.core.skills
    }
    fn frontend(&self) -> smelt_core::runtime::FrontendKind {
        self.core.frontend
    }
    fn confirms(&mut self) -> &mut smelt_core::confirms::Confirms {
        &mut self.core.confirms
    }
}

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
