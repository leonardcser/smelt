use std::collections::VecDeque;
use std::time::{Duration, Instant};

use protocol::{HistoryItem, ReasoningEffort, TokenUsage, TurnMeta};
use smelt_core::session::{ContextCheckpoint, ContextTokenIdentity, Session};
use smelt_core::session_runtime::LiveSession;
use smelt_transcript::content::stream_parser::{StreamParser, ToolStart};
use smelt_transcript::transcript_model::{Block, BlockId, BlockOrigin, ToolOutputRef, ToolStatus};

use crate::app::transcript::{
    RecordSaveBoundsError, TranscriptDocument, TranscriptRecordSaveBounds,
    TranscriptSaveHydrationError,
};

/// Save bookkeeping: what changed, what was captured for saving, and what is durable.
///
/// Every capture holds only the changes since the previous one. Saves reach the store
/// in capture order, so the store head after a capture is known when it is taken.
#[derive(Debug, Default)]
struct DocumentChanges {
    /// Bumped by every change that needs saving.
    current: u64,
    /// `current` when the last save was captured.
    captured: u64,
    /// `current` of the last durable save.
    durable: u64,
    /// Sequence number of the last captured save.
    seq: u64,
    /// Store head once every captured save is durable.
    captured_head: smelt_store::StoreHead,
    durable_head: smelt_store::StoreHead,
    history_dirty_from: Option<usize>,
    /// A save failed: the next capture rewrites everything after the durable head.
    resync: bool,
    in_flight: VecDeque<InFlightSave>,
    /// Transcript records made durable since the transcript last learned about them.
    durable_records: Option<TranscriptRecordSaveBounds>,
    /// Last captured values, so unchanged ones are not written again.
    first_user_message: Option<Option<std::sync::Arc<str>>>,
    checkpoint: Option<Option<serde_json::Value>>,
}

#[derive(Clone, Copy, Debug)]
struct InFlightSave {
    seq: u64,
    generation: u64,
    /// Where the save's history suffix starts, if it has one.
    history_start: Option<usize>,
    records: Option<TranscriptRecordSaveBounds>,
}

impl DocumentChanges {
    fn new(head: smelt_store::StoreHead) -> Self {
        Self {
            captured_head: head,
            durable_head: head,
            ..Self::default()
        }
    }

    fn record(&mut self, change: &DocumentChange) {
        if !change.canonical_changed() {
            return;
        }
        self.current += 1;
        if let Some(index) = change.history_dirty_from {
            self.mark_history_dirty_from(index);
        }
    }

    fn mark_history_dirty_from(&mut self, index: usize) {
        self.history_dirty_from = Some(self.history_dirty_from.map_or(index, |i| i.min(index)));
    }

    /// The store copy at `head` is stale: the next capture rewrites the session.
    fn rewrite_all(&mut self, head: smelt_store::StoreHead) {
        *self = Self {
            current: 1,
            seq: self.seq,
            history_dirty_from: Some(0),
            resync: true,
            ..Self::new(head)
        };
    }
}

fn merge_record_bounds(
    current: Option<TranscriptRecordSaveBounds>,
    next: Option<TranscriptRecordSaveBounds>,
) -> Option<TranscriptRecordSaveBounds> {
    match (current, next) {
        (Some(current), Some(next)) if current.order_start <= next.order_start => {
            Some(TranscriptRecordSaveBounds {
                record_end_idx: next.record_end_idx,
                ..current
            })
        }
        (_, Some(next)) => Some(next),
        (current, None) => current,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SaveCaptureError {
    HydrationPending,
    Invalid(String),
}

impl From<String> for SaveCaptureError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

impl From<smelt_core::session::SessionStoreError> for SaveCaptureError {
    fn from(error: smelt_core::session::SessionStoreError) -> Self {
        Self::Invalid(error.to_string())
    }
}

impl std::fmt::Display for SaveCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HydrationPending => f.write_str("transcript hydration is pending"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

pub(crate) struct SessionDocument {
    session: Session,
    transcript: crate::app::transcript::LoadedTranscript,
    live_session: Option<smelt_core::session_runtime::LiveSession>,
    store_head: Option<smelt_store::StoreHead>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChangeTracking {
    Enabled,
    Disabled,
}

pub(crate) struct TuiSessionDocument {
    pub(crate) transcript: TranscriptDocument,
    pub(crate) live_session: Option<LiveSession>,
    pub(crate) navigation: smelt_core::session_runtime::HistoryNavigation,
    changes: DocumentChanges,
    change_tracking: ChangeTracking,
}

impl TuiSessionDocument {
    pub(crate) fn new(transcript: TranscriptDocument) -> Self {
        Self {
            transcript,
            live_session: None,
            navigation: Default::default(),
            changes: DocumentChanges::default(),
            change_tracking: ChangeTracking::Enabled,
        }
    }

    fn record_change(&mut self, change: &DocumentChange) {
        if self.change_tracking == ChangeTracking::Enabled {
            self.changes.record(change);
        }
    }

    pub(super) fn enable_change_tracking(&mut self) {
        self.change_tracking = ChangeTracking::Enabled;
    }

    pub(super) fn disable_change_tracking(&mut self) {
        self.change_tracking = ChangeTracking::Disabled;
    }

    pub(super) fn apply_history(
        &mut self,
        session: &mut Session,
        mutation: HistoryMutation,
    ) -> DocumentChange {
        let change = SessionDocument::apply_history(
            session,
            self.live_session.as_mut(),
            Some(&mut self.transcript),
            mutation,
        );
        if self.live_session.is_none() {
            if let Some(start) = change.history_dirty_from {
                self.navigation.truncate(start);
                for (offset, item) in session
                    .history
                    .get(start..)
                    .unwrap_or(&[])
                    .iter()
                    .enumerate()
                {
                    self.navigation.append(start + offset, item);
                }
            }
        }
        self.record_change(&change);
        change
    }

    pub(super) fn apply_transcript(&mut self, mutation: TranscriptMutation) -> DocumentChange {
        let change = SessionDocument::apply_to_transcript(&mut self.transcript, mutation);
        self.record_change(&change);
        change
    }

    pub(super) fn apply_stream(
        &mut self,
        parser: &mut StreamParser,
        mutation: StreamMutation,
    ) -> DocumentChange {
        let change = SessionDocument::apply_stream(parser, &mut self.transcript, mutation);
        self.record_change(&change);
        change
    }

    pub(super) fn apply_usage(
        &mut self,
        session: &mut Session,
        mutation: UsageMutation,
    ) -> DocumentChange {
        let change = SessionDocument::apply_usage(session, mutation);
        self.record_change(&change);
        change
    }

    pub(super) fn clear_token_baseline_for_loaded_model(
        &mut self,
        session: &mut Session,
        identity: ContextTokenIdentity,
    ) {
        SessionDocument::apply_usage(
            session,
            UsageMutation::ClearTokenBaselineIfMismatched { identity },
        );
    }

    pub(super) fn apply_metadata(
        &mut self,
        session: &mut Session,
        mutation: MetadataMutation,
    ) -> DocumentChange {
        let change = SessionDocument::apply_metadata(session, mutation);
        self.record_change(&change);
        change
    }

    pub(super) fn apply_turn_state(
        &mut self,
        session: &mut Session,
        mutation: TurnStateMutation,
    ) -> DocumentChange {
        let change = SessionDocument::apply_turn_state(session, mutation);
        self.record_change(&change);
        change
    }

    /// Whether some change is not durable yet.
    pub(crate) fn has_unsaved_work(&self) -> bool {
        self.changes.current > self.changes.durable
            || self.changes.resync
            || self.transcript.history().record_dirty_from().is_some()
    }

    /// Whether some change is not captured into a save yet.
    pub(crate) fn has_uncaptured_work(&self) -> bool {
        self.changes.current > self.changes.captured
            || self.changes.resync
            || self.transcript.history().record_dirty_from().is_some()
    }

    pub(crate) fn records_persisted(&self) -> bool {
        self.transcript.history().record_dirty_from().is_none()
            && self.changes.in_flight.is_empty()
            && !self.changes.resync
    }

    pub(crate) fn durable_head(&self) -> smelt_store::StoreHead {
        self.changes.durable_head
    }

    /// Advances with every change that needs saving.
    pub(crate) fn revision(&self) -> u64 {
        self.changes.current
    }

    /// The store holds nothing of this session: the next save writes all of it.
    pub(crate) fn mark_session_unpersisted(&mut self) {
        self.changes.rewrite_all(smelt_store::StoreHead::default());
        self.transcript.history_mut().require_record_resave_from(0);
    }

    pub(crate) fn install_materialized_session(&mut self, records_persisted: bool) {
        if records_persisted {
            self.transcript.history_mut().clear_record_dirty();
        } else {
            self.transcript.history_mut().require_record_resave_from(0);
        }
    }

    pub(crate) fn install_rewind_prefix(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
        record_count: usize,
    ) {
        self.transcript
            .install_rewind_prefix(transcript, record_count);
    }

    /// Installs a session whose store copy at `store_head` the next save rewrites.
    pub(crate) fn install_loaded_full_session(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
        store_head: smelt_store::StoreHead,
    ) {
        self.live_session = None;
        self.transcript.replace_loaded_transcript(transcript);
        self.changes.rewrite_all(store_head);
        self.transcript.history_mut().require_record_resave_from(0);
    }

    pub(crate) fn install_loaded_store_session(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
        live_session: LiveSession,
        store_head: smelt_store::StoreHead,
    ) {
        debug_assert_eq!(
            store_head.history_len.as_usize(),
            live_session.history_len()
        );
        self.live_session = Some(live_session);
        self.navigation = Default::default();
        self.transcript.replace_loaded_transcript(transcript);
        self.changes = DocumentChanges::new(store_head);
    }

    #[cfg(test)]
    pub(crate) fn set_history_resave_from_for_test(&mut self, history_index: usize) {
        self.changes.current += 1;
        self.changes.mark_history_dirty_from(history_index);
    }

    #[cfg(test)]
    pub(crate) fn dirty_history_from_for_test(&self) -> Option<usize> {
        self.changes.history_dirty_from
    }

    /// Captures every change since the previous capture as one save.
    pub(crate) fn capture_save(
        &mut self,
        session: &mut Session,
        metadata: RuntimeSessionMetadata,
    ) -> Result<Option<crate::persist::SessionSave>, SaveCaptureError> {
        let _perf = smelt_perf::perf::begin("session:capture_save");
        if !self.has_uncaptured_work() {
            return Ok(None);
        }
        let history_len =
            SessionHistoryRef::new(self.live_session.as_ref(), &session.history).len();
        if self.changes.captured_head == smelt_store::StoreHead::default()
            && history_len == 0
            && self.transcript.is_empty()
        {
            return Ok(None);
        }
        self.apply_runtime_metadata(session, metadata);
        let resync = self.changes.resync;
        let base = if resync {
            self.changes.durable_head
        } else {
            self.changes.captured_head
        };
        let suffix = capture_suffix(
            &mut self.transcript,
            SessionHistoryRef::new(self.live_session.as_ref(), &session.history),
            base,
            self.changes.history_dirty_from,
        )?;

        if resync {
            session.turn_metas.mark_all_dirty();
            session.metadata_snapshots.mark_all_dirty();
            session.context_snapshots.mark_all_dirty();
            session.checkpoint_events.mark_all_dirty();
        }
        let encode = |error: serde_json::Error| format!("encode session rows: {error}");
        let first_user_message = Some(session.first_user_message.clone());
        let checkpoint = Some(smelt_core::session::store_checkpoint(
            session.checkpoint.as_ref(),
            history_len,
        )?);
        let history_start = suffix
            .history
            .as_ref()
            .map(|history| history.start as usize);
        let commit = smelt_store::SessionCommit {
            metadata: smelt_core::session::store_metadata(session)?,
            first_user_message: (resync || first_user_message != self.changes.first_user_message)
                .then(|| {
                    first_user_message
                        .clone()
                        .flatten()
                        .map(|text| text.to_string())
                }),
            checkpoint: (resync || checkpoint != self.changes.checkpoint)
                .then(|| checkpoint.clone().flatten()),
            history: suffix.history,
            turn_metas: session.turn_metas.take_changes().map_err(encode)?,
            metadata_snapshots: session.metadata_snapshots.take_changes().map_err(encode)?,
            context_snapshots: session.context_snapshots.take_changes().map_err(encode)?,
            checkpoint_events: session.checkpoint_events.take_changes().map_err(encode)?,
            transcript: suffix.transcript,
        };

        let changes = &mut self.changes;
        changes.seq += 1;
        changes.captured = changes.current;
        changes.captured_head = suffix.head;
        changes.history_dirty_from = None;
        changes.resync = false;
        changes.first_user_message = first_user_message;
        changes.checkpoint = checkpoint;
        changes.in_flight.push_back(InFlightSave {
            seq: changes.seq,
            generation: changes.current,
            history_start,
            records: suffix.records,
        });
        if suffix.records.is_some() {
            // Released records are read back from the store, which lags this capture.
            self.transcript.suspend_compaction();
        }
        self.transcript.history_mut().clear_record_dirty();
        Ok(Some(crate::persist::SessionSave {
            seq: changes.seq,
            resync,
            identity: smelt_core::session::store_identity(session),
            commit,
        }))
    }

    /// The commit that brings a fresh copy of the durable session up to `forked`,
    /// a fork of this document's session. Leaves the document unchanged.
    pub(crate) fn capture_fork_commit(
        &mut self,
        forked: &mut Session,
        metadata: RuntimeSessionMetadata,
        source_history: &[HistoryItem],
    ) -> Result<smelt_store::SessionCommit, SaveCaptureError> {
        let _perf = smelt_perf::perf::begin("session:capture_fork_commit");
        // An in-flight save could land in the source after the fork copies it.
        // A resync is fine: it already marks everything after the durable head.
        if !self.changes.in_flight.is_empty() {
            return Err(SaveCaptureError::Invalid(
                "the source session has saves in flight".into(),
            ));
        }
        SessionDocument::apply_metadata(forked, metadata.into_mutation());
        let history = SessionHistoryRef::new(self.live_session.as_ref(), source_history);
        let suffix = capture_suffix(
            &mut self.transcript,
            history,
            self.changes.durable_head,
            self.changes.history_dirty_from,
        )?;
        let mut commit = smelt_core::session::full_commit(forked)?;
        commit.checkpoint = Some(smelt_core::session::store_checkpoint(
            forked.checkpoint.as_ref(),
            history.len(),
        )?);
        commit.history = suffix.history;
        commit.transcript = suffix.transcript;
        Ok(commit)
    }

    /// Applies the store's report that every save through `durable.seq` is durable.
    /// Memory then keeps only the model context and what is not saved yet.
    pub(crate) fn acknowledge(
        &mut self,
        durable: crate::persist::DurableSave,
        session: &mut Session,
    ) -> bool {
        let changes = &mut self.changes;
        let mut acknowledged = None;
        while let Some(save) = changes
            .in_flight
            .front()
            .copied()
            .filter(|save| save.seq <= durable.seq)
        {
            changes.in_flight.pop_front();
            changes.durable_records = merge_record_bounds(changes.durable_records, save.records);
            acknowledged = Some(save);
        }
        let Some(save) = acknowledged else {
            return false;
        };
        changes.durable = save.generation;
        changes.durable_head = durable.head;
        if !changes.in_flight.is_empty() {
            return true;
        }
        debug_assert_eq!(durable.head, changes.captured_head);
        let record_len = durable.head.transcript_record_count.as_usize();
        let records = changes.durable_records.take();
        if let Some(bounds) = records {
            self.transcript
                .apply_persisted_record_suffix(bounds, record_len);
        }
        self.transcript
            .schedule_durable_compaction(record_len, records);
        self.adopt_stored_history(session);
        if let Some(live) = self.live_session.as_mut() {
            let keep_from = live
                .model_context_start(session.checkpoint.as_ref())
                .min(durable.head.history_len.as_usize())
                .min(self.changes.history_dirty_from.unwrap_or(usize::MAX));
            live.release_before(keep_from);
        }
        true
    }

    /// Moves the history of a session that now exists in the store into a
    /// store-backed live session, like a resumed one, so durable items can leave memory.
    fn adopt_stored_history(&mut self, session: &mut Session) {
        if self.live_session.is_some() {
            return;
        }
        let Some(address) = self
            .transcript
            .store_address()
            .filter(|address| address.session_id == session.id)
            .cloned()
        else {
            return;
        };
        let mut live = LiveSession::from_parts(
            address.session_id.clone(),
            address.artifact_dir(),
            Some(address),
            0,
            0,
        );
        for item in std::mem::take(&mut session.history) {
            live.append_history(item);
        }
        self.navigation = Default::default();
        self.live_session = Some(live);
    }

    /// Every save in flight was dropped: rewrite everything after the durable head.
    pub(crate) fn fail_all_in_flight(&mut self) {
        self.fail_save(u64::MAX);
    }

    /// Saves from `seq` on were dropped: rewrite everything after the durable head.
    /// Returns false when this failure was already handled.
    pub(crate) fn fail_save(&mut self, seq: u64) -> bool {
        let changes = &mut self.changes;
        if changes.in_flight.front().is_none_or(|save| save.seq > seq) {
            return false;
        }
        let failed: Vec<_> = changes.in_flight.drain(..).collect();
        if let Some(order_start) = failed
            .iter()
            .filter_map(|save| save.records.map(|bounds| bounds.order_start))
            .min()
        {
            self.transcript
                .history_mut()
                .require_record_resave_from(order_start);
        }
        changes.resync = true;
        changes.captured_head = changes.durable_head;
        let history_start = failed
            .iter()
            .filter_map(|save| save.history_start)
            .min()
            .unwrap_or(usize::MAX);
        changes.mark_history_dirty_from(
            history_start.min(changes.durable_head.history_len.as_usize()),
        );
        changes.first_user_message = None;
        changes.checkpoint = None;
        true
    }

    /// Ephemeral sessions are never saved; treat every change as durable.
    pub(crate) fn mark_ephemeral_persisted(&mut self) {
        self.changes.captured = self.changes.current;
        self.changes.durable = self.changes.current;
        self.changes.history_dirty_from = None;
        self.changes.resync = false;
        self.transcript.history_mut().clear_record_dirty();
    }

    fn apply_runtime_metadata(&mut self, session: &mut Session, metadata: RuntimeSessionMetadata) {
        let change = SessionDocument::apply_metadata(session, metadata.into_mutation());
        self.changes.record(&change);
    }
}

fn capture_suffix(
    transcript: &mut TranscriptDocument,
    source_history: SessionHistoryRef<'_>,
    base: smelt_store::StoreHead,
    history_dirty_from: Option<usize>,
) -> Result<CapturedSuffix, SaveCaptureError> {
    let history_len = source_history.len();
    let base_history_len = base.history_len.as_usize();
    let history_start = history_dirty_from
        .map_or(base_history_len, |dirty| dirty.min(base_history_len))
        .min(history_len);
    let history = (history_start < history_len || history_len != base_history_len)
        .then(|| {
            Ok::<_, String>(smelt_store::HistorySuffix {
                start: history_start as u64,
                items: source_history.range(history_start..history_len)?,
            })
        })
        .transpose()?;

    let base_record_len = base.transcript_record_count.as_usize();
    // A record's stored origin depends on the history saved with it, so records
    // that point into a rewritten history suffix are rewritten too.
    let origins_dirty_from = history.as_ref().map(|suffix| suffix.start as usize);
    let records = transcript
        .record_save_bounds_at_or_before(origins_dirty_from, base_record_len)
        .map_err(|error| match error {
            RecordSaveBoundsError::DetachedTail => SaveCaptureError::HydrationPending,
            RecordSaveBoundsError::Invalid(message) => SaveCaptureError::Invalid(message),
        })?;
    transcript
        .pin_record_suffix_for_save(records)
        .map_err(|error| match error {
            TranscriptSaveHydrationError::Pending => SaveCaptureError::HydrationPending,
            TranscriptSaveHydrationError::Failed => {
                SaveCaptureError::Invalid("hydrate canonical transcript record suffix".to_owned())
            }
        })?;
    let captured = records.map(|bounds| {
        transcript
            .history()
            .capture_block_records_with_ids_from(bounds.order_start)
    });
    transcript.unpin_record_suffix_after_save();
    let transcript_suffix =
        records
            .zip(captured)
            .map(|(bounds, captured)| {
                let mut rows = Vec::with_capacity(captured.len());
                for (offset, record) in captured.into_iter().enumerate() {
                    let mut record = record.prepare();
                    // The store keeps only origins that name a history item. The
                    // in-memory origin stays: a pending rebuild may still need it.
                    if record.record.origin.is_some_and(|origin| {
                        !origin_matches(&record.record, origin, source_history)
                    }) {
                        record.record.origin = None;
                    }
                    rows.push(transcript_record_row(
                        bounds.record_start_idx + offset,
                        &record,
                    )?);
                }
                Ok::<_, smelt_store::StoreError>(smelt_store::TranscriptRecordSuffix {
                    start: bounds.record_start_idx as u64,
                    records: rows,
                })
            })
            .transpose()
            .map_err(|error| format!("encode transcript records: {error}"))?;
    Ok(CapturedSuffix {
        history,
        transcript: transcript_suffix,
        records,
        head: smelt_store::StoreHead {
            history_len: history_len.into(),
            transcript_record_count: records
                .map_or(base_record_len, |bounds| bounds.record_end_idx)
                .into(),
        },
    })
}

struct CapturedSuffix {
    history: Option<smelt_store::HistorySuffix>,
    transcript: Option<smelt_store::TranscriptRecordSuffix>,
    records: Option<TranscriptRecordSaveBounds>,
    head: smelt_store::StoreHead,
}

pub(crate) struct FullSessionDocument {
    pub(crate) session: Session,
    pub(crate) transcript: crate::app::transcript::LoadedTranscript,
}

pub(crate) struct StoreBackedSessionDocument {
    pub(crate) session: Session,
    pub(crate) transcript: crate::app::transcript::LoadedTranscript,
    pub(crate) live_session: smelt_core::session_runtime::LiveSession,
    pub(crate) store_head: smelt_store::StoreHead,
}

#[derive(Clone, Copy)]
enum SessionHistoryRef<'a> {
    Materialized(&'a [HistoryItem]),
    StoreBacked(&'a LiveSession),
}

impl<'a> SessionHistoryRef<'a> {
    fn new(live_session: Option<&'a LiveSession>, session_history: &'a [HistoryItem]) -> Self {
        live_session.map_or(Self::Materialized(session_history), Self::StoreBacked)
    }

    fn len(self) -> usize {
        match self {
            SessionHistoryRef::Materialized(history) => history.len(),
            SessionHistoryRef::StoreBacked(live) => live.history_len(),
        }
    }

    fn range(self, range: std::ops::Range<usize>) -> Result<Vec<HistoryItem>, String> {
        match self {
            SessionHistoryRef::Materialized(history) => {
                Ok(history.get(range).unwrap_or_default().to_vec())
            }
            SessionHistoryRef::StoreBacked(live) => live.history_range(range),
        }
    }

    /// The item at `index`, if it is held in memory.
    fn in_memory(self, index: usize) -> Option<&'a HistoryItem> {
        match self {
            SessionHistoryRef::Materialized(history) => history.get(index),
            SessionHistoryRef::StoreBacked(live) => index
                .checked_sub(live.live_start)
                .and_then(|offset| live.live_history().get(offset)),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeSessionMetadata {
    pub(crate) updated_at_ms: u64,
    pub(crate) mode: String,
    pub(crate) reasoning_effort: ReasoningEffort,
    pub(crate) model: Option<String>,
    pub(crate) fast_mode: bool,
}

impl RuntimeSessionMetadata {
    pub(crate) fn into_mutation(self) -> MetadataMutation {
        MetadataMutation::UpdateRuntime {
            updated_at_ms: self.updated_at_ms,
            mode: self.mode,
            reasoning_effort: self.reasoning_effort,
            model: self.model,
            fast_mode: self.fast_mode,
        }
    }
}

pub(crate) enum HistoryMutation {
    AppendItem {
        item: HistoryItem,
    },
    CommitRequestItem {
        item: HistoryItem,
        block: Option<Box<Block>>,
        first_user_message: Option<String>,
    },
    TruncateFrom {
        index: usize,
        user_turn_count: Option<usize>,
        identity: ContextTokenIdentity,
    },
    RewindTo {
        index: usize,
        user_turn_count: Option<usize>,
        keep_checkpoint_at_boundary: bool,
        identity: ContextTokenIdentity,
    },
}

pub(crate) enum TranscriptMutation {
    AppendBlock {
        block: Block,
    },
    InsertBlockBefore {
        before: BlockId,
        block: Block,
    },
    AppendAccepted {
        block: Block,
    },
    PromoteAccepted {
        id: BlockId,
        history_index: usize,
    },
    RemoveAccepted {
        id: BlockId,
    },
    RemoveSubmittedUser {
        id: BlockId,
        history_index: usize,
    },
    InsertCheckpointMarker {
        block_index: usize,
        history_index: usize,
        block: Block,
    },
    RemoveUnoriginatedBlockAt {
        block_index: usize,
    },
    ReplaceFromHistory {
        transcript: Box<smelt_transcript::content::transcript::Transcript>,
    },
    TruncateTo {
        block_index: usize,
    },
    Clear,
    UpdateCompactionPreview {
        summary: String,
    },
    ClearCompactionPreview,
    RewriteBlock {
        id: BlockId,
        block: Block,
    },
}

pub(crate) enum StreamMutation {
    AppendThinking {
        delta: String,
    },
    FlushThinking,
    AppendText {
        delta: String,
    },
    FlushText,
    SyncActiveToolElapsed {
        now: Instant,
    },
    StartTool {
        start: ToolStart,
        now: Instant,
    },
    AppendToolOutput {
        invocation_id: protocol::InvocationId,
        line: String,
    },
    AppendToolOutputSlice {
        block_id: BlockId,
        chunk: smelt_transcript::transcript_content::SharedContentSlice,
        line_start: bool,
    },
    SetToolStatus {
        invocation_id: protocol::InvocationId,
        status: ToolStatus,
        now: Instant,
    },
    SetToolUserMessage {
        invocation_id: protocol::InvocationId,
        message: String,
    },
    FinishTool {
        invocation_id: protocol::InvocationId,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
        now: Instant,
    },
    FinalizeTools,
    PromoteToolDraft {
        stream_id: Option<String>,
        start: ToolStart,
        now: Instant,
    },
    ClearToolDrafts,
    StartToolDraft {
        stream_id: String,
        call_id: Option<String>,
        name: Option<String>,
        native: bool,
    },
    AppendToolDraft {
        stream_id: String,
        call_id: Option<String>,
        name: Option<String>,
        delta: String,
    },
    FinishToolDraft {
        stream_id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
    SetToolDraftSummary {
        block_id: BlockId,
        summary: protocol::StyledLines,
    },
    StartExec {
        command: String,
    },
    AppendExecOutput {
        chunk: String,
    },
    FinishExec {
        final_output: Option<String>,
    },
}

pub(crate) enum UsageMutation {
    RecordTokens {
        usage: TokenUsage,
        history_len: usize,
        identity: ContextTokenIdentity,
    },
    ClearTokenBaselineIfMismatched {
        identity: ContextTokenIdentity,
    },
    Accumulate {
        usage: TokenUsage,
        cost_usd: f64,
    },
}

pub(crate) enum MetadataMutation {
    SetTitle {
        title: String,
        slug: String,
        snapshot_history_len: usize,
    },
    UpdateRuntime {
        updated_at_ms: u64,
        mode: String,
        reasoning_effort: ReasoningEffort,
        model: Option<String>,
        fast_mode: bool,
    },
    SetFastMode {
        enabled: bool,
    },
    SetCwd {
        cwd: String,
    },
    RestoreAfterRewind {
        history_len: usize,
    },
}

pub(crate) enum TurnStateMutation {
    Finish {
        history_len: usize,
        meta: TurnMeta,
        update_context_token_history_len: bool,
    },
    InstallCheckpoint {
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    },
    InstallCheckpointAtHistoryIndex {
        kind: String,
        summary: String,
        first_live_index: usize,
        tokens_before: Option<u32>,
        history_len: usize,
    },
    SetCheckpoint {
        checkpoint: Option<ContextCheckpoint>,
    },
    SetCheckpointTokensAfterEstimate {
        tokens: u32,
        history_len: usize,
    },
    PruneRewindable {
        history_len: usize,
        identity: ContextTokenIdentity,
    },
}

#[derive(Default)]
pub(crate) struct DocumentChange {
    pub(crate) session_dirty: bool,
    #[allow(dead_code)]
    pub(crate) transcript_dirty: bool,
    pub(crate) records_unpersisted: bool,
    pub(crate) context_tokens_updated: bool,
    pub(crate) applied: bool,
    pub(crate) history_idx: Option<usize>,
    pub(crate) history_dirty_from: Option<usize>,
    pub(crate) block_id: Option<BlockId>,
    pub(crate) presentation_changed: bool,
    pub(crate) turn_meta: Option<TurnMeta>,
}

impl DocumentChange {
    fn canonical_changed(&self) -> bool {
        self.session_dirty
            || self.history_dirty_from.is_some()
            || self.transcript_dirty
            || self.records_unpersisted
    }
}

impl FullSessionDocument {
    pub(crate) fn new(
        session: Session,
        transcript: crate::app::transcript::LoadedTranscript,
    ) -> Self {
        Self {
            session,
            transcript,
        }
    }
}

impl StoreBackedSessionDocument {
    pub(crate) fn new(
        session: Session,
        transcript: crate::app::transcript::LoadedTranscript,
        live_session: smelt_core::session_runtime::LiveSession,
        store_head: smelt_store::StoreHead,
    ) -> Self {
        Self {
            session,
            transcript,
            live_session,
            store_head,
        }
    }
}

impl SessionDocument {
    pub(crate) fn from_full_session(
        session: Session,
        transcript: crate::app::transcript::LoadedTranscript,
    ) -> Self {
        Self {
            session,
            transcript,
            live_session: None,
            store_head: None,
        }
    }

    pub(crate) fn from_store(
        session: Session,
        live_session: LiveSession,
        store_head: smelt_store::StoreHead,
        transcript: crate::app::transcript::LoadedTranscript,
    ) -> Self {
        Self {
            session,
            transcript,
            live_session: Some(live_session),
            store_head: Some(store_head),
        }
    }

    fn append_history_item_with_transcript_block(
        session: &mut Session,
        live_session: Option<&mut LiveSession>,
        transcript: &mut TranscriptDocument,
        item: HistoryItem,
        block: Option<Block>,
        first_user_message: Option<String>,
    ) -> DocumentChange {
        let before_generation = transcript.history().record_dirty_generation();
        let before_dirty_from = transcript.history().record_dirty_from();
        let idx = live_session
            .as_ref()
            .map_or(session.history.len(), |live| live.history_len());
        if let Some(message) = first_user_message {
            if session.first_user_message.is_none() {
                session.first_user_message = Some(message.into());
                session.snapshot_metadata_at(idx + 1);
            }
        }
        if let Some(block) = block {
            transcript.push_with_origin(block, BlockOrigin::History(idx));
        }
        if let Some(live_session) = live_session {
            let appended_idx = live_session.append_history(item);
            debug_assert_eq!(appended_idx, idx);
        } else {
            session.history.push(item);
        }
        let history = transcript.history();
        DocumentChange {
            session_dirty: true,
            transcript_dirty: history.record_dirty_generation() != before_generation
                || history.record_dirty_from() != before_dirty_from,
            applied: true,
            history_idx: Some(idx),
            history_dirty_from: Some(idx),
            ..Default::default()
        }
    }

    fn apply_history(
        session: &mut Session,
        mut live_session: Option<&mut LiveSession>,
        transcript: Option<&mut TranscriptDocument>,
        mutation: HistoryMutation,
    ) -> DocumentChange {
        let rewrites_history = matches!(
            mutation,
            HistoryMutation::TruncateFrom { .. } | HistoryMutation::RewindTo { .. }
        );
        let before_session = rewrites_history.then(|| session.clone());
        let before_live = rewrites_history
            .then(|| {
                live_session
                    .as_deref()
                    .map(|live| (live.live_start, live.live_suffix_len()))
            })
            .flatten();
        let mut result = match mutation {
            HistoryMutation::AppendItem { item } => {
                let idx = if let Some(live_session) = live_session.as_deref_mut() {
                    live_session.append_history(item)
                } else {
                    let idx = session.history.len();
                    session.history.push(item);
                    idx
                };
                DocumentChange {
                    session_dirty: true,
                    history_idx: Some(idx),
                    history_dirty_from: Some(idx),
                    ..Default::default()
                }
            }
            HistoryMutation::CommitRequestItem {
                item,
                block,
                first_user_message,
            } => Self::append_history_item_with_transcript_block(
                session,
                live_session.as_deref_mut(),
                transcript.expect("request history mutation requires a transcript"),
                item,
                block.map(|block| *block),
                first_user_message,
            ),
            HistoryMutation::TruncateFrom {
                index,
                user_turn_count,
                identity,
            } => {
                let dirty_from = if let Some(live_session) = live_session.as_deref_mut() {
                    let index = index.min(live_session.history_len());
                    live_session.truncate_from(index, user_turn_count);
                    index
                } else {
                    let index = index.min(session.history.len());
                    session.history.truncate(index);
                    index
                };
                let turn_meta = session.prune_rewindable_snapshots(dirty_from);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                DocumentChange {
                    session_dirty: true,
                    history_dirty_from: Some(dirty_from),
                    turn_meta,
                    ..Default::default()
                }
            }
            HistoryMutation::RewindTo {
                index,
                user_turn_count,
                keep_checkpoint_at_boundary,
                identity,
            } => {
                let dirty_from = if let Some(live_session) = live_session.as_deref_mut() {
                    let index = index.min(live_session.history_len());
                    live_session.truncate_from(index, user_turn_count);
                    index
                } else {
                    let index = index.min(session.history.len());
                    session.history.truncate(index);
                    index
                };
                let turn_meta = session.restore_rewindable_snapshots_after_rewind(
                    dirty_from,
                    keep_checkpoint_at_boundary,
                );
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                DocumentChange {
                    session_dirty: true,
                    history_dirty_from: Some(dirty_from),
                    turn_meta,
                    ..Default::default()
                }
            }
        };
        if before_session
            .as_ref()
            .is_some_and(|before| before == session)
            && before_live
                == live_session
                    .as_deref()
                    .map(|live| (live.live_start, live.live_suffix_len()))
        {
            result.session_dirty = false;
            result.history_dirty_from = None;
        }
        result
    }

    fn apply_usage(session: &mut Session, mutation: UsageMutation) -> DocumentChange {
        match mutation {
            UsageMutation::RecordTokens {
                usage,
                history_len,
                identity,
            } => {
                let Some(tokens) = usage.context_tokens.or(usage.prompt_tokens) else {
                    return DocumentChange::default();
                };
                if tokens == 0 {
                    return DocumentChange::default();
                }
                let changed = session.context_tokens != Some(tokens)
                    || session.context_tokens_history_len != Some(history_len)
                    || session.context_token_identity.as_ref() != Some(&identity)
                    || session.display_context_tokens != Some(tokens)
                    || session.display_context_token_identity.as_ref() != Some(&identity);
                session.record_context_tokens(tokens, history_len, identity);
                DocumentChange {
                    session_dirty: changed,
                    context_tokens_updated: changed,
                    ..Default::default()
                }
            }
            UsageMutation::ClearTokenBaselineIfMismatched { identity } => {
                let changed = session
                    .context_token_identity
                    .as_ref()
                    .is_some_and(|current| current != &identity);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                DocumentChange {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            UsageMutation::Accumulate { usage, cost_usd } => {
                let before_cost = session.session_cost_usd;
                let before_usage = session.session_usage.clone();
                session.session_cost_usd += cost_usd;
                session.session_usage.accumulate(&usage);
                DocumentChange {
                    session_dirty: session.session_cost_usd != before_cost
                        || session.session_usage != before_usage,
                    ..Default::default()
                }
            }
        }
    }

    fn apply_metadata(session: &mut Session, mutation: MetadataMutation) -> DocumentChange {
        match mutation {
            MetadataMutation::SetTitle {
                title,
                slug,
                snapshot_history_len,
            } => {
                let changed =
                    session.title.as_ref() != Some(&title) || session.slug.as_ref() != Some(&slug);
                session.title = Some(title);
                session.slug = Some(slug);
                let snapshots_changed = session.snapshot_metadata_at(snapshot_history_len);
                DocumentChange {
                    session_dirty: changed || snapshots_changed,
                    ..Default::default()
                }
            }
            MetadataMutation::UpdateRuntime {
                updated_at_ms,
                mode,
                reasoning_effort,
                model,
                fast_mode,
            } => {
                let changed = session.updated_at_ms != updated_at_ms
                    || session.mode.as_deref() != Some(mode.as_str())
                    || session.reasoning_effort.as_ref() != Some(&reasoning_effort)
                    || session.model != model
                    || session.fast_mode != Some(fast_mode);
                session.updated_at_ms = updated_at_ms;
                session.mode = Some(mode);
                session.reasoning_effort = Some(reasoning_effort);
                session.model = model;
                session.fast_mode = Some(fast_mode);
                DocumentChange {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            MetadataMutation::SetFastMode { enabled } => {
                let changed = session.fast_mode != Some(enabled);
                session.fast_mode = Some(enabled);
                DocumentChange {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            MetadataMutation::SetCwd { cwd } => {
                let changed = session.cwd.as_deref() != Some(cwd.as_str());
                session.cwd = Some(cwd);
                DocumentChange {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            MetadataMutation::RestoreAfterRewind { history_len } => {
                let before_title = session.title.clone();
                let before_slug = session.slug.clone();
                let before_first_user_message = session.first_user_message.clone();
                let before_snapshots = session.metadata_snapshots.clone();
                session.restore_metadata_after_rewind(history_len);
                DocumentChange {
                    session_dirty: session.title != before_title
                        || session.slug != before_slug
                        || session.first_user_message != before_first_user_message
                        || session.metadata_snapshots != before_snapshots,
                    ..Default::default()
                }
            }
        }
    }

    fn apply_turn_state(session: &mut Session, mutation: TurnStateMutation) -> DocumentChange {
        match mutation {
            TurnStateMutation::Finish {
                history_len,
                meta,
                update_context_token_history_len,
            } => {
                let changed =
                    session.finish_turn_state(history_len, meta, update_context_token_history_len);
                DocumentChange {
                    session_dirty: changed,
                    applied: changed,
                    ..Default::default()
                }
            }
            TurnStateMutation::InstallCheckpoint {
                kind,
                summary,
                first_live_message_index,
                tokens_before,
            } => {
                let installed = session.install_context_checkpoint(
                    kind,
                    summary,
                    first_live_message_index,
                    tokens_before,
                );
                DocumentChange {
                    session_dirty: installed,
                    applied: installed,
                    ..Default::default()
                }
            }
            TurnStateMutation::InstallCheckpointAtHistoryIndex {
                kind,
                summary,
                first_live_index,
                tokens_before,
                history_len,
            } => {
                let installed = session.install_context_checkpoint_at_history_index(
                    kind,
                    summary,
                    first_live_index,
                    tokens_before,
                    history_len,
                );
                DocumentChange {
                    session_dirty: installed,
                    applied: installed,
                    ..Default::default()
                }
            }
            TurnStateMutation::SetCheckpoint { checkpoint } => {
                if session.checkpoint == checkpoint {
                    return DocumentChange::default();
                }
                session.checkpoint = checkpoint;
                DocumentChange {
                    session_dirty: true,
                    ..Default::default()
                }
            }
            TurnStateMutation::SetCheckpointTokensAfterEstimate {
                tokens,
                history_len,
            } => {
                let changed = session.record_checkpoint_tokens_after_estimate(tokens, history_len);
                DocumentChange {
                    session_dirty: changed,
                    applied: changed,
                    ..Default::default()
                }
            }
            TurnStateMutation::PruneRewindable {
                history_len,
                identity,
            } => {
                let before_turn_metas = session.turn_metas.len();
                let before_context_snapshots = session.context_snapshots.len();
                let before_metadata_snapshots = session.metadata_snapshots.len();
                let before_context_tokens = session.context_tokens;
                let before_context_token_identity = session.context_token_identity.clone();
                let turn_meta = session.prune_rewindable_snapshots(history_len);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                let changed = session.turn_metas.len() != before_turn_metas
                    || session.context_snapshots.len() != before_context_snapshots
                    || session.metadata_snapshots.len() != before_metadata_snapshots
                    || session.context_tokens != before_context_tokens
                    || session.context_token_identity != before_context_token_identity;
                DocumentChange {
                    session_dirty: changed,
                    turn_meta,
                    ..Default::default()
                }
            }
        }
    }

    fn apply_stream(
        parser: &mut StreamParser,
        transcript: &mut TranscriptDocument,
        mutation: StreamMutation,
    ) -> DocumentChange {
        let before_generation = transcript.history().record_dirty_generation();
        let before_dirty_from = transcript.history().record_dirty_from();
        let mut applied = false;
        let mut block_id = None;
        let mut presentation_changed = false;
        match mutation {
            StreamMutation::AppendThinking { delta } => {
                parser.append_streaming_thinking(transcript.history_mut(), &delta);
            }
            StreamMutation::FlushThinking => {
                parser.flush_streaming_thinking(transcript.history_mut());
            }
            StreamMutation::AppendText { delta } => {
                parser.append_streaming_text(transcript.history_mut(), &delta);
            }
            StreamMutation::FlushText => {
                parser.flush_streaming_text(transcript.history_mut());
            }
            StreamMutation::SyncActiveToolElapsed { now } => {
                parser.sync_active_tool_elapsed_at(transcript.history_mut(), now);
            }
            StreamMutation::StartTool { start, now } => {
                parser.start_tool(transcript.history_mut(), start, now);
            }
            StreamMutation::AppendToolOutput {
                invocation_id,
                line,
            } => {
                parser.append_active_output_line(transcript.history_mut(), invocation_id, line);
            }
            StreamMutation::AppendToolOutputSlice {
                block_id,
                chunk,
                line_start,
            } => {
                parser.append_tool_output_slice(
                    transcript.history_mut(),
                    block_id,
                    chunk,
                    line_start,
                );
            }
            StreamMutation::SetToolStatus {
                invocation_id,
                status,
                now,
            } => {
                parser.set_active_status(transcript.history_mut(), invocation_id, status, now);
            }
            StreamMutation::SetToolUserMessage {
                invocation_id,
                message,
            } => {
                parser.set_active_user_message(transcript.history_mut(), invocation_id, message);
            }
            StreamMutation::FinishTool {
                invocation_id,
                status,
                output,
                engine_elapsed,
                now,
            } => {
                parser.finish_tool(
                    transcript.history_mut(),
                    invocation_id,
                    status,
                    output,
                    engine_elapsed,
                    now,
                );
            }
            StreamMutation::FinalizeTools => {
                parser.finalize_active_tools(transcript.history_mut());
            }
            StreamMutation::PromoteToolDraft {
                stream_id,
                start,
                now,
            } => {
                applied = parser.promote_tool_draft(
                    transcript.history_mut(),
                    stream_id.as_deref(),
                    start,
                    now,
                );
            }
            StreamMutation::ClearToolDrafts => {
                parser.clear_tool_drafts(transcript.history_mut());
            }
            StreamMutation::StartToolDraft {
                stream_id,
                call_id,
                name,
                native,
            } => {
                let change = parser.start_tool_draft(
                    transcript.history_mut(),
                    stream_id,
                    call_id,
                    name,
                    native,
                );
                block_id = Some(change.block_id);
                presentation_changed = change.presentation_changed;
                applied = true;
            }
            StreamMutation::AppendToolDraft {
                stream_id,
                call_id,
                name,
                delta,
            } => {
                let change = parser.append_tool_draft(
                    transcript.history_mut(),
                    stream_id,
                    call_id,
                    name,
                    delta,
                );
                block_id = Some(change.block_id);
                presentation_changed = change.presentation_changed;
                applied = true;
            }
            StreamMutation::FinishToolDraft {
                stream_id,
                call_id,
                name,
                arguments,
            } => {
                let change = parser.finish_tool_draft(
                    transcript.history_mut(),
                    stream_id,
                    call_id,
                    name,
                    arguments,
                );
                block_id = Some(change.block_id);
                presentation_changed = change.presentation_changed;
                applied = true;
            }
            StreamMutation::SetToolDraftSummary {
                block_id: id,
                summary,
            } => {
                applied = parser.set_tool_draft_summary(transcript.history_mut(), id, summary);
                block_id = Some(id);
            }
            StreamMutation::StartExec { command } => {
                parser.start_exec(transcript.history_mut(), command);
            }
            StreamMutation::AppendExecOutput { chunk } => {
                parser.append_exec_output(transcript.history_mut(), chunk);
            }
            StreamMutation::FinishExec { final_output } => {
                parser.finish_exec(transcript.history_mut(), final_output);
            }
        }
        let history = transcript.history();
        let transcript_dirty = history.record_dirty_generation() != before_generation
            || history.record_dirty_from() != before_dirty_from;
        DocumentChange {
            transcript_dirty,
            records_unpersisted: transcript_dirty,
            applied,
            block_id,
            presentation_changed,
            ..Default::default()
        }
    }

    fn apply_to_transcript(
        transcript: &mut TranscriptDocument,
        mutation: TranscriptMutation,
    ) -> DocumentChange {
        let before_generation = transcript.history().record_dirty_generation();
        let before_dirty_from = transcript.history().record_dirty_from();
        let mut records_unpersisted = false;
        let mut block_id = None;
        let applied = match mutation {
            TranscriptMutation::AppendBlock { block } => {
                transcript.push(block);
                let applied = transcript.history().record_dirty_generation() != before_generation
                    || transcript.history().record_dirty_from() != before_dirty_from;
                if applied {
                    block_id = transcript.history().last_block_id();
                }
                applied
            }
            TranscriptMutation::InsertBlockBefore { before, block } => {
                block_id = transcript.insert_before(before, block);
                block_id.is_some()
            }
            TranscriptMutation::AppendAccepted { block } => {
                transcript.push_with_origin(
                    block,
                    smelt_transcript::transcript_model::BlockOrigin::Accepted,
                );
                block_id = transcript.history().last_block_id();
                block_id.is_some()
            }
            TranscriptMutation::PromoteAccepted { id, history_index } => {
                transcript.history_mut().promote_accepted(id, history_index)
            }
            TranscriptMutation::RemoveAccepted { id } => {
                transcript.history_mut().remove_accepted(id)
            }
            TranscriptMutation::RemoveSubmittedUser { id, history_index } => transcript
                .history_mut()
                .remove_submitted_user(id, history_index),
            TranscriptMutation::InsertCheckpointMarker {
                block_index,
                history_index,
                block,
            } => {
                transcript.insert_checkpoint_marker_at(block_index, history_index, block);
                transcript.history().record_dirty_generation() != before_generation
                    || transcript.history().record_dirty_from() != before_dirty_from
            }
            TranscriptMutation::RemoveUnoriginatedBlockAt { block_index } => {
                transcript.remove_unoriginated_at(block_index).is_some()
            }
            TranscriptMutation::ReplaceFromHistory {
                transcript: rebuilt,
            } => {
                transcript.replace_transcript(*rebuilt);
                transcript.history_mut().mark_changed();
                records_unpersisted = true;
                true
            }
            TranscriptMutation::TruncateTo { block_index } => {
                transcript.truncate_to(block_index);
                transcript.history().record_dirty_generation() != before_generation
                    || transcript.history().record_dirty_from() != before_dirty_from
            }
            TranscriptMutation::Clear => {
                transcript.clear();
                transcript.history().record_dirty_generation() != before_generation
                    || transcript.history().record_dirty_from() != before_dirty_from
            }
            TranscriptMutation::UpdateCompactionPreview { summary } => {
                block_id = transcript.set_compaction_preview(summary);
                block_id.is_some()
            }
            TranscriptMutation::ClearCompactionPreview => {
                block_id = transcript.clear_compaction_preview();
                block_id.is_some()
            }
            TranscriptMutation::RewriteBlock { id, block } => {
                transcript.history_mut().rewrite(id, block);
                true
            }
        };
        let history = transcript.history();
        DocumentChange {
            transcript_dirty: history.record_dirty_generation() != before_generation
                || history.record_dirty_from() != before_dirty_from,
            records_unpersisted,
            applied,
            block_id,
            ..Default::default()
        }
    }

    pub(crate) fn into_full(self) -> FullSessionDocument {
        FullSessionDocument::new(self.session, self.transcript)
    }

    pub(crate) fn into_store_backed(self) -> StoreBackedSessionDocument {
        StoreBackedSessionDocument::new(
            self.session,
            self.transcript,
            self.live_session
                .expect("store-backed session document includes live session state"),
            self.store_head
                .expect("store-backed session document includes a store head"),
        )
    }
}

/// Whether a history origin names a history item of the block's kind. Items
/// outside memory are durable and were checked when their records were saved.
fn origin_matches(
    record: &smelt_core::TranscriptBlockRecord,
    origin: BlockOrigin,
    history: SessionHistoryRef<'_>,
) -> bool {
    let BlockOrigin::History(index) = origin else {
        return true;
    };
    index < history.len()
        && history.in_memory(index).is_none_or(|item| {
            protocol::transcript_block_kind_matches_history_item(record.block.kind(), item)
        })
}

fn transcript_record_row(
    record_idx: usize,
    record: &smelt_core::TranscriptBlockRecordWithId,
) -> Result<smelt_store::StoredTranscriptBlock, smelt_store::StoreError> {
    let _perf = smelt_perf::perf::begin("session:project_transcript_record");
    smelt_transcript::transcript_model::transcript_block_row_with_block_idx(
        record_idx,
        record.block_id.get(),
        &record.record,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Content, ReasoningEffort, StyledLines};
    use smelt_core::session::{
        AuthoritativeContextTokens, ContextTokenIdentity, DisplayContextTokens, SessionMeta,
    };
    use std::collections::HashMap;

    const INVOCATION_ID: protocol::InvocationId = protocol::InvocationId::new(1);

    fn token_identity() -> ContextTokenIdentity {
        ContextTokenIdentity {
            model: Some("model-a".into()),
            api_base: Some("https://api.example.test".into()),
            provider_type: Some("openai".into()),
        }
    }

    fn meta_with_token_identity() -> SessionMeta {
        let identity = token_identity();
        SessionMeta {
            id: "session-a".into(),
            title: Some("Title".into()),
            slug: Some("title".into()),
            first_user_message: Some("hello".into()),
            created_at_ms: 10,
            updated_at_ms: 20,
            mode: Some("agent".into()),
            reasoning_effort: Some(ReasoningEffort::Low),
            model: Some("model-a".into()),
            fast_mode: Some(true),
            cwd: Some("/tmp".into()),
            parent_id: Some("parent".into()),
            authoritative_context_tokens: Some(AuthoritativeContextTokens {
                tokens: 42,
                history_len: 3,
                identity: identity.clone(),
            }),
            display_context_tokens: Some(DisplayContextTokens {
                tokens: 42,
                identity: Some(identity),
            }),
            history_len: Some(3),
            checkpoint: None,
            checkpoint_events: Default::default(),
            text_bytes: Some(128),
        }
    }

    fn empty_live_session_for(session: &Session, history_len: usize) -> LiveSession {
        let mut meta = meta_with_token_identity();
        meta.id = session.id.clone();
        meta.history_len = Some(history_len);
        LiveSession::from_parts(
            meta.id,
            std::path::PathBuf::new(),
            None,
            history_len,
            session
                .history
                .iter()
                .take(history_len)
                .filter_map(smelt_core::session_runtime::rewind_label)
                .count(),
        )
    }

    fn apply_usage(session: &mut Session, mutation: UsageMutation) -> DocumentChange {
        SessionDocument::apply_usage(session, mutation)
    }

    fn apply_metadata(session: &mut Session, mutation: MetadataMutation) -> DocumentChange {
        SessionDocument::apply_metadata(session, mutation)
    }

    fn apply_turn_state(session: &mut Session, mutation: TurnStateMutation) -> DocumentChange {
        SessionDocument::apply_turn_state(session, mutation)
    }

    fn apply_history(session: &mut Session, mutation: HistoryMutation) -> DocumentChange {
        SessionDocument::apply_history(session, None, None, mutation)
    }

    fn apply_transcript(
        transcript: &mut TranscriptDocument,
        mutation: TranscriptMutation,
    ) -> DocumentChange {
        SessionDocument::apply_to_transcript(transcript, mutation)
    }

    fn apply_history_with_transcript(
        session: &mut Session,
        transcript: &mut TranscriptDocument,
        mutation: HistoryMutation,
    ) -> DocumentChange {
        SessionDocument::apply_history(session, None, Some(transcript), mutation)
    }

    fn apply_stream_parser_transcript(
        parser: &mut StreamParser,
        transcript: &mut TranscriptDocument,
        mutation: StreamMutation,
    ) -> DocumentChange {
        SessionDocument::apply_stream(parser, transcript, mutation)
    }

    #[test]
    fn full_session_load_constructs_document_before_install_projection() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.id = "full-document".into();
        session
            .history
            .push(HistoryItem::user(Content::text("hello")));
        let transcript = crate::app::transcript::LoadedTranscript::full(
            smelt_transcript::content::transcript::Transcript::new(),
        );

        let projection = SessionDocument::from_full_session(session, transcript).into_full();

        assert_eq!(projection.session.id, "full-document");
        assert_eq!(projection.session.history.len(), 1);
    }

    #[test]
    fn record_token_usage_mutation_updates_accounting_and_reports_dirty() {
        let identity = token_identity();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_usage(
            &mut session,
            UsageMutation::RecordTokens {
                usage: TokenUsage {
                    context_tokens: Some(777),
                    ..Default::default()
                },
                history_len: 12,
                identity: identity.clone(),
            },
        );

        assert!(result.session_dirty);
        assert!(result.context_tokens_updated);
        assert_eq!(session.display_context_tokens(), Some(777));
        assert_eq!(session.context_tokens_history_len, Some(12));
        assert_eq!(session.context_token_identity, Some(identity.clone()));
        assert_eq!(session.display_context_token_identity, Some(identity));
    }

    #[test]
    fn zero_token_usage_mutation_is_noop() {
        let identity = token_identity();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_usage(
            &mut session,
            UsageMutation::RecordTokens {
                usage: TokenUsage {
                    context_tokens: Some(0),
                    ..Default::default()
                },
                history_len: 0,
                identity,
            },
        );

        assert!(!result.session_dirty);
        assert!(!result.context_tokens_updated);
        assert_eq!(session.display_context_tokens(), None);
    }

    #[test]
    fn clear_context_tokens_baseline_mutation_reports_dirty_on_identity_mismatch() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.record_context_tokens(100, 0, token_identity());
        let mut new_identity = token_identity();
        new_identity.model = Some("model-b".into());

        let result = apply_usage(
            &mut session,
            UsageMutation::ClearTokenBaselineIfMismatched {
                identity: new_identity,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.context_tokens, None);
        assert_eq!(session.context_token_identity, None);
    }

    #[test]
    fn accumulate_usage_mutation_updates_accounting_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_usage(
            &mut session,
            UsageMutation::Accumulate {
                usage: TokenUsage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    ..Default::default()
                },
                cost_usd: 0.25,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.session_cost_usd, 0.25);
        assert_eq!(session.session_usage.prompt_tokens, Some(10));
        assert_eq!(session.session_usage.completion_tokens, Some(5));
    }

    #[test]
    fn commit_request_history_item_sets_first_user_snapshot_once() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut transcript = TranscriptDocument::new();

        let result = apply_history_with_transcript(
            &mut session,
            &mut transcript,
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("hello")),
                block: None,
                first_user_message: Some("hello".into()),
            },
        );
        let second = apply_history_with_transcript(
            &mut session,
            &mut transcript,
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("second")),
                block: None,
                first_user_message: Some("ignored".into()),
            },
        );

        assert!(result.session_dirty);
        assert!(second.session_dirty);
        assert_eq!(session.first_user_message.as_deref(), Some("hello"));
        assert_eq!(
            session
                .metadata_snapshots
                .iter()
                .filter(|(idx, _)| *idx == 1)
                .count(),
            1
        );
    }

    #[test]
    fn title_mutation_updates_metadata_snapshot_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_metadata(
            &mut session,
            MetadataMutation::SetTitle {
                title: "New title".into(),
                slug: "new-title".into(),
                snapshot_history_len: 2,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.title.as_deref(), Some("New title"));
        assert_eq!(session.slug.as_deref(), Some("new-title"));
        assert!(session.metadata_snapshots.iter().any(|(idx, _)| *idx == 2));
    }

    #[test]
    fn restore_metadata_after_rewind_mutation_restores_snapshot() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        apply_metadata(
            &mut session,
            MetadataMutation::SetTitle {
                title: "First".into(),
                slug: "first".into(),
                snapshot_history_len: 1,
            },
        );
        apply_metadata(
            &mut session,
            MetadataMutation::SetTitle {
                title: "Second".into(),
                slug: "second".into(),
                snapshot_history_len: 2,
            },
        );

        let result = apply_metadata(
            &mut session,
            MetadataMutation::RestoreAfterRewind { history_len: 1 },
        );

        assert!(result.session_dirty);
        assert_eq!(session.title.as_deref(), Some("First"));
        assert_eq!(session.slug.as_deref(), Some("first"));
    }

    #[test]
    fn rewind_to_start_mutation_clears_history_snapshots_and_tokens() {
        let identity = token_identity();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::user(Content::text("prompt")));
        session.record_context_tokens(500, 1, identity.clone());
        session.snapshot_context_at(1);
        apply_metadata(
            &mut session,
            MetadataMutation::SetTitle {
                title: "Title".into(),
                slug: "title".into(),
                snapshot_history_len: 1,
            },
        );
        session.turn_metas.push((
            1,
            TurnMeta {
                elapsed_ms: 10,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
            },
        ));

        let result = apply_history(
            &mut session,
            HistoryMutation::RewindTo {
                index: 0,
                user_turn_count: None,
                keep_checkpoint_at_boundary: false,
                identity,
            },
        );

        assert!(result.session_dirty);
        assert!(session.history.is_empty());
        assert!(session.turn_metas.is_empty());
        assert!(session.context_snapshots.is_empty());
        assert_eq!(session.current_context_tokens(session.history.len()), None);
        assert_eq!(session.title, None);
        assert_eq!(session.slug, None);
    }

    #[test]
    fn runtime_metadata_mutation_updates_save_metadata_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_metadata(
            &mut session,
            MetadataMutation::UpdateRuntime {
                updated_at_ms: 42,
                mode: "agent".into(),
                reasoning_effort: ReasoningEffort::High,
                model: Some("provider/model".into()),
                fast_mode: true,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.updated_at_ms, 42);
        assert_eq!(session.mode.as_deref(), Some("agent"));
        assert_eq!(session.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(session.model.as_deref(), Some("provider/model"));
        assert_eq!(session.fast_mode, Some(true));
    }

    #[test]
    fn cwd_mutation_updates_session_metadata_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_metadata(
            &mut session,
            MetadataMutation::SetCwd {
                cwd: "/repo".into(),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn checkpoint_mutation_installs_checkpoint_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("old")),
            protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("old reply")),
                None,
                Vec::new(),
            )),
            HistoryItem::user(Content::text("recent")),
        ];

        let result = apply_turn_state(
            &mut session,
            TurnStateMutation::InstallCheckpoint {
                kind: "compaction".into(),
                summary: "summary".into(),
                first_live_message_index: 2,
                tokens_before: Some(100),
            },
        );

        assert!(result.applied);
        assert!(result.session_dirty);
        assert_eq!(session.checkpoint.as_ref().unwrap().first_live_index, 2);
    }

    #[test]
    fn finish_turn_state_mutation_appends_meta_and_snapshots_context() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.context_tokens = Some(123);
        session.history = vec![HistoryItem::user(Content::text("hello"))];
        let meta = TurnMeta {
            elapsed_ms: 10,
            avg_tps: Some(2.0),
            display_tps: Some(2.0),
            interrupted: false,
        };

        let result = apply_turn_state(
            &mut session,
            TurnStateMutation::Finish {
                history_len: 7,
                meta,
                update_context_token_history_len: true,
            },
        );

        assert!(result.session_dirty);
        assert!(result.applied);
        assert_eq!(session.turn_metas.len(), 1);
        assert_eq!(session.turn_metas[0].0, 7);
        assert_eq!(session.context_tokens_history_len, Some(7));
        assert!(session.context_snapshots.iter().any(|(idx, _)| *idx == 7));
    }

    #[test]
    fn prune_rewindable_state_mutation_truncates_snapshots_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let meta = TurnMeta {
            elapsed_ms: 10,
            avg_tps: None,
            display_tps: None,
            interrupted: false,
        };
        session.turn_metas.push((2, meta.clone()));
        session.turn_metas.push((4, meta));
        session.snapshot_metadata_at(2);
        session.snapshot_metadata_at(4);

        let result = apply_turn_state(
            &mut session,
            TurnStateMutation::PruneRewindable {
                history_len: 2,
                identity: token_identity(),
            },
        );

        assert!(result.session_dirty);
        assert!(result.turn_meta.is_some());
        assert_eq!(session.turn_metas.len(), 1);
        assert_eq!(session.metadata_snapshots.len(), 1);
    }

    #[test]
    fn append_history_item_mutation_updates_history_and_reports_dirty_range() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::user(Content::text("existing")));

        let result = apply_history(
            &mut session,
            HistoryMutation::AppendItem {
                item: HistoryItem::user(Content::text("new")),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(result.history_idx, Some(1));
        assert_eq!(result.history_dirty_from, Some(1));
        assert_eq!(session.history.len(), 2);
    }

    #[test]
    fn append_history_with_transcript_block_updates_both_projections() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::user(Content::text("existing")));
        let mut transcript = TranscriptDocument::new();

        let result = apply_history_with_transcript(
            &mut session,
            &mut transcript,
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("new")),
                block: Some(Box::new(Block::User {
                    text: "new".into(),
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                })),
                first_user_message: None,
            },
        );

        assert!(result.session_dirty);
        assert!(result.transcript_dirty);
        assert_eq!(result.history_idx, Some(1));
        assert_eq!(result.history_dirty_from, Some(1));
        assert_eq!(session.history.len(), 2);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(1))
        );
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn commit_request_history_item_snapshots_first_user_message_atomically() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut transcript = TranscriptDocument::new();

        let result = apply_history_with_transcript(
            &mut session,
            &mut transcript,
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("new")),
                block: Some(Box::new(Block::User {
                    text: "new".into(),
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                })),
                first_user_message: Some("new".into()),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(result.history_idx, Some(0));
        assert_eq!(session.first_user_message.as_deref(), Some("new"));
        assert_eq!(session.metadata_snapshots.len(), 1);
        assert_eq!(session.metadata_snapshots[0].0, 1);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn live_append_history_with_transcript_block_updates_both_projections() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut live_session = empty_live_session_for(&session, 0);
        let mut transcript = TranscriptDocument::new();

        let result = SessionDocument::apply_history(
            &mut session,
            Some(&mut live_session),
            Some(&mut transcript),
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("new")),
                block: Some(Box::new(Block::User {
                    text: "new".into(),
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                })),
                first_user_message: None,
            },
        );

        assert!(result.session_dirty);
        assert!(result.transcript_dirty);
        assert_eq!(result.history_idx, Some(0));
        assert_eq!(live_session.history_len(), 1);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn runtime_apply_routes_history_append_to_live_session() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.live_session = Some(empty_live_session_for(&session, 0));
        let result = document.apply_history(
            &mut session,
            HistoryMutation::AppendItem {
                item: HistoryItem::user(Content::text("live")),
            },
        );

        assert_eq!(result.history_idx, Some(0));
        assert!(session.history.is_empty());
        assert_eq!(
            document
                .live_session
                .as_ref()
                .expect("live session")
                .history_len(),
            1
        );
        assert_eq!(document.revision(), 1);
        assert_eq!(document.dirty_history_from_for_test(), Some(0));
    }

    #[test]
    fn runtime_apply_routes_streaming_mutation_to_parser_and_transcript() {
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        let mut parser = StreamParser::new();

        let result = document.apply_stream(
            &mut parser,
            StreamMutation::AppendText {
                delta: "hello".into(),
            },
        );

        assert!(result.transcript_dirty);
        assert_eq!(document.transcript.history().record_dirty_from(), Some(0));
        assert_eq!(document.revision(), 1);
    }

    #[test]
    fn truncate_history_mutation_updates_history_and_reports_dirty_range() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("one")),
            HistoryItem::user(Content::text("two")),
            HistoryItem::user(Content::text("three")),
        ];

        session.snapshot_context_at(3);
        let result = apply_history(
            &mut session,
            HistoryMutation::TruncateFrom {
                index: 1,
                user_turn_count: None,
                identity: token_identity(),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(result.history_dirty_from, Some(1));
        assert_eq!(session.history.len(), 1);
        assert!(session.context_snapshots.is_empty());
    }

    #[test]
    fn append_transcript_block_mutation_updates_transcript_and_reports_dirty() {
        let mut transcript = TranscriptDocument::new();

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::AppendBlock {
                block: Block::Text {
                    content: "record-only text".into(),
                },
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert!(!transcript.history().is_empty());
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn empty_append_transcript_block_mutation_reports_noop() {
        let mut transcript = TranscriptDocument::new();

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::AppendBlock {
                block: Block::Text {
                    content: "  \n\t  ".into(),
                },
            },
        );

        assert!(!result.applied);
        assert!(!result.transcript_dirty);
        assert!(transcript.history().is_empty());
        assert_eq!(transcript.history().record_dirty_from(), None);
    }

    #[test]
    fn rewrite_transcript_block_mutation_updates_transcript_and_reports_dirty() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "old".into(),
        });
        let id = transcript.history().order[0];
        transcript.history_mut().clear_record_dirty();

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::RewriteBlock {
                id,
                block: Block::Text {
                    content: "new".into(),
                },
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
        assert!(matches!(
            transcript.history().block(id),
            Some(Block::Text { content }) if content == "new"
        ));
    }

    #[test]
    fn replace_transcript_from_history_reports_dirty_and_unpersisted() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "old".into(),
        });
        transcript.history_mut().clear_record_dirty();

        let mut rebuilt = smelt_transcript::content::transcript::Transcript::new();
        rebuilt.push_with_origin(
            Block::Text {
                content: "rebuilt".into(),
            },
            BlockOrigin::History(0),
        );

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::ReplaceFromHistory {
                transcript: Box::new(rebuilt),
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert!(result.records_unpersisted);
        assert_eq!(transcript.history().len(), 1);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn checkpoint_marker_transcript_mutation_reports_dirty() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "history text".into(),
        });
        transcript.history_mut().clear_record_dirty();

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::InsertCheckpointMarker {
                block_index: 0,
                history_index: 3,
                block: Block::Compacted {
                    summary: "summary".into(),
                    kept: Vec::new(),
                },
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::Checkpoint { history_index: 3 })
        );
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn remove_unoriginated_transcript_mutation_reports_when_applied() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "temporary".into(),
        });
        transcript.push_with_origin(
            Block::Text {
                content: "history".into(),
            },
            BlockOrigin::History(0),
        );
        transcript.history_mut().clear_record_dirty();

        let removed = apply_transcript(
            &mut transcript,
            TranscriptMutation::RemoveUnoriginatedBlockAt { block_index: 0 },
        );
        let skipped = apply_transcript(
            &mut transcript,
            TranscriptMutation::RemoveUnoriginatedBlockAt { block_index: 0 },
        );

        assert!(removed.applied);
        assert!(removed.transcript_dirty);
        assert!(!skipped.applied);
        assert!(!skipped.transcript_dirty);
        assert_eq!(transcript.history().len(), 1);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn truncate_transcript_mutation_reports_dirty_when_it_removes_blocks() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "one".into(),
        });
        transcript.push(Block::Text {
            content: "two".into(),
        });
        transcript.history_mut().clear_record_dirty();

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::TruncateTo { block_index: 1 },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert_eq!(transcript.history().len(), 1);
        assert_eq!(transcript.history().record_dirty_from(), Some(1));
    }

    #[test]
    fn clear_transcript_mutation_reports_dirty_and_removes_blocks() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "one".into(),
        });
        transcript.push(Block::Text {
            content: "two".into(),
        });
        transcript.history_mut().clear_record_dirty();

        let result = apply_transcript(&mut transcript, TranscriptMutation::Clear);

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert!(transcript.history().is_empty());
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn compaction_preview_mutations_update_transcript_and_report_dirty() {
        let mut transcript = TranscriptDocument::new();

        let updated = apply_transcript(
            &mut transcript,
            TranscriptMutation::UpdateCompactionPreview {
                summary: "streaming summary".into(),
            },
        );
        let id = updated.block_id.expect("preview block id");

        assert!(updated.applied);
        assert!(updated.transcript_dirty);
        assert_eq!(transcript.compaction_preview_id(), Some(id));
        assert_eq!(transcript.history().record_dirty_from(), Some(0));

        transcript.history_mut().clear_record_dirty();
        let cleared = apply_transcript(&mut transcript, TranscriptMutation::ClearCompactionPreview);

        assert!(cleared.applied);
        assert!(cleared.transcript_dirty);
        assert_eq!(cleared.block_id, Some(id));
        assert_eq!(transcript.compaction_preview_id(), None);
    }

    #[test]
    fn stream_parser_noop_does_not_report_ambient_transcript_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "already dirty".into(),
        });
        assert_eq!(transcript.history().record_dirty_from(), Some(0));

        let result = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::ClearToolDrafts,
        );

        assert!(!result.transcript_dirty);
        assert!(!result.records_unpersisted);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn streaming_text_mutation_updates_transcript_and_reports_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();

        let result = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::AppendText {
                delta: "hello".into(),
            },
        );

        assert!(result.transcript_dirty);
        assert!(result.records_unpersisted);
        assert!(!transcript.history().is_empty());
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn active_tool_elapsed_mutation_does_not_dirty_persistence() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        let now = Instant::now();

        apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::StartTool {
                start: ToolStart {
                    invocation_id: INVOCATION_ID,
                    call_id: "tool-1".into(),
                    name: "bash".into(),
                    summary: StyledLines::from_plain("bash"),
                    args: HashMap::new(),
                    preview_output: None,
                    called_at_ms: 0,
                },
                now,
            },
        );
        transcript.history_mut().clear_record_dirty();

        let result = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::SyncActiveToolElapsed {
                now: now + Duration::from_secs(2),
            },
        );

        assert!(!result.transcript_dirty);
        assert!(!result.records_unpersisted);
        assert_eq!(transcript.history().record_dirty_from(), None);
        assert_eq!(
            transcript
                .history()
                .tool_state(transcript.history().order[0])
                .and_then(|state| state.elapsed),
            Some(Duration::from_secs(2)),
        );
    }

    #[test]
    fn tool_lifecycle_mutations_update_transcript_and_report_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        let now = Instant::now();

        let started = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::StartTool {
                start: ToolStart {
                    invocation_id: INVOCATION_ID,
                    call_id: "tool-1".into(),
                    name: "bash".into(),
                    summary: StyledLines::from_plain("bash"),
                    args: HashMap::new(),
                    preview_output: None,
                    called_at_ms: 0,
                },
                now,
            },
        );
        let output = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::AppendToolOutput {
                invocation_id: INVOCATION_ID,
                line: "done".into(),
            },
        );
        let finished = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::FinishTool {
                invocation_id: INVOCATION_ID,
                status: ToolStatus::Ok,
                output: None,
                engine_elapsed: None,
                now,
            },
        );

        assert!(started.transcript_dirty);
        assert!(output.transcript_dirty);
        assert!(finished.transcript_dirty);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
        assert_eq!(transcript.history().len(), 1);
    }

    #[test]
    fn draft_tool_mutations_update_transcript_and_report_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        let now = Instant::now();

        let appended = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::AppendToolDraft {
                stream_id: "stream-1".into(),
                call_id: Some("tool-1".into()),
                name: Some("bash".into()),
                delta: "{}".into(),
            },
        );
        transcript.history_mut().clear_record_dirty();
        let promoted = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::PromoteToolDraft {
                stream_id: Some("stream-1".into()),
                start: ToolStart {
                    invocation_id: INVOCATION_ID,
                    call_id: "tool-1".into(),
                    name: "bash".into(),
                    summary: StyledLines::from_plain("bash"),
                    args: HashMap::new(),
                    preview_output: None,
                    called_at_ms: 0,
                },
                now,
            },
        );

        assert!(appended.transcript_dirty);
        assert!(promoted.applied);
        assert!(promoted.transcript_dirty);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
        assert_eq!(transcript.history().len(), 1);
    }

    #[test]
    fn exec_lifecycle_mutations_update_transcript_and_report_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();

        let started = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::StartExec {
                command: "echo hi".into(),
            },
        );
        let output = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::AppendExecOutput { chunk: "hi".into() },
        );
        let finished = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::FinishExec {
                final_output: Some("authoritative hi".into()),
            },
        );

        assert!(started.transcript_dirty);
        assert!(output.transcript_dirty);
        assert!(finished.transcript_dirty);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
        assert_eq!(transcript.history().len(), 1);
        let exec_id = transcript.history().last_block_id().expect("exec block id");
        let Block::Exec { output, .. } = transcript.history().block(exec_id).unwrap() else {
            panic!("expected exec block");
        };
        assert_eq!(output.snapshot(), "authoritative hi");
    }

    fn runtime_metadata() -> RuntimeSessionMetadata {
        RuntimeSessionMetadata {
            updated_at_ms: 20,
            mode: "agent".into(),
            reasoning_effort: ReasoningEffort::Low,
            model: Some("model-a".into()),
            fast_mode: false,
        }
    }

    /// A store the test writes to directly, standing in for the persist actor.
    struct TestStore {
        _root: tempfile::TempDir,
        store: smelt_store::Store,
    }

    impl TestStore {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let store = smelt_store::Store::new(root.path());
            Self { _root: root, store }
        }

        fn commit(&self, save: &crate::persist::SessionSave) -> crate::persist::DurableSave {
            let head = self
                .store
                .writer(&save.identity.id)
                .unwrap()
                .commit(&save.identity, &save.commit)
                .unwrap();
            crate::persist::DurableSave {
                seq: save.seq,
                head,
            }
        }

        /// What one full write of `session` and `transcript` stores, under a fresh id.
        fn reference(
            &self,
            session: &Session,
            transcript: &TranscriptDocument,
        ) -> (
            smelt_store::SessionState,
            Vec<HistoryItem>,
            Vec<smelt_store::StoredTranscriptBlock>,
        ) {
            let mut reference = session.clone();
            reference.id = "f".repeat(64);
            let mut commit = smelt_core::session::full_commit(&reference).unwrap();
            commit.transcript = Some(smelt_store::TranscriptRecordSuffix {
                start: 0,
                records: expected_records(transcript, &session.history),
            });
            self.store
                .writer(&reference.id)
                .unwrap()
                .commit(&smelt_core::session::store_identity(&reference), &commit)
                .unwrap();
            let stored = self.stored(&reference.id);
            self.store.delete(&reference.id).unwrap();
            stored
        }

        fn stored(
            &self,
            id: &str,
        ) -> (
            smelt_store::SessionState,
            Vec<HistoryItem>,
            Vec<smelt_store::StoredTranscriptBlock>,
        ) {
            let reader = self.store.reader(id).unwrap();
            let state = reader.state().unwrap();
            let history = reader
                .history_range(0, state.head.history_len.get())
                .unwrap();
            let transcript = reader
                .transcript_range(0, state.head.transcript_record_count.get())
                .unwrap();
            (state, history, transcript)
        }
    }

    fn expected_records(
        transcript: &TranscriptDocument,
        history: &[HistoryItem],
    ) -> Vec<smelt_store::StoredTranscriptBlock> {
        transcript
            .history()
            .block_records_with_ids()
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let mut record = record.clone();
                if record.record.origin.is_some_and(|origin| {
                    !origin_matches(
                        &record.record,
                        origin,
                        SessionHistoryRef::Materialized(history),
                    )
                }) {
                    record.record.origin = None;
                }
                transcript_record_row(index, &record).unwrap()
            })
            .collect()
    }

    /// The stored session matches one full write of the document, ignoring only
    /// the session id.
    fn assert_store_matches_document(
        store: &TestStore,
        session: &Session,
        document: &TuiSessionDocument,
        context: &str,
    ) {
        let (mut state, history, transcript) = store.stored(&session.id);
        let (mut expected, expected_history, expected_transcript) =
            store.reference(session, &document.transcript);
        state.identity.id.clear();
        expected.identity.id.clear();
        assert_eq!(history, expected_history, "{context}: history");
        assert_eq!(transcript, expected_transcript, "{context}: transcript");
        assert_eq!(state, expected, "{context}: session state");
    }

    fn user_block(text: &str) -> Block {
        Block::User {
            text: text.into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        }
    }

    fn apply_generated_mutation(
        document: &mut TuiSessionDocument,
        session: &mut Session,
        state: u64,
        label: &str,
    ) {
        let history_len = session.history.len();
        match state % 7 {
            0 | 1 => {
                document.apply_history(
                    session,
                    HistoryMutation::CommitRequestItem {
                        item: HistoryItem::user(Content::text(label)),
                        block: Some(Box::new(user_block(label))),
                        first_user_message: Some(label.into()),
                    },
                );
            }
            2 => {
                document.apply_history(
                    session,
                    HistoryMutation::AppendItem {
                        item: HistoryItem::assistant(protocol::AssistantStep::terminal(
                            Some(Content::text(label)),
                            None,
                            Vec::new(),
                        )),
                    },
                );
            }
            3 => {
                document.apply_transcript(TranscriptMutation::AppendBlock {
                    block: Block::Text {
                        content: label.into(),
                    },
                });
            }
            4 => {
                document.apply_history(
                    session,
                    HistoryMutation::TruncateFrom {
                        index: (state as usize >> 8) % (history_len + 1),
                        user_turn_count: None,
                        identity: token_identity(),
                    },
                );
            }
            5 => {
                document.apply_metadata(
                    session,
                    MetadataMutation::SetTitle {
                        title: label.into(),
                        slug: label.into(),
                        snapshot_history_len: history_len,
                    },
                );
            }
            _ => {
                document.apply_metadata(
                    session,
                    MetadataMutation::SetCwd {
                        cwd: format!("/tmp/{label}"),
                    },
                );
            }
        }
    }

    #[test]
    fn pipelined_saves_with_failures_converge_on_the_document() {
        for seed in 0..48_u64 {
            let store = TestStore::new();
            let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
            let mut document = TuiSessionDocument::new(TranscriptDocument::new());
            let mut queued: VecDeque<crate::persist::SessionSave> = VecDeque::new();
            let mut writer_failed = false;
            let mut state = seed.wrapping_add(1);
            for step in 0..40 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let label = format!("{seed}:{step}");
                match (state >> 33) % 6 {
                    0..=2 => {
                        apply_generated_mutation(&mut document, &mut session, state >> 3, &label)
                    }
                    3 => {
                        if let Some(save) = document
                            .capture_save(&mut session, runtime_metadata())
                            .unwrap()
                        {
                            queued.push_back(save);
                        }
                    }
                    4 => {
                        // The actor commits the oldest save, skipping ones that build
                        // on a failure until a resync arrives.
                        if let Some(save) = queued.pop_front() {
                            if writer_failed && !save.resync {
                                continue;
                            }
                            writer_failed = false;
                            let durable = store.commit(&save);
                            assert!(document.acknowledge(durable, &mut session));
                        }
                    }
                    _ => {
                        if let Some(save) = queued.pop_front() {
                            if !writer_failed || save.resync {
                                writer_failed = true;
                                document.fail_save(save.seq);
                            }
                        }
                    }
                }
            }
            loop {
                if let Some(save) = document
                    .capture_save(&mut session, runtime_metadata())
                    .unwrap()
                {
                    queued.push_back(save);
                }
                let Some(save) = queued.pop_front() else {
                    break;
                };
                if writer_failed && !save.resync {
                    continue;
                }
                writer_failed = false;
                let durable = store.commit(&save);
                document.acknowledge(durable, &mut session);
            }
            assert!(!document.has_unsaved_work(), "seed {seed}");
            if store.store.exists(&session.id).unwrap() {
                assert_store_matches_document(&store, &session, &document, &format!("seed {seed}"));
            }
        }
    }

    #[test]
    fn unchanged_document_captures_nothing() {
        let store = TestStore::new();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        assert!(document
            .capture_save(&mut session, runtime_metadata())
            .unwrap()
            .is_none());
        apply_generated_mutation(&mut document, &mut session, 0, "first");
        let save = document
            .capture_save(&mut session, runtime_metadata())
            .unwrap()
            .expect("first change is captured");
        assert!(document.has_unsaved_work());
        assert!(!document.has_uncaptured_work());
        document.acknowledge(store.commit(&save), &mut session);
        assert!(!document.has_unsaved_work());
        assert!(document
            .capture_save(&mut session, runtime_metadata())
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_save_writes_only_the_changed_suffix() {
        let store = TestStore::new();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        for index in 0..32 {
            apply_generated_mutation(&mut document, &mut session, 0, &format!("user {index}"));
        }
        let save = document
            .capture_save(&mut session, runtime_metadata())
            .unwrap()
            .unwrap();
        document.acknowledge(store.commit(&save), &mut session);

        apply_generated_mutation(&mut document, &mut session, 0, "next");
        let save = document
            .capture_save(&mut session, runtime_metadata())
            .unwrap()
            .unwrap();
        let history = save.commit.history.as_ref().expect("history suffix");
        assert_eq!(history.start, 32);
        assert_eq!(history.items.len(), 1);
        let transcript = save.commit.transcript.as_ref().expect("transcript suffix");
        assert_eq!(transcript.start, 32);
        assert_eq!(transcript.records.len(), 1);
        assert!(
            save.commit.first_user_message.is_none(),
            "unchanged first message is not rewritten"
        );
        assert!(
            save.commit.checkpoint.is_none(),
            "unchanged checkpoint is not rewritten"
        );
        document.acknowledge(store.commit(&save), &mut session);
        assert_store_matches_document(&store, &session, &document, "suffix save");
    }

    #[test]
    fn a_failed_save_resyncs_everything_after_the_durable_head() {
        let store = TestStore::new();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        apply_generated_mutation(&mut document, &mut session, 0, "durable");
        let save = document
            .capture_save(&mut session, runtime_metadata())
            .unwrap()
            .unwrap();
        document.acknowledge(store.commit(&save), &mut session);

        apply_generated_mutation(&mut document, &mut session, 0, "lost");
        let lost = document
            .capture_save(&mut session, runtime_metadata())
            .unwrap()
            .unwrap();
        apply_generated_mutation(&mut document, &mut session, 0, "queued after loss");
        let skipped = document
            .capture_save(&mut session, runtime_metadata())
            .unwrap()
            .unwrap();
        assert!(!skipped.resync);
        assert!(document.fail_save(lost.seq));
        assert!(!document.fail_save(skipped.seq), "one failure resyncs once");
        assert!(document.has_uncaptured_work());

        let resync = document
            .capture_save(&mut session, runtime_metadata())
            .unwrap()
            .unwrap();
        assert!(resync.resync);
        assert_eq!(resync.commit.history.as_ref().unwrap().start, 1);
        document.acknowledge(store.commit(&resync), &mut session);
        assert!(!document.has_unsaved_work());
        assert_store_matches_document(&store, &session, &document, "after resync");
    }
}
