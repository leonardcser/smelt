//! `UiHost` impl for `TuiApp`. Delegates to `crate::smelt_edit::Ui`; overrides
//! `rows_for`/`breaks_for` for the prompt and transcript windows.

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
    fn rows_for(&mut self, win: crate::smelt_edit::WinId) -> Option<Vec<String>> {
        if win == crate::app::PROMPT_WIN {
            let buf_id = self.ui.win(self.well_known.prompt)?.buf;
            let buf = self.ui.buf(buf_id)?;
            Some(buf.lines().to_vec())
        } else if win == crate::app::TRANSCRIPT_WIN {
            let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
            Some((*rows).clone())
        } else {
            crate::smelt_edit::UiHost::rows_for(&mut self.ui, win)
        }
    }
    fn breaks_for(&mut self, win: crate::smelt_edit::WinId) -> Option<(Vec<usize>, Vec<usize>)> {
        if win == crate::app::PROMPT_WIN {
            let buf_id = self.ui.win(self.well_known.prompt)?.buf;
            let buf = self.ui.buf(buf_id)?;
            Some((
                Vec::new(),
                crate::smelt_edit::hard_breaks_for_text(buf.source()),
            ))
        } else if win == crate::app::TRANSCRIPT_WIN {
            Some(self.transcript_line_breaks(self.core.config.settings.show_thinking))
        } else {
            crate::smelt_edit::UiHost::breaks_for(&mut self.ui, win)
        }
    }

    fn rows_for_range(
        &mut self,
        win: crate::smelt_edit::WinId,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> Option<Vec<String>> {
        if win == crate::app::TRANSCRIPT_WIN {
            Some(self.transcript_display_rows_range(
                self.core.config.settings.show_thinking,
                start,
                count,
            ))
        } else if win == crate::app::PROMPT_WIN {
            let buf_id = self.ui.win(self.well_known.prompt)?.buf;
            let buf = self.ui.buf(buf_id)?;
            let rows = buf.lines();
            let start_idx = crate::smelt_edit::row_to_usize(start).min(rows.len());
            let end = crate::smelt_edit::row_to_usize(start.saturating_add(count)).min(rows.len());
            Some(rows[start_idx..end].to_vec())
        } else {
            crate::smelt_edit::UiHost::rows_for_range(&mut self.ui, win, start, count)
        }
    }

    fn breaks_for_range(
        &mut self,
        win: crate::smelt_edit::WinId,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> Option<(Vec<usize>, Vec<usize>)> {
        if win == crate::app::TRANSCRIPT_WIN {
            Some(self.transcript_line_breaks_range(
                self.core.config.settings.show_thinking,
                start,
                count,
            ))
        } else {
            crate::smelt_edit::UiHost::breaks_for_range(&mut self.ui, win, start, count)
        }
    }

    fn virtual_total_rows(
        &mut self,
        win: crate::smelt_edit::WinId,
    ) -> Option<crate::smelt_edit::RowIndex> {
        if win == crate::app::TRANSCRIPT_WIN {
            let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
            Some(rows.len() as crate::smelt_edit::RowIndex)
        } else {
            crate::smelt_edit::UiHost::virtual_total_rows(&mut self.ui, win)
        }
    }

    fn copy_virtual_range(
        &mut self,
        win: crate::smelt_edit::WinId,
        range: crate::smelt_edit::DocRange,
    ) -> Option<crate::smelt_edit::CopyOutput> {
        if win == crate::app::TRANSCRIPT_WIN {
            let tw = self.transcript_width() as u16;
            let theme = self.ui.theme().clone();
            Some(self.transcript.copy_range(
                tw,
                self.core.config.settings.show_thinking,
                &theme,
                range,
            ))
        } else {
            crate::smelt_edit::UiHost::copy_virtual_range(&mut self.ui, win, range)
        }
    }
}
