//! Transcript block history, streaming state, projection, and cursor glyph cache.

use crate::app::transcript_scroll_trace::{
    TranscriptDescriptorTraceRange, TranscriptInteractionTraceEvent,
    TranscriptProjectionTargetTrace, TranscriptScrollIntent, TranscriptScrollTrace,
    TranscriptScrollTraceFrame, TranscriptScrollTraceFrameStart, TranscriptScrollTraceRenderInput,
    TranscriptTraceAnchor, TranscriptVisibleContentAnchor,
};
use crate::app::TuiApp;
use crate::content::prompt_parser::{
    build_prompt_display_lines, prompt_display_uses_cursor_padding,
};
use crate::smelt_edit::{
    Buffer, DisplayDocument, DisplayRow, DisplayRows, DisplaySnapshot, DocRange, RowIndex,
    TextRange, Theme,
};
use smelt_buffer::wrap_layout::WrappedLayout;
use smelt_core::content::file_icons::FileIconOptions;
use smelt_core::content::highlight::InlineOptions;

use smelt_core::content::transcript::Transcript;
use smelt_core::lua::runtime::LuaRuntime;
use smelt_core::transcript_model::{
    Block, BlockHistory, BlockId, LayoutKey, ToolOutputRef, ToolStatus, TranscriptBlockDescriptor,
    TranscriptBlockRecordWithId,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DISPLAY_ONLY_TRANSCRIPT_OVERSCAN_VIEWPORTS: u16 = 3;
const DISPLAY_ONLY_TRANSCRIPT_MIN_TARGET_ROWS: u16 = 80;

pub(crate) struct LoadedDescriptorWindow {
    pub(crate) start: smelt_store::TranscriptDescriptorIndex,
    pub(crate) total_count: usize,
    pub(crate) hydration: smelt_store::TranscriptDescriptorHydration,
    pub(crate) records: Vec<TranscriptBlockRecordWithId>,
}

impl LoadedDescriptorWindow {
    pub(crate) fn end(&self) -> smelt_store::TranscriptDescriptorIndex {
        smelt_store::TranscriptDescriptorIndex::new(
            self.start.get().saturating_add(self.records.len()),
        )
    }

    fn from_slice(slice: smelt_store::TranscriptDescriptorSlice) -> Option<Self> {
        if slice.is_empty() {
            return None;
        }
        Some(Self {
            start: slice.start,
            total_count: slice.total_count,
            hydration: slice.hydration,
            records: descriptor_records_from_rows(slice.into_records())?,
        })
    }
}

pub(crate) struct LoadedTranscript {
    pub(crate) transcript: Transcript,
    pub(crate) descriptor_window: Option<LoadedDescriptorWindow>,
    pub(crate) session_dir: Option<PathBuf>,
}

impl LoadedTranscript {
    pub(crate) fn full(transcript: Transcript) -> Self {
        Self {
            transcript,
            descriptor_window: None,
            session_dir: None,
        }
    }

    pub(crate) fn tail_from_sqlite_dir(
        session_dir: PathBuf,
        width: u16,
        viewport_rows: u16,
    ) -> Option<Self> {
        let store = {
            let _perf = smelt_perf::perf::begin("transcript:resume_tail:open_store");
            SqliteTranscriptStore::open_read_only(&session_dir).ok()?
        };
        let target_rows = descriptor_tail_target_rows(viewport_rows);
        let slice = {
            let _perf = smelt_perf::perf::begin("transcript:resume_tail:read_tail_slice");
            store
                .read_tail_descriptor_slice_for_rows(width, target_rows)
                .ok()?
        };
        smelt_perf::perf::record_value(
            "transcript:sqlite:descriptor_total",
            slice.total_count as u64,
        );
        smelt_perf::perf::record_value("transcript:sqlite:descriptor_loaded", slice.len() as u64);
        let _perf = smelt_perf::perf::begin("transcript:resume_tail:build_loaded");
        Self::from_descriptor_slice(slice, session_dir)
    }

    pub(crate) fn from_descriptor_slice(
        slice: smelt_store::TranscriptDescriptorSlice,
        session_dir: PathBuf,
    ) -> Option<Self> {
        let descriptor_window = LoadedDescriptorWindow::from_slice(slice)?;
        Some(Self {
            transcript: Transcript::new(),
            descriptor_window: Some(descriptor_window),
            session_dir: Some(session_dir),
        })
    }
}

fn descriptor_records_from_rows(
    rows: Vec<smelt_store::TranscriptDescriptorRecord>,
) -> Option<Vec<TranscriptBlockRecordWithId>> {
    if rows.is_empty() {
        return None;
    }
    let _perf = smelt_perf::perf::begin("transcript:descriptor_window:decode_records");
    smelt_perf::perf::record_value(
        "transcript:descriptor_window:decode_rows",
        rows.len() as u64,
    );
    rows.into_iter()
        .map(TryInto::try_into)
        .collect::<Result<Vec<_>, serde_json::Error>>()
        .ok()
}

struct SqliteTranscriptStore {
    db: smelt_store::SessionDb,
}

impl SqliteTranscriptStore {
    fn open_read_only(session_dir: impl AsRef<std::path::Path>) -> smelt_store::Result<Self> {
        let db = smelt_store::SessionDb::open_read_only(session_dir.as_ref().join("session.db"))?;
        Ok(Self { db })
    }

    fn read_tail_descriptor_slice_for_rows(
        &self,
        width: u16,
        target_rows: u16,
    ) -> smelt_store::Result<smelt_store::TranscriptDescriptorSlice> {
        let total = {
            let _perf = smelt_perf::perf::begin("transcript:resume_tail:descriptor_count");
            self.db.transcript_descriptor_count()?
        };
        if total == 0 {
            return self
                .db
                .read_transcript_descriptor_tail_slice_with_total(total, 0);
        }

        let target_rows = u64::from(target_rows.max(1));
        let mut count = target_rows
            .saturating_add(1)
            .saturating_div(2)
            .min(total as u64) as usize;
        let mut probes = 0u64;
        while count < total {
            probes = probes.saturating_add(1);
            smelt_perf::perf::record_value("transcript:resume_tail:tail_probe_count", count as u64);
            let slice = {
                let _perf = smelt_perf::perf::begin("transcript:resume_tail:tail_slice_probe");
                self.db
                    .read_transcript_descriptor_tail_slice_with_total(total, count)?
            };
            if estimate_descriptor_rows(&slice.records, width) >= target_rows {
                smelt_perf::perf::record_value("transcript:resume_tail:tail_probes", probes);
                return Ok(slice);
            }
            count = count.saturating_mul(2).min(total);
        }
        smelt_perf::perf::record_value("transcript:resume_tail:tail_probes", probes + 1);
        smelt_perf::perf::record_value("transcript:resume_tail:tail_probe_count", total as u64);
        let _perf = smelt_perf::perf::begin("transcript:resume_tail:tail_slice_probe");
        self.db
            .read_transcript_descriptor_tail_slice_with_total(total, total)
    }

    fn estimated_descriptor_rows(
        &self,
        width: u16,
        range: Range<usize>,
    ) -> smelt_store::Result<u64> {
        self.db
            .transcript_descriptor_estimated_rows(range.into(), width)
    }
}

fn descriptor_tail_target_rows(viewport_rows: u16) -> u16 {
    viewport_rows
        .max(1)
        .saturating_mul(DISPLAY_ONLY_TRANSCRIPT_OVERSCAN_VIEWPORTS.saturating_add(1))
        .max(DISPLAY_ONLY_TRANSCRIPT_MIN_TARGET_ROWS)
}

fn estimate_descriptor_rows(
    records: &[smelt_store::TranscriptDescriptorRecord],
    width: u16,
) -> u64 {
    let width = u64::from(width.max(1));
    records
        .iter()
        .map(|record| {
            let text_rows = record.estimated_text_bytes.saturating_add(width - 1) / width;
            text_rows.max(1).saturating_add(1)
        })
        .sum()
}

fn estimate_text_rows_for_width(text: &str, width: usize) -> RowIndex {
    text.lines()
        .map(|line| {
            let cells = smelt_buffer::text::byte_to_cell(line, line.len());
            cells.max(1).div_ceil(width.max(1)) as RowIndex
        })
        .sum::<RowIndex>()
        .max(1)
}

fn descriptor_window_payload_bytes(records: &[TranscriptBlockRecordWithId]) -> u64 {
    records
        .iter()
        .filter_map(|record| record.record.descriptor.raw_text())
        .map(|text| text.len() as u64)
        .sum()
}

fn record_descriptor_window_metrics(window: &LoadedDescriptorWindow) {
    smelt_perf::perf::record_value(
        "transcript:descriptor_window:start",
        window.start.get() as u64,
    );
    smelt_perf::perf::record_value(
        "transcript:descriptor_window:end",
        window.end().get() as u64,
    );
    smelt_perf::perf::record_value(
        "transcript:descriptor_window:total",
        window.total_count as u64,
    );
    smelt_perf::perf::record_value(
        "transcript:descriptor_window:loaded",
        window.records.len() as u64,
    );
    smelt_perf::perf::record_value(
        "transcript:descriptor_window:payload_bytes",
        descriptor_window_payload_bytes(&window.records),
    );
    smelt_perf::perf::record_value(
        "transcript:descriptor_window:object_backed",
        u64::from(matches!(
            window.hydration,
            smelt_store::TranscriptDescriptorHydration::ObjectBacked
        )),
    );
}

#[derive(Default)]
struct SparseTranscriptDescriptors {
    total_count: Option<usize>,
    loaded_ranges: Vec<Range<smelt_store::TranscriptDescriptorIndex>>,
    records: BTreeMap<smelt_store::TranscriptDescriptorIndex, TranscriptBlockRecordWithId>,
}

impl SparseTranscriptDescriptors {
    fn from_loaded(loaded: Option<&LoadedDescriptorWindow>) -> Self {
        let mut descriptors = Self::default();
        if let Some(loaded) = loaded {
            descriptors.merge(loaded);
        }
        descriptors
    }

    fn merge(&mut self, loaded: &LoadedDescriptorWindow) -> bool {
        let start = loaded.start;
        let end = loaded.end();
        if start >= end {
            self.total_count = Some(loaded.total_count);
            return false;
        }
        self.total_count = Some(loaded.total_count);
        self.records
            .retain(|index, _| *index < start || *index >= end);
        for (offset, record) in loaded.records.iter().cloned().enumerate() {
            self.records.insert(
                smelt_store::TranscriptDescriptorIndex::new(start.get().saturating_add(offset)),
                record,
            );
        }
        self.add_loaded_range(start..end);
        true
    }

    fn add_loaded_range(&mut self, range: Range<smelt_store::TranscriptDescriptorIndex>) {
        if range.start >= range.end {
            return;
        }
        self.loaded_ranges.push(range);
        self.loaded_ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<smelt_store::TranscriptDescriptorIndex>> =
            Vec::with_capacity(self.loaded_ranges.len());
        for range in self.loaded_ranges.drain(..) {
            if let Some(last) = merged.last_mut() {
                if range.start <= last.end {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            merged.push(range);
        }
        self.loaded_ranges = merged;
    }

    fn records_for_range(
        &self,
        range: Option<&Range<smelt_store::TranscriptDescriptorIndex>>,
    ) -> Vec<TranscriptBlockRecordWithId> {
        match range {
            Some(range) => self
                .records
                .range(range.clone())
                .map(|(_, record)| record.clone())
                .collect(),
            None => self.records.values().cloned().collect(),
        }
    }

    fn total_count(&self) -> Option<usize> {
        self.total_count
    }

    fn loaded_descriptor_count(&self) -> usize {
        self.records.len()
    }

    fn missing_prefix_count_for_range(
        &self,
        range: Option<&Range<smelt_store::TranscriptDescriptorIndex>>,
    ) -> usize {
        range
            .map(|range| range.start.get())
            .unwrap_or_default()
            .min(self.total_count.unwrap_or_default())
    }

    fn missing_suffix_count_for_range(
        &self,
        range: Option<&Range<smelt_store::TranscriptDescriptorIndex>>,
    ) -> usize {
        self.total_count
            .map(|total| {
                total.saturating_sub(
                    range
                        .map(|range| range.end.get())
                        .unwrap_or_else(|| self.records.len()),
                )
            })
            .unwrap_or_default()
    }

    fn range_is_loaded(&self, range: &Range<smelt_store::TranscriptDescriptorIndex>) -> bool {
        range.start >= range.end
            || self
                .loaded_ranges
                .iter()
                .any(|loaded| loaded.start <= range.start && loaded.end >= range.end)
    }

    fn record(
        &self,
        index: smelt_store::TranscriptDescriptorIndex,
    ) -> Option<&TranscriptBlockRecordWithId> {
        self.records.get(&index)
    }

    fn descriptor_index_for_block_id(
        &self,
        block_id: BlockId,
    ) -> Option<smelt_store::TranscriptDescriptorIndex> {
        self.records
            .iter()
            .find_map(|(index, record)| (record.block_id == block_id).then_some(*index))
    }

    fn navigation_record(
        &self,
        role: &str,
        anchor: smelt_store::TranscriptDescriptorIndex,
        direction: TranscriptNavigationDirection,
    ) -> Option<(usize, TranscriptBlockRecordWithId)> {
        match direction {
            TranscriptNavigationDirection::Previous => {
                self.records
                    .range(..anchor)
                    .rev()
                    .find_map(|(index, record)| {
                        (descriptor_role(&record.record.descriptor) == role)
                            .then(|| (index.get(), record.clone()))
                    })
            }
            TranscriptNavigationDirection::Next => {
                let after_anchor =
                    smelt_store::TranscriptDescriptorIndex::new(anchor.get().saturating_add(1));
                self.records
                    .range(after_anchor..)
                    .find_map(|(index, record)| {
                        (descriptor_role(&record.record.descriptor) == role)
                            .then(|| (index.get(), record.clone()))
                    })
            }
        }
    }

    #[cfg(test)]
    fn loaded_ranges(&self) -> &[Range<smelt_store::TranscriptDescriptorIndex>] {
        &self.loaded_ranges
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DescriptorRangeState {
    Unavailable,
    Loaded,
    Missing,
}

const TRANSCRIPT_ESTIMATE_RANGE_QUERY_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct DescriptorRowsEstimateKey {
    width: u16,
    start: usize,
    end: usize,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ExactDescriptorRowsKey {
    descriptor_index: usize,
    width: u16,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
    node_key: LayoutKey,
}

#[derive(Default)]
struct TranscriptExtentIndex {
    descriptor_rows_estimate_cache: HashMap<DescriptorRowsEstimateKey, RowIndex>,
    estimate_store: Option<(PathBuf, SqliteTranscriptStore)>,
    exact_descriptor_rows: HashMap<ExactDescriptorRowsKey, RowIndex>,
    latest_exact_descriptor_rows: HashMap<(usize, u16), ExactDescriptorRowsKey>,
}

impl TranscriptExtentIndex {
    fn clear_exact_descriptor_rows(&mut self) {
        self.exact_descriptor_rows.clear();
        self.latest_exact_descriptor_rows.clear();
    }

    fn exact_observation_count(&self) -> usize {
        self.exact_descriptor_rows.len()
    }

    fn exact_rows_for_descriptor(&self, descriptor_index: usize, width: u16) -> Option<RowIndex> {
        let width = width.max(1);
        let key = self
            .latest_exact_descriptor_rows
            .get(&(descriptor_index, width))?;
        self.exact_descriptor_rows.get(key).copied()
    }

    fn observe_exact_loaded_descriptor_rows(
        &mut self,
        descriptors: &SparseTranscriptDescriptors,
        snapshot: crate::content::transcript_buf::TranscriptExactHeightSnapshot,
    ) {
        if snapshot.observations.is_empty() {
            return;
        }
        let descriptor_by_block: HashMap<BlockId, usize> = descriptors
            .records
            .iter()
            .map(|(index, record)| (record.block_id, index.get()))
            .collect();
        for observation in snapshot.observations {
            let Some(descriptor_index) = descriptor_by_block.get(&observation.block_id).copied()
            else {
                continue;
            };
            let key = ExactDescriptorRowsKey {
                descriptor_index,
                width: snapshot.width.max(1),
                renderer_generation: snapshot.renderer_generation,
                renderer_cache_key: snapshot.renderer_cache_key,
                presentation_generation: snapshot.presentation_generation,
                node_key: observation.key,
            };
            self.exact_descriptor_rows
                .insert(key.clone(), observation.rows);
            self.latest_exact_descriptor_rows
                .insert((descriptor_index, key.width), key);
        }
    }

    fn approximate_rows_for_loaded_descriptors(
        &self,
        descriptors: &SparseTranscriptDescriptors,
        width: u16,
    ) -> RowIndex {
        let text_width = usize::from(width.max(1));
        descriptors
            .records
            .iter()
            .map(|(index, record)| {
                self.exact_rows_for_descriptor(index.get(), width)
                    .unwrap_or_else(|| {
                        record
                            .record
                            .descriptor
                            .raw_text()
                            .map(|text| estimate_text_rows_for_width(&text, text_width))
                            .unwrap_or(1)
                            .saturating_add(1)
                    })
            })
            .sum()
    }

    fn loaded_descriptor_count(&self, descriptors: &SparseTranscriptDescriptors) -> usize {
        descriptors.loaded_descriptor_count()
    }

    fn approximate_average_rows_per_loaded_descriptor(
        &self,
        descriptors: &SparseTranscriptDescriptors,
        width: u16,
    ) -> RowIndex {
        let loaded = self.loaded_descriptor_count(descriptors) as RowIndex;
        if loaded == 0 {
            return 2;
        }
        self.approximate_rows_for_loaded_descriptors(descriptors, width)
            .saturating_add(loaded.saturating_sub(1))
            .saturating_div(loaded)
            .max(1)
    }

    fn store_for_session(&mut self, session_dir: &PathBuf) -> Option<&SqliteTranscriptStore> {
        let needs_open = self
            .estimate_store
            .as_ref()
            .is_none_or(|(open_dir, _)| open_dir != session_dir);
        if needs_open {
            let store = SqliteTranscriptStore::open_read_only(session_dir).ok()?;
            self.estimate_store = Some((session_dir.clone(), store));
        }
        self.estimate_store.as_ref().map(|(_, store)| store)
    }

    fn approximate_rows_for_unloaded_descriptor_range(
        &mut self,
        session_dir: Option<&PathBuf>,
        width: u16,
        range: Range<usize>,
    ) -> Option<RowIndex> {
        if range.start >= range.end {
            return Some(0);
        }
        let key = DescriptorRowsEstimateKey {
            width: width.max(1),
            start: range.start,
            end: range.end,
        };
        if let Some(rows) = self.descriptor_rows_estimate_cache.get(&key).copied() {
            return Some(rows);
        }
        let requested = range.end.saturating_sub(range.start);
        if requested > TRANSCRIPT_ESTIMATE_RANGE_QUERY_LIMIT {
            smelt_perf::perf::record_value(
                "transcript:extent:descriptor_estimate_coarse_fallback_count",
                requested as u64,
            );
            return None;
        }
        let session_dir = session_dir?;
        let store = self.store_for_session(session_dir)?;
        let rows = store.estimated_descriptor_rows(key.width, range).ok()? as RowIndex;
        self.descriptor_rows_estimate_cache.insert(key, rows);
        Some(rows)
    }

    fn approximate_rows_for_missing_descriptor_range(
        &mut self,
        descriptors: &SparseTranscriptDescriptors,
        session_dir: Option<&PathBuf>,
        width: u16,
        range: Range<usize>,
    ) -> RowIndex {
        let count = range.end.saturating_sub(range.start) as RowIndex;
        self.approximate_rows_for_unloaded_descriptor_range(session_dir, width, range)
            .unwrap_or_else(|| {
                count.saturating_mul(
                    self.approximate_average_rows_per_loaded_descriptor(descriptors, width),
                )
            })
    }

    fn approximate_sparse_prefix_rows(
        &mut self,
        descriptors: &SparseTranscriptDescriptors,
        active_descriptor_range: Option<&Range<smelt_store::TranscriptDescriptorIndex>>,
        session_dir: Option<&PathBuf>,
        width: u16,
    ) -> RowIndex {
        let end = active_descriptor_range
            .map(|range| range.start.get())
            .unwrap_or_else(|| descriptors.missing_prefix_count_for_range(active_descriptor_range));
        self.approximate_rows_for_missing_descriptor_range(descriptors, session_dir, width, 0..end)
    }

    fn approximate_sparse_suffix_rows(
        &mut self,
        descriptors: &SparseTranscriptDescriptors,
        active_descriptor_range: Option<&Range<smelt_store::TranscriptDescriptorIndex>>,
        session_dir: Option<&PathBuf>,
        width: u16,
    ) -> RowIndex {
        let Some(total) = descriptors.total_count() else {
            let count = descriptors.missing_suffix_count_for_range(active_descriptor_range);
            return (count as RowIndex).saturating_mul(
                self.approximate_average_rows_per_loaded_descriptor(descriptors, width),
            );
        };
        let start = active_descriptor_range
            .map(|range| range.end.get())
            .unwrap_or(total);
        self.approximate_rows_for_missing_descriptor_range(
            descriptors,
            session_dir,
            width,
            start..total,
        )
    }

    fn approximate_mixed_scrollbar_total_rows(
        &mut self,
        descriptors: &SparseTranscriptDescriptors,
        active_descriptor_range: Option<&Range<smelt_store::TranscriptDescriptorIndex>>,
        session_dir: Option<&PathBuf>,
        width: u16,
        exact_loaded_rows: RowIndex,
    ) -> RowIndex {
        self.approximate_sparse_prefix_rows(
            descriptors,
            active_descriptor_range,
            session_dir,
            width,
        )
        .saturating_add(exact_loaded_rows)
        .saturating_add(self.approximate_sparse_suffix_rows(
            descriptors,
            active_descriptor_range,
            session_dir,
            width,
        ))
    }
}

pub(crate) struct TranscriptDocument {
    transcript: Transcript,
    projection: crate::content::transcript_buf::TranscriptProjection,
    render_cache: TranscriptRenderCache,
    compaction_preview_id: Option<BlockId>,
    sparse_descriptors: SparseTranscriptDescriptors,
    active_descriptor_range: Option<Range<smelt_store::TranscriptDescriptorIndex>>,
    session_dir: Option<PathBuf>,
    extent_index: TranscriptExtentIndex,
    viewport_state: TranscriptViewportState,
    scroll_trace: Option<TranscriptScrollTrace>,
}

pub(crate) struct TranscriptRenderContext {
    pub(crate) width: u16,
    pub(crate) theme_key: u64,
    pub(crate) renderer_generation: u64,
    pub(crate) renderer_cache_key: Option<u64>,
}

pub(crate) enum TranscriptDescriptorSaveSuffix {
    Unchanged,
    Suffix {
        descriptor_start_idx: usize,
        descriptor_records: Vec<smelt_core::TranscriptBlockRecord>,
    },
    NeedsFullRebuild,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SparseProjectionGap {
    scroll_top: RowIndex,
    row_base: RowIndex,
    end: RowIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptAnchorBias {
    Top,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TranscriptContentAnchor {
    descriptor_index: usize,
    block_id: BlockId,
    intra_block_row: RowIndex,
    bias: TranscriptAnchorBias,
    row_anchor: crate::content::transcript_buf::TranscriptRowAnchor,
    fallback_row: RowIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptScrollAnchor {
    Tail,
    Content(TranscriptContentAnchor),
    EstimatedRow(RowIndex),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptViewportMode {
    Tail,
    Anchored,
    FarSeek,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TranscriptViewportState {
    top_anchor: Option<TranscriptScrollAnchor>,
    top_offset_rows: isize,
    mode: TranscriptViewportMode,
    pending_intent: Option<TranscriptScrollIntent>,
    resolved_scroll_top: Option<RowIndex>,
}

impl Default for TranscriptViewportState {
    fn default() -> Self {
        Self {
            top_anchor: None,
            top_offset_rows: 0,
            mode: TranscriptViewportMode::Tail,
            pending_intent: None,
            resolved_scroll_top: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TranscriptViewportProjectionInput {
    pub(crate) fallback_scroll_top: RowIndex,
    pub(crate) follow_tail: bool,
    pub(crate) width_changed: bool,
    pub(crate) previous_width: Option<u16>,
}

pub(crate) struct AppliedTranscriptViewport {
    pub(crate) materialized_rows: crate::smelt_edit::MaterializedRows,
    pub(crate) top_anchor: Option<TranscriptTraceAnchor>,
    pub(crate) scrollbar_total_rows: RowIndex,
    pub(crate) exact_visible_range: Range<RowIndex>,
    pub(crate) placeholder_rows_visible: bool,
    pub(crate) follow_tail: bool,
}

enum TranscriptMaterializationPlan {
    Loaded(crate::content::transcript_buf::ProjectionPlan),
    UnloadedGap(SparseProjectionGap),
}

fn inert_sparse_gap_rows(count: RowIndex) -> Vec<DisplayRow> {
    (0..count)
        .map(|_| {
            DisplayRow::new(String::new(), Vec::new())
                .with_break_before(crate::smelt_edit::RowBreak::Hard)
        })
        .collect()
}

pub(crate) struct TranscriptProjectionPlan {
    materialization: TranscriptMaterializationPlan,
    row_offset: RowIndex,
    total_rows: RowIndex,
    requested_scroll: Option<RowIndex>,
    scroll_anchor: TranscriptScrollAnchor,
    width: u16,
    viewport_rows: u16,
    trace_frame: Option<TranscriptScrollTraceFrameStart>,
    trace_started_at: Option<Instant>,
}

struct TranscriptScrollTraceFinishContext {
    width: u16,
    row_offset: RowIndex,
    viewport_rows: u16,
    trace_frame: Option<TranscriptScrollTraceFrameStart>,
    trace_started_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct TranscriptRenderCacheKey {
    generation: u64,
    width: u16,
    theme: u64,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    start: RowIndex,
    count: RowIndex,
}

struct TranscriptRenderCache {
    rows: HashMap<TranscriptRenderCacheKey, DisplayRows>,
    order: VecDeque<TranscriptRenderCacheKey>,
    limit: usize,
}

impl TranscriptRenderCache {
    const DEFAULT_LIMIT: usize = 16;

    fn new() -> Self {
        Self {
            rows: HashMap::new(),
            order: VecDeque::new(),
            limit: Self::DEFAULT_LIMIT,
        }
    }

    fn get(&mut self, key: TranscriptRenderCacheKey) -> Option<DisplayRows> {
        let rows = self.rows.get(&key)?.clone();
        self.order.retain(|existing| *existing != key);
        self.order.push_back(key);
        Some(rows)
    }

    fn insert(&mut self, key: TranscriptRenderCacheKey, rows: DisplayRows) {
        self.order.retain(|existing| *existing != key);
        self.order.push_back(key);
        self.rows.insert(key, rows);
        while self.order.len() > self.limit {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if !self.order.contains(&oldest) {
                self.rows.remove(&oldest);
            }
        }
    }

    fn clear(&mut self) {
        self.rows.clear();
        self.order.clear();
    }
}

impl TranscriptDocument {
    pub(crate) fn new() -> Self {
        Self::from_transcript(Transcript::new())
    }

    pub(crate) fn from_transcript(transcript: Transcript) -> Self {
        Self::from_loaded_transcript(LoadedTranscript::full(transcript))
    }

    pub(crate) fn from_loaded_transcript(loaded: LoadedTranscript) -> Self {
        if let Some(window) = loaded.descriptor_window.as_ref() {
            record_descriptor_window_metrics(window);
        }
        let active_descriptor_range = loaded
            .descriptor_window
            .as_ref()
            .map(|window| window.start..window.end());
        let sparse_descriptors =
            SparseTranscriptDescriptors::from_loaded(loaded.descriptor_window.as_ref());
        let mut document = Self {
            transcript: loaded.transcript,
            projection: crate::content::transcript_buf::TranscriptProjection::new(),
            render_cache: TranscriptRenderCache::new(),
            compaction_preview_id: None,
            sparse_descriptors,
            active_descriptor_range,
            session_dir: loaded.session_dir,
            extent_index: TranscriptExtentIndex::default(),
            viewport_state: TranscriptViewportState::default(),
            scroll_trace: None,
        };
        if document.active_descriptor_range.is_some() {
            document.install_active_descriptor_projection();
        }
        document
    }

    pub(crate) fn replace_transcript(&mut self, transcript: Transcript) {
        self.replace_loaded_transcript(LoadedTranscript::full(transcript));
    }

    pub(crate) fn replace_loaded_transcript(&mut self, loaded: LoadedTranscript) {
        let inline_options = self.projection.inline_options().clone();
        let scroll_trace = self.scroll_trace.take();
        *self = Self::from_loaded_transcript(loaded);
        self.scroll_trace = scroll_trace;
        self.set_inline_options(inline_options);
    }

    // COMPAT(transcript-deferred-full-descriptor-bridge): deferred session load still validates against a fully materialized semantic session until normal resume becomes metadata-only.
    pub(crate) fn legacy_merge_full_descriptor_slice_for_deferred_load(&mut self) -> bool {
        let Some(total_count) = self.sparse_descriptors.total_count() else {
            return false;
        };
        let Some(window) = self.load_descriptor_window((0..total_count).into()) else {
            return false;
        };
        self.merge_descriptor_window(window)
    }

    pub(crate) fn load_descriptor_window(
        &self,
        range: smelt_store::TranscriptDescriptorRange,
    ) -> Option<LoadedDescriptorWindow> {
        let session_dir = self.session_dir.clone()?;
        let db = smelt_store::SessionDb::open_read_only(session_dir.join("session.db")).ok()?;
        let slice = db.read_transcript_descriptor_slice(range).ok()?;
        LoadedDescriptorWindow::from_slice(slice)
    }

    pub(crate) fn merge_descriptor_window(&mut self, window: LoadedDescriptorWindow) -> bool {
        let active_range = window.start..window.end();
        if !self.sparse_descriptors.merge(&window) {
            return false;
        }
        self.active_descriptor_range = Some(active_range);
        record_descriptor_window_metrics(&window);
        self.install_active_descriptor_projection();
        true
    }

    pub(crate) fn scroll_trace_enabled(&self) -> bool {
        self.scroll_trace.is_some()
    }

    #[allow(dead_code)]
    pub(crate) fn set_scroll_trace_enabled(&mut self, enabled: bool) {
        match (enabled, self.scroll_trace.is_some()) {
            (true, false) => self.scroll_trace = Some(TranscriptScrollTrace::default()),
            (false, true) => self.scroll_trace = None,
            _ => {}
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_scroll_trace_timings_enabled(&mut self, enabled: bool) {
        self.scroll_trace = Some(TranscriptScrollTrace::with_timings(enabled));
    }

    pub(crate) fn enable_scroll_trace_jsonl(
        &mut self,
        path: std::path::PathBuf,
        record_timings: bool,
    ) {
        self.scroll_trace = Some(TranscriptScrollTrace::with_jsonl_path(path, record_timings));
    }

    pub(crate) fn record_scroll_trace_event(
        &mut self,
        kind: impl Into<String>,
        data: serde_json::Value,
    ) {
        if let Some(trace) = self.scroll_trace.as_mut() {
            trace.record_interaction(kind, data);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn take_scroll_trace_interaction_events(
        &mut self,
    ) -> Vec<TranscriptInteractionTraceEvent> {
        self.scroll_trace
            .as_mut()
            .map(TranscriptScrollTrace::take_interaction_events)
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub(crate) fn scroll_trace_interaction_events(&self) -> &[TranscriptInteractionTraceEvent] {
        self.scroll_trace
            .as_ref()
            .map(TranscriptScrollTrace::interaction_events)
            .unwrap_or(&[])
    }

    #[allow(dead_code)]
    pub(crate) fn scroll_trace_frames(&self) -> &[TranscriptScrollTraceFrame] {
        self.scroll_trace
            .as_ref()
            .map(TranscriptScrollTrace::frames)
            .unwrap_or(&[])
    }

    #[allow(dead_code)]
    pub(crate) fn take_scroll_trace_frames(&mut self) -> Vec<TranscriptScrollTraceFrame> {
        self.scroll_trace
            .as_mut()
            .map(TranscriptScrollTrace::take_frames)
            .unwrap_or_default()
    }

    pub(crate) fn last_traced_resolved_scroll_top(&self) -> Option<RowIndex> {
        self.scroll_trace
            .as_ref()
            .and_then(TranscriptScrollTrace::last_resolved_scroll_top)
    }

    pub(crate) fn scroll_trace_has_pending_input(&self) -> bool {
        self.scroll_trace
            .as_ref()
            .is_some_and(TranscriptScrollTrace::has_pending_input)
    }

    pub(crate) fn set_next_scroll_trace_input(&mut self, input: TranscriptScrollTraceRenderInput) {
        if let Some(trace) = self.scroll_trace.as_mut() {
            trace.set_pending_input(input);
        }
    }

    pub(crate) fn set_pending_scroll_intent(&mut self, intent: TranscriptScrollIntent) {
        let intent = match (self.viewport_state.pending_intent.take(), intent) {
            (
                Some(TranscriptScrollIntent::UserDelta { rows: pending }),
                TranscriptScrollIntent::UserDelta { rows },
            ) => TranscriptScrollIntent::UserDelta {
                rows: pending.saturating_add(rows),
            },
            (Some(_), intent) | (None, intent) => intent,
        };
        self.viewport_state.mode = Self::viewport_mode_for_intent(&intent);
        self.viewport_state.pending_intent = Some(intent);
    }

    fn viewport_mode_for_intent(intent: &TranscriptScrollIntent) -> TranscriptViewportMode {
        match intent {
            TranscriptScrollIntent::Tail => TranscriptViewportMode::Tail,
            TranscriptScrollIntent::ScrollbarFraction { .. }
            | TranscriptScrollIntent::ApproximateRowSeek(_) => TranscriptViewportMode::FarSeek,
            TranscriptScrollIntent::PreserveViewport
            | TranscriptScrollIntent::UserDelta { .. }
            | TranscriptScrollIntent::PageDelta { .. }
            | TranscriptScrollIntent::ExactContentAnchor(_)
            | TranscriptScrollIntent::SearchJump(_)
            | TranscriptScrollIntent::RevealBlock { .. }
            | TranscriptScrollIntent::ResizeReflow { .. } => TranscriptViewportMode::Anchored,
        }
    }

    fn intent_allows_sparse_placeholders(intent: &TranscriptScrollIntent) -> bool {
        matches!(
            intent,
            TranscriptScrollIntent::ScrollbarFraction { .. }
                | TranscriptScrollIntent::ApproximateRowSeek(_)
        )
    }

    fn trace_descriptor_range(
        range: Option<&Range<smelt_store::TranscriptDescriptorIndex>>,
    ) -> Option<TranscriptDescriptorTraceRange> {
        range.map(TranscriptDescriptorTraceRange::from_store_range)
    }

    fn trace_anchor(anchor: TranscriptScrollAnchor) -> TranscriptTraceAnchor {
        match anchor {
            TranscriptScrollAnchor::Tail => TranscriptTraceAnchor::Tail,
            TranscriptScrollAnchor::Content(anchor) => TranscriptTraceAnchor::Content {
                virtual_row: anchor.fallback_row,
                descriptor_index: anchor.descriptor_index,
                block_id: anchor.block_id,
                node_id: anchor.row_anchor.id,
                row_offset: anchor.intra_block_row,
            },
            TranscriptScrollAnchor::EstimatedRow(row) => TranscriptTraceAnchor::EstimatedRow(row),
        }
    }

    pub(crate) fn trace_anchor_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
    ) -> TranscriptTraceAnchor {
        let anchor = self
            .content_anchor_at_row(lua, width, row, TranscriptAnchorBias::Top)
            .map(TranscriptScrollAnchor::Content)
            .unwrap_or(TranscriptScrollAnchor::EstimatedRow(row));
        Self::trace_anchor(anchor)
    }

    fn trace_projection_target(
        target: crate::content::transcript_buf::ScrollTarget,
    ) -> TranscriptProjectionTargetTrace {
        match target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => TranscriptProjectionTargetTrace::Tail,
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ExactRow(row),
            ) => TranscriptProjectionTargetTrace::ExactRow(row),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(row),
            ) => TranscriptProjectionTargetTrace::ReflowStableRow(row),
        }
    }

    fn start_scroll_trace_frame(
        &mut self,
        width: u16,
        projection_target: crate::content::transcript_buf::ScrollTarget,
    ) -> Option<(TranscriptScrollTraceFrameStart, Option<Instant>)> {
        let projection_target = Self::trace_projection_target(projection_target);
        let (input, record_timings) = {
            let trace = self.scroll_trace.as_mut()?;
            (
                trace.take_pending_input_or_default(projection_target),
                trace.record_timings(),
            )
        };
        let viewport_anchor_before = self.viewport_state.top_anchor.map(Self::trace_anchor);
        let active_descriptor_range_before =
            Self::trace_descriptor_range(self.active_descriptor_range.as_ref());
        let prefix_estimate_before = self.approximate_sparse_prefix_row_offset(width);
        let suffix_estimate_before = self.approximate_sparse_suffix_rows(width);
        let exact_observation_count = self.extent_index.exact_observation_count();
        let started_at = record_timings.then(Instant::now);
        Some((
            TranscriptScrollTraceFrameStart {
                input,
                viewport_anchor_before,
                projection_target,
                active_descriptor_range_before,
                prefix_estimate_before,
                suffix_estimate_before,
                exact_observation_count,
            },
            started_at,
        ))
    }

    fn finish_scroll_trace_frame(
        &mut self,
        lua: &LuaRuntime,
        rows: crate::smelt_edit::MaterializedRows,
        ctx: &mut TranscriptScrollTraceFinishContext,
        placeholder_rows_visible: bool,
    ) {
        let Some(trace_frame) = ctx.trace_frame.take() else {
            return;
        };
        let viewport_rows = RowIndex::from(ctx.viewport_rows.max(1));
        let first_visible_content_anchor =
            self.trace_visible_content_anchor_at_row(lua, ctx.width, rows.clamped_scroll);
        let last_visible_row = rows
            .clamped_scroll
            .saturating_add(viewport_rows.saturating_sub(1))
            .min(rows.total_rows.saturating_sub(1));
        let last_visible_content_anchor =
            self.trace_visible_content_anchor_at_row(lua, ctx.width, last_visible_row);
        let active_descriptor_range_after =
            Self::trace_descriptor_range(self.active_descriptor_range.as_ref());
        let viewport_anchor_after = self.viewport_state.top_anchor.map(Self::trace_anchor);
        let visible_record_or_block_ids = self.visible_block_ids_for_virtual_range(
            rows.clamped_scroll..rows.clamped_scroll.saturating_add(viewport_rows),
            ctx.row_offset,
        );
        let render_or_projection_ms = ctx.trace_started_at.take().map(|started| {
            let millis = started.elapsed().as_millis();
            millis.min(u128::from(u64::MAX)) as u64
        });
        let frame = trace_frame.finish(
            rows.clamped_scroll,
            viewport_anchor_after,
            active_descriptor_range_after,
            rows.materialized_range(),
            placeholder_rows_visible,
            first_visible_content_anchor,
            last_visible_content_anchor,
            visible_record_or_block_ids,
            render_or_projection_ms,
        );
        if let Some(trace) = self.scroll_trace.as_mut() {
            trace.push(frame);
        }
    }

    fn trace_visible_content_anchor_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
    ) -> Option<TranscriptVisibleContentAnchor> {
        self.row_anchor_at_row(lua, width, row)
            .map(|anchor| TranscriptVisibleContentAnchor {
                virtual_row: row,
                node_id: anchor.id,
                row_offset: anchor.row_offset,
            })
    }

    fn visible_block_ids_for_virtual_range(
        &self,
        range: Range<RowIndex>,
        row_offset: RowIndex,
    ) -> Vec<BlockId> {
        self.visible_block_layout()
            .filter_map(|(id, first_row, rows)| {
                let first_row = first_row.saturating_add(row_offset);
                let end = first_row.saturating_add(rows);
                (first_row < range.end && end > range.start).then_some(id)
            })
            .collect()
    }

    fn install_active_descriptor_projection(&mut self) {
        let records = self
            .sparse_descriptors
            .records_for_range(self.active_descriptor_range.as_ref());
        smelt_perf::perf::record_value(
            "transcript:descriptor_window:active_records",
            records.len() as u64,
        );
        if records.is_empty() {
            return;
        }
        let inline_options = self.projection.inline_options().clone();
        self.transcript = Transcript::from_descriptor_records_with_ids(records);
        self.projection = crate::content::transcript_buf::TranscriptProjection::new();
        self.projection.set_inline_options(inline_options);
        self.render_cache.clear();
        self.extent_index.clear_exact_descriptor_rows();
    }

    fn approximate_average_descriptor_rows(&self, width: u16) -> RowIndex {
        self.extent_index
            .approximate_average_rows_per_loaded_descriptor(&self.sparse_descriptors, width)
    }

    fn approximate_sparse_prefix_row_offset(&mut self, width: u16) -> RowIndex {
        self.extent_index.approximate_sparse_prefix_rows(
            &self.sparse_descriptors,
            self.active_descriptor_range.as_ref(),
            self.session_dir.as_ref(),
            width,
        )
    }

    fn approximate_sparse_suffix_rows(&mut self, width: u16) -> RowIndex {
        self.extent_index.approximate_sparse_suffix_rows(
            &self.sparse_descriptors,
            self.active_descriptor_range.as_ref(),
            self.session_dir.as_ref(),
            width,
        )
    }

    fn observe_exact_loaded_descriptor_rows(&mut self) {
        let snapshot = self.projection.exact_height_snapshot();
        self.extent_index
            .observe_exact_loaded_descriptor_rows(&self.sparse_descriptors, snapshot);
    }

    fn approximate_mixed_scrollbar_total_rows(
        &mut self,
        width: u16,
        exact_loaded_rows: RowIndex,
    ) -> RowIndex {
        self.extent_index.approximate_mixed_scrollbar_total_rows(
            &self.sparse_descriptors,
            self.active_descriptor_range.as_ref(),
            self.session_dir.as_ref(),
            width,
            exact_loaded_rows,
        )
    }

    fn exact_loaded_row_for_virtual_content_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
    ) -> Option<RowIndex> {
        let (loaded_start, loaded_end) = self.active_virtual_row_span(lua, width)?;
        (loaded_start <= row && row < loaded_end).then_some(row.saturating_sub(loaded_start))
    }

    fn offset_node_row(
        &mut self,
        width: u16,
        mut node: crate::content::transcript_buf::TranscriptNodeRow,
    ) -> crate::content::transcript_buf::TranscriptNodeRow {
        let offset = self.approximate_sparse_prefix_row_offset(width);
        node.first_row = node.first_row.saturating_add(offset);
        node
    }

    pub(crate) fn set_inline_options(&mut self, options: InlineOptions) {
        self.projection.set_inline_options(options);
        self.extent_index.clear_exact_descriptor_rows();
    }

    pub(crate) fn invalidate_theme(&mut self) {
        self.projection.invalidate_theme();
        self.extent_index.clear_exact_descriptor_rows();
    }

    pub(crate) fn build_rows(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
    ) -> Arc<Vec<String>> {
        self.projection
            .build_rows(lua, &mut self.transcript.history, width, theme)
    }

    pub(crate) fn approximate_scrollbar_total_rows(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> crate::smelt_edit::RowIndex {
        let loaded_rows =
            self.projection
                .estimated_total_rows(lua, &mut self.transcript.history, width);
        self.approximate_mixed_scrollbar_total_rows(width, loaded_rows)
    }

    pub(crate) fn materialize_exact_loaded_block_layout(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> Vec<(
        BlockId,
        crate::smelt_edit::RowIndex,
        crate::smelt_edit::RowIndex,
    )> {
        let offset = self.approximate_sparse_prefix_row_offset(width);
        self.projection
            .materialize_block_layout(lua, &mut self.transcript.history, width)
            .into_iter()
            .map(|(id, first_row, rows)| (id, first_row.saturating_add(offset), rows))
            .collect()
    }

    fn block_snapshots_from_layout(
        &self,
        layout: impl Iterator<
            Item = (
                BlockId,
                crate::smelt_edit::RowIndex,
                crate::smelt_edit::RowIndex,
            ),
        >,
    ) -> Vec<TranscriptBlockSnapshot> {
        let mut out = Vec::new();
        let history = self.history();
        for (block_id, first_row, rows) in layout {
            let Some(idx) = history.order.iter().position(|id| *id == block_id) else {
                continue;
            };
            let Some(block) = history.block(block_id) else {
                continue;
            };
            out.push((
                idx,
                transcript_block_role(block),
                first_row,
                rows,
                transcript_block_first_line(block),
            ));
        }
        out
    }

    pub(crate) fn visible_block_snapshots(&self) -> Vec<TranscriptBlockSnapshot> {
        self.block_snapshots_from_layout(self.visible_block_layout())
    }

    pub(crate) fn materialize_block_snapshots(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> Vec<TranscriptBlockSnapshot> {
        let layout = self.materialize_exact_loaded_block_layout(lua, width);
        self.block_snapshots_from_layout(layout.into_iter())
    }

    pub(crate) fn block_snapshot_before_or_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        role: Option<&str>,
    ) -> Option<TranscriptBlockSnapshot> {
        let _ = self.activate_descriptor_window_for_approximate_display_row(width, row, 20);
        loop {
            if let Some(node) =
                self.loaded_block_node_before_or_at_virtual_row(lua, width, row, |history, id| {
                    role.is_none_or(|role| transcript_history_role(history, id) == role)
                        && !transcript_raw_first_line(history, id).is_empty()
                })
            {
                let block_id = node.id.as_block_id()?;
                let history = self.history();
                let idx = history.order.iter().position(|id| *id == block_id)?;
                return Some((
                    idx,
                    transcript_history_role(history, block_id),
                    node.first_row,
                    node.rows,
                    transcript_raw_first_line(history, block_id),
                ));
            }

            if !self.activate_previous_descriptor_window(width, 20) {
                return None;
            }
        }
    }

    fn navigation_block_from_record(
        index: usize,
        record: &TranscriptBlockRecordWithId,
        current_block_id: BlockId,
        current_row_offset: RowIndex,
    ) -> Option<TranscriptNavigationBlock> {
        let first_line = descriptor_first_line(&record.record.descriptor);
        if first_line.is_empty() {
            return None;
        }
        Some(TranscriptNavigationBlock {
            descriptor_index: index,
            block_id: record.block_id,
            role: descriptor_role(&record.record.descriptor),
            first_line,
            already_at_anchor: record.block_id == current_block_id && current_row_offset == 0,
        })
    }

    fn descriptor_index_for_block_id(&self, block_id: BlockId) -> Option<usize> {
        if let Some(index) = self
            .sparse_descriptors
            .descriptor_index_for_block_id(block_id)
        {
            return Some(index.get());
        }
        if self.sparse_descriptors.total_count().is_none() {
            return self.history().order.iter().position(|id| *id == block_id);
        }
        None
    }

    fn content_anchor_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
        bias: TranscriptAnchorBias,
    ) -> Option<TranscriptContentAnchor> {
        let row_anchor = self.row_anchor_at_row(lua, width, row)?;
        let block_id = row_anchor.id.as_block_id()?;
        let descriptor_index = self.descriptor_index_for_block_id(block_id)?;
        Some(TranscriptContentAnchor {
            descriptor_index,
            block_id,
            intra_block_row: row_anchor.row_offset,
            bias,
            row_anchor,
            fallback_row: row,
        })
    }

    fn row_for_content_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        anchor: TranscriptContentAnchor,
    ) -> Option<RowIndex> {
        if self.sparse_descriptors.total_count().is_some() {
            let range = self.descriptor_window_range_around_center(
                width,
                anchor.descriptor_index,
                viewport_rows,
                true,
            )?;
            let _ = self.activate_descriptor_window_range(range);
        }
        self.row_for_anchor(lua, width, anchor.row_anchor)
    }

    fn content_anchor_at_or_after_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        top_row: RowIndex,
        viewport_rows: u16,
    ) -> Option<(TranscriptContentAnchor, isize)> {
        let viewport_rows = RowIndex::from(viewport_rows.max(1));
        for offset in 0..viewport_rows {
            let row = top_row.saturating_add(offset);
            if let Some(anchor) =
                self.content_anchor_at_row(lua, width, row, TranscriptAnchorBias::Top)
            {
                return Some((anchor, -(offset as isize)));
            }
        }

        let visible_end = top_row.saturating_add(viewport_rows);
        let (block_id, first_row, rows) = self
            .materialize_exact_loaded_block_layout(lua, width)
            .into_iter()
            .find(|(_, first_row, rows)| {
                let end = first_row.saturating_add(*rows);
                *first_row < visible_end && end > top_row
            })?;
        let descriptor_index = self.descriptor_index_for_block_id(block_id)?;
        let intra_block_row = top_row
            .saturating_sub(first_row)
            .min(rows.saturating_sub(1));
        let anchor_row = first_row.saturating_add(intra_block_row);
        let offset = if anchor_row >= top_row {
            -(anchor_row
                .saturating_sub(top_row)
                .min(isize::MAX as RowIndex) as isize)
        } else {
            top_row
                .saturating_sub(anchor_row)
                .min(isize::MAX as RowIndex) as isize
        };
        Some((
            TranscriptContentAnchor {
                descriptor_index,
                block_id,
                intra_block_row,
                bias: TranscriptAnchorBias::Top,
                row_anchor: crate::content::transcript_buf::TranscriptRowAnchor {
                    id: crate::content::render_plan::RenderNodeId::Block(block_id),
                    row_offset: intra_block_row,
                },
                fallback_row: anchor_row,
            },
            offset,
        ))
    }

    fn current_viewport_descriptor_anchor(&self) -> Option<(usize, BlockId, RowIndex)> {
        if let Some(TranscriptScrollAnchor::Content(anchor)) = self.viewport_state.top_anchor {
            return Some((
                anchor.descriptor_index,
                anchor.block_id,
                anchor.intra_block_row,
            ));
        }

        if let Some(active) = self.active_descriptor_range.as_ref() {
            let index = active.start.get();
            if let Some(record) = self
                .sparse_descriptors
                .record(smelt_store::TranscriptDescriptorIndex::new(index))
            {
                return Some((index, record.block_id, 0));
            }
        }

        self.history()
            .order
            .first()
            .copied()
            .map(|block_id| (0, block_id, 0))
    }

    fn navigation_record_from_store(
        &self,
        role: &str,
        anchor_index: usize,
        direction: TranscriptNavigationDirection,
    ) -> Option<(usize, TranscriptBlockRecordWithId)> {
        let session_dir = self.session_dir.clone()?;
        let db = smelt_store::SessionDb::open_read_only(session_dir.join("session.db")).ok()?;
        let record = match direction {
            TranscriptNavigationDirection::Previous => {
                let before = anchor_index.checked_sub(1)?;
                db.read_transcript_descriptor_before_kind(role, before as u64)
                    .ok()
                    .flatten()?
            }
            TranscriptNavigationDirection::Next => db
                .read_transcript_descriptor_after_kind(role, anchor_index.saturating_add(1) as u64)
                .ok()
                .flatten()?,
        };
        let index = record.block_idx as usize;
        let record = TranscriptBlockRecordWithId::try_from(record).ok()?;
        Some((index, record))
    }

    fn choose_navigation_record(
        loaded: Option<(usize, TranscriptBlockRecordWithId)>,
        stored: Option<(usize, TranscriptBlockRecordWithId)>,
        direction: TranscriptNavigationDirection,
    ) -> Option<(usize, TranscriptBlockRecordWithId)> {
        match (loaded, stored) {
            (Some(loaded), Some(stored)) => match direction {
                TranscriptNavigationDirection::Previous => {
                    Some(if loaded.0 >= stored.0 { loaded } else { stored })
                }
                TranscriptNavigationDirection::Next => {
                    Some(if loaded.0 <= stored.0 { loaded } else { stored })
                }
            },
            (Some(record), None) | (None, Some(record)) => Some(record),
            (None, None) => None,
        }
    }

    fn navigation_block(
        &self,
        role: Option<&str>,
        direction: TranscriptNavigationDirection,
    ) -> Option<TranscriptNavigationBlock> {
        let (anchor_index, current_block_id, current_row_offset) =
            self.current_viewport_descriptor_anchor()?;
        let role = role.unwrap_or("user");

        if self.sparse_descriptors.total_count().is_some() {
            let anchor = smelt_store::TranscriptDescriptorIndex::new(anchor_index);
            let loaded = self
                .sparse_descriptors
                .navigation_record(role, anchor, direction);
            let stored = self.navigation_record_from_store(role, anchor_index, direction);
            let (index, record) = Self::choose_navigation_record(loaded, stored, direction)?;
            return Self::navigation_block_from_record(
                index,
                &record,
                current_block_id,
                current_row_offset,
            );
        }

        let history = self.history();
        let iter: Box<dyn Iterator<Item = (usize, BlockId)> + '_> = match direction {
            TranscriptNavigationDirection::Previous => Box::new(
                history
                    .order
                    .iter()
                    .copied()
                    .enumerate()
                    .take(anchor_index)
                    .rev(),
            ),
            TranscriptNavigationDirection::Next => Box::new(
                history
                    .order
                    .iter()
                    .copied()
                    .enumerate()
                    .skip(anchor_index.saturating_add(1)),
            ),
        };
        for (index, block_id) in iter {
            if transcript_history_role(history, block_id) != role {
                continue;
            }
            let first_line = transcript_raw_first_line(history, block_id);
            if first_line.is_empty() {
                continue;
            }
            return Some(TranscriptNavigationBlock {
                descriptor_index: index,
                block_id,
                role: transcript_history_role(history, block_id),
                first_line,
                already_at_anchor: block_id == current_block_id && current_row_offset == 0,
            });
        }
        None
    }

    pub(crate) fn previous_navigation_block(
        &self,
        role: Option<&str>,
    ) -> Option<TranscriptNavigationBlock> {
        self.navigation_block(role, TranscriptNavigationDirection::Previous)
    }

    pub(crate) fn next_navigation_block(
        &self,
        role: Option<&str>,
    ) -> Option<TranscriptNavigationBlock> {
        self.navigation_block(role, TranscriptNavigationDirection::Next)
    }

    fn descriptor_block_target_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        descriptor_index: usize,
        row_offset: RowIndex,
        viewport_rows: u16,
    ) -> Option<(BlockId, RowIndex)> {
        let block_id = if self.sparse_descriptors.total_count().is_some() {
            let range = self.descriptor_window_range_around_center(
                width,
                descriptor_index,
                viewport_rows,
                true,
            )?;
            let _ = self.activate_descriptor_window_range(range);
            self.sparse_descriptors
                .record(smelt_store::TranscriptDescriptorIndex::new(
                    descriptor_index,
                ))?
                .block_id
        } else {
            self.history().order.get(descriptor_index).copied()?
        };

        let (_, first_row, rows) = self
            .materialize_exact_loaded_block_layout(lua, width)
            .into_iter()
            .find(|(id, _, _)| *id == block_id)?;
        let row_offset = row_offset.min(rows.saturating_sub(1));
        Some((block_id, first_row.saturating_add(row_offset)))
    }

    pub(crate) fn descriptor_block_reveal_position(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        descriptor_index: usize,
        row_offset: RowIndex,
        screen_padding_top: RowIndex,
        viewport_rows: u16,
    ) -> Option<TranscriptBlockRevealPosition> {
        let (block_id, target_row) = self.descriptor_block_target_row(
            lua,
            width,
            descriptor_index,
            row_offset,
            viewport_rows,
        )?;
        Some(TranscriptBlockRevealPosition {
            block_id,
            target_row,
            scroll_top: target_row.saturating_sub(screen_padding_top),
        })
    }

    pub(crate) fn materialize_exact_loaded_search_layout(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> crate::content::transcript_buf::TranscriptSearchLayout {
        let offset = self.approximate_sparse_prefix_row_offset(width);
        let mut layout =
            self.projection
                .materialize_search_layout(lua, &mut self.transcript.history, width);
        for entry in &mut layout.entries {
            entry.first_row = entry.first_row.saturating_add(offset);
        }
        layout
    }

    pub(crate) fn materialize_exact_loaded_search_layout_for_blocks(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        block_indices: &[u64],
    ) -> crate::content::transcript_buf::TranscriptSearchLayout {
        let offset = self.approximate_sparse_prefix_row_offset(width);
        let mut layout = self.projection.materialize_search_layout_for_blocks(
            lua,
            &mut self.transcript.history,
            width,
            block_indices,
        );
        for entry in &mut layout.entries {
            entry.first_row = entry.first_row.saturating_add(offset);
        }
        layout
    }

    pub(crate) fn block_id_at_or_before_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        forward: bool,
    ) -> Option<BlockId> {
        let local_row = self.exact_loaded_row_for_virtual_content_row(lua, width, row)?;
        self.projection.block_id_at_or_before_row(
            lua,
            &mut self.transcript.history,
            width,
            local_row,
            forward,
        )
    }

    pub(crate) fn visible_block_layout(
        &self,
    ) -> impl Iterator<
        Item = (
            BlockId,
            crate::smelt_edit::RowIndex,
            crate::smelt_edit::RowIndex,
        ),
    > + '_ {
        self.projection.visible_block_layout()
    }

    fn descriptor_window_count(&self, width: u16, viewport_rows: u16, total: usize) -> usize {
        let avg_rows = self.approximate_average_descriptor_rows(width).max(1);
        let visible_descriptors =
            (RowIndex::from(viewport_rows.max(1)) / avg_rows).saturating_add(1) as usize;
        visible_descriptors.saturating_mul(4).max(32).min(total)
    }

    fn previous_descriptor_window_range(
        &self,
        width: u16,
        viewport_rows: u16,
    ) -> Option<Range<smelt_store::TranscriptDescriptorIndex>> {
        let active = self.active_descriptor_range.clone()?;
        let end = active.start.get();
        if end == 0 {
            return None;
        }
        let total = self.sparse_descriptors.total_count()?;
        let count = self.descriptor_window_count(width, viewport_rows, total);
        let start = end.saturating_sub(count);
        Some(
            smelt_store::TranscriptDescriptorIndex::new(start)
                ..smelt_store::TranscriptDescriptorIndex::new(end),
        )
    }

    fn activate_previous_descriptor_window(&mut self, width: u16, viewport_rows: u16) -> bool {
        let Some(range) = self.previous_descriptor_window_range(width, viewport_rows) else {
            return false;
        };
        self.activate_descriptor_window_range(range)
    }

    fn descriptor_window_range_around_center(
        &self,
        width: u16,
        center: usize,
        viewport_rows: u16,
        reuse_active: bool,
    ) -> Option<Range<smelt_store::TranscriptDescriptorIndex>> {
        let total = self.sparse_descriptors.total_count()?;
        if total == 0 {
            return None;
        }
        let center = center.min(total.saturating_sub(1));
        if reuse_active {
            if let Some(active) = self.active_descriptor_range.as_ref() {
                if active.start.get() <= center && center < active.end.get() {
                    return Some(active.clone());
                }
            }
        }
        let count = self.descriptor_window_count(width, viewport_rows, total);
        let start = center
            .saturating_sub(count / 2)
            .min(total.saturating_sub(count));
        let end = start.saturating_add(count).min(total);
        Some(
            smelt_store::TranscriptDescriptorIndex::new(start)
                ..smelt_store::TranscriptDescriptorIndex::new(end),
        )
    }

    fn descriptor_window_range_for_approximate_display_row(
        &self,
        width: u16,
        row: RowIndex,
        viewport_rows: u16,
    ) -> Option<Range<smelt_store::TranscriptDescriptorIndex>> {
        let avg_rows = self.approximate_average_descriptor_rows(width).max(1);
        let total = self.sparse_descriptors.total_count()?;
        let center = ((row / avg_rows) as usize).min(total.saturating_sub(1));
        self.descriptor_window_range_around_center(width, center, viewport_rows, true)
    }

    fn tail_descriptor_window_range(
        &self,
        width: u16,
        viewport_rows: u16,
    ) -> Option<Range<smelt_store::TranscriptDescriptorIndex>> {
        let total = self.sparse_descriptors.total_count()?;
        if let Some(active) = self.active_descriptor_range.as_ref() {
            if active.end.get() == total {
                return Some(active.clone());
            }
        }
        self.descriptor_window_range_around_center(
            width,
            total.saturating_sub(1),
            viewport_rows,
            false,
        )
    }

    fn activate_descriptor_window_range(
        &mut self,
        range: Range<smelt_store::TranscriptDescriptorIndex>,
    ) -> bool {
        if self.active_descriptor_range.as_ref() == Some(&range) {
            return false;
        }
        if self.sparse_descriptors.range_is_loaded(&range) {
            self.active_descriptor_range = Some(range);
            self.install_active_descriptor_projection();
            return true;
        }
        let Some(window) = self.load_descriptor_window((range.start.get()..range.end.get()).into())
        else {
            return false;
        };
        self.merge_descriptor_window(window)
    }

    fn activate_descriptor_window_for_approximate_display_row(
        &mut self,
        width: u16,
        row: RowIndex,
        viewport_rows: u16,
    ) -> bool {
        let Some(range) =
            self.descriptor_window_range_for_approximate_display_row(width, row, viewport_rows)
        else {
            return false;
        };
        self.activate_descriptor_window_range(range)
    }

    fn active_virtual_row_span(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> Option<(RowIndex, RowIndex)> {
        let loaded_rows =
            self.projection
                .estimated_total_rows(lua, &mut self.transcript.history, width);
        if self.active_descriptor_range.is_none() {
            return self
                .sparse_descriptors
                .total_count()
                .is_none()
                .then_some((0, loaded_rows));
        }
        let row_offset = self.approximate_sparse_prefix_row_offset(width);
        Some((row_offset, row_offset.saturating_add(loaded_rows)))
    }

    fn descriptor_window_expanded_toward_row(
        &self,
        width: u16,
        row: RowIndex,
        loaded_start: RowIndex,
        loaded_end: RowIndex,
    ) -> Option<Range<smelt_store::TranscriptDescriptorIndex>> {
        let active = self.active_descriptor_range.clone()?;
        let total = self.sparse_descriptors.total_count()?;
        let avg_rows = self.approximate_average_descriptor_rows(width).max(1);
        let missing_rows = if row < loaded_start {
            loaded_start.saturating_sub(row)
        } else {
            row.saturating_sub(loaded_end).saturating_add(1)
        };
        let missing_descriptors = missing_rows
            .saturating_add(avg_rows.saturating_sub(1))
            .saturating_div(avg_rows)
            .max(1) as usize;
        let (start, end) = if row < loaded_start {
            (
                active.start.get().saturating_sub(missing_descriptors),
                active.end.get(),
            )
        } else {
            (
                active.start.get(),
                active
                    .end
                    .get()
                    .saturating_add(missing_descriptors)
                    .min(total),
            )
        };
        let range = smelt_store::TranscriptDescriptorIndex::new(start)
            ..smelt_store::TranscriptDescriptorIndex::new(end);
        (self.active_descriptor_range.as_ref() != Some(&range)).then_some(range)
    }

    fn activate_descriptor_window_covering_approximate_display_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: RowIndex,
        viewport_rows: u16,
    ) {
        let viewport_row_count = RowIndex::from(viewport_rows.max(1));
        let _ =
            self.activate_descriptor_window_for_approximate_display_row(width, row, viewport_rows);
        while let Some((loaded_start, loaded_end)) = self.active_virtual_row_span(lua, width) {
            let target_end = row.saturating_add(viewport_row_count);
            if row >= loaded_start && target_end <= loaded_end {
                return;
            }
            let edge = if row < loaded_start { row } else { target_end };
            let Some(range) =
                self.descriptor_window_expanded_toward_row(width, edge, loaded_start, loaded_end)
            else {
                return;
            };
            if !self.activate_descriptor_window_range(range) {
                return;
            }
        }
    }

    fn activate_tail_descriptor_window(&mut self, width: u16, viewport_rows: u16) -> bool {
        let Some(range) = self.tail_descriptor_window_range(width, viewport_rows) else {
            return false;
        };
        self.activate_descriptor_window_range(range)
    }

    fn scroll_anchor_for_projection_target(
        &self,
        scroll_target: crate::content::transcript_buf::ScrollTarget,
    ) -> TranscriptScrollAnchor {
        match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => TranscriptScrollAnchor::Tail,
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ExactRow(row)
                | crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(row),
            ) => TranscriptScrollAnchor::EstimatedRow(row),
        }
    }

    fn capture_viewport_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        top_row: RowIndex,
        viewport_rows: u16,
        fallback: TranscriptScrollAnchor,
    ) {
        let (top_anchor, top_offset_rows) = match fallback {
            TranscriptScrollAnchor::Tail => (Some(TranscriptScrollAnchor::Tail), 0),
            TranscriptScrollAnchor::Content(_) | TranscriptScrollAnchor::EstimatedRow(_) => self
                .content_anchor_at_or_after_row(lua, width, top_row, viewport_rows)
                .map(|(anchor, offset)| (Some(TranscriptScrollAnchor::Content(anchor)), offset))
                .unwrap_or((Some(fallback), 0)),
        };
        self.viewport_state.mode = match top_anchor {
            Some(TranscriptScrollAnchor::Tail) => TranscriptViewportMode::Tail,
            Some(TranscriptScrollAnchor::Content(_)) => TranscriptViewportMode::Anchored,
            Some(TranscriptScrollAnchor::EstimatedRow(_)) | None => TranscriptViewportMode::FarSeek,
        };
        self.viewport_state.top_anchor = top_anchor;
        self.viewport_state.top_offset_rows = top_offset_rows;
    }

    fn take_viewport_intent(
        &mut self,
        input: TranscriptViewportProjectionInput,
    ) -> TranscriptScrollIntent {
        if let Some(intent) = self.viewport_state.pending_intent.take() {
            return intent;
        }
        if input.follow_tail {
            TranscriptScrollIntent::Tail
        } else if input.width_changed {
            TranscriptScrollIntent::ResizeReflow {
                previous_width: input.previous_width.unwrap_or_default(),
            }
        } else {
            TranscriptScrollIntent::PreserveViewport
        }
    }

    fn max_scroll_for_total(total_rows: RowIndex, viewport_rows: u16) -> RowIndex {
        total_rows.saturating_sub(RowIndex::from(viewport_rows.max(1)))
    }

    fn add_rows(row: RowIndex, delta: isize) -> RowIndex {
        if delta >= 0 {
            row.saturating_add(delta as RowIndex)
        } else {
            row.saturating_sub(delta.unsigned_abs() as RowIndex)
        }
    }

    fn row_for_trace_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        anchor: TranscriptTraceAnchor,
    ) -> Option<RowIndex> {
        match anchor {
            TranscriptTraceAnchor::Tail => None,
            TranscriptTraceAnchor::EstimatedRow(row) => Some(row),
            TranscriptTraceAnchor::Content {
                descriptor_index,
                block_id,
                row_offset,
                ..
            } => {
                if self.sparse_descriptors.total_count().is_some() {
                    let range = self.descriptor_window_range_around_center(
                        width,
                        descriptor_index,
                        viewport_rows,
                        true,
                    )?;
                    let _ = self.activate_descriptor_window_range(range);
                }
                self.row_for_anchor(
                    lua,
                    width,
                    crate::content::transcript_buf::TranscriptRowAnchor {
                        id: crate::content::render_plan::RenderNodeId::Block(block_id),
                        row_offset,
                    },
                )
            }
        }
    }

    fn row_for_viewport_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        fallback_scroll_top: RowIndex,
    ) -> Option<RowIndex> {
        match self.viewport_state.top_anchor {
            // Tail-follow is an input intent, not durable row authority. Local
            // movement after tail-follow starts from the last resolved paint row
            // so generic Window pre-scroll cannot apply the same delta twice.
            Some(TranscriptScrollAnchor::Tail) => self
                .viewport_state
                .resolved_scroll_top
                .or(Some(fallback_scroll_top)),
            Some(TranscriptScrollAnchor::Content(anchor)) => {
                self.row_for_content_anchor(lua, width, viewport_rows, anchor)
            }
            Some(TranscriptScrollAnchor::EstimatedRow(row)) => Some(row),
            None => Some(fallback_scroll_top),
        }
        .map(|row| Self::add_rows(row, self.viewport_state.top_offset_rows))
    }

    fn approximate_scrollbar_total_for_viewport(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> RowIndex {
        let loaded_rows =
            self.projection
                .estimated_total_rows(lua, &mut self.transcript.history, width);
        self.approximate_mixed_scrollbar_total_rows(width, loaded_rows)
    }

    fn activate_descriptor_window_covering_local_delta(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        target_row: RowIndex,
    ) -> bool {
        const MAX_EXPANSIONS_PER_DELTA: usize = 4;
        let mut changed = false;
        let viewport_row_count = RowIndex::from(viewport_rows.max(1));
        for _ in 0..MAX_EXPANSIONS_PER_DELTA {
            let Some((loaded_start, loaded_end)) = self.active_virtual_row_span(lua, width) else {
                return changed;
            };
            let target_end = target_row.saturating_add(viewport_row_count);
            if target_row >= loaded_start && target_end <= loaded_end {
                return changed;
            }
            let edge = if target_row < loaded_start {
                target_row
            } else {
                target_end
            };
            let Some(range) =
                self.descriptor_window_expanded_toward_row(width, edge, loaded_start, loaded_end)
            else {
                return changed;
            };
            if !self.activate_descriptor_window_range(range) {
                return changed;
            }
            changed = true;
        }
        changed
    }

    fn local_delta_scroll_target(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        fallback_scroll_top: RowIndex,
        rows: isize,
    ) -> crate::content::transcript_buf::ScrollTarget {
        const MAX_LOCAL_DELTA_REBASES: usize = 4;
        let base = self
            .row_for_viewport_anchor(lua, width, viewport_rows, fallback_scroll_top)
            .unwrap_or(fallback_scroll_top);
        let mut row = Self::add_rows(base, rows);
        for _ in 0..MAX_LOCAL_DELTA_REBASES {
            if !self.activate_descriptor_window_covering_local_delta(lua, width, viewport_rows, row)
            {
                break;
            }
            let base = self
                .row_for_viewport_anchor(lua, width, viewport_rows, fallback_scroll_top)
                .unwrap_or(fallback_scroll_top);
            row = Self::add_rows(base, rows);
        }

        crate::content::transcript_buf::ScrollTarget::visible_row(row)
    }

    fn scroll_target_for_intent(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        viewport_rows: u16,
        fallback_scroll_top: RowIndex,
        intent: &TranscriptScrollIntent,
    ) -> crate::content::transcript_buf::ScrollTarget {
        match intent {
            TranscriptScrollIntent::Tail => {
                crate::content::transcript_buf::ScrollTarget::visible_tail()
            }
            TranscriptScrollIntent::PreserveViewport => {
                match self.row_for_viewport_anchor(lua, width, viewport_rows, fallback_scroll_top) {
                    Some(row) => {
                        crate::content::transcript_buf::ScrollTarget::visible_reflow_stable_row(row)
                    }
                    None => crate::content::transcript_buf::ScrollTarget::visible_tail(),
                }
            }
            TranscriptScrollIntent::ResizeReflow { .. } => {
                crate::content::transcript_buf::ScrollTarget::visible_reflow_stable_row(
                    fallback_scroll_top,
                )
            }
            TranscriptScrollIntent::UserDelta { rows } => self.local_delta_scroll_target(
                lua,
                width,
                viewport_rows,
                fallback_scroll_top,
                *rows,
            ),
            TranscriptScrollIntent::PageDelta { pages } => {
                let rows = pages.saturating_mul(viewport_rows.max(1) as isize);
                self.local_delta_scroll_target(lua, width, viewport_rows, fallback_scroll_top, rows)
            }
            TranscriptScrollIntent::ExactContentAnchor(anchor)
            | TranscriptScrollIntent::SearchJump(anchor) => {
                let Some(row) = self.row_for_trace_anchor(lua, width, viewport_rows, *anchor)
                else {
                    return crate::content::transcript_buf::ScrollTarget::visible_tail();
                };
                if matches!(*anchor, TranscriptTraceAnchor::EstimatedRow(_)) {
                    self.activate_descriptor_window_covering_approximate_display_row(
                        lua,
                        width,
                        row,
                        viewport_rows,
                    );
                }
                crate::content::transcript_buf::ScrollTarget::visible_reflow_stable_row(row)
            }
            TranscriptScrollIntent::RevealBlock {
                descriptor_index,
                block_id,
                row_offset,
                screen_padding_top,
            } => {
                let Some(reveal) = self.descriptor_block_reveal_position(
                    lua,
                    width,
                    *descriptor_index,
                    *row_offset,
                    *screen_padding_top,
                    viewport_rows,
                ) else {
                    return crate::content::transcript_buf::ScrollTarget::visible_tail();
                };
                if reveal.block_id != *block_id {
                    return crate::content::transcript_buf::ScrollTarget::visible_tail();
                }
                crate::content::transcript_buf::ScrollTarget::visible_reflow_stable_row(
                    reveal.scroll_top,
                )
            }
            TranscriptScrollIntent::ScrollbarFraction {
                numerator,
                denominator,
            } => {
                let total_rows = self.approximate_scrollbar_total_for_viewport(lua, width);
                let max_scroll = Self::max_scroll_for_total(total_rows, viewport_rows);
                let denominator = (*denominator).max(1);
                let row = ((*numerator).min(denominator) as u128)
                    .saturating_mul(max_scroll as u128)
                    .checked_div(denominator as u128)
                    .unwrap_or(0)
                    .min(RowIndex::MAX as u128) as RowIndex;
                crate::content::transcript_buf::ScrollTarget::visible_row(row)
            }
            TranscriptScrollIntent::ApproximateRowSeek(row) => {
                crate::content::transcript_buf::ScrollTarget::visible_row(*row)
            }
        }
    }

    pub(crate) fn plan_viewport_projection_measured(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        input: TranscriptViewportProjectionInput,
        viewport_rows: u16,
    ) -> TranscriptProjectionPlan {
        let intent = self.take_viewport_intent(input);
        let allow_sparse_placeholders = Self::intent_allows_sparse_placeholders(&intent);
        let scroll_target = self.scroll_target_for_intent(
            lua,
            width,
            viewport_rows,
            input.fallback_scroll_top,
            &intent,
        );
        if self.scroll_trace_enabled() && !self.scroll_trace_has_pending_input() {
            self.set_next_scroll_trace_input(TranscriptScrollTraceRenderInput {
                input_event_or_tick: "render_frame".to_string(),
                scroll_intent: intent,
                window_scroll_before: self
                    .last_traced_resolved_scroll_top()
                    .unwrap_or(input.fallback_scroll_top),
                window_scroll_after_input: input.fallback_scroll_top,
            });
        }
        self.plan_projection_measured_with_sparse_placeholders(
            lua,
            width,
            theme,
            scroll_target,
            viewport_rows,
            allow_sparse_placeholders,
        )
    }

    pub(crate) fn plan_projection_measured(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        scroll_target: crate::content::transcript_buf::ScrollTarget,
        viewport_rows: u16,
    ) -> TranscriptProjectionPlan {
        self.plan_projection_measured_with_sparse_placeholders(
            lua,
            width,
            theme,
            scroll_target,
            viewport_rows,
            true,
        )
    }

    fn plan_projection_measured_with_sparse_placeholders(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        scroll_target: crate::content::transcript_buf::ScrollTarget,
        viewport_rows: u16,
        allow_sparse_placeholders: bool,
    ) -> TranscriptProjectionPlan {
        let (trace_frame, trace_started_at) = self
            .start_scroll_trace_frame(width, scroll_target)
            .map_or((None, None), |(frame, started_at)| {
                (Some(frame), started_at)
            });
        let scroll_anchor = self.scroll_anchor_for_projection_target(scroll_target);
        match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ExactRow(row),
            ) => {
                self.activate_descriptor_window_covering_approximate_display_row(
                    lua,
                    width,
                    row,
                    viewport_rows,
                );
            }
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(_),
            ) => {}
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => {
                let _ = self.activate_tail_descriptor_window(width, viewport_rows);
            }
        }
        let row_offset = self.approximate_sparse_prefix_row_offset(width);
        let requested_scroll = match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ExactRow(row),
            ) => Some(row),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(_)
                | crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => None,
        };
        let local_target = match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ExactRow(row),
            ) => crate::content::transcript_buf::ScrollTarget::visible_row(
                row.saturating_sub(row_offset),
            ),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(row),
            ) => crate::content::transcript_buf::ScrollTarget::visible_reflow_stable_row(
                row.saturating_sub(row_offset),
            ),
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => crate::content::transcript_buf::ScrollTarget::visible_tail(),
        };
        let inner = self.projection.plan_projection_measured(
            lua,
            &mut self.transcript.history,
            width,
            theme,
            local_target,
            viewport_rows,
        );
        let loaded_rows =
            self.projection
                .estimated_total_rows(lua, &mut self.transcript.history, width);
        let total_rows = self.approximate_mixed_scrollbar_total_rows(width, loaded_rows);
        let viewport_row_count = RowIndex::from(viewport_rows.max(1));
        let loaded_start = row_offset;
        let loaded_end = row_offset.saturating_add(loaded_rows).min(total_rows);
        let sparse_gap = if allow_sparse_placeholders {
            match scroll_target {
                crate::content::transcript_buf::ScrollTarget::Visible(
                    crate::content::transcript_buf::ScrollAnchor::ExactRow(row)
                    | crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(row),
                ) if row < loaded_start => {
                    let scroll_top = row.min(total_rows.saturating_sub(viewport_row_count));
                    Some(SparseProjectionGap {
                        scroll_top,
                        row_base: scroll_top
                            .saturating_sub(viewport_row_count / 2)
                            .min(loaded_start),
                        end: loaded_start,
                    })
                }
                crate::content::transcript_buf::ScrollTarget::Visible(
                    crate::content::transcript_buf::ScrollAnchor::ExactRow(row)
                    | crate::content::transcript_buf::ScrollAnchor::ReflowStableRow(row),
                ) if row >= loaded_end && loaded_end < total_rows => {
                    let scroll_top = row.min(total_rows.saturating_sub(viewport_row_count));
                    Some(SparseProjectionGap {
                        scroll_top,
                        row_base: scroll_top
                            .saturating_sub(viewport_row_count / 2)
                            .max(loaded_end)
                            .min(total_rows),
                        end: total_rows,
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        let materialization = sparse_gap
            .map(TranscriptMaterializationPlan::UnloadedGap)
            .unwrap_or(TranscriptMaterializationPlan::Loaded(inner));
        TranscriptProjectionPlan {
            materialization,
            row_offset,
            total_rows,
            requested_scroll,
            scroll_anchor,
            width,
            viewport_rows,
            trace_frame,
            trace_started_at,
        }
    }

    fn project_unloaded_sparse_gap(
        &mut self,
        buf: &mut Buffer,
        total_rows: RowIndex,
        viewport_rows: u16,
        gap: SparseProjectionGap,
    ) -> crate::smelt_edit::MaterializedRows {
        let viewport_rows = RowIndex::from(viewport_rows.max(1));
        let materialized_rows = gap
            .end
            .saturating_sub(gap.row_base)
            .min(viewport_rows.saturating_mul(2))
            .min(total_rows.saturating_sub(gap.row_base));
        buf.set_all_lines(vec![String::new(); materialized_rows as usize]);
        self.viewport_state.top_anchor = Some(TranscriptScrollAnchor::EstimatedRow(gap.scroll_top));
        self.viewport_state.top_offset_rows = 0;
        self.viewport_state.mode = TranscriptViewportMode::FarSeek;
        self.viewport_state.resolved_scroll_top = Some(gap.scroll_top);
        crate::smelt_edit::MaterializedRows {
            clamped_scroll: gap.scroll_top,
            row_base: gap.row_base,
            total_rows,
            materialized_rows,
        }
    }

    fn applied_viewport(
        &self,
        rows: crate::smelt_edit::MaterializedRows,
        viewport_rows: u16,
        placeholder_rows_visible: bool,
    ) -> AppliedTranscriptViewport {
        let viewport_rows = RowIndex::from(viewport_rows.max(1));
        AppliedTranscriptViewport {
            materialized_rows: rows,
            top_anchor: self.viewport_state.top_anchor.map(Self::trace_anchor),
            scrollbar_total_rows: rows.total_rows,
            exact_visible_range: rows.clamped_scroll
                ..rows
                    .clamped_scroll
                    .saturating_add(viewport_rows)
                    .min(rows.total_rows),
            placeholder_rows_visible,
            follow_tail: self.viewport_state.mode == TranscriptViewportMode::Tail,
        }
    }

    pub(crate) fn project_applied_viewport(
        &mut self,
        lua: &LuaRuntime,
        buf: &mut Buffer,
        theme: &Theme,
        plan: TranscriptProjectionPlan,
    ) -> AppliedTranscriptViewport {
        let TranscriptProjectionPlan {
            materialization,
            row_offset,
            total_rows,
            requested_scroll,
            scroll_anchor,
            width,
            viewport_rows,
            trace_frame,
            trace_started_at,
        } = plan;
        let mut trace_ctx = TranscriptScrollTraceFinishContext {
            width,
            row_offset,
            viewport_rows,
            trace_frame,
            trace_started_at,
        };
        let mut rows = match materialization {
            TranscriptMaterializationPlan::UnloadedGap(gap) => {
                let rows = self.project_unloaded_sparse_gap(buf, total_rows, viewport_rows, gap);
                self.finish_scroll_trace_frame(lua, rows, &mut trace_ctx, true);
                return self.applied_viewport(rows, viewport_rows, true);
            }
            TranscriptMaterializationPlan::Loaded(inner) => self.projection.project_planned(
                lua,
                buf,
                &mut self.transcript.history,
                theme,
                inner,
            ),
        };
        self.observe_exact_loaded_descriptor_rows();
        rows.clamped_scroll = rows.clamped_scroll.saturating_add(row_offset);
        rows.row_base = rows.row_base.saturating_add(row_offset);
        rows.total_rows = total_rows;
        if let Some(requested) = requested_scroll {
            let viewport_rows = RowIndex::from(viewport_rows.max(1));
            let requested = requested.min(total_rows.saturating_sub(viewport_rows));
            if requested >= rows.row_base
                && requested.saturating_add(viewport_rows)
                    <= rows.row_base.saturating_add(rows.materialized_rows)
            {
                rows.clamped_scroll = requested;
            }
        }
        self.capture_viewport_anchor(
            lua,
            width,
            rows.clamped_scroll,
            viewport_rows,
            scroll_anchor,
        );
        self.viewport_state.resolved_scroll_top = Some(rows.clamped_scroll);
        self.finish_scroll_trace_frame(lua, rows, &mut trace_ctx, false);
        self.applied_viewport(rows, viewport_rows, false)
    }

    pub(crate) fn project_planned(
        &mut self,
        lua: &LuaRuntime,
        buf: &mut Buffer,
        theme: &Theme,
        plan: TranscriptProjectionPlan,
    ) -> crate::smelt_edit::MaterializedRows {
        self.project_applied_viewport(lua, buf, theme, plan)
            .materialized_rows
    }

    pub(crate) fn exact_or_gap_display_rows_for_range(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> crate::smelt_edit::DisplayRows {
        let row_offset = self.approximate_sparse_prefix_row_offset(width);
        let end = start.saturating_add(count);
        if count == 0 || end <= start {
            return DisplayRows::empty();
        }
        let mut rows = Vec::new();
        if start < row_offset {
            let prefix_end = end.min(row_offset);
            rows.extend(inert_sparse_gap_rows(prefix_end.saturating_sub(start)));
            if end <= row_offset {
                return DisplayRows { rows };
            }
        }
        let loaded_rows =
            self.projection
                .estimated_total_rows(lua, &mut self.transcript.history, width);
        let loaded_end = row_offset.saturating_add(loaded_rows);
        if start >= loaded_end {
            return DisplayRows {
                rows: inert_sparse_gap_rows(count),
            };
        }
        let local_start = start.saturating_sub(row_offset);
        let local_end = end.saturating_sub(row_offset).min(loaded_rows);
        let mut loaded = self.projection.display_rows_for_range(
            lua,
            &mut self.transcript.history,
            width,
            theme,
            local_start..local_end,
        );
        rows.append(&mut loaded.rows);
        if end > loaded_end {
            rows.extend(inert_sparse_gap_rows(end.saturating_sub(loaded_end)));
        }
        DisplayRows { rows }
    }

    pub(crate) fn search_matches_for_row_range(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        start: RowIndex,
        count: RowIndex,
        query: &str,
    ) -> Vec<DocRange> {
        if query.is_empty() || count == 0 {
            return Vec::new();
        }
        let display = self.exact_or_gap_display_rows_for_range(lua, width, theme, start, count);
        let mut matches = Vec::new();
        for (offset, row) in display.rows.iter().enumerate() {
            let row_index = start.saturating_add(offset as RowIndex);
            matches.extend(crate::smelt_edit::display_row_matches(
                row, row_index, query,
            ));
        }
        matches
    }

    pub(crate) fn cached_exact_or_gap_display_rows_for_range(
        &mut self,
        lua: &LuaRuntime,
        theme: &Theme,
        render: TranscriptRenderContext,
        start: RowIndex,
        count: RowIndex,
    ) -> DisplayRows {
        let key = TranscriptRenderCacheKey {
            generation: self.projection_generation(),
            width: render.width,
            theme: render.theme_key,
            renderer_generation: render.renderer_generation,
            renderer_cache_key: render.renderer_cache_key,
            start,
            count,
        };
        if let Some(rows) = self.render_cache.get(key) {
            smelt_perf::perf::record_value("transcript:render_cache:hit", 1);
            return rows;
        }
        smelt_perf::perf::record_value("transcript:render_cache:miss", 1);
        let rows = self.exact_or_gap_display_rows_for_range(lua, render.width, theme, start, count);
        self.render_cache.insert(key, rows.clone());
        rows
    }

    pub(crate) fn node_metadata_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        let local_row = self.exact_loaded_row_for_virtual_content_row(lua, width, row)?;
        self.projection
            .node_metadata_at_row(lua, &mut self.transcript.history, width, local_row)
            .map(|node| self.offset_node_row(width, node))
    }

    pub(crate) fn row_anchor_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::content::transcript_buf::TranscriptRowAnchor> {
        let local_row = self.exact_loaded_row_for_virtual_content_row(lua, width, row)?;
        self.projection
            .row_anchor_at_row(lua, &mut self.transcript.history, width, local_row)
    }

    pub(crate) fn row_for_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: crate::content::transcript_buf::TranscriptRowAnchor,
    ) -> Option<crate::smelt_edit::RowIndex> {
        let offset = self.approximate_sparse_prefix_row_offset(width);
        self.projection
            .row_for_anchor(lua, &mut self.transcript.history, width, anchor)
            .map(|row| row.saturating_add(offset))
    }

    fn position_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        position: crate::smelt_edit::DocPosition,
    ) -> TranscriptPositionAnchor {
        TranscriptPositionAnchor {
            anchor: self.content_anchor_at_row(lua, width, position.row, TranscriptAnchorBias::Top),
            position,
        }
    }

    fn range_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        range: crate::smelt_edit::DocRange,
    ) -> TranscriptRangeAnchor {
        TranscriptRangeAnchor {
            start: self.position_anchor(lua, width, range.start),
            end: self.position_anchor(lua, width, range.end),
        }
    }

    fn resolve_position_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: TranscriptPositionAnchor,
    ) -> crate::smelt_edit::DocPosition {
        let row = anchor
            .anchor
            .and_then(|anchor| self.row_for_content_anchor(lua, width, 20, anchor))
            .unwrap_or(anchor.position.row);
        crate::smelt_edit::DocPosition {
            row,
            byte_col: anchor.position.byte_col,
        }
    }

    fn resolve_range_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: TranscriptRangeAnchor,
    ) -> crate::smelt_edit::DocRange {
        crate::smelt_edit::DocRange {
            start: self.resolve_position_anchor(lua, width, anchor.start),
            end: self.resolve_position_anchor(lua, width, anchor.end),
        }
    }

    fn loaded_block_node_before_or_at_virtual_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        matches: impl FnMut(&BlockHistory, BlockId) -> bool,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        let (loaded_start, loaded_end) = self.active_virtual_row_span(lua, width)?;
        if row < loaded_start || loaded_start >= loaded_end {
            return None;
        }
        let local_row = row
            .min(loaded_end.saturating_sub(1))
            .saturating_sub(loaded_start);
        self.projection
            .block_node_before_or_at_row(
                lua,
                &mut self.transcript.history,
                width,
                local_row,
                matches,
            )
            .map(|node| self.offset_node_row(width, node))
    }

    #[cfg(test)]
    pub(crate) fn block_node_before_or_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        matches: impl FnMut(&BlockHistory, BlockId) -> bool,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        let local_row = self.exact_loaded_row_for_virtual_content_row(lua, width, row)?;
        self.projection
            .block_node_before_or_at_row(
                lua,
                &mut self.transcript.history,
                width,
                local_row,
                matches,
            )
            .map(|node| self.offset_node_row(width, node))
    }

    pub(crate) fn fold_node_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        action: crate::content::transcript_buf::FoldAction,
        activation: crate::content::transcript_buf::FoldActivation,
    ) -> bool {
        let Some(local_row) = self.exact_loaded_row_for_virtual_content_row(lua, width, row) else {
            return false;
        };
        let changed = self.projection.fold_node_at_row(
            lua,
            &mut self.transcript.history,
            width,
            crate::content::transcript_buf::FoldAtRow {
                row: local_row,
                action,
                activation,
            },
        );
        if changed {
            self.extent_index.clear_exact_descriptor_rows();
        }
        changed
    }

    pub(crate) fn prepare_layout(&mut self, lua: &LuaRuntime, width: u16) {
        self.projection
            .prepare_layout(lua, &mut self.transcript.history, width);
    }

    pub(crate) fn fold_node(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        id: crate::content::render_plan::RenderNodeId,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.prepare_layout(lua, width);
        let changed = self
            .projection
            .fold_node(&self.transcript.history, id, action);
        if changed {
            self.extent_index.clear_exact_descriptor_rows();
        }
        changed
    }

    pub(crate) fn fold_all(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.prepare_layout(lua, width);
        let changed = self.projection.fold_all(&self.transcript.history, action);
        if changed {
            self.extent_index.clear_exact_descriptor_rows();
        }
        changed
    }

    pub(crate) fn fold_block_kind(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        kind: &str,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.prepare_layout(lua, width);
        let changed = self
            .projection
            .fold_block_kind(&self.transcript.history, kind, action);
        if changed {
            self.extent_index.clear_exact_descriptor_rows();
        }
        changed
    }

    pub(crate) fn copy_exact_loaded_range(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        range: crate::smelt_edit::DocRange,
    ) -> crate::smelt_edit::CopyOutput {
        let row_offset = self.approximate_sparse_prefix_row_offset(width);
        if range.end.row < row_offset || (range.end.row == row_offset && range.end.byte_col == 0) {
            return crate::smelt_edit::CopyOutput::default();
        }
        let loaded_rows =
            self.projection
                .estimated_total_rows(lua, &mut self.transcript.history, width);
        let mut local_range = range;
        local_range.start.row = local_range.start.row.saturating_sub(row_offset);
        local_range.end.row = local_range
            .end
            .row
            .saturating_sub(row_offset)
            .min(loaded_rows);
        if local_range.start.row > local_range.end.row
            || (local_range.start.row == local_range.end.row
                && local_range.start.byte_col >= local_range.end.byte_col)
        {
            return crate::smelt_edit::CopyOutput::default();
        }
        self.projection
            .copy_range(lua, &mut self.transcript.history, width, theme, local_range)
    }

    pub(crate) fn descriptor_save_suffix(
        &self,
        descriptors_persisted: bool,
        dirty_history_from: Option<usize>,
    ) -> TranscriptDescriptorSaveSuffix {
        let history = self.history();
        let descriptor_order_dirty = if descriptors_persisted {
            history.descriptor_dirty_from().or_else(|| {
                dirty_history_from
                    .and_then(|idx| history.first_block_index_for_history_origin_at_or_after(idx))
            })
        } else {
            Some(0)
        };
        let Some(descriptor_order_start) = descriptor_order_dirty else {
            return TranscriptDescriptorSaveSuffix::Unchanged;
        };
        if descriptors_persisted
            && self.dirty_history_precedes_active_descriptor_window(dirty_history_from)
        {
            return TranscriptDescriptorSaveSuffix::NeedsFullRebuild;
        }
        let local_descriptor_start_idx = if descriptors_persisted {
            history.descriptor_record_index_for_order_index(descriptor_order_start)
        } else {
            0
        };
        let descriptor_start_idx = if descriptors_persisted {
            self.active_descriptor_range
                .as_ref()
                .map(|range| range.start.get().saturating_add(local_descriptor_start_idx))
                .unwrap_or(local_descriptor_start_idx)
        } else {
            0
        };
        TranscriptDescriptorSaveSuffix::Suffix {
            descriptor_start_idx,
            descriptor_records: history.descriptor_records_from(descriptor_order_start),
        }
    }

    fn dirty_history_precedes_active_descriptor_window(
        &self,
        dirty_history_from: Option<usize>,
    ) -> bool {
        let Some(dirty_history_from) = dirty_history_from else {
            return false;
        };
        let Some(active_range) = self.active_descriptor_range.as_ref() else {
            return false;
        };
        if active_range.start.get() == 0 {
            return false;
        }
        let first_loaded_history_origin = self
            .history()
            .descriptor_records_from(0)
            .into_iter()
            .find_map(|record| match record.origin {
                Some(smelt_core::BlockOrigin::History(index)) => Some(index),
                _ => None,
            });
        first_loaded_history_origin.is_none_or(|origin| origin > dirty_history_from)
    }

    pub(crate) fn history(&self) -> &BlockHistory {
        &self.transcript.history
    }

    pub(crate) fn history_mut(&mut self) -> &mut BlockHistory {
        self.extent_index.clear_exact_descriptor_rows();
        &mut self.transcript.history
    }

    pub(crate) fn projection_generation(&self) -> u64 {
        self.projection.projection_generation()
    }

    pub(crate) fn invalidate_renderer_if_changed(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) -> bool {
        let changed = self
            .projection
            .invalidate_renderer_if_changed(generation, cache_key);
        if changed {
            self.extent_index.clear_exact_descriptor_rows();
        }
        changed
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.transcript.history.is_empty()
            && self
                .sparse_descriptors
                .total_count()
                .is_none_or(|total_count| total_count == 0)
    }

    pub(crate) fn descriptor_total_count(&self) -> Option<usize> {
        self.sparse_descriptors.total_count()
    }

    pub(crate) fn note_persisted_descriptor_append(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let total = self
            .sparse_descriptors
            .total_count
            .unwrap_or_else(|| self.transcript.history.descriptor_records().len());
        if let Some(active) = self.active_descriptor_range.as_mut() {
            if active.end.get() == total {
                active.end =
                    smelt_store::TranscriptDescriptorIndex::new(total.saturating_add(count));
            }
        }
        self.sparse_descriptors.total_count = Some(total.saturating_add(count));
    }

    #[cfg(test)]
    fn loaded_descriptor_count(&self) -> usize {
        self.sparse_descriptors.loaded_descriptor_count()
    }

    #[cfg(test)]
    fn loaded_descriptor_ranges(&self) -> &[Range<smelt_store::TranscriptDescriptorIndex>] {
        self.sparse_descriptors.loaded_ranges()
    }

    #[cfg(test)]
    fn descriptor_range_state(&self, range: Range<usize>) -> DescriptorRangeState {
        let Some(total_count) = self.sparse_descriptors.total_count() else {
            return DescriptorRangeState::Unavailable;
        };
        let start = range.start.min(total_count);
        let end = range.end.min(total_count);
        if start >= end {
            return DescriptorRangeState::Loaded;
        }
        let start = smelt_store::TranscriptDescriptorIndex::new(start);
        let end = smelt_store::TranscriptDescriptorIndex::new(end);
        if self
            .sparse_descriptors
            .loaded_ranges()
            .iter()
            .any(|loaded| loaded.start <= start && loaded.end >= end)
        {
            DescriptorRangeState::Loaded
        } else {
            DescriptorRangeState::Missing
        }
    }

    pub(crate) fn push(&mut self, block: Block) {
        self.transcript.push(block);
        self.extent_index.clear_exact_descriptor_rows();
    }

    pub(crate) fn push_with_origin(
        &mut self,
        block: Block,
        origin: smelt_core::transcript_model::BlockOrigin,
    ) {
        self.transcript.push_with_origin(block, origin);
        self.extent_index.clear_exact_descriptor_rows();
    }

    pub(crate) fn set_compaction_preview(&mut self, summary: String) -> Option<BlockId> {
        let summary = summary.trim().to_string();
        if summary.is_empty() {
            return self.clear_compaction_preview();
        }

        if let Some(id) = self.compaction_preview_id {
            if self.transcript.block(id).is_some() {
                self.transcript.rewrite_compaction_preview(id, summary);
                self.extent_index.clear_exact_descriptor_rows();
                return Some(id);
            }
            self.compaction_preview_id = None;
        }

        let id = self.transcript.push_compaction_preview(summary)?;
        self.compaction_preview_id = Some(id);
        self.extent_index.clear_exact_descriptor_rows();
        Some(id)
    }

    pub(crate) fn clear_compaction_preview(&mut self) -> Option<BlockId> {
        let id = self.compaction_preview_id.take()?;
        self.transcript.remove_compaction_preview(id);
        self.extent_index.clear_exact_descriptor_rows();
        Some(id)
    }

    pub(crate) fn compaction_preview_id(&self) -> Option<BlockId> {
        self.compaction_preview_id
    }

    pub(crate) fn insert_checkpoint_marker_at(&mut self, block_index: usize, block: Block) {
        self.transcript
            .insert_checkpoint_marker_at(block_index, block);
        self.extent_index.clear_exact_descriptor_rows();
    }

    pub(crate) fn remove_unoriginated_at(&mut self, block_index: usize) -> Option<Block> {
        let removed = self.transcript.remove_unoriginated_at(block_index);
        if removed.is_some() {
            self.extent_index.clear_exact_descriptor_rows();
        }
        removed
    }

    pub(crate) fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        let drained = self.transcript.drain_finished_blocks();
        if !drained.is_empty() {
            self.extent_index.clear_exact_descriptor_rows();
        }
        drained
    }

    pub(crate) fn user_turns(&self) -> Vec<(usize, String)> {
        self.transcript.user_turns()
    }

    pub(crate) fn truncate_to(&mut self, block_idx: usize) {
        self.transcript.truncate_to(block_idx);
    }
}

impl Default for TranscriptDocument {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod document_tests {
    use super::*;
    use crate::app::transcript_scroll_trace::{
        TranscriptDescriptorTraceRange, TranscriptProjectionTargetTrace, TranscriptScrollIntent,
        TranscriptScrollTraceRenderInput, TranscriptTraceAnchor,
    };
    use smelt_core::transcript_model::TranscriptBlockRecord;

    #[test]
    fn sparse_tail_document_reports_virtual_prefix_rows() {
        let lua = LuaRuntime::new();
        let mut source = Transcript::new();
        source.push(Block::Text {
            content: "tail one".into(),
        });
        source.push(Block::Text {
            content: "tail two".into(),
        });
        let records = source.history.descriptor_records();
        let window_records = descriptor_records_with_ids(&records, 8);
        let loaded = LoadedTranscript {
            transcript: Transcript::new(),
            descriptor_window: Some(LoadedDescriptorWindow {
                start: smelt_store::TranscriptDescriptorIndex::new(8),
                total_count: 10,
                hydration: smelt_store::TranscriptDescriptorHydration::Hydrated,
                records: window_records,
            }),
            session_dir: None,
        };
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let loaded_rows =
            document
                .projection
                .estimated_total_rows(&lua, &mut document.transcript.history, 80);

        let total_rows = document.approximate_scrollbar_total_rows(&lua, 80);

        assert!(total_rows > loaded_rows);
        assert!(document.approximate_sparse_prefix_row_offset(80) > 0);
    }

    #[test]
    fn sparse_tail_projection_offsets_materialized_rows() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let mut source = Transcript::new();
        source.push(Block::Text {
            content: "tail one".into(),
        });
        source.push(Block::Text {
            content: "tail two".into(),
        });
        let records = source.history.descriptor_records();
        let window_records = descriptor_records_with_ids(&records, 8);
        let loaded = LoadedTranscript {
            transcript: Transcript::new(),
            descriptor_window: Some(LoadedDescriptorWindow {
                start: smelt_store::TranscriptDescriptorIndex::new(8),
                total_count: 10,
                hydration: smelt_store::TranscriptDescriptorHydration::Hydrated,
                records: window_records,
            }),
            session_dir: None,
        };
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let offset = document.approximate_sparse_prefix_row_offset(80);
        let plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_tail(),
            10,
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(401), Default::default());

        let rows = document.project_planned(&lua, &mut buf, &theme, plan);

        assert!(rows.total_rows > rows.materialized_rows);
        assert!(rows.row_base >= offset);
        assert!(rows.clamped_scroll >= offset);
        assert!(buf.lines().iter().any(|line| line == "tail two"));
    }

    #[test]
    fn transcript_scroll_trace_records_projection_contract_fields() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let mut source = Transcript::new();
        source.push(Block::Text {
            content: "trace head".into(),
        });
        source.push(Block::Text {
            content: "trace tail".into(),
        });
        let records = source.history.descriptor_records();
        let window_records = descriptor_records_with_ids(&records, 0);
        let loaded = LoadedTranscript {
            transcript: Transcript::new(),
            descriptor_window: Some(LoadedDescriptorWindow {
                start: smelt_store::TranscriptDescriptorIndex::new(0),
                total_count: 2,
                hydration: smelt_store::TranscriptDescriptorHydration::Hydrated,
                records: window_records,
            }),
            session_dir: None,
        };
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        document.set_scroll_trace_enabled(true);
        document.set_next_scroll_trace_input(TranscriptScrollTraceRenderInput {
            input_event_or_tick: "unit_wheel_tick".to_string(),
            scroll_intent: TranscriptScrollIntent::UserDelta { rows: -3 },
            window_scroll_before: 12,
            window_scroll_after_input: 9,
        });
        let plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_tail(),
            10,
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(409), Default::default());
        let rows = document.project_planned(&lua, &mut buf, &theme, plan);

        let frames = document.take_scroll_trace_frames();
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame.input_event_or_tick, "unit_wheel_tick");
        assert_eq!(
            frame.scroll_intent,
            TranscriptScrollIntent::UserDelta { rows: -3 }
        );
        assert_eq!(frame.window_scroll_before, 12);
        assert_eq!(frame.window_scroll_after_input, 9);
        assert_eq!(
            frame.projection_target,
            TranscriptProjectionTargetTrace::Tail
        );
        assert_eq!(frame.resolved_scroll_top, rows.clamped_scroll);
        assert_eq!(frame.materialized_range, rows.materialized_range().into());
        assert_eq!(
            frame.active_descriptor_range_after,
            Some(TranscriptDescriptorTraceRange { start: 0, end: 2 })
        );
        assert!(!frame.placeholder_rows_visible);
        assert!(matches!(
            frame.viewport_anchor_after,
            Some(TranscriptTraceAnchor::Tail)
        ));
        assert!(frame.first_visible_content_anchor.is_some());
        assert!(!frame.visible_record_or_block_ids.is_empty());
        assert_eq!(frame.render_or_projection_ms, None);
    }

    #[test]
    fn transcript_viewport_state_preserves_anchor_without_window_scroll_change() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 5;
        let mut document = TranscriptDocument::from_transcript(fixed_transcript(40));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(411), Default::default());

        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(8),
            viewport_rows,
        );
        let first = document.project_planned(&lua, &mut buf, &theme, plan);
        assert_eq!(first.clamped_scroll, 8);

        document.set_scroll_trace_enabled(true);
        document.take_scroll_trace_frames();
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: first.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let applied = document.project_applied_viewport(&lua, &mut buf, &theme, plan);

        assert_eq!(
            applied.materialized_rows.clamped_scroll,
            first.clamped_scroll
        );
        assert_eq!(applied.exact_visible_range.start, first.clamped_scroll);
        let frames = document.take_scroll_trace_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].scroll_intent,
            TranscriptScrollIntent::PreserveViewport
        );
        assert_eq!(
            frames[0].projection_target,
            TranscriptProjectionTargetTrace::ReflowStableRow(first.clamped_scroll)
        );
    }

    #[test]
    fn transcript_viewport_state_coalesces_pending_user_deltas() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 5;
        let mut document = TranscriptDocument::from_transcript(fixed_transcript(40));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(412), Default::default());

        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(20),
            viewport_rows,
        );
        let first = document.project_planned(&lua, &mut buf, &theme, plan);
        assert_eq!(first.clamped_scroll, 20);

        document.set_scroll_trace_enabled(true);
        document.take_scroll_trace_frames();
        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: -1 });
        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: -2 });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: first.clamped_scroll.saturating_sub(3),
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let applied = document.project_applied_viewport(&lua, &mut buf, &theme, plan);

        assert_eq!(applied.materialized_rows.clamped_scroll, 17);
        let frames = document.take_scroll_trace_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].scroll_intent,
            TranscriptScrollIntent::UserDelta { rows: -3 }
        );
        assert_eq!(
            frames[0].projection_target,
            TranscriptProjectionTargetTrace::ExactRow(17)
        );
    }

    #[test]
    fn local_delta_without_adjacent_descriptors_stays_on_exact_loaded_content() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let records = transcript_records(80);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            60..80,
            None,
        ));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(413), Default::default());

        let tail_plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: 0,
                follow_tail: true,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let tail = document.project_applied_viewport(&lua, &mut buf, &theme, tail_plan);
        assert!(!tail.placeholder_rows_visible);

        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: -10_000 });
        let local_plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: tail.materialized_rows.clamped_scroll.saturating_sub(10_000),
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let local = document.project_applied_viewport(&lua, &mut buf, &theme, local_plan);
        assert!(
            !local.placeholder_rows_visible,
            "local deltas must stay on exact loaded content instead of sparse placeholders"
        );
        assert!(local.top_anchor.is_some());

        document.set_pending_scroll_intent(TranscriptScrollIntent::ScrollbarFraction {
            numerator: 0,
            denominator: 1,
        });
        let far_plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: local.materialized_rows.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let far = document.project_applied_viewport(&lua, &mut buf, &theme, far_plan);
        assert!(
            far.placeholder_rows_visible,
            "far seek intents may expose inert sparse placeholders"
        );
    }

    #[test]
    fn local_delta_crossing_sparse_boundary_loads_adjacent_window() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let records = transcript_records(100);
        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            60..80,
            Some(dir.path().to_path_buf()),
        ));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(414), Default::default());
        let loaded_start = document.approximate_sparse_prefix_row_offset(width);
        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(loaded_start),
            viewport_rows,
        );
        let first = document.project_applied_viewport(&lua, &mut buf, &theme, plan);
        assert!(!first.placeholder_rows_visible);
        assert!(active_history_contains(&document, "block 60"));

        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: -4 });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: first.materialized_rows.clamped_scroll.saturating_sub(4),
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let local = document.project_applied_viewport(&lua, &mut buf, &theme, plan);

        assert!(
            !local.placeholder_rows_visible,
            "local deltas must load adjacent content instead of exposing sparse placeholders"
        );
        let active = document
            .active_descriptor_range
            .as_ref()
            .expect("active adjacent descriptor window");
        assert_eq!(active.end.get(), 80);
        assert!(active.start.get() < 60);
        assert!(active_history_contains(&document, "block 59"));
        assert!(active_history_contains(&document, "block 60"));
        assert!(matches!(
            document.viewport_state.top_anchor,
            Some(TranscriptScrollAnchor::Content(_))
        ));
        assert!(local.materialized_rows.clamped_scroll < first.materialized_rows.clamped_scroll);
    }

    #[test]
    fn local_delta_crossing_sparse_boundary_loads_next_adjacent_window() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let records = transcript_records(100);
        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            20..40,
            Some(dir.path().to_path_buf()),
        ));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(415), Default::default());
        let (_, loaded_end) = document
            .active_virtual_row_span(&lua, width)
            .expect("active row span");
        let top = loaded_end.saturating_sub(RowIndex::from(viewport_rows));
        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(top),
            viewport_rows,
        );
        let first = document.project_applied_viewport(&lua, &mut buf, &theme, plan);
        assert!(!first.placeholder_rows_visible);
        assert!(active_history_contains(&document, "block 39"));

        document.set_pending_scroll_intent(TranscriptScrollIntent::UserDelta { rows: 4 });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: first.materialized_rows.clamped_scroll.saturating_add(4),
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let local = document.project_applied_viewport(&lua, &mut buf, &theme, plan);

        assert!(
            !local.placeholder_rows_visible,
            "local downward deltas must load adjacent content instead of sparse placeholders"
        );
        let active = document
            .active_descriptor_range
            .as_ref()
            .expect("active adjacent descriptor window");
        assert_eq!(active.start.get(), 20);
        assert!(active.end.get() > 40);
        assert!(active_history_contains(&document, "block 39"));
        assert!(active_history_contains(&document, "block 40"));
        assert!(matches!(
            document.viewport_state.top_anchor,
            Some(TranscriptScrollAnchor::Content(_))
        ));
        assert!(local.materialized_rows.clamped_scroll > first.materialized_rows.clamped_scroll);
    }

    #[test]
    fn transcript_scroll_trace_records_injected_exact_observation_count() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let records = transcript_records(4);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..4,
            None,
        ));
        document.extent_index.observe_exact_loaded_descriptor_rows(
            &document.sparse_descriptors,
            exact_height_snapshot(BlockId::new(1), width, records[1].content_hash, 17),
        );
        document.set_scroll_trace_enabled(true);
        document.set_next_scroll_trace_input(TranscriptScrollTraceRenderInput {
            input_event_or_tick: "exact_height_refinement_probe".to_string(),
            scroll_intent: TranscriptScrollIntent::PreserveViewport,
            window_scroll_before: 0,
            window_scroll_after_input: 0,
        });
        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(0),
            10,
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(410), Default::default());
        let _rows = document.project_planned(&lua, &mut buf, &theme, plan);

        let frames = document.take_scroll_trace_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].exact_observation_count, 1);
    }

    #[test]
    fn sparse_projection_preserves_scroll_in_unloaded_suffix_gap() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            40..48,
            None,
        ));
        let offset = document.approximate_sparse_prefix_row_offset(80);
        let loaded_rows =
            document
                .projection
                .estimated_total_rows(&lua, &mut document.transcript.history, 80);
        let target = offset.saturating_add(loaded_rows).saturating_add(10);
        let plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(target),
            10,
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(402), Default::default());

        let rows = document.project_planned(&lua, &mut buf, &theme, plan);

        assert_eq!(rows.clamped_scroll, target);
        assert!(rows.row_base <= target);
        assert!(rows.row_base >= offset.saturating_add(loaded_rows));
        assert!(rows.materialized_rows > 0);
        assert!(buf.lines().iter().all(|line| line.is_empty()));
    }

    #[test]
    fn unloaded_sparse_gaps_do_not_provide_content_or_hit_targets() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            40..48,
            None,
        ));
        let width = 80;
        let offset = document.approximate_sparse_prefix_row_offset(width);
        let gap_row = offset.saturating_sub(1);
        assert!(gap_row > 0);

        let display = document.exact_or_gap_display_rows_for_range(&lua, width, &theme, gap_row, 1);
        assert_eq!(display.rows.len(), 1);
        assert!(display.rows[0].text.is_empty());
        assert!(display.rows[0].actions.is_empty());

        let copy = document.copy_exact_loaded_range(
            &lua,
            width,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: gap_row,
                    byte_col: 0,
                },
                end: crate::smelt_edit::DocPosition {
                    row: gap_row.saturating_add(1),
                    byte_col: 0,
                },
            },
        );
        assert!(copy.is_empty());
        assert!(document
            .node_metadata_at_row(&lua, width, gap_row)
            .is_none());
        assert!(document.row_anchor_at_row(&lua, width, gap_row).is_none());
        assert!(document
            .block_node_before_or_at_row(&lua, width, gap_row, |_, _| true)
            .is_none());
        assert!(!document.fold_node_at_row(
            &lua,
            width,
            gap_row,
            crate::content::transcript_buf::FoldAction::Open,
            crate::content::transcript_buf::FoldActivation::AnyNodeRow,
        ));

        let action = {
            let mut display_document =
                TranscriptDisplayDocument::new(&mut document, &lua, width, &theme);
            DisplayDocument::action_at(
                &mut display_document,
                crate::smelt_edit::DocPosition {
                    row: gap_row,
                    byte_col: 0,
                },
            )
        };
        assert!(action.is_none());

        let loaded_rows =
            document
                .projection
                .estimated_total_rows(&lua, &mut document.transcript.history, width);
        let suffix_gap_row = offset.saturating_add(loaded_rows).saturating_add(1);
        let display =
            document.exact_or_gap_display_rows_for_range(&lua, width, &theme, suffix_gap_row, 1);
        assert!(display.rows.iter().all(|row| row.text.is_empty()));
        assert!(display.rows.iter().all(|row| row.actions.is_empty()));
        let copy = document.copy_exact_loaded_range(
            &lua,
            width,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: suffix_gap_row,
                    byte_col: 0,
                },
                end: crate::smelt_edit::DocPosition {
                    row: suffix_gap_row.saturating_add(1),
                    byte_col: 0,
                },
            },
        );
        assert!(copy.is_empty());
        assert!(document
            .node_metadata_at_row(&lua, width, suffix_gap_row)
            .is_none());
        assert!(document
            .row_anchor_at_row(&lua, width, suffix_gap_row)
            .is_none());
        assert!(document
            .block_node_before_or_at_row(&lua, width, suffix_gap_row, |_, _| true)
            .is_none());
        assert!(!document.fold_node_at_row(
            &lua,
            width,
            suffix_gap_row,
            crate::content::transcript_buf::FoldAction::Open,
            crate::content::transcript_buf::FoldActivation::AnyNodeRow,
        ));
        let action = {
            let mut display_document =
                TranscriptDisplayDocument::new(&mut document, &lua, width, &theme);
            DisplayDocument::action_at(
                &mut display_document,
                crate::smelt_edit::DocPosition {
                    row: suffix_gap_row,
                    byte_col: 0,
                },
            )
        };
        assert!(action.is_none());
    }

    #[test]
    fn large_sparse_prefix_uses_bounded_coarse_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = Transcript::new();
        for idx in 0..2048 {
            let content = if idx < 2000 {
                format!("block {idx} {}", "wide text ".repeat(40))
            } else {
                format!("block {idx}")
            };
            source.push(Block::Text { content });
        }
        let records = source.history.descriptor_records();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            2000..2040,
            Some(dir.path().to_path_buf()),
        ));
        let average_rows = document.approximate_average_descriptor_rows(20);

        let prefix_rows = document.approximate_sparse_prefix_row_offset(20);

        assert_eq!(prefix_rows, 2000 * average_rows);
    }

    #[test]
    fn viewport_content_anchor_survives_sparse_prefix_estimate_refinement() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            40..48,
            None,
        ));
        let width = 80;
        let viewport_rows = 10;
        let original_top = document.approximate_sparse_prefix_row_offset(width);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(408), Default::default());
        let plan = document.plan_projection_measured(
            &lua,
            width,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(original_top),
            viewport_rows,
        );
        let rows = document.project_planned(&lua, &mut buf, &theme, plan);
        assert_eq!(rows.clamped_scroll, original_top);
        assert!(buf.lines().iter().any(|line| line.contains("block 40")));

        let refined_top = original_top.saturating_add(25);
        document.extent_index.descriptor_rows_estimate_cache.insert(
            DescriptorRowsEstimateKey {
                width,
                start: 0,
                end: 40,
            },
            refined_top,
        );
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: rows.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let rows = document.project_planned(&lua, &mut buf, &theme, plan);

        assert_eq!(rows.clamped_scroll, refined_top);
        assert!(buf.lines().iter().any(|line| line.contains("block 40")));
    }

    #[test]
    fn transcript_action_at_materializes_exact_unrendered_span() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let url = "https://example.test/exact-action";
        let target_index = 120usize;
        let mut source = Transcript::new();
        for index in 0..240 {
            let content = if index == target_index {
                format!("target [link]({url})")
            } else {
                format!("block {index}")
            };
            source.push(Block::Text { content });
        }
        let mut document = TranscriptDocument::from_transcript(source);
        let (_id, target_row, target_rows) = document
            .materialize_exact_loaded_block_layout(&lua, 80)
            .into_iter()
            .find(|(id, _, _)| *id == BlockId::new(target_index as u64))
            .expect("target block row");
        let target_display_rows =
            document.exact_or_gap_display_rows_for_range(&lua, 80, &theme, target_row, target_rows);
        let (row_offset, byte_col) = target_display_rows
            .rows
            .iter()
            .enumerate()
            .find_map(|(offset, row)| {
                row.actions.first().map(|action| {
                    (
                        offset as crate::smelt_edit::RowIndex,
                        crate::smelt_edit::text::cell_to_byte(&row.text, action.cell_start),
                    )
                })
            })
            .expect("target action span");
        let pos = crate::smelt_edit::DocPosition {
            row: target_row.saturating_add(row_offset),
            byte_col,
        };

        document.projection.reset_counters();
        let action = {
            let mut display_document =
                TranscriptDisplayDocument::new(&mut document, &lua, 80, &theme);
            DisplayDocument::action_at(&mut display_document, pos)
        };
        let counters = document.projection.counters();

        assert_eq!(
            action,
            Some(smelt_core::buffer::SpanAction::OpenUrl(url.to_string()))
        );
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.max_range_materialized_blocks, 1);
        assert!(
            counters.max_range_materialized_rows <= target_rows.saturating_add(1) as usize,
            "action hit testing should render only the target block and its leading gap, counters: {counters:?}"
        );
    }

    fn fixed_transcript(count: usize) -> Transcript {
        let mut source = Transcript::new();
        for idx in 0..count {
            source.push(Block::Text {
                content: format!("block {idx}"),
            });
        }
        source
    }

    fn transcript_records(count: usize) -> Vec<TranscriptBlockRecord> {
        fixed_transcript(count).history.descriptor_records()
    }

    fn varied_transcript(count: usize) -> Transcript {
        let mut source = Transcript::new();
        for idx in 0..count {
            let repeats = match idx % 9 {
                0 => 1,
                1 | 2 => 4,
                3..=5 => 12,
                _ => 28,
            };
            source.push(Block::Text {
                content: format!("block {idx} {}", "wrapped text ".repeat(repeats)),
            });
        }
        source
    }

    fn varied_transcript_records(count: usize) -> Vec<TranscriptBlockRecord> {
        varied_transcript(count).history.descriptor_records()
    }

    fn descriptor_records_with_ids(
        records: &[TranscriptBlockRecord],
        block_id_start: usize,
    ) -> Vec<TranscriptBlockRecordWithId> {
        records
            .iter()
            .cloned()
            .enumerate()
            .map(|(offset, record)| TranscriptBlockRecordWithId {
                block_id: BlockId::new(block_id_start.saturating_add(offset) as u64),
                record,
            })
            .collect()
    }

    fn sparse_loaded_transcript(
        records: &[TranscriptBlockRecord],
        range: Range<usize>,
        session_dir: Option<PathBuf>,
    ) -> LoadedTranscript {
        let window_records = descriptor_records_with_ids(&records[range.clone()], range.start);
        LoadedTranscript {
            transcript: Transcript::new(),
            descriptor_window: Some(LoadedDescriptorWindow {
                start: smelt_store::TranscriptDescriptorIndex::new(range.start),
                total_count: records.len(),
                hydration: smelt_store::TranscriptDescriptorHydration::Hydrated,
                records: window_records,
            }),
            session_dir,
        }
    }

    fn exact_height_snapshot(
        block_id: BlockId,
        width: u16,
        content_hash: u64,
        rows: RowIndex,
    ) -> crate::content::transcript_buf::TranscriptExactHeightSnapshot {
        crate::content::transcript_buf::TranscriptExactHeightSnapshot {
            width,
            renderer_generation: 7,
            renderer_cache_key: Some(11),
            presentation_generation: 13,
            observations: vec![
                crate::content::transcript_buf::TranscriptExactHeightObservation {
                    block_id,
                    key: LayoutKey {
                        width,
                        view_state: smelt_core::transcript_model::ViewState::Expanded,
                        content_hash,
                        sidecar_hash: 0,
                    },
                    rows,
                },
            ],
        }
    }

    #[test]
    fn exact_loaded_descriptor_rows_override_text_estimates() {
        let records = transcript_records(3);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..3,
            None,
        ));
        let width = 80;
        let block_id = BlockId::new(1);
        let raw_estimate = document
            .extent_index
            .approximate_rows_for_loaded_descriptors(&document.sparse_descriptors, width);

        document.extent_index.observe_exact_loaded_descriptor_rows(
            &document.sparse_descriptors,
            exact_height_snapshot(block_id, width, records[1].content_hash, 17),
        );

        let refined = document
            .extent_index
            .approximate_rows_for_loaded_descriptors(&document.sparse_descriptors, width);
        assert_eq!(raw_estimate, 6);
        assert_eq!(refined, 21);
        assert_eq!(document.approximate_average_descriptor_rows(width), 7);
    }

    #[test]
    fn exact_loaded_descriptor_rows_are_width_and_invalidation_scoped() {
        let records = transcript_records(2);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            0..2,
            None,
        ));
        let width = 80;
        assert!(!document.invalidate_renderer_if_changed(7, Some(11)));
        let block_id = BlockId::new(0);
        document.extent_index.observe_exact_loaded_descriptor_rows(
            &document.sparse_descriptors,
            exact_height_snapshot(block_id, width, records[0].content_hash, 19),
        );

        assert_eq!(
            document.extent_index.exact_rows_for_descriptor(0, width),
            Some(19)
        );
        assert_eq!(
            document
                .extent_index
                .exact_rows_for_descriptor(0, width - 1),
            None
        );
        document.extent_index.observe_exact_loaded_descriptor_rows(
            &document.sparse_descriptors,
            exact_height_snapshot(block_id, width - 1, records[0].content_hash, 23),
        );
        assert_eq!(
            document
                .extent_index
                .exact_rows_for_descriptor(0, width - 1),
            Some(23)
        );
        assert_eq!(
            document.extent_index.exact_rows_for_descriptor(0, width),
            Some(19)
        );

        assert!(document.invalidate_renderer_if_changed(8, Some(11)));
        assert_eq!(
            document.extent_index.exact_rows_for_descriptor(0, width),
            None
        );

        document.extent_index.observe_exact_loaded_descriptor_rows(
            &document.sparse_descriptors,
            exact_height_snapshot(block_id, width, records[0].content_hash, 19),
        );
        document.set_inline_options(InlineOptions::default());
        assert_eq!(
            document.extent_index.exact_rows_for_descriptor(0, width),
            None
        );
    }

    fn skewed_transcript_records(count: usize) -> Vec<TranscriptBlockRecord> {
        let mut source = Transcript::new();
        for idx in 0..count {
            let repeats = if idx < count / 2 { 80 } else { 1 };
            source.push(Block::Text {
                content: format!("block {idx} {}", "skewed text ".repeat(repeats)),
            });
        }
        source.history.descriptor_records()
    }

    #[test]
    fn resumed_sparse_prefix_rows_use_persisted_descriptor_estimates() {
        let width = 24;
        let records = skewed_transcript_records(300);
        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let expected = smelt_store::SessionDb::open_read_only(dir.path().join("session.db"))
            .unwrap()
            .transcript_descriptor_estimated_rows((0..220).into(), width)
            .unwrap() as RowIndex;

        let mut sparse = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            220..235,
            Some(dir.path().to_path_buf()),
        ));

        assert_eq!(sparse.approximate_sparse_prefix_row_offset(width), expected);
    }

    fn active_history_contains(document: &TranscriptDocument, needle: &str) -> bool {
        document
            .transcript
            .history
            .descriptor_records()
            .iter()
            .any(|record| {
                record
                    .descriptor
                    .raw_text()
                    .is_some_and(|text| text == needle)
            })
    }

    #[test]
    fn resumed_sparse_scroll_up_never_stalls_or_reverses() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 24;
        let viewport_rows = 12;
        let records = varied_transcript_records(400);
        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(dir.path().to_path_buf(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(403), Default::default());
        let mut rows = {
            let plan = document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                viewport_rows,
            );
            document.project_planned(&lua, &mut buf, &theme, plan)
        };

        for step in 0..600 {
            if rows.clamped_scroll == 0 {
                break;
            }
            let previous = rows.clamped_scroll;
            let requested = previous.saturating_sub(3);
            let plan = document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_row(requested),
                viewport_rows,
            );
            rows = document.project_planned(&lua, &mut buf, &theme, plan);

            assert!(
                rows.clamped_scroll < previous,
                "scroll step {step} stalled or reversed: previous={previous}, requested={requested}, resolved={}, row_base={}, materialized={}, total={}, active={:?}",
                rows.clamped_scroll,
                rows.row_base,
                rows.materialized_rows,
                rows.total_rows,
                document.active_descriptor_range,
            );
            assert!(
                rows.row_base <= rows.clamped_scroll,
                "scroll step {step} resolved before materialized base: {:?}",
                rows
            );
            assert!(
                rows.clamped_scroll
                    .saturating_add(RowIndex::from(viewport_rows))
                    <= rows.row_base.saturating_add(rows.materialized_rows),
                "scroll step {step} viewport is not materialized: {:?}",
                rows
            );
        }
    }

    #[test]
    fn resumed_sparse_skewed_scroll_up_materializes_requested_viewport() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 24;
        let viewport_rows = 12;
        let records = skewed_transcript_records(300);
        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(dir.path().to_path_buf(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(404), Default::default());
        let mut rows = {
            let plan = document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                viewport_rows,
            );
            document.project_planned(&lua, &mut buf, &theme, plan)
        };

        for step in 0..1000 {
            if rows.clamped_scroll == 0 {
                break;
            }
            let previous = rows.clamped_scroll;
            let requested = previous.saturating_sub(3);
            let plan = document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_row(requested),
                viewport_rows,
            );
            rows = document.project_planned(&lua, &mut buf, &theme, plan);

            assert_eq!(
                rows.clamped_scroll, requested,
                "scroll step {step} did not honor request: previous={previous}, rows={rows:?}, active={:?}",
                document.active_descriptor_range,
            );
            assert!(
                buf.lines().iter().any(|line| !line.is_empty()),
                "scroll step {step} materialized only sparse placeholders: requested={requested}, rows={rows:?}, active={:?}",
                document.active_descriptor_range,
            );
        }
    }

    struct ProjectionHarness<'a> {
        lua: &'a LuaRuntime,
        theme: &'a Theme,
        width: u16,
        viewport_rows: u16,
    }

    fn project_frame_into_window(
        document: &mut TranscriptDocument,
        win: &mut crate::smelt_edit::Window,
        buf: &mut Buffer,
        ctx: ProjectionHarness<'_>,
        follow_tail: bool,
    ) -> crate::smelt_edit::MaterializedRows {
        let cursor_screen_row = win.cursor_screen_row(ctx.viewport_rows);
        let scroll_target = if follow_tail {
            crate::content::transcript_buf::ScrollTarget::visible_tail()
        } else {
            crate::content::transcript_buf::ScrollTarget::visible_row(win.scroll_top())
        };
        let plan = document.plan_projection_measured(
            ctx.lua,
            ctx.width,
            ctx.theme,
            scroll_target,
            ctx.viewport_rows,
        );
        let rows = document.project_planned(ctx.lua, buf, ctx.theme, plan);
        win.apply_materialized_rows(rows);
        win.set_resolved_scroll(rows.clamped_scroll);
        win.ensure_layout(buf, ctx.width);
        win.refresh_document_view_position_from_buffer(buf);
        if let Some(screen_row) = cursor_screen_row {
            win.restore_cursor_screen_row(buf, screen_row);
        }
        win.sync_row_render_state(buf, ctx.viewport_rows, std::time::Instant::now());
        rows
    }

    fn transcript_window() -> crate::smelt_edit::Window {
        crate::smelt_edit::Window::new(
            crate::smelt_edit::WinId(1),
            crate::smelt_edit::BufId(1),
            crate::smelt_edit::SplitConfig {
                region: "transcript".into(),
                gutters: smelt_term::layout::Gutters::default(),
            },
        )
    }

    #[test]
    fn resumed_sparse_window_wheel_scroll_up_keeps_moving() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 24;
        let viewport_rows = 12;
        let records = varied_transcript_records(400);
        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(dir.path().to_path_buf(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut win = transcript_window();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
        let ctx = || ProjectionHarness {
            lua: &lua,
            theme: &theme,
            width,
            viewport_rows,
        };
        project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), true);

        for step in 0..200 {
            if win.scroll_top() == 0 {
                break;
            }
            let previous = win.scroll_top();
            win.pan_by_lines(&buf, -3, viewport_rows);
            let requested = win.scroll_top();
            let rows = project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
            assert!(
                win.scroll_top() < previous,
                "wheel step {step} stalled or reversed: previous={previous}, requested={requested}, resolved={}, rows={rows:?}, active={:?}",
                win.scroll_top(),
                document.active_descriptor_range,
            );
            assert!(
                win.scroll_top() <= requested,
                "wheel step {step} projection reversed past the requested scroll: requested={requested}, resolved={}",
                win.scroll_top()
            );
            assert!(
                requested.saturating_sub(win.scroll_top()) <= RowIndex::from(viewport_rows),
                "wheel step {step} projection jumped too far above the requested scroll: requested={requested}, resolved={}",
                win.scroll_top()
            );
        }
    }

    #[test]
    fn resumed_sparse_window_wheel_scroll_down_is_monotonic_and_does_not_snap_back() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 24;
        let viewport_rows = 12;
        let records = skewed_transcript_records(300);
        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(dir.path().to_path_buf(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut win = transcript_window();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(2), Default::default());
        let ctx = || ProjectionHarness {
            lua: &lua,
            theme: &theme,
            width,
            viewport_rows,
        };
        project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), true);
        win.pin_scroll(0);
        project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);

        let mut steps = 0;
        for step in 0..300 {
            let previous = win.scroll_top();
            win.pan_by_lines(&buf, 3, viewport_rows);
            let requested = win.scroll_top();
            if requested == previous {
                break;
            }
            let rows = project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
            assert!(
                win.scroll_top() > previous,
                "wheel step {step} reversed or stalled: previous={previous}, requested={requested}, resolved={}, rows={rows:?}, active={:?}",
                win.scroll_top(),
                document.active_descriptor_range,
            );
            assert_eq!(
                win.scroll_top(),
                requested,
                "wheel step {step} projection did not honor the requested scroll"
            );
            steps += 1;
        }
        assert!(steps > 0, "scroll-down scenario did not move");

        let settled = win.scroll_top();
        let rows = project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
        assert_eq!(
            win.scroll_top(),
            settled,
            "idle projection snapped after scroll-down settled: rows={rows:?}, active={:?}",
            document.active_descriptor_range,
        );
    }

    #[test]
    fn resumed_sparse_drag_select_autoscroll_up_keeps_moving() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 24;
        let viewport_rows = 12;
        let records = varied_transcript_records(400);
        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(dir.path().to_path_buf(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut win = transcript_window();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
        let ctx = || ProjectionHarness {
            lua: &lua,
            theme: &theme,
            width,
            viewport_rows,
        };
        project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), true);
        let top = win.scroll_top();
        let mut state = win.document_view_state();
        state.cursor = crate::smelt_edit::DocPosition {
            row: top,
            byte_col: 0,
        };
        state.drag_endpoint = Some(state.cursor);
        state.selection_anchor = Some(crate::smelt_edit::DocPosition {
            row: top.saturating_add(RowIndex::from(viewport_rows.saturating_sub(1))),
            byte_col: 0,
        });
        state.preferred_cell_col = Some(0);
        win.set_document_view_state(state);

        for step in 0..200 {
            if win.scroll_top() == 0 {
                break;
            }
            let previous = win.scroll_top();
            assert!(
                win.drag_autoscroll_step(&buf, viewport_rows, -1),
                "drag step {step} did not move before projection"
            );
            let requested = win.scroll_top();
            let rows = project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
            let state = win.document_view_state();
            assert!(
                win.scroll_top() < previous,
                "drag step {step} stalled or reversed: previous={previous}, requested={requested}, resolved={}, rows={rows:?}, active={:?}, state={state:?}",
                win.scroll_top(),
                document.active_descriptor_range,
            );
            assert_eq!(
                win.scroll_top(),
                requested,
                "drag step {step} projection did not honor autoscroll request"
            );
            assert_eq!(
                state.drag_endpoint.map(|pos| pos.row),
                Some(win.scroll_top()),
                "drag step {step} endpoint did not stay parked at the top edge"
            );
        }
    }

    #[test]
    fn resumed_sparse_scrollbar_total_matches_full_transcript_truth_for_fixed_rows() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 32;
        let viewport_rows = 12;
        let source = fixed_transcript(240);
        let records = source.history.descriptor_records();
        let mut full_document = TranscriptDocument::from_transcript(source);
        let mut full_buf = Buffer::new(crate::smelt_edit::BufId(2), Default::default());
        let full_rows = {
            let plan = full_document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                viewport_rows,
            );
            full_document.project_planned(&lua, &mut full_buf, &theme, plan)
        };

        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(dir.path().to_path_buf(), width, viewport_rows)
                .expect("tail transcript");
        let mut sparse_document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut sparse_buf = Buffer::new(crate::smelt_edit::BufId(3), Default::default());
        let sparse_rows = {
            let plan = sparse_document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                viewport_rows,
            );
            sparse_document.project_planned(&lua, &mut sparse_buf, &theme, plan)
        };

        assert_eq!(sparse_rows.total_rows, full_rows.total_rows);
        let bar = crate::smelt_edit::ScrollbarState::new(
            width.saturating_sub(1),
            sparse_rows.total_rows,
            viewport_rows,
        )
        .expect("scrollbar");
        assert_eq!(bar.total_rows, full_rows.total_rows);
        assert_eq!(bar.viewport_rows, viewport_rows);
    }

    #[test]
    fn resumed_sparse_scrollbar_click_and_drag_requests_do_not_snap_back() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 32;
        let viewport_rows = 12;
        let source = fixed_transcript(240);
        let records = source.history.descriptor_records();
        let mut full_document = TranscriptDocument::from_transcript(source);
        let mut full_buf = Buffer::new(crate::smelt_edit::BufId(2), Default::default());
        let full_rows = {
            let plan = full_document.plan_projection_measured(
                &lua,
                width,
                &theme,
                crate::content::transcript_buf::ScrollTarget::visible_tail(),
                viewport_rows,
            );
            full_document.project_planned(&lua, &mut full_buf, &theme, plan)
        };

        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let loaded =
            LoadedTranscript::tail_from_sqlite_dir(dir.path().to_path_buf(), width, viewport_rows)
                .expect("tail transcript");
        let mut document = TranscriptDocument::from_loaded_transcript(loaded);
        let mut win = transcript_window();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());
        let ctx = || ProjectionHarness {
            lua: &lua,
            theme: &theme,
            width,
            viewport_rows,
        };
        let sparse_rows = project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), true);
        assert_eq!(sparse_rows.total_rows, full_rows.total_rows);

        let full_bar = crate::smelt_edit::ScrollbarState::new(
            width.saturating_sub(1),
            full_rows.total_rows,
            viewport_rows,
        )
        .expect("full scrollbar");
        let sparse_bar = crate::smelt_edit::ScrollbarState::new(
            width.saturating_sub(1),
            sparse_rows.total_rows,
            viewport_rows,
        )
        .expect("sparse scrollbar");

        let mut previous = None;
        for rel_row in 0..viewport_rows {
            let request =
                sparse_bar.scroll_from_top_for_thumb(sparse_bar.thumb_top_for_click(rel_row));
            let full_request =
                full_bar.scroll_from_top_for_thumb(full_bar.thumb_top_for_click(rel_row));
            assert_eq!(
                request, full_request,
                "click row {rel_row} mapped differently"
            );

            win.scroll_to_preserving_cursor_screen_row(request, &buf, viewport_rows);
            let requested = win.scroll_top();
            project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
            assert_eq!(
                win.scroll_top(),
                requested,
                "click row {rel_row} snapped after projection"
            );
            if let Some(previous) = previous {
                assert!(
                    win.scroll_top() >= previous,
                    "click row {rel_row} moved backward: previous={previous}, current={}",
                    win.scroll_top()
                );
            }
            previous = Some(win.scroll_top());
        }

        for thumb_top in 0..=sparse_bar.max_thumb_top() {
            let request = sparse_bar.scroll_from_top_for_thumb(thumb_top);
            assert_eq!(
                request,
                full_bar.scroll_from_top_for_thumb(thumb_top),
                "thumb row {thumb_top} mapped differently"
            );
            win.scroll_to_preserving_cursor_screen_row(request, &buf, viewport_rows);
            let requested = win.scroll_top();
            project_frame_into_window(&mut document, &mut win, &mut buf, ctx(), false);
            assert_eq!(
                win.scroll_top(),
                requested,
                "thumb row {thumb_top} snapped after projection"
            );
            let resolved_thumb = sparse_bar.thumb_top_for_scroll(win.scroll_top());
            assert!(
                resolved_thumb.abs_diff(thumb_top) <= 1,
                "thumb row {thumb_top} resolved to {resolved_thumb} for scroll {}",
                win.scroll_top()
            );
        }
    }

    #[test]
    fn sparse_block_lookup_translates_virtual_rows_for_top_pill() {
        let lua = LuaRuntime::new();
        let mut source = Transcript::new();
        for idx in 0..100 {
            if idx % 10 == 0 {
                source.push(Block::User {
                    text: format!("user {idx}"),
                    image_labels: Vec::new(),
                });
            } else {
                source.push(Block::Text {
                    content: format!("assistant {idx}"),
                });
            }
        }
        let records = source.history.descriptor_records();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            40..48,
            None,
        ));
        let offset = document.approximate_sparse_prefix_row_offset(80);

        let snapshot = document
            .block_snapshot_before_or_at_row(&lua, 80, offset.saturating_add(1), Some("user"))
            .expect("loaded user block before virtual row");

        assert_eq!(snapshot.1, "user");
        assert_eq!(snapshot.4, "user 40");
        assert_eq!(snapshot.2, offset);
    }

    #[test]
    fn sparse_block_lookup_loads_window_for_requested_virtual_row() {
        let lua = LuaRuntime::new();
        let dir = tempfile::tempdir().unwrap();
        let mut source = Transcript::new();
        for idx in 0..100 {
            if idx % 10 == 0 {
                source.push(Block::User {
                    text: format!("user {idx}"),
                    image_labels: Vec::new(),
                });
            } else {
                source.push(Block::Text {
                    content: format!("assistant {idx}"),
                });
            }
        }
        let records = source.history.descriptor_records();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            Some(dir.path().to_path_buf()),
        ));
        assert!(!active_history_contains(&document, "user 40"));

        let snapshot = document
            .block_snapshot_before_or_at_row(&lua, 80, 81, Some("user"))
            .expect("user block loaded for requested row");

        assert_eq!(snapshot.1, "user");
        assert_eq!(snapshot.4, "user 40");
        assert!(active_history_contains(&document, "user 40"));
    }

    #[test]
    fn sparse_previous_navigation_block_preserves_active_descriptor_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = Transcript::new();
        for idx in 0..100 {
            if idx == 0 || idx == 50 || idx == 80 {
                source.push(Block::User {
                    text: format!("user {idx}"),
                    image_labels: Vec::new(),
                });
            } else {
                source.push(Block::Text {
                    content: format!("assistant {idx}"),
                });
            }
        }
        let records = source.history.descriptor_records();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            Some(dir.path().to_path_buf()),
        ));
        assert!(document.sparse_descriptors.merge(&LoadedDescriptorWindow {
            start: smelt_store::TranscriptDescriptorIndex::new(0),
            total_count: records.len(),
            hydration: smelt_store::TranscriptDescriptorHydration::Hydrated,
            records: descriptor_records_with_ids(&records[0..1], 0),
        }));
        let active_before = document.active_descriptor_range.clone();
        assert!(!active_history_contains(&document, "user 0"));
        assert!(!active_history_contains(&document, "user 50"));

        let previous = document
            .previous_navigation_block(Some("user"))
            .expect("previous user block outside the active window");

        assert_eq!(previous.descriptor_index, 50);
        assert_eq!(previous.block_id, BlockId::new(50));
        assert_eq!(previous.role, "user");
        assert_eq!(previous.first_line, "user 50");
        assert!(!previous.already_at_anchor);

        let next = document
            .next_navigation_block(Some("user"))
            .expect("next user block in the active window");

        assert_eq!(next.descriptor_index, 80);
        assert_eq!(next.block_id, BlockId::new(80));
        assert_eq!(next.role, "user");
        assert_eq!(next.first_line, "user 80");
        assert!(!next.already_at_anchor);
        assert_eq!(document.active_descriptor_range, active_before);
        assert!(!active_history_contains(&document, "user 0"));
        assert!(!active_history_contains(&document, "user 50"));
    }

    #[test]
    fn reveal_block_intent_places_target_at_requested_screen_row() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let descriptor_index = 50;
        let top_padding = 2;
        let dir = tempfile::tempdir().unwrap();
        let mut source = Transcript::new();
        for idx in 0..100 {
            if idx == descriptor_index {
                source.push(Block::User {
                    text: format!("user {idx}"),
                    image_labels: Vec::new(),
                });
            } else {
                source.push(Block::Text {
                    content: format!("assistant {idx}"),
                });
            }
        }
        let records = source.history.descriptor_records();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            80..100,
            Some(dir.path().to_path_buf()),
        ));
        let block_id = BlockId::new(descriptor_index as u64);

        document.set_pending_scroll_intent(TranscriptScrollIntent::RevealBlock {
            descriptor_index,
            block_id,
            row_offset: 0,
            screen_padding_top: top_padding,
        });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: 0,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(911), Default::default());
        let rows = document.project_planned(&lua, &mut buf, &theme, plan);
        let first_scroll = rows.clamped_scroll;
        let reveal = document
            .descriptor_block_reveal_position(
                &lua,
                width,
                descriptor_index,
                0,
                top_padding,
                viewport_rows,
            )
            .expect("revealed block position");
        assert_eq!(reveal.block_id, block_id);
        assert_eq!(
            reveal.target_row.saturating_sub(rows.clamped_scroll),
            top_padding
        );
        assert!(active_history_contains(&document, "user 50"));

        let active_start = document
            .active_descriptor_range
            .as_ref()
            .expect("active reveal window")
            .start
            .get();
        assert!(active_start > 0);
        document.extent_index.descriptor_rows_estimate_cache.insert(
            DescriptorRowsEstimateKey {
                width,
                start: 0,
                end: active_start,
            },
            first_scroll.saturating_add(123),
        );
        document.set_pending_scroll_intent(TranscriptScrollIntent::RevealBlock {
            descriptor_index,
            block_id,
            row_offset: 0,
            screen_padding_top: top_padding,
        });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: rows.clamped_scroll,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let rows = document.project_planned(&lua, &mut buf, &theme, plan);
        assert_ne!(rows.clamped_scroll, first_scroll);
        let reveal = document
            .descriptor_block_reveal_position(
                &lua,
                width,
                descriptor_index,
                0,
                top_padding,
                viewport_rows,
            )
            .expect("revealed block position after prefix refinement");
        assert_eq!(
            reveal.target_row.saturating_sub(rows.clamped_scroll),
            top_padding
        );
    }

    #[test]
    fn viewport_anchor_preserves_descriptor_block_across_window_replacement() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 6;
        let descriptor_index = 50;
        let dir = tempfile::tempdir().unwrap();
        let mut source = Transcript::new();
        for idx in 0..100 {
            if idx == descriptor_index {
                source.push(Block::User {
                    text: format!("user {idx}"),
                    image_labels: Vec::new(),
                });
            } else {
                source.push(Block::Text {
                    content: format!("assistant {idx}"),
                });
            }
        }
        let records = source.history.descriptor_records();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            80..100,
            Some(dir.path().to_path_buf()),
        ));
        let block_id = BlockId::new(descriptor_index as u64);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(912), Default::default());

        document.set_pending_scroll_intent(TranscriptScrollIntent::RevealBlock {
            descriptor_index,
            block_id,
            row_offset: 0,
            screen_padding_top: 0,
        });
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: 0,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let rows = document.project_planned(&lua, &mut buf, &theme, plan);
        let anchor = match document.viewport_state.top_anchor {
            Some(TranscriptScrollAnchor::Content(anchor)) => anchor,
            other => panic!("expected content viewport anchor, got {other:?}"),
        };
        assert_eq!(anchor.descriptor_index, descriptor_index);
        assert_eq!(anchor.block_id, block_id);
        assert_eq!(anchor.intra_block_row, anchor.row_anchor.row_offset);
        assert_eq!(anchor.bias, TranscriptAnchorBias::Top);
        assert_eq!(anchor.fallback_row, rows.clamped_scroll);
        let position_anchor = document.position_anchor(
            &lua,
            width,
            crate::smelt_edit::DocPosition {
                row: rows.clamped_scroll,
                byte_col: 0,
            },
        );
        let position_content_anchor = position_anchor
            .anchor
            .expect("position preservation should store a content anchor");
        assert_eq!(position_content_anchor.descriptor_index, descriptor_index);
        assert_eq!(position_content_anchor.block_id, block_id);

        assert!(document.activate_descriptor_window_range(
            smelt_store::TranscriptDescriptorIndex::new(80)
                ..smelt_store::TranscriptDescriptorIndex::new(100)
        ));
        assert!(!active_history_contains(&document, "user 50"));

        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: rows.clamped_scroll.saturating_add(999),
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let _rows = document.project_planned(&lua, &mut buf, &theme, plan);
        assert!(active_history_contains(&document, "user 50"));
        let anchor_after = match document.viewport_state.top_anchor {
            Some(TranscriptScrollAnchor::Content(anchor)) => anchor,
            other => panic!("expected preserved content viewport anchor, got {other:?}"),
        };
        assert_eq!(anchor_after.descriptor_index, descriptor_index);
        assert_eq!(anchor_after.block_id, block_id);
        assert_eq!(anchor_after.intra_block_row, anchor.intra_block_row);
    }

    #[test]
    fn sparse_block_lookup_searches_previous_windows_for_role_match() {
        let lua = LuaRuntime::new();
        let dir = tempfile::tempdir().unwrap();
        let mut source = Transcript::new();
        for idx in 0..100 {
            if idx == 0 || idx == 90 {
                source.push(Block::User {
                    text: format!("user {idx}"),
                    image_labels: Vec::new(),
                });
            } else {
                source.push(Block::Text {
                    content: format!("assistant {idx}"),
                });
            }
        }
        let records = source.history.descriptor_records();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            Some(dir.path().to_path_buf()),
        ));
        assert!(!active_history_contains(&document, "user 0"));

        let snapshot = document
            .block_snapshot_before_or_at_row(&lua, 80, 161, Some("user"))
            .expect("previous user block outside the initial lookup window");

        assert_eq!(snapshot.1, "user");
        assert_eq!(snapshot.4, "user 0");
        assert!(active_history_contains(&document, "user 0"));
    }

    #[test]
    fn sparse_active_window_keeps_loaded_ranges_separate() {
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            None,
        ));

        assert!(document.merge_descriptor_window(LoadedDescriptorWindow {
            start: smelt_store::TranscriptDescriptorIndex::new(40),
            total_count: records.len(),
            hydration: smelt_store::TranscriptDescriptorHydration::Hydrated,
            records: descriptor_records_with_ids(&records[40..48], 40),
        }));

        assert_eq!(document.sparse_descriptors.loaded_ranges().len(), 2);
        assert_eq!(
            document.active_descriptor_range,
            Some(
                smelt_store::TranscriptDescriptorIndex::new(40)
                    ..smelt_store::TranscriptDescriptorIndex::new(48)
            )
        );
        assert_eq!(document.transcript.history.descriptor_records().len(), 8);
        assert!(active_history_contains(&document, "block 40"));
        assert!(!active_history_contains(&document, "block 90"));
    }

    #[test]
    fn sparse_row_jump_loads_bounded_descriptor_window() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let dir = tempfile::tempdir().unwrap();
        let records = transcript_records(100);
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            Some(dir.path().to_path_buf()),
        ));

        let _plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(80),
            10,
        );
        let active = document.active_descriptor_range.clone().unwrap();

        assert!(active.start.get() <= 40);
        assert!(active.end.get() > 40);
        assert!(active.start.get() > 0);
        assert!(active.end.get() < records.len());
        assert_eq!(document.sparse_descriptors.loaded_ranges().len(), 2);
        assert!(active_history_contains(&document, "block 40"));
        assert!(!active_history_contains(&document, "block 90"));
    }

    #[test]
    fn sparse_nearby_scroll_reuses_active_descriptor_window() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            40..48,
            None,
        ));
        let active_before = document.active_descriptor_range.clone();

        let _plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_row(82),
            10,
        );

        assert_eq!(document.active_descriptor_range, active_before);
        assert_eq!(document.sparse_descriptors.loaded_ranges().len(), 1);
        assert!(active_history_contains(&document, "block 40"));
    }

    #[test]
    fn approximate_row_seek_reanchors_to_loaded_content() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let width = 80;
        let viewport_rows = 10;
        let records = transcript_records(100);
        let dir = tempfile::tempdir().unwrap();
        crate::persist::write_transcript_descriptor_suffix(dir.path(), 0, &records).unwrap();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            Some(dir.path().to_path_buf()),
        ));
        let mut buf = Buffer::new(crate::smelt_edit::BufId(416), Default::default());

        document.set_pending_scroll_intent(TranscriptScrollIntent::ApproximateRowSeek(80));
        let plan = document.plan_viewport_projection_measured(
            &lua,
            width,
            &theme,
            TranscriptViewportProjectionInput {
                fallback_scroll_top: 80,
                follow_tail: false,
                width_changed: false,
                previous_width: None,
            },
            viewport_rows,
        );
        let applied = document.project_applied_viewport(&lua, &mut buf, &theme, plan);

        assert!(!applied.placeholder_rows_visible);
        assert!(active_history_contains(&document, "block 40"));
        let anchor = match document.viewport_state.top_anchor {
            Some(TranscriptScrollAnchor::Content(anchor)) => anchor,
            other => panic!("far seek should re-anchor to loaded content, got {other:?}"),
        };
        assert_eq!(anchor.descriptor_index, 40);
        assert_eq!(anchor.block_id, BlockId::new(40));
        assert_eq!(
            document.viewport_state.mode,
            TranscriptViewportMode::Anchored
        );
    }

    #[test]
    fn sparse_average_rows_stays_stable_when_switching_loaded_windows() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let mut source = Transcript::new();
        for idx in 0..100 {
            let content = if idx >= 68 {
                format!("block {idx} {}", "long text ".repeat(40))
            } else {
                format!("block {idx}")
            };
            source.push(Block::Text { content });
        }
        let records = source.history.descriptor_records();
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            None,
        ));
        assert!(document.merge_descriptor_window(LoadedDescriptorWindow {
            start: smelt_store::TranscriptDescriptorIndex::new(40),
            total_count: records.len(),
            hydration: smelt_store::TranscriptDescriptorHydration::Hydrated,
            records: descriptor_records_with_ids(&records[40..48], 40),
        }));
        let average_before = document.approximate_average_descriptor_rows(20);

        let _plan = document.plan_projection_measured(
            &lua,
            20,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_tail(),
            10,
        );

        assert_eq!(
            document.approximate_average_descriptor_rows(20),
            average_before
        );
    }

    #[test]
    fn sparse_tail_target_reactivates_loaded_tail_window() {
        let lua = LuaRuntime::new();
        let theme = Theme::default();
        let records = transcript_records(100);
        let mut document = TranscriptDocument::from_loaded_transcript(sparse_loaded_transcript(
            &records,
            68..100,
            None,
        ));
        assert!(document.merge_descriptor_window(LoadedDescriptorWindow {
            start: smelt_store::TranscriptDescriptorIndex::new(40),
            total_count: records.len(),
            hydration: smelt_store::TranscriptDescriptorHydration::Hydrated,
            records: descriptor_records_with_ids(&records[40..48], 40),
        }));

        let _plan = document.plan_projection_measured(
            &lua,
            80,
            &theme,
            crate::content::transcript_buf::ScrollTarget::visible_tail(),
            10,
        );

        assert_eq!(
            document.active_descriptor_range,
            Some(
                smelt_store::TranscriptDescriptorIndex::new(68)
                    ..smelt_store::TranscriptDescriptorIndex::new(100)
            )
        );
        assert!(active_history_contains(&document, "block 90"));
        assert!(!active_history_contains(&document, "block 40"));
    }
}

pub(crate) struct TranscriptDisplayDocument<'a> {
    document: &'a mut TranscriptDocument,
    lua: &'a LuaRuntime,
    width: u16,
    theme: &'a Theme,
}

impl<'a> TranscriptDisplayDocument<'a> {
    pub(crate) fn new(
        document: &'a mut TranscriptDocument,
        lua: &'a LuaRuntime,
        width: u16,
        theme: &'a Theme,
    ) -> Self {
        Self {
            document,
            lua,
            width,
            theme,
        }
    }
}

impl DisplayDocument for TranscriptDisplayDocument<'_> {
    fn snapshot(&mut self) -> DisplaySnapshot {
        DisplaySnapshot {
            generation: self.document.projection_generation(),
            total_rows: self
                .document
                .approximate_scrollbar_total_rows(self.lua, self.width),
        }
    }

    fn materialize(
        &mut self,
        range: std::ops::Range<crate::smelt_edit::RowIndex>,
    ) -> crate::smelt_edit::DisplayRows {
        self.document.exact_or_gap_display_rows_for_range(
            self.lua,
            self.width,
            self.theme,
            range.start,
            range.end.saturating_sub(range.start),
        )
    }

    fn copy_range(&mut self, range: TextRange) -> Option<crate::smelt_edit::CopyOutput> {
        range.rows().map(|range| {
            self.document
                .copy_exact_loaded_range(self.lua, self.width, self.theme, range)
        })
    }
}

pub(crate) struct ResumePreviewCache {
    views: HashMap<String, TranscriptDocument>,
    order: VecDeque<String>,
    limit: usize,
}

impl ResumePreviewCache {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            views: HashMap::new(),
            order: VecDeque::new(),
            limit,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.views.clear();
        self.order.clear();
    }

    pub(crate) fn take(&mut self, key: &str) -> Option<TranscriptDocument> {
        self.views.remove(key)
    }

    pub(crate) fn store(&mut self, key: String, view: TranscriptDocument) {
        self.order.retain(|existing| existing != &key);
        self.order.push_back(key.clone());
        self.views.insert(key.clone(), view);

        while self.order.len() > self.limit {
            let Some(old_key) = self.order.pop_front() else {
                break;
            };
            if old_key != key {
                self.views.remove(&old_key);
            }
        }
    }

    pub(crate) fn set_inline_options(&mut self, options: InlineOptions) {
        for view in self.views.values_mut() {
            view.set_inline_options(options.clone());
        }
    }

    pub(crate) fn invalidate_theme(&mut self) {
        for view in self.views.values_mut() {
            view.invalidate_theme();
        }
    }

    pub(crate) fn invalidate_renderer_if_changed(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) {
        for view in self.views.values_mut() {
            view.invalidate_renderer_if_changed(generation, cache_key);
        }
    }
}

type TranscriptBlockSnapshot = (
    usize,
    &'static str,
    crate::smelt_edit::RowIndex,
    crate::smelt_edit::RowIndex,
    String,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranscriptNavigationDirection {
    Previous,
    Next,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptNavigationBlock {
    pub(crate) descriptor_index: usize,
    pub(crate) block_id: BlockId,
    pub(crate) role: &'static str,
    pub(crate) first_line: String,
    pub(crate) already_at_anchor: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranscriptBlockRevealPosition {
    pub(crate) block_id: BlockId,
    pub(crate) target_row: RowIndex,
    pub(crate) scroll_top: RowIndex,
}

fn transcript_block_role(block: &Block) -> &'static str {
    match block {
        Block::User { .. } => "user",
        Block::Mode { .. } => "mode",
        Block::ProcessStatus { .. } => "process_status",
        Block::Text { .. } => "assistant",
        Block::Thinking { .. } => "thinking",
        Block::ToolDraft { .. } | Block::ToolCall { .. } => "tool",
        Block::CodeLine { .. } => "code",
        Block::Exec { .. } => "exec",
        Block::Compacted { .. } => "compacted",
        Block::CompactionPreview { .. } => "compaction_preview",
    }
}

fn descriptor_role(descriptor: &TranscriptBlockDescriptor) -> &'static str {
    match descriptor {
        TranscriptBlockDescriptor::User { .. } => "user",
        TranscriptBlockDescriptor::Mode { .. } => "mode",
        TranscriptBlockDescriptor::ProcessStatus { .. } => "process_status",
        TranscriptBlockDescriptor::Text { .. } => "assistant",
        TranscriptBlockDescriptor::Thinking { .. } => "thinking",
        TranscriptBlockDescriptor::ToolDraft { .. }
        | TranscriptBlockDescriptor::ToolCall { .. } => "tool",
        TranscriptBlockDescriptor::CodeLine { .. } => "code",
        TranscriptBlockDescriptor::Exec { .. } => "exec",
        TranscriptBlockDescriptor::Compacted { .. } => "compacted",
        TranscriptBlockDescriptor::CompactionPreview { .. } => "compaction_preview",
    }
}

fn descriptor_first_line(descriptor: &TranscriptBlockDescriptor) -> String {
    descriptor
        .raw_text()
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()).map(str::to_string))
        .unwrap_or_default()
}

fn transcript_block_first_line(block: &Block) -> String {
    block
        .raw_text()
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()).map(str::to_string))
        .unwrap_or_default()
}

fn transcript_raw_first_line(history: &BlockHistory, id: BlockId) -> String {
    history
        .raw_text(id)
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()).map(str::to_string))
        .unwrap_or_default()
}

fn transcript_history_role(history: &BlockHistory, id: BlockId) -> &'static str {
    history.block_kind(id).unwrap_or_default()
}

#[derive(Clone, Copy)]
struct TranscriptPositionAnchor {
    anchor: Option<TranscriptContentAnchor>,
    position: crate::smelt_edit::DocPosition,
}

#[derive(Clone, Copy)]
struct TranscriptRangeAnchor {
    start: TranscriptPositionAnchor,
    end: TranscriptPositionAnchor,
}

struct TranscriptViewAnchors {
    following_tail: bool,
    scroll_top: Option<TranscriptPositionAnchor>,
    cursor: Option<TranscriptPositionAnchor>,
    selection_anchor: Option<TranscriptPositionAnchor>,
    drag_endpoint: Option<TranscriptPositionAnchor>,
    search_current: Option<TranscriptRangeAnchor>,
}

impl TuiApp {
    pub(crate) fn begin_turn(&mut self) {
        self.context_tokens_updated_this_turn = false;
        self.parser.begin_turn();
    }

    pub(crate) fn push_block(&mut self, block: Block) {
        self.transcript.push(block);
    }

    pub(crate) fn append_streaming_thinking(&mut self, delta: &str) {
        self.parser
            .append_streaming_thinking(self.transcript.history_mut(), delta);
    }

    pub(crate) fn flush_streaming_thinking(&mut self) {
        self.parser
            .flush_streaming_thinking(self.transcript.history_mut());
    }

    pub(crate) fn append_streaming_text(&mut self, delta: &str) {
        self.parser
            .append_streaming_text(self.transcript.history_mut(), delta);
    }

    pub(crate) fn flush_streaming_text(&mut self) {
        self.parser
            .flush_streaming_text(self.transcript.history_mut());
    }

    pub(crate) fn update_compaction_preview(&mut self, summary: String) {
        let follow_tail = self.transcript_win().is_following_tail();
        let existing = self.transcript.compaction_preview_id();
        let Some(id) = self.transcript.set_compaction_preview(summary) else {
            return;
        };
        if existing.is_none() {
            let width = self.transcript_width() as u16;
            self.transcript.fold_node(
                &self.lua,
                width,
                crate::content::render_plan::RenderNodeId::Block(id),
                crate::content::transcript_buf::FoldAction::Peek,
            );
        }
        if follow_tail {
            self.transcript_win_mut().follow_tail();
        }
        self.request_transient_render();
    }

    pub(crate) fn clear_compaction_preview(&mut self) {
        self.transcript.clear_compaction_preview();
    }

    pub(crate) fn start_tool(
        &mut self,
        call_id: String,
        name: String,
        summary: ::protocol::StyledLines,
        args: HashMap<String, serde_json::Value>,
    ) {
        let now = self.core.clock.instant_now();
        self.parser.start_tool(
            self.transcript.history_mut(),
            call_id,
            name,
            summary,
            args,
            now,
        );
    }

    pub(crate) fn start_exec(&mut self, command: String) {
        self.parser
            .start_exec(self.transcript.history_mut(), command);
    }

    pub(crate) fn append_exec_output(&mut self, chunk: &str) {
        self.parser
            .append_exec_output(self.transcript.history_mut(), chunk);
    }

    pub(crate) fn finish_exec(&mut self, exit_code: Option<i32>) {
        self.parser.finish_exec(exit_code);
    }

    pub(crate) fn finalize_exec(&mut self) {
        self.parser.finalize_exec(self.transcript.history_mut());
    }

    pub(crate) fn has_active_exec(&self) -> bool {
        self.parser.has_active_exec()
    }

    pub(crate) fn append_active_output(&mut self, call_id: &str, chunk: &str) {
        self.parser
            .append_active_output(self.transcript.history_mut(), call_id, chunk);
    }

    pub(crate) fn set_active_status(&mut self, call_id: &str, status: ToolStatus) {
        let now = self.core.clock.instant_now();
        self.parser
            .set_active_status(self.transcript.history_mut(), call_id, status, now);
    }

    pub(crate) fn set_active_user_message(&mut self, call_id: &str, msg: String) {
        self.parser
            .set_active_user_message(self.transcript.history_mut(), call_id, msg);
    }

    pub(crate) fn finish_tool(
        &mut self,
        call_id: &str,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
    ) {
        let now = self.core.clock.instant_now();
        self.parser.finish_tool(
            self.transcript.history_mut(),
            call_id,
            status,
            output,
            engine_elapsed,
            now,
        );
    }

    pub(crate) fn has_transcript_content(&mut self) -> bool {
        !self.transcript.is_empty()
    }

    /// Explicit full transcript materialization for APIs/tests that request the
    /// whole post-render display text. Do not use for normal viewport rendering.
    pub(crate) fn materialize_full_transcript_display_rows_expensive(
        &mut self,
    ) -> Arc<Vec<String>> {
        let _perf = smelt_perf::perf::begin("transcript:materialize_rows_full:explicit");
        self.sync_transcript_renderer_generation();
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        self.transcript.build_rows(&self.lua, tw, &theme)
    }

    fn capture_transcript_view_anchors(&mut self, width: u16) -> TranscriptViewAnchors {
        let (following_tail, scroll_top, cursor, selection_anchor, drag_endpoint) = {
            let win = self.transcript_win();
            let state = win.document_view_state();
            (
                win.is_following_tail(),
                win.scroll_top(),
                state.cursor,
                state.selection_anchor,
                state.drag_endpoint,
            )
        };
        let search_current = self
            .search
            .session
            .as_ref()
            .filter(|session| session.target == self.well_known.transcript)
            .and_then(|session| match &session.backend {
                crate::app::search::SearchBackend::Transcript(transcript) => transcript
                    .current
                    .and_then(|index| transcript.matches.get(index).copied()),
                crate::app::search::SearchBackend::Full { .. } => None,
            })
            .map(|range| self.transcript.range_anchor(&self.lua, width, range));
        TranscriptViewAnchors {
            following_tail,
            scroll_top: (!following_tail).then(|| {
                self.transcript.position_anchor(
                    &self.lua,
                    width,
                    crate::smelt_edit::DocPosition {
                        row: scroll_top,
                        byte_col: 0,
                    },
                )
            }),
            cursor: Some(self.transcript.position_anchor(&self.lua, width, cursor)),
            selection_anchor: selection_anchor
                .map(|position| self.transcript.position_anchor(&self.lua, width, position)),
            drag_endpoint: drag_endpoint
                .map(|position| self.transcript.position_anchor(&self.lua, width, position)),
            search_current,
        }
    }

    fn restore_transcript_view_anchors(&mut self, width: u16, anchors: TranscriptViewAnchors) {
        let scroll_top = anchors.scroll_top.map(|anchor| {
            self.transcript
                .resolve_position_anchor(&self.lua, width, anchor)
                .row
        });
        let cursor = anchors.cursor.map(|anchor| {
            self.transcript
                .resolve_position_anchor(&self.lua, width, anchor)
        });
        let selection_anchor = anchors.selection_anchor.map(|anchor| {
            self.transcript
                .resolve_position_anchor(&self.lua, width, anchor)
        });
        let drag_endpoint = anchors.drag_endpoint.map(|anchor| {
            self.transcript
                .resolve_position_anchor(&self.lua, width, anchor)
        });
        let search_current = anchors.search_current.map(|anchor| {
            self.transcript
                .resolve_range_anchor(&self.lua, width, anchor)
        });

        if let Some(win) = self.ui.win_mut(self.well_known.transcript) {
            if !anchors.following_tail {
                if let Some(row) = scroll_top {
                    win.pin_scroll(row);
                }
            }
            let mut state = win.document_view_state();
            if let Some(cursor) = cursor {
                state.cursor = cursor;
            }
            state.selection_anchor = selection_anchor;
            state.drag_endpoint = drag_endpoint;
            win.set_document_view_state(state);
        }

        if let Some(range) = search_current {
            if let Some(session) = self.search.session.as_mut() {
                if let crate::app::search::SearchBackend::Transcript(transcript) =
                    &mut session.backend
                {
                    if let Some(index) = transcript.current {
                        if let Some(current) = transcript.matches.get_mut(index) {
                            *current = range;
                        }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn transcript_total_rows(&mut self) -> crate::smelt_edit::RowIndex {
        self.document_snapshot_for_win(crate::app::TRANSCRIPT_WIN)
            .map(|snapshot| snapshot.total_rows)
            .unwrap_or(0)
    }

    pub(crate) fn transcript_rows_and_breaks_range(
        &mut self,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> crate::smelt_edit::DisplayRows {
        let _perf = smelt_perf::perf::begin("transcript:materialize_rows_range");
        self.materialize_document_rows(crate::app::TRANSCRIPT_WIN, start, count)
            .unwrap_or_default()
    }

    pub(crate) fn transcript_visible_rows(
        &mut self,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> Vec<String> {
        self.transcript_rows_and_breaks_range(start, count)
            .into_text_rows()
    }

    pub(crate) fn transcript_node_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        self.transcript.node_metadata_at_row(&self.lua, width, row)
    }

    pub(crate) fn fold_transcript_node_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
        action: crate::content::transcript_buf::FoldAction,
        activation: crate::content::transcript_buf::FoldActivation,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let anchors = self.capture_transcript_view_anchors(width);
        let changed = self
            .transcript
            .fold_node_at_row(&self.lua, width, row, action, activation);
        if changed {
            self.restore_transcript_view_anchors(width, anchors);
        }
        changed
    }

    pub(crate) fn fold_transcript_node(
        &mut self,
        id: crate::content::render_plan::RenderNodeId,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let anchors = self.capture_transcript_view_anchors(width);
        let changed = self.transcript.fold_node(&self.lua, width, id, action);
        if changed {
            self.restore_transcript_view_anchors(width, anchors);
        }
        changed
    }

    pub(crate) fn fold_all_transcript_nodes(
        &mut self,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let anchors = self.capture_transcript_view_anchors(width);
        let changed = self.transcript.fold_all(&self.lua, width, action);
        if changed {
            self.restore_transcript_view_anchors(width, anchors);
        }
        changed
    }

    pub(crate) fn fold_transcript_block_kind(
        &mut self,
        kind: &str,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let anchors = self.capture_transcript_view_anchors(width);
        let changed = self
            .transcript
            .fold_block_kind(&self.lua, width, kind, action);
        if changed {
            self.restore_transcript_view_anchors(width, anchors);
        }
        changed
    }

    pub(crate) fn snap_cpos_to_selectable(&mut self, rows: &[String], cpos: usize) -> usize {
        let buf_id = self.transcript_win().buf;
        let Some(buf) = self.ui.buf(buf_id) else {
            return cpos;
        };
        let mut acc = 0usize;
        for (r, row) in rows.iter().enumerate() {
            let row_end = acc + row.len();
            if cpos <= row_end {
                let col_byte = cpos.saturating_sub(acc).min(row.len());
                let col = row[..col_byte].chars().count();
                let snapped = smelt_buffer::coords::snap_col_to_selectable(buf, r, col);
                if snapped == col {
                    return cpos;
                }
                let byte_col: usize = row.chars().take(snapped).map(|c| c.len_utf8()).sum();
                return acc + byte_col;
            }
            acc = row_end + 1;
        }
        cpos
    }

    /// Snapshot of the laid-out transcript blocks as `(idx, role, first_row,
    /// rows, first_line)`. `idx` is 0-based into `transcript.history.order` to
    /// match `session.rewind_to(block_idx)`. `first_line` is the first
    /// non-empty line of the block's raw source text (truncated upstream by
    /// the caller as needed). Returns empty when no projection has run yet
    /// (i.e. before the first frame).
    pub(crate) fn visible_transcript_block_snapshots(&self) -> Vec<TranscriptBlockSnapshot> {
        self.transcript.visible_block_snapshots()
    }

    pub(crate) fn transcript_block_snapshots(&mut self) -> Vec<TranscriptBlockSnapshot> {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        self.transcript
            .materialize_block_snapshots(&self.lua, width)
    }

    pub(crate) fn transcript_block_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<TranscriptBlockSnapshot> {
        self.transcript_block_snapshots()
            .into_iter()
            .find(|(_, _, first_row, rows, _)| {
                let end = first_row.saturating_add(*rows);
                row >= *first_row && row < end
            })
    }

    pub(crate) fn transcript_block_before_or_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
        role: Option<&str>,
    ) -> Option<TranscriptBlockSnapshot> {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        self.transcript
            .block_snapshot_before_or_at_row(&self.lua, width, row, role)
    }

    pub(crate) fn previous_transcript_navigation_block(
        &mut self,
        role: Option<&str>,
    ) -> Option<TranscriptNavigationBlock> {
        self.sync_transcript_renderer_generation();
        self.transcript.previous_navigation_block(role)
    }

    pub(crate) fn next_transcript_navigation_block(
        &mut self,
        role: Option<&str>,
    ) -> Option<TranscriptNavigationBlock> {
        self.sync_transcript_renderer_generation();
        self.transcript.next_navigation_block(role)
    }

    pub(crate) fn reveal_transcript_descriptor_block(
        &mut self,
        descriptor_index: usize,
        top_padding: crate::smelt_edit::RowIndex,
        cursor: bool,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let viewport_rows = self
            .transcript_win()
            .viewport
            .map(|viewport| viewport.rect.height)
            .unwrap_or(1)
            .max(1);
        let Some(reveal) = self.transcript.descriptor_block_reveal_position(
            &self.lua,
            width,
            descriptor_index,
            0,
            top_padding,
            viewport_rows,
        ) else {
            return false;
        };
        let window_scroll_before = self.transcript_scroll_top();
        self.reveal_position(
            crate::app::TRANSCRIPT_WIN,
            crate::smelt_edit::DocPosition {
                row: reveal.target_row,
                byte_col: 0,
            },
            crate::app::reveal::RevealOptions {
                top_padding,
                cursor,
                ..Default::default()
            },
        );
        self.record_transcript_scroll_intent(
            "reveal_block",
            TranscriptScrollIntent::RevealBlock {
                descriptor_index,
                block_id: reveal.block_id,
                row_offset: 0,
                screen_padding_top: top_padding,
            },
            window_scroll_before,
        );
        true
    }

    pub(crate) fn finish_transcript_turn(&mut self) {
        let _perf = smelt_perf::perf::begin("render:finish_turn");
        self.parser
            .finalize_active_tools(self.transcript.history_mut());
    }

    pub(crate) fn set_agent_blocked_paused(&mut self, paused: bool) {
        let now = self.core.clock.instant_now();
        self.working.set_paused(paused);
        self.parser
            .set_active_tools_paused(self.transcript.history(), paused, now);
    }

    pub(crate) fn apply_pending_history_appends_for_request(&mut self) {
        let appends = std::mem::take(&mut self.pending_history_appends);
        for append in appends {
            let mode_base = append.mode().map(|_| self.mode_history_base());
            let history_append = append.history_append(mode_base);
            let replace_note_kind = history_append.replacement_note_kind();
            let result = self.apply_history_append_to_history(&history_append);
            if let Some(block) = append.transcript_block(&self.lua) {
                self.commit_history_append_block(block, replace_note_kind, result);
            }
        }
    }

    pub(crate) fn commit_pending_history_append(&mut self, item: &protocol::HistoryItem) {
        let Some(idx) = self
            .pending_history_appends
            .iter()
            .position(|append| append.matches_history_item(item))
        else {
            return;
        };
        let append = self.pending_history_appends.remove(idx);
        let result = if append.replacement_note_kind().is_some() {
            protocol::HistoryAppendResult::ReplacedLast
        } else {
            protocol::HistoryAppendResult::Pushed
        };
        if let Some(block) = append.transcript_block(&self.lua) {
            self.commit_history_append_block(block, append.replacement_note_kind(), result);
        }
    }

    pub(crate) fn commit_history_append_block(
        &mut self,
        block: Block,
        replace_note_kind: Option<protocol::HistoryNoteKind>,
        result: protocol::HistoryAppendResult,
    ) {
        match result {
            protocol::HistoryAppendResult::Unchanged => {}
            protocol::HistoryAppendResult::RemovedLast => {
                self.remove_last_mode_block();
            }
            protocol::HistoryAppendResult::ReplacedLast => {
                if self.rewrite_last_mode_block(block.clone(), replace_note_kind) {
                    return;
                }
                self.push_block(block);
            }
            protocol::HistoryAppendResult::Pushed => self.push_block(block),
        }
    }

    fn rewrite_last_mode_block(
        &mut self,
        block: Block,
        replace_note_kind: Option<protocol::HistoryNoteKind>,
    ) -> bool {
        if replace_note_kind != Some(protocol::HistoryNoteKind::ModeChange) {
            return false;
        }
        let history = self.transcript.history();
        let Some(id) = history.order.last().copied() else {
            return false;
        };
        if !matches!(history.block(id), Some(Block::Mode { .. })) {
            return false;
        }
        self.transcript.history_mut().rewrite(id, block);
        true
    }

    fn remove_last_mode_block(&mut self) {
        let history = self.transcript.history();
        let Some((idx, id)) = history.order.iter().copied().enumerate().next_back() else {
            return;
        };
        if matches!(history.block(id), Some(Block::Mode { .. })) {
            self.truncate_to(idx);
        }
    }

    pub(crate) fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.transcript.drain_finished_blocks()
    }

    /// No-op: width changes invalidate the cache implicitly on next paint.
    pub(crate) fn invalidate_for_width(&mut self, _width: u16) {}

    pub(crate) fn invalidate_for_theme(&mut self) {
        self.transcript.invalidate_theme();
        self.resume_preview_cache.invalidate_theme();
    }

    pub(crate) fn inline_options(&self) -> InlineOptions {
        InlineOptions {
            file_icons: FileIconOptions::new(
                self.core.config.settings.file_icons,
                self.core.config.settings.file_icon_colors,
                self.ui.theme().is_light(),
                Some(std::path::PathBuf::from(&self.cwd)),
            ),
        }
    }

    pub(crate) fn sync_inline_options(&mut self) {
        let options = self.inline_options();
        self.transcript.set_inline_options(options.clone());
        self.resume_preview_cache.set_inline_options(options);
    }

    pub(crate) fn sync_transcript_renderer_generation(&mut self) {
        let generation = self.lua.transcript_renderer_generation();
        let inline_options = self.inline_options();
        let cache_key = crate::content::display_layout::transcript_renderer_cache_key(
            &self.lua,
            &inline_options,
        );
        self.transcript
            .invalidate_renderer_if_changed(generation, cache_key);
        self.resume_preview_cache
            .invalidate_renderer_if_changed(generation, cache_key);
    }

    /// Install a complete theme and publish it to the process-wide active slot.
    pub(crate) fn install_theme(&mut self, theme: Theme) {
        *self.ui.theme_mut() = theme;
        smelt_core::theme::set_active(self.ui.theme().clone());
        self.sync_inline_options();
        self.sync_transcript_renderer_generation();
        self.invalidate_for_theme();
    }

    /// Mutate the current theme and republish.
    pub(crate) fn mutate_theme(&mut self, f: impl FnOnce(&mut Theme)) {
        f(self.ui.theme_mut());
        smelt_core::theme::set_active(self.ui.theme().clone());
        self.sync_inline_options();
        self.sync_transcript_renderer_generation();
        self.invalidate_for_theme();
    }

    pub(crate) fn clear_transcript(&mut self) {
        self.pending_history_appends.clear();
        self.transcript.history_mut().clear();
        self.parser.clear();
    }

    pub(crate) fn user_turns(&self) -> Vec<(usize, String)> {
        self.transcript.user_turns()
    }

    pub(crate) fn truncate_to(&mut self, block_idx: usize) {
        self.transcript.truncate_to(block_idx);
        self.parser.clear_tools();
    }

    /// Advance spinner animation. Returns `true` if the frame changed.
    pub(crate) fn update_spinner(&mut self) -> bool {
        let mut changed = false;
        if let (Some(elapsed), Some(prev_frame)) =
            (self.working.elapsed(), self.working.last_spinner_frame())
        {
            let frame = smelt_core::content::spinner_frame_index(elapsed);
            if frame != prev_frame {
                self.working.set_last_spinner_frame(frame);
                changed = true;
            }
        }
        changed
    }

    /// Per-line selection ranges (line, col_start, col_end) in display-cell units.
    /// No-op when no vim visual or selection anchor is active.
    #[cfg(test)]
    pub(crate) fn transcript_selection_highlights(
        &mut self,
        scroll_top: crate::smelt_edit::RowIndex,
        row_base: crate::smelt_edit::RowIndex,
        viewport_rows: u16,
    ) -> Vec<(usize, u16, u16)> {
        let win = self.transcript_win();
        if win.has_materialized_rows() {
            let buf_id = win.buf;
            let buf = match self.ui.buf(buf_id) {
                Some(b) => b,
                None => return Vec::new(),
            };
            let now = self.core.clock.instant_now();
            return win
                .row_selection_ranges(buf, viewport_rows, now)
                .into_iter()
                .map(|r| (r.line, r.col_start, r.col_end))
                .collect();
        }
        let vim_visual = win.vim_enabled()
            && matches!(
                win.vim_mode(),
                crate::smelt_edit::VimMode::Visual | crate::smelt_edit::VimMode::VisualLine
            );
        let anchor_set = win.selection_anchor().is_some();
        if !vim_visual && !anchor_set {
            return Vec::new();
        }

        let buf_id = self.transcript_win().buf;
        let buf = match self.ui.buf(buf_id) {
            Some(b) => b,
            None => return Vec::new(),
        };
        let rows = buf.lines();
        if rows.is_empty() {
            return Vec::new();
        }
        let text = buf.text();
        let win = self.transcript_win();
        let endpoint = win.effective_endpoint();
        let active_selection = if win.vim_enabled() {
            match win.vim_mode() {
                crate::smelt_edit::VimMode::Visual | crate::smelt_edit::VimMode::VisualLine => {
                    crate::smelt_edit::vim::visual_range(
                        win.vim_state(),
                        &text,
                        endpoint,
                        win.vim_mode(),
                    )
                }
                _ => win.selection_range_at(endpoint, &text),
            }
        } else {
            win.selection_range_at(endpoint, &text)
        };
        let (s, e) = match active_selection {
            Some(range) => range,
            None => return Vec::new(),
        };
        if s >= e {
            return Vec::new();
        }
        // Route through the shared coord helper so the prompt's per-row
        // selection painting and the transcript's stay one implementation -
        // including the "1-cell fallback span on empty middle rows" rule.
        let first = scroll_top
            .saturating_sub(row_base)
            .min(usize::MAX as crate::smelt_edit::RowIndex) as usize;
        let last = first + viewport_rows as usize;
        smelt_buffer::coords::byte_range_to_row_ranges(buf, s, e)
            .into_iter()
            .filter(|r| r.line >= first && r.line < last)
            .map(|r| (r.line, r.col_start, r.col_end))
            .collect()
    }

    /// Wrap the prompt input against `width` and return the resulting row count.
    /// The Lua layout composer reads this as `state.prompt_input_rows` and
    /// gives the prompt window that many rows in the splits tree.
    pub(crate) fn measure_prompt_input_rows(
        &self,
        edit_buf: &crate::smelt_edit::Buffer,
        width: usize,
        placeholder: Option<&str>,
    ) -> u16 {
        let usable = width.saturating_sub(2).min(u16::MAX as usize) as u16;
        let store = self.input.store.lock().unwrap();
        let lines = build_prompt_display_lines(
            edit_buf.source(),
            &edit_buf.attachment_ids,
            &store,
            placeholder,
        );
        let cursor_padding = prompt_display_uses_cursor_padding(edit_buf.source(), placeholder);
        let layout = if cursor_padding {
            WrappedLayout::from_lines_with_cursor_padding(&lines, usable, true)
        } else {
            WrappedLayout::from_lines(&lines, usable, true)
        };
        layout.visual_count().max(1) as u16
    }
}

#[cfg(test)]
mod tests {
    use crate::app::search::SearchDirection;
    use crate::app::test_harness::TestApp;
    use crate::content::transcript_buf::{FoldAction, FoldActivation};
    use crate::smelt_edit::{DocPosition, TextRange};

    #[test]
    fn selection_highlights_subtract_materialized_row_base() {
        let mut harness = TestApp::builder().build();
        let buf_id = harness.app.transcript_win().buf;
        {
            let buf = harness.app.ui.buf_mut(buf_id).expect("transcript buffer");
            buf.set_all_lines((30..40).map(|i| format!("line {i}")).collect());
        }

        let (line_idx, start, end, line_count) = {
            let buf = harness.app.ui.buf(buf_id).expect("transcript buffer");
            let line_idx = buf
                .lines()
                .iter()
                .position(|line| line == "line 39")
                .expect("tail line is materialized");
            let offsets = smelt_buffer::text::line_start_offsets(buf.lines());
            let start = offsets[line_idx];
            (line_idx, start, start + "line 39".len(), buf.lines().len())
        };
        {
            let win = harness.app.transcript_win_mut();
            win.set_selection_anchor(Some(start));
            win.set_cpos(end);
        }

        let ranges = harness.app.transcript_selection_highlights(39, 30, 5);
        assert!(
            ranges.iter().any(|(line, _, _)| *line == line_idx),
            "selection range should be expressed in materialized buffer rows"
        );
        assert!(ranges.iter().all(|(line, _, _)| *line < line_count));
    }

    #[test]
    fn fold_preserves_transcript_document_anchors() {
        let mut app = TestApp::builder().build().app;
        app.push_block(smelt_core::Block::Thinking {
            content: (0..20)
                .map(|i| format!("folded prefix {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        app.push_block(smelt_core::Block::Text {
            content: "anchor target\nanchor cursor".into(),
        });
        app.push_block(smelt_core::Block::Text {
            content: "after".into(),
        });
        assert!(app.fold_transcript_node_at_row(0, FoldAction::Open, FoldActivation::AnyNodeRow));
        app.render_normal_to(&mut std::io::sink());
        let before = app.transcript_block_snapshots();
        let target_start = before
            .iter()
            .find(|(_, _, _, _, first_line)| first_line == "anchor target")
            .map(|(_, _, row, _, _)| *row)
            .expect("target block before fold");
        app.submit_search(
            app.well_known.transcript,
            SearchDirection::Forward,
            "anchor cursor".into(),
        );
        let before_match = app
            .search
            .session
            .as_ref()
            .and_then(|session| session.current_range())
            .expect("current search match before fold");
        assert!(matches!(
            before_match,
            TextRange::Rows(range) if range.start.row == target_start.saturating_add(1)
        ));
        {
            let win = app.transcript_win_mut();
            win.pin_scroll(target_start);
            let mut state = win.document_view_state();
            state.cursor = DocPosition {
                row: target_start.saturating_add(1),
                byte_col: "anchor ".len(),
            };
            state.selection_anchor = Some(DocPosition {
                row: target_start,
                byte_col: 0,
            });
            state.drag_endpoint = Some(DocPosition {
                row: target_start.saturating_add(1),
                byte_col: "anchor cursor".len(),
            });
            win.set_document_view_state(state);
        }

        assert!(app.fold_transcript_node_at_row(0, FoldAction::Close, FoldActivation::AnyNodeRow));
        let after = app.transcript_block_snapshots();
        let new_target_start = after
            .iter()
            .find(|(_, _, _, _, first_line)| first_line == "anchor target")
            .map(|(_, _, row, _, _)| *row)
            .expect("target block after fold");
        let win = app.transcript_win();
        let state = win.document_view_state();

        assert!(new_target_start < target_start);
        assert_eq!(win.scroll_top(), new_target_start);
        assert_eq!(
            state.cursor,
            DocPosition {
                row: new_target_start.saturating_add(1),
                byte_col: "anchor ".len(),
            }
        );
        assert_eq!(
            state.selection_anchor,
            Some(DocPosition {
                row: new_target_start,
                byte_col: 0,
            })
        );
        assert_eq!(
            state.drag_endpoint,
            Some(DocPosition {
                row: new_target_start.saturating_add(1),
                byte_col: "anchor cursor".len(),
            })
        );
        let after_match = app
            .search
            .session
            .as_ref()
            .and_then(|session| session.current_range())
            .expect("current search match after fold");
        assert!(matches!(
            after_match,
            TextRange::Rows(range) if range.start.row == new_target_start.saturating_add(1)
        ));
    }

    #[test]
    fn fold_preserves_transcript_tail_follow() {
        let mut app = TestApp::builder().build().app;
        for i in 0..40 {
            app.push_block(smelt_core::Block::Text {
                content: format!("tail line {i}"),
            });
        }
        app.transcript_win_mut().follow_tail();
        app.render_normal_to(&mut std::io::sink());
        assert!(app.transcript_win().is_following_tail());

        assert!(app.fold_all_transcript_nodes(FoldAction::Close));

        assert!(app.transcript_win().is_following_tail());
    }

    fn test_descriptor_record(idx: u64) -> smelt_store::TranscriptDescriptorRecord {
        smelt_store::TranscriptDescriptorRecord {
            block_idx: idx,
            history_idx: None,
            kind: "text".into(),
            tool_call_id: None,
            tool_name: None,
            content_hash: format!("{idx}"),
            estimated_text_bytes: 8,
            preview_text: format!("block {idx}"),
            search_text: format!("block {idx}"),
            descriptor_json: serde_json::to_string(&smelt_core::TranscriptBlockDescriptor::Text {
                content: format!("block {idx}"),
            })
            .unwrap(),
            origin_json: Some(
                serde_json::to_string(&smelt_core::BlockOrigin::History(idx as usize)).unwrap(),
            ),
            tool_state_json: None,
        }
    }

    #[test]
    fn transcript_document_loads_descriptor_window_from_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = smelt_store::SessionDb::open(dir.path().join("session.db")).unwrap();
        let records = (0..4).map(test_descriptor_record).collect::<Vec<_>>();
        db.replace_transcript_descriptor_records(&records).unwrap();
        let tail = db.read_transcript_descriptor_tail_slice(1).unwrap();
        drop(db);

        let loaded = super::LoadedTranscript::from_descriptor_slice(tail, dir.path().to_path_buf())
            .expect("loaded tail");
        let document = super::TranscriptDocument::from_loaded_transcript(loaded);

        let window = document
            .load_descriptor_window((1..3).into())
            .expect("loaded middle window");
        assert_eq!(window.start.get(), 1);
        assert_eq!(window.end().get(), 3);
        assert_eq!(window.total_count, 4);
        assert_eq!(window.records.len(), 2);
    }

    #[test]
    fn transcript_document_merges_descriptor_windows_without_discarding_tail() {
        let dir = tempfile::tempdir().unwrap();
        let db = smelt_store::SessionDb::open(dir.path().join("session.db")).unwrap();
        let records = (0..6).map(test_descriptor_record).collect::<Vec<_>>();
        db.replace_transcript_descriptor_records(&records).unwrap();
        let tail = db.read_transcript_descriptor_tail_slice(2).unwrap();
        drop(db);

        let loaded = super::LoadedTranscript::from_descriptor_slice(tail, dir.path().to_path_buf())
            .expect("loaded tail");
        let mut document = super::TranscriptDocument::from_loaded_transcript(loaded);
        assert_eq!(document.descriptor_total_count(), Some(6));
        assert_eq!(document.loaded_descriptor_count(), 2);
        let ranges = document.loaded_descriptor_ranges();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start.get(), 4);
        assert_eq!(ranges[0].end.get(), 6);
        assert_eq!(
            document.descriptor_range_state(4..6),
            super::DescriptorRangeState::Loaded
        );
        assert_eq!(
            document.descriptor_range_state(1..3),
            super::DescriptorRangeState::Missing
        );

        let middle = document
            .load_descriptor_window((1..3).into())
            .expect("loaded middle window");
        assert!(document.merge_descriptor_window(middle));

        assert_eq!(document.loaded_descriptor_count(), 4);
        let ranges = document.loaded_descriptor_ranges();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].start.get(), 1);
        assert_eq!(ranges[0].end.get(), 3);
        assert_eq!(ranges[1].start.get(), 4);
        assert_eq!(ranges[1].end.get(), 6);
        assert_eq!(
            document.descriptor_range_state(1..3),
            super::DescriptorRangeState::Loaded
        );
        assert_eq!(
            document.descriptor_range_state(3..4),
            super::DescriptorRangeState::Missing
        );
        assert_eq!(document.history().order.len(), 2);
        assert_eq!(
            document.history().order,
            vec![
                smelt_core::transcript_model::BlockId::new(1),
                smelt_core::transcript_model::BlockId::new(2),
            ]
        );
        let origins = document
            .history()
            .descriptor_records()
            .into_iter()
            .map(|record| record.origin)
            .collect::<Vec<_>>();
        assert_eq!(
            origins,
            vec![
                Some(smelt_core::BlockOrigin::History(1)),
                Some(smelt_core::BlockOrigin::History(2)),
            ]
        );
    }
}
