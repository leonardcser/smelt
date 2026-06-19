//! Transcript block history, streaming state, projection, and cursor glyph cache.

use crate::app::TuiApp;
use crate::content::prompt_parser::{
    build_prompt_display_lines, prompt_display_uses_cursor_padding,
};
use crate::smelt_edit::{
    Buffer, DisplayDocument, DisplayRow, DisplayRows, DisplaySnapshot, RowIndex, TextRange, Theme,
};
use smelt_buffer::wrap_layout::WrappedLayout;
use smelt_core::content::file_icons::FileIconOptions;
use smelt_core::content::highlight::InlineOptions;

use smelt_core::content::transcript::Transcript;
use smelt_core::lua::runtime::LuaRuntime;
use smelt_core::transcript_model::{
    Block, BlockHistory, BlockId, ToolOutputRef, ToolStatus, TranscriptBlockRecordWithId,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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

    pub(crate) fn from_sqlite_dir(session_dir: PathBuf) -> Option<Self> {
        let store = SqliteTranscriptStore::open_read_only(&session_dir).ok()?;
        let rows = store.read_all_descriptor_records_expensive().ok()?;
        Self::from_full_descriptor_rows(rows, session_dir)
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

    pub(crate) fn from_full_descriptor_rows(
        rows: Vec<smelt_store::TranscriptDescriptorRecord>,
        session_dir: PathBuf,
    ) -> Option<Self> {
        let total_count = rows.len();
        let records = descriptor_records_from_rows(rows)?;
        Some(Self {
            transcript: Transcript::new(),
            descriptor_window: Some(LoadedDescriptorWindow {
                start: smelt_store::TranscriptDescriptorIndex::new(0),
                total_count,
                hydration: smelt_store::TranscriptDescriptorHydration::Hydrated,
                records,
            }),
            session_dir: Some(session_dir),
        })
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

    fn read_all_descriptor_records_expensive(
        &self,
    ) -> smelt_store::Result<Vec<smelt_store::TranscriptDescriptorRecord>> {
        self.db.read_all_transcript_descriptor_records()
    }

    fn read_tail_descriptor_slice_for_rows(
        &self,
        width: u16,
        target_rows: u16,
    ) -> smelt_store::Result<smelt_store::TranscriptDescriptorSlice> {
        let total = {
            let _perf = smelt_perf::perf::begin("transcript:resume_tail:descriptor_count");
            self.db.transcript_descriptor_dense_extent()?
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

pub(crate) struct TranscriptDocument {
    transcript: Transcript,
    projection: crate::content::transcript_buf::TranscriptProjection,
    render_cache: TranscriptRenderCache,
    compaction_preview_id: Option<BlockId>,
    sparse_descriptors: SparseTranscriptDescriptors,
    active_descriptor_range: Option<Range<smelt_store::TranscriptDescriptorIndex>>,
    session_dir: Option<PathBuf>,
}

pub(crate) struct TranscriptRenderContext {
    pub(crate) width: u16,
    pub(crate) theme_key: u64,
    pub(crate) renderer_generation: u64,
    pub(crate) renderer_cache_key: Option<u64>,
}

pub(crate) struct TranscriptProjectionPlan {
    inner: crate::content::transcript_buf::ProjectionPlan,
    row_offset: RowIndex,
    total_rows: RowIndex,
    prefix_gap_scroll: Option<RowIndex>,
    viewport_rows: u16,
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
        *self = Self::from_loaded_transcript(loaded);
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
    }

    fn estimated_loaded_descriptor_rows(&self, width: u16) -> RowIndex {
        let width = usize::from(width.max(1));
        self.sparse_descriptors
            .records
            .range(match self.active_descriptor_range.as_ref() {
                Some(range) => range.clone(),
                None => {
                    smelt_store::TranscriptDescriptorIndex::new(0)
                        ..smelt_store::TranscriptDescriptorIndex::new(usize::MAX)
                }
            })
            .map(|(_, record)| {
                record
                    .record
                    .descriptor
                    .raw_text()
                    .map(|text| estimate_text_rows_for_width(&text, width))
                    .unwrap_or(1)
                    .saturating_add(1)
            })
            .sum()
    }

    fn active_descriptor_count(&self) -> usize {
        match self.active_descriptor_range.as_ref() {
            Some(range) => self.sparse_descriptors.records.range(range.clone()).count(),
            None => self.sparse_descriptors.loaded_descriptor_count(),
        }
    }

    fn average_descriptor_rows(&self, width: u16) -> RowIndex {
        let loaded = self.active_descriptor_count() as RowIndex;
        if loaded == 0 {
            return 2;
        }
        self.estimated_loaded_descriptor_rows(width)
            .saturating_add(loaded.saturating_sub(1))
            .saturating_div(loaded)
            .max(1)
    }

    fn sparse_prefix_row_offset(&self, width: u16) -> RowIndex {
        (self
            .sparse_descriptors
            .missing_prefix_count_for_range(self.active_descriptor_range.as_ref())
            as RowIndex)
            .saturating_mul(self.average_descriptor_rows(width))
    }

    fn sparse_suffix_rows(&self, width: u16) -> RowIndex {
        (self
            .sparse_descriptors
            .missing_suffix_count_for_range(self.active_descriptor_range.as_ref())
            as RowIndex)
            .saturating_mul(self.average_descriptor_rows(width))
    }

    fn virtual_total_rows(&self, width: u16, loaded_rows: RowIndex) -> RowIndex {
        self.sparse_prefix_row_offset(width)
            .saturating_add(loaded_rows)
            .saturating_add(self.sparse_suffix_rows(width))
    }

    fn virtual_row_to_loaded_row(&self, width: u16, row: RowIndex) -> RowIndex {
        row.saturating_sub(self.sparse_prefix_row_offset(width))
    }

    fn offset_node_row(
        &self,
        width: u16,
        mut node: crate::content::transcript_buf::TranscriptNodeRow,
    ) -> crate::content::transcript_buf::TranscriptNodeRow {
        let offset = self.sparse_prefix_row_offset(width);
        node.first_row = node.first_row.saturating_add(offset);
        node
    }

    pub(crate) fn set_inline_options(&mut self, options: InlineOptions) {
        self.projection.set_inline_options(options);
    }

    pub(crate) fn invalidate_theme(&mut self) {
        self.projection.invalidate_theme();
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

    pub(crate) fn estimated_total_rows(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> crate::smelt_edit::RowIndex {
        let loaded_rows =
            self.projection
                .estimated_total_rows(lua, &mut self.transcript.history, width);
        self.virtual_total_rows(width, loaded_rows)
    }

    pub(crate) fn materialize_block_layout(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> Vec<(
        BlockId,
        crate::smelt_edit::RowIndex,
        crate::smelt_edit::RowIndex,
    )> {
        let offset = self.sparse_prefix_row_offset(width);
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
        let layout = self.materialize_block_layout(lua, width);
        self.block_snapshots_from_layout(layout.into_iter())
    }

    pub(crate) fn block_snapshot_before_or_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        role: Option<&str>,
    ) -> Option<TranscriptBlockSnapshot> {
        let node = self.block_node_before_or_at_row(lua, width, row, |history, id| {
            role.is_none_or(|role| transcript_history_role(history, id) == role)
                && !transcript_raw_first_line(history, id).is_empty()
        })?;
        let block_id = node.id.as_block_id()?;
        let history = self.history();
        let idx = history.order.iter().position(|id| *id == block_id)?;
        Some((
            idx,
            transcript_history_role(history, block_id),
            node.first_row,
            node.rows,
            transcript_raw_first_line(history, block_id),
        ))
    }

    pub(crate) fn materialize_search_layout(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> crate::content::transcript_buf::TranscriptSearchLayout {
        let offset = self.sparse_prefix_row_offset(width);
        let mut layout =
            self.projection
                .materialize_search_layout(lua, &mut self.transcript.history, width);
        for entry in &mut layout.entries {
            entry.first_row = entry.first_row.saturating_add(offset);
        }
        layout
    }

    pub(crate) fn materialize_search_layout_for_blocks(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        block_indices: &[u64],
    ) -> crate::content::transcript_buf::TranscriptSearchLayout {
        let offset = self.sparse_prefix_row_offset(width);
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
        let local_row = self.virtual_row_to_loaded_row(width, row);
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
        let avg_rows = self.average_descriptor_rows(width).max(1);
        let visible_descriptors =
            (RowIndex::from(viewport_rows.max(1)) / avg_rows).saturating_add(1) as usize;
        let count = visible_descriptors.saturating_mul(4).max(32).min(total);
        let start = center
            .saturating_sub(count / 2)
            .min(total.saturating_sub(count));
        let end = start.saturating_add(count).min(total);
        Some(
            smelt_store::TranscriptDescriptorIndex::new(start)
                ..smelt_store::TranscriptDescriptorIndex::new(end),
        )
    }

    fn descriptor_window_range_for_virtual_row(
        &self,
        width: u16,
        row: RowIndex,
        viewport_rows: u16,
    ) -> Option<Range<smelt_store::TranscriptDescriptorIndex>> {
        let avg_rows = self.average_descriptor_rows(width).max(1);
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

    fn activate_descriptor_window_for_virtual_row(
        &mut self,
        width: u16,
        row: RowIndex,
        viewport_rows: u16,
    ) -> bool {
        let Some(range) = self.descriptor_window_range_for_virtual_row(width, row, viewport_rows)
        else {
            return false;
        };
        self.activate_descriptor_window_range(range)
    }

    fn activate_tail_descriptor_window(&mut self, width: u16, viewport_rows: u16) -> bool {
        let Some(range) = self.tail_descriptor_window_range(width, viewport_rows) else {
            return false;
        };
        self.activate_descriptor_window_range(range)
    }

    pub(crate) fn plan_projection_measured(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        scroll_target: crate::content::transcript_buf::ScrollTarget,
        viewport_rows: u16,
    ) -> TranscriptProjectionPlan {
        match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Row(row),
            ) => {
                let _ = self.activate_descriptor_window_for_virtual_row(width, row, viewport_rows);
            }
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Tail,
            ) => {
                let _ = self.activate_tail_descriptor_window(width, viewport_rows);
            }
        }
        let row_offset = self.sparse_prefix_row_offset(width);
        let prefix_gap_scroll = match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Row(row),
            ) if row < row_offset => Some(row),
            _ => None,
        };
        let local_target = match scroll_target {
            crate::content::transcript_buf::ScrollTarget::Visible(
                crate::content::transcript_buf::ScrollAnchor::Row(row),
            ) => crate::content::transcript_buf::ScrollTarget::visible_row(
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
        TranscriptProjectionPlan {
            inner,
            row_offset,
            total_rows: self.virtual_total_rows(width, loaded_rows),
            prefix_gap_scroll,
            viewport_rows,
        }
    }

    pub(crate) fn project_planned(
        &mut self,
        lua: &LuaRuntime,
        buf: &mut Buffer,
        theme: &Theme,
        plan: TranscriptProjectionPlan,
    ) -> crate::smelt_edit::MaterializedRows {
        if let Some(scroll_top) = plan.prefix_gap_scroll {
            let viewport_rows = RowIndex::from(plan.viewport_rows.max(1));
            let row_base = scroll_top
                .saturating_sub(viewport_rows / 2)
                .min(plan.row_offset);
            let materialized_rows = plan
                .row_offset
                .saturating_sub(row_base)
                .min(plan.total_rows.saturating_sub(row_base));
            buf.set_all_lines(vec![String::new(); materialized_rows as usize]);
            return crate::smelt_edit::MaterializedRows {
                clamped_scroll: scroll_top,
                row_base,
                total_rows: plan.total_rows,
                materialized_rows,
            };
        }
        let mut rows = self.projection.project_planned(
            lua,
            buf,
            &mut self.transcript.history,
            theme,
            plan.inner,
        );
        rows.clamped_scroll = rows.clamped_scroll.saturating_add(plan.row_offset);
        rows.row_base = rows.row_base.saturating_add(plan.row_offset);
        rows.total_rows = plan.total_rows;
        rows
    }

    pub(crate) fn display_rows_for_range(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> crate::smelt_edit::DisplayRows {
        let row_offset = self.sparse_prefix_row_offset(width);
        let end = start.saturating_add(count);
        if count == 0 || end <= start {
            return DisplayRows::empty();
        }
        let mut rows = Vec::new();
        if start < row_offset {
            let prefix_end = end.min(row_offset);
            rows.extend((start..prefix_end).map(|_| {
                DisplayRow::new(String::new(), Vec::new())
                    .with_break_before(crate::smelt_edit::RowBreak::Hard)
            }));
            if end <= row_offset {
                return DisplayRows { rows };
            }
        }
        let local_start = start.saturating_sub(row_offset);
        let local_end = end.saturating_sub(row_offset);
        let mut loaded = self.projection.display_rows_for_range(
            lua,
            &mut self.transcript.history,
            width,
            theme,
            local_start..local_end,
        );
        rows.append(&mut loaded.rows);
        DisplayRows { rows }
    }

    pub(crate) fn cached_display_rows_for_range(
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
        let rows = self.display_rows_for_range(lua, render.width, theme, start, count);
        self.render_cache.insert(key, rows.clone());
        rows
    }

    pub(crate) fn node_metadata_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        let local_row = self.virtual_row_to_loaded_row(width, row);
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
        let local_row = self.virtual_row_to_loaded_row(width, row);
        self.projection
            .row_anchor_at_row(lua, &mut self.transcript.history, width, local_row)
    }

    pub(crate) fn row_for_anchor(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        anchor: crate::content::transcript_buf::TranscriptRowAnchor,
    ) -> Option<crate::smelt_edit::RowIndex> {
        let offset = self.sparse_prefix_row_offset(width);
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
            anchor: self.row_anchor_at_row(lua, width, position.row),
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
            .and_then(|anchor| self.row_for_anchor(lua, width, anchor))
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

    pub(crate) fn block_node_before_or_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        matches: impl FnMut(&BlockHistory, BlockId) -> bool,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        let local_row = self.virtual_row_to_loaded_row(width, row);
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
        let local_row = self.virtual_row_to_loaded_row(width, row);
        self.projection.fold_node_at_row(
            lua,
            &mut self.transcript.history,
            width,
            crate::content::transcript_buf::FoldAtRow {
                row: local_row,
                action,
                activation,
            },
        )
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
        self.projection
            .fold_node(&self.transcript.history, id, action)
    }

    pub(crate) fn fold_all(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.prepare_layout(lua, width);
        self.projection.fold_all(&self.transcript.history, action)
    }

    pub(crate) fn fold_block_kind(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        kind: &str,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.prepare_layout(lua, width);
        self.projection
            .fold_block_kind(&self.transcript.history, kind, action)
    }

    pub(crate) fn copy_range(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        range: crate::smelt_edit::DocRange,
    ) -> crate::smelt_edit::CopyOutput {
        let row_offset = self.sparse_prefix_row_offset(width);
        if range.end.row < row_offset {
            return crate::smelt_edit::CopyOutput::default();
        }
        let mut local_range = range;
        local_range.start.row = local_range.start.row.saturating_sub(row_offset);
        local_range.end.row = local_range.end.row.saturating_sub(row_offset);
        self.projection
            .copy_range(lua, &mut self.transcript.history, width, theme, local_range)
    }

    pub(crate) fn history(&self) -> &BlockHistory {
        &self.transcript.history
    }

    pub(crate) fn history_mut(&mut self) -> &mut BlockHistory {
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
        self.projection
            .invalidate_renderer_if_changed(generation, cache_key)
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
    }

    pub(crate) fn push_with_origin(
        &mut self,
        block: Block,
        origin: smelt_core::transcript_model::BlockOrigin,
    ) {
        self.transcript.push_with_origin(block, origin);
    }

    pub(crate) fn set_compaction_preview(&mut self, summary: String) -> Option<BlockId> {
        let summary = summary.trim().to_string();
        if summary.is_empty() {
            return self.clear_compaction_preview();
        }

        if let Some(id) = self.compaction_preview_id {
            if self.transcript.block(id).is_some() {
                self.transcript.rewrite_compaction_preview(id, summary);
                return Some(id);
            }
            self.compaction_preview_id = None;
        }

        let id = self.transcript.push_compaction_preview(summary)?;
        self.compaction_preview_id = Some(id);
        Some(id)
    }

    pub(crate) fn clear_compaction_preview(&mut self) -> Option<BlockId> {
        let id = self.compaction_preview_id.take()?;
        self.transcript.remove_compaction_preview(id);
        Some(id)
    }

    pub(crate) fn compaction_preview_id(&self) -> Option<BlockId> {
        self.compaction_preview_id
    }

    pub(crate) fn insert_checkpoint_marker_at(&mut self, block_index: usize, block: Block) {
        self.transcript
            .insert_checkpoint_marker_at(block_index, block);
    }

    pub(crate) fn remove_unoriginated_at(&mut self, block_index: usize) -> Option<Block> {
        self.transcript.remove_unoriginated_at(block_index)
    }

    pub(crate) fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.transcript.drain_finished_blocks()
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

        let total_rows = document.estimated_total_rows(&lua, 80);

        assert!(total_rows > loaded_rows);
        assert!(document.sparse_prefix_row_offset(80) > 0);
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
        let offset = document.sparse_prefix_row_offset(80);
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
            .materialize_block_layout(&lua, 80)
            .into_iter()
            .find(|(id, _, _)| *id == BlockId::new(target_index as u64))
            .expect("target block row");
        let target_display_rows =
            document.display_rows_for_range(&lua, 80, &theme, target_row, target_rows);
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

    fn transcript_records(count: usize) -> Vec<TranscriptBlockRecord> {
        let mut source = Transcript::new();
        for idx in 0..count {
            source.push(Block::Text {
                content: format!("block {idx}"),
            });
        }
        source.history.descriptor_records()
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
            total_rows: self.document.estimated_total_rows(self.lua, self.width),
        }
    }

    fn materialize(
        &mut self,
        range: std::ops::Range<crate::smelt_edit::RowIndex>,
    ) -> crate::smelt_edit::DisplayRows {
        self.document.display_rows_for_range(
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
                .copy_range(self.lua, self.width, self.theme, range)
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
    anchor: Option<crate::content::transcript_buf::TranscriptRowAnchor>,
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
        self.transcript_win_mut().follow_tail();
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
