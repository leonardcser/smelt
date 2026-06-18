//! `UiHost` impl for `TuiApp`. Delegates window resource operations to
//! `crate::smelt_edit::Ui`; document operations use the app document resolver.

use crate::app::TuiApp;

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
        self.with_display_document_for_win(win, |document| {
            document.materialize(start..start.saturating_add(count))
        })
    }

    fn document_total_rows(
        &mut self,
        win: crate::smelt_edit::WinId,
    ) -> Option<crate::smelt_edit::RowIndex> {
        self.with_display_document_for_win(win, |document| document.snapshot().total_rows)
    }

    fn copy_document_range(
        &mut self,
        win: crate::smelt_edit::WinId,
        range: crate::smelt_edit::DocRange,
    ) -> Option<crate::smelt_edit::CopyOutput> {
        self.with_display_document_for_win(win, |document| {
            document.copy_range(crate::smelt_edit::TextRange::Rows(range))
        })
        .flatten()
    }
}
