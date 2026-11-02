use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(test)]
use std::thread;
use std::time::Instant;

use smelt_core::transcript_model::{
    BlockId, StoredBlockRef, StoredBlockWithId, TranscriptBlockRecordWithId,
};

use super::latest_worker::{LatestWorker, Mailbox};
use super::transcript::TranscriptStoreAddress;

type MaterializedBlockRecords =
    BTreeMap<BlockId, (Arc<StoredBlockRef>, TranscriptBlockRecordWithId)>;

pub(crate) struct LoadedRecordWindow {
    pub(crate) start: smelt_store::TranscriptRecordOffset,
    pub(crate) total_count: usize,
    pub(crate) hydration: smelt_store::TranscriptRecordHydration,
    pub(crate) records: Vec<StoredBlockWithId>,
}

impl std::fmt::Debug for LoadedRecordWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedRecordWindow")
            .field("start", &self.start)
            .field("total_count", &self.total_count)
            .field("hydration", &self.hydration)
            .field("record_count", &self.records.len())
            .finish()
    }
}

impl LoadedRecordWindow {
    pub(crate) fn end(&self) -> smelt_store::TranscriptRecordOffset {
        smelt_store::TranscriptRecordOffset::new(
            self.start.get().saturating_add(self.records.len()),
        )
    }

    pub(super) fn from_slice(slice: smelt_store::TranscriptRecordSlice) -> Option<Self> {
        Self::from_slice_materializing(slice, &HashSet::new()).map(|(window, _)| window)
    }

    fn from_slice_materializing(
        slice: smelt_store::TranscriptRecordSlice,
        materialize_block_ids: &HashSet<BlockId>,
    ) -> Option<(Self, MaterializedBlockRecords)> {
        if slice.is_empty() {
            return None;
        }
        let start = slice.start;
        let total_count = slice.total_count;
        let hydration = slice.hydration;
        let mut materialized = BTreeMap::new();
        let records = {
            let _perf = smelt_perf::perf::begin("transcript:record_window:compact_records");
            smelt_perf::perf::record_value(
                "transcript:record_window:decode_rows",
                slice.len() as u64,
            );
            slice
                .into_records()
                .into_iter()
                .enumerate()
                .map(|(offset, row)| {
                    let (block_id, stored, record) =
                        stored_block_from_row(start.get().saturating_add(offset), row)?;
                    if materialize_block_ids.contains(&block_id) {
                        materialized.insert(block_id, (Arc::clone(&stored), record));
                    }
                    Some(StoredBlockWithId { block_id, stored })
                })
                .collect::<Option<Vec<_>>>()?
        };
        Some((
            Self {
                start,
                total_count,
                hydration,
                records,
            },
            materialized,
        ))
    }
}

fn stored_block_from_row(
    record_index: usize,
    mut row: smelt_store::StoredTranscriptBlock,
) -> Option<(BlockId, Arc<StoredBlockRef>, TranscriptBlockRecordWithId)> {
    let indexed_text = std::mem::take(&mut row.indexed_text);
    let estimated_text_bytes = row.estimated_text_bytes;
    let record = TranscriptBlockRecordWithId::try_from(row).ok()?;
    let (block_id, stored) = StoredBlockRef::from_record(
        record_index,
        record.block_id,
        &record.record,
        &indexed_text,
        estimated_text_bytes,
    );
    Some((block_id, stored, record))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingRecordHydration {
    pub(super) projection_range: Range<smelt_store::TranscriptRecordOffset>,
    pub(super) cache_ranges: Vec<Range<smelt_store::TranscriptRecordOffset>>,
    pub(super) materialize_block_ids: HashSet<BlockId>,
}

#[derive(Default)]
pub(super) struct TranscriptHydrationQueue {
    record: Option<PendingRecordHydration>,
    record_semantic: bool,
    blocks: BTreeMap<BlockId, Arc<StoredBlockRef>>,
    revision: u64,
    dispatched_revision: u64,
}

impl TranscriptHydrationQueue {
    pub(super) fn request_record_ranges(
        &mut self,
        projection_range: Range<smelt_store::TranscriptRecordOffset>,
        cache_ranges: Vec<Range<smelt_store::TranscriptRecordOffset>>,
    ) {
        self.request_record_ranges_inner(projection_range, cache_ranges, None);
    }

    pub(super) fn request_semantic_record_ranges(
        &mut self,
        projection_range: Range<smelt_store::TranscriptRecordOffset>,
        cache_ranges: Vec<Range<smelt_store::TranscriptRecordOffset>>,
        materialize_block_id: BlockId,
    ) {
        self.request_record_ranges_inner(
            projection_range,
            cache_ranges,
            Some(materialize_block_id),
        );
    }

    fn request_record_ranges_inner(
        &mut self,
        mut projection_range: Range<smelt_store::TranscriptRecordOffset>,
        mut cache_ranges: Vec<Range<smelt_store::TranscriptRecordOffset>>,
        materialize_block_id: Option<BlockId>,
    ) {
        cache_ranges.retain(|range| range.start < range.end);
        let previous_record = self.record.clone();
        let previous_semantic = self.record_semantic;
        let semantic = materialize_block_id.is_some();
        let mut materialize_block_ids = materialize_block_id.into_iter().collect::<HashSet<_>>();
        let mut retain_semantic = semantic;
        if let Some(previous) = self.record.take() {
            let adjacent = previous.cache_ranges.iter().any(|left| {
                cache_ranges
                    .iter()
                    .any(|right| left.start <= right.end && right.start <= left.end)
            });
            if self.record_semantic && !semantic {
                projection_range = previous.projection_range.clone();
                cache_ranges.extend(previous.cache_ranges);
                materialize_block_ids.extend(previous.materialize_block_ids);
                retain_semantic = true;
            } else if adjacent {
                cache_ranges.extend(previous.cache_ranges);
                materialize_block_ids.extend(previous.materialize_block_ids);
            }
        }
        cache_ranges.sort_unstable_by_key(|range| range.start);
        let mut merged = Vec::<Range<smelt_store::TranscriptRecordOffset>>::new();
        for range in cache_ranges {
            match merged.last_mut() {
                Some(previous) if range.start <= previous.end => {
                    previous.end = previous.end.max(range.end);
                }
                _ => merged.push(range),
            }
        }
        self.record = Some(PendingRecordHydration {
            projection_range,
            cache_ranges: merged,
            materialize_block_ids,
        });
        self.record_semantic = retain_semantic;
        if self.record != previous_record || self.record_semantic != previous_semantic {
            self.bump_revision();
        }
    }

    pub(super) fn request_blocks(
        &mut self,
        blocks: impl IntoIterator<Item = (BlockId, Arc<StoredBlockRef>)>,
    ) {
        let mut changed = false;
        for (id, stored) in blocks {
            let already_requested = self
                .blocks
                .get(&id)
                .is_some_and(|previous| Arc::ptr_eq(previous, &stored));
            if !already_requested {
                self.blocks.insert(id, stored);
                changed = true;
            }
        }
        if changed {
            self.bump_revision();
        }
    }

    pub(super) fn request(
        &mut self,
        context_id: u64,
        store_address: Option<&TranscriptStoreAddress>,
        total_count: usize,
    ) -> Option<TranscriptHydrationRequest> {
        if self.is_empty() || self.revision == self.dispatched_revision {
            return None;
        }
        let store_address = store_address?.clone();
        self.dispatched_revision = self.revision;
        Some(TranscriptHydrationRequest {
            worker_generation: 0,
            context_id,
            revision: self.revision,
            store_address,
            total_count,
            record: self.record.clone(),
            blocks: self.blocks.clone(),
        })
    }

    pub(super) fn awaits_record(&self, result: &TranscriptHydrationWorkerResult) -> bool {
        self.record.as_ref() == result.record.as_ref()
    }

    pub(super) fn complete(&mut self, result: &TranscriptHydrationWorkerResult) {
        if result.revision != self.revision {
            return;
        }
        if self.record.as_ref() == result.record.as_ref() {
            self.record = None;
            self.record_semantic = false;
        }
        for id in &result.requested_block_ids {
            self.blocks.remove(id);
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.record.is_none() && self.blocks.is_empty()
    }

    pub(super) fn cancel_record(&mut self) {
        if self.record.take().is_some() {
            self.record_semantic = false;
            self.bump_revision();
        }
    }

    pub(super) fn record_is_pending(&self) -> bool {
        self.record.is_some()
    }

    pub(super) fn redispatch(&mut self) {
        self.dispatched_revision = 0;
    }

    fn bump_revision(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("transcript hydration revision overflow");
    }
}

#[derive(Clone)]
pub(super) struct TranscriptHydrationRequest {
    worker_generation: u64,
    pub(super) context_id: u64,
    pub(super) revision: u64,
    pub(super) store_address: TranscriptStoreAddress,
    pub(super) total_count: usize,
    pub(super) record: Option<PendingRecordHydration>,
    pub(super) blocks: BTreeMap<BlockId, Arc<StoredBlockRef>>,
}

#[derive(Debug)]
pub struct TranscriptHydrationWorkerResult {
    worker_generation: u64,
    pub(super) context_id: u64,
    pub(super) revision: u64,
    pub(super) store_address: TranscriptStoreAddress,
    pub(super) record: Option<PendingRecordHydration>,
    pub(super) record_windows: Vec<LoadedRecordWindow>,
    pub(super) record_complete: bool,
    pub(super) requested_block_ids: Vec<BlockId>,
    pub(super) hydrated_blocks: Vec<(BlockId, Arc<StoredBlockRef>, TranscriptBlockRecordWithId)>,
    pub(super) failed_block_ids: Vec<BlockId>,
    pub(super) blocks_complete: bool,
    pub(super) hydration_ranges: u64,
    pub(super) duration_us: u64,
}

#[cfg(test)]
#[derive(Default)]
struct HydrationWorkerMetrics {
    retained_reader_count: AtomicU64,
    open_attempt_count: AtomicU64,
}

pub(super) struct TranscriptHydrationWorker {
    worker: LatestWorker<TranscriptHydrationRequest>,
    context_id: AtomicU64,
    #[cfg(test)]
    metrics: Arc<HydrationWorkerMetrics>,
}

impl TranscriptHydrationWorker {
    pub(super) fn spawn(
        event_tx: tokio::sync::mpsc::UnboundedSender<crate::app::AppEvent>,
    ) -> Self {
        #[cfg(test)]
        let metrics = Arc::new(HydrationWorkerMetrics::default());
        #[cfg(test)]
        let worker_metrics = Arc::clone(&metrics);
        let worker = LatestWorker::start("smelt-transcript-hydration", move |mailbox| {
            transcript_hydration_worker_loop(
                mailbox,
                event_tx,
                #[cfg(test)]
                worker_metrics,
            )
        });
        Self {
            worker,
            context_id: AtomicU64::new(0),
            #[cfg(test)]
            metrics,
        }
    }

    pub(super) fn set_context(&self, context_id: u64) {
        if self.context_id.swap(context_id, Ordering::AcqRel) != context_id {
            self.worker.cancel();
        }
    }

    pub(super) fn request(&self, request: TranscriptHydrationRequest) {
        self.set_context(request.context_id);
        self.worker.request(request);
    }

    pub(super) fn is_current(&self, result: &TranscriptHydrationWorkerResult) -> bool {
        self.context_id.load(Ordering::Acquire) == result.context_id
            && self.worker.is_current(result.worker_generation)
    }

    #[cfg(test)]
    pub(super) fn set_delay(&self, delay: std::time::Duration) {
        self.worker.set_delay(delay);
    }

    #[cfg(test)]
    pub(super) fn retained_reader_count(&self) -> usize {
        usize::try_from(self.metrics.retained_reader_count.load(Ordering::Acquire))
            .unwrap_or(usize::MAX)
    }

    #[cfg(test)]
    pub(super) fn open_attempt_count(&self) -> usize {
        usize::try_from(self.metrics.open_attempt_count.load(Ordering::Acquire))
            .unwrap_or(usize::MAX)
    }
}

fn transcript_hydration_worker_loop(
    mailbox: Arc<Mailbox<TranscriptHydrationRequest>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<crate::app::AppEvent>,
    #[cfg(test)] metrics: Arc<HydrationWorkerMetrics>,
) {
    let mut retained_store: Option<(TranscriptStoreAddress, smelt_store::LineageSessionReader)> =
        None;
    while let Some((token, mut request)) = mailbox.recv() {
        request.worker_generation = token.generation;

        let needs_open = retained_store
            .as_ref()
            .is_none_or(|(address, _)| address != &request.store_address);
        if needs_open {
            drop(retained_store.take());
            #[cfg(test)]
            metrics.retained_reader_count.store(0, Ordering::Release);
            #[cfg(test)]
            metrics.open_attempt_count.fetch_add(1, Ordering::AcqRel);
            let reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
                &request.store_address.sessions_root,
                &request.store_address.lineage_id,
                &request.store_address.session_id,
            );
            match reader {
                Ok(reader) => {
                    retained_store = Some((request.store_address.clone(), reader));
                    #[cfg(test)]
                    metrics.retained_reader_count.store(1, Ordering::Release);
                }
                Err(_) => {
                    if send_result(
                        &event_tx,
                        failed_hydration_result(request, false, false, 0, Instant::now()),
                    )
                    .is_err()
                    {
                        break;
                    }
                    continue;
                }
            }
        }
        let reader = &retained_store.as_ref().expect("opened hydration reader").1;
        let result = execute_hydration_request(reader, request, || token.cancelled());
        let Some(result) = result else {
            continue;
        };
        if send_result(&event_tx, result).is_err() {
            break;
        }
    }
    drop(retained_store);
    #[cfg(test)]
    metrics.retained_reader_count.store(0, Ordering::Release);
}

pub(super) fn execute_hydration_request(
    reader: &smelt_store::LineageSessionReader,
    request: TranscriptHydrationRequest,
    cancelled: impl Fn() -> bool,
) -> Option<TranscriptHydrationWorkerResult> {
    let started_at = Instant::now();
    let requested_block_ids = request.blocks.keys().copied().collect::<Vec<_>>();
    let mut materialize_block_ids = requested_block_ids.iter().copied().collect::<HashSet<_>>();
    if let Some(record) = request.record.as_ref() {
        materialize_block_ids.extend(record.materialize_block_ids.iter().copied());
    }

    let mut hydrated_by_id = BTreeMap::new();
    let mut record_windows = Vec::new();
    let mut record_complete = request.record.is_none();
    let mut hydration_ranges = 0_u64;
    if let Some(record) = request.record.as_ref() {
        record_complete = true;
        for range in &record.cache_ranges {
            if cancelled() {
                return None;
            }
            hydration_ranges = hydration_ranges.saturating_add(1);
            let slice = reader.transcript_record_slice_with_total(
                smelt_store::TranscriptRecordRange::new(range.start, range.end),
                request.total_count,
            );
            let Some((window, materialized)) = slice.ok().and_then(|slice| {
                LoadedRecordWindow::from_slice_materializing(slice, &materialize_block_ids)
            }) else {
                record_complete = false;
                break;
            };
            for (block_id, (stored, record)) in materialized {
                let stored = request.blocks.get(&block_id).cloned().unwrap_or(stored);
                hydrated_by_id.insert(block_id, (stored, record));
            }
            record_windows.push(window);
        }
    }

    // Saves can move records, so blocks are read by id and their known index is only a hint.
    let lookups = materialize_block_ids
        .iter()
        .filter(|block_id| !hydrated_by_id.contains_key(*block_id))
        .map(|block_id| smelt_store::TranscriptBlockLookup {
            block_idx: block_id.get(),
            record_index_hint: request
                .blocks
                .get(block_id)
                .map(|stored| stored.record_index),
        })
        .collect::<Vec<_>>();
    if !lookups.is_empty() {
        hydration_ranges = hydration_ranges.saturating_add(block_lookup_ranges(&lookups));
        match reader.transcript_blocks_by_id(&lookups, &cancelled) {
            Ok(rows) => {
                for (record_index, row) in rows {
                    let Some((block_id, stored, record)) = stored_block_from_row(record_index, row)
                    else {
                        continue;
                    };
                    let stored = request.blocks.get(&block_id).cloned().unwrap_or(stored);
                    hydrated_by_id.insert(block_id, (stored, record));
                }
            }
            Err(smelt_store::StoreError::Cancelled) => return None,
            Err(_) => {}
        }
    }
    let hydrated_blocks = hydrated_by_id
        .into_iter()
        .map(|(block_id, (stored, record))| (block_id, stored, record))
        .collect::<Vec<_>>();
    if cancelled() {
        return None;
    }
    let hydrated_block_ids = hydrated_blocks
        .iter()
        .map(|(block_id, _, _)| *block_id)
        .collect::<HashSet<_>>();
    let failed_block_ids = materialize_block_ids
        .into_iter()
        .filter(|block_id| !hydrated_block_ids.contains(block_id))
        .collect::<Vec<_>>();
    let blocks_complete = failed_block_ids.is_empty();
    Some(TranscriptHydrationWorkerResult {
        worker_generation: request.worker_generation,
        context_id: request.context_id,
        revision: request.revision,
        store_address: request.store_address,
        record: request.record,
        record_windows,
        record_complete,
        requested_block_ids,
        hydrated_blocks,
        failed_block_ids,
        blocks_complete,
        hydration_ranges,
        duration_us: u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
    })
}

/// Ranges a by-id read takes when every hint holds: one per run of adjacent hints, and one per
/// block without a hint.
fn block_lookup_ranges(lookups: &[smelt_store::TranscriptBlockLookup]) -> u64 {
    let mut hints = lookups
        .iter()
        .filter_map(|lookup| lookup.record_index_hint)
        .collect::<Vec<_>>();
    hints.sort_unstable();
    hints.dedup();
    let runs = hints
        .iter()
        .enumerate()
        .filter(|(position, index)| *position == 0 || hints[position - 1] + 1 != **index)
        .count();
    let unhinted = lookups
        .iter()
        .filter(|lookup| lookup.record_index_hint.is_none())
        .count();
    (runs + unhinted) as u64
}

pub(super) fn failed_hydration_result(
    request: TranscriptHydrationRequest,
    record_complete: bool,
    blocks_complete: bool,
    hydration_ranges: u64,
    started_at: Instant,
) -> TranscriptHydrationWorkerResult {
    let requested_block_ids = request.blocks.keys().copied().collect();
    let failed_block_ids = if blocks_complete {
        Vec::new()
    } else {
        let mut failed = request.blocks.keys().copied().collect::<HashSet<_>>();
        if let Some(record) = request.record.as_ref() {
            failed.extend(record.materialize_block_ids.iter().copied());
        }
        failed.into_iter().collect()
    };
    TranscriptHydrationWorkerResult {
        worker_generation: request.worker_generation,
        context_id: request.context_id,
        revision: request.revision,
        store_address: request.store_address,
        record: request.record,
        record_windows: Vec::new(),
        record_complete,
        requested_block_ids,
        hydrated_blocks: Vec::new(),
        failed_block_ids,
        blocks_complete,
        hydration_ranges,
        duration_us: u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
    }
}

#[cfg(test)]
struct TranscriptHydrationTestService {
    worker: TranscriptHydrationWorker,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<crate::app::AppEvent>,
}

#[cfg(test)]
thread_local! {
    static TEST_HYDRATION_SERVICE: std::cell::RefCell<Option<TranscriptHydrationTestService>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn execute_hydration_request_for_test(
    request: TranscriptHydrationRequest,
) -> Option<TranscriptHydrationWorkerResult> {
    TEST_HYDRATION_SERVICE.with(|service| {
        let mut service = service.borrow_mut();
        let service = service.get_or_insert_with(|| {
            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
            TranscriptHydrationTestService {
                worker: TranscriptHydrationWorker::spawn(event_tx),
                event_rx,
            }
        });
        service.worker.request(request);

        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match service.event_rx.try_recv() {
                Ok(crate::app::AppEvent::TranscriptHydrationCompleted(result)) => {
                    return service.worker.is_current(&result).then_some(*result);
                }
                Ok(_) => return None,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
            }
        }
    })
}

fn send_result(
    event_tx: &tokio::sync::mpsc::UnboundedSender<crate::app::AppEvent>,
    result: TranscriptHydrationWorkerResult,
) -> Result<(), ()> {
    event_tx
        .send(crate::app::AppEvent::TranscriptHydrationCompleted(
            Box::new(result),
        ))
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_store(root: &std::path::Path, marker: char) -> TranscriptStoreAddress {
        let session_id = marker.to_string().repeat(64);
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.id.clone_from(&session_id);
        let mut command = smelt_core::session::initial_store_commit_from_session(&session).unwrap();
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        for index in 0..32 {
            transcript.push(smelt_core::transcript_model::Block::Text {
                content: format!("{marker} record {index}").into(),
            });
        }
        let records = transcript
            .history
            .block_records()
            .iter()
            .enumerate()
            .map(|(index, record)| {
                smelt_core::transcript_model::transcript_block_row_with_block_idx(
                    index,
                    index as u64,
                    record,
                )
                .unwrap()
            })
            .collect();
        command.transcript_records = Some(smelt_store::TranscriptRecordSuffix {
            start: smelt_store::TranscriptRecordIndex::ZERO,
            records,
        });
        let mut writer = smelt_store::OwnedLineageWriter::open(root, &session_id).unwrap();
        writer.commit_session(&command).unwrap();
        let lineage_id = writer.lineage_id().to_string();
        writer.release().unwrap();
        TranscriptStoreAddress::new(root.to_path_buf(), session_id, lineage_id)
    }

    fn record_request(
        context_id: u64,
        store_address: TranscriptStoreAddress,
        range: Range<usize>,
    ) -> TranscriptHydrationRequest {
        let range = smelt_store::TranscriptRecordOffset::new(range.start)
            ..smelt_store::TranscriptRecordOffset::new(range.end);
        TranscriptHydrationRequest {
            worker_generation: 0,
            context_id,
            revision: 1,
            store_address,
            total_count: 32,
            record: Some(PendingRecordHydration {
                projection_range: range.clone(),
                cache_ranges: vec![range],
                materialize_block_ids: HashSet::new(),
            }),
            blocks: BTreeMap::new(),
        }
    }

    fn receive_hydration_event(
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<crate::app::AppEvent>,
    ) -> TranscriptHydrationWorkerResult {
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match receiver.try_recv() {
                Ok(crate::app::AppEvent::TranscriptHydrationCompleted(result)) => return *result,
                Ok(other) => panic!("unexpected app event: {other:?}"),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "hydration worker timed out");
                    thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    panic!("hydration worker disconnected")
                }
            }
        }
    }

    #[test]
    fn semantic_record_request_survives_a_following_viewport_request() {
        let semantic = smelt_store::TranscriptRecordOffset::new(4)
            ..smelt_store::TranscriptRecordOffset::new(36);
        let viewport = smelt_store::TranscriptRecordOffset::new(168)
            ..smelt_store::TranscriptRecordOffset::new(200);
        let mut queue = TranscriptHydrationQueue::default();
        queue.request_semantic_record_ranges(
            semantic.clone(),
            vec![semantic.clone()],
            BlockId::new(4),
        );
        queue.request_record_ranges(viewport.clone(), vec![viewport.clone()]);
        let address = TranscriptStoreAddress::new(
            std::path::PathBuf::from("/tmp/smelt-hydration-test"),
            "session".to_owned(),
            "lineage".to_owned(),
        );

        let request = queue.request(1, Some(&address), 200).unwrap();
        let record = request.record.unwrap();

        assert_eq!(record.projection_range, semantic);
        assert_eq!(record.cache_ranges, vec![semantic, viewport]);
        assert_eq!(
            record.materialize_block_ids,
            HashSet::from([BlockId::new(4)])
        );
    }

    #[test]
    fn record_window_read_materializes_an_overlapping_block_without_a_second_range() {
        let root = tempfile::tempdir().unwrap();
        let address = seed_store(root.path(), 'e');
        let reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
            &address.sessions_root,
            &address.lineage_id,
            &address.session_id,
        )
        .unwrap();
        let mut row = reader
            .transcript_record_slice_with_total((5..6).into(), 32)
            .unwrap()
            .into_records()
            .pop()
            .unwrap();
        let indexed_text = std::mem::take(&mut row.indexed_text);
        let estimated_text_bytes = row.estimated_text_bytes;
        let record = TranscriptBlockRecordWithId::try_from(row).unwrap();
        let (block_id, stored) = StoredBlockRef::from_record(
            5,
            record.block_id,
            &record.record,
            &indexed_text,
            estimated_text_bytes,
        );
        let mut request = record_request(1, address, 0..10);
        request.blocks.insert(block_id, stored);

        let result = execute_hydration_request(&reader, request, || false).unwrap();

        assert_eq!(result.hydration_ranges, 1);
        assert_eq!(result.record_windows.len(), 1);
        assert_eq!(result.hydrated_blocks.len(), 1);
        assert_eq!(result.hydrated_blocks[0].0, block_id);
        assert!(result.blocks_complete);
    }

    #[test]
    fn worker_supersedes_a_stale_disjoint_viewport_request() {
        let root = tempfile::tempdir().unwrap();
        let address = seed_store(root.path(), 'a');
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = TranscriptHydrationWorker::spawn(event_tx);
        worker.set_delay(std::time::Duration::from_millis(60));
        worker.request(record_request(7, address.clone(), 0..2));
        thread::sleep(std::time::Duration::from_millis(10));
        worker.request(record_request(7, address, 20..22));

        let result = receive_hydration_event(&mut event_rx);

        assert!(worker.is_current(&result));
        let record = result.record.expect("record hydration result");
        assert_eq!(record.projection_range.start.get(), 20);
        assert_eq!(record.projection_range.end.get(), 22);
        thread::sleep(std::time::Duration::from_millis(80));
        assert!(matches!(
            event_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn worker_cancels_inflight_io_when_the_session_context_changes() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let first = seed_store(first_root.path(), 'b');
        let second = seed_store(second_root.path(), 'c');
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = TranscriptHydrationWorker::spawn(event_tx);
        worker.set_delay(std::time::Duration::from_millis(60));
        worker.request(record_request(11, first, 0..2));
        thread::sleep(std::time::Duration::from_millis(10));
        worker.request(record_request(12, second.clone(), 8..10));

        let result = receive_hydration_event(&mut event_rx);

        assert!(worker.is_current(&result));
        assert_eq!(result.context_id, 12);
        assert_eq!(result.store_address, second);
        assert_eq!(worker.retained_reader_count(), 1);
        assert_eq!(worker.open_attempt_count(), 1);
    }

    #[test]
    fn worker_reuses_one_reader_for_sequential_requests_to_one_store() {
        let root = tempfile::tempdir().unwrap();
        let address = seed_store(root.path(), 'd');
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = TranscriptHydrationWorker::spawn(event_tx);

        worker.request(record_request(19, address.clone(), 0..2));
        let first = receive_hydration_event(&mut event_rx);
        assert!(worker.is_current(&first));
        worker.request(record_request(19, address, 2..4));
        let second = receive_hydration_event(&mut event_rx);

        assert!(worker.is_current(&second));
        assert_eq!(worker.retained_reader_count(), 1);
        assert_eq!(worker.open_attempt_count(), 1);
        assert!(first.record_complete);
        assert!(second.record_complete);
    }

    #[test]
    fn worker_counts_failed_opens_and_releases_the_previous_reader() {
        let root = tempfile::tempdir().unwrap();
        let address = seed_store(root.path(), 'f');
        let missing = TranscriptStoreAddress::new(
            root.path().join("missing"),
            "0".repeat(64),
            "missing-lineage".to_owned(),
        );
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = TranscriptHydrationWorker::spawn(event_tx);

        worker.request(record_request(23, address, 0..2));
        let first = receive_hydration_event(&mut event_rx);
        assert!(worker.is_current(&first));
        assert_eq!(worker.retained_reader_count(), 1);
        assert_eq!(worker.open_attempt_count(), 1);

        worker.request(record_request(23, missing, 2..4));
        let failed = receive_hydration_event(&mut event_rx);

        assert!(worker.is_current(&failed));
        assert!(!failed.record_complete);
        assert_eq!(worker.retained_reader_count(), 0);
        assert_eq!(worker.open_attempt_count(), 2);
    }
}
