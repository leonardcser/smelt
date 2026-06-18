use crate::app::transcript_search::{
    previous_search_position, TranscriptSearchIndex, TranscriptSearchSession,
};
use crate::app::TuiApp;
use crate::smelt_edit::{
    BufId, Buffer, DisplayRow, DocPosition, DocRange, RowIndex, TextRange, WinId, Window,
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

enum FullSearchRefresh {
    Unchanged,
    Changed(Option<DocRange>),
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
        changedtick: u64,
    },
    Transcript(TranscriptSearchSession),
}

pub(crate) struct SearchRenderSession {
    pub(crate) target: WinId,
    query: String,
}

impl From<&SearchSession> for SearchRenderSession {
    fn from(session: &SearchSession) -> Self {
        Self {
            target: session.target,
            query: session.query.clone(),
        }
    }
}

impl SearchRenderSession {
    pub(crate) fn apply_to_window(&self, win: &mut Window, buf: &Buffer, visible_rows: u16) {
        let ranges = win.doc_ranges_to_row_ranges(
            buf,
            visible_rows,
            visible_buffer_matches(win, buf, visible_rows, &self.query),
        );
        win.set_range_layer(crate::smelt_edit::RangeLayer::Search, ranges);
    }
}

impl SearchSession {
    pub(crate) fn current_range(&self) -> Option<TextRange> {
        match &self.backend {
            SearchBackend::Full {
                matches, current, ..
            } => current.and_then(|index| matches.get(index).cloned()),
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
        if let Some(win) = self.ui.win_mut(session.target) {
            win.clear_range_layer(crate::smelt_edit::RangeLayer::Search);
        }
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
        if self.transcript_document_is_attached_to(target) {
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
        let changedtick = self
            .ui
            .buf(target_buf)
            .map(Buffer::changedtick)
            .unwrap_or_default();
        self.search.session = Some(SearchSession {
            target,
            target_buf,
            query,
            direction,
            backend: SearchBackend::Full {
                matches,
                current,
                changedtick,
            },
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

        match self.sync_full_search_session(target, direction) {
            FullSearchRefresh::Unchanged => {}
            FullSearchRefresh::Changed(Some(range)) => {
                self.jump_to_search_range(range);
                return true;
            }
            FullSearchRefresh::Changed(None) => return false,
        }

        let Some((next, range)) = self.search.session.as_ref().and_then(|session| {
            let SearchBackend::Full {
                matches, current, ..
            } = &session.backend
            else {
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

    fn sync_full_search_session(
        &mut self,
        target: WinId,
        direction: SearchDirection,
    ) -> FullSearchRefresh {
        let Some(session) = self.search.session.as_ref() else {
            return FullSearchRefresh::Unchanged;
        };
        let SearchBackend::Full { changedtick, .. } = &session.backend else {
            return FullSearchRefresh::Unchanged;
        };
        let Some(buf_tick) = self.ui.buf(session.target_buf).map(Buffer::changedtick) else {
            return FullSearchRefresh::Unchanged;
        };
        if buf_tick == *changedtick {
            return FullSearchRefresh::Unchanged;
        }

        let query = session.query.clone();
        let origin = self.search_origin(target).unwrap_or_default();
        let matches = self.scan_search_matches(target, &query);
        let current = initial_match(&matches, origin, direction);
        let range = current
            .and_then(|index| matches.get(index))
            .and_then(TextRange::rows);
        if let Some(SearchSession {
            backend:
                SearchBackend::Full {
                    matches: session_matches,
                    current: session_current,
                    changedtick,
                },
            ..
        }) = self.search.session.as_mut()
        {
            *session_matches = matches;
            *session_current = current;
            *changedtick = buf_tick;
        }
        FullSearchRefresh::Changed(range)
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
        let total_rows = self
            .document_snapshot_for_win(win)
            .map(|snapshot| snapshot.total_rows)
            .unwrap_or(0);
        let mut matches = Vec::new();
        let mut start = 0;
        while start < total_rows {
            let count = SEARCH_SCAN_ROWS.min(total_rows - start);
            let display = self
                .materialize_document_rows(win, start, count)
                .unwrap_or_default();
            for (offset, row) in display.rows.iter().enumerate() {
                let row_index = start.saturating_add(offset as RowIndex);
                matches.extend(display_row_matches(row, row_index, query).map(TextRange::Rows));
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
        let Some(target) = self.search.session.as_ref().map(|session| session.target) else {
            return;
        };
        self.reveal_position(
            target,
            range.start,
            crate::app::reveal::RevealOptions::avoid_edge_chrome(target),
        );
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

pub(super) fn display_row_matches<'a>(
    row: &'a DisplayRow,
    row_index: RowIndex,
    query: &'a str,
) -> impl Iterator<Item = DocRange> + 'a {
    row.text
        .match_indices(query)
        .filter_map(move |(byte_col, _)| {
            let end_col = byte_col + query.len();
            row_match_is_selectable(row, byte_col, end_col)
                .then(|| doc_range_for_match(row_index, byte_col, end_col))
        })
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
        let row = DisplayRow::new(
            line.clone(),
            crate::smelt_edit::selectable_byte_ranges_for_line(line, &buf.highlights_at(local_row)),
        );
        matches.extend(display_row_matches(&row, absolute_row, query));
    }
    matches
}
