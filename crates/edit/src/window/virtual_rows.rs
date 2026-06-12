use super::*;
use smelt_buffer::kill_ring::YANK_FLASH_DURATION;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerCommand {
    MoveRows(isize),
    PageRows(isize),
    HalfPageRows(isize),
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
    MoveCursorCol(isize),
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
    /// Authoritative virtual cursor position. `byte_col` is a byte column in
    /// the materialized/virtual row text, not a terminal cell column.
    pub cursor: DocPosition,
    /// Preferred display column for virtual vertical motion, measured in
    /// terminal cells so multibyte chrome/text does not drift.
    pub preferred_cell_col: Option<usize>,
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

    pub fn drag_active(&self) -> bool {
        self.drag_endpoint.is_some()
            || self
                .virtual_rows
                .is_some_and(|state| state.drag_endpoint.is_some())
    }

    pub fn virtual_selection_anchor_active(&self) -> bool {
        self.virtual_rows
            .map(|state| state.selection_anchor.is_some())
            .unwrap_or(false)
    }

    pub fn virtual_selection_range(&self, now: Instant) -> Option<DocRange> {
        self.virtual_yank_flash_range(now)
            .or_else(|| self.virtual_selection_anchor_range())
    }

    pub fn virtual_selection_anchor_range(&self) -> Option<DocRange> {
        let state = self.virtual_rows?;
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

    pub fn virtual_yank_flash_range(&self, now: Instant) -> Option<DocRange> {
        let state = self.virtual_rows?;
        state
            .yank_flash
            .filter(|flash| now < flash.until)
            .map(|flash| flash.range)
    }

    pub fn virtual_selection_ranges(
        &self,
        buf: &Buffer,
        viewport_rows: u16,
        now: Instant,
    ) -> Vec<smelt_buffer::buffer::SelectionRange> {
        let Some(range) = self.virtual_selection_range(now) else {
            return Vec::new();
        };
        self.doc_range_to_row_ranges(buf, viewport_rows, range)
    }

    pub fn doc_ranges_to_row_ranges(
        &self,
        buf: &Buffer,
        viewport_rows: u16,
        ranges: impl IntoIterator<Item = DocRange>,
    ) -> Vec<smelt_buffer::buffer::SelectionRange> {
        ranges
            .into_iter()
            .flat_map(|range| self.doc_range_to_row_ranges(buf, viewport_rows, range))
            .collect()
    }

    pub(crate) fn doc_range_to_row_ranges(
        &self,
        buf: &Buffer,
        viewport_rows: u16,
        range: DocRange,
    ) -> Vec<smelt_buffer::buffer::SelectionRange> {
        let rows = buf.lines();
        if rows.is_empty()
            || (range.start.row, range.start.byte_col) >= (range.end.row, range.end.byte_col)
        {
            return Vec::new();
        }
        let total_rows = self
            .virtual_rows
            .map(|state| state.materialized.total_rows)
            .unwrap_or_else(|| self.visual_row_total(buf));
        if total_rows == 0 {
            return Vec::new();
        }

        let visible_start = self.scroll_top;
        let visible_end = self.scroll_top.saturating_add(viewport_rows as RowIndex);
        let start_row = range.start.row.max(visible_start);
        let selection_end_row = if range.end.byte_col == 0 {
            range.end.row.saturating_sub(1)
        } else {
            range.end.row
        };
        let end_row = selection_end_row.min(visible_end.saturating_sub(1));
        if start_row > end_row {
            return Vec::new();
        }

        let mut out = Vec::new();
        for abs_row in start_row..=end_row {
            let Some(local) = self.backed_display_row(buf, abs_row) else {
                continue;
            };
            let Some(line) = rows.get(row_to_usize(local)) else {
                continue;
            };
            let line_width = text::byte_to_cell(line, line.len()) as u16;
            let mut col_start = if abs_row == range.start.row {
                text::byte_to_cell(line, text::snap(line, range.start.byte_col.min(line.len())))
                    as u16
            } else {
                0
            };
            let mut col_end = if abs_row == range.end.row {
                text::byte_to_cell(line, text::snap(line, range.end.byte_col.min(line.len())))
                    as u16
            } else {
                line_width
            };
            // A row that's part of the selection but has no selectable text gets a
            // one-cell virtual span placed after any leading non-selectable chrome
            // (gutter, padding) so the highlight doesn't paint over chrome cells.
            // This mirrors the rule in `smelt_buffer::coords::byte_range_to_row_ranges`.
            if line_width > 0
                && !smelt_buffer::coords::range_contains_selectable(
                    buf,
                    row_to_usize(local),
                    0,
                    line_width as usize,
                )
            {
                let chrome_end = smelt_buffer::coords::last_non_selectable_end(
                    buf,
                    row_to_usize(local),
                    line_width as usize,
                ) as u16;
                col_start = col_start.max(chrome_end);
                col_end = col_start + 1;
            }
            out.push(smelt_buffer::buffer::SelectionRange {
                line: row_to_usize(local),
                col_start,
                col_end: col_end.max(col_start.saturating_add(1)),
            });
        }
        out
    }

    pub fn sync_virtual_render_state(
        &mut self,
        buf: &mut Buffer,
        viewport_rows: u16,
        now: Instant,
    ) {
        self.sync_virtual_cursor_to_local(buf, viewport_rows);
        let text = buf.text();
        self.clamp_anchors_to_source(&text);
        self.clear_expired_virtual_yank_flash(now);
        let anchor_range = self.virtual_selection_anchor_range();
        let selection_ranges = anchor_range
            .map(|r| self.doc_range_to_row_ranges(buf, viewport_rows, r))
            .unwrap_or_default();
        buf.set_range_layer(crate::RangeLayer::Selection, selection_ranges);
        let flash_ranges = self
            .virtual_yank_flash_range(now)
            .map(|r| self.doc_range_to_row_ranges(buf, viewport_rows, r))
            .unwrap_or_default();
        buf.set_range_layer(crate::RangeLayer::YankFlash, flash_ranges);
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
        let local_cursor_synced = self.project_virtual_cursor_to_local(state, buf);
        if viewport_rows > 0 {
            let viewport_cols = if local_cursor_synced {
                self.viewport.map(|v| v.content_width).unwrap_or(0)
            } else {
                0
            };
            self.keep_cursor_visible(
                buf,
                state.materialized.total_rows,
                viewport_rows,
                viewport_cols,
            );
        }
    }

    pub fn handle_virtual_viewer_key(&mut self, key: KeyEvent) -> Option<ViewerCommand> {
        let text = self.text_state_mut();
        vim::handle_virtual_viewer_key(key, &mut text.vim_mode, &mut text.vim_state)
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
        if let Some(cell) = self.virtual_position_cell(state, buf, pos) {
            state.preferred_cell_col = Some(cell);
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                state.cursor = pos;
                state.drag_endpoint = Some(pos);
                state.selection_anchor = None;
                state.yank_flash = None;
                match ctx.click_count {
                    2 => {
                        let local = state
                            .materialized
                            .local_row(pos.row)
                            .min(self.visual_row_total(buf).saturating_sub(1));
                        if let Some(line) = buf.get_line(row_to_usize(local)) {
                            let snap_col = text::snap(line, pos.byte_col.min(line.len()));
                            if let Some((start, end)) =
                                text::big_word_range_at_transparent(line, snap_col, &[])
                            {
                                state.selection_anchor = Some(DocPosition {
                                    row: pos.row,
                                    byte_col: start,
                                });
                                state.cursor = DocPosition {
                                    row: pos.row,
                                    byte_col: end,
                                };
                            }
                        }
                    }
                    3 => {
                        let local = state
                            .materialized
                            .local_row(pos.row)
                            .min(self.visual_row_total(buf).saturating_sub(1));
                        if let Some(line) = buf.get_line(row_to_usize(local)) {
                            state.selection_anchor = Some(DocPosition {
                                row: pos.row,
                                byte_col: 0,
                            });
                            state.cursor = DocPosition {
                                row: pos.row,
                                byte_col: line.len(),
                            };
                        }
                    }
                    _ => {}
                }
                if let Some(cell) = self.virtual_position_cell(state, buf, state.cursor) {
                    state.preferred_cell_col = Some(cell);
                }
                self.virtual_rows = Some(state);
                // A new mouse gesture starts fresh: exit any existing visual
                // mode so the old anchor doesn't pollute the new selection.
                if self.vim_enabled
                    && matches!(self.vim_mode, VimMode::Visual | VimMode::VisualLine)
                {
                    self.set_vim_mode(VimMode::Normal);
                }
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
                // Use the cursor position that was built up during the gesture
                // (e.g. word_end for double-click, line_end for triple-click,
                // or the last drag position for a normal drag) rather than
                // the release coordinates, which may truncate the selection.
                let copy = state
                    .selection_anchor
                    .map(|anchor| order_doc_range(anchor, state.cursor))
                    .filter(|range| {
                        (range.start.row, range.start.byte_col)
                            < (range.end.row, range.end.byte_col)
                    });
                state.cursor = pos;
                state.drag_endpoint = None;
                state.selection_anchor = None;
                state.yank_flash = copy.map(|range| VirtualYankFlash {
                    range,
                    until: now + YANK_FLASH_DURATION,
                });
                self.virtual_rows = Some(state);
                // Mouse-up concludes the gesture: exit visual mode so the
                // selection is not left dangling.
                if self.vim_enabled
                    && matches!(self.vim_mode, VimMode::Visual | VimMode::VisualLine)
                {
                    self.set_vim_mode(VimMode::Normal);
                }
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
        // Use the authoritative scroll_top on self, not the stale viewport copy
        // (viewport is set during render prep; autoscroll may have changed
        // scroll_top since then).
        let scroll_top = self.scroll_top;
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
        let row = scroll_top
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
                let row = add_signed_row(current.row, delta).min(total_rows.saturating_sub(1));
                self.move_virtual_cursor_to_row_preserving_cell(&mut state, buf, &mut next, row);
            }
            ViewerCommand::PageRows(delta) => {
                let rows = (viewport_rows as isize).saturating_mul(delta);
                let row = add_signed_row(current.row, rows).min(total_rows.saturating_sub(1));
                self.move_virtual_cursor_to_row_preserving_cell(&mut state, buf, &mut next, row);
            }
            ViewerCommand::HalfPageRows(delta) => {
                let rows = (viewport_rows as isize / 2).max(1).saturating_mul(delta);
                let row = add_signed_row(current.row, rows).min(total_rows.saturating_sub(1));
                self.move_virtual_cursor_to_row_preserving_cell(&mut state, buf, &mut next, row);
            }
            ViewerCommand::ScrollRows(delta) => {
                let max_scroll = total_rows.saturating_sub(viewport_rows as RowIndex);
                let cur_scroll = if self.is_following_tail() && self.scroll_top != max_scroll {
                    max_scroll
                } else {
                    self.scroll_top
                };
                let screen_row =
                    Self::screen_row_or_edge(current.row, cur_scroll, viewport_rows) as RowIndex;
                let new_scroll = add_signed_row(cur_scroll, delta).min(max_scroll);
                self.set_scroll(new_scroll, buf);
                self.update_tail_state(buf, viewport_rows);
                let row = new_scroll
                    .saturating_add(screen_row)
                    .min(total_rows.saturating_sub(1));
                self.move_virtual_cursor_to_row_preserving_cell(&mut state, buf, &mut next, row);
            }
            ViewerCommand::BufferStart => {
                next.row = 0;
                next.byte_col = 0;
                state.preferred_cell_col = None;
            }
            ViewerCommand::BufferEnd => {
                next.row = total_rows.saturating_sub(1);
            }
            ViewerCommand::GotoRow(row) => {
                self.move_virtual_cursor_to_row_preserving_cell(&mut state, buf, &mut next, row);
            }
            ViewerCommand::GotoPosition(pos) => {
                next.row = pos.row.min(total_rows.saturating_sub(1));
                next.byte_col = pos.byte_col;
                state.preferred_cell_col = None;
            }
            ViewerCommand::LineStart => {
                next.byte_col = 0;
                state.preferred_cell_col = None;
            }
            ViewerCommand::LineEnd => {
                let local = state.materialized.local_row(current.row);
                next.byte_col = buf
                    .get_line(row_to_usize(local))
                    .map(|line| line.len())
                    .unwrap_or(0);
                state.preferred_cell_col = None;
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
            ViewerCommand::MoveCursorCol(delta) => {
                let local = state.materialized.local_row(next.row);
                if let Some(line) = buf.get_line(row_to_usize(local)) {
                    let old = next.byte_col;
                    if delta < 0 {
                        for _ in 0..(-delta) as usize {
                            next.byte_col = text::prev_char_boundary(line, next.byte_col);
                        }
                    } else {
                        for _ in 0..delta as usize {
                            next.byte_col = text::next_char_boundary(line, next.byte_col);
                        }
                    }
                    // In Normal mode `l` must stop on the last character of the
                    // line, not move past it. Visual/VisualLine allow the
                    // cursor to sit past the last char so the selection is
                    // inclusive.
                    if !matches!(self.vim_mode, VimMode::Visual | VimMode::VisualLine)
                        && delta > 0
                        && next.byte_col > 0
                        && next.byte_col >= line.len()
                    {
                        next.byte_col = text::prev_char_boundary(line, line.len());
                    }
                    // Update the preferred cell from the new byte position so
                    // vertical motion preserves the intended column, but only
                    // when the cursor actually moved (vim curswant semantics).
                    if next.byte_col != old {
                        if let Some(cell) = self.virtual_position_cell(state, buf, next) {
                            state.preferred_cell_col = Some(cell);
                        }
                    }
                }
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
        if matches!(
            command,
            ViewerCommand::BufferStart
                | ViewerCommand::BufferEnd
                | ViewerCommand::GotoPosition(_)
                | ViewerCommand::LineStart
                | ViewerCommand::LineEnd
                | ViewerCommand::WordForward(_)
                | ViewerCommand::WordBackward(_)
                | ViewerCommand::WordEnd(_)
        ) {
            if let Some(cell) = self.virtual_position_cell(state, buf, next) {
                state.preferred_cell_col = Some(cell);
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
