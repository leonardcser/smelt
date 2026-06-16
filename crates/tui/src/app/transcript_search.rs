use crate::app::search::{doc_range_for_match, row_match_is_selectable, SearchDirection};
use crate::app::TuiApp;
use crate::smelt_edit::{DisplayRow, DocPosition, DocRange, RowIndex};
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, LayoutKey, ViewState};
use std::collections::HashMap;

const SEARCH_TRANSCRIPT_PREFETCH_BLOCKS: usize = 64;
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
    trigrams: HashMap<u32, Vec<usize>>,
}

#[derive(Clone, Debug)]
struct TranscriptSearchEntry {
    block_id: BlockId,
    layout_key: LayoutKey,
    first_row: RowIndex,
    rows: RowIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TranscriptSearchKey {
    generation: u64,
    width: u16,
    show_thinking: bool,
}

impl TranscriptSearchIndex {
    fn append_from_layout(
        mut self,
        key: TranscriptSearchKey,
        layout: &[(BlockId, RowIndex, RowIndex)],
        history: &BlockHistory,
        base_key: LayoutKey,
    ) -> Option<Self> {
        if self.key.width != key.width || self.key.show_thinking != key.show_thinking {
            return None;
        }
        let old_len = self.entries.len();
        let old_key = self.key;
        if old_len >= layout.len() {
            return None;
        }
        for (entry, &(block_id, first_row, rows)) in self.entries.iter().zip(layout.iter()) {
            if entry.block_id != block_id
                || entry.first_row != first_row
                || entry.rows != rows
                || entry.layout_key != history.resolve_key(block_id, base_key)
            {
                return None;
            }
        }

        let _perf = smelt_perf::perf::begin("search:transcript:index_extend");
        for &(block_id, first_row, rows) in &layout[old_len..] {
            let block = history.blocks.get(&block_id)?;
            let entry_index = self.entries.len();
            self.entries.push(TranscriptSearchEntry {
                block_id,
                layout_key: history.resolve_key(block_id, base_key),
                first_row,
                rows,
            });
            for gram in unique_trigrams(&block_search_text(history, block)) {
                self.trigrams.entry(gram).or_default().push(entry_index);
            }
        }
        self.key = key;
        self.extends = Some(old_key);
        smelt_perf::perf::record_value(
            "search:transcript:index_appended_blocks",
            (layout.len() - old_len) as u64,
        );
        Some(self)
    }

    fn total_rows(&self) -> RowIndex {
        self.entries
            .last()
            .map(|entry| entry.first_row.saturating_add(entry.rows))
            .unwrap_or(0)
    }

    fn candidate_entries(&self, query: &str) -> Vec<usize> {
        let grams = unique_trigrams(query);
        if grams.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let mut postings: Vec<&Vec<usize>> = Vec::with_capacity(grams.len());
        for gram in grams {
            let Some(list) = self.trigrams.get(&gram) else {
                return Vec::new();
            };
            postings.push(list);
        }
        postings.sort_by_key(|list| list.len());
        let mut out = postings[0].clone();
        for list in postings.into_iter().skip(1) {
            out.retain(|entry| list.binary_search(entry).is_ok());
            if out.is_empty() {
                break;
            }
        }
        out
    }
}

fn base_transcript_search_layout_key(key: TranscriptSearchKey) -> LayoutKey {
    LayoutKey {
        width: key.width,
        show_thinking: key.show_thinking,
        view_state: ViewState::Expanded,
        content_hash: 0,
        sidecar_hash: 0,
    }
}

fn build_transcript_search_index(
    key: TranscriptSearchKey,
    layout: &[(BlockId, RowIndex, RowIndex)],
    history: &BlockHistory,
    base_key: LayoutKey,
) -> TranscriptSearchIndex {
    let mut entries = Vec::with_capacity(layout.len());
    let mut trigrams: HashMap<u32, Vec<usize>> = HashMap::new();
    for &(block_id, first_row, rows) in layout {
        let Some(block) = history.blocks.get(&block_id) else {
            continue;
        };
        let entry_index = entries.len();
        entries.push(TranscriptSearchEntry {
            block_id,
            layout_key: history.resolve_key(block_id, base_key),
            first_row,
            rows,
        });
        for gram in unique_trigrams(&block_search_text(history, block)) {
            trigrams.entry(gram).or_default().push(entry_index);
        }
    }
    TranscriptSearchIndex {
        key,
        extends: None,
        entries,
        trigrams,
    }
}

fn record_index_size(index: &TranscriptSearchIndex) {
    smelt_perf::perf::record_value("search:transcript:index_blocks", index.entries.len() as u64);
    smelt_perf::perf::record_value(
        "search:transcript:index_trigrams",
        index.trigrams.len() as u64,
    );
}

impl TuiApp {
    fn transcript_search_key(&mut self) -> TranscriptSearchKey {
        self.sync_transcript_renderer_generation();
        TranscriptSearchKey {
            generation: self.transcript.history().generation(),
            width: self.transcript_width() as u16,
            show_thinking: self.core.config.settings.show_thinking,
        }
    }

    fn ensure_transcript_search_index(&mut self) -> Option<&TranscriptSearchIndex> {
        let key = self.transcript_search_key();
        if self
            .search
            .transcript_index
            .as_ref()
            .is_some_and(|index| index.key == key)
        {
            return self.search.transcript_index.as_ref();
        }

        let base_key = base_transcript_search_layout_key(key);
        let layout = {
            let _perf = smelt_perf::perf::begin("search:transcript:index_layout");
            self.transcript
                .materialize_block_layout(&self.lua, key.width, key.show_thinking)
        };
        let history = self.transcript.history();
        if let Some(index) = self.search.transcript_index.take() {
            if let Some(index) = index.append_from_layout(key, &layout, history, base_key) {
                self.search.transcript_index = Some(index);
                record_index_size(self.search.transcript_index.as_ref()?);
                return self.search.transcript_index.as_ref();
            }
        }

        let _perf = smelt_perf::perf::begin("search:transcript:index_build");
        self.search.transcript_index = Some(build_transcript_search_index(
            key, &layout, history, base_key,
        ));
        record_index_size(self.search.transcript_index.as_ref()?);
        self.search.transcript_index.as_ref()
    }

    pub(super) fn new_transcript_search_session(
        &mut self,
        query: &str,
    ) -> Option<TranscriptSearchSession> {
        let index = self.ensure_transcript_search_index()?;
        let candidates = index.candidate_entries(query);
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
        session.candidates = index.candidate_entries(query);
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
        let mut scanned_blocks = 0usize;
        let origin = match direction {
            SearchDirection::Forward => range.end,
            SearchDirection::Backward => previous_search_position(range.start),
        };
        while session.matches.len() < target_matches
            && scanned_blocks < SEARCH_TRANSCRIPT_PREFETCH_BLOCKS
        {
            let Some(entry_index) =
                self.next_unscanned_transcript_candidate(session, origin, direction)
            else {
                break;
            };
            self.scan_transcript_candidate(session, query, entry_index);
            scanned_blocks += 1;
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
        let Some((first_row, rows)) = self
            .search
            .transcript_index
            .as_ref()
            .and_then(|index| index.entries.get(entry_index))
            .map(|entry| (entry.first_row, entry.rows))
        else {
            return;
        };
        if let Some(scanned) = session.scanned.get_mut(entry_index) {
            *scanned = true;
        }
        smelt_perf::perf::record_value("search:transcript:scanned_blocks", 1);
        smelt_perf::perf::record_value("search:transcript:scanned_rows", rows);
        if rows == 0 {
            return;
        }
        let display = self.transcript_rows_and_breaks_range(
            self.core.config.settings.show_thinking,
            first_row,
            rows,
        );
        let mut found = Vec::new();
        for (offset, row) in display.rows.iter().enumerate() {
            let row_index = first_row.saturating_add(offset as RowIndex);
            collect_row_matches(row_index, row, query, &mut found);
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

fn block_search_text(
    history: &smelt_core::transcript_model::BlockHistory,
    block: &Block,
) -> String {
    match block {
        Block::User { text, image_labels } => {
            if image_labels.is_empty() {
                text.clone()
            } else {
                format!("{}\n{}", text, image_labels.join("\n"))
            }
        }
        Block::Mode { text, icon, .. } => format!("{icon}{text}"),
        Block::ProcessStatus { text, .. } => text.clone(),
        Block::Thinking { content } | Block::Text { content } | Block::CodeLine { content, .. } => {
            content.clone()
        }
        Block::ToolCall {
            call_id,
            name,
            summary,
            args,
        } => {
            let output = history
                .tool_state(call_id)
                .and_then(|state| state.output.as_ref())
                .map(|output| output.content.as_str())
                .unwrap_or("");
            format!(
                "{}\n{}\n{:?}\n{}",
                name,
                summary.as_plain_text(),
                args,
                output
            )
        }
        Block::Exec { command, output } => format!("$ {command}\n{output}"),
        Block::Compacted { summary } => summary.clone(),
    }
}
