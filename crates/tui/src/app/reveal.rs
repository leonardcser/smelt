use serde_json::json;

use crate::app::transcript_scroll_trace::TranscriptScrollIntent;
use crate::app::TuiApp;
use crate::smelt_edit::{DocPosition, RowIndex, WinId};

#[derive(Clone, Copy, Debug)]
pub(crate) enum RevealScrollIntent {
    Position,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RevealOptions {
    pub(crate) top_padding: RowIndex,
    pub(crate) bottom_padding: RowIndex,
    pub(crate) cursor: bool,
    pub(crate) transcript_scroll_intent: Option<RevealScrollIntent>,
}

impl Default for RevealOptions {
    fn default() -> Self {
        Self {
            top_padding: 0,
            bottom_padding: 0,
            cursor: true,
            transcript_scroll_intent: None,
        }
    }
}

impl RevealOptions {
    pub(crate) fn avoid_edge_chrome(leaf: WinId) -> Self {
        let edge_padding = (leaf == crate::app::TRANSCRIPT_WIN) as RowIndex;
        Self {
            top_padding: edge_padding,
            bottom_padding: edge_padding,
            ..Self::default()
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
        let Some((buf_id, viewport_rows, viewport_width, is_row_backed)) =
            self.ui.win(leaf).map(|w| {
                (
                    w.buf,
                    w.viewport.map(|v| v.rect.height).unwrap_or(1).max(1),
                    w.viewport.map(|v| v.content_width).unwrap_or(1).max(1),
                    w.has_materialized_rows(),
                )
            })
        else {
            return;
        };
        let trace_reveal =
            leaf == crate::app::TRANSCRIPT_WIN && self.transcript.scroll_trace_enabled();
        if trace_reveal {
            let anchor =
                self.transcript
                    .trace_anchor_at_row(&self.lua, viewport_width, position.row);
            self.transcript.record_scroll_trace_event(
                "reveal_before",
                json!({
                    "position_row": position.row,
                    "position_byte_col": position.byte_col,
                    "position_anchor": format!("{:?}", anchor),
                    "cursor": opts.cursor,
                    "top_padding": opts.top_padding,
                    "bottom_padding": opts.bottom_padding,
                    "has_scroll_intent": opts.transcript_scroll_intent.is_some(),
                    "window_scroll_before": self.transcript_scroll_top(),
                    "viewport_rows": viewport_rows,
                    "viewport_width": viewport_width,
                    "is_row_backed": is_row_backed,
                }),
            );
        }
        let trace_before = if leaf == crate::app::TRANSCRIPT_WIN {
            opts.transcript_scroll_intent
                .map(|intent| (intent, self.transcript_scroll_top()))
        } else {
            None
        };
        let now = self.core.clock.instant_now();

        {
            let (win, buf) = self.ui.win_and_buf_mut(leaf, buf_id);
            let (Some(win), Some(buf)) = (win, buf) else {
                return;
            };

            let total_rows = win.scroll_row_total(buf);
            position.row = position.row.min(total_rows.saturating_sub(1));

            if opts.cursor {
                if is_row_backed {
                    win.execute_document_view_command(
                        buf,
                        crate::smelt_edit::DocumentCommand::GotoPosition(position),
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
                let scroll_top = crate::smelt_edit::scroll_to_show(
                    win.scroll_top(),
                    position.row,
                    viewport_rows,
                );
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

        if let Some((intent, window_scroll_before)) = trace_before {
            let (label, scroll_intent) = match intent {
                RevealScrollIntent::Position => {
                    let anchor = self.transcript.trace_anchor_at_row(
                        &self.lua,
                        viewport_width,
                        position.row,
                    );
                    ("reveal", TranscriptScrollIntent::ExactContentAnchor(anchor))
                }
            };
            self.record_transcript_scroll_intent(label, scroll_intent, window_scroll_before);
        }

        if trace_reveal {
            let anchor =
                self.transcript
                    .trace_anchor_at_row(&self.lua, viewport_width, position.row);
            self.transcript.record_scroll_trace_event(
                "reveal_after",
                json!({
                    "position_row": position.row,
                    "position_byte_col": position.byte_col,
                    "position_anchor": format!("{:?}", anchor),
                    "window_scroll_after": self.transcript_scroll_top(),
                }),
            );
        }
    }
}

pub(crate) fn target_screen_row_for_reveal(
    scroll_top: RowIndex,
    viewport_rows: u16,
    row: RowIndex,
    opts: RevealOptions,
) -> RowIndex {
    let last_screen_row = viewport_rows.saturating_sub(1) as RowIndex;
    let min_screen_row = opts.top_padding.min(last_screen_row);
    let max_screen_row = last_screen_row
        .saturating_sub(opts.bottom_padding)
        .max(min_screen_row);
    if row < scroll_top.saturating_add(min_screen_row) {
        min_screen_row
    } else if row > scroll_top.saturating_add(max_screen_row) {
        max_screen_row
    } else {
        row.saturating_sub(scroll_top)
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

    let target_screen_row = target_screen_row_for_reveal(
        win.scroll_top(),
        viewport_rows,
        row,
        RevealOptions {
            top_padding,
            bottom_padding,
            ..RevealOptions::default()
        },
    );
    let target_scroll = row.saturating_sub(target_screen_row);
    if target_scroll != win.scroll_top() {
        win.pin_scroll(target_scroll.min(max_scroll(total_rows, viewport_rows)));
    }
}

fn max_scroll(total_rows: RowIndex, viewport_rows: u16) -> RowIndex {
    total_rows.saturating_sub(viewport_rows.max(1) as RowIndex)
}
