use crate::app::search::SearchDirection;
use crate::app::transcript::{
    TranscriptSearchBound, TranscriptSearchMatch, TranscriptSearchPositionKey,
};
use crate::app::TuiApp;
use crate::content::transcript_buf::{TranscriptSearchLayout, TranscriptSearchLayoutEntry};
use crate::smelt_edit::{DocPosition, RowIndex};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const SEARCH_TRANSCRIPT_PREFETCH_ENTRIES: usize = 64;
const SEARCH_TRANSCRIPT_PREFETCH_MATCHES: usize = 128;

#[derive(Clone, Debug)]
pub(crate) struct TranscriptSearchSession {
    pub(super) key: TranscriptSearchKey,
    pub(super) total_rows: RowIndex,
    pub(super) candidate_backed: bool,
    origin_block_idx: Option<u64>,
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
    store_address: super::transcript::TranscriptStoreAddress,
    reader: smelt_store::LineageSessionReader,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptSearchContext {
    pub(crate) session_id: String,
}

#[derive(Debug)]
pub struct TranscriptSearchWorkerResult {
    pub(crate) generation: u64,
    pub(crate) context: TranscriptSearchContext,
    pub(crate) query: String,
    pub(crate) direction: SearchDirection,
    pub(crate) candidates: SqliteTranscriptCandidateBlocks,
}

#[derive(Clone, Debug)]
pub(super) struct TranscriptSearchWorkerRequest {
    pub(super) generation: u64,
    pub(super) context: TranscriptSearchContext,
    pub(super) store_address: super::transcript::TranscriptStoreAddress,
    pub(super) query: String,
    pub(super) origin_block_idx: Option<u64>,
    pub(super) direction: SearchDirection,
}

#[derive(Default)]
struct TranscriptSearchWorkerState {
    pending: Option<TranscriptSearchWorkerRequest>,
    shutdown: bool,
}

struct TranscriptSearchWorkerShared {
    state: Mutex<TranscriptSearchWorkerState>,
    changed: Condvar,
    latest_generation: AtomicU64,
    #[cfg(test)]
    delay_ms: AtomicU64,
}

pub(super) struct TranscriptSearchWorker {
    shared: Arc<TranscriptSearchWorkerShared>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TranscriptSearchWorker {
    pub(super) fn spawn(
        event_tx: tokio::sync::mpsc::UnboundedSender<crate::app::AppEvent>,
    ) -> Self {
        let shared = Arc::new(TranscriptSearchWorkerShared {
            state: Mutex::new(TranscriptSearchWorkerState::default()),
            changed: Condvar::new(),
            latest_generation: AtomicU64::new(0),
            #[cfg(test)]
            delay_ms: AtomicU64::new(0),
        });
        let worker_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("smelt-transcript-search".into())
            .spawn(move || transcript_search_worker_loop(worker_shared, event_tx))
            .expect("failed to spawn transcript search worker");
        Self {
            shared,
            thread: Some(thread),
        }
    }

    pub(super) fn request(&self, request: TranscriptSearchWorkerRequest) {
        self.shared
            .latest_generation
            .store(request.generation, Ordering::Release);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending = Some(request);
        self.shared.changed.notify_one();
    }

    pub(super) fn cancel(&self, generation: u64) {
        self.shared
            .latest_generation
            .store(generation, Ordering::Release);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending = None;
        self.shared.changed.notify_one();
    }

    #[cfg(test)]
    pub(super) fn set_delay(&self, delay: std::time::Duration) {
        self.shared.delay_ms.store(
            u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
    }
}

impl Drop for TranscriptSearchWorker {
    fn drop(&mut self) {
        self.shared.latest_generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown = true;
        state.pending = None;
        self.shared.changed.notify_one();
        drop(state);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn transcript_search_worker_loop(
    shared: Arc<TranscriptSearchWorkerShared>,
    event_tx: tokio::sync::mpsc::UnboundedSender<crate::app::AppEvent>,
) {
    loop {
        let request = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.pending.is_none() && !state.shutdown {
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
            if state.shutdown {
                return;
            }
            state.pending.take().expect("pending search request")
        };

        #[cfg(test)]
        {
            let mut remaining = shared.delay_ms.load(Ordering::Acquire);
            while remaining > 0 {
                if shared.latest_generation.load(Ordering::Acquire) != request.generation {
                    break;
                }
                let sleep_ms = remaining.min(5);
                thread::sleep(std::time::Duration::from_millis(sleep_ms));
                remaining -= sleep_ms;
            }
        }
        if shared.latest_generation.load(Ordering::Acquire) != request.generation {
            continue;
        }

        let cancelled = || shared.latest_generation.load(Ordering::Acquire) != request.generation;
        let candidates = match persistent_transcript_candidate_blocks(&request, &cancelled) {
            Ok(candidates) => candidates,
            Err(smelt_store::StoreError::Cancelled) => continue,
            Err(_) => SqliteTranscriptCandidateBlocks::default(),
        };
        if cancelled() {
            continue;
        }
        if event_tx
            .send(crate::app::AppEvent::TranscriptSearchCompleted(
                TranscriptSearchWorkerResult {
                    generation: request.generation,
                    context: request.context,
                    query: request.query,
                    direction: request.direction,
                    candidates,
                },
            ))
            .is_err()
        {
            return;
        }
    }
}

fn persistent_transcript_candidate_blocks(
    request: &TranscriptSearchWorkerRequest,
    cancelled: &dyn Fn() -> bool,
) -> smelt_store::Result<SqliteTranscriptCandidateBlocks> {
    let store = TranscriptSearchStore::open(request.store_address.clone())?;
    let store_direction = match request.direction {
        SearchDirection::Forward => smelt_store::TranscriptSearchDirection::Forward,
        SearchDirection::Backward => smelt_store::TranscriptSearchDirection::Backward,
    };
    let limit = SEARCH_TRANSCRIPT_PREFETCH_ENTRIES * 8;
    let mut page = store.search_candidate_page(
        &request.query,
        request.origin_block_idx,
        store_direction,
        limit,
        cancelled,
    )?;
    if request.origin_block_idx.is_some() && page.len() < limit {
        let wrapped = store.search_candidate_page(
            &request.query,
            None,
            store_direction,
            limit - page.len(),
            cancelled,
        )?;
        for candidate in wrapped {
            if !page
                .iter()
                .any(|seen| seen.block_idx == candidate.block_idx)
            {
                page.push(candidate);
            }
        }
    }
    Ok(SqliteTranscriptCandidateBlocks {
        block_indices: page
            .into_iter()
            .map(|candidate| candidate.block_idx)
            .collect(),
        available: true,
    })
}

impl TranscriptSearchStore {
    fn open(store_address: super::transcript::TranscriptStoreAddress) -> smelt_store::Result<Self> {
        let reader = smelt_store::LineageSessionReader::try_open_existing(
            &store_address.sessions_root,
            &store_address.session_id,
        )?
        .ok_or_else(|| {
            smelt_store::StoreError::Integrity("lineage session does not exist".into())
        })?;
        Ok(Self {
            store_address,
            reader,
        })
    }

    fn search_candidate_page(
        &self,
        query: &str,
        origin_block_idx: Option<u64>,
        direction: smelt_store::TranscriptSearchDirection,
        limit: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> smelt_store::Result<Vec<smelt_store::TranscriptSearchCandidate>> {
        self.reader
            .search_transcript_candidate_page_with_cancellation(
                query,
                origin_block_idx,
                direction,
                limit,
                cancelled,
            )
    }

    fn record_count(&self) -> smelt_store::Result<usize> {
        usize::try_from(self.reader.snapshot()?.transcript_len).map_err(|_| {
            smelt_store::StoreError::Integrity(
                "lineage transcript length exceeds platform limits".into(),
            )
        })
    }
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
pub(crate) struct SqliteTranscriptCandidateBlocks {
    pub(crate) block_indices: Vec<u64>,
    pub(crate) available: bool,
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
    mut indexed_text_for_entry: impl FnMut(&TranscriptSearchLayoutEntry) -> String,
) -> TranscriptSearchIndex {
    let mut entries = Vec::with_capacity(layout.entries.len());
    let mut block_entries: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut trigrams: HashMap<u32, Vec<usize>> = HashMap::new();
    for layout_entry in &layout.entries {
        let indexed_text = index_trigrams.then(|| indexed_text_for_entry(layout_entry));
        push_search_entry(
            &mut entries,
            &mut block_entries,
            &mut trigrams,
            layout_entry,
            indexed_text.as_deref(),
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
    indexed_text: Option<&str>,
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
    if let Some(indexed_text) = indexed_text {
        for gram in unique_trigrams(indexed_text) {
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

    fn transcript_indexed_text_for_entry(&self, entry: &TranscriptSearchLayoutEntry) -> String {
        let history = self.conversation.transcript().history();
        let mut text = String::new();
        for id in &entry.block_ids {
            let Some(record) = history.cloned_block(*id) else {
                continue;
            };
            let tool_state = history.tool_state(*id);
            let block_text =
                smelt_core::transcript_model::transcript_indexed_text(&record, tool_state)
                    .indexed_text;
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
        preferred_origin_block: Option<u64>,
    ) {
        let origin_block = preferred_origin_block
            .or_else(|| self.transcript_candidate_origin_block(origin, direction));
        let Some(block_idx) =
            transcript_search_activation_block(candidate_blocks, origin_block, direction)
        else {
            return;
        };
        self.conversation.set_transcript_search_hydration_pin(Some(
            smelt_core::transcript_model::BlockId::new(block_idx),
        ));
        let width = self.transcript_width() as u16;
        let viewport_rows = self.transcript_search_viewport_rows();
        let _ = self.conversation.activate_transcript_search_record_window(
            width,
            block_idx,
            viewport_rows,
        );
    }

    pub(super) fn dirty_transcript_candidate_blocks(&self, query: &str) -> Vec<u64> {
        let Some(start) = self.conversation.transcript().history().record_dirty_from() else {
            return Vec::new();
        };
        let _perf = smelt_perf::perf::begin("search:transcript:dirty_candidate_scan");
        let history = self.conversation.transcript().history();
        let mut scanned = 0u64;
        let mut out = Vec::new();
        for id in history.order.iter().skip(start.min(history.order.len())) {
            let Some(record) = history.cloned_block(*id) else {
                continue;
            };
            scanned = scanned.saturating_add(1);
            let tool_state = history.tool_state(*id);
            if smelt_core::transcript_model::transcript_indexed_text(&record, tool_state)
                .indexed_text
                .contains(query)
            {
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

    fn transcript_search_store(&mut self) -> Option<&TranscriptSearchStore> {
        let store_address = self.conversation.transcript().store_address()?.clone();
        if self
            .overlays
            .transcript_search_store()
            .is_none_or(|store| store.store_address != store_address)
        {
            let store = TranscriptSearchStore::open(store_address).ok()?;
            self.overlays.install_transcript_search_store(store);
        }
        self.overlays.transcript_search_store()
    }

    fn sqlite_transcript_candidate_blocks(
        &mut self,
        query: &str,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> SqliteTranscriptCandidateBlocks {
        smelt_perf::perf::record_value(
            "search:transcript:projection_requested",
            u64::from(self.conversation.request_search_projection()),
        );
        let dirty_blocks = self.dirty_transcript_candidate_blocks(query);
        let width = self.transcript_width() as u16;
        let origin_block = self
            .conversation
            .transcript_search_block_at_row(
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
        let records_persisted = self.conversation.transcript_records_persisted();
        let sqlite_candidates = self.transcript_search_store().and_then(|db| {
            let mut page = db
                .search_candidate_page(query, origin_block, store_direction, limit, &|| false)
                .ok()?;
            if origin_block.is_some() && page.len() < limit {
                let wrapped = db
                    .search_candidate_page(
                        query,
                        None,
                        store_direction,
                        limit - page.len(),
                        &|| false,
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
            if page.is_empty() && !records_persisted && db.record_count().ok() == Some(0) {
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
            self.conversation.materialize_transcript_search_layout(
                &self.lua,
                width,
                candidate_blocks,
            )
        };
        let candidate_key = candidate_blocks.unwrap_or(&[]);
        let key = self.transcript_search_key(layout.generation, candidate_key);
        if self
            .overlays
            .transcript_search_index()
            .is_some_and(|index| index.key == key)
        {
            return self.overlays.transcript_search_index();
        }

        let _perf = smelt_perf::perf::begin("search:transcript:index_build");
        let index_trigrams = candidate_blocks.is_none();
        smelt_perf::perf::record_value(
            "search:transcript:index_trigram_build_enabled",
            u64::from(index_trigrams),
        );
        let index = build_transcript_search_index(key, &layout, index_trigrams, |layout_entry| {
            self.transcript_indexed_text_for_entry(layout_entry)
        });
        record_index_size(&index);
        self.overlays.install_transcript_search_index(index);
        self.overlays.transcript_search_index()
    }

    pub(super) fn new_transcript_search_session(
        &mut self,
        query: &str,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> Option<TranscriptSearchSession> {
        let sqlite_candidates = self.sqlite_transcript_candidate_blocks(query, origin, direction);
        self.new_transcript_search_session_with_candidates(
            query,
            origin,
            direction,
            sqlite_candidates,
            None,
        )
    }

    pub(super) fn new_transcript_search_session_with_candidates(
        &mut self,
        query: &str,
        origin: DocPosition,
        direction: SearchDirection,
        sqlite_candidates: SqliteTranscriptCandidateBlocks,
        preferred_origin_block: Option<u64>,
    ) -> Option<TranscriptSearchSession> {
        let candidate_backed = sqlite_candidates.available;
        let candidate_blocks = sqlite_candidates.block_indices;
        let origin_block_idx = preferred_origin_block
            .or_else(|| self.transcript_candidate_origin_block(origin, direction));
        if candidate_backed {
            self.activate_transcript_search_candidate_window(
                &candidate_blocks,
                origin,
                direction,
                origin_block_idx,
            );
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
            .conversation
            .transcript_search_total_rows(&self.lua, width)
            .max(indexed_total_rows);
        Some(TranscriptSearchSession {
            key,
            total_rows,
            candidate_backed,
            origin_block_idx,
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
        let current_key = self
            .overlays
            .transcript_search_index()
            .map(|index| index.key);
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
                None,
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
        let anchor = self
            .conversation
            .transcript_search_anchor_at_row(&self.lua, width, row);
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
        let index = self.overlays.transcript_search_index()?;
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
        let origin_block = session
            .origin_block_idx
            .or_else(|| self.transcript_candidate_origin_block(origin, direction));
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
        self.conversation
            .transcript_search_block_at_row(
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
        self.conversation.set_transcript_search_hydration_pin(Some(
            smelt_core::transcript_model::BlockId::new(block_idx),
        ));
        let width = self.transcript_width() as u16;
        let viewport_rows = self.transcript_search_viewport_rows();
        let _ = self.conversation.activate_transcript_search_record_window(
            width,
            block_idx,
            viewport_rows,
        );
        let layout = self.conversation.materialize_transcript_search_layout(
            &self.lua,
            width,
            Some(&[block_idx]),
        );
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
            let found = self.conversation.transcript_search_matches_for_row_range(
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
            .overlays
            .transcript_search_index()
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
        let found = self.conversation.transcript_search_matches_for_row_range(
            &self.lua, width, &theme, first_row, rows, query,
        );
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

    #[test]
    fn dropping_search_worker_cancels_and_joins_in_flight_work() {
        let store_address = tempfile::tempdir().unwrap();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = TranscriptSearchWorker::spawn(event_tx);
        worker.set_delay(std::time::Duration::from_secs(30));
        worker.request(TranscriptSearchWorkerRequest {
            generation: 1,
            context: TranscriptSearchContext {
                session_id: "1".repeat(64),
            },
            store_address: crate::app::transcript::TranscriptStoreAddress::new(
                store_address.path().to_path_buf(),
                "1".repeat(64),
                "1".repeat(64),
            ),
            query: "needle".into(),
            origin_block_idx: None,
            direction: SearchDirection::Forward,
        });
        std::thread::sleep(std::time::Duration::from_millis(20));

        let started = std::time::Instant::now();
        drop(worker);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "search worker did not cooperatively stop during drop"
        );
    }

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

    #[test]
    fn sparse_search_hydrates_only_the_match_and_rereads_after_eviction() {
        use crate::app::test_harness::TestApp;
        use crate::app::transcript::TranscriptMemoryBudget;
        use crate::app::{AppFocus, TRANSCRIPT_WIN};
        use crate::smelt_edit::VimMode;

        const MATCH_INDEX: usize = 42;
        const QUERY: &str = "phase-five-unique-search-target";

        let mut app = TestApp::builder().with_vim(true).build();
        app.app.handle_resize(100, 32);
        for index in 0..700 {
            let marker = if index == MATCH_INDEX {
                QUERY
            } else {
                "ordinary"
            };
            app.app
                .push_block(smelt_core::transcript_model::Block::Text {
                    content: format!("search block {index}: {marker}").into(),
                });
        }
        let match_id = app
            .app
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .copied()
            .find(|id| {
                app.app
                    .conversation
                    .transcript()
                    .history()
                    .block(*id)
                    .and_then(smelt_core::transcript_model::Block::raw_text)
                    .is_some_and(|text| text.contains(QUERY))
            })
            .expect("search target block");
        app.app.save_session_and_flush();
        let loaded = crate::app::history::load_transcript_tail_from_sqlite_store(
            app.app.core.sessions.sessions_dir(),
            app.app.conversation.session().id.clone(),
            100,
            32,
        )
        .expect("load sparse transcript");
        app.app.clear_transcript();
        app.app
            .conversation
            .replace_transcript_document_for_harness(
                crate::app::transcript::TranscriptDocument::from_loaded_transcript(loaded),
            );
        app.app
            .conversation
            .set_transcript_memory_budget_for_harness(TranscriptMemoryBudget {
                hydrated_blocks: 1,
                ..Default::default()
            });
        app.app.app_focus = AppFocus::Content;
        app.app.ui.set_focus(TRANSCRIPT_WIN);
        app.app.transcript_win_mut().set_vim_enabled(true);
        app.app.transcript_win_mut().set_vim_mode(VimMode::Normal);
        app.render_silent();

        app.app
            .submit_search(TRANSCRIPT_WIN, SearchDirection::Forward, QUERY.to_string());
        app.render_silent();
        let first = app
            .app
            .conversation
            .transcript_memory_snapshot_for_harness();
        assert!(first.hydration_reads > 0);
        assert!(
            first.hydration_reads <= 64,
            "candidate verification exceeded one viewport-sized neighborhood: {first:?}"
        );
        assert!(app
            .app
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .filter_map(|id| app.app.conversation.transcript().history().block(*id))
            .filter_map(smelt_core::transcript_model::Block::raw_text)
            .any(|text| text.contains(QUERY)));

        app.render_silent();
        assert_eq!(
            app.app
                .conversation
                .transcript_memory_snapshot_for_harness()
                .hydration_reads,
            first.hydration_reads,
            "render reread a block that remained in the viewport working set"
        );

        app.app.clear_search();
        assert!(!app.app.reveal_transcript_record_block(699, 0, true));
        app.render_silent();
        assert!(app.app.reveal_transcript_target_at_top(
            699,
            smelt_core::transcript_model::BlockId::new(699),
            0,
            true,
        ));
        app.render_silent();
        assert!(!app
            .app
            .conversation
            .transcript()
            .history()
            .is_materialized(match_id));
        let before_reveal = app
            .app
            .conversation
            .transcript_memory_snapshot_for_harness();

        app.app
            .submit_search(TRANSCRIPT_WIN, SearchDirection::Forward, QUERY.to_string());
        app.render_silent();
        let revealed = app
            .app
            .conversation
            .transcript_memory_snapshot_for_harness();
        assert!(revealed.hydration_reads > before_reveal.hydration_reads);
        assert!(
            revealed.hydration_reads - before_reveal.hydration_reads <= 64,
            "search reveal exceeded one viewport-sized neighborhood: before={before_reveal:?}, after={revealed:?}"
        );
        assert!(app
            .app
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .filter_map(|id| app.app.conversation.transcript().history().block(*id))
            .filter_map(smelt_core::transcript_model::Block::raw_text)
            .any(|text| text.contains(QUERY)));
    }
}
