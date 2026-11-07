use crate::app::search::SearchDirection;
use crate::app::transcript::{
    TranscriptSearchBound, TranscriptSearchMatch, TranscriptSearchPositionKey,
};
use crate::app::TuiApp;
use crate::smelt_edit::{DocPosition, RowIndex, SearchPattern};
use smelt_view::transcript_buf::{TranscriptSearchLayout, TranscriptSearchLayoutEntry};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use super::latest_worker::{LatestWorker, Mailbox};

const SEARCH_TRANSCRIPT_PREFETCH_ENTRIES: usize = 64;
const SEARCH_TRANSCRIPT_PREFETCH_MATCHES: usize = 128;

/// Indexed text of the unsaved transcript suffix, reused across keystrokes
/// while the history is unchanged.
#[derive(Debug)]
pub(crate) struct DirtyTranscriptText {
    generation: u64,
    start: usize,
    blocks: Vec<(smelt_transcript::transcript_model::BlockId, Box<str>)>,
}

impl DirtyTranscriptText {
    fn build(history: &smelt_transcript::transcript_model::BlockHistory, start: usize) -> Self {
        let blocks = history
            .order
            .iter()
            .skip(start.min(history.order.len()))
            .filter_map(|id| {
                let record = history.block(*id)?;
                let indexed = smelt_transcript::transcript_model::transcript_indexed_text(
                    record,
                    history.tool_state(*id),
                )
                .indexed_text;
                Some((*id, indexed.into_boxed_str()))
            })
            .collect();
        Self {
            generation: history.generation(),
            start,
            blocks,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptSearchSession {
    pub(super) key: TranscriptSearchKey,
    pub(super) total_rows: RowIndex,
    pub(super) candidate_backed: bool,
    /// The block a candidate walk starts from before any match is current.
    origin_block_idx: Option<u64>,
    /// Matches at or past where the search started, fixed when it began.
    start_bound: TranscriptSearchBound,
    candidate_blocks: TranscriptCandidateBlocks,
    /// Matches of each scanned candidate block, in document order.
    block_matches: HashMap<u64, Vec<TranscriptSearchMatch>>,
    pub(super) candidates: Vec<usize>,
    pub(super) scanned: Vec<bool>,
    pub(super) scanned_blocks: HashSet<u64>,
    pub(super) matches: Vec<TranscriptSearchMatch>,
    pub(super) current: Option<usize>,
}

/// Blocks the session store reports holding a query, with the block ranges
/// over which that list is complete. A walk that leaves the covered ranges
/// loads the next page from the store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TranscriptCandidateBlocks {
    blocks: BTreeSet<u64>,
    /// Sorted, disjoint, non-adjacent block ranges. An end of `u64::MAX`
    /// covers every block from the start on.
    covered: Vec<Range<u64>>,
}

/// Where a walk over candidate blocks goes next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateStep {
    Block(u64),
    /// The candidates from this block on are unknown until a page loads.
    Load(u64),
    /// No candidate lies further that way.
    End,
}

impl TranscriptCandidateBlocks {
    /// Candidates known for every block.
    pub(crate) fn complete(blocks: impl IntoIterator<Item = u64>) -> Self {
        Self {
            blocks: blocks.into_iter().collect(),
            covered: std::iter::once(0..u64::MAX).collect(),
        }
    }

    /// Adds a store page of at most `limit` candidates found from `origin`
    /// toward `direction`. A short page covers every block that way.
    pub(crate) fn insert_page(
        &mut self,
        origin: u64,
        direction: SearchDirection,
        page: &[u64],
        limit: usize,
    ) {
        self.blocks.extend(page.iter().copied());
        let full = page.len() >= limit;
        let range = match direction {
            SearchDirection::Forward => {
                let end = match page.iter().max() {
                    Some(last) if full => last.saturating_add(1),
                    _ => u64::MAX,
                };
                origin..end
            }
            SearchDirection::Backward => {
                let start = match page.iter().min() {
                    Some(first) if full => *first,
                    _ => 0,
                };
                start..origin.saturating_add(1)
            }
        };
        self.cover(range);
    }

    /// Adds blocks known to hold the query, such as unsaved ones the store
    /// cannot report.
    pub(crate) fn insert_blocks(&mut self, blocks: impl IntoIterator<Item = u64>) {
        self.blocks.extend(blocks);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = u64> + '_ {
        self.blocks.iter().copied()
    }

    fn cover(&mut self, range: Range<u64>) {
        if range.start >= range.end {
            return;
        }
        let mut merged = range;
        self.covered.retain(|covered| {
            let touches = covered.start <= merged.end && merged.start <= covered.end;
            if touches {
                merged = merged.start.min(covered.start)..merged.end.max(covered.end);
            }
            !touches
        });
        let at = self
            .covered
            .partition_point(|covered| covered.start < merged.start);
        self.covered.insert(at, merged);
    }

    fn covering(&self, block: u64) -> Option<Range<u64>> {
        self.covered
            .iter()
            .find(|range| range.start <= block && (block < range.end || range.end == u64::MAX))
            .cloned()
    }

    /// The first candidate at `from` or past it toward `direction`.
    fn step(&self, from: u64, direction: SearchDirection) -> CandidateStep {
        let Some(range) = self.covering(from) else {
            return CandidateStep::Load(from);
        };
        match direction {
            SearchDirection::Forward => match self.blocks.range(from..).next() {
                Some(block) if range.end == u64::MAX || *block < range.end => {
                    CandidateStep::Block(*block)
                }
                _ if range.end == u64::MAX => CandidateStep::End,
                _ => CandidateStep::Load(range.end),
            },
            SearchDirection::Backward => match self.blocks.range(range.start..=from).next_back() {
                Some(block) => CandidateStep::Block(*block),
                None if range.start == 0 => CandidateStep::End,
                None => CandidateStep::Load(range.start - 1),
            },
        }
    }
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
    reader: smelt_store::SessionReader,
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

pub(super) type TranscriptSearchWorker = LatestWorker<TranscriptSearchWorkerRequest>;

impl TranscriptSearchWorker {
    pub(super) fn spawn(
        event_tx: tokio::sync::mpsc::UnboundedSender<crate::app::AppEvent>,
    ) -> Self {
        Self::start("smelt-transcript-search", move |mailbox| {
            transcript_search_worker_loop(mailbox, event_tx)
        })
    }
}

fn transcript_search_worker_loop(
    mailbox: Arc<Mailbox<TranscriptSearchWorkerRequest>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<crate::app::AppEvent>,
) {
    let mut store: Option<TranscriptSearchStore> = None;
    while let Some((token, request)) = mailbox.recv() {
        let cancelled = || token.cancelled();
        if store
            .as_ref()
            .is_none_or(|store| store.store_address != request.store_address)
        {
            store = TranscriptSearchStore::open(request.store_address.clone()).ok();
        }
        let candidates = match store
            .as_ref()
            .map(|store| persistent_transcript_candidate_blocks(store, &request))
        {
            Some(Ok(candidates)) => candidates,
            Some(Err(_)) | None => SqliteTranscriptCandidateBlocks::default(),
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
    store: &TranscriptSearchStore,
    request: &TranscriptSearchWorkerRequest,
) -> smelt_store::Result<SqliteTranscriptCandidateBlocks> {
    let mut candidates = TranscriptCandidateBlocks::default();
    store.load_candidate_page(
        &mut candidates,
        SearchPattern::parse(&request.query).needle,
        request.origin_block_idx,
        request.direction,
    )?;
    Ok(SqliteTranscriptCandidateBlocks {
        candidates,
        available: true,
    })
}

impl TranscriptSearchStore {
    fn open(store_address: super::transcript::TranscriptStoreAddress) -> smelt_store::Result<Self> {
        let reader = store_address.reader()?;
        Ok(Self {
            store_address,
            reader,
        })
    }

    /// Loads the page of candidates for `needle` from `origin` toward
    /// `direction`, from the transcript edge without an origin.
    fn load_candidate_page(
        &self,
        candidates: &mut TranscriptCandidateBlocks,
        needle: &str,
        origin: Option<u64>,
        direction: SearchDirection,
    ) -> smelt_store::Result<()> {
        let store_direction = match direction {
            SearchDirection::Forward => smelt_store::TranscriptSearchDirection::Forward,
            SearchDirection::Backward => smelt_store::TranscriptSearchDirection::Backward,
        };
        let limit = SEARCH_TRANSCRIPT_PREFETCH_ENTRIES;
        let page = self
            .reader
            .search_transcript_candidate_page(needle, origin, store_direction, limit)?
            .into_iter()
            .map(|candidate| candidate.block_idx)
            .collect::<Vec<_>>();
        let origin = origin.unwrap_or(match direction {
            SearchDirection::Forward => 0,
            SearchDirection::Backward => u64::MAX,
        });
        candidates.insert_page(origin, direction, &page, limit);
        Ok(())
    }

    fn record_count(&self) -> smelt_store::Result<usize> {
        Ok(self.reader.head()?.transcript_record_count.as_usize())
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

/// Where a search started in the transcript, captured before any match
/// preview moved the loaded window.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TranscriptSearchOrigin {
    pub(crate) anchor: crate::app::transcript::TranscriptSearchAnchor,
    pub(crate) block_idx: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SqliteTranscriptCandidateBlocks {
    pub(crate) candidates: TranscriptCandidateBlocks,
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
        let grams = unique_trigrams(SearchPattern::parse(query).needle);
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
            candidates_hash: smelt_transcript::hash::hash_serializable(&candidate_blocks),
        }
    }

    fn transcript_indexed_text_for_entry(&self, entry: &TranscriptSearchLayoutEntry) -> String {
        let history = self.conversation.transcript().history();
        let mut text = String::new();
        for id in &entry.block_ids {
            let Some(record) = history.block(*id) else {
                continue;
            };
            let tool_state = history.tool_state(*id);
            let block_text =
                smelt_transcript::transcript_model::transcript_indexed_text(record, tool_state)
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
        candidate_blocks: &TranscriptCandidateBlocks,
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
            smelt_transcript::transcript_model::BlockId::new(block_idx),
        ));
        let width = self.transcript_width() as u16;
        let viewport_rows = self.transcript_search_viewport_rows();
        let _ = self.conversation.activate_transcript_search_record_window(
            width,
            block_idx,
            viewport_rows,
        );
    }

    pub(super) fn dirty_transcript_candidate_blocks(&mut self, query: &str) -> Vec<u64> {
        let history = self.conversation.transcript().history();
        let Some(start) = history.record_dirty_from() else {
            return Vec::new();
        };
        let _perf = smelt_perf::perf::begin("search:transcript:dirty_candidate_scan");
        let search = self.overlays.search_state_mut();
        let text = match search.dirty_text.take() {
            Some(text) if text.generation == history.generation() && text.start == start => text,
            _ => DirtyTranscriptText::build(history, start),
        };
        let needle = SearchPattern::parse(query).needle;
        let out: Vec<u64> = text
            .blocks
            .iter()
            .filter(|(_, indexed)| indexed.contains(needle))
            .map(|(id, _)| id.get())
            .collect();
        smelt_perf::perf::record_value(
            "search:transcript:dirty_candidates_scanned",
            text.blocks.len() as u64,
        );
        search.dirty_text = Some(text);
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
        let dirty_blocks = self.dirty_transcript_candidate_blocks(query);
        let needle = SearchPattern::parse(query).needle;
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
        let records_persisted = self.conversation.transcript_records_persisted();
        let sqlite_candidates = self.transcript_search_store().and_then(|db| {
            let mut candidates = TranscriptCandidateBlocks::default();
            db.load_candidate_page(&mut candidates, needle, origin_block, direction)
                .ok()?;
            if candidates.is_empty() && !records_persisted && db.record_count().ok() == Some(0) {
                return None;
            }
            Some(candidates)
        });
        let sqlite_available = sqlite_candidates.is_some();
        let mut candidates = match sqlite_candidates {
            Some(candidates) => {
                smelt_perf::perf::record_value("search:transcript:sqlite_available", 1);
                smelt_perf::perf::record_value(
                    "search:transcript:sqlite_candidate_blocks",
                    candidates.blocks.len() as u64,
                );
                candidates
            }
            None => {
                smelt_perf::perf::record_value("search:transcript:sqlite_available", 0);
                TranscriptCandidateBlocks::complete([])
            }
        };
        candidates.insert_blocks(dirty_blocks);
        let available = sqlite_available || !candidates.is_empty();
        if !available {
            return SqliteTranscriptCandidateBlocks::default();
        }
        SqliteTranscriptCandidateBlocks {
            candidates,
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
        origin_anchor: Option<TranscriptSearchOrigin>,
    ) -> Option<TranscriptSearchSession> {
        let candidate_backed = sqlite_candidates.available;
        let candidate_blocks = sqlite_candidates.candidates;
        let width = self.transcript_width() as u16;
        self.sync_transcript_renderer_generation();
        let start_bound = match origin_anchor.map(|origin| origin.anchor) {
            Some(anchor @ crate::app::transcript::TranscriptSearchAnchor::Content(_)) => {
                TranscriptSearchBound::inclusive(anchor.position_key(origin.byte_col))
            }
            _ => {
                let total_rows = self
                    .conversation
                    .transcript_search_total_rows(&self.lua, width);
                self.transcript_search_bound_for_origin(total_rows, origin, direction)
            }
        };
        let origin_block_idx = origin_anchor
            .and_then(|origin| origin.block_idx)
            .or_else(|| self.transcript_candidate_origin_block(origin, direction));
        if candidate_backed {
            self.activate_transcript_search_candidate_window(
                &candidate_blocks,
                origin,
                direction,
                origin_block_idx,
            );
            let total_rows = self
                .conversation
                .transcript_search_total_rows(&self.lua, width);
            smelt_perf::perf::record_value(
                "search:transcript:candidates",
                candidate_blocks.blocks.len() as u64,
            );
            return Some(TranscriptSearchSession {
                key: self.transcript_search_key(0, &[]),
                total_rows,
                candidate_backed,
                origin_block_idx,
                start_bound,
                candidate_blocks,
                block_matches: HashMap::new(),
                candidates: Vec::new(),
                scanned: Vec::new(),
                scanned_blocks: HashSet::new(),
                matches: Vec::new(),
                current: None,
            });
        }
        let (key, candidates, scanned_len, indexed_total_rows) = {
            let index = self.ensure_transcript_candidate_index(None)?;
            let indexed_total_rows = index
                .entries
                .iter()
                .map(|entry| entry.first_row.saturating_add(entry.rows))
                .max()
                .unwrap_or(0);
            (
                index.key,
                index.candidate_entries(query, None),
                index.entries.len(),
                indexed_total_rows,
            )
        };
        smelt_perf::perf::record_value("search:transcript:candidates", candidates.len() as u64);
        let total_rows = self
            .conversation
            .transcript_search_total_rows(&self.lua, width)
            .max(indexed_total_rows);
        Some(TranscriptSearchSession {
            key,
            total_rows,
            candidate_backed,
            origin_block_idx,
            start_bound,
            candidate_blocks,
            block_matches: HashMap::new(),
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
        // A candidate walk loads store pages and blocks as it goes.
        if session.candidate_backed {
            return true;
        }
        let origin_bound =
            self.transcript_search_bound_for_origin(session.total_rows, origin, direction);
        if self
            .cached_transcript_match(session, origin_bound, direction)
            .is_some()
        {
            return true;
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
        if session.candidate_backed && session.key.width == self.transcript_width() as u16 {
            let dirty = self.dirty_transcript_candidate_blocks(query);
            session.candidate_blocks.insert_blocks(dirty);
        }
        if self.transcript_search_session_can_advance(session, origin, direction) {
            return true;
        }
        if session.candidate_backed {
            return false;
        }
        // A store that can now answer replaces the loaded-window index.
        if self
            .sqlite_transcript_candidate_blocks(query, origin, direction)
            .available
        {
            return false;
        }
        let Some(key) = self
            .ensure_transcript_candidate_index(None)
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

        let origin_bound = match (origin_bound, session.current) {
            (Some(bound), _) => bound,
            (None, None) => session.start_bound,
            (None, Some(_)) => {
                self.transcript_search_bound_for_origin(session.total_rows, origin, direction)
            }
        };
        if session.candidate_backed {
            return self.advance_transcript_candidate_walk(session, query, origin_bound, direction);
        }

        let current = match self.cached_transcript_match(session, origin_bound, direction) {
            Some(index) => MatchScan::Found(index),
            None => self.scan_transcript_candidates_until_match(
                session,
                query,
                origin,
                origin_bound,
                direction,
            ),
        };
        let current = match current {
            MatchScan::Found(index) => Some(index),
            // The next candidate is loading; wrapping now would skip it.
            MatchScan::Pending => None,
            MatchScan::Exhausted => {
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
                let wrap_bound = self.transcript_search_bound_for_origin(
                    session.total_rows,
                    wrap_origin,
                    direction,
                );
                match self.scan_transcript_candidates_until_match(
                    session,
                    query,
                    wrap_origin,
                    wrap_bound,
                    direction,
                ) {
                    MatchScan::Found(index) => Some(index),
                    MatchScan::Pending => None,
                    MatchScan::Exhausted => {
                        self.cached_transcript_match(session, wrap_bound, direction)
                    }
                }
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

    /// The bound of matches at or past display position `origin`. Positions
    /// shift as hydration replaces estimated rows, so a search computes this
    /// once, before it loads anything.
    pub(super) fn transcript_search_bound_for_origin(
        &mut self,
        total_rows: RowIndex,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> TranscriptSearchBound {
        if matches!(direction, SearchDirection::Backward) && origin.row == RowIndex::MAX {
            return TranscriptSearchBound::inclusive(TranscriptSearchPositionKey::max());
        }
        let row = origin.row.min(total_rows.saturating_sub(1));
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
    ) -> MatchScan {
        loop {
            let Some(entry_index) =
                self.next_unscanned_transcript_candidate(session, origin, direction)
            else {
                return MatchScan::Exhausted;
            };
            self.scan_transcript_candidate(session, query, entry_index);
            if let Some(index) = self.cached_transcript_match(session, origin_bound, direction) {
                return MatchScan::Found(index);
            }
        }
    }

    /// Moves to the next match from `origin_bound` toward `direction` in a
    /// candidate-backed session, visiting candidate blocks in document order
    /// from the current match's block and wrapping at the transcript edge.
    /// `None` without a match, or while the next candidate is still loading.
    fn advance_transcript_candidate_walk(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        origin_bound: TranscriptSearchBound,
        direction: SearchDirection,
    ) -> Option<TranscriptSearchMatch> {
        let origin_block = session
            .current
            .and_then(|index| session.matches.get(index))
            .and_then(|matched| matched.anchor.block_id())
            .map(|id| id.get())
            .or(session.origin_block_idx);
        let found = match self.walk_transcript_candidates(
            session,
            query,
            origin_block,
            None,
            Some(origin_bound),
            direction,
        ) {
            MatchScan::Found(found) => Some(found),
            MatchScan::Pending => None,
            MatchScan::Exhausted => match self.walk_transcript_candidates(
                session,
                query,
                None,
                origin_block,
                None,
                direction,
            ) {
                MatchScan::Found(found) => Some(found),
                MatchScan::Pending | MatchScan::Exhausted => None,
            },
        }?;
        let matched = session.matches.get(found).copied()?;
        session.current = Some(found);
        Some(matched)
    }

    /// Walks candidate blocks from `from` (the transcript edge when `None`)
    /// through `until` (the opposite edge when `None`) toward `direction`,
    /// scanning each, until one holds a match within `bound`.
    fn walk_transcript_candidates(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        from: Option<u64>,
        until: Option<u64>,
        bound: Option<TranscriptSearchBound>,
        direction: SearchDirection,
    ) -> MatchScan {
        let needle = SearchPattern::parse(query).needle;
        let mut at = from.unwrap_or(match direction {
            SearchDirection::Forward => 0,
            SearchDirection::Backward => u64::MAX,
        });
        loop {
            let block_idx = match session.candidate_blocks.step(at, direction) {
                CandidateStep::Block(block_idx) => block_idx,
                CandidateStep::End => return MatchScan::Exhausted,
                CandidateStep::Load(origin) => {
                    let loaded = self.transcript_search_store().is_some_and(|store| {
                        store
                            .load_candidate_page(
                                &mut session.candidate_blocks,
                                needle,
                                Some(origin),
                                direction,
                            )
                            .is_ok()
                    });
                    if !loaded {
                        return MatchScan::Exhausted;
                    }
                    continue;
                }
            };
            let past_until = until.is_some_and(|until| match direction {
                SearchDirection::Forward => block_idx > until,
                SearchDirection::Backward => block_idx < until,
            });
            if past_until {
                return MatchScan::Exhausted;
            }
            if !session.block_matches.contains_key(&block_idx)
                && self.scan_transcript_candidate_block(session, query, block_idx)
                    == CandidateScan::Pending
            {
                return MatchScan::Pending;
            }
            let block_matches = session
                .block_matches
                .get(&block_idx)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let within = |matched: &&TranscriptSearchMatch| {
                bound.is_none_or(|bound| match direction {
                    SearchDirection::Forward => bound.contains_forward(matched.start_key()),
                    SearchDirection::Backward => bound.contains_backward(matched.start_key()),
                })
            };
            let found = match direction {
                SearchDirection::Forward => block_matches.iter().find(within),
                SearchDirection::Backward => block_matches.iter().rev().find(within),
            };
            if let Some(found) = found.copied() {
                let index = session
                    .matches
                    .iter()
                    .position(|matched| matched.same_position(&found));
                if let Some(index) = index {
                    return MatchScan::Found(index);
                }
            }
            at = match direction {
                SearchDirection::Forward if block_idx < u64::MAX - 1 => block_idx + 1,
                SearchDirection::Backward if block_idx > 0 => block_idx - 1,
                _ => return MatchScan::Exhausted,
            };
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

    /// Scans candidate `block_idx` for matches. A block whose records are
    /// still loading stays unscanned and reports [`CandidateScan::Pending`].
    fn scan_transcript_candidate_block(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        block_idx: u64,
    ) -> CandidateScan {
        let _perf = smelt_perf::perf::begin("search:transcript:scan_candidate_block");
        self.sync_transcript_renderer_generation();
        self.conversation.set_transcript_search_hydration_pin(Some(
            smelt_transcript::transcript_model::BlockId::new(block_idx),
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
        let laid_out = layout
            .entries
            .iter()
            .any(|entry| entry.block_ids.iter().any(|id| id.get() == block_idx));
        if !laid_out && self.conversation.transcript_hydration_is_pending() {
            return CandidateScan::Pending;
        }
        let theme = self.ui.theme().clone();
        let mut scanned_rows = 0;
        let mut block_matches = Vec::new();
        for entry in layout.entries {
            scanned_rows += entry.rows;
            if entry.rows == 0 {
                continue;
            }
            block_matches.extend(self.conversation.transcript_search_matches_for_row_range(
                &self.lua,
                width,
                &theme,
                entry.first_row,
                entry.rows,
                query,
            ));
        }
        block_matches.sort_by_key(TranscriptSearchMatch::sort_key);
        block_matches.dedup_by(|a, b| a.same_position(b));
        merge_transcript_matches(&mut session.matches, block_matches.iter().copied());
        session.block_matches.insert(block_idx, block_matches);
        smelt_perf::perf::record_value("search:transcript:scanned_entries", 1);
        smelt_perf::perf::record_value("search:transcript:scanned_rows", scanned_rows);
        smelt_perf::perf::record_value(
            "search:transcript:cached_matches",
            session.matches.len() as u64,
        );
        CandidateScan::Scanned
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

/// How scanning one candidate block ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateScan {
    Scanned,
    /// The block's records are still loading.
    Pending,
}

/// How a scan for the next match ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchScan {
    /// The index of the match in the session's cached matches.
    Found(usize),
    /// No candidate that way holds a match.
    Exhausted,
    /// The next candidate is still loading.
    Pending,
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
    candidate_blocks: &TranscriptCandidateBlocks,
    origin_block: Option<u64>,
    direction: SearchDirection,
) -> Option<u64> {
    match direction {
        SearchDirection::Forward => origin_block
            .and_then(|origin_block| {
                candidate_blocks
                    .iter()
                    .find(|block_idx| *block_idx >= origin_block)
            })
            .or_else(|| candidate_blocks.iter().next()),
        SearchDirection::Backward => origin_block
            .and_then(|origin_block| {
                candidate_blocks
                    .iter()
                    .rev()
                    .find(|block_idx| *block_idx <= origin_block)
            })
            .or_else(|| candidate_blocks.iter().next_back()),
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
    use smelt_transcript::transcript_model::BlockId;

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
                smelt_store::Store::new(store_address.path()),
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
        app.tui.handle_resize(100, 32);
        for index in 0..700 {
            let marker = if index == MATCH_INDEX {
                QUERY
            } else {
                "ordinary"
            };
            app.tui
                .push_block(smelt_transcript::transcript_model::Block::Text {
                    content: format!("search block {index}: {marker}").into(),
                });
        }
        let match_id = app
            .tui
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .copied()
            .find(|id| {
                app.tui
                    .conversation
                    .transcript()
                    .history()
                    .block(*id)
                    .and_then(smelt_transcript::transcript_model::Block::raw_text)
                    .is_some_and(|text| text.contains(QUERY))
            })
            .expect("search target block");
        app.tui.save_session_and_flush();
        let loaded = crate::app::history::load_transcript_tail_from_sqlite_id(
            &app.tui.core.sessions,
            &app.tui.conversation.session().id,
            100,
            32,
        )
        .expect("load sparse transcript");
        app.clear_transcript();
        app.tui
            .conversation
            .replace_transcript_document_for_harness(
                crate::app::transcript::TranscriptDocument::from_loaded_transcript(loaded),
            );
        app.tui
            .conversation
            .set_transcript_memory_budget_for_harness(TranscriptMemoryBudget {
                hydrated_blocks: 1,
                ..Default::default()
            });
        app.tui.app_focus = AppFocus::Content;
        app.tui.ui.set_focus(TRANSCRIPT_WIN);
        app.tui.transcript_win_mut().set_vim_enabled(true);
        app.tui.transcript_win_mut().set_vim_mode(VimMode::Normal);
        app.render_silent();

        app.submit_search(TRANSCRIPT_WIN, SearchDirection::Forward, QUERY.to_string());
        app.render_silent();
        let first = app
            .tui
            .conversation
            .transcript_memory_snapshot_for_harness();
        assert!(first.hydration_reads > 0);
        assert!(
            first.hydration_reads <= 64,
            "candidate verification exceeded one viewport-sized neighborhood: {first:?}"
        );
        assert!(app
            .tui
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .filter_map(|id| app.tui.conversation.transcript().history().block(*id))
            .filter_map(smelt_transcript::transcript_model::Block::raw_text)
            .any(|text| text.contains(QUERY)));

        app.render_silent();
        assert_eq!(
            app.tui
                .conversation
                .transcript_memory_snapshot_for_harness()
                .hydration_reads,
            first.hydration_reads,
            "render reread a block that remained in the viewport working set"
        );

        app.tui.clear_search();
        assert!(!app.tui.reveal_transcript_record_block(699, 0, true));
        app.render_silent();
        assert!(app.tui.reveal_transcript_target_at_top(
            699,
            smelt_transcript::transcript_model::BlockId::new(699),
            0,
            true,
        ));
        app.render_silent();
        assert!(!app
            .tui
            .conversation
            .transcript()
            .history()
            .is_materialized(match_id));
        let before_reveal = app
            .tui
            .conversation
            .transcript_memory_snapshot_for_harness();

        app.submit_search(TRANSCRIPT_WIN, SearchDirection::Forward, QUERY.to_string());
        app.render_silent();
        let revealed = app
            .tui
            .conversation
            .transcript_memory_snapshot_for_harness();
        assert!(revealed.hydration_reads > before_reveal.hydration_reads);
        assert!(
            revealed.hydration_reads - before_reveal.hydration_reads <= 64,
            "search reveal exceeded one viewport-sized neighborhood: before={before_reveal:?}, after={revealed:?}"
        );
        assert!(app
            .tui
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .filter_map(|id| app.tui.conversation.transcript().history().block(*id))
            .filter_map(smelt_transcript::transcript_model::Block::raw_text)
            .any(|text| text.contains(QUERY)));
    }
}
