//! `UiHost` impl for `TuiApp`. Delegates window resource operations to
//! `crate::smelt_edit::Ui`.

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
}
