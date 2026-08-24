use super::*;

pub(super) struct SqliteTranscriptStore {
    reader: smelt_store::LineageSessionReader,
    #[cfg(test)]
    extent_read_count: std::cell::Cell<usize>,
}

impl SqliteTranscriptStore {
    pub(super) fn open_read_only(store: &TranscriptStoreAddress) -> smelt_store::Result<Self> {
        let _perf = smelt_perf::perf::begin("transcript:store:open_read_only");
        let reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
            &store.sessions_root,
            &store.lineage_id,
            &store.session_id,
        )?;
        Ok(Self {
            reader,
            #[cfg(test)]
            extent_read_count: std::cell::Cell::new(0),
        })
    }

    pub(super) fn read_tail_record_slice_for_rows(
        &self,
        width: u16,
        target_rows: u16,
    ) -> smelt_store::Result<smelt_store::TranscriptRecordSlice> {
        let total = usize::try_from(self.reader.snapshot()?.transcript_len).map_err(|_| {
            smelt_store::StoreError::Integrity(
                "lineage transcript length exceeds platform limits".into(),
            )
        })?;
        self.reader
            .transcript_tail_for_rows_with_total(total, width, target_rows)
    }

    #[cfg(test)]
    pub(super) fn read_record_slice(
        &self,
        range: smelt_store::TranscriptRecordRange,
        total_count: usize,
    ) -> smelt_store::Result<smelt_store::TranscriptRecordSlice> {
        self.reader
            .transcript_record_slice_with_total(range, total_count)
    }

    pub(super) fn record_index_for_block_idx(
        &self,
        block_idx: u64,
    ) -> smelt_store::Result<Option<usize>> {
        self.reader.transcript_record_index_for_block_idx(block_idx)
    }

    pub(super) fn estimated_record_rows(
        &self,
        width: u16,
        range: Range<usize>,
    ) -> smelt_store::Result<u64> {
        let _perf = smelt_perf::perf::begin("transcript:extent:sqlite_estimated_record_rows");
        smelt_perf::perf::record_value(
            "transcript:extent:sqlite_estimated_record_rows:records",
            range.end.saturating_sub(range.start) as u64,
        );
        #[cfg(test)]
        self.extent_read_count
            .set(self.extent_read_count.get().saturating_add(1));
        self.reader
            .transcript_estimated_rows(range.into(), width.max(1))
    }

    fn hydrate_navigation_record(
        &self,
        navigation: smelt_store::TranscriptNavigationRecord,
    ) -> smelt_store::Result<Option<(usize, smelt_store::StoredTranscriptBlock)>> {
        let index = navigation.record_index.get();
        let mut records = self
            .reader
            .transcript_object_backed_range(index as u64, index.saturating_add(1) as u64)?;
        let Some(record) = records.pop() else {
            return Ok(None);
        };
        if record.block_idx != navigation.profile.block_idx
            || record.kind != navigation.profile.kind
            || !records.is_empty()
        {
            return Err(smelt_store::StoreError::Integrity(
                "transcript navigation profile disagrees with its target record".into(),
            ));
        }
        Ok(Some((index, record)))
    }

    pub(super) fn record_before_kind(
        &self,
        kind: &str,
        before_or_at: usize,
    ) -> smelt_store::Result<Option<(usize, smelt_store::StoredTranscriptBlock)>> {
        let _perf = smelt_perf::perf::begin("transcript:navigation:record_before_kind");
        let Some(navigation) = self
            .reader
            .transcript_record_before_kind(kind, before_or_at)?
        else {
            return Ok(None);
        };
        self.hydrate_navigation_record(navigation)
    }

    pub(super) fn record_after_kind(
        &self,
        kind: &str,
        after_or_at: usize,
    ) -> smelt_store::Result<Option<(usize, smelt_store::StoredTranscriptBlock)>> {
        let Some(navigation) = self
            .reader
            .transcript_record_after_kind(kind, after_or_at)?
        else {
            return Ok(None);
        };
        self.hydrate_navigation_record(navigation)
    }

    pub(super) fn total_estimated_record_rows(&self, width: u16) -> smelt_store::Result<u64> {
        #[cfg(test)]
        self.extent_read_count
            .set(self.extent_read_count.get().saturating_add(1));
        self.reader.transcript_total_estimated_rows(width.max(1))
    }

    pub(super) fn record_for_row(
        &self,
        width: u16,
        row: RowIndex,
    ) -> smelt_store::Result<Option<smelt_store::TranscriptRowLocation>> {
        #[cfg(test)]
        self.extent_read_count
            .set(self.extent_read_count.get().saturating_add(1));
        self.reader.transcript_record_for_row(width.max(1), row)
    }

    #[cfg(test)]
    pub(super) fn extent_read_count(&self) -> usize {
        self.extent_read_count.get()
    }
}

#[derive(Default)]
pub(super) struct TranscriptStoreCache {
    pub(super) store: Option<(TranscriptStoreAddress, SqliteTranscriptStore)>,
    #[cfg(test)]
    pub(super) open_attempt_count: usize,
    #[cfg(test)]
    pub(super) payload_read_count: usize,
}

impl TranscriptStoreCache {
    pub(super) fn cached_store_for_session(
        &self,
        store_address: &TranscriptStoreAddress,
    ) -> Option<&SqliteTranscriptStore> {
        self.store
            .as_ref()
            .filter(|(open_dir, _)| open_dir == store_address)
            .map(|(_, store)| store)
    }

    pub(super) fn store_for_session(
        &mut self,
        store_address: Option<&TranscriptStoreAddress>,
    ) -> Option<&SqliteTranscriptStore> {
        let store_address = store_address?.clone();
        let needs_open = self
            .store
            .as_ref()
            .is_none_or(|(open_dir, _)| open_dir != &store_address);
        if needs_open {
            #[cfg(test)]
            {
                self.open_attempt_count = self.open_attempt_count.saturating_add(1);
            }
            let store = SqliteTranscriptStore::open_read_only(&store_address).ok()?;
            self.store = Some((store_address, store));
        }
        self.store.as_ref().map(|(_, store)| store)
    }

    #[cfg(test)]
    pub(super) fn read_record_slice(
        &mut self,
        store_address: Option<&TranscriptStoreAddress>,
        range: smelt_store::TranscriptRecordRange,
        total_count: usize,
    ) -> Option<smelt_store::TranscriptRecordSlice> {
        self.payload_read_count = self.payload_read_count.saturating_add(1);
        self.store_for_session(store_address)?
            .read_record_slice(range, total_count)
            .ok()
    }
}
