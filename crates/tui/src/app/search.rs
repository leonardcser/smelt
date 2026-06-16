use crate::app::transcript_search::{
    previous_search_position, TranscriptSearchIndex, TranscriptSearchSession,
};
use crate::app::TuiApp;
use crate::smelt_edit::{
    BufId, Buffer, DisplayDocument, DocPosition, DocRange, HostDisplayDocument, RowIndex,
    TextRange, WinId, Window,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// Search scans bounded display-row windows so large row-backed documents do not
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
    pub(crate) backend: SearchBackend,
}

#[derive(Clone, Debug)]
pub(crate) enum SearchBackend {
    Full {
        matches: Vec<TextRange>,
        current: Option<usize>,
    },
    Transcript(TranscriptSearchSession),
}

pub(crate) struct SearchRenderSession {
    pub(crate) target: WinId,
    query: String,
    full_matches: Option<Vec<TextRange>>,
}

impl From<&SearchSession> for SearchRenderSession {
    fn from(session: &SearchSession) -> Self {
        Self {
            target: session.target,
            query: session.query.clone(),
            full_matches: match &session.backend {
                SearchBackend::Full { matches, .. } => Some(matches.clone()),
                SearchBackend::Transcript(_) => None,
            },
        }
    }
}

impl SearchRenderSession {
    pub(crate) fn visible_line_matches(
        &self,
        win: &Window,
        buf: &Buffer,
        visible_rows: u16,
    ) -> Vec<DocRange> {
        if let Some(matches) = &self.full_matches {
            let visible_start = win.scroll_top();
            let visible_end = visible_start.saturating_add(visible_rows.max(1) as RowIndex);
            matches
                .iter()
                .filter_map(TextRange::rows)
                .filter(|range| range.start.row >= visible_start && range.start.row < visible_end)
                .collect()
        } else {
            visible_buffer_matches(win, buf, visible_rows, &self.query)
        }
    }
}

impl SearchSession {
    pub(crate) fn current_range(&self) -> Option<TextRange> {
        match &self.backend {
            SearchBackend::Full { matches, current } => {
                current.and_then(|index| matches.get(index).cloned())
            }
            SearchBackend::Transcript(session) => session
                .current
                .and_then(|index| session.matches.get(index).copied())
                .map(TextRange::Rows),
        }
    }

    #[cfg(test)]
    pub(crate) fn full_matches(&self) -> &[TextRange] {
        match &self.backend {
            SearchBackend::Full { matches, .. } => matches,
            SearchBackend::Transcript(_) => &[],
        }
    }
}

#[derive(Default)]
pub(crate) struct SearchState {
    pub(crate) session: Option<SearchSession>,
    pub(super) transcript_index: Option<TranscriptSearchIndex>,
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
        let origin = self.search_origin(target).unwrap_or_default();
        if target == crate::app::TRANSCRIPT_WIN {
            let Some(mut transcript_session) = self.new_transcript_search_session(&query) else {
                self.clear_search();
                return;
            };
            let current =
                self.advance_transcript_search(&mut transcript_session, &query, origin, direction);
            self.search.session = Some(SearchSession {
                target,
                target_buf,
                query,
                direction,
                backend: SearchBackend::Transcript(transcript_session),
            });
            if let Some(range) = current {
                self.jump_to_search_range(range);
            }
            return;
        }

        let matches = self.scan_search_matches(target, &query);
        let current = initial_match(&matches, origin, direction);
        self.search.session = Some(SearchSession {
            target,
            target_buf,
            query,
            direction,
            backend: SearchBackend::Full { matches, current },
        });
        if let Some(range) = self
            .search
            .session
            .as_ref()
            .and_then(SearchSession::current_range)
            .and_then(|range| range.rows())
        {
            self.jump_to_search_range(range);
        }
    }

    fn repeat_search(&mut self, target: WinId, reverse: bool) -> bool {
        let Some(session) = self.search.session.as_ref() else {
            return false;
        };
        if session.target != target || session.query.is_empty() {
            return false;
        }
        let direction = if reverse {
            session.direction.reversed()
        } else {
            session.direction
        };

        if matches!(session.backend, SearchBackend::Transcript(_)) {
            let mut session = self.search.session.take().expect("search session exists");
            let query = session.query.clone();
            let range = match &mut session.backend {
                SearchBackend::Transcript(transcript) => {
                    if !self.sync_transcript_search_session(transcript, &query) {
                        let Some(new_session) = self.new_transcript_search_session(&query) else {
                            self.search.session = Some(session);
                            return false;
                        };
                        *transcript = new_session;
                    }
                    let origin = match (
                        transcript
                            .current
                            .and_then(|index| transcript.matches.get(index).copied()),
                        direction,
                    ) {
                        (Some(range), SearchDirection::Forward) => range.end,
                        (Some(range), SearchDirection::Backward)
                            if range.start.row == 0 && range.start.byte_col == 0 =>
                        {
                            DocPosition {
                                row: RowIndex::MAX,
                                byte_col: usize::MAX,
                            }
                        }
                        (Some(range), SearchDirection::Backward) => {
                            previous_search_position(range.start)
                        }
                        (None, _) => self.search_origin(target).unwrap_or_default(),
                    };
                    self.advance_transcript_search(transcript, &query, origin, direction)
                }
                SearchBackend::Full { .. } => None,
            };
            let Some(range) = range else {
                self.search.session = Some(session);
                return false;
            };
            self.search.session = Some(session);
            self.jump_to_search_range(range);
            return true;
        }

        let Some((next, range)) = self.search.session.as_ref().and_then(|session| {
            let SearchBackend::Full { matches, current } = &session.backend else {
                return None;
            };
            if matches.is_empty() {
                return None;
            }
            let len = matches.len();
            let current = current.unwrap_or_else(|| {
                initial_match(
                    matches,
                    self.search_origin(target).unwrap_or_default(),
                    direction,
                )
                .unwrap_or(0)
            });
            let next = match direction {
                SearchDirection::Forward => (current + 1) % len,
                SearchDirection::Backward => (current + len - 1) % len,
            };
            matches
                .get(next)
                .and_then(TextRange::rows)
                .map(|range| (next, range))
        }) else {
            return false;
        };
        if let Some(SearchSession {
            backend: SearchBackend::Full { current, .. },
            ..
        }) = self.search.session.as_mut()
        {
            *current = Some(next);
        }
        self.jump_to_search_range(range);
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
                    if !row_match_is_selectable(row, byte_col, end_col) {
                        continue;
                    }
                    matches.push(TextRange::Rows(doc_range_for_match(
                        row_index, byte_col, end_col,
                    )));
                }
            }
            start = start.saturating_add(count);
        }
        matches
    }

    fn search_origin(&self, win: WinId) -> Option<DocPosition> {
        let window = self.ui.win(win)?;
        if let Some(pos) = window.row_cursor() {
            return Some(pos);
        }
        let buf = self.ui.buf(window.buf)?;
        let (row, byte_col) = buf.display_byte_pos(window.cpos());
        Some(DocPosition {
            row: row as RowIndex,
            byte_col,
        })
    }

    fn jump_to_search_range(&mut self, range: DocRange) {
        let Some(session) = self.search.session.as_ref() else {
            return;
        };
        let target = session.target;
        let Some((buf_id, viewport_rows, is_row_backed)) = self.ui.win(target).map(|w| {
            (
                w.buf,
                w.viewport.map(|v| v.rect.height).unwrap_or(1).max(1),
                w.has_materialized_rows(),
            )
        }) else {
            return;
        };
        let top_padding = search_top_padding(target);
        let now = self.core.clock.instant_now();
        let (win, buf) = self.ui.win_and_buf_mut(target, buf_id);
        let (Some(win), Some(buf)) = (win, buf) else {
            return;
        };
        if is_row_backed {
            win.execute_row_viewer_command(
                buf,
                crate::smelt_edit::ViewerCommand::GotoPosition(range.start),
                viewport_rows,
                now,
            );
            apply_search_top_padding(win, buf, range.start.row, viewport_rows, top_padding);
        } else {
            if let Some(cpos) = byte_offset_for_doc_position(buf, range.start) {
                win.set_cpos(cpos);
                win.resync(buf, viewport_rows);
                apply_search_top_padding(win, buf, range.start.row, viewport_rows, top_padding);
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

pub(super) fn row_match_is_selectable(
    row: &crate::smelt_edit::DisplayRow,
    byte_col: usize,
    end_col: usize,
) -> bool {
    row.selectable_ranges
        .iter()
        .any(|range| range.start <= byte_col && end_col <= range.end)
}

pub(super) fn doc_range_for_match(row: RowIndex, byte_col: usize, end_col: usize) -> DocRange {
    DocRange {
        start: DocPosition { row, byte_col },
        end: DocPosition {
            row,
            byte_col: end_col,
        },
    }
}

fn visible_buffer_matches(
    win: &Window,
    buf: &Buffer,
    visible_rows: u16,
    query: &str,
) -> Vec<DocRange> {
    if query.is_empty() {
        return Vec::new();
    }
    let visible_start = win.scroll_top();
    let visible_end = visible_start.saturating_add(visible_rows.max(1) as RowIndex);
    let materialized = win.materialized_rows();
    let mut matches = Vec::new();
    for (local_row, line) in buf.lines().iter().enumerate() {
        let absolute_row = materialized
            .map(|rows| rows.absolute_row(local_row as RowIndex))
            .unwrap_or(local_row as RowIndex);
        if absolute_row < visible_start || absolute_row >= visible_end {
            continue;
        }
        let selectable_ranges =
            crate::smelt_edit::selectable_byte_ranges_for_line(line, &buf.highlights_at(local_row));
        for (byte_col, _) in line.match_indices(query) {
            let end_col = byte_col + query.len();
            if selectable_ranges
                .iter()
                .any(|range| range.start <= byte_col && end_col <= range.end)
            {
                matches.push(doc_range_for_match(absolute_row, byte_col, end_col));
            }
        }
    }
    matches
}

fn search_top_padding(win: WinId) -> RowIndex {
    (win == crate::app::TRANSCRIPT_WIN) as RowIndex
}

fn apply_search_top_padding(
    win: &mut crate::smelt_edit::Window,
    buf: &crate::smelt_edit::Buffer,
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
    let total_rows = win.scroll_row_total(buf);
    let max_scroll = total_rows.saturating_sub(viewport_rows.max(1) as RowIndex);
    win.pin_scroll(row.saturating_sub(padding).min(max_scroll));
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
