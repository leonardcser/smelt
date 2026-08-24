use super::*;

pub(super) struct TranscriptRecordExtentModel<'a> {
    pub(super) records: &'a SparseTranscriptRecords,
    pub(super) store: Option<&'a SqliteTranscriptStore>,
    pub(super) width: u16,
    pub(super) active_range: Option<Range<usize>>,
    pub(super) total_count: Option<usize>,
    pub(super) fallback_rows_per_record: RowIndex,
}

fn persisted_record_rows_to_transcript_rows(
    estimated_record_rows: RowIndex,
    record_count: usize,
) -> RowIndex {
    estimated_record_rows.saturating_sub(RowIndex::from(record_count > 0))
}

#[derive(Clone, Debug)]
pub(super) struct RetainedTranscriptExtent {
    pub(super) width: u16,
    pub(super) total_count: usize,
    pub(super) total_record_rows: RowIndex,
    pub(super) prefix_rows: HashMap<usize, RowIndex>,
    pub(super) prefix_order: VecDeque<usize>,
    pub(super) row_locations: HashMap<RowIndex, (usize, RowIndex)>,
    pub(super) row_location_order: VecDeque<RowIndex>,
}

impl RetainedTranscriptExtent {
    fn matches(&self, width: u16, total_count: usize) -> bool {
        self.width == width.max(1) && self.total_count == total_count
    }

    pub(super) fn insert_prefix_rows(&mut self, record_index: usize, rows: RowIndex) {
        if self.prefix_rows.insert(record_index, rows).is_some()
            || record_index == 0
            || record_index == self.total_count
        {
            return;
        }
        self.prefix_order.push_back(record_index);
        while self.prefix_order.len() > TRANSCRIPT_RETAINED_EXTENT_QUERY_LIMIT {
            if let Some(evicted) = self.prefix_order.pop_front() {
                self.prefix_rows.remove(&evicted);
            }
        }
    }

    pub(super) fn insert_row_location(&mut self, row: RowIndex, location: (usize, RowIndex)) {
        if self.row_locations.insert(row, location).is_some() {
            return;
        }
        self.row_location_order.push_back(row);
        while self.row_location_order.len() > TRANSCRIPT_RETAINED_EXTENT_QUERY_LIMIT {
            if let Some(evicted) = self.row_location_order.pop_front() {
                self.row_locations.remove(&evicted);
            }
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ExactRecordRowsKey {
    record_index: usize,
    width: u16,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
    node_key: LayoutKey,
}

#[derive(Default)]
pub(super) struct TranscriptExtentIndex {
    pub(super) retained: Option<RetainedTranscriptExtent>,
    exact_record_rows: HashMap<ExactRecordRowsKey, RowIndex>,
    latest_exact_record_rows: BTreeMap<(u16, usize), ExactRecordRowsKey>,
}

impl TranscriptExtentIndex {
    pub(super) fn clear_exact_local_record_rows(&mut self) {
        self.exact_record_rows.clear();
        self.latest_exact_record_rows.clear();
    }

    pub(super) fn clear_persisted_record_estimates(&mut self) {
        self.retained = None;
    }

    pub(super) fn exact_observation_count(&self) -> usize {
        self.exact_record_rows.len()
    }

    pub(super) fn exact_local_rows_for_record(
        &self,
        record_index: usize,
        width: u16,
    ) -> Option<RowIndex> {
        let width = width.max(1);
        let key = self.latest_exact_record_rows.get(&(width, record_index))?;
        self.exact_record_rows.get(key).copied()
    }

    pub(super) fn observe_exact_loaded_record_rows(
        &mut self,
        records: &SparseTranscriptRecords,
        snapshot: crate::content::transcript_buf::TranscriptExactHeightSnapshot,
    ) {
        let loaded_record_indices: HashSet<_> =
            records.records.keys().map(|index| index.get()).collect();
        self.latest_exact_record_rows
            .retain(|(_, record_index), _| loaded_record_indices.contains(record_index));
        self.exact_record_rows
            .retain(|key, _| loaded_record_indices.contains(&key.record_index));
        if snapshot.observations.is_empty() {
            return;
        }
        let record_by_block: HashMap<BlockId, usize> = records
            .records
            .iter()
            .map(|(index, record)| (record.block_id, index.get()))
            .collect();
        for observation in snapshot.observations {
            let Some(record_index) = record_by_block.get(&observation.block_id).copied() else {
                continue;
            };
            let key = ExactRecordRowsKey {
                record_index,
                width: snapshot.width.max(1),
                renderer_generation: snapshot.renderer_generation,
                renderer_cache_key: snapshot.renderer_cache_key,
                presentation_generation: snapshot.presentation_generation,
                node_key: observation.key,
            };
            let latest_key = (key.width, record_index);
            self.exact_record_rows.insert(key.clone(), observation.rows);
            if let Some(previous_key) = self
                .latest_exact_record_rows
                .insert(latest_key, key.clone())
            {
                if previous_key != key {
                    self.exact_record_rows.remove(&previous_key);
                }
            }
        }
    }

    pub(super) fn local_rows_for_loaded_records(
        &self,
        records: &SparseTranscriptRecords,
        width: u16,
    ) -> RowIndex {
        records
            .records
            .iter()
            .map(|(index, record)| {
                self.exact_local_rows_for_record(index.get(), width)
                    .unwrap_or_else(|| {
                        record
                            .stored
                            .estimated_text_bytes
                            .max(1)
                            .div_ceil(u64::from(width.max(1)))
                            .saturating_add(1)
                    })
            })
            .sum()
    }

    fn loaded_record_count(&self, records: &SparseTranscriptRecords) -> usize {
        records.loaded_record_count()
    }

    pub(super) fn fallback_average_rows_per_loaded_record(
        &self,
        records: &SparseTranscriptRecords,
        width: u16,
    ) -> RowIndex {
        let loaded = self.loaded_record_count(records) as RowIndex;
        if loaded == 0 {
            return 2;
        }
        self.local_rows_for_loaded_records(records, width)
            .saturating_add(loaded.saturating_sub(1))
            .saturating_div(loaded)
            .max(1)
    }

    fn approximate_rows_for_unloaded_record_range(
        &self,
        store: Option<&SqliteTranscriptStore>,
        width: u16,
        range: Range<usize>,
    ) -> Option<RowIndex> {
        if range.start >= range.end {
            return Some(0);
        }
        store?
            .estimated_record_rows(width.max(1), range)
            .ok()
            .map(|rows| rows as RowIndex)
    }

    pub(super) fn record_extent_model<'a>(
        &self,
        records: &'a SparseTranscriptRecords,
        active_record_range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
        store: Option<&'a SqliteTranscriptStore>,
        width: u16,
    ) -> TranscriptRecordExtentModel<'a> {
        let width = width.max(1);
        TranscriptRecordExtentModel {
            records,
            store,
            width,
            active_range: active_record_range.map(|range| range.start.get()..range.end.get()),
            total_count: records.total_count(),
            fallback_rows_per_record: self.fallback_average_rows_per_loaded_record(records, width),
        }
    }

    fn estimated_rows_for_missing_record_range(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
        range: Range<usize>,
    ) -> RowIndex {
        let count = range.end.saturating_sub(range.start) as RowIndex;
        self.approximate_rows_for_unloaded_record_range(model.store, model.width, range)
            .unwrap_or_else(|| count.saturating_mul(model.fallback_rows_per_record))
    }

    fn estimated_rows_for_record_range(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
        range: Range<usize>,
    ) -> RowIndex {
        let _perf = smelt_perf::perf::begin("transcript:extent:estimated_rows_for_record_range");
        smelt_perf::perf::record_value(
            "transcript:extent:estimated_rows_for_record_range:records",
            range.end.saturating_sub(range.start) as u64,
        );
        if range.start >= range.end {
            return 0;
        }
        self.estimated_rows_for_missing_record_range(model, range)
    }

    fn prepare_retained_extent(&mut self, model: &TranscriptRecordExtentModel<'_>) -> bool {
        let Some(total) = model.total_count else {
            return false;
        };
        let width = model.width.max(1);
        if self
            .retained
            .as_ref()
            .is_some_and(|extent| extent.matches(width, total))
        {
            return true;
        }
        let total_record_rows = model
            .store
            .and_then(|store| store.total_estimated_record_rows(width).ok())
            .unwrap_or_else(|| (total as RowIndex).saturating_mul(model.fallback_rows_per_record));
        let mut prefix_rows = HashMap::with_capacity(2);
        prefix_rows.insert(0, 0);
        prefix_rows.insert(total, total_record_rows);
        self.retained = Some(RetainedTranscriptExtent {
            width,
            total_count: total,
            total_record_rows,
            prefix_rows,
            prefix_order: VecDeque::new(),
            row_locations: HashMap::new(),
            row_location_order: VecDeque::new(),
        });
        true
    }

    pub(super) fn estimated_rows_before_record(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
        record_index: usize,
    ) -> RowIndex {
        let _perf = smelt_perf::perf::begin("transcript:extent:estimated_rows_before_record");
        let Some(total) = model.total_count else {
            return self.estimated_rows_for_record_range(model, 0..record_index);
        };
        let record_index = record_index.min(total);
        if !self.prepare_retained_extent(model) {
            return 0;
        }
        if let Some(rows) = self
            .retained
            .as_ref()
            .and_then(|extent| extent.prefix_rows.get(&record_index).copied())
        {
            return rows;
        }
        let rows = self.estimated_rows_for_record_range(model, 0..record_index);
        if let Some(extent) = self.retained.as_mut() {
            extent.insert_prefix_rows(record_index, rows);
        }
        rows
    }

    pub(super) fn cached_rows_before_record(
        &self,
        width: u16,
        total_count: usize,
        record_index: usize,
    ) -> Option<RowIndex> {
        let record_index = record_index.min(total_count);
        let extent = self.retained.as_ref()?;
        if !extent.matches(width, total_count) {
            return None;
        }
        extent.prefix_rows.get(&record_index).copied()
    }

    pub(super) fn cache_rows_before_record(
        &mut self,
        width: u16,
        total_count: usize,
        record_index: usize,
        rows: RowIndex,
    ) {
        let Some(extent) = self.retained.as_mut() else {
            return;
        };
        if extent.matches(width, total_count) {
            extent.insert_prefix_rows(record_index.min(total_count), rows);
        }
    }

    pub(super) fn cached_total_rows(&self, width: u16, total_count: usize) -> Option<RowIndex> {
        let extent = self.retained.as_ref()?;
        if !extent.matches(width, total_count) {
            return None;
        }
        Some(persisted_record_rows_to_transcript_rows(
            extent.total_record_rows,
            total_count,
        ))
    }

    fn estimated_total_record_rows(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
    ) -> Option<RowIndex> {
        let total = model.total_count?;
        self.prepare_retained_extent(model);
        let rows = self.retained.as_ref()?.total_record_rows;
        Some(persisted_record_rows_to_transcript_rows(rows, total))
    }

    pub(super) fn estimated_record_for_row(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
        row: RowIndex,
    ) -> Option<(usize, RowIndex)> {
        let total = model.total_count?;
        if total == 0 || !self.prepare_retained_extent(model) {
            return None;
        }
        if let Some(location) = self
            .retained
            .as_ref()
            .and_then(|extent| extent.row_locations.get(&row).copied())
        {
            return Some(location);
        }
        let location = model
            .store
            .and_then(|store| store.record_for_row(model.width, row).ok().flatten())
            .map(|location| (location.record_index.get(), location.row_offset))
            .unwrap_or_else(|| {
                let rows_per_record = model.fallback_rows_per_record.max(1);
                (
                    usize::try_from(row / rows_per_record)
                        .unwrap_or(usize::MAX)
                        .min(total.saturating_sub(1)),
                    row % rows_per_record,
                )
            });
        if let Some(extent) = self.retained.as_mut() {
            extent.insert_row_location(row, location);
        }
        Some(location)
    }

    fn estimated_sparse_prefix_rows(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
    ) -> RowIndex {
        let end = model
            .active_range
            .as_ref()
            .map(|range| range.start)
            .unwrap_or_else(|| model.records.missing_prefix_count_for_range(None));
        self.estimated_rows_before_record(model, end)
    }

    fn estimated_sparse_suffix_rows(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
    ) -> RowIndex {
        let Some(total) = model.total_count else {
            let count = model.records.missing_suffix_count_for_range(None);
            return (count as RowIndex).saturating_mul(model.fallback_rows_per_record);
        };
        let start = model
            .active_range
            .as_ref()
            .map(|range| range.end)
            .unwrap_or(total);
        self.estimated_rows_for_record_range(model, start..total)
    }

    fn extent_total_rows(
        &mut self,
        model: &TranscriptRecordExtentModel<'_>,
        exact_loaded_rows: RowIndex,
    ) -> RowIndex {
        if let Some(total) = model.total_count {
            let mut total_rows = self.estimated_total_record_rows(model).unwrap_or_default();
            if model
                .active_range
                .as_ref()
                .is_some_and(|range| range.end >= total)
            {
                // Tail windows can include live blocks that are not in persisted row estimates yet.
                total_rows = total_rows.max(
                    self.estimated_sparse_prefix_rows(model)
                        .saturating_add(exact_loaded_rows),
                );
            }
            return total_rows;
        }

        self.estimated_sparse_prefix_rows(model)
            .saturating_add(exact_loaded_rows)
            .saturating_add(self.estimated_sparse_suffix_rows(model))
    }

    pub(super) fn approximate_sparse_prefix_rows(
        &mut self,
        records: &SparseTranscriptRecords,
        active_record_range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
        store: Option<&SqliteTranscriptStore>,
        width: u16,
    ) -> RowIndex {
        let model = self.record_extent_model(records, active_record_range, store, width);
        self.estimated_sparse_prefix_rows(&model)
    }

    pub(super) fn scrollbar_total_rows(
        &mut self,
        records: &SparseTranscriptRecords,
        active_record_range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
        store: Option<&SqliteTranscriptStore>,
        width: u16,
        exact_loaded_rows: RowIndex,
    ) -> RowIndex {
        let model = self.record_extent_model(records, active_record_range, store, width);
        self.extent_total_rows(&model, exact_loaded_rows)
    }
}
