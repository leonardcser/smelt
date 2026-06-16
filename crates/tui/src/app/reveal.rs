use crate::app::TuiApp;
use crate::smelt_edit::{DocPosition, RowIndex, WinId};

#[derive(Clone, Copy, Debug)]
pub(crate) struct RevealOptions {
    pub(crate) top_padding: RowIndex,
    pub(crate) cursor: bool,
}

impl Default for RevealOptions {
    fn default() -> Self {
        Self {
            top_padding: 0,
            cursor: true,
        }
    }
}

impl TuiApp {
    /// Reveal a document position in `leaf`, optionally moving the cursor there
    /// and leaving fixed rows above it. Shared by search jumps and transcript
    /// affordances so both avoid placing their target under top-edge chrome.
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

        apply_top_padding(
            win,
            total_rows,
            position.row,
            viewport_rows,
            opts.top_padding,
        );
    }
}

fn apply_top_padding(
    win: &mut crate::smelt_edit::Window,
    total_rows: RowIndex,
    row: RowIndex,
    viewport_rows: u16,
    padding: RowIndex,
) {
    if padding == 0 || row == 0 {
        return;
    }
    let screen_row = row.saturating_sub(win.scroll_top());
    if screen_row >= padding {
        return;
    }
    win.pin_scroll(
        row.saturating_sub(padding)
            .min(max_scroll(total_rows, viewport_rows)),
    );
}

fn max_scroll(total_rows: RowIndex, viewport_rows: u16) -> RowIndex {
    total_rows.saturating_sub(viewport_rows.max(1) as RowIndex)
}
