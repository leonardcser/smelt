use crate::app::TuiApp;
use crate::smelt_edit::{
    BufId, DisplayDocument, DocPosition, DocRange, HostDisplayDocument, RowIndex, TextRange, WinId,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// Search scans bounded display-row windows so large virtual documents do not
// need to concatenate or materialize the whole transcript at once.
const SEARCH_SCAN_ROWS: RowIndex = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchDirection {
    Forward,
    Backward,
}

impl SearchDirection {
    fn reversed(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SearchSession {
    pub(crate) target: WinId,
    pub(crate) target_buf: BufId,
    pub(crate) query: String,
    pub(crate) direction: SearchDirection,
    pub(crate) matches: Vec<TextRange>,
    pub(crate) current: Option<usize>,
}

impl SearchSession {
    pub(crate) fn visible_line_matches(
        &self,
        visible_start: RowIndex,
        visible_rows: u16,
    ) -> Vec<DocRange> {
        let visible_end = visible_start.saturating_add(visible_rows.max(1) as RowIndex);
        self.matches
            .iter()
            .filter_map(TextRange::rows)
            .filter(|range| range.start.row >= visible_start && range.start.row < visible_end)
            .collect()
    }
}

#[derive(Default)]
pub(crate) struct SearchState {
    pub(crate) session: Option<SearchSession>,
}

impl TuiApp {
    pub(crate) fn open_search_input(&mut self, direction: SearchDirection) -> bool {
        let Some(target) = self.search_target() else {
            return false;
        };
        self.open_search_cmdline(target, direction);
        true
    }

    pub(crate) fn clear_search(&mut self) {
        let Some(session) = self.search.session.take() else {
            return;
        };
        let Some(buf) = self.ui.buf_mut(session.target_buf) else {
            return;
        };
        buf.clear_range_layer(crate::smelt_edit::RangeLayer::Search);
    }

    pub(crate) fn handle_search_key_for_target(&mut self, target: WinId, k: KeyEvent) -> bool {
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _)
                if self
                    .search
                    .session
                    .as_ref()
                    .is_some_and(|s| s.target == target) =>
            {
                self.clear_search();
                true
            }
            (KeyCode::Char('n'), KeyModifiers::NONE) => self.repeat_search(target, false),
            (KeyCode::Char('N'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.repeat_search(target, true)
            }
            _ => false,
        }
    }

    pub(crate) fn submit_search(
        &mut self,
        target: WinId,
        direction: SearchDirection,
        query: String,
    ) {
        if query.is_empty() || query.contains('\n') {
            self.clear_search();
            return;
        }
        let Some(target_buf) = self.ui.win(target).map(|win| win.buf) else {
            self.clear_search();
            return;
        };
        let matches = self.scan_search_matches(target, &query);
        let origin = self.search_origin(target).unwrap_or_default();
        let current = initial_match(&matches, origin, direction);
        self.search.session = Some(SearchSession {
            target,
            target_buf,
            query,
            direction,
            matches,
            current,
        });
        if let Some(index) = current {
            self.jump_to_search_match(index);
        }
    }

    fn repeat_search(&mut self, target: WinId, reverse: bool) -> bool {
        let Some(session) = self.search.session.as_ref() else {
            return false;
        };
        if session.target != target || session.query.is_empty() || session.matches.is_empty() {
            return false;
        }
        let direction = if reverse {
            session.direction.reversed()
        } else {
            session.direction
        };
        let len = session.matches.len();
        let current = session.current.unwrap_or_else(|| {
            initial_match(
                &session.matches,
                self.search_origin(target).unwrap_or_default(),
                direction,
            )
            .unwrap_or(0)
        });
        let next = match direction {
            SearchDirection::Forward => (current + 1) % len,
            SearchDirection::Backward => (current + len - 1) % len,
        };
        self.jump_to_search_match(next);
        true
    }

    fn search_target(&self) -> Option<WinId> {
        let focused = self.ui.focus();
        let content =
            (self.app_focus == crate::app::AppFocus::Content).then_some(crate::app::TRANSCRIPT_WIN);
        [focused, content]
            .into_iter()
            .flatten()
            .find(|&win| self.ui.win(win).is_some_and(|w| w.supports_search()))
    }

    /// Finds literal, display-row-local matches. Queries containing `\n` are
    /// rejected by `submit_search`; multi-line display search would need
    /// row-break-aware scanning and match storage.
    fn scan_search_matches(&mut self, win: WinId, query: &str) -> Vec<TextRange> {
        let mut doc = HostDisplayDocument::new(self, win);
        let total_rows = doc.snapshot().total_rows;
        let mut matches = Vec::new();
        let mut start = 0;
        while start < total_rows {
            let count = SEARCH_SCAN_ROWS.min(total_rows - start);
            let display = doc.materialize(start..start.saturating_add(count));
            for (offset, row) in display.rows.iter().enumerate() {
                let row_index = start.saturating_add(offset as RowIndex);
                for (byte_col, _) in row.text.match_indices(query) {
                    let end_col = byte_col + query.len();
                    if !row
                        .selectable_ranges
                        .iter()
                        .any(|range| range.start <= byte_col && end_col <= range.end)
                    {
                        continue;
                    }
                    matches.push(TextRange::Rows(DocRange {
                        start: DocPosition {
                            row: row_index,
                            byte_col,
                        },
                        end: DocPosition {
                            row: row_index,
                            byte_col: byte_col + query.len(),
                        },
                    }));
                }
            }
            start = start.saturating_add(count);
        }
        matches
    }

    fn search_origin(&self, win: WinId) -> Option<DocPosition> {
        let window = self.ui.win(win)?;
        if let Some(pos) = window.virtual_cursor() {
            return Some(pos);
        }
        let buf = self.ui.buf(window.buf)?;
        let (row, byte_col) = buf.display_byte_pos(window.cpos);
        Some(DocPosition {
            row: row as RowIndex,
            byte_col,
        })
    }

    fn jump_to_search_match(&mut self, index: usize) {
        let Some(session) = self.search.session.as_mut() else {
            return;
        };
        let Some(range) = session.matches.get(index).and_then(TextRange::rows) else {
            return;
        };
        session.current = Some(index);
        let target = session.target;
        let Some((buf_id, viewport_rows, is_virtual)) = self.ui.win(target).map(|w| {
            (
                w.buf,
                w.viewport.map(|v| v.rect.height).unwrap_or(1).max(1),
                w.is_virtual_rows(),
            )
        }) else {
            return;
        };
        let now = self.core.clock.instant_now();
        let (win, buf) = self.ui.win_and_buf_mut(target, buf_id);
        let (Some(win), Some(buf)) = (win, buf) else {
            return;
        };
        if is_virtual {
            win.execute_virtual_viewer_command(
                buf,
                crate::smelt_edit::ViewerCommand::GotoPosition(range.start),
                viewport_rows,
                now,
            );
        } else {
            if let Some(cpos) = byte_offset_for_doc_position(buf, range.start) {
                win.cpos = cpos;
                win.resync(buf, viewport_rows);
            }
        }
    }
}

fn initial_match(
    matches: &[TextRange],
    origin: DocPosition,
    direction: SearchDirection,
) -> Option<usize> {
    let starts_at_or_after = |m: &TextRange| {
        m.start_position()
            .is_some_and(|pos| (pos.row, pos.byte_col) >= (origin.row, origin.byte_col))
    };
    let starts_at_or_before = |m: &TextRange| {
        m.start_position()
            .is_some_and(|pos| (pos.row, pos.byte_col) <= (origin.row, origin.byte_col))
    };
    match direction {
        SearchDirection::Forward => matches
            .iter()
            .position(starts_at_or_after)
            .or_else(|| (!matches.is_empty()).then_some(0)),
        SearchDirection::Backward => matches
            .iter()
            .rposition(starts_at_or_before)
            .or_else(|| (!matches.is_empty()).then_some(matches.len() - 1)),
    }
}

fn byte_offset_for_doc_position(
    buf: &crate::smelt_edit::Buffer,
    pos: DocPosition,
) -> Option<usize> {
    let row = crate::smelt_edit::row_to_usize(pos.row);
    let line = buf.get_line(row)?;
    let byte_col = crate::smelt_edit::text::snap(line, pos.byte_col.min(line.len()));
    let prior: usize = buf
        .lines()
        .iter()
        .take(row)
        .map(|line| line.len() + 1)
        .sum();
    Some(prior + byte_col)
}
