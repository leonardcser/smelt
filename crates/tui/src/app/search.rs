use crate::app::transcript::{
    TranscriptProjectionHint, TranscriptSearchAnchor, TranscriptSearchBound,
};
use crate::app::transcript_search::{
    previous_search_position, SqliteTranscriptCandidateBlocks, TranscriptSearchContext,
    TranscriptSearchIndex, TranscriptSearchSession, TranscriptSearchStore, TranscriptSearchWorker,
    TranscriptSearchWorkerRequest, TranscriptSearchWorkerResult,
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
                .map(|matched| TextRange::Rows(matched.range)),
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

#[derive(Clone, Copy, Debug)]
struct TranscriptSearchPreviewAnchor {
    anchor: TranscriptSearchAnchor,
    width: u16,
    target_screen_row: RowIndex,
    origin_block_idx: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct SearchPreviewAnchor {
    target: WinId,
    target_buf: BufId,
    origin: DocPosition,
    scroll_top: RowIndex,
    transcript: Option<TranscriptSearchPreviewAnchor>,
}

#[derive(Clone, Debug)]
struct LiveSearchState {
    generation: u64,
    target: WinId,
    target_buf: BufId,
    direction: SearchDirection,
    context: TranscriptSearchContext,
    query: String,
    awaiting_worker: bool,
    confirmed: bool,
    original: SearchPreviewAnchor,
}

pub(crate) struct SearchState {
    pub(crate) session: Option<SearchSession>,
    pub(super) transcript_index: Option<TranscriptSearchIndex>,
    pub(super) transcript_store: Option<TranscriptSearchStore>,
    live: Option<LiveSearchState>,
    worker: Option<TranscriptSearchWorker>,
    next_generation: u64,
    #[cfg(test)]
    worker_delay: std::time::Duration,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            session: None,
            transcript_index: None,
            transcript_store: None,
            live: None,
            worker: None,
            next_generation: 1,
            #[cfg(test)]
            worker_delay: std::time::Duration::ZERO,
        }
    }
}

impl SearchState {
    fn advance_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("transcript search generation overflow");
        generation
    }

    fn cancel_worker(&mut self) -> u64 {
        let generation = self.advance_generation();
        if let Some(worker) = &self.worker {
            worker.cancel(generation);
        }
        generation
    }

    fn request_worker(
        &mut self,
        event_tx: tokio::sync::mpsc::UnboundedSender<crate::app::AppEvent>,
        request: TranscriptSearchWorkerRequest,
    ) {
        let worker = self
            .worker
            .get_or_insert_with(|| TranscriptSearchWorker::spawn(event_tx));
        #[cfg(test)]
        worker.set_delay(self.worker_delay);
        worker.request(request);
    }
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
        let Some(session) = self.overlays.take_search_session() else {
            return;
        };
        if let Some(win) = self.ui.win_mut(session.target) {
            win.clear_range_layer(crate::smelt_edit::RangeLayer::Search);
        }
    }

    fn transcript_search_context(&self) -> TranscriptSearchContext {
        TranscriptSearchContext {
            session_id: self.conversation.session().id.clone(),
        }
    }

    fn capture_search_preview_anchor(&mut self, target: WinId) -> Option<SearchPreviewAnchor> {
        let target_buf = self.ui.win(target)?.buf;
        let origin = self.search_origin(target)?;
        let scroll_top = self.ui.win(target)?.scroll_top();
        let transcript = if self.transcript_document_is_attached_to(target) {
            let width = self.transcript_width() as u16;
            let anchor = self
                .conversation
                .transcript_search_anchor_at_row(&self.lua, width, origin.row);
            let origin_block_idx = anchor
                .block_id()
                .map(|block_id| block_id.get())
                .or_else(|| {
                    self.conversation
                        .transcript_search_block_at_row(&self.lua, width, origin.row, true)
                        .map(|block_id| block_id.get())
                });
            Some(TranscriptSearchPreviewAnchor {
                anchor,
                width,
                target_screen_row: origin.row.saturating_sub(scroll_top),
                origin_block_idx,
            })
        } else {
            None
        };
        Some(SearchPreviewAnchor {
            target,
            target_buf,
            origin,
            scroll_top,
            transcript,
        })
    }

    fn restore_search_preview_anchor(&mut self, original: SearchPreviewAnchor) {
        if self
            .ui
            .win(original.target)
            .is_none_or(|window| window.buf != original.target_buf)
        {
            return;
        }
        if let Some(transcript) = original.transcript {
            let window_scroll_before = self.transcript_scroll_top();
            let hint = TranscriptProjectionHint::SearchProjectedRow {
                width: transcript.width,
                anchor: transcript.anchor,
                start_byte_col: original.origin.byte_col,
                row: original.origin.row,
                prefer_projected_row: false,
            };
            self.record_transcript_scroll_intent_with_hint(
                "search_restore",
                crate::app::transcript_scroll_trace::TranscriptScrollIntent::SearchJump {
                    anchor: transcript.anchor,
                    target_screen_row: transcript.target_screen_row,
                    match_start_byte_col: original.origin.byte_col,
                    match_end_byte_col: original.origin.byte_col,
                },
                window_scroll_before,
                hint,
            );
            return;
        }
        self.reveal_position(
            original.target,
            original.origin,
            crate::app::reveal::RevealOptions::default(),
        );
        if let Some(window) = self.ui.win_mut(original.target) {
            window.pin_scroll(original.scroll_top);
        }
    }

    pub(crate) fn begin_live_search(&mut self, target: WinId, direction: SearchDirection) {
        let Some(original) = self.capture_search_preview_anchor(target) else {
            return;
        };
        self.clear_search();
        let context = self.transcript_search_context();
        let search = self.overlays.search_state_mut();
        let generation = search.cancel_worker();
        search.live = Some(LiveSearchState {
            generation,
            target,
            target_buf: original.target_buf,
            direction,
            context,
            query: String::new(),
            awaiting_worker: false,
            confirmed: false,
            original,
        });
    }

    pub(crate) fn update_live_search(
        &mut self,
        target: WinId,
        direction: SearchDirection,
        query: String,
    ) {
        let current_context = self.transcript_search_context();
        let original = {
            let Some(live) = self.overlays.search_state().live.as_ref() else {
                return;
            };
            if live.target != target
                || live.direction != direction
                || self.ui.win(target).map(|window| window.buf) != Some(live.target_buf)
                || live.context != current_context
                || live.query == query
            {
                return;
            }
            live.original
        };

        let generation = {
            let search = self.overlays.search_state_mut();
            let generation = search.cancel_worker();
            let live = search.live.as_mut().expect("live search exists");
            live.generation = generation;
            live.query.clone_from(&query);
            live.awaiting_worker = false;
            generation
        };

        if query.is_empty() || query.contains('\n') {
            self.clear_search();
            self.restore_search_preview_anchor(original);
            return;
        }
        if !self.transcript_document_is_attached_to(target) {
            self.submit_search(target, direction, query);
            return;
        }

        smelt_perf::perf::record_value(
            "search:transcript:projection_requested",
            u64::from(self.conversation.request_search_projection()),
        );
        if !self.conversation.transcript_records_persisted() {
            let block_indices = self.dirty_transcript_candidate_blocks(&query);
            self.apply_live_transcript_candidates(
                generation,
                current_context,
                query,
                direction,
                SqliteTranscriptCandidateBlocks {
                    available: !block_indices.is_empty(),
                    block_indices,
                },
            );
            return;
        }

        let request = TranscriptSearchWorkerRequest {
            generation,
            context: current_context,
            session_dir: self
                .conversation
                .transcript()
                .session_dir()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| self.conversation.current_session_dir()),
            query,
            origin_block_idx: original
                .transcript
                .and_then(|anchor| anchor.origin_block_idx),
            direction,
        };
        let event_tx = self.platform.app_event_sender();
        let search = self.overlays.search_state_mut();
        if let Some(live) = search.live.as_mut() {
            live.awaiting_worker = true;
        }
        search.request_worker(event_tx, request);
    }

    fn apply_live_transcript_candidates(
        &mut self,
        generation: u64,
        context: TranscriptSearchContext,
        query: String,
        direction: SearchDirection,
        mut candidates: SqliteTranscriptCandidateBlocks,
    ) {
        let Some(live) = self.overlays.search_state().live.clone() else {
            return;
        };
        if live.generation != generation
            || live.context != context
            || live.query != query
            || live.direction != direction
            || context != self.transcript_search_context()
            || self
                .ui
                .win(live.target)
                .is_none_or(|window| window.buf != live.target_buf)
            || !self.transcript_document_is_attached_to(live.target)
        {
            return;
        }

        candidates
            .block_indices
            .extend(self.dirty_transcript_candidate_blocks(&query));
        candidates.block_indices.sort_unstable();
        candidates.block_indices.dedup();
        candidates.available |= !candidates.block_indices.is_empty();
        if let Some(mut transcript_session) = self.new_transcript_search_session_with_candidates(
            &query,
            live.original.origin,
            direction,
            candidates,
            live.original
                .transcript
                .and_then(|anchor| anchor.origin_block_idx),
        ) {
            let current = self.advance_transcript_search(
                &mut transcript_session,
                &query,
                live.original.origin,
                direction,
                None,
            );
            self.overlays.install_search_session(SearchSession {
                target: live.target,
                target_buf: live.target_buf,
                query,
                direction,
                backend: SearchBackend::Transcript(transcript_session),
            });
            if let Some(matched) = current {
                self.jump_to_transcript_search_match(matched);
            } else {
                self.restore_search_preview_anchor(live.original);
            }
        } else {
            self.clear_search();
            self.restore_search_preview_anchor(live.original);
        }

        let search = self.overlays.search_state_mut();
        if let Some(active) = search
            .live
            .as_mut()
            .filter(|active| active.generation == generation)
        {
            active.awaiting_worker = false;
            if active.confirmed {
                search.live = None;
            }
        }
    }

    pub(crate) fn handle_transcript_search_worker_result(
        &mut self,
        result: TranscriptSearchWorkerResult,
    ) {
        self.apply_live_transcript_candidates(
            result.generation,
            result.context,
            result.query,
            result.direction,
            result.candidates,
        );
    }

    pub(crate) fn confirm_live_search(
        &mut self,
        target: WinId,
        direction: SearchDirection,
        query: String,
    ) {
        if query.is_empty() || query.contains('\n') {
            self.cancel_live_search(true);
            return;
        }
        let search = self.overlays.search_state_mut();
        let Some(live) = search.live.as_mut() else {
            self.submit_search(target, direction, query);
            return;
        };
        if live.target != target || live.direction != direction || live.query != query {
            self.submit_search(target, direction, query);
            return;
        }
        live.confirmed = true;
        if !live.awaiting_worker {
            search.live = None;
        }
    }

    pub(crate) fn cancel_live_search(&mut self, restore_original: bool) {
        let live = {
            let search = self.overlays.search_state_mut();
            let live = search.live.take();
            search.cancel_worker();
            live
        };
        self.clear_search();
        if restore_original {
            if let Some(live) = live {
                self.restore_search_preview_anchor(live.original);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_transcript_search_worker_delay_for_harness(
        &mut self,
        delay: std::time::Duration,
    ) {
        self.overlays.search_state_mut().worker_delay = delay;
    }

    pub(crate) fn handle_search_key_for_target(&mut self, target: WinId, k: KeyEvent) -> bool {
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _)
                if self
                    .overlays
                    .search_session()
                    .is_some_and(|session| session.target == target) =>
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
            let Some(mut transcript_session) =
                self.new_transcript_search_session(&query, origin, direction)
            else {
                self.clear_search();
                return;
            };
            let current = self.advance_transcript_search(
                &mut transcript_session,
                &query,
                origin,
                direction,
                None,
            );
            self.overlays.install_search_session(SearchSession {
                target,
                target_buf,
                query,
                direction,
                backend: SearchBackend::Transcript(transcript_session),
            });
            if let Some(matched) = current {
                self.jump_to_transcript_search_match(matched);
            }
            return;
        }

        let current_range = self.search_next_match(target, &query, origin, direction);
        let matches = current_range
            .map(TextRange::Rows)
            .into_iter()
            .collect::<Vec<_>>();
        let current = (!matches.is_empty()).then_some(0);
        let changedtick = self
            .ui
            .buf(target_buf)
            .map(Buffer::changedtick)
            .unwrap_or_default();
        self.overlays.install_search_session(SearchSession {
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
            .overlays
            .search_session()
            .and_then(SearchSession::current_range)
            .and_then(|range| range.rows())
        {
            self.jump_to_search_range(range);
        }
    }

    fn repeat_search(&mut self, target: WinId, reverse: bool) -> bool {
        let Some(session) = self.overlays.search_session() else {
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
            let mut session = self
                .overlays
                .take_search_session()
                .expect("search session exists");
            let query = session.query.clone();
            let matched = match &mut session.backend {
                SearchBackend::Transcript(transcript) => {
                    let current_match = transcript
                        .current
                        .and_then(|index| transcript.matches.get(index).copied());
                    let (origin, mut origin_bound) = match (current_match, direction) {
                        (Some(matched), SearchDirection::Forward) => (
                            matched.range.end,
                            Some(TranscriptSearchBound::inclusive(matched.end_key())),
                        ),
                        (Some(matched), SearchDirection::Backward) => (
                            previous_search_position(matched.range.start),
                            Some(TranscriptSearchBound::before(matched.start_key())),
                        ),
                        (None, _) => (self.search_origin(target).unwrap_or_default(), None),
                    };
                    if !self.sync_transcript_search_session(transcript, &query, origin, direction) {
                        let Some(new_session) =
                            self.new_transcript_search_session(&query, origin, direction)
                        else {
                            self.overlays.install_search_session(session);
                            return false;
                        };
                        *transcript = new_session;
                        origin_bound = None;
                    }
                    self.advance_transcript_search(
                        transcript,
                        &query,
                        origin,
                        direction,
                        origin_bound,
                    )
                }
                SearchBackend::Full { .. } => None,
            };
            let Some(matched) = matched else {
                self.overlays.install_search_session(session);
                return false;
            };
            self.overlays.install_search_session(session);
            self.jump_to_transcript_search_match(matched);
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

        let Some((query, origin)) = self.overlays.search_session().and_then(|session| {
            let SearchBackend::Full {
                matches, current, ..
            } = &session.backend
            else {
                return None;
            };
            let current_range = current
                .and_then(|index| matches.get(index))
                .and_then(TextRange::rows);
            let origin = match (current_range, direction) {
                (Some(range), SearchDirection::Forward) => range.end,
                (Some(range), SearchDirection::Backward)
                    if range.start.row == 0 && range.start.byte_col == 0 =>
                {
                    DocPosition {
                        row: RowIndex::MAX,
                        byte_col: usize::MAX,
                    }
                }
                (Some(range), SearchDirection::Backward) => previous_search_position(range.start),
                (None, _) => self.search_origin(target).unwrap_or_default(),
            };
            Some((session.query.clone(), origin))
        }) else {
            return false;
        };
        let Some(range) = self.search_next_match(target, &query, origin, direction) else {
            return false;
        };
        self.overlays.replace_full_search_match(range);
        self.jump_to_search_range(range);
        true
    }

    fn sync_full_search_session(
        &mut self,
        target: WinId,
        direction: SearchDirection,
    ) -> FullSearchRefresh {
        let Some(session) = self.overlays.search_session() else {
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
        let range = self.search_next_match(target, &query, origin, direction);
        let matches = range.map(TextRange::Rows).into_iter().collect::<Vec<_>>();
        let current = (!matches.is_empty()).then_some(0);
        self.overlays
            .refresh_full_search(matches, current, buf_tick);
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

    /// Finds the next literal, display-row-local match. Queries containing `\n`
    /// are rejected by `submit_search`; multi-line display search would need
    /// row-break-aware scanning and match storage.
    fn search_next_match(
        &mut self,
        win: WinId,
        query: &str,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> Option<DocRange> {
        self.with_display_document_for_win(win, |document| {
            document.search_next_match(
                query,
                origin,
                matches!(direction, SearchDirection::Forward),
                SEARCH_SCAN_ROWS,
            )
        })
        .flatten()
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

    pub(crate) fn update_current_transcript_search_range(
        &mut self,
        target: WinId,
        range: DocRange,
    ) {
        self.overlays
            .update_current_transcript_search_range(target, range);
    }

    fn jump_to_transcript_search_match(
        &mut self,
        matched: crate::app::transcript::TranscriptSearchMatch,
    ) {
        let Some(target) = self.overlays.search_session().map(|session| session.target) else {
            return;
        };
        let window_scroll_before = self.transcript_scroll_top();
        let transcript_width = self.transcript_width() as u16;
        let target_screen_row = self
            .ui
            .win(target)
            .map(|win| {
                crate::app::reveal::target_screen_row_for_reveal(
                    win.scroll_top(),
                    win.viewport.map(|v| v.rect.height).unwrap_or(1).max(1),
                    matched.range.start.row,
                    crate::app::reveal::RevealOptions::avoid_edge_chrome(target),
                )
            })
            .unwrap_or_default();
        let hint = TranscriptProjectionHint::SearchProjectedRow {
            width: transcript_width,
            anchor: matched.anchor,
            start_byte_col: matched.start_byte_col(),
            row: matched.range.start.row,
            prefer_projected_row: false,
        };
        self.record_transcript_scroll_intent_with_hint(
            "search_jump",
            crate::app::transcript_scroll_trace::TranscriptScrollIntent::SearchJump {
                anchor: matched.anchor,
                target_screen_row,
                match_start_byte_col: matched.start_byte_col(),
                match_end_byte_col: matched.end_byte_col(),
            },
            window_scroll_before,
            hint,
        );
    }

    fn jump_to_search_range(&mut self, range: DocRange) {
        let Some(target) = self.overlays.search_session().map(|session| session.target) else {
            return;
        };
        self.reveal_position(
            target,
            range.start,
            crate::app::reveal::RevealOptions::avoid_edge_chrome(target),
        );
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
        matches.extend(crate::smelt_edit::display_row_matches(
            &row,
            absolute_row,
            query,
        ));
    }
    matches
}
