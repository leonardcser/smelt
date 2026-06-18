//! `UiHost` impl for `TuiApp`. Delegates to `crate::smelt_edit::Ui`; overrides
//! row-range access for prompt and transcript windows, with explicitly named
//! full-document fallbacks for export/debug-style callers.

use crate::app::TuiApp;

impl TuiApp {
    fn document_handle_for_win(
        &self,
        win: crate::smelt_edit::WinId,
    ) -> Option<crate::smelt_edit::DocumentHandle> {
        self.ui.win(win).and_then(|win| win.document_handle())
    }
}

impl crate::smelt_edit::UiHost for TuiApp {
    fn ui(&mut self) -> &mut crate::smelt_edit::Ui {
        &mut self.ui
    }
    fn set_focus(&mut self, win: crate::smelt_edit::WinId) -> bool {
        self.ui.set_focus(win)
    }
    fn buf_create(&mut self, opts: crate::smelt_edit::BufCreateOpts) -> crate::smelt_edit::BufId {
        self.ui.buf_create(opts)
    }
    fn buf_mut(&mut self, id: crate::smelt_edit::BufId) -> Option<&mut crate::smelt_edit::Buffer> {
        self.ui.buf_mut(id)
    }
    fn win_open_split(
        &mut self,
        buf: crate::smelt_edit::BufId,
        config: crate::smelt_edit::SplitConfig,
    ) -> Option<crate::smelt_edit::WinId> {
        self.ui.win_open_split(buf, config)
    }
    fn win_close(&mut self, id: crate::smelt_edit::WinId) -> Vec<u64> {
        self.ui.win_close(id)
    }
    fn win_mut(&mut self, id: crate::smelt_edit::WinId) -> Option<&mut crate::smelt_edit::Window> {
        self.ui.win_mut(id)
    }
    fn overlay_open(
        &mut self,
        overlay: crate::smelt_edit::Overlay,
    ) -> crate::smelt_edit::OverlayId {
        self.ui.overlay_open(overlay)
    }
    fn viewport_for(
        &self,
        win: crate::smelt_edit::WinId,
    ) -> Option<crate::smelt_edit::WindowViewport> {
        self.ui.win(win).and_then(|w| w.viewport)
    }
    fn display_rows_for_range(
        &mut self,
        win: crate::smelt_edit::WinId,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> Option<crate::smelt_edit::DisplayRows> {
        match self.document_handle_for_win(win) {
            Some(crate::app::TRANSCRIPT_DOCUMENT) => {
                Some(self.transcript_rows_and_breaks_range(start, count))
            }
            _ => crate::smelt_edit::UiHost::display_rows_for_range(&mut self.ui, win, start, count),
        }
    }

    fn document_total_rows(
        &mut self,
        win: crate::smelt_edit::WinId,
    ) -> Option<crate::smelt_edit::RowIndex> {
        match self.document_handle_for_win(win) {
            Some(crate::app::TRANSCRIPT_DOCUMENT) => Some(self.transcript_total_rows()),
            _ => crate::smelt_edit::UiHost::document_total_rows(&mut self.ui, win),
        }
    }

    fn copy_document_range(
        &mut self,
        win: crate::smelt_edit::WinId,
        range: crate::smelt_edit::DocRange,
    ) -> Option<crate::smelt_edit::CopyOutput> {
        match self.document_handle_for_win(win) {
            Some(crate::app::TRANSCRIPT_DOCUMENT) => self.transcript_copy_range(range),
            _ => crate::smelt_edit::UiHost::copy_document_range(&mut self.ui, win, range),
        }
    }
}
