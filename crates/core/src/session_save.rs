#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSaveSkipReason {
    EmptyUnstored,
    Unchanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentGeneration {
    pub session: u64,
    pub transcript_descriptors: u64,
}

impl DocumentGeneration {
    pub const fn new(session: u64, transcript_descriptors: u64) -> Self {
        Self {
            session,
            transcript_descriptors,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSavePlan {
    Skip(SessionSaveSkipReason),
    MetadataOnly {
        generation: DocumentGeneration,
    },
    History {
        generation: DocumentGeneration,
        history_start_idx: usize,
        dirty_history_from: Option<usize>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionSaveState {
    pub generation: DocumentGeneration,
    pub store_ready: bool,
    pub descriptors_persisted: bool,
    pub session_dirty: bool,
    pub dirty_history_from: Option<usize>,
    pub descriptor_dirty_from: Option<usize>,
    pub history_len: usize,
    pub durable_history_len: usize,
    pub supports_metadata_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentDirtyState {
    pub pending_or_queued_save: bool,
    pub session_dirty: bool,
    pub dirty_history_from: Option<usize>,
    pub descriptor_dirty_from: Option<usize>,
    pub store_ready: bool,
    pub descriptors_persisted: bool,
    pub history_len: usize,
    pub transcript_empty: bool,
}

impl Default for DocumentDirtyState {
    fn default() -> Self {
        Self {
            pending_or_queued_save: false,
            session_dirty: false,
            dirty_history_from: None,
            descriptor_dirty_from: None,
            store_ready: true,
            descriptors_persisted: true,
            history_len: 0,
            transcript_empty: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveAckPlan {
    MarkClean { clear_descriptors: bool },
    SaveAgain,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SaveFailurePlan {
    pub reconcile_descriptor_len: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSaveKind {
    History,
    Metadata,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DescriptorAppendSubmission {
    pub count: usize,
    pub had_descriptor_total: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmittedHistoryRange {
    pub start_idx: usize,
    pub len: usize,
}

impl SubmittedHistoryRange {
    pub const fn new(start_idx: usize, len: usize) -> Self {
        Self { start_idx, len }
    }

    pub fn from_history_suffix(suffix: &smelt_store::HistorySuffix) -> Self {
        Self {
            start_idx: suffix
                .start
                .as_usize()
                .expect("prepared history start originated as usize"),
            len: suffix
                .final_len
                .as_usize()
                .expect("prepared history length originated as usize"),
        }
    }

    pub fn metadata_only(history_len: u64) -> Self {
        let len = usize::try_from(history_len)
            .expect("prepared metadata history length originated as usize");
        Self {
            start_idx: len,
            len,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingSessionSave {
    save_id: u64,
    session_id: String,
    kind: SessionSaveKind,
    generation: DocumentGeneration,
    history: SubmittedHistoryRange,
    descriptor_append: Option<DescriptorAppendSubmission>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PersistBase {
    pub revision: u64,
    pub history_len: usize,
    pub descriptor_len: usize,
}

#[derive(Clone)]
pub struct PersistDelta {
    pub identity: smelt_store::SessionIdentity,
    pub metadata: smelt_store::SessionMetadata,
    pub history: smelt_store::HistorySuffix,
    pub side_tables: smelt_store::SideTableSuffixes,
    pub descriptors: Option<PersistDescriptorDelta>,
}

#[derive(Clone)]
pub struct PersistDescriptorDelta {
    pub start_descriptor_idx: usize,
    pub records: Vec<crate::TranscriptBlockRecordWithId>,
}

#[derive(Clone, Debug)]
pub struct SubmittedSessionCommit {
    pub command: smelt_store::SessionCommit,
}

fn history_commit_from_delta(
    save_id: u64,
    session_id: String,
    base: PersistBase,
    delta: PersistDelta,
) -> Result<smelt_store::SessionCommit, String> {
    let descriptors = delta
        .descriptors
        .as_ref()
        .map(|descriptors| {
            let records = descriptors
                .records
                .iter()
                .enumerate()
                .map(|(offset, record)| {
                    transcript_descriptor_row(
                        descriptors.start_descriptor_idx + offset,
                        record,
                        &delta.history,
                    )
                })
                .collect::<Result<Vec<_>, smelt_store::StoreError>>()
                .map_err(|err| format!("prepare transcript descriptors: {err}"))?;
            Ok::<_, String>(smelt_store::TranscriptDescriptorSuffix {
                start: smelt_store::DescriptorIndex::new(descriptors.start_descriptor_idx as u64),
                records,
            })
        })
        .transpose()?;
    Ok(smelt_store::SessionCommit {
        session_id,
        save_id: smelt_store::SaveId::new(save_id),
        expected: store_head_from_base(base),
        identity: delta.identity,
        metadata: delta.metadata,
        history: delta.history,
        side_tables: delta.side_tables,
        descriptors,
    })
}

fn metadata_commit(
    save_id: u64,
    session_id: String,
    base: PersistBase,
    identity: smelt_store::SessionIdentity,
    metadata: smelt_store::SessionMetadata,
    side_tables: smelt_store::SideTableSuffixes,
) -> smelt_store::SessionCommit {
    smelt_store::SessionCommit {
        session_id,
        save_id: smelt_store::SaveId::new(save_id),
        expected: store_head_from_base(base),
        identity,
        metadata,
        history: smelt_store::HistorySuffix {
            start: smelt_store::HistoryIndex::new(base.history_len as u64),
            final_len: smelt_store::HistoryLen::new(base.history_len as u64),
            items: Vec::new(),
        },
        side_tables,
        descriptors: None,
    }
}

fn store_head_from_base(base: PersistBase) -> smelt_store::StoreHead {
    smelt_store::StoreHead {
        revision: smelt_store::Revision::new(base.revision),
        history_len: smelt_store::HistoryLen::new(base.history_len as u64),
        descriptor_len: smelt_store::DescriptorLen::new(base.descriptor_len as u64),
    }
}

fn transcript_descriptor_row(
    descriptor_idx: usize,
    record: &crate::TranscriptBlockRecordWithId,
    history: &smelt_store::HistorySuffix,
) -> Result<smelt_store::TranscriptDescriptorRecord, smelt_store::StoreError> {
    let owned_record;
    let record_ref = match record.record.origin {
        Some(crate::BlockOrigin::History(idx))
            if !history_suffix_contains_matching_descriptor_origin(history, idx, record) =>
        {
            owned_record = crate::TranscriptBlockRecord {
                origin: None,
                ..record.record.clone()
            };
            &owned_record
        }
        _ => &record.record,
    };
    crate::transcript_model::transcript_descriptor_row_with_block_idx(
        descriptor_idx,
        record.block_id.get(),
        record_ref,
    )
}

fn history_suffix_contains_matching_descriptor_origin(
    history: &smelt_store::HistorySuffix,
    history_idx: usize,
    record: &crate::TranscriptBlockRecordWithId,
) -> bool {
    let history_start = history
        .start
        .as_usize()
        .expect("prepared history start originated as usize");
    let history_len = history
        .final_len
        .as_usize()
        .expect("prepared history length originated as usize");
    if history_idx >= history_len {
        return false;
    }
    if history_idx < history_start {
        return true;
    }
    history
        .items
        .get(history_idx - history_start)
        .is_some_and(|item| descriptor_origin_matches_history_item(&record.record.descriptor, item))
}

fn descriptor_origin_matches_history_item(
    descriptor: &crate::TranscriptBlockDescriptor,
    item: &protocol::HistoryItem,
) -> bool {
    matches!(
        (descriptor.kind(), item),
        ("user", protocol::HistoryItem::User { .. })
            | (
                "assistant" | "thinking" | "tool" | "exec" | "code",
                protocol::HistoryItem::Assistant(_),
            )
    )
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DurableCursor {
    pub store_history_len: usize,
    pub descriptor_len: usize,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SaveAckOutcome {
    pub plan: SaveAckPlan,
    pub kind: SessionSaveKind,
    pub descriptor_append: Option<DescriptorAppendSubmission>,
}

#[derive(Clone, Debug)]
pub struct SessionPersistState {
    pub save_pending: bool,
    pending_save: Option<PendingSessionSave>,
    pub next_save_id: u64,
    pub dirty_generation: u64,
    pub durable: DurableCursor,
    pub store_ready: bool,
    pub descriptors_persisted: bool,
    pub session_dirty: bool,
    pub dirty_history_from: Option<usize>,
}

impl Default for SessionPersistState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionPersistState {
    pub fn new() -> Self {
        Self {
            save_pending: false,
            pending_save: None,
            next_save_id: 1,
            dirty_generation: 0,
            durable: DurableCursor::default(),
            store_ready: false,
            descriptors_persisted: false,
            session_dirty: false,
            dirty_history_from: None,
        }
    }

    fn begin_save(
        &mut self,
        session_id: String,
        kind: SessionSaveKind,
        generation: DocumentGeneration,
        history: SubmittedHistoryRange,
        descriptor_append: Option<DescriptorAppendSubmission>,
    ) -> Option<u64> {
        let save_id = self.available_save_id_or_queue()?;
        self.install_pending_save(
            save_id,
            session_id,
            kind,
            generation,
            history,
            descriptor_append,
        );
        Some(save_id)
    }

    fn available_save_id_or_queue(&mut self) -> Option<u64> {
        if self.pending_save.is_some() {
            self.save_pending = true;
            return None;
        }
        Some(self.next_save_id)
    }

    fn install_pending_save(
        &mut self,
        save_id: u64,
        session_id: String,
        kind: SessionSaveKind,
        generation: DocumentGeneration,
        history: SubmittedHistoryRange,
        descriptor_append: Option<DescriptorAppendSubmission>,
    ) {
        debug_assert!(self.pending_save.is_none());
        self.next_save_id = save_id.saturating_add(1);
        self.pending_save = Some(PendingSessionSave {
            save_id,
            session_id,
            kind,
            generation,
            history,
            descriptor_append,
        });
    }

    pub fn begin_metadata_commit(
        &mut self,
        session_id: String,
        generation: DocumentGeneration,
        identity: smelt_store::SessionIdentity,
        metadata: smelt_store::SessionMetadata,
        side_tables: smelt_store::SideTableSuffixes,
    ) -> Option<SubmittedSessionCommit> {
        let base = self.base();
        let history = SubmittedHistoryRange::metadata_only(base.history_len as u64);
        let save_id = self.begin_save(
            session_id.clone(),
            SessionSaveKind::Metadata,
            generation,
            history,
            None,
        )?;
        Some(SubmittedSessionCommit {
            command: metadata_commit(save_id, session_id, base, identity, metadata, side_tables),
        })
    }

    pub fn begin_history_commit(
        &mut self,
        session_id: String,
        generation: DocumentGeneration,
        delta: PersistDelta,
        descriptor_append: Option<DescriptorAppendSubmission>,
    ) -> Result<Option<SubmittedSessionCommit>, String> {
        let history = SubmittedHistoryRange::from_history_suffix(&delta.history);
        let base = self.base();
        let Some(save_id) = self.available_save_id_or_queue() else {
            return Ok(None);
        };
        let command = history_commit_from_delta(save_id, session_id.clone(), base, delta)?;
        self.install_pending_save(
            save_id,
            session_id,
            SessionSaveKind::History,
            generation,
            history,
            descriptor_append,
        );
        Ok(Some(SubmittedSessionCommit { command }))
    }

    pub fn ack_save(
        &mut self,
        receipt: &smelt_store::SaveReceipt,
        current_generation: DocumentGeneration,
    ) -> Option<SaveAckOutcome> {
        let pending = self.pending_save.take()?;
        if pending.save_id != receipt.save_id.get() || pending.session_id != receipt.session_id {
            self.pending_save = Some(pending);
            return None;
        }
        let history_len = receipt
            .current
            .history_len
            .as_usize()
            .expect("saved history length originated as usize");
        if pending.history.len != history_len {
            self.mark_history_dirty_from(pending.history.start_idx);
            self.save_pending = true;
            return Some(SaveAckOutcome {
                plan: SaveAckPlan::SaveAgain,
                kind: pending.kind,
                descriptor_append: None,
            });
        }
        debug_assert!(pending.history.start_idx <= pending.history.len);
        self.store_ready = true;
        self.durable.store_history_len = history_len;
        self.durable.descriptor_len = receipt
            .current
            .descriptor_len
            .as_usize()
            .expect("saved descriptor length originated as usize");
        self.durable.revision = receipt.current.revision.get();
        if matches!(pending.kind, SessionSaveKind::History) {
            self.descriptors_persisted = true;
        }
        let plan = plan_save_ack_for_kind(pending.generation, current_generation, pending.kind);
        match plan {
            SaveAckPlan::MarkClean { .. } => self.mark_clean(),
            SaveAckPlan::SaveAgain => self.save_pending = true,
        }
        Some(SaveAckOutcome {
            plan,
            kind: pending.kind,
            descriptor_append: pending.descriptor_append,
        })
    }

    pub fn is_save_queued(&self) -> bool {
        self.save_pending
    }

    pub fn clear_queued_save(&mut self) {
        self.save_pending = false;
    }

    pub fn queue_save(&mut self) {
        self.save_pending = true;
    }

    pub fn has_pending_save(&self) -> bool {
        self.pending_save.is_some()
    }

    pub fn generation_for_descriptor(
        &self,
        transcript_descriptor_generation: u64,
    ) -> DocumentGeneration {
        DocumentGeneration::new(self.dirty_generation, transcript_descriptor_generation)
    }

    pub fn base(&self) -> PersistBase {
        PersistBase {
            revision: self.durable.revision,
            history_len: self.durable.store_history_len,
            descriptor_len: self.durable.descriptor_len,
        }
    }

    pub fn install_loaded_full_session(
        &mut self,
        writable: bool,
        history_len: usize,
        descriptor_len: usize,
        revision: u64,
    ) {
        self.store_ready = writable;
        self.durable = DurableCursor {
            store_history_len: history_len,
            descriptor_len,
            revision,
        };
        self.descriptors_persisted = false;
        self.mark_clean();
    }

    pub fn install_loaded_store_session(
        &mut self,
        writable: bool,
        history_len: usize,
        descriptor_len: usize,
        revision: u64,
    ) {
        self.store_ready = writable;
        self.durable = DurableCursor {
            store_history_len: history_len,
            descriptor_len,
            revision,
        };
        self.descriptors_persisted = true;
        self.mark_clean();
    }

    pub fn install_materialized_session(
        &mut self,
        descriptors_persisted: bool,
        descriptor_len: usize,
    ) {
        self.store_ready = true;
        self.descriptors_persisted = descriptors_persisted;
        self.durable.descriptor_len = descriptor_len;
        self.mark_clean();
    }

    pub fn mark_descriptors_unpersisted(&mut self) {
        self.descriptors_persisted = false;
    }

    pub fn forget_pending_save_if_matches(&mut self, save_id: u64, session_id: &str) -> bool {
        self.take_pending_save_if_matches(save_id, session_id)
            .is_some()
    }

    fn take_pending_save_if_matches(
        &mut self,
        save_id: u64,
        session_id: &str,
    ) -> Option<PendingSessionSave> {
        let pending = self.pending_save.take()?;
        if pending.save_id == save_id && pending.session_id == session_id {
            Some(pending)
        } else {
            self.pending_save = Some(pending);
            None
        }
    }

    pub fn record_save_failure(
        &mut self,
        save_id: u64,
        session_id: &str,
        failure: Option<&smelt_store::SessionCommitFailure>,
    ) -> SaveFailurePlan {
        let Some(pending) = self.take_pending_save_if_matches(save_id, session_id) else {
            return SaveFailurePlan::default();
        };
        self.mark_history_dirty_from(pending.history.start_idx);
        let mut plan = SaveFailurePlan::default();
        if let Some(smelt_store::SessionCommitFailure::StaleBase { current, .. }) = failure {
            self.durable.revision = current.revision.get();
            if let Some(history_len) = current.history_len.as_usize() {
                self.durable.store_history_len = history_len;
            }
            if let Some(descriptor_len) = current.descriptor_len.as_usize() {
                self.durable.descriptor_len = descriptor_len;
                plan.reconcile_descriptor_len = Some(descriptor_len);
            }
        }
        plan
    }

    pub fn mark_persist_failure_retry(&mut self) {
        self.mark_history_dirty_from(0);
        self.queue_save();
    }

    pub fn save_state(
        &self,
        generation: DocumentGeneration,
        descriptor_dirty_from: Option<usize>,
        history_len: usize,
        supports_metadata_only: bool,
    ) -> SessionSaveState {
        SessionSaveState {
            generation,
            store_ready: self.store_ready,
            descriptors_persisted: self.descriptors_persisted,
            session_dirty: self.session_dirty,
            dirty_history_from: self.dirty_history_from,
            descriptor_dirty_from,
            history_len,
            durable_history_len: self.durable.store_history_len,
            supports_metadata_only,
        }
    }

    pub fn dirty_state(
        &self,
        descriptor_dirty_from: Option<usize>,
        history_len: usize,
        transcript_empty: bool,
    ) -> DocumentDirtyState {
        DocumentDirtyState {
            pending_or_queued_save: self.has_pending_save() || self.is_save_queued(),
            session_dirty: self.session_dirty,
            dirty_history_from: self.dirty_history_from,
            descriptor_dirty_from,
            store_ready: self.store_ready,
            descriptors_persisted: self.descriptors_persisted,
            history_len,
            transcript_empty,
        }
    }

    pub fn has_session_work(&self) -> bool {
        self.session_dirty || self.dirty_history_from.is_some() || !self.store_ready
    }

    pub fn descriptors_persisted(&self) -> bool {
        self.descriptors_persisted
    }

    pub fn can_persist_descriptor_suffix_at(&self, history_index: usize) -> bool {
        (self.dirty_history_from.is_none() || self.dirty_history_from == Some(history_index))
            && ((self.store_ready && self.descriptors_persisted) || history_index == 0)
    }

    pub fn can_persist_request_append_at(&self, history_index: usize) -> bool {
        history_index == self.durable.store_history_len
            && !self.has_pending_save()
            && self.can_persist_descriptor_suffix_at(history_index)
    }

    pub fn set_pending_save_for_test(
        &mut self,
        save_id: u64,
        session_id: String,
        kind: SessionSaveKind,
        generation: DocumentGeneration,
        history_len: usize,
    ) {
        self.set_pending_save_submission_for_test(
            save_id,
            session_id,
            kind,
            generation,
            SubmittedHistoryRange {
                start_idx: history_len,
                len: history_len,
            },
            None,
        );
        self.durable.store_history_len = history_len;
    }

    pub fn set_pending_save_submission_for_test(
        &mut self,
        save_id: u64,
        session_id: String,
        kind: SessionSaveKind,
        generation: DocumentGeneration,
        history: SubmittedHistoryRange,
        descriptor_append: Option<DescriptorAppendSubmission>,
    ) {
        self.pending_save = Some(PendingSessionSave {
            save_id,
            session_id,
            kind,
            generation,
            history,
            descriptor_append,
        });
    }

    pub fn require_history_resave_from(&mut self, idx: usize) {
        self.mark_history_dirty_from(idx);
    }

    pub fn record_mutation(&mut self, session_dirty: bool, history_dirty_from: Option<usize>) {
        if let Some(idx) = history_dirty_from {
            self.mark_history_dirty_from(idx);
        } else if session_dirty {
            self.mark_session_dirty();
        }
    }

    pub fn record_transcript_descriptors_unpersisted(&mut self, descriptors_unpersisted: bool) {
        if descriptors_unpersisted {
            self.mark_descriptors_unpersisted();
        }
    }

    pub fn mark_session_dirty(&mut self) {
        self.session_dirty = true;
        self.bump_dirty_generation();
    }

    pub fn bump_dirty_generation(&mut self) {
        self.dirty_generation = self.dirty_generation.saturating_add(1);
    }

    pub fn mark_history_dirty_from(&mut self, idx: usize) {
        self.mark_session_dirty();
        self.dirty_history_from = Some(
            self.dirty_history_from
                .map_or(idx, |current| current.min(idx)),
        );
    }

    pub fn mark_clean(&mut self) {
        self.session_dirty = false;
        self.dirty_history_from = None;
    }

    pub fn reset_unpersisted(&mut self) {
        self.store_ready = false;
        self.durable = DurableCursor::default();
        self.descriptors_persisted = false;
    }
}

pub fn plan_session_save(state: SessionSaveState) -> SessionSavePlan {
    if state.history_len == 0
        && !state.session_dirty
        && !state.store_ready
        && state.descriptor_dirty_from.is_none()
    {
        return SessionSavePlan::Skip(SessionSaveSkipReason::EmptyUnstored);
    }
    if !state.session_dirty
        && state.store_ready
        && state.dirty_history_from.is_none()
        && state.descriptors_persisted
        && state.descriptor_dirty_from.is_none()
    {
        return SessionSavePlan::Skip(SessionSaveSkipReason::Unchanged);
    }

    let no_history_work = state.store_ready && state.dirty_history_from.is_none();
    let no_descriptor_work = state.descriptors_persisted && state.descriptor_dirty_from.is_none();
    if state.supports_metadata_only
        && state.session_dirty
        && no_history_work
        && no_descriptor_work
        && state.durable_history_len == state.history_len
    {
        return SessionSavePlan::MetadataOnly {
            generation: state.generation,
        };
    }

    let dirty_history_from = if state.supports_metadata_only
        && state.session_dirty
        && no_history_work
        && no_descriptor_work
    {
        Some(0)
    } else {
        state.dirty_history_from
    };
    let durable_history_len = if state.store_ready {
        state.durable_history_len
    } else {
        0
    };
    let history_start_idx =
        bounded_history_save_start(dirty_history_from, state.history_len, durable_history_len);
    if state.descriptors_persisted
        && state.descriptor_dirty_from.is_none()
        && dirty_history_from.is_none()
        && !state.session_dirty
    {
        return SessionSavePlan::Skip(SessionSaveSkipReason::Unchanged);
    }

    SessionSavePlan::History {
        generation: state.generation,
        history_start_idx,
        dirty_history_from,
    }
}

pub fn has_unflushed_work(state: DocumentDirtyState) -> bool {
    state.pending_or_queued_save
        || state.session_dirty
        || state.dirty_history_from.is_some()
        || state.descriptor_dirty_from.is_some()
        || (!state.store_ready && state.history_len > 0)
        || (!state.descriptors_persisted && !state.transcript_empty)
}

pub fn plan_save_ack(
    submitted: DocumentGeneration,
    current: DocumentGeneration,
    is_history_save: bool,
) -> SaveAckPlan {
    if submitted == current {
        SaveAckPlan::MarkClean {
            clear_descriptors: is_history_save,
        }
    } else {
        SaveAckPlan::SaveAgain
    }
}

pub fn plan_save_ack_for_kind(
    submitted: DocumentGeneration,
    current: DocumentGeneration,
    kind: SessionSaveKind,
) -> SaveAckPlan {
    plan_save_ack(submitted, current, matches!(kind, SessionSaveKind::History))
}

fn bounded_history_save_start(
    dirty_history_from: Option<usize>,
    history_len: usize,
    durable_history_len: usize,
) -> usize {
    dirty_history_from
        .unwrap_or(history_len)
        .min(history_len)
        .min(durable_history_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_only_requires_clean_history_and_descriptors() {
        let plan = plan_session_save(SessionSaveState {
            generation: DocumentGeneration::new(1, 2),
            store_ready: true,
            descriptors_persisted: true,
            session_dirty: true,
            dirty_history_from: None,
            descriptor_dirty_from: None,
            history_len: 3,
            durable_history_len: 3,
            supports_metadata_only: true,
        });

        assert_eq!(
            plan,
            SessionSavePlan::MetadataOnly {
                generation: DocumentGeneration::new(1, 2)
            }
        );
    }

    fn suffix(
        history_start_idx: usize,
        history_len: usize,
        items: Vec<protocol::HistoryItem>,
    ) -> smelt_store::HistorySuffix {
        smelt_store::HistorySuffix {
            start: smelt_store::HistoryIndex::new(history_start_idx as u64),
            final_len: smelt_store::HistoryLen::new(history_len as u64),
            items,
        }
    }

    fn save_receipt(
        save_id: u64,
        history_len: usize,
        descriptor_len: usize,
        revision: u64,
    ) -> smelt_store::SaveReceipt {
        smelt_store::SaveReceipt {
            session_id: "session-a".into(),
            save_id: smelt_store::SaveId::new(save_id),
            previous: smelt_store::StoreHead {
                revision: smelt_store::Revision::new(revision.saturating_sub(1)),
                history_len: smelt_store::HistoryLen::new(history_len as u64),
                descriptor_len: smelt_store::DescriptorLen::new(descriptor_len as u64),
            },
            current: smelt_store::StoreHead {
                revision: smelt_store::Revision::new(revision),
                history_len: smelt_store::HistoryLen::new(history_len as u64),
                descriptor_len: smelt_store::DescriptorLen::new(descriptor_len as u64),
            },
        }
    }

    #[test]
    fn transcript_descriptor_row_preserves_sparse_block_id() {
        let record = crate::TranscriptBlockRecordWithId {
            block_id: crate::BlockId::new(302),
            record: crate::TranscriptBlockRecord {
                descriptor: crate::TranscriptBlockDescriptor::User {
                    text: "follow up".to_string(),
                    image_labels: Vec::new(),
                },
                content_hash: 0,
                origin: Some(crate::BlockOrigin::History(11)),
                tool_state: None,
            },
        };

        let history = suffix(
            11,
            12,
            vec![protocol::HistoryItem::user(protocol::Content::text(
                "follow up",
            ))],
        );
        let row = transcript_descriptor_row(1, &record, &history).expect("descriptor row");

        assert_eq!(row.block_idx, 302);
        assert_eq!(row.history_idx, Some(11));
    }

    #[test]
    fn transcript_descriptor_row_omits_unsaved_history_origin() {
        let record = crate::TranscriptBlockRecordWithId {
            block_id: crate::BlockId::new(303),
            record: crate::TranscriptBlockRecord {
                descriptor: crate::TranscriptBlockDescriptor::User {
                    text: "follow up".to_string(),
                    image_labels: Vec::new(),
                },
                content_hash: 0,
                origin: Some(crate::BlockOrigin::History(12)),
                tool_state: None,
            },
        };

        let history = suffix(12, 12, Vec::new());
        let row = transcript_descriptor_row(2, &record, &history).expect("descriptor row");

        assert_eq!(row.block_idx, 303);
        assert_eq!(row.history_idx, None);
        assert_eq!(row.origin_json, None);
    }

    #[test]
    fn transcript_descriptor_row_omits_origin_that_points_to_nonmatching_suffix_item() {
        let record = crate::TranscriptBlockRecordWithId {
            block_id: crate::BlockId::new(304),
            record: crate::TranscriptBlockRecord {
                descriptor: crate::TranscriptBlockDescriptor::User {
                    text: "follow up".to_string(),
                    image_labels: Vec::new(),
                },
                content_hash: 0,
                origin: Some(crate::BlockOrigin::History(3)),
                tool_state: None,
            },
        };
        let history = suffix(
            3,
            4,
            vec![protocol::HistoryItem::note(protocol::HistoryNote::context(
                "cwd changed",
            ))],
        );

        let row = transcript_descriptor_row(3, &record, &history).expect("descriptor row");

        assert_eq!(row.block_idx, 304);
        assert_eq!(row.history_idx, None);
        assert_eq!(row.origin_json, None);
    }

    #[test]
    fn history_save_start_is_bounded_by_durable_prefix() {
        let plan = plan_session_save(SessionSaveState {
            generation: DocumentGeneration::new(0, 0),
            store_ready: true,
            descriptors_persisted: true,
            session_dirty: false,
            dirty_history_from: Some(10),
            descriptor_dirty_from: None,
            history_len: 12,
            durable_history_len: 4,
            supports_metadata_only: true,
        });

        assert_eq!(
            plan,
            SessionSavePlan::History {
                generation: DocumentGeneration::new(0, 0),
                history_start_idx: 4,
                dirty_history_from: Some(10),
            }
        );
    }

    #[test]
    fn persist_state_ack_records_durable_cursors() {
        let mut state = SessionPersistState::new();
        state.mark_history_dirty_from(2);
        state.descriptors_persisted = false;
        let generation = state.generation_for_descriptor(5);
        let save_id = state
            .begin_save(
                "session-a".into(),
                SessionSaveKind::History,
                generation,
                SubmittedHistoryRange::new(2, 3),
                Some(DescriptorAppendSubmission {
                    count: 1,
                    had_descriptor_total: true,
                }),
            )
            .expect("save begins");

        let plan = state.ack_save(&save_receipt(save_id, 3, 9, 7), generation);

        assert_eq!(
            plan,
            Some(SaveAckOutcome {
                plan: SaveAckPlan::MarkClean {
                    clear_descriptors: true
                },
                kind: SessionSaveKind::History,
                descriptor_append: Some(DescriptorAppendSubmission {
                    count: 1,
                    had_descriptor_total: true,
                }),
            })
        );
        assert!(!state.session_dirty);
        assert_eq!(state.dirty_history_from, None);
        assert_eq!(state.durable.store_history_len, 3);
        assert_eq!(state.durable.descriptor_len, 9);
        assert_eq!(state.durable.revision, 7);
        assert!(state.descriptors_persisted);
    }

    #[test]
    fn persist_state_ack_with_unexpected_history_len_forces_retry() {
        let mut state = SessionPersistState::new();
        state.install_loaded_store_session(true, 1, 4, 7);
        let save_id = state
            .begin_save(
                "session-a".into(),
                SessionSaveKind::History,
                DocumentGeneration::new(0, 0),
                SubmittedHistoryRange::new(1, 2),
                None,
            )
            .expect("save begins");

        let plan = state.ack_save(
            &save_receipt(save_id, 1, 5, 8),
            DocumentGeneration::new(0, 0),
        );

        assert_eq!(
            plan,
            Some(SaveAckOutcome {
                plan: SaveAckPlan::SaveAgain,
                kind: SessionSaveKind::History,
                descriptor_append: None,
            })
        );
        assert!(state.is_save_queued());
        assert_eq!(state.dirty_history_from, Some(1));
        assert_eq!(
            state.durable,
            DurableCursor {
                store_history_len: 1,
                descriptor_len: 4,
                revision: 7,
            }
        );
    }

    #[test]
    fn persist_failure_retry_marks_conservative_history_resave() {
        let mut state = SessionPersistState::new();
        state.install_loaded_store_session(true, 3, 9, 7);
        state.mark_persist_failure_retry();

        assert!(state.is_save_queued());
        assert!(state.session_dirty);
        assert_eq!(state.dirty_history_from, Some(0));
        assert_eq!(state.durable.store_history_len, 3);
        assert_eq!(state.durable.descriptor_len, 9);
    }

    #[test]
    fn structured_stale_descriptor_failure_updates_durable_cursor() {
        let mut state = SessionPersistState::new();
        state.install_loaded_store_session(true, 3, 303, 7);
        let save_id = state
            .begin_save(
                "session-a".into(),
                SessionSaveKind::History,
                DocumentGeneration::new(0, 0),
                SubmittedHistoryRange::new(3, 4),
                None,
            )
            .expect("save begins");

        let plan = state.record_save_failure(
            save_id,
            "session-a",
            Some(&smelt_store::SessionCommitFailure::StaleBase {
                expected: smelt_store::StoreHead {
                    revision: smelt_store::Revision::new(7),
                    history_len: smelt_store::HistoryLen::new(3),
                    descriptor_len: smelt_store::DescriptorLen::new(303),
                },
                current: smelt_store::StoreHead {
                    revision: smelt_store::Revision::new(7),
                    history_len: smelt_store::HistoryLen::new(3),
                    descriptor_len: smelt_store::DescriptorLen::new(111),
                },
            }),
        );

        assert_eq!(
            plan,
            SaveFailurePlan {
                reconcile_descriptor_len: Some(111)
            }
        );
        assert!(!state.has_pending_save());
        assert!(!state.is_save_queued());
        assert_eq!(state.durable.descriptor_len, 111);
        assert_eq!(state.dirty_history_from, Some(3));
    }

    #[test]
    fn save_failure_marks_work_dirty_without_owning_retry_policy() {
        let mut state = SessionPersistState::new();
        state.install_loaded_store_session(true, 3, 3, 7);
        state.set_pending_save_submission_for_test(
            1,
            "session-a".into(),
            SessionSaveKind::History,
            DocumentGeneration::new(0, 0),
            SubmittedHistoryRange::new(3, 4),
            None,
        );

        state.record_save_failure(
            1,
            "session-a",
            Some(&smelt_store::SessionCommitFailure::Busy {
                operation: "commit session".into(),
                attempts: 6,
                waited_ms: 250,
            }),
        );

        assert!(!state.has_pending_save());
        assert!(!state.is_save_queued());
        assert!(state.session_dirty);
        assert_eq!(state.dirty_history_from, Some(3));
    }

    #[test]
    fn unmatched_save_failure_is_ignored() {
        let mut state = SessionPersistState::new();
        state.install_loaded_store_session(true, 3, 303, 7);
        let save_id = state
            .begin_save(
                "session-a".into(),
                SessionSaveKind::History,
                DocumentGeneration::new(0, 0),
                SubmittedHistoryRange::new(3, 4),
                None,
            )
            .expect("save begins");

        let plan = state.record_save_failure(
            save_id + 1,
            "session-a",
            Some(&smelt_store::SessionCommitFailure::StaleBase {
                expected: smelt_store::StoreHead {
                    revision: smelt_store::Revision::new(7),
                    history_len: smelt_store::HistoryLen::new(3),
                    descriptor_len: smelt_store::DescriptorLen::new(303),
                },
                current: smelt_store::StoreHead {
                    revision: smelt_store::Revision::new(7),
                    history_len: smelt_store::HistoryLen::new(3),
                    descriptor_len: smelt_store::DescriptorLen::new(111),
                },
            }),
        );

        assert_eq!(plan, SaveFailurePlan::default());
        assert!(state.has_pending_save());
        assert!(!state.is_save_queued());
        assert_eq!(state.durable.descriptor_len, 303);
        assert_eq!(state.dirty_history_from, None);
    }

    #[test]
    fn interleaved_save_model_preserves_dirty_work_and_one_pending_commit() {
        let mut state = SessionPersistState::new();
        state.install_loaded_store_session(true, 3, 3, 4);
        state.mark_history_dirty_from(3);
        let first_generation = state.generation_for_descriptor(0);
        let first_save = state
            .begin_save(
                "session-a".into(),
                SessionSaveKind::History,
                first_generation,
                SubmittedHistoryRange::new(3, 4),
                None,
            )
            .expect("first save begins");

        state.mark_history_dirty_from(4);
        let current_generation = state.generation_for_descriptor(1);
        assert_eq!(
            state.begin_save(
                "session-a".into(),
                SessionSaveKind::History,
                current_generation,
                SubmittedHistoryRange::new(3, 5),
                None,
            ),
            None
        );
        assert!(state.has_pending_save());
        assert!(state.is_save_queued());

        assert_eq!(
            state.ack_save(&save_receipt(first_save + 1, 4, 4, 5), current_generation,),
            None
        );
        assert!(state.has_pending_save());

        assert_eq!(
            state.ack_save(&save_receipt(first_save, 4, 4, 5), current_generation,),
            Some(SaveAckOutcome {
                plan: SaveAckPlan::SaveAgain,
                kind: SessionSaveKind::History,
                descriptor_append: None,
            })
        );
        assert!(!state.has_pending_save());
        assert!(state.is_save_queued());
        assert!(state.session_dirty);
        assert_eq!(state.dirty_history_from, Some(3));
        assert_eq!(state.durable.store_history_len, 4);

        state.clear_queued_save();
        let second_save = state
            .begin_save(
                "session-a".into(),
                SessionSaveKind::History,
                current_generation,
                SubmittedHistoryRange::new(3, 5),
                None,
            )
            .expect("second save begins");
        let failure = state.record_save_failure(
            second_save,
            "session-a",
            Some(&smelt_store::SessionCommitFailure::StaleBase {
                expected: smelt_store::StoreHead {
                    revision: smelt_store::Revision::new(5),
                    history_len: smelt_store::HistoryLen::new(4),
                    descriptor_len: smelt_store::DescriptorLen::new(4),
                },
                current: smelt_store::StoreHead {
                    revision: smelt_store::Revision::new(7),
                    history_len: smelt_store::HistoryLen::new(4),
                    descriptor_len: smelt_store::DescriptorLen::new(4),
                },
            }),
        );
        assert_eq!(
            failure,
            SaveFailurePlan {
                reconcile_descriptor_len: Some(4),
            }
        );
        assert!(!state.has_pending_save());
        assert!(!state.is_save_queued());
        assert!(state.session_dirty);
        assert_eq!(state.dirty_history_from, Some(3));
        assert_eq!(state.durable.revision, 7);

        state.clear_queued_save();
        let final_save = state
            .begin_save(
                "session-a".into(),
                SessionSaveKind::History,
                current_generation,
                SubmittedHistoryRange::new(3, 5),
                None,
            )
            .expect("retry begins");
        assert_eq!(
            state.ack_save(&save_receipt(final_save, 5, 5, 8), current_generation,),
            Some(SaveAckOutcome {
                plan: SaveAckPlan::MarkClean {
                    clear_descriptors: true
                },
                kind: SessionSaveKind::History,
                descriptor_append: None,
            })
        );
        assert!(!state.has_pending_save());
        assert!(!state.is_save_queued());
        assert!(!state.session_dirty);
        assert_eq!(state.dirty_history_from, None);
        assert_eq!(
            state.durable,
            DurableCursor {
                store_history_len: 5,
                descriptor_len: 5,
                revision: 8,
            }
        );
    }

    #[test]
    fn acknowledged_truncation_moves_durable_history_to_explicit_rewind_boundary() {
        let mut state = SessionPersistState::new();
        state.install_loaded_store_session(true, 5, 5, 9);
        state.mark_history_dirty_from(2);
        let generation = state.generation_for_descriptor(2);
        let save_id = state
            .begin_save(
                "session-a".into(),
                SessionSaveKind::History,
                generation,
                SubmittedHistoryRange::new(2, 2),
                None,
            )
            .expect("truncation save begins");

        let plan = state.ack_save(&save_receipt(save_id, 2, 2, 10), generation);

        assert_eq!(
            plan,
            Some(SaveAckOutcome {
                plan: SaveAckPlan::MarkClean {
                    clear_descriptors: true
                },
                kind: SessionSaveKind::History,
                descriptor_append: None,
            })
        );
        assert_eq!(state.durable.store_history_len, 2);
        assert_eq!(state.durable.descriptor_len, 2);
        assert_eq!(state.dirty_history_from, None);
    }

    #[test]
    fn stale_ack_generation_requests_another_save() {
        assert_eq!(
            plan_save_ack(
                DocumentGeneration::new(1, 0),
                DocumentGeneration::new(2, 0),
                true,
            ),
            SaveAckPlan::SaveAgain,
        );
    }
}
