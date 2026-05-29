//! `UiHost` impl for `TuiApp`. Delegates to `crate::smelt_term::Ui`; overrides
//! `rows_for`/`breaks_for` for the prompt and transcript windows.

use crate::app::TuiApp;

impl crate::smelt_term::UiHost for TuiApp {
    fn ui(&mut self) -> &mut crate::smelt_term::Ui {
        &mut self.ui
    }
    fn set_focus(&mut self, win: crate::smelt_term::WinId) -> bool {
        self.ui.set_focus(win)
    }
    fn buf_create(&mut self, opts: crate::smelt_term::BufCreateOpts) -> crate::smelt_term::BufId {
        self.ui.buf_create(opts)
    }
    fn buf_mut(&mut self, id: crate::smelt_term::BufId) -> Option<&mut crate::smelt_term::Buffer> {
        self.ui.buf_mut(id)
    }
    fn win_open_split(
        &mut self,
        buf: crate::smelt_term::BufId,
        config: crate::smelt_term::SplitConfig,
    ) -> Option<crate::smelt_term::WinId> {
        self.ui.win_open_split(buf, config)
    }
    fn win_close(&mut self, id: crate::smelt_term::WinId) -> Vec<u64> {
        self.ui.win_close(id)
    }
    fn win_mut(&mut self, id: crate::smelt_term::WinId) -> Option<&mut crate::smelt_term::Window> {
        self.ui.win_mut(id)
    }
    fn overlay_open(
        &mut self,
        overlay: crate::smelt_term::Overlay,
    ) -> crate::smelt_term::OverlayId {
        self.ui.overlay_open(overlay)
    }
    fn viewport_for(
        &self,
        win: crate::smelt_term::WinId,
    ) -> Option<crate::smelt_term::WindowViewport> {
        self.ui.win(win).and_then(|w| w.viewport)
    }
    fn rows_for(&mut self, win: crate::smelt_term::WinId) -> Option<Vec<String>> {
        if win == crate::app::PROMPT_WIN {
            let buf_id = self.ui.win(self.well_known.prompt)?.buf;
            let buf = self.ui.buf(buf_id)?;
            Some(buf.lines().to_vec())
        } else if win == crate::app::TRANSCRIPT_WIN {
            let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
            Some((*rows).clone())
        } else {
            crate::smelt_term::UiHost::rows_for(&mut self.ui, win)
        }
    }
    fn breaks_for(&mut self, win: crate::smelt_term::WinId) -> Option<(Vec<usize>, Vec<usize>)> {
        if win == crate::app::PROMPT_WIN {
            let buf_id = self.ui.win(self.well_known.prompt)?.buf;
            let buf = self.ui.buf(buf_id)?;
            Some((
                Vec::new(),
                crate::smelt_term::hard_breaks_for_text(buf.source()),
            ))
        } else if win == crate::app::TRANSCRIPT_WIN {
            Some(self.transcript_line_breaks(self.core.config.settings.show_thinking))
        } else {
            crate::smelt_term::UiHost::breaks_for(&mut self.ui, win)
        }
    }
}
