use super::*;

/// Records per correction chunk. Prefix sums add whole chunks, then the spans
/// inside one chunk, so a lookup stays cheap however long the session grows.
const CHUNK_RECORDS: usize = 1024;
/// Upper bound on remembered spans. The chunk farthest from the viewport is
/// forgotten first; its records fall back to their store estimate.
const MAX_SPANS: usize = 1 << 18;
/// Upper bound on memoized store prefix queries.
const MAX_ESTIMATE_PREFIXES: usize = 4096;

/// Row geometry of the whole transcript in a single coordinate space.
///
/// A record's rows are what the projection last measured for it, or the store's
/// estimate when it was never measured. Window offsets, the scrollbar total and
/// row-to-record lookups all derive from [`Self::rows_before`], so they can never
/// disagree with each other. The projection estimates its unmeasured nodes with
/// [`Self::record_rows`], so the loaded window's layout agrees with it too.
#[derive(Default)]
pub(super) struct TranscriptExtent {
    layout: Option<ExtentLayoutKey>,
    /// Laid-out record spans keyed by their exclusive end record.
    spans: BTreeMap<usize, ExtentSpan>,
    /// Sum of span corrections per chunk of span end records.
    chunk_corrections: BTreeMap<usize, i64>,
    estimate_prefixes: HashMap<(u16, usize), RowIndex>,
    /// Bumped whenever spans are forgotten rather than refined.
    generation: u64,
}

/// Layout inputs that every projected height depends on. Per-node view state
/// changes are corrected by re-observation instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ExtentLayoutKey {
    pub(super) width: u16,
    pub(super) renderer_generation: u64,
    pub(super) renderer_cache_key: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct ExtentSpan {
    start: usize,
    rows: RowIndex,
    estimate: RowIndex,
}

impl ExtentSpan {
    fn correction(&self) -> i64 {
        self.rows as i64 - self.estimate as i64
    }
}

/// Where a record's store estimate comes from.
pub(super) struct ExtentEstimates<'a> {
    pub(super) store: Option<&'a SqliteTranscriptStore>,
    pub(super) records: &'a SparseTranscriptRecords,
    pub(super) width: u16,
}

impl ExtentEstimates<'_> {
    /// Estimated rows of one record. Loaded records carry the same profile the
    /// store aggregates, so this agrees with [`Self::rows_before`] exactly.
    fn record_rows(&self, record: usize) -> RowIndex {
        self.records
            .record(smelt_store::TranscriptRecordOffset::new(record))
            .map_or(FALLBACK_RECORD_ROWS, |record| {
                record.stored.extent.estimated_rows(self.width)
            })
    }

    fn rows_before(&self, record: usize) -> RowIndex {
        self.store
            .and_then(|store| store.estimated_record_rows(self.width, 0..record).ok())
            .unwrap_or_else(|| {
                (0..record)
                    .map(|record| self.record_rows(record))
                    .sum::<RowIndex>()
            })
    }
}

/// Rows assumed for a record whose profile is unknown.
const FALLBACK_RECORD_ROWS: RowIndex = 2;

impl TranscriptExtent {
    /// Forget laid-out rows after a presentation change. Store estimates stay.
    pub(super) fn forget_layout(&mut self) {
        self.layout = None;
        self.spans.clear();
        self.chunk_corrections.clear();
        self.generation += 1;
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// Forget everything from `record` on, after those records were rewritten.
    pub(super) fn invalidate_from(&mut self, record: usize) {
        let stale: Vec<usize> = self
            .spans
            .range(record.saturating_add(1)..)
            .map(|(end, _)| *end)
            .collect();
        for end in stale {
            self.remove_span(end);
        }
        self.generation += 1;
        self.estimate_prefixes
            .retain(|(_, before), _| *before <= record);
    }

    /// Rows above `record` in transcript coordinates.
    pub(super) fn rows_before(
        &mut self,
        estimates: &ExtentEstimates<'_>,
        record: usize,
    ) -> RowIndex {
        if self
            .layout
            .is_none_or(|layout| layout.width != estimates.width)
        {
            return self.estimate_before(estimates, record);
        }
        if let Some((_, span)) = self.spans.range(record.saturating_add(1)..).next() {
            if span.start < record {
                let span = *span;
                let start_rows = self.rows_before(estimates, span.start);
                let inner = self
                    .estimate_before(estimates, record)
                    .saturating_sub(self.estimate_before(estimates, span.start));
                return start_rows.saturating_add(inner.min(span.rows));
            }
        }
        let estimate = self.estimate_before(estimates, record) as i64;
        estimate
            .saturating_add(self.correction_before(record))
            .max(0) as RowIndex
    }

    /// The record containing `row` and the row's offset inside it.
    pub(super) fn record_at_row(
        &mut self,
        estimates: &ExtentEstimates<'_>,
        total_count: usize,
        row: RowIndex,
    ) -> Option<(usize, RowIndex)> {
        if total_count == 0 {
            return None;
        }
        let (mut low, mut high) = (0, total_count - 1);
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            if self.rows_before(estimates, mid) <= row {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let first_row = self.rows_before(estimates, low);
        let last_row = self
            .rows_before(estimates, low + 1)
            .saturating_sub(1)
            .max(first_row);
        Some((low, row.clamp(first_row, last_row) - first_row))
    }

    /// Rows of each record in `records`, consistent with [`Self::rows_before`]:
    /// a span's rows are dealt out to its records in order, each taking up to
    /// its estimate and the last one taking the rest.
    pub(super) fn record_rows(
        &self,
        estimates: &ExtentEstimates<'_>,
        records: Range<usize>,
    ) -> Vec<RowIndex> {
        let corrects = self
            .layout
            .is_some_and(|layout| layout.width == estimates.width);
        let mut rows = Vec::with_capacity(records.len());
        let mut record = records.start;
        while record < records.end {
            let span = corrects
                .then(|| self.spans.range(record.saturating_add(1)..).next())
                .flatten()
                .filter(|(_, span)| span.start <= record);
            let Some((&end, span)) = span else {
                rows.push(estimates.record_rows(record));
                record += 1;
                continue;
            };
            let mut inner: RowIndex = (span.start..record)
                .map(|record| estimates.record_rows(record))
                .sum();
            while record < records.end.min(end) {
                let before = inner.min(span.rows);
                inner = inner.saturating_add(estimates.record_rows(record));
                let through = if record + 1 == end {
                    span.rows
                } else {
                    inner.min(span.rows)
                };
                rows.push(through - before);
                record += 1;
            }
        }
        rows
    }

    /// Record what the projection laid out for whole record spans.
    pub(super) fn observe(
        &mut self,
        estimates: &ExtentEstimates<'_>,
        layout: ExtentLayoutKey,
        spans: impl IntoIterator<Item = (Range<usize>, RowIndex)>,
        keep_near: usize,
    ) {
        if self.layout != Some(layout) {
            self.forget_layout();
            self.layout = Some(layout);
        }
        for (range, rows) in spans {
            if range.start >= range.end {
                continue;
            }
            let estimate = (range.clone())
                .map(|record| estimates.record_rows(record))
                .sum();
            self.insert_span(range, rows, estimate);
        }
        while self.spans.len() > MAX_SPANS {
            self.evict_farthest_chunk(keep_near);
        }
    }

    fn estimate_before(&mut self, estimates: &ExtentEstimates<'_>, record: usize) -> RowIndex {
        let key = (estimates.width, record);
        if let Some(rows) = self.estimate_prefixes.get(&key) {
            return *rows;
        }
        if self.estimate_prefixes.len() >= MAX_ESTIMATE_PREFIXES {
            self.estimate_prefixes.clear();
        }
        let rows = estimates.rows_before(record);
        self.estimate_prefixes.insert(key, rows);
        rows
    }

    fn correction_before(&self, record: usize) -> i64 {
        let chunk = record / CHUNK_RECORDS;
        let whole_chunks: i64 = self
            .chunk_corrections
            .range(..chunk)
            .map(|(_, sum)| sum)
            .sum();
        let partial: i64 = self
            .spans
            .range(chunk * CHUNK_RECORDS..=record)
            .map(|(_, span)| span.correction())
            .sum();
        whole_chunks.saturating_add(partial)
    }

    fn insert_span(&mut self, range: Range<usize>, rows: RowIndex, estimate: RowIndex) {
        let overlapping: Vec<usize> = self
            .spans
            .range(range.start.saturating_add(1)..)
            .take_while(|(_, span)| span.start < range.end)
            .map(|(end, _)| *end)
            .collect();
        for end in overlapping {
            self.remove_span(end);
        }
        let span = ExtentSpan {
            start: range.start,
            rows,
            estimate,
        };
        *self
            .chunk_corrections
            .entry(range.end / CHUNK_RECORDS)
            .or_default() += span.correction();
        self.spans.insert(range.end, span);
    }

    fn remove_span(&mut self, end: usize) {
        let Some(span) = self.spans.remove(&end) else {
            return;
        };
        let chunk = end / CHUNK_RECORDS;
        if let Some(sum) = self.chunk_corrections.get_mut(&chunk) {
            *sum -= span.correction();
        }
        if self
            .spans
            .range(chunk * CHUNK_RECORDS..(chunk + 1) * CHUNK_RECORDS)
            .next()
            .is_none()
        {
            self.chunk_corrections.remove(&chunk);
        }
    }

    fn evict_farthest_chunk(&mut self, keep_near: usize) {
        let keep = keep_near / CHUNK_RECORDS;
        let first = self.chunk_corrections.keys().next().copied();
        let last = self.chunk_corrections.keys().next_back().copied();
        let Some(chunk) = [first, last]
            .into_iter()
            .flatten()
            .max_by_key(|chunk| chunk.abs_diff(keep))
        else {
            return;
        };
        let ends: Vec<usize> = self
            .spans
            .range(chunk * CHUNK_RECORDS..(chunk + 1) * CHUNK_RECORDS)
            .map(|(end, _)| *end)
            .collect();
        for end in ends {
            self.remove_span(end);
        }
    }

    pub(super) fn span_count(&self) -> usize {
        self.spans.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn estimates(records: &SparseTranscriptRecords) -> ExtentEstimates<'_> {
        ExtentEstimates {
            store: None,
            records,
            width: 80,
        }
    }

    fn layout() -> ExtentLayoutKey {
        ExtentLayoutKey {
            width: 80,
            renderer_generation: 0,
            renderer_cache_key: None,
        }
    }

    #[test]
    fn unobserved_records_use_estimates() {
        let records = SparseTranscriptRecords::default();
        let mut extent = TranscriptExtent::default();
        assert_eq!(extent.rows_before(&estimates(&records), 10), 20);
        assert_eq!(
            extent.record_at_row(&estimates(&records), 100, 21),
            Some((10, 1))
        );
    }

    #[test]
    fn observed_spans_shift_later_records_and_invert_exactly() {
        let records = SparseTranscriptRecords::default();
        let estimates = estimates(&records);
        let mut extent = TranscriptExtent::default();
        extent.observe(&estimates, layout(), [(3..4, 12), (4..7, 1)], 0);

        assert_eq!(extent.rows_before(&estimates, 3), 6);
        assert_eq!(extent.rows_before(&estimates, 4), 18);
        assert_eq!(extent.rows_before(&estimates, 7), 19);
        assert_eq!(extent.rows_before(&estimates, 10), 25);
        for row in 0..30 {
            let (record, offset) = extent.record_at_row(&estimates, 20, row).unwrap();
            let first = extent.rows_before(&estimates, record);
            assert!(
                first + offset == row || record == 19,
                "row {row} -> {record}+{offset}"
            );
            assert!(row < extent.rows_before(&estimates, record + 1) || record == 19);
        }
    }

    #[test]
    fn rows_inside_a_group_span_stay_monotonic() {
        let records = SparseTranscriptRecords::default();
        let estimates = estimates(&records);
        let mut extent = TranscriptExtent::default();
        extent.observe(&estimates, layout(), [(2..6, 3)], 0);
        let rows: Vec<_> = (0..8).map(|r| extent.rows_before(&estimates, r)).collect();
        assert!(rows.windows(2).all(|pair| pair[0] <= pair[1]), "{rows:?}");
        assert_eq!(rows[6], rows[2] + 3);
    }

    #[test]
    fn reobserving_replaces_overlapping_spans() {
        let records = SparseTranscriptRecords::default();
        let estimates = estimates(&records);
        let mut extent = TranscriptExtent::default();
        extent.observe(&estimates, layout(), [(2..6, 40)], 0);
        extent.observe(&estimates, layout(), [(4..5, 10)], 0);
        assert_eq!(extent.span_count(), 1);
        assert_eq!(extent.rows_before(&estimates, 8), 16 - 2 + 10);
    }

    #[test]
    fn record_rows_agree_with_rows_before() {
        let records = SparseTranscriptRecords::default();
        let estimates = estimates(&records);
        let mut extent = TranscriptExtent::default();
        extent.observe(&estimates, layout(), [(1..2, 9), (3..6, 3), (7..8, 0)], 0);
        let rows = extent.record_rows(&estimates, 0..10);
        for record in 0..10 {
            let before: RowIndex = rows[..record].iter().sum();
            assert_eq!(before, extent.rows_before(&estimates, record), "{rows:?}");
        }
    }

    #[test]
    fn spans_only_correct_their_own_width() {
        let records = SparseTranscriptRecords::default();
        let mut extent = TranscriptExtent::default();
        extent.observe(&estimates(&records), layout(), [(1..2, 10)], 0);
        let narrow = ExtentEstimates {
            width: 40,
            ..estimates(&records)
        };
        assert_eq!(extent.rows_before(&estimates(&records), 4), 16);
        assert_eq!(extent.rows_before(&narrow, 4), 8);
    }

    #[test]
    fn layout_change_forgets_spans() {
        let records = SparseTranscriptRecords::default();
        let estimates = estimates(&records);
        let mut extent = TranscriptExtent::default();
        extent.observe(&estimates, layout(), [(0..1, 50)], 0);
        let wider = ExtentLayoutKey {
            width: 100,
            ..layout()
        };
        extent.observe(&estimates, wider, [], 0);
        assert_eq!(extent.rows_before(&estimates, 1), 2);
    }

    #[test]
    fn invalidating_drops_rewritten_spans() {
        let records = SparseTranscriptRecords::default();
        let estimates = estimates(&records);
        let mut extent = TranscriptExtent::default();
        extent.observe(&estimates, layout(), [(0..1, 50), (5..6, 50)], 0);
        extent.invalidate_from(5);
        assert_eq!(extent.span_count(), 1);
        assert_eq!(extent.rows_before(&estimates, 10), 50 + 18);
    }

    #[test]
    fn spans_across_chunks_sum_correctly() {
        let records = SparseTranscriptRecords::default();
        let estimates = estimates(&records);
        let mut extent = TranscriptExtent::default();
        let far = CHUNK_RECORDS * 3 + 7;
        extent.observe(&estimates, layout(), [(1..2, 10), (far..far + 1, 5)], 0);
        assert_eq!(
            extent.rows_before(&estimates, far + 1),
            (far as RowIndex + 1) * 2 + 8 + 3
        );
    }
}
