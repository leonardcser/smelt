use super::*;
use smelt_buffer::kill_ring::YANK_FLASH_DURATION;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerCommand {
    MoveRows(isize),
    PageRows(isize),
    ScrollRows(isize),
    BufferStart,
    BufferEnd,
    GotoRow(RowIndex),
    GotoPosition(DocPosition),
    LineStart,
    LineEnd,
    WordForward(RowIndex),
    WordBackward(RowIndex),
    WordEnd(RowIndex),
    StartVisual,
    StartVisualLine,
    YankSelection,
    YankSelectionLinewise,
    YankLines(RowIndex),
    CenterScroll,
    PanColumns(isize),
    ClearSelection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtualYankFlash {
    pub range: DocRange,
    pub until: Instant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtualRowsState {
    pub materialized: MaterializedRows,
    pub cursor: DocPosition,
    pub selection_anchor: Option<DocPosition>,
    pub drag_endpoint: Option<DocPosition>,
    pub yank_flash: Option<VirtualYankFlash>,
}

impl Window {
    pub fn set_virtual_rows(&mut self, row_base: RowIndex, total_rows: RowIndex) {
        debug_assert!(
            total_rows >= row_base,
            "virtual row total {total_rows} must cover row_base {row_base}"
        );
        let materialized = MaterializedRows {
            clamped_scroll: self.scroll_top,
            row_base,
            total_rows,
            materialized_rows: self
                .virtual_rows
                .map(|state| state.materialized.materialized_rows)
                .unwrap_or(0),
        };
        self.set_virtual_materialized_rows(materialized);
    }

    /// Configure the backing buffer as a materialized slice of a larger virtual
    /// row space. Clears virtual mode only when the slice covers the whole row
    /// space; a top slice with more rows below must still keep `total_rows`.
    pub fn set_materialized_rows(
        &mut self,
        row_base: RowIndex,
        materialized_rows: RowIndex,
        total_rows: RowIndex,
    ) {
        self.apply_materialized_rows(MaterializedRows {
            clamped_scroll: self.scroll_top,
            row_base,
            total_rows,
            materialized_rows,
        });
    }

    pub fn apply_materialized_rows(&mut self, rows: MaterializedRows) {
        if rows.row_base == 0 && rows.materialized_rows >= rows.total_rows {
            self.clear_virtual_rows();
        } else {
            self.set_virtual_materialized_rows(rows);
        }
    }

    fn set_virtual_materialized_rows(&mut self, rows: MaterializedRows) {
        let cursor = self
            .virtual_rows
            .map(|state| state.cursor)
            .unwrap_or_else(|| DocPosition {
                row: rows.absolute_row(self.cursor_row),
                byte_col: self.cursor_col as usize,
            });
        let mut state = self.virtual_rows.unwrap_or_default();
        state.materialized = rows;
        state.cursor = DocPosition {
            row: cursor.row.min(rows.total_rows.saturating_sub(1)),
            byte_col: cursor.byte_col,
        };
        self.virtual_rows = Some(state);
    }

    pub fn clear_virtual_rows(&mut self) {
        self.virtual_rows = None;
    }

    pub fn scroll_row_total(&self, buf: &Buffer) -> RowIndex {
        self.virtual_rows
            .map(|state| state.materialized.total_rows)
            .unwrap_or_else(|| self.visual_row_total(buf))
    }

    pub fn is_virtual_rows(&self) -> bool {
        self.virtual_rows.is_some()
    }

    pub fn virtual_cursor(&self) -> Option<DocPosition> {
        self.virtual_rows.map(|state| state.cursor)
    }

    pub fn virtual_selection_anchor_active(&self) -> bool {
        self.virtual_rows
            .map(|state| state.selection_anchor.is_some())
            .unwrap_or(false)
    }

    pub fn virtual_selection_range(&self, now: Instant) -> Option<DocRange> {
        let state = self.virtual_rows?;
        if let Some(flash) = state.yank_flash.filter(|flash| now < flash.until) {
            return Some(flash.range);
        }
        state.selection_anchor.map(|anchor| {
            if matches!(self.vim_mode, VimMode::VisualLine) {
                let start = anchor.row.min(state.cursor.row);
                let end = anchor.row.max(state.cursor.row).saturating_add(1);
                DocRange {
                    start: DocPosition {
                        row: start,
                        byte_col: 0,
                    },
                    end: DocPosition {
                        row: end,
                        byte_col: 0,
                    },
                }
            } else {
                order_doc_range(anchor, state.cursor)
            }
        })
    }

    pub fn virtual_yank_flash_until(&self) -> Option<Instant> {
        self.virtual_rows
            .and_then(|state| state.yank_flash.map(|flash| flash.until))
    }

    pub fn clear_expired_virtual_yank_flash(&mut self, now: Instant) {
        let Some(mut state) = self.virtual_rows else {
            return;
        };
        if state.yank_flash.is_some_and(|flash| now >= flash.until) {
            state.yank_flash = None;
            self.virtual_rows = Some(state);
        }
    }

    pub fn sync_virtual_cursor_to_local(&mut self, buf: &Buffer, viewport_rows: u16) {
        let Some(state) = self.virtual_rows else {
            return;
        };
        if buf.lines().is_empty() {
            self.reset_cursor();
            return;
        }
        let local_row = state
            .materialized
            .local_row(state.cursor.row)
            .min(self.visual_row_total(buf).saturating_sub(1));
        self.cpos = self.cpos_at_visual(buf, row_to_usize(local_row), state.cursor.byte_col);
        let (row, col) = self.cursor_visual(buf, self.cpos);
        self.cursor_row = row;
        self.cursor_col = col;
        self.curswant = Some(state.cursor.byte_col);
        if viewport_rows > 0 {
            let viewport_cols = self.viewport.map(|v| v.content_width).unwrap_or(0);
            self.keep_cursor_visible(
                buf,
                state.materialized.total_rows,
                viewport_rows,
                viewport_cols,
            );
        }
    }

    pub fn handle_virtual_mouse(
        &mut self,
        buf: &Buffer,
        event: MouseEvent,
        ctx: MouseCtx,
        now: Instant,
    ) -> (Status, Option<DocRange>) {
        let Some(mut state) = self.virtual_rows else {
            return (Status::Ignored, None);
        };
        if ctx.viewport.rect.height == 0
            || state.materialized.total_rows == 0
            || buf.lines().is_empty()
        {
            return (Status::Consumed, None);
        }
        let pos = self.virtual_doc_pos_at_mouse(buf, event, ctx.viewport);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                state.cursor = pos;
                state.drag_endpoint = Some(pos);
                state.selection_anchor = None;
                state.yank_flash = None;
                self.virtual_rows = Some(state);
                self.pin_scroll(ctx.viewport.scroll_top);
                (Status::Capture, None)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if state.selection_anchor.is_none() {
                    state.selection_anchor = state.drag_endpoint.or(Some(state.cursor));
                }
                state.cursor = pos;
                state.drag_endpoint = Some(pos);
                state.yank_flash = None;
                self.virtual_rows = Some(state);
                (Status::Consumed, None)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                state.cursor = pos;
                let copy = state
                    .selection_anchor
                    .map(|anchor| order_doc_range(anchor, pos))
                    .filter(|range| {
                        (range.start.row, range.start.byte_col)
                            < (range.end.row, range.end.byte_col)
                    });
                state.drag_endpoint = None;
                state.selection_anchor = None;
                state.yank_flash = copy.map(|range| VirtualYankFlash {
                    range,
                    until: now + YANK_FLASH_DURATION,
                });
                self.virtual_rows = Some(state);
                (Status::Consumed, copy)
            }
            _ => (Status::Ignored, None),
        }
    }

    fn virtual_doc_pos_at_mouse(
        &mut self,
        buf: &Buffer,
        event: MouseEvent,
        viewport: WindowViewport,
    ) -> DocPosition {
        let Some(state) = self.virtual_rows else {
            return DocPosition::default();
        };
        let height = viewport.rect.height.max(1);
        let mut scroll_top = viewport.scroll_top;
        if event.row < viewport.rect.top {
            scroll_top = scroll_top.saturating_sub(1);
        } else if event.row >= viewport.rect.bottom() {
            scroll_top = scroll_top.saturating_add(1).min(
                state
                    .materialized
                    .total_rows
                    .saturating_sub(height as RowIndex),
            );
        }
        self.pin_scroll(scroll_top);
        let rel_row = event
            .row
            .saturating_sub(viewport.rect.top)
            .min(height.saturating_sub(1));
        let rel_col = event
            .column
            .saturating_sub(viewport.rect.left)
            .saturating_sub(viewport.gutter_width)
            .saturating_sub(self.config.gutters.pad_left)
            .min(viewport.content_width.saturating_sub(1));
        let row = self
            .scroll_top
            .saturating_add(rel_row as RowIndex)
            .min(state.materialized.total_rows.saturating_sub(1));
        let local = state
            .materialized
            .local_row(row)
            .min(self.visual_row_total(buf).saturating_sub(1));
        let cpos = self.cpos_at_visual(
            buf,
            row_to_usize(local),
            rel_col as usize + self.scroll_left as usize,
        );
        let (_, byte_col) = buf.display_byte_pos(cpos);
        DocPosition { row, byte_col }
    }

    pub fn execute_virtual_viewer_command(
        &mut self,
        buf: &Buffer,
        command: ViewerCommand,
        viewport_rows: u16,
        now: Instant,
    ) -> Option<DocRange> {
        let mut state = self.virtual_rows?;
        let total_rows = state.materialized.total_rows;
        if total_rows == 0 || viewport_rows == 0 {
            return None;
        }
        let current = state.cursor;
        let mut next = current;
        let mut copy = None;
        match command {
            ViewerCommand::MoveRows(delta) => {
                next.row = add_signed_row(current.row, delta).min(total_rows.saturating_sub(1));
            }
            ViewerCommand::PageRows(delta) => {
                let rows = (viewport_rows as isize).saturating_mul(delta);
                next.row = add_signed_row(current.row, rows).min(total_rows.saturating_sub(1));
            }
            ViewerCommand::ScrollRows(delta) => {
                self.pan_by_lines(buf, delta, viewport_rows);
                let screen_row = self.cursor_screen_row(viewport_rows).unwrap_or(0) as RowIndex;
                next.row = self
                    .scroll_top
                    .saturating_add(screen_row)
                    .min(total_rows.saturating_sub(1));
            }
            ViewerCommand::BufferStart => {
                next.row = 0;
                next.byte_col = 0;
            }
            ViewerCommand::BufferEnd => {
                next.row = total_rows.saturating_sub(1);
            }
            ViewerCommand::GotoRow(row) => {
                next.row = row.min(total_rows.saturating_sub(1));
            }
            ViewerCommand::GotoPosition(pos) => {
                next.row = pos.row.min(total_rows.saturating_sub(1));
                next.byte_col = pos.byte_col;
            }
            ViewerCommand::LineStart => next.byte_col = 0,
            ViewerCommand::LineEnd => {
                let local = state.materialized.local_row(current.row);
                next.byte_col = buf
                    .get_line(row_to_usize(local))
                    .map(|line| line.len())
                    .unwrap_or(0);
            }
            ViewerCommand::StartVisual => {
                state.selection_anchor = Some(current);
            }
            ViewerCommand::StartVisualLine => {
                state.selection_anchor = Some(DocPosition {
                    row: current.row,
                    byte_col: 0,
                });
            }
            ViewerCommand::YankSelection => {
                if let Some(anchor) = state.selection_anchor {
                    copy = Some(order_doc_range(anchor, current));
                    state.yank_flash = copy.map(|range| VirtualYankFlash {
                        range,
                        until: now + YANK_FLASH_DURATION,
                    });
                    state.selection_anchor = None;
                }
            }
            ViewerCommand::YankSelectionLinewise => {
                if let Some(anchor) = state.selection_anchor {
                    let start = anchor.row.min(current.row);
                    let end = anchor
                        .row
                        .max(current.row)
                        .saturating_add(1)
                        .min(total_rows);
                    copy = Some(DocRange {
                        start: DocPosition {
                            row: start,
                            byte_col: 0,
                        },
                        end: DocPosition {
                            row: end,
                            byte_col: 0,
                        },
                    });
                    state.yank_flash = copy.map(|range| VirtualYankFlash {
                        range,
                        until: now + YANK_FLASH_DURATION,
                    });
                    state.selection_anchor = None;
                }
            }
            ViewerCommand::YankLines(count) => {
                let end_row = current.row.saturating_add(count.max(1)).min(total_rows);
                copy = Some(DocRange {
                    start: DocPosition {
                        row: current.row,
                        byte_col: 0,
                    },
                    end: DocPosition {
                        row: end_row,
                        byte_col: 0,
                    },
                });
                state.yank_flash = copy.map(|range| VirtualYankFlash {
                    range,
                    until: now + YANK_FLASH_DURATION,
                });
            }
            ViewerCommand::CenterScroll => {
                let half = viewport_rows as RowIndex / 2;
                let max_scroll = total_rows.saturating_sub(viewport_rows as RowIndex);
                self.pin_scroll(current.row.saturating_sub(half).min(max_scroll));
            }
            ViewerCommand::PanColumns(delta) => {
                let viewport_cols = self.viewport.map(|v| v.content_width).unwrap_or(0);
                self.pan_by_columns(delta, viewport_cols);
            }
            ViewerCommand::WordForward(count) => {
                for _ in 0..count.max(1) {
                    let local = state.materialized.local_row(next.row);
                    if let Some(line) = buf.get_line(row_to_usize(local)) {
                        let col =
                            text::word_forward_pos(line, next.byte_col, text::CharClass::Word);
                        if col < line.len() || next.row + 1 >= total_rows {
                            next.byte_col = col;
                        } else {
                            next.row = next.row.saturating_add(1).min(total_rows.saturating_sub(1));
                            next.byte_col = 0;
                        }
                    }
                }
            }
            ViewerCommand::WordEnd(count) => {
                for _ in 0..count.max(1) {
                    let local = state.materialized.local_row(next.row);
                    if let Some(line) = buf.get_line(row_to_usize(local)) {
                        let col = text::word_end_pos(line, next.byte_col, text::CharClass::Word);
                        if col > next.byte_col || next.row + 1 >= total_rows {
                            next.byte_col = col;
                        } else {
                            next.row = next.row.saturating_add(1).min(total_rows.saturating_sub(1));
                            next.byte_col = 0;
                        }
                    }
                }
            }
            ViewerCommand::WordBackward(count) => {
                for _ in 0..count.max(1) {
                    let local = state.materialized.local_row(next.row);
                    if let Some(line) = buf.get_line(row_to_usize(local)) {
                        let col =
                            text::word_backward_pos(line, next.byte_col, text::CharClass::Word);
                        if col > 0 || next.row == 0 {
                            next.byte_col = col;
                        } else {
                            next.row = next.row.saturating_sub(1);
                            let prev_local = state.materialized.local_row(next.row);
                            next.byte_col = buf
                                .get_line(row_to_usize(prev_local))
                                .map(|line| line.len())
                                .unwrap_or(0);
                        }
                    }
                }
            }
            ViewerCommand::ClearSelection => {
                state.selection_anchor = None;
                state.drag_endpoint = None;
                state.yank_flash = None;
            }
        }
        state.cursor = next;
        self.virtual_rows = Some(state);
        if !matches!(
            command,
            ViewerCommand::ScrollRows(_)
                | ViewerCommand::CenterScroll
                | ViewerCommand::PanColumns(_)
        ) {
            self.scroll_top = scroll_to_show(self.scroll_top, next.row, viewport_rows);
            self.pin_current_scroll();
        }
        copy
    }
}

fn order_doc_range(a: DocPosition, b: DocPosition) -> DocRange {
    if (a.row, a.byte_col) <= (b.row, b.byte_col) {
        DocRange { start: a, end: b }
    } else {
        DocRange { start: b, end: a }
    }
}
