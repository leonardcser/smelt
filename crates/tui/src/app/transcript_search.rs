use crate::app::search::SearchDirection;
use crate::app::transcript::{
    TranscriptSearchBound, TranscriptSearchMatch, TranscriptSearchPositionKey,
};
use crate::app::TuiApp;
use crate::content::transcript_buf::{TranscriptSearchLayout, TranscriptSearchLayoutEntry};
use crate::content::transcript_search_text::descriptor_search_text;
use crate::smelt_edit::{DocPosition, RowIndex};
use std::collections::{HashMap, HashSet};

const SEARCH_TRANSCRIPT_PREFETCH_ENTRIES: usize = 64;
const SEARCH_TRANSCRIPT_PREFETCH_MATCHES: usize = 128;

#[derive(Clone, Debug)]
pub(crate) struct TranscriptSearchSession {
    pub(super) key: TranscriptSearchKey,
    pub(super) total_rows: RowIndex,
    pub(super) candidate_backed: bool,
    pub(super) candidate_blocks: Vec<u64>,
    pub(super) candidates: Vec<usize>,
    pub(super) scanned: Vec<bool>,
    pub(super) scanned_blocks: HashSet<u64>,
    pub(super) matches: Vec<TranscriptSearchMatch>,
    pub(super) current: Option<usize>,
}

#[derive(Clone, Debug)]
pub(super) struct TranscriptSearchIndex {
    key: TranscriptSearchKey,
    entries: Vec<TranscriptSearchEntry>,
    block_entries: HashMap<u64, Vec<usize>>,
    trigrams: HashMap<u32, Vec<usize>>,
}

pub(super) struct TranscriptSearchStore {
    session_id: String,
    db: smelt_store::SessionDb,
}

#[derive(Clone, Debug)]
struct TranscriptSearchEntry {
    block_ids: Vec<u64>,
    first_row: RowIndex,
    rows: RowIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TranscriptSearchKey {
    layout_generation: u64,
    width: u16,
    candidates_hash: u64,
}

#[derive(Clone, Debug, Default)]
struct SqliteTranscriptCandidateBlocks {
    block_indices: Vec<u64>,
    available: bool,
}

impl TranscriptSearchIndex {
    fn candidate_entries(&self, query: &str, preferred_blocks: Option<&[u64]>) -> Vec<usize> {
        let mut out = Vec::new();
        let mut seen = vec![false; self.entries.len()];
        if let Some(preferred_blocks) = preferred_blocks {
            for block_idx in preferred_blocks {
                let Some(entries) = self.block_entries.get(block_idx) else {
                    continue;
                };
                for entry in entries.iter().copied() {
                    if entry < self.entries.len() && !seen[entry] {
                        seen[entry] = true;
                        out.push(entry);
                    }
                }
            }
        }
        let grams = unique_trigrams(query);
        if grams.is_empty() {
            if preferred_blocks.is_none() && out.is_empty() {
                out.extend((0..self.entries.len()).filter(|entry| !seen[*entry]));
            }
            return out;
        }
        let mut postings: Vec<&Vec<usize>> = Vec::with_capacity(grams.len());
        for gram in grams {
            let Some(list) = self.trigrams.get(&gram) else {
                return out;
            };
            postings.push(list);
        }
        postings.sort_by_key(|list| list.len());
        let mut trigram_hits = postings[0].clone();
        for list in postings.into_iter().skip(1) {
            trigram_hits.retain(|entry| list.binary_search(entry).is_ok());
            if trigram_hits.is_empty() {
                break;
            }
        }
        for entry in trigram_hits {
            if entry < self.entries.len() && !seen[entry] {
                seen[entry] = true;
                out.push(entry);
            }
        }
        out
    }
}

fn build_transcript_search_index(
    key: TranscriptSearchKey,
    layout: &TranscriptSearchLayout,
    index_trigrams: bool,
    mut search_text_for_entry: impl FnMut(&TranscriptSearchLayoutEntry) -> String,
) -> TranscriptSearchIndex {
    let mut entries = Vec::with_capacity(layout.entries.len());
    let mut block_entries: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut trigrams: HashMap<u32, Vec<usize>> = HashMap::new();
    for layout_entry in &layout.entries {
        let search_text = index_trigrams.then(|| search_text_for_entry(layout_entry));
        push_search_entry(
            &mut entries,
            &mut block_entries,
            &mut trigrams,
            layout_entry,
            search_text.as_deref(),
        );
    }
    TranscriptSearchIndex {
        key,
        entries,
        block_entries,
        trigrams,
    }
}

fn push_search_entry(
    entries: &mut Vec<TranscriptSearchEntry>,
    block_entries: &mut HashMap<u64, Vec<usize>>,
    trigrams: &mut HashMap<u32, Vec<usize>>,
    layout_entry: &TranscriptSearchLayoutEntry,
    search_text: Option<&str>,
) {
    let entry_index = entries.len();
    entries.push(TranscriptSearchEntry {
        block_ids: layout_entry.block_ids.iter().map(|id| id.get()).collect(),
        first_row: layout_entry.first_row,
        rows: layout_entry.rows,
    });
    for block_id in &layout_entry.block_ids {
        block_entries
            .entry(block_id.get())
            .or_default()
            .push(entry_index);
    }
    if let Some(search_text) = search_text {
        for gram in unique_trigrams(search_text) {
            trigrams.entry(gram).or_default().push(entry_index);
        }
    }
}

fn record_index_size(index: &TranscriptSearchIndex) {
    smelt_perf::perf::record_value(
        "search:transcript:index_entries",
        index.entries.len() as u64,
    );
    smelt_perf::perf::record_value(
        "search:transcript:index_trigrams",
        index.trigrams.len() as u64,
    );
}

impl TuiApp {
    fn transcript_search_key(
        &mut self,
        layout_generation: u64,
        candidate_blocks: &[u64],
    ) -> TranscriptSearchKey {
        TranscriptSearchKey {
            layout_generation,
            width: self.transcript_width() as u16,
            candidates_hash: smelt_core::utils::hash_serializable(&candidate_blocks),
        }
    }

    fn transcript_search_text_for_entry(&self, entry: &TranscriptSearchLayoutEntry) -> String {
        let history = self.transcript.history();
        let mut text = String::new();
        for id in &entry.block_ids {
            let Some(descriptor) = history.descriptor(*id) else {
                continue;
            };
            let tool_state = descriptor
                .tool_call_id()
                .and_then(|call_id| history.tool_state(call_id));
            let block_text = descriptor_search_text(&descriptor, tool_state);
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&block_text);
        }
        text
    }

    fn transcript_search_viewport_rows(&self) -> u16 {
        self.transcript_win()
            .viewport
            .map(|viewport| viewport.rect.height)
            .unwrap_or(20)
            .max(1)
    }

    fn activate_transcript_search_candidate_window(
        &mut self,
        candidate_blocks: &[u64],
        origin: DocPosition,
        direction: SearchDirection,
    ) {
        let origin_block = self.transcript_candidate_origin_block(origin, direction);
        let Some(block_idx) =
            transcript_search_activation_block(candidate_blocks, origin_block, direction)
        else {
            return;
        };
        let width = self.transcript_width() as u16;
        let viewport_rows = self.transcript_search_viewport_rows();
        let _ = self.transcript.activate_descriptor_window_for_block_idx(
            width,
            block_idx,
            viewport_rows,
        );
    }

    fn dirty_transcript_candidate_blocks(&self, query: &str) -> Vec<u64> {
        let Some(start) = self.transcript.history().descriptor_dirty_from() else {
            return Vec::new();
        };
        let _perf = smelt_perf::perf::begin("search:transcript:dirty_candidate_scan");
        let history = self.transcript.history();
        let mut scanned = 0u64;
        let mut out = Vec::new();
        for id in history.order.iter().skip(start.min(history.order.len())) {
            let Some(descriptor) = history.descriptor(*id) else {
                continue;
            };
            scanned = scanned.saturating_add(1);
            let tool_state = descriptor
                .tool_call_id()
                .and_then(|call_id| history.tool_state(call_id));
            if descriptor_search_text(&descriptor, tool_state).contains(query) {
                out.push(id.get());
            }
        }
        smelt_perf::perf::record_value("search:transcript:dirty_candidates_scanned", scanned);
        smelt_perf::perf::record_value(
            "search:transcript:dirty_candidate_blocks",
            out.len() as u64,
        );
        out
    }

    fn transcript_search_store(&mut self) -> Option<&smelt_store::SessionDb> {
        let session_id = self.core.session.id.clone();
        if self
            .search
            .transcript_store
            .as_ref()
            .is_none_or(|store| store.session_id != session_id)
        {
            let db_path = smelt_core::session::dir_for(&self.core.session).join("session.db");
            let db = smelt_store::SessionDb::open_read_only(db_path).ok()?;
            self.search.transcript_store = Some(TranscriptSearchStore { session_id, db });
        }
        self.search.transcript_store.as_ref().map(|store| &store.db)
    }

    fn sqlite_transcript_candidate_blocks(
        &mut self,
        query: &str,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> SqliteTranscriptCandidateBlocks {
        let dirty_blocks = self.dirty_transcript_candidate_blocks(query);
        let width = self.transcript_width() as u16;
        let origin_block = self
            .transcript
            .block_id_at_or_before_row(
                &self.lua,
                width,
                origin.row,
                matches!(direction, SearchDirection::Forward),
            )
            .map(|id| id.get());
        let store_direction = match direction {
            SearchDirection::Forward => smelt_store::TranscriptSearchDirection::Forward,
            SearchDirection::Backward => smelt_store::TranscriptSearchDirection::Backward,
        };
        let limit = SEARCH_TRANSCRIPT_PREFETCH_ENTRIES * 8;
        let descriptors_persisted = self.session_persist.descriptors_persisted;
        let sqlite_candidates = self.transcript_search_store().and_then(|db| {
            let mut page = db
                .search_transcript_candidate_page(query, origin_block, store_direction, limit)
                .ok()?;
            if origin_block.is_some() && page.len() < limit {
                let wrapped = db
                    .search_transcript_candidate_page(
                        query,
                        None,
                        store_direction,
                        limit - page.len(),
                    )
                    .ok()?;
                for candidate in wrapped {
                    if !page
                        .iter()
                        .any(|seen| seen.block_idx == candidate.block_idx)
                    {
                        page.push(candidate);
                    }
                }
            }
            if page.is_empty()
                && !descriptors_persisted
                && db.transcript_descriptor_count().ok() == Some(0)
            {
                return None;
            }
            Some(page)
        });
        let mut block_indices = Vec::new();
        let sqlite_available = sqlite_candidates.is_some();
        if let Some(candidates) = sqlite_candidates {
            smelt_perf::perf::record_value("search:transcript:sqlite_available", 1);
            smelt_perf::perf::record_value(
                "search:transcript:sqlite_candidate_blocks",
                candidates.len() as u64,
            );
            block_indices.extend(candidates.into_iter().map(|candidate| candidate.block_idx));
        } else {
            smelt_perf::perf::record_value("search:transcript:sqlite_available", 0);
        }
        for block_idx in dirty_blocks {
            if !block_indices.contains(&block_idx) {
                block_indices.push(block_idx);
            }
        }
        let available = sqlite_available || !block_indices.is_empty();
        if !available {
            return SqliteTranscriptCandidateBlocks::default();
        }
        SqliteTranscriptCandidateBlocks {
            block_indices,
            available,
        }
    }

    fn ensure_transcript_candidate_index(
        &mut self,
        candidate_blocks: Option<&[u64]>,
    ) -> Option<&TranscriptSearchIndex> {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let layout = {
            let _perf = smelt_perf::perf::begin("search:transcript:candidate_layout");
            match candidate_blocks {
                Some(candidate_blocks) => self
                    .transcript
                    .materialize_exact_loaded_search_layout_for_blocks(
                        &self.lua,
                        width,
                        candidate_blocks,
                    ),
                None => self
                    .transcript
                    .materialize_exact_loaded_search_layout(&self.lua, width),
            }
        };
        let candidate_key = candidate_blocks.unwrap_or(&[]);
        let key = self.transcript_search_key(layout.generation, candidate_key);
        if self
            .search
            .transcript_index
            .as_ref()
            .is_some_and(|index| index.key == key)
        {
            return self.search.transcript_index.as_ref();
        }

        let _perf = smelt_perf::perf::begin("search:transcript:index_build");
        let index_trigrams = candidate_blocks.is_none();
        smelt_perf::perf::record_value(
            "search:transcript:index_trigram_build_enabled",
            u64::from(index_trigrams),
        );
        self.search.transcript_index = Some(build_transcript_search_index(
            key,
            &layout,
            index_trigrams,
            |layout_entry| self.transcript_search_text_for_entry(layout_entry),
        ));
        record_index_size(self.search.transcript_index.as_ref()?);
        self.search.transcript_index.as_ref()
    }

    pub(super) fn new_transcript_search_session(
        &mut self,
        query: &str,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> Option<TranscriptSearchSession> {
        let sqlite_candidates = self.sqlite_transcript_candidate_blocks(query, origin, direction);
        let candidate_backed = sqlite_candidates.available;
        let candidate_blocks = sqlite_candidates.block_indices;
        if candidate_backed {
            self.activate_transcript_search_candidate_window(&candidate_blocks, origin, direction);
        }
        let (key, candidates, scanned_len, indexed_total_rows) = {
            let candidate_blocks_slice = candidate_backed.then_some(candidate_blocks.as_slice());
            let index = self.ensure_transcript_candidate_index(candidate_blocks_slice)?;
            let preferred = candidate_backed.then_some(candidate_blocks.as_slice());
            let indexed_total_rows = index
                .entries
                .iter()
                .map(|entry| entry.first_row.saturating_add(entry.rows))
                .max()
                .unwrap_or(0);
            (
                index.key,
                index.candidate_entries(query, preferred),
                index.entries.len(),
                indexed_total_rows,
            )
        };
        smelt_perf::perf::record_value("search:transcript:candidates", candidates.len() as u64);
        let width = self.transcript_width() as u16;
        let total_rows = self
            .transcript
            .approximate_scrollbar_total_rows(&self.lua, width)
            .max(indexed_total_rows);
        Some(TranscriptSearchSession {
            key,
            total_rows,
            candidate_backed,
            candidate_blocks,
            candidates,
            scanned: vec![false; scanned_len],
            scanned_blocks: HashSet::new(),
            matches: Vec::new(),
            current: None,
        })
    }

    fn transcript_search_session_can_advance(
        &mut self,
        session: &TranscriptSearchSession,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        if session.key.width != self.transcript_width() as u16 {
            return false;
        }
        let origin_bound = self.transcript_search_bound_for_origin(session, origin, direction);
        if self
            .cached_transcript_match(session, origin_bound, direction)
            .is_some()
        {
            return true;
        }
        if session.candidate_backed {
            return self
                .next_unscanned_transcript_candidate_block(session, origin, direction)
                .is_some();
        }
        let current_key = self.search.transcript_index.as_ref().map(|index| index.key);
        current_key == Some(session.key)
            && self
                .next_unscanned_transcript_candidate(session, origin, direction)
                .is_some()
    }

    pub(super) fn sync_transcript_search_session(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> bool {
        if self.transcript_search_session_can_advance(session, origin, direction) {
            return true;
        }
        if session.candidate_backed {
            return false;
        }
        let sqlite_candidates = self.sqlite_transcript_candidate_blocks(query, origin, direction);
        if sqlite_candidates.available {
            self.activate_transcript_search_candidate_window(
                &sqlite_candidates.block_indices,
                origin,
                direction,
            );
        }
        let candidate_blocks = sqlite_candidates
            .available
            .then_some(sqlite_candidates.block_indices.as_slice());
        let Some(key) = self
            .ensure_transcript_candidate_index(candidate_blocks)
            .map(|index| index.key)
        else {
            return false;
        };
        key == session.key
    }

    pub(super) fn advance_transcript_search(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        origin: DocPosition,
        direction: SearchDirection,
        origin_bound: Option<TranscriptSearchBound>,
    ) -> Option<TranscriptSearchMatch> {
        let _perf = smelt_perf::perf::begin("search:transcript:advance");
        if query.is_empty() || session.total_rows == 0 {
            return None;
        }
        let origin = match direction {
            SearchDirection::Forward => origin,
            SearchDirection::Backward => DocPosition {
                row: origin.row.min(session.total_rows.saturating_sub(1)),
                byte_col: origin.byte_col,
            },
        };

        let origin_bound = origin_bound
            .unwrap_or_else(|| self.transcript_search_bound_for_origin(session, origin, direction));

        let current = self
            .cached_transcript_match(session, origin_bound, direction)
            .or_else(|| {
                self.scan_transcript_candidates_until_match(
                    session,
                    query,
                    origin,
                    origin_bound,
                    direction,
                )
            });
        let current = match current {
            Some(index) => Some(index),
            None => {
                let wrap_origin = match direction {
                    SearchDirection::Forward if (origin.row, origin.byte_col) != (0, 0) => {
                        Some(DocPosition::default())
                    }
                    SearchDirection::Backward
                        if origin.row + 1 != session.total_rows
                            || origin.byte_col != usize::MAX =>
                    {
                        Some(DocPosition {
                            row: session.total_rows.saturating_sub(1),
                            byte_col: usize::MAX,
                        })
                    }
                    _ => None,
                }?;
                let wrap_bound =
                    self.transcript_search_bound_for_origin(session, wrap_origin, direction);
                self.scan_transcript_candidates_until_match(
                    session,
                    query,
                    wrap_origin,
                    wrap_bound,
                    direction,
                )
                .or_else(|| self.cached_transcript_match(session, wrap_bound, direction))
            }
        }?;
        session.current = Some(current);
        let matched = session.matches.get(current).copied()?;
        self.prefetch_transcript_matches(session, query, current, matched, direction);
        session.current = session
            .matches
            .iter()
            .position(|candidate| candidate.same_position(&matched))
            .or(Some(current));
        Some(matched)
    }

    pub(super) fn transcript_search_bound_for_origin(
        &mut self,
        session: &TranscriptSearchSession,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> TranscriptSearchBound {
        if matches!(direction, SearchDirection::Backward) && origin.row == RowIndex::MAX {
            return TranscriptSearchBound::inclusive(TranscriptSearchPositionKey::max());
        }
        let row = origin.row.min(session.total_rows.saturating_sub(1));
        let width = self.transcript_width() as u16;
        let anchor = self.transcript.search_anchor_at_row(&self.lua, width, row);
        let key = match (anchor, direction) {
            (
                crate::app::transcript::TranscriptSearchAnchor::EstimatedRow(_),
                SearchDirection::Forward,
            ) => TranscriptSearchPositionKey::min(),
            (
                crate::app::transcript::TranscriptSearchAnchor::EstimatedRow(_),
                SearchDirection::Backward,
            ) => TranscriptSearchPositionKey::max(),
            _ => anchor.position_key(origin.byte_col),
        };
        TranscriptSearchBound::inclusive(key)
    }

    fn cached_transcript_match(
        &self,
        session: &TranscriptSearchSession,
        origin_bound: TranscriptSearchBound,
        direction: SearchDirection,
    ) -> Option<usize> {
        match direction {
            SearchDirection::Forward => session
                .matches
                .iter()
                .position(|matched| origin_bound.contains_forward(matched.start_key())),
            SearchDirection::Backward => session
                .matches
                .iter()
                .rposition(|matched| origin_bound.contains_backward(matched.start_key())),
        }
    }

    fn scan_transcript_candidates_until_match(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        origin: DocPosition,
        origin_bound: TranscriptSearchBound,
        direction: SearchDirection,
    ) -> Option<usize> {
        if session.candidate_backed {
            return self.scan_transcript_candidate_blocks_until_match(
                session,
                query,
                origin,
                origin_bound,
                direction,
            );
        }
        loop {
            let entry_index =
                self.next_unscanned_transcript_candidate(session, origin, direction)?;
            self.scan_transcript_candidate(session, query, entry_index);
            if let Some(index) = self.cached_transcript_match(session, origin_bound, direction) {
                return Some(index);
            }
        }
    }

    fn scan_transcript_candidate_blocks_until_match(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        origin: DocPosition,
        origin_bound: TranscriptSearchBound,
        direction: SearchDirection,
    ) -> Option<usize> {
        loop {
            let block_idx =
                self.next_unscanned_transcript_candidate_block(session, origin, direction)?;
            self.scan_transcript_candidate_block(session, query, block_idx);
            if let Some(index) = self.cached_transcript_match(session, origin_bound, direction) {
                return Some(index);
            }
        }
    }

    fn prefetch_transcript_matches(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        current: usize,
        matched: TranscriptSearchMatch,
        direction: SearchDirection,
    ) {
        if session.candidate_backed {
            // Persisted candidate scans hydrate sparse transcript windows. Keep the
            // active window on the match the user is jumping to; the next repeat
            // can hydrate the next candidate on demand.
            return;
        }
        let target_matches =
            transcript_prefetch_target_len(session.matches.len(), current, direction);
        let mut scanned_entries = 0usize;
        let origin = match direction {
            SearchDirection::Forward => matched.range.end,
            SearchDirection::Backward => previous_search_position(matched.range.start),
        };
        while session.matches.len() < target_matches
            && scanned_entries < SEARCH_TRANSCRIPT_PREFETCH_ENTRIES
        {
            let Some(entry_index) =
                self.next_unscanned_transcript_candidate(session, origin, direction)
            else {
                break;
            };
            self.scan_transcript_candidate(session, query, entry_index);
            scanned_entries += 1;
        }
    }

    fn next_unscanned_transcript_candidate(
        &self,
        session: &TranscriptSearchSession,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> Option<usize> {
        let index = self.search.transcript_index.as_ref()?;
        match direction {
            SearchDirection::Forward => session.candidates.iter().copied().find(|entry_index| {
                !session.scanned.get(*entry_index).copied().unwrap_or(true)
                    && index.entries.get(*entry_index).is_some_and(|entry| {
                        !transcript_entry_scanned(session, entry)
                            && entry.first_row.saturating_add(entry.rows) > origin.row
                    })
            }),
            SearchDirection::Backward => {
                session
                    .candidates
                    .iter()
                    .rev()
                    .copied()
                    .find(|entry_index| {
                        !session.scanned.get(*entry_index).copied().unwrap_or(true)
                            && index.entries.get(*entry_index).is_some_and(|entry| {
                                !transcript_entry_scanned(session, entry)
                                    && entry.first_row <= origin.row
                            })
                    })
            }
        }
    }

    fn next_unscanned_transcript_candidate_block(
        &mut self,
        session: &TranscriptSearchSession,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> Option<u64> {
        let origin_block = self.transcript_candidate_origin_block(origin, direction);
        match direction {
            SearchDirection::Forward => {
                if let Some(origin_block) = origin_block {
                    if let Some(block_idx) =
                        session.candidate_blocks.iter().copied().find(|block_idx| {
                            *block_idx >= origin_block
                                && !session.scanned_blocks.contains(block_idx)
                        })
                    {
                        return Some(block_idx);
                    }
                }
                session
                    .candidate_blocks
                    .iter()
                    .copied()
                    .find(|block_idx| !session.scanned_blocks.contains(block_idx))
            }
            SearchDirection::Backward => {
                if let Some(origin_block) = origin_block {
                    if let Some(block_idx) =
                        session
                            .candidate_blocks
                            .iter()
                            .rev()
                            .copied()
                            .find(|block_idx| {
                                *block_idx <= origin_block
                                    && !session.scanned_blocks.contains(block_idx)
                            })
                    {
                        return Some(block_idx);
                    }
                }
                session
                    .candidate_blocks
                    .iter()
                    .rev()
                    .copied()
                    .find(|block_idx| !session.scanned_blocks.contains(block_idx))
            }
        }
    }

    fn transcript_candidate_origin_block(
        &mut self,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> Option<u64> {
        let width = self.transcript_width() as u16;
        self.transcript
            .block_id_at_or_before_row(
                &self.lua,
                width,
                origin.row,
                matches!(direction, SearchDirection::Forward),
            )
            .map(|id| id.get())
    }

    fn scan_transcript_candidate_block(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        block_idx: u64,
    ) {
        let _perf = smelt_perf::perf::begin("search:transcript:scan_candidate_block");
        if !session.scanned_blocks.insert(block_idx) {
            return;
        }
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let viewport_rows = self.transcript_search_viewport_rows();
        let _ = self.transcript.activate_descriptor_window_for_block_idx(
            width,
            block_idx,
            viewport_rows,
        );
        let layout = self
            .transcript
            .materialize_exact_loaded_search_layout_for_blocks(&self.lua, width, &[block_idx]);
        let theme = self.ui.theme().clone();
        let mut scanned_rows = 0;
        for entry in layout.entries {
            for id in &entry.block_ids {
                session.scanned_blocks.insert(id.get());
            }
            scanned_rows += entry.rows;
            if entry.rows == 0 {
                continue;
            }
            let found = self.transcript.search_matches_for_row_range(
                &self.lua,
                width,
                &theme,
                entry.first_row,
                entry.rows,
                query,
            );
            merge_transcript_matches(&mut session.matches, found);
        }
        smelt_perf::perf::record_value("search:transcript:scanned_entries", 1);
        smelt_perf::perf::record_value("search:transcript:scanned_rows", scanned_rows);
        smelt_perf::perf::record_value(
            "search:transcript:cached_matches",
            session.matches.len() as u64,
        );
    }

    fn scan_transcript_candidate(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        entry_index: usize,
    ) {
        let _perf = smelt_perf::perf::begin("search:transcript:scan_candidate");
        let Some((block_ids, first_row, rows)) = self
            .search
            .transcript_index
            .as_ref()
            .and_then(|index| index.entries.get(entry_index))
            .map(|entry| (entry.block_ids.clone(), entry.first_row, entry.rows))
        else {
            return;
        };
        if let Some(scanned) = session.scanned.get_mut(entry_index) {
            *scanned = true;
        }
        session.scanned_blocks.extend(block_ids);
        smelt_perf::perf::record_value("search:transcript:scanned_entries", 1);
        smelt_perf::perf::record_value("search:transcript:scanned_rows", rows);
        if rows == 0 {
            return;
        }
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        let found = self
            .transcript
            .search_matches_for_row_range(&self.lua, width, &theme, first_row, rows, query);
        merge_transcript_matches(&mut session.matches, found);
        smelt_perf::perf::record_value(
            "search:transcript:cached_matches",
            session.matches.len() as u64,
        );
    }
}

fn transcript_entry_scanned(
    session: &TranscriptSearchSession,
    entry: &TranscriptSearchEntry,
) -> bool {
    !entry.block_ids.is_empty()
        && entry
            .block_ids
            .iter()
            .all(|block_idx| session.scanned_blocks.contains(block_idx))
}

fn transcript_search_activation_block(
    candidate_blocks: &[u64],
    origin_block: Option<u64>,
    direction: SearchDirection,
) -> Option<u64> {
    match direction {
        SearchDirection::Forward => origin_block
            .and_then(|origin_block| {
                candidate_blocks
                    .iter()
                    .copied()
                    .find(|block_idx| *block_idx >= origin_block)
            })
            .or_else(|| candidate_blocks.first().copied()),
        SearchDirection::Backward => origin_block
            .and_then(|origin_block| {
                candidate_blocks
                    .iter()
                    .rev()
                    .copied()
                    .find(|block_idx| *block_idx <= origin_block)
            })
            .or_else(|| candidate_blocks.last().copied()),
    }
}

fn merge_transcript_matches(
    matches: &mut Vec<TranscriptSearchMatch>,
    ranges: impl IntoIterator<Item = TranscriptSearchMatch>,
) {
    matches.extend(ranges);
    matches.sort_by_key(TranscriptSearchMatch::sort_key);
    matches.dedup_by(|a, b| a.same_position(b));
}

fn transcript_prefetch_target_len(
    cached_matches: usize,
    current: usize,
    direction: SearchDirection,
) -> usize {
    match direction {
        SearchDirection::Forward => current
            .saturating_add(1)
            .saturating_add(SEARCH_TRANSCRIPT_PREFETCH_MATCHES),
        SearchDirection::Backward => {
            cached_matches.saturating_add(SEARCH_TRANSCRIPT_PREFETCH_MATCHES)
        }
    }
}

pub(super) fn previous_search_position(pos: DocPosition) -> DocPosition {
    if pos.byte_col == 0 {
        DocPosition {
            row: pos.row.saturating_sub(1),
            byte_col: usize::MAX,
        }
    } else {
        DocPosition {
            row: pos.row,
            byte_col: pos.byte_col - 1,
        }
    }
}

fn trigram(bytes: &[u8]) -> u32 {
    ((bytes[0] as u32) << 16) | ((bytes[1] as u32) << 8) | bytes[2] as u32
}

fn unique_trigrams(text: &str) -> Vec<u32> {
    let bytes = text.as_bytes();
    if bytes.len() < 3 {
        return Vec::new();
    }
    let mut grams: Vec<u32> = bytes.windows(3).map(trigram).collect();
    grams.sort_unstable();
    grams.dedup();
    grams
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::transcript_model::BlockId;

    fn test_index(texts: &[&str]) -> TranscriptSearchIndex {
        let mut entries = Vec::new();
        let mut block_entries = HashMap::new();
        let mut trigrams = HashMap::new();
        for (i, search_text) in texts.iter().enumerate() {
            let id = BlockId::new(i as u64);
            let layout_entry = TranscriptSearchLayoutEntry {
                block_ids: vec![id],
                first_row: i as RowIndex,
                rows: 1,
            };
            push_search_entry(
                &mut entries,
                &mut block_entries,
                &mut trigrams,
                &layout_entry,
                Some(search_text),
            );
        }
        TranscriptSearchIndex {
            key: TranscriptSearchKey {
                layout_generation: 1,
                width: 80,
                candidates_hash: 0,
            },
            entries,
            block_entries,
            trigrams,
        }
    }

    #[test]
    fn forward_prefetch_targets_matches_ahead_of_current_match() {
        assert_eq!(
            transcript_prefetch_target_len(1_000, 10, SearchDirection::Forward),
            10 + 1 + SEARCH_TRANSCRIPT_PREFETCH_MATCHES
        );
        assert_eq!(
            transcript_prefetch_target_len(1_000, 900, SearchDirection::Forward),
            900 + 1 + SEARCH_TRANSCRIPT_PREFETCH_MATCHES
        );
    }

    #[test]
    fn backward_prefetch_keeps_existing_batch_growth() {
        assert_eq!(
            transcript_prefetch_target_len(32, 10, SearchDirection::Backward),
            32 + SEARCH_TRANSCRIPT_PREFETCH_MATCHES
        );
    }

    #[test]
    fn candidate_entries_do_not_append_everything_after_trigram_hits() {
        let index = test_index(&["alpha needle", "renderer only", "other text"]);
        assert_eq!(index.candidate_entries("needle", None), vec![0]);
        assert!(index.candidate_entries("absent", None).is_empty());
    }

    #[test]
    fn candidate_entries_keep_sqlite_preferred_without_full_fallback() {
        let index = test_index(&["alpha", "renderer only", "other text"]);
        assert_eq!(index.candidate_entries("absent", Some(&[1])), vec![1]);
        assert_eq!(index.candidate_entries("al", Some(&[0])), vec![0]);
    }

    #[test]
    fn candidate_entries_empty_sqlite_result_stops_short_query_full_scan() {
        let index = test_index(&["alpha", "renderer only", "other text"]);
        assert_eq!(
            index.candidate_entries("al", Some(&[])),
            Vec::<usize>::new()
        );
        assert_eq!(index.candidate_entries("al", None), vec![0, 1, 2]);
    }

    #[test]
    fn sqlite_block_candidates_map_to_render_entries() {
        let mut entries = Vec::new();
        let mut block_entries = HashMap::new();
        let mut trigrams = HashMap::new();
        let layout_entry = TranscriptSearchLayoutEntry {
            block_ids: vec![BlockId::new(7), BlockId::new(8)],
            first_row: 0,
            rows: 2,
        };
        push_search_entry(
            &mut entries,
            &mut block_entries,
            &mut trigrams,
            &layout_entry,
            Some("group text"),
        );
        let index = TranscriptSearchIndex {
            key: TranscriptSearchKey {
                layout_generation: 1,
                width: 80,
                candidates_hash: 0,
            },
            entries,
            block_entries,
            trigrams,
        };
        assert_eq!(index.candidate_entries("absent", Some(&[8])), vec![0]);
    }
}
