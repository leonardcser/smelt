use super::*;

#[derive(Default)]
pub(super) struct SparseTranscriptRecords {
    pub(super) total_count: Option<usize>,
    pub(super) loaded_ranges: Vec<Range<smelt_store::TranscriptRecordOffset>>,
    pub(super) records: BTreeMap<smelt_store::TranscriptRecordOffset, StoredBlockWithId>,
    pub(super) lru: VecDeque<smelt_store::TranscriptRecordOffset>,
}

impl SparseTranscriptRecords {
    pub(super) fn from_loaded(loaded: Option<&LoadedRecordWindow>) -> Self {
        let mut records = Self::default();
        if let Some(loaded) = loaded {
            records.merge(loaded);
        }
        records
    }

    pub(super) fn merge(&mut self, loaded: &LoadedRecordWindow) -> bool {
        let start = loaded.start;
        let end = loaded.end();
        if start >= end {
            self.total_count = Some(loaded.total_count);
            return false;
        }
        self.total_count = Some(loaded.total_count);
        self.records
            .retain(|index, _| *index < start || *index >= end);
        self.lru.retain(|index| *index < start || *index >= end);
        for (offset, record) in loaded.records.iter().cloned().enumerate() {
            let index =
                smelt_store::TranscriptRecordOffset::new(start.get().saturating_add(offset));
            self.records.insert(index, record);
            self.lru.push_back(index);
        }
        self.add_loaded_range(start..end);
        true
    }

    pub(super) fn truncate(&mut self, total_count: usize) -> usize {
        let total_count = self
            .total_count
            .map_or(total_count, |current| current.min(total_count));
        let end = smelt_store::TranscriptRecordOffset::new(total_count);
        self.total_count = Some(total_count);
        self.records.retain(|index, _| *index < end);
        self.lru.retain(|index| *index < end);
        for range in &mut self.loaded_ranges {
            range.end = range.end.min(end);
        }
        self.loaded_ranges.retain(|range| range.start < range.end);
        total_count
    }

    pub(super) fn invalidate_from(&mut self, start: smelt_store::TranscriptRecordOffset) {
        self.records.retain(|index, _| *index < start);
        self.lru.retain(|index| *index < start);
        self.loaded_ranges = self
            .loaded_ranges
            .drain(..)
            .filter_map(|range| {
                if range.start >= start {
                    None
                } else {
                    Some(range.start..range.end.min(start))
                }
            })
            .collect();
    }

    fn add_loaded_range(&mut self, range: Range<smelt_store::TranscriptRecordOffset>) {
        if range.start >= range.end {
            return;
        }
        self.loaded_ranges.push(range);
        self.loaded_ranges.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<smelt_store::TranscriptRecordOffset>> =
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

    pub(super) fn records_for_range(
        &self,
        range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
    ) -> Vec<StoredBlockWithId> {
        match range {
            Some(range) => self
                .records
                .range(range.clone())
                .map(|(_, record)| record.clone())
                .collect(),
            None => self.records.values().cloned().collect(),
        }
    }

    pub(super) fn total_count(&self) -> Option<usize> {
        self.total_count
    }

    pub(super) fn loaded_record_count(&self) -> usize {
        self.records.len()
    }

    pub(super) fn missing_prefix_count_for_range(
        &self,
        range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
    ) -> usize {
        range
            .map(|range| range.start.get())
            .unwrap_or_default()
            .min(self.total_count.unwrap_or_default())
    }

    pub(super) fn missing_suffix_count_for_range(
        &self,
        range: Option<&Range<smelt_store::TranscriptRecordOffset>>,
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

    pub(super) fn range_is_loaded(
        &self,
        range: &Range<smelt_store::TranscriptRecordOffset>,
    ) -> bool {
        range.start >= range.end
            || self
                .loaded_ranges
                .iter()
                .any(|loaded| loaded.start <= range.start && loaded.end >= range.end)
    }

    pub(super) fn missing_ranges(
        &self,
        range: &Range<smelt_store::TranscriptRecordOffset>,
    ) -> Vec<Range<smelt_store::TranscriptRecordOffset>> {
        if range.start >= range.end {
            return Vec::new();
        }
        let mut missing = Vec::new();
        let mut cursor = range.start;
        for loaded in &self.loaded_ranges {
            if loaded.end <= cursor {
                continue;
            }
            if loaded.start >= range.end {
                break;
            }
            if loaded.start > cursor {
                missing.push(cursor..loaded.start.min(range.end));
            }
            cursor = cursor.max(loaded.end);
            if cursor >= range.end {
                break;
            }
        }
        if cursor < range.end {
            missing.push(cursor..range.end);
        }
        missing
    }

    pub(super) fn cache_range_around(
        &self,
        range: &Range<smelt_store::TranscriptRecordOffset>,
    ) -> Range<smelt_store::TranscriptRecordOffset> {
        let Some(total) = self.total_count else {
            return range.clone();
        };
        let guard = TRANSCRIPT_RECORD_PAGE_SIZE.saturating_mul(TRANSCRIPT_RECORD_CACHE_GUARD_PAGES);
        let mut start = range.start.get().saturating_sub(guard);
        let mut end = range.end.get().saturating_add(guard).min(total);
        if end.saturating_sub(start) >= TRANSCRIPT_RECORD_PAGE_SIZE {
            start = start / TRANSCRIPT_RECORD_PAGE_SIZE * TRANSCRIPT_RECORD_PAGE_SIZE;
            end = end
                .saturating_add(TRANSCRIPT_RECORD_PAGE_SIZE - 1)
                .saturating_div(TRANSCRIPT_RECORD_PAGE_SIZE)
                .saturating_mul(TRANSCRIPT_RECORD_PAGE_SIZE)
                .min(total);
        }
        smelt_store::TranscriptRecordOffset::new(start)
            ..smelt_store::TranscriptRecordOffset::new(end)
    }

    pub(super) fn touch_range(&mut self, range: &Range<smelt_store::TranscriptRecordOffset>) {
        self.lru
            .retain(|index| !(range.start <= *index && *index < range.end));
        self.lru
            .extend(self.records.range(range.clone()).map(|(index, _)| *index));
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.records
            .values()
            .map(|record| record.stored.retained_bytes())
            .sum()
    }

    pub(super) fn enforce_byte_budget(
        &mut self,
        pinned: &Range<smelt_store::TranscriptRecordOffset>,
        budget: usize,
    ) {
        let mut retained = self.retained_bytes();
        let mut attempts = self.lru.len();
        while retained > budget && attempts > 0 {
            attempts -= 1;
            let Some(candidate) = self.lru.pop_front() else {
                break;
            };
            if pinned.start <= candidate && candidate < pinned.end {
                self.lru.push_back(candidate);
                continue;
            }
            if let Some(record) = self.records.remove(&candidate) {
                retained = retained.saturating_sub(record.stored.retained_bytes());
                attempts = self.lru.len();
            }
        }
        self.lru.retain(|index| self.records.contains_key(index));
        self.rebuild_loaded_ranges();
        smelt_perf::perf::record_value("transcript:record_cache:retained_bytes", retained as u64);
        smelt_perf::perf::record_value(
            "transcript:record_cache:oversize_debt_bytes",
            retained.saturating_sub(budget) as u64,
        );
    }

    fn rebuild_loaded_ranges(&mut self) {
        let mut ranges: Vec<Range<smelt_store::TranscriptRecordOffset>> = Vec::new();
        for index in self.records.keys().copied() {
            match ranges.last_mut() {
                Some(range) if range.end.get() == index.get() => {
                    range.end =
                        smelt_store::TranscriptRecordOffset::new(index.get().saturating_add(1));
                }
                _ => ranges.push(
                    index..smelt_store::TranscriptRecordOffset::new(index.get().saturating_add(1)),
                ),
            }
        }
        self.loaded_ranges = ranges;
    }

    pub(super) fn record(
        &self,
        index: smelt_store::TranscriptRecordOffset,
    ) -> Option<&StoredBlockWithId> {
        self.records.get(&index)
    }

    pub(super) fn record_index_for_block_id(
        &self,
        block_id: BlockId,
    ) -> Option<smelt_store::TranscriptRecordOffset> {
        self.records
            .iter()
            .find_map(|(index, record)| (record.block_id == block_id).then_some(*index))
    }

    pub(super) fn navigation_record(
        &self,
        role: &str,
        anchor: smelt_store::TranscriptRecordOffset,
        direction: TranscriptNavigationDirection,
    ) -> Option<(usize, StoredBlockWithId)> {
        match direction {
            TranscriptNavigationDirection::Previous => {
                self.records
                    .range(..anchor)
                    .rev()
                    .find_map(|(index, record)| {
                        (record.stored.kind.as_str() == role).then(|| (index.get(), record.clone()))
                    })
            }
            TranscriptNavigationDirection::Next => {
                let after_anchor =
                    smelt_store::TranscriptRecordOffset::new(anchor.get().saturating_add(1));
                self.records
                    .range(after_anchor..)
                    .find_map(|(index, record)| {
                        (record.stored.kind.as_str() == role).then(|| (index.get(), record.clone()))
                    })
            }
        }
    }

    #[cfg(test)]
    pub(super) fn loaded_ranges(&self) -> &[Range<smelt_store::TranscriptRecordOffset>] {
        &self.loaded_ranges
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RecordRangeState {
    Unavailable,
    Loaded,
    Missing,
}
