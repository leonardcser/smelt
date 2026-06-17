use crate::app::search::{doc_range_for_match, row_match_is_selectable, SearchDirection};
use crate::app::TuiApp;
use crate::content::render_plan::RenderNodeId;
use crate::content::transcript_buf::{TranscriptSearchLayout, TranscriptSearchLayoutEntry};
use crate::content::transcript_search_text::descriptor_search_text;
use crate::smelt_edit::{DisplayRow, DocPosition, DocRange, RowIndex};
use smelt_core::transcript_model::{BlockId, LayoutKey};
use std::collections::HashMap;

const SEARCH_TRANSCRIPT_PREFETCH_ENTRIES: usize = 64;
const SEARCH_TRANSCRIPT_PREFETCH_MATCHES: usize = 128;

#[derive(Clone, Debug)]
pub(crate) struct TranscriptSearchSession {
    pub(super) key: TranscriptSearchKey,
    pub(super) total_rows: RowIndex,
    pub(super) candidates: Vec<usize>,
    pub(super) scanned: Vec<bool>,
    pub(super) matches: Vec<DocRange>,
    pub(super) current: Option<usize>,
}

#[derive(Clone, Debug)]
pub(super) struct TranscriptSearchIndex {
    key: TranscriptSearchKey,
    extends: Option<TranscriptSearchKey>,
    entries: Vec<TranscriptSearchEntry>,
    block_entries: HashMap<u64, Vec<usize>>,
    trigrams: HashMap<u32, Vec<usize>>,
}

#[derive(Clone, Debug)]
struct TranscriptSearchEntry {
    id: RenderNodeId,
    layout_key: LayoutKey,
    block_ids: Vec<BlockId>,
    first_row: RowIndex,
    rows: RowIndex,
    search_text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TranscriptSearchKey {
    layout_generation: u64,
    width: u16,
}

#[derive(Clone, Debug, Default)]
struct SqliteTranscriptCandidateBlocks {
    block_indices: Vec<u64>,
    available: bool,
}

impl TranscriptSearchIndex {
    fn append_from_layout(
        mut self,
        key: TranscriptSearchKey,
        layout: &TranscriptSearchLayout,
        mut search_text_for_entry: impl FnMut(&TranscriptSearchLayoutEntry) -> String,
    ) -> Option<Self> {
        if self.key.width != key.width {
            return None;
        }
        let old_len = self.entries.len();
        let old_key = self.key;
        if old_len >= layout.entries.len() {
            return None;
        }
        for (entry, layout_entry) in self.entries.iter().zip(layout.entries.iter()) {
            if entry.id != layout_entry.id
                || entry.layout_key != layout_entry.key
                || entry.block_ids != layout_entry.block_ids
                || entry.first_row != layout_entry.first_row
                || entry.rows != layout_entry.rows
            {
                return None;
            }
        }

        let _perf = smelt_perf::perf::begin("search:transcript:index_extend");
        for layout_entry in &layout.entries[old_len..] {
            let search_text = search_text_for_entry(layout_entry);
            push_search_entry(
                &mut self.entries,
                &mut self.block_entries,
                &mut self.trigrams,
                layout_entry,
                search_text,
            );
        }
        self.key = key;
        self.extends = Some(old_key);
        smelt_perf::perf::record_value(
            "search:transcript:index_appended_entries",
            (layout.entries.len() - old_len) as u64,
        );
        Some(self)
    }

    fn total_rows(&self) -> RowIndex {
        self.entries
            .last()
            .map(|entry| entry.first_row.saturating_add(entry.rows))
            .unwrap_or(0)
    }

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
    mut search_text_for_entry: impl FnMut(&TranscriptSearchLayoutEntry) -> String,
) -> TranscriptSearchIndex {
    let mut entries = Vec::with_capacity(layout.entries.len());
    let mut block_entries: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut trigrams: HashMap<u32, Vec<usize>> = HashMap::new();
    for layout_entry in &layout.entries {
        let search_text = search_text_for_entry(layout_entry);
        push_search_entry(
            &mut entries,
            &mut block_entries,
            &mut trigrams,
            layout_entry,
            search_text,
        );
    }
    TranscriptSearchIndex {
        key,
        extends: None,
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
    search_text: String,
) {
    let entry_index = entries.len();
    entries.push(TranscriptSearchEntry {
        id: layout_entry.id,
        layout_key: layout_entry.key,
        block_ids: layout_entry.block_ids.clone(),
        first_row: layout_entry.first_row,
        rows: layout_entry.rows,
        search_text: search_text.clone(),
    });
    for block_id in &layout_entry.block_ids {
        block_entries
            .entry(block_id.get())
            .or_default()
            .push(entry_index);
    }
    for gram in unique_trigrams(&search_text) {
        trigrams.entry(gram).or_default().push(entry_index);
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
    fn transcript_search_key(&mut self, layout_generation: u64) -> TranscriptSearchKey {
        TranscriptSearchKey {
            layout_generation,
            width: self.transcript_width() as u16,
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

    fn sqlite_transcript_candidate_blocks(&self, query: &str) -> SqliteTranscriptCandidateBlocks {
        let db_path = smelt_core::session::dir_for(&self.core.session).join("session.db");
        let Some(candidates) = smelt_store::SessionDb::open_read_only(db_path)
            .ok()
            .and_then(|db| db.search_transcript_candidates(query).ok())
        else {
            return SqliteTranscriptCandidateBlocks::default();
        };
        SqliteTranscriptCandidateBlocks {
            block_indices: candidates
                .into_iter()
                .map(|candidate| candidate.block_idx)
                .collect(),
            available: true,
        }
    }

    fn ensure_transcript_search_index(&mut self) -> Option<&TranscriptSearchIndex> {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let layout = {
            let _perf = smelt_perf::perf::begin("search:transcript:index_layout");
            self.transcript.materialize_search_layout(&self.lua, width)
        };
        let key = self.transcript_search_key(layout.generation);
        if self
            .search
            .transcript_index
            .as_ref()
            .is_some_and(|index| index.key == key)
        {
            return self.search.transcript_index.as_ref();
        }

        if let Some(index) = self.search.transcript_index.take() {
            if let Some(index) = index.append_from_layout(key, &layout, |layout_entry| {
                self.transcript_search_text_for_entry(layout_entry)
            }) {
                self.search.transcript_index = Some(index);
                record_index_size(self.search.transcript_index.as_ref()?);
                return self.search.transcript_index.as_ref();
            }
        }

        let _perf = smelt_perf::perf::begin("search:transcript:index_build");
        self.search.transcript_index = Some(build_transcript_search_index(
            key,
            &layout,
            |layout_entry| self.transcript_search_text_for_entry(layout_entry),
        ));
        record_index_size(self.search.transcript_index.as_ref()?);
        self.search.transcript_index.as_ref()
    }

    pub(super) fn new_transcript_search_session(
        &mut self,
        query: &str,
    ) -> Option<TranscriptSearchSession> {
        let sqlite_candidates = self.sqlite_transcript_candidate_blocks(query);
        let index = self.ensure_transcript_search_index()?;
        let preferred = sqlite_candidates
            .available
            .then_some(sqlite_candidates.block_indices.as_slice());
        let candidates = index.candidate_entries(query, preferred);
        smelt_perf::perf::record_value("search:transcript:candidates", candidates.len() as u64);
        let total_rows = index.total_rows();
        Some(TranscriptSearchSession {
            key: index.key,
            total_rows,
            candidates,
            scanned: vec![false; index.entries.len()],
            matches: Vec::new(),
            current: None,
        })
    }

    pub(super) fn sync_transcript_search_session(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
    ) -> bool {
        let sqlite_candidates = self.sqlite_transcript_candidate_blocks(query);
        let Some(index) = self.ensure_transcript_search_index() else {
            return false;
        };
        if index.key == session.key {
            return true;
        }
        if index.extends != Some(session.key) || session.scanned.len() > index.entries.len() {
            return false;
        }

        session.key = index.key;
        session.total_rows = index.total_rows();
        session.scanned.resize(index.entries.len(), false);
        let preferred = sqlite_candidates
            .available
            .then_some(sqlite_candidates.block_indices.as_slice());
        session.candidates = index.candidate_entries(query, preferred);
        smelt_perf::perf::record_value(
            "search:transcript:candidates",
            session.candidates.len() as u64,
        );
        true
    }

    pub(super) fn advance_transcript_search(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> Option<DocRange> {
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

        let current = self
            .cached_transcript_match(session, origin, direction)
            .or_else(|| {
                self.scan_transcript_candidates_until_match(session, query, origin, direction)
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
                self.cached_transcript_match(session, wrap_origin, direction)
                    .or_else(|| {
                        self.scan_transcript_candidates_until_match(
                            session,
                            query,
                            wrap_origin,
                            direction,
                        )
                    })
            }
        }?;
        session.current = Some(current);
        let range = session.matches.get(current).copied()?;
        self.prefetch_transcript_matches(session, query, range, direction);
        session.current = session
            .matches
            .iter()
            .position(|candidate| *candidate == range)
            .or(Some(current));
        Some(range)
    }

    fn cached_transcript_match(
        &self,
        session: &TranscriptSearchSession,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> Option<usize> {
        match direction {
            SearchDirection::Forward => session.matches.iter().position(|range| {
                (range.start.row, range.start.byte_col) >= (origin.row, origin.byte_col)
            }),
            SearchDirection::Backward => session.matches.iter().rposition(|range| {
                (range.start.row, range.start.byte_col) <= (origin.row, origin.byte_col)
            }),
        }
    }

    fn scan_transcript_candidates_until_match(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        origin: DocPosition,
        direction: SearchDirection,
    ) -> Option<usize> {
        loop {
            let entry_index =
                self.next_unscanned_transcript_candidate(session, origin, direction)?;
            self.scan_transcript_candidate(session, query, entry_index);
            if let Some(index) = self.cached_transcript_match(session, origin, direction) {
                return Some(index);
            }
        }
    }

    fn prefetch_transcript_matches(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        range: DocRange,
        direction: SearchDirection,
    ) {
        let target_matches = session
            .matches
            .len()
            .saturating_add(SEARCH_TRANSCRIPT_PREFETCH_MATCHES);
        let mut scanned_entries = 0usize;
        let origin = match direction {
            SearchDirection::Forward => range.end,
            SearchDirection::Backward => previous_search_position(range.start),
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
                        entry.first_row.saturating_add(entry.rows) > origin.row
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
                            && index
                                .entries
                                .get(*entry_index)
                                .is_some_and(|entry| entry.first_row <= origin.row)
                    })
            }
        }
    }

    fn scan_transcript_candidate(
        &mut self,
        session: &mut TranscriptSearchSession,
        query: &str,
        entry_index: usize,
    ) {
        let _perf = smelt_perf::perf::begin("search:transcript:scan_candidate");
        let Some((first_row, rows, search_text)) = self
            .search
            .transcript_index
            .as_ref()
            .and_then(|index| index.entries.get(entry_index))
            .map(|entry| (entry.first_row, entry.rows, entry.search_text.clone()))
        else {
            return;
        };
        if let Some(scanned) = session.scanned.get_mut(entry_index) {
            *scanned = true;
        }
        smelt_perf::perf::record_value("search:transcript:scanned_entries", 1);
        smelt_perf::perf::record_value("search:transcript:scanned_rows", rows);
        if rows == 0 {
            return;
        }
        let mut semantic_found = Vec::new();
        for (offset, line) in search_text.lines().enumerate() {
            let row = DisplayRow::new(line.to_string(), std::iter::once(0..line.len()).collect());
            collect_row_matches(
                first_row.saturating_add(offset as RowIndex),
                &row,
                query,
                &mut semantic_found,
            );
        }
        let display = self.transcript_rows_and_breaks_range(first_row, rows);
        let mut found = Vec::new();
        for (offset, row) in display.rows.iter().enumerate() {
            let row_index = first_row.saturating_add(offset as RowIndex);
            collect_row_matches(row_index, row, query, &mut found);
        }
        if found.is_empty() {
            found = semantic_found;
        }
        merge_doc_ranges(&mut session.matches, found);
        smelt_perf::perf::record_value(
            "search:transcript:cached_matches",
            session.matches.len() as u64,
        );
    }
}

fn collect_row_matches(
    row_index: RowIndex,
    row: &DisplayRow,
    query: &str,
    out: &mut Vec<DocRange>,
) {
    for (byte_col, _) in row.text.match_indices(query) {
        let end_col = byte_col + query.len();
        if row_match_is_selectable(row, byte_col, end_col) {
            out.push(doc_range_for_match(row_index, byte_col, end_col));
        }
    }
}

fn merge_doc_ranges(matches: &mut Vec<DocRange>, ranges: impl IntoIterator<Item = DocRange>) {
    matches.extend(ranges);
    matches.sort_by_key(|range| (range.start.row, range.start.byte_col, range.end.byte_col));
    matches.dedup();
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
    use smelt_core::transcript_model::ViewState;

    fn test_index(texts: &[&str]) -> TranscriptSearchIndex {
        let mut entries = Vec::new();
        let mut block_entries = HashMap::new();
        let mut trigrams = HashMap::new();
        for (i, search_text) in texts.iter().enumerate() {
            let id = BlockId::new(i as u64);
            let layout_entry = TranscriptSearchLayoutEntry {
                id: RenderNodeId::Block(id),
                key: LayoutKey {
                    width: 80,
                    view_state: ViewState::Expanded,
                    content_hash: i as u64,
                    sidecar_hash: 0,
                },
                block_ids: vec![id],
                first_row: i as RowIndex,
                rows: 1,
            };
            push_search_entry(
                &mut entries,
                &mut block_entries,
                &mut trigrams,
                &layout_entry,
                (*search_text).to_string(),
            );
        }
        TranscriptSearchIndex {
            key: TranscriptSearchKey {
                layout_generation: 1,
                width: 80,
            },
            extends: None,
            entries,
            block_entries,
            trigrams,
        }
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
            id: RenderNodeId::Group(1),
            key: LayoutKey {
                width: 80,
                view_state: ViewState::Expanded,
                content_hash: 1,
                sidecar_hash: 0,
            },
            block_ids: vec![BlockId::new(7), BlockId::new(8)],
            first_row: 0,
            rows: 2,
        };
        push_search_entry(
            &mut entries,
            &mut block_entries,
            &mut trigrams,
            &layout_entry,
            "group text".to_string(),
        );
        let index = TranscriptSearchIndex {
            key: TranscriptSearchKey {
                layout_generation: 1,
                width: 80,
            },
            extends: None,
            entries,
            block_entries,
            trigrams,
        };
        assert_eq!(index.candidate_entries("absent", Some(&[8])), vec![0]);
    }
}
