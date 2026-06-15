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
pub struct RowYankFlash {
    pub range: DocRange,
    pub until: Instant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowTextState {
    pub active: bool,
    pub materialized: MaterializedRows,
    /// Authoritative row-document cursor position. `byte_col` is a byte column in
    /// the materialized/document row text, not a terminal cell column.
    pub cursor: DocPosition,
    /// Preferred display column for document-row vertical motion, measured in
    /// terminal cells so multibyte chrome/text does not drift.
    pub preferred_cell_col: Option<usize>,
    pub selection_anchor: Option<DocPosition>,
    pub drag_endpoint: Option<DocPosition>,
    pub yank_flash: Option<RowYankFlash>,
    /// Whether the active character-wise selection should include the cursor
    /// cell under the block (true for mouse drags and Visual mode, false for
    /// VisualLine and bare single clicks that never became a drag).
    pub selection_includes_cursor_cell: bool,
}

impl Window {
    /// Configure the backing buffer as a materialized slice of a larger row
    /// document. Clears row-document mode only when the slice covers the whole
    /// row space; a top slice with more rows below must still keep `total_rows`.
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
            self.clear_materialized_rows();
        } else {
            self.set_row_materialization(rows);
        }
    }

    fn set_row_materialization(&mut self, rows: MaterializedRows) {
        let cursor = if self.row_text_state().active {
            self.row_text_state().cursor
        } else {
            DocPosition {
                row: rows.absolute_row(self.cursor_row()),
                byte_col: self.cursor_col() as usize,
            }
        };
        let state = self.row_text_state_mut();
        state.active = true;
        state.materialized = rows;
        state.cursor = DocPosition {
            row: cursor.row.min(rows.total_rows.saturating_sub(1)),
            byte_col: cursor.byte_col,
        };
    }

    pub fn clear_materialized_rows(&mut self) {
        *self.row_text_state_mut() = RowTextState::default();
    }

    pub fn scroll_row_total(&self, buf: &Buffer) -> RowIndex {
        let state = self.row_text_state();
        if state.active {
            state.materialized.total_rows
        } else {
            self.visual_row_total(buf)
        }
    }

    pub fn has_materialized_rows(&self) -> bool {
        self.row_text_state().active
    }

    pub fn row_cursor(&self) -> Option<DocPosition> {
        self.row_text_state()
            .active
            .then_some(self.row_text_state().cursor)
    }

    pub fn drag_active(&self) -> bool {
        self.text_state().drag_endpoint.is_some()
            || (self.row_text_state().active && self.row_text_state().drag_endpoint.is_some())
    }

    pub fn row_selection_anchor_active(&self) -> bool {
        self.row_text_state().active && self.row_text_state().selection_anchor.is_some()
    }

    pub fn row_selection_range(&self, buf: &Buffer, now: Instant) -> Option<DocRange> {
        self.row_yank_flash_range(now)
            .or_else(|| self.row_selection_anchor_range(buf))
    }

    pub fn row_selection_anchor_range(&self, buf: &Buffer) -> Option<DocRange> {
        if !self.row_text_state().active {
            return None;
        }
        let state = *self.row_text_state();
        state.selection_anchor.map(|anchor| {
            if matches!(self.vim_mode(), VimMode::VisualLine) {
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
                order_doc_range_including_cursor_cell(
                    buf,
                    state.materialized,
                    anchor,
                    state.cursor,
                    state.selection_includes_cursor_cell,
                )
            }
        })
    }

    pub fn row_yank_flash_range(&self, now: Instant) -> Option<DocRange> {
        if !self.row_text_state().active {
            return None;
        }
        self.row_text_state()
            .yank_flash
            .filter(|flash| now < flash.until)
            .map(|flash| flash.range)
    }

    pub fn row_selection_ranges(
        &self,
        buf: &Buffer,
        viewport_rows: u16,
        now: Instant,
    ) -> Vec<smelt_buffer::buffer::SelectionRange> {
        let Some(range) = self.row_selection_range(buf, now) else {
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
        let state = self.row_text_state();
        let total_rows = if state.active {
            state.materialized.total_rows
        } else {
            self.visual_row_total(buf)
        };
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
            // one-cell fallback span placed after any leading non-selectable chrome
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

    pub fn sync_row_render_state(&mut self, buf: &mut Buffer, viewport_rows: u16, now: Instant) {
        self.resync_row_display_coords(buf);
        let text = buf.text();
        self.clamp_anchors_to_source(&text);
        self.clear_expired_row_yank_flash(now);
        let anchor_range = self.row_selection_anchor_range(buf);
        let selection_ranges = anchor_range
            .map(|r| self.doc_range_to_row_ranges(buf, viewport_rows, r))
            .unwrap_or_default();
        buf.set_range_layer(crate::RangeLayer::Selection, selection_ranges);
        let flash_ranges = self
            .row_yank_flash_range(now)
            .map(|r| self.doc_range_to_row_ranges(buf, viewport_rows, r))
            .unwrap_or_default();
        buf.set_range_layer(crate::RangeLayer::YankFlash, flash_ranges);
    }

    pub fn row_yank_flash_until(&self) -> Option<Instant> {
        self.row_text_state().active.then_some(())?;
        self.row_text_state().yank_flash.map(|flash| flash.until)
    }

    pub fn clear_expired_row_yank_flash(&mut self, now: Instant) {
        if self.row_text_state().active
            && self
                .row_text_state()
                .yank_flash
                .is_some_and(|flash| now >= flash.until)
        {
            self.row_text_state_mut().yank_flash = None;
        }
    }

    pub fn sync_row_cursor_to_local(&mut self, buf: &Buffer, viewport_rows: u16) {
        self.reveal_row_cursor(buf, viewport_rows);
    }

    pub fn resync_row_display_coords(&mut self, buf: &Buffer) -> bool {
        if !self.row_text_state().active {
            return false;
        }
        let state = *self.row_text_state();
        if buf.lines().is_empty() {
            self.reset_cursor();
            return false;
        }
        self.project_row_cursor_to_local(state, buf)
    }

    pub fn reveal_row_cursor(&mut self, buf: &Buffer, viewport_rows: u16) {
        if !self.row_text_state().active {
            return;
        }
        if buf.lines().is_empty() {
            self.reset_cursor();
            return;
        }
        let state = *self.row_text_state();
        let local_cursor_synced = self.resync_row_display_coords(buf);
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

    pub fn handle_row_viewer_key(&mut self, key: KeyEvent) -> Option<ViewerCommand> {
        let text = self.text_state_mut();
        vim::handle_row_viewer_key(key, &mut text.vim_mode, &mut text.vim_state)
    }

    pub fn handle_row_mouse(
        &mut self,
        buf: &Buffer,
        event: MouseEvent,
        ctx: MouseCtx,
        now: Instant,
    ) -> (Status, Option<DocRange>) {
        if !self.row_text_state().active {
            return (Status::Ignored, None);
        }
        let mut state = *self.row_text_state();
        if ctx.viewport.rect.height == 0
            || state.materialized.total_rows == 0
            || buf.lines().is_empty()
        {
            return (Status::Consumed, None);
        }
        let pos = self.row_doc_pos_at_mouse(buf, event, ctx.viewport);
        if let Some(cell) = self.row_position_cell(state, buf, pos) {
            state.preferred_cell_col = Some(cell);
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                state.cursor = pos;
                state.drag_endpoint = Some(pos);
                state.selection_anchor = None;
                state.yank_flash = None;
                state.selection_includes_cursor_cell = false;
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
                if let Some(cell) = self.row_position_cell(state, buf, state.cursor) {
                    state.preferred_cell_col = Some(cell);
                }
                *self.row_text_state_mut() = state;
                // A new mouse gesture starts fresh: exit any existing visual
                // mode so the old anchor doesn't pollute the new selection.
                if self.vim_enabled()
                    && matches!(self.vim_mode(), VimMode::Visual | VimMode::VisualLine)
                {
                    self.set_vim_mode(VimMode::Normal);
                }
                (Status::Capture, None)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if state.selection_anchor.is_none() {
                    state.selection_anchor = state.drag_endpoint.or(Some(state.cursor));
                    state.selection_includes_cursor_cell = true;
                }
                state.cursor = pos;
                state.drag_endpoint = Some(pos);
                state.yank_flash = None;
                *self.row_text_state_mut() = state;
                (Status::Consumed, None)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Use the cursor position that was built up during the gesture
                // (e.g. word_end for double-click, line_end for triple-click,
                // or the last drag position for a normal drag) rather than
                // the release coordinates, which may truncate the selection.
                let copy = state
                    .selection_anchor
                    .map(|anchor| {
                        order_doc_range_including_cursor_cell(
                            buf,
                            state.materialized,
                            anchor,
                            state.cursor,
                            state.selection_includes_cursor_cell,
                        )
                    })
                    .filter(|range| {
                        (range.start.row, range.start.byte_col)
                            < (range.end.row, range.end.byte_col)
                    });
                state.cursor = pos;
                state.drag_endpoint = None;
                state.selection_anchor = None;
                state.selection_includes_cursor_cell = false;
                state.yank_flash = copy.map(|range| RowYankFlash {
                    range,
                    until: now + YANK_FLASH_DURATION,
                });
                *self.row_text_state_mut() = state;
                // Mouse-up concludes the gesture: exit visual mode so the
                // selection is not left dangling.
                if self.vim_enabled()
                    && matches!(self.vim_mode(), VimMode::Visual | VimMode::VisualLine)
                {
                    self.set_vim_mode(VimMode::Normal);
                }
                (Status::Consumed, copy)
            }
            _ => (Status::Ignored, None),
        }
    }

    fn row_doc_pos_at_mouse(
        &mut self,
        buf: &Buffer,
        event: MouseEvent,
        viewport: WindowViewport,
    ) -> DocPosition {
        if !self.row_text_state().active {
            return DocPosition::default();
        }
        let state = *self.row_text_state();
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

    pub fn execute_row_viewer_command(
        &mut self,
        buf: &Buffer,
        command: ViewerCommand,
        viewport_rows: u16,
        now: Instant,
    ) -> Option<DocRange> {
        if !self.row_text_state().active {
            return None;
        }
        let mut state = *self.row_text_state();
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
                self.move_row_cursor_to_row_preserving_cell(&mut state, buf, &mut next, row);
            }
            ViewerCommand::PageRows(delta) => {
                let rows = (viewport_rows as isize).saturating_mul(delta);
                let row = add_signed_row(current.row, rows).min(total_rows.saturating_sub(1));
                self.move_row_cursor_to_row_preserving_cell(&mut state, buf, &mut next, row);
            }
            ViewerCommand::HalfPageRows(delta) => {
                let rows = (viewport_rows as isize / 2).max(1).saturating_mul(delta);
                let row = add_signed_row(current.row, rows).min(total_rows.saturating_sub(1));
                self.move_row_cursor_to_row_preserving_cell(&mut state, buf, &mut next, row);
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
                self.move_row_cursor_to_row_preserving_cell(&mut state, buf, &mut next, row);
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
                self.move_row_cursor_to_row_preserving_cell(&mut state, buf, &mut next, row);
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
                    .map(|line| {
                        if matches!(self.vim_mode(), VimMode::Normal) && !line.is_empty() {
                            text::prev_char_boundary(line, line.len())
                        } else {
                            line.len()
                        }
                    })
                    .unwrap_or(0);
                state.preferred_cell_col = None;
            }
            ViewerCommand::StartVisual => {
                state.selection_anchor = Some(current);
                state.selection_includes_cursor_cell = true;
            }
            ViewerCommand::StartVisualLine => {
                state.selection_anchor = Some(DocPosition {
                    row: current.row,
                    byte_col: 0,
                });
                state.selection_includes_cursor_cell = false;
            }
            ViewerCommand::YankSelection => {
                if let Some(anchor) = state.selection_anchor {
                    let range = order_doc_range_including_cursor_cell(
                        buf,
                        state.materialized,
                        anchor,
                        current,
                        state.selection_includes_cursor_cell,
                    );
                    copy = Some(range);
                    state.yank_flash = copy.map(|range| RowYankFlash {
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
                    state.yank_flash = copy.map(|range| RowYankFlash {
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
                state.yank_flash = copy.map(|range| RowYankFlash {
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
                    if !matches!(self.vim_mode(), VimMode::Visual | VimMode::VisualLine)
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
                        if let Some(cell) = self.row_position_cell(state, buf, next) {
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
            if let Some(cell) = self.row_position_cell(state, buf, next) {
                state.preferred_cell_col = Some(cell);
            }
        }
        state.cursor = next;
        *self.row_text_state_mut() = state;
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

fn order_doc_range_including_cursor_cell(
    buf: &Buffer,
    materialized: MaterializedRows,
    a: DocPosition,
    b: DocPosition,
    include_cursor_cell: bool,
) -> DocRange {
    if (a.row, a.byte_col) <= (b.row, b.byte_col) {
        DocRange {
            start: a,
            end: advance_doc_position_if_on_char(buf, materialized, b, include_cursor_cell),
        }
    } else {
        DocRange {
            start: b,
            end: advance_doc_position_if_on_char(buf, materialized, a, include_cursor_cell),
        }
    }
}

fn advance_doc_position_if_on_char(
    buf: &Buffer,
    materialized: MaterializedRows,
    mut pos: DocPosition,
    advance: bool,
) -> DocPosition {
    if !advance {
        return pos;
    }
    let local = materialized
        .local_row(pos.row)
        .min(buf.lines().len().saturating_sub(1) as RowIndex);
    let Some(line) = buf.get_line(row_to_usize(local)) else {
        return pos;
    };
    if pos.byte_col < line.len() && line.as_bytes()[pos.byte_col] != b'\n' {
        pos.byte_col = text::next_char_boundary(line, pos.byte_col);
    }
    pos
}
