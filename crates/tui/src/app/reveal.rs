use crate::app::TuiApp;
use crate::smelt_edit::{DocPosition, RowIndex, WinId};

#[derive(Clone, Copy, Debug)]
pub(crate) struct RevealOptions {
    pub(crate) top_padding: RowIndex,
    pub(crate) bottom_padding: RowIndex,
    pub(crate) cursor: bool,
}

impl Default for RevealOptions {
    fn default() -> Self {
        Self {
            top_padding: 0,
            bottom_padding: 0,
            cursor: true,
        }
    }
}

impl RevealOptions {
    pub(crate) fn avoid_edge_chrome(leaf: WinId) -> Self {
        let edge_padding = (leaf == crate::app::TRANSCRIPT_WIN) as RowIndex;
        Self {
            top_padding: edge_padding,
            bottom_padding: edge_padding,
            cursor: true,
        }
    }
}

impl TuiApp {
    /// Reveal a document position in `leaf`, optionally moving the cursor there
    /// and leaving fixed rows around it. Shared by search jumps and transcript
    /// affordances so both avoid placing their target under edge chrome.
    pub(crate) fn reveal_position(
        &mut self,
        leaf: WinId,
        mut position: DocPosition,
        opts: RevealOptions,
    ) {
        let Some((buf_id, viewport_rows, is_row_backed)) = self.ui.win(leaf).map(|w| {
            (
                w.buf,
                w.viewport.map(|v| v.rect.height).unwrap_or(1).max(1),
                w.has_materialized_rows(),
            )
        }) else {
            return;
        };
        let now = self.core.clock.instant_now();
        let (win, buf) = self.ui.win_and_buf_mut(leaf, buf_id);
        let (Some(win), Some(buf)) = (win, buf) else {
            return;
        };

        let total_rows = win.scroll_row_total(buf);
        position.row = position.row.min(total_rows.saturating_sub(1));

        if opts.cursor {
            if is_row_backed {
                win.execute_row_viewer_command(
                    buf,
                    crate::smelt_edit::ViewerCommand::GotoPosition(position),
                    viewport_rows,
                    now,
                );
            } else {
                let cpos = buf.byte_at_display_byte_pos(
                    crate::smelt_edit::row_to_usize(position.row),
                    position.byte_col,
                );
                win.set_cpos(cpos);
                win.resync(buf, viewport_rows);
            }
        } else {
            let scroll_top =
                crate::smelt_edit::scroll_to_show(win.scroll_top(), position.row, viewport_rows);
            win.pin_scroll(scroll_top.min(max_scroll(total_rows, viewport_rows)));
        }

        apply_reveal_padding(
            win,
            total_rows,
            position.row,
            viewport_rows,
            opts.top_padding,
            opts.bottom_padding,
        );
    }
}

fn apply_reveal_padding(
    win: &mut crate::smelt_edit::Window,
    total_rows: RowIndex,
    row: RowIndex,
    viewport_rows: u16,
    top_padding: RowIndex,
    bottom_padding: RowIndex,
) {
    if viewport_rows == 0 || (top_padding == 0 && bottom_padding == 0) {
        return;
    }

    let last_screen_row = viewport_rows.saturating_sub(1) as RowIndex;
    let min_screen_row = top_padding.min(last_screen_row);
    let max_screen_row = last_screen_row
        .saturating_sub(bottom_padding)
        .max(min_screen_row);
    let screen_row = row.saturating_sub(win.scroll_top());

    let target_scroll = if screen_row < min_screen_row {
        row.saturating_sub(min_screen_row)
    } else if screen_row > max_screen_row {
        row.saturating_sub(max_screen_row)
    } else {
        return;
    };

    win.pin_scroll(target_scroll.min(max_scroll(total_rows, viewport_rows)));
}

fn max_scroll(total_rows: RowIndex, viewport_rows: u16) -> RowIndex {
    total_rows.saturating_sub(viewport_rows.max(1) as RowIndex)
}
