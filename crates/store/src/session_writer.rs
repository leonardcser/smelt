use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Result, StoreError};
use crate::filesystem::{ensure_private_directory_all, reject_symlink, sync_directory};
use crate::lineage_access::OwnedLineageWriter;
use crate::session_commit::{
    SaveReceipt, SessionCommit, SessionCommitFailure, StoreHead, SubmitTurn, SubmitTurnReceipt,
    TurnTransition, TurnTransitionReceipt,
};
use crate::SessionStoreLayout;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionBatchBarrier {
    #[default]
    None,
    Turn,
    Lifecycle,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEventCommand {
    Save {
        session: SessionCommit,
    },
    SubmitTurn {
        command: SubmitTurn,
    },
    TurnTransition {
        command: TurnTransition,
    },
    CompactSave {
        session: crate::CompactSessionCommit,
    },
    CompactSubmitTurn {
        command: crate::CompactSubmitTurn,
    },
    CompactTurnTransition {
        command: crate::CompactTurnTransition,
    },
}

impl SessionEventCommand {
    pub fn legacy_session(&self) -> Option<&SessionCommit> {
        match self {
            Self::Save { session } => Some(session),
            Self::SubmitTurn { command } => Some(&command.session),
            Self::TurnTransition { command } => Some(&command.session),
            Self::CompactSave { .. }
            | Self::CompactSubmitTurn { .. }
            | Self::CompactTurnTransition { .. } => None,
        }
    }

    fn journal_version(&self) -> u16 {
        match self {
            Self::Save { .. } | Self::SubmitTurn { .. } | Self::TurnTransition { .. } => 1,
            Self::CompactSave { .. }
            | Self::CompactSubmitTurn { .. }
            | Self::CompactTurnTransition { .. } => 2,
        }
    }
}

#[derive(serde::Serialize)]
struct SessionEventBatchIdSeed<'a> {
    schema: &'static str,
    document_revision: u64,
    barrier: SessionBatchBarrier,
    command: &'a SessionEventCommand,
}

fn session_event_batch_id(
    document_revision: u64,
    barrier: SessionBatchBarrier,
    command: &SessionEventCommand,
) -> String {
    let seed = SessionEventBatchIdSeed {
        schema: if command.journal_version() == 1 {
            "smelt-session-event-batch-v2"
        } else {
            "smelt-session-event-batch-v3"
        },
        document_revision,
        barrier,
        command,
    };
    let bytes = serde_json::to_vec(&seed).expect("session event batches serialize for IDs");
    crate::object::sha256_hex(&bytes)
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SessionEventBatch {
    pub document_revision: u64,
    pub batch_id: String,
    pub barrier: SessionBatchBarrier,
    pub command: SessionEventCommand,
}

impl SessionEventBatch {
    fn new(
        document_revision: u64,
        barrier: SessionBatchBarrier,
        command: SessionEventCommand,
    ) -> Self {
        Self {
            document_revision,
            batch_id: session_event_batch_id(document_revision, barrier, &command),
            barrier,
            command,
        }
    }

    pub fn save(
        document_revision: u64,
        session: SessionCommit,
        barrier: SessionBatchBarrier,
    ) -> Self {
        let command = SessionEventCommand::Save { session };
        Self::new(document_revision, barrier, command)
    }

    pub fn submit_turn(document_revision: u64, command: SubmitTurn) -> Self {
        let barrier = SessionBatchBarrier::Turn;
        let command = SessionEventCommand::SubmitTurn { command };
        Self::new(document_revision, barrier, command)
    }

    pub fn turn_transition(document_revision: u64, command: TurnTransition) -> Self {
        let barrier = if command.state.is_terminal() {
            SessionBatchBarrier::Lifecycle
        } else {
            SessionBatchBarrier::Turn
        };
        let command = SessionEventCommand::TurnTransition { command };
        Self::new(document_revision, barrier, command)
    }

    pub fn compact_save(
        document_revision: u64,
        session: crate::CompactSessionCommit,
        barrier: SessionBatchBarrier,
    ) -> Self {
        let command = SessionEventCommand::CompactSave { session };
        Self::new(document_revision, barrier, command)
    }

    pub fn compact_submit_turn(document_revision: u64, command: crate::CompactSubmitTurn) -> Self {
        let barrier = SessionBatchBarrier::Turn;
        let command = SessionEventCommand::CompactSubmitTurn { command };
        Self::new(document_revision, barrier, command)
    }

    pub fn compact_turn_transition(
        document_revision: u64,
        command: crate::CompactTurnTransition,
    ) -> Self {
        let barrier = if command.state.is_terminal() {
            SessionBatchBarrier::Lifecycle
        } else {
            SessionBatchBarrier::Turn
        };
        let command = SessionEventCommand::CompactTurnTransition { command };
        Self::new(document_revision, barrier, command)
    }

    pub fn legacy_session(&self) -> Option<&SessionCommit> {
        self.command.legacy_session()
    }

    fn expected_batch_id(&self) -> String {
        session_event_batch_id(self.document_revision, self.barrier, &self.command)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionJournalRecovery {
    pub complete_batches: usize,
    pub ignored_incomplete_tail: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct JournalRecord {
    version: u16,
    batch_id: String,
    payload_len: u64,
    checksum: String,
    payload: serde_json::Value,
}

impl JournalRecord {
    fn decode(self) -> Result<SessionEventBatch> {
        let payload_bytes = serde_json::to_vec(&self.payload)?;
        if self.payload_len != payload_bytes.len() as u64 {
            return Err(StoreError::Integrity(
                "session journal payload length mismatch".into(),
            ));
        }
        if crate::object::sha256_hex(&payload_bytes) != self.checksum {
            return Err(StoreError::Integrity(
                "session journal checksum mismatch".into(),
            ));
        }
        let batch: SessionEventBatch = serde_json::from_value(self.payload).map_err(|error| {
            StoreError::Integrity(format!("invalid session journal batch: {error}"))
        })?;
        if self.version != batch.command.journal_version()
            || batch.batch_id != batch.expected_batch_id()
            || self.batch_id != batch.batch_id
        {
            return Err(StoreError::Integrity(
                "session journal batch identity mismatch".into(),
            ));
        }
        Ok(batch)
    }
}

#[derive(Debug)]
struct SessionJournal {
    path: PathBuf,
}

impl SessionJournal {
    fn new(root: &Path, session_id: &str) -> Self {
        Self {
            path: SessionStoreLayout::from_sessions_root(root).session_journal_path(session_id),
        }
    }

    fn append_many(&self, batches: &[SessionEventBatch]) -> Result<()> {
        if batches.is_empty() {
            return Ok(());
        }
        let Some(parent) = self.path.parent() else {
            return Err(StoreError::Integrity(format!(
                "session journal path {} has no parent",
                self.path.display()
            )));
        };
        ensure_private_directory_all(parent)?;
        reject_symlink(&self.path)?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        for batch in batches {
            #[derive(serde::Serialize)]
            struct Payload<'a> {
                document_revision: u64,
                batch_id: &'a str,
                barrier: SessionBatchBarrier,
                command: &'a SessionEventCommand,
            }
            let batch_id = batch.expected_batch_id();
            let payload = serde_json::to_value(Payload {
                document_revision: batch.document_revision,
                batch_id: &batch_id,
                barrier: batch.barrier,
                command: &batch.command,
            })?;
            let payload_bytes = serde_json::to_vec(&payload)?;
            let record = JournalRecord {
                version: batch.command.journal_version(),
                batch_id,
                payload_len: payload_bytes.len() as u64,
                checksum: crate::object::sha256_hex(&payload_bytes),
                payload,
            };
            serde_json::to_writer(&mut file, &record)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        sync_directory(parent)?;
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => {
                if let Some(parent) = self.path.parent() {
                    sync_directory(parent)?;
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn load_complete(&self) -> Result<(Vec<SessionEventBatch>, bool)> {
        reject_symlink(&self.path)?;
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), false));
            }
            Err(error) => return Err(error.into()),
        };
        let missing_final_newline = !bytes.is_empty() && !bytes.ends_with(b"\n");
        let complete_len = if missing_final_newline {
            bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1)
        } else {
            bytes.len()
        };
        let lines = bytes[..complete_len].split(|byte| *byte == b'\n');
        // A complete native record opts the whole stream into fail-closed decoding.
        // A damaged legacy prefix must not discard later pending native commands.
        let native_group = lines.clone().any(|line| {
            line.starts_with(b"{\"version\":2")
                || serde_json::from_slice::<serde_json::Value>(line)
                    .ok()
                    .is_some_and(|value| {
                        value["version"] == 2
                            || value["payload"]["command"]["kind"]
                                .as_str()
                                .is_some_and(|kind| {
                                    matches!(
                                        kind,
                                        "compact_save"
                                            | "compact_submit_turn"
                                            | "compact_turn_transition"
                                    )
                                })
                    })
        });
        let mut batches = Vec::new();
        let mut ignored_tail = missing_final_newline;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let record = match serde_json::from_slice::<JournalRecord>(line) {
                Ok(record) => record,
                Err(error) => {
                    // Only identifiable v1 framing permits the legacy damaged-tail policy.
                    // An unrecognized complete record is not an interrupted append.
                    let legacy = !native_group
                        && (line.starts_with(b"{\"version\":1,")
                            || serde_json::from_slice::<serde_json::Value>(line)
                                .ok()
                                .is_some_and(|value| value["version"] == 1));
                    if !legacy {
                        return Err(StoreError::Integrity(format!(
                            "invalid session journal record: {error}"
                        )));
                    }
                    ignored_tail = true;
                    break;
                }
            };
            if !matches!(record.version, 1 | 2) {
                return Err(StoreError::Integrity(format!(
                    "unsupported session journal version {}",
                    record.version
                )));
            }
            match record.decode() {
                Ok(batch) => batches.push(batch),
                Err(error) if native_group => return Err(error),
                Err(_) => {
                    ignored_tail = true;
                    break;
                }
            }
        }
        Ok((batches, ignored_tail))
    }
}

#[derive(Debug)]
pub struct SessionWriter {
    inner: OwnedLineageWriter,
    journal: SessionJournal,
    startup_journal_recovery: SessionJournalRecovery,
}

impl SessionWriter {
    pub fn open(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Self::open_inner(root.as_ref(), session_id.into(), true)
    }

    pub fn open_existing(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Self::open_inner(root.as_ref(), session_id.into(), false)
    }

    pub fn open_existing_in_lineage(
        root: impl AsRef<Path>,
        lineage_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Self> {
        let session_id = session_id.into();
        let inner = OwnedLineageWriter::acquire_existing_in_lineage(
            root.as_ref(),
            lineage_id.into(),
            session_id.clone(),
        )?;
        Self::finish_open(inner, SessionJournal::new(root.as_ref(), &session_id))
    }

    fn open_inner(root: &Path, session_id: String, create: bool) -> Result<Self> {
        let inner = OwnedLineageWriter::acquire(root, session_id.clone(), create)?;
        Self::finish_open(inner, SessionJournal::new(root, &session_id))
    }

    fn finish_open(inner: OwnedLineageWriter, journal: SessionJournal) -> Result<Self> {
        let mut writer = Self {
            inner,
            journal,
            startup_journal_recovery: SessionJournalRecovery::default(),
        };
        // Durable intent settles before interruption can invalidate pending transitions.
        writer.startup_journal_recovery =
            writer
                .recover_journal()
                .map_err(|failure| StoreError::JournalRecovery {
                    failure: Box::new(failure),
                })?;
        writer.inner = writer.inner.finish_startup()?;
        Ok(writer)
    }

    /// Journal settlement performed before this writer became available.
    pub fn startup_journal_recovery(&self) -> &SessionJournalRecovery {
        &self.startup_journal_recovery
    }

    pub fn lineage_writer_mut(&mut self) -> &mut OwnedLineageWriter {
        &mut self.inner
    }

    pub fn release(self) -> Result<()> {
        self.inner.release()
    }

    pub fn invalidate_connection(&mut self) {
        self.inner.invalidate_connection();
    }

    pub fn reopen_connection(&mut self) -> Result<()> {
        self.inner.reopen_connection()
    }

    pub fn store_head(&self) -> Result<StoreHead> {
        self.inner.store_head()
    }

    pub fn last_session_commit(&self) -> Result<Option<(String, SaveReceipt)>> {
        self.inner.last_session_commit()
    }

    pub fn take_startup_recovery(&mut self) -> Option<crate::StartupRecoveryResult> {
        self.inner.take_startup_recovery()
    }

    pub fn startup_recovery(&self) -> Option<&crate::StartupRecoveryResult> {
        self.inner.startup_recovery()
    }

    pub fn latest_terminal_turn_id(&self) -> Result<Option<crate::TurnId>> {
        self.inner.latest_terminal_turn_id()
    }

    pub fn spawn_search_projector(&self) -> Result<crate::LineageSearchProjector> {
        self.inner.spawn_search_projector()
    }

    pub fn append_request_attempt(
        &mut self,
        entry: &protocol::request_log::RequestLogEntry,
        payload_mode: crate::RequestAuditPayloadMode,
    ) -> Result<i64> {
        self.inner.append_request_attempt(entry, payload_mode)
    }

    pub fn commit_session(
        &mut self,
        command: &SessionCommit,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        self.inner.commit_session(command)
    }

    pub fn submit_turn(
        &mut self,
        command: &SubmitTurn,
    ) -> std::result::Result<SubmitTurnReceipt, SessionCommitFailure> {
        self.inner.submit_turn(command)
    }

    pub fn transition_turn(
        &mut self,
        command: &TurnTransition,
    ) -> std::result::Result<TurnTransitionReceipt, SessionCommitFailure> {
        self.inner.transition_turn(command)
    }

    pub fn recover_submit_turn(
        &self,
        command: &SubmitTurn,
    ) -> std::result::Result<Option<SubmitTurnReceipt>, SessionCommitFailure> {
        self.inner.recover_submit_turn(command)
    }

    pub fn recover_turn_transition(
        &self,
        command: &TurnTransition,
    ) -> std::result::Result<Option<TurnTransitionReceipt>, SessionCommitFailure> {
        self.inner.recover_turn_transition(command)
    }

    pub fn recover_journal(
        &mut self,
    ) -> std::result::Result<SessionJournalRecovery, SessionCommitFailure> {
        let (batches, ignored_tail) = self
            .journal
            .load_complete()
            .map_err(crate::session_command::commit_failure_from_store_error)?;
        for batch in &batches {
            self.apply_batch(batch)?;
        }
        if !batches.is_empty() || ignored_tail {
            self.journal
                .clear()
                .map_err(crate::session_command::commit_failure_from_store_error)?;
        }
        Ok(SessionJournalRecovery {
            complete_batches: batches.len(),
            ignored_incomplete_tail: ignored_tail,
        })
    }

    /// Reads the exact matching receipt without committing, journaling or changing catalog markers.
    /// Native receipts retain their immutable result; legacy receipts remain ordinary receipts.
    pub fn recover_batch(
        &self,
        batch: &SessionEventBatch,
    ) -> std::result::Result<Option<SessionEventReceipt>, SessionCommitFailure> {
        match &batch.command {
            SessionEventCommand::Save { session } => self
                .inner
                .recover_session_commit(session)
                .map(|receipt| receipt.map(SessionEventReceipt::Save)),
            SessionEventCommand::SubmitTurn { command } => self
                .inner
                .recover_submit_turn(command)
                .map(|receipt| receipt.map(SessionEventReceipt::SubmitTurn)),
            SessionEventCommand::TurnTransition { command } => self
                .inner
                .recover_turn_transition(command)
                .map(|receipt| receipt.map(SessionEventReceipt::TurnTransition)),
            SessionEventCommand::CompactSave { session } => self
                .inner
                .recover_compact_session(session)
                .map(|result| result.map(SessionEventReceipt::CompactSave)),
            SessionEventCommand::CompactSubmitTurn { command } => self
                .inner
                .recover_compact_submit_turn(command)
                .map(|result| result.map(SessionEventReceipt::CompactSubmitTurn)),
            SessionEventCommand::CompactTurnTransition { command } => self
                .inner
                .recover_compact_turn_transition(command)
                .map(|result| result.map(SessionEventReceipt::CompactTurnTransition)),
        }
    }

    pub fn commit_batch(
        &mut self,
        batch: &SessionEventBatch,
    ) -> std::result::Result<SessionEventReceipt, SessionCommitFailure> {
        // A single event is already atomic in the canonical lineage transaction. The journal is
        // only needed to recover a group that spans multiple canonical transactions.
        self.apply_batch(batch)
    }

    pub fn commit_batches(
        &mut self,
        batches: &[SessionEventBatch],
    ) -> std::result::Result<Vec<SessionEventReceipt>, SessionCommitFailure> {
        if batches.is_empty() {
            return Ok(Vec::new());
        }
        // Pending commands must become durable before a new group can replace their
        // journal. A failed retry leaves the same bytes instead of appending copies.
        self.recover_journal()?;
        self.journal
            .append_many(batches)
            .map_err(crate::session_command::commit_failure_from_store_error)?;
        let mut receipts = Vec::with_capacity(batches.len());
        for batch in batches {
            receipts.push(self.apply_batch(batch)?);
        }
        self.journal
            .clear()
            .map_err(crate::session_command::commit_failure_from_store_error)?;
        Ok(receipts)
    }

    fn apply_batch(
        &mut self,
        batch: &SessionEventBatch,
    ) -> std::result::Result<SessionEventReceipt, SessionCommitFailure> {
        match &batch.command {
            SessionEventCommand::Save { session } => Ok(SessionEventReceipt::Save(
                self.inner.commit_session(session)?,
            )),
            SessionEventCommand::SubmitTurn { command } => Ok(SessionEventReceipt::SubmitTurn(
                self.inner.submit_turn(command)?,
            )),
            SessionEventCommand::TurnTransition { command } => Ok(
                SessionEventReceipt::TurnTransition(self.inner.transition_turn(command)?),
            ),
            SessionEventCommand::CompactSave { session } => Ok(SessionEventReceipt::CompactSave(
                self.inner.commit_compact_session(session)?,
            )),
            SessionEventCommand::CompactSubmitTurn { command } => Ok(
                SessionEventReceipt::CompactSubmitTurn(self.inner.submit_compact_turn(command)?),
            ),
            SessionEventCommand::CompactTurnTransition { command } => {
                Ok(SessionEventReceipt::CompactTurnTransition(
                    self.inner.transition_compact_turn(command)?,
                ))
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEventReceipt {
    Save(SaveReceipt),
    SubmitTurn(SubmitTurnReceipt),
    TurnTransition(TurnTransitionReceipt),
    CompactSave(crate::SessionCommitResult),
    CompactSubmitTurn(crate::CompactSubmitTurnResult),
    CompactTurnTransition(crate::CompactTurnTransitionResult),
}

impl SessionEventReceipt {
    pub fn session(&self) -> &SaveReceipt {
        match self {
            Self::Save(receipt) => receipt,
            Self::SubmitTurn(receipt) => &receipt.session,
            Self::TurnTransition(receipt) => &receipt.session,
            Self::CompactSave(result) => &result.receipt,
            Self::CompactSubmitTurn(result) => &result.session.receipt,
            Self::CompactTurnTransition(result) => &result.session.receipt,
        }
    }

    /// Native receipts retain the exact immutable result, not just its former branch head.
    pub fn exact_session(&self) -> Option<&crate::SessionCommitResult> {
        match self {
            Self::CompactSave(result) => Some(result),
            Self::CompactSubmitTurn(result) => Some(&result.session),
            Self::CompactTurnTransition(result) => Some(&result.session),
            Self::Save(_) | Self::SubmitTurn(_) | Self::TurnTransition(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HistoryLen, HistorySuffix, Revision, SessionIdentity, SessionMetadata};

    fn identity(id: &str) -> SessionIdentity {
        SessionIdentity {
            id: id.into(),
            created_at: 1,
            parent_id: None,
        }
    }

    fn metadata() -> SessionMetadata {
        SessionMetadata {
            title: None,
            slug: None,
            first_user_message: None,
            cwd: None,
            mode: None,
            reasoning_effort: None,
            model: None,
            fast_mode: None,
            accounting_json: None,
            checkpoint_json: None,
            checkpoint_events_json: None,
            context_tokens: None,
            context_tokens_history_len: None,
            display_context_tokens: None,
            session_cost_usd: crate::SessionCostUsd::new(0.0).unwrap(),
            updated_at: 1,
        }
    }

    fn batch(session_id: &str, text: &str) -> SessionEventBatch {
        SessionEventBatch::save(
            1,
            SessionCommit {
                session_id: session_id.into(),
                expected: StoreHead::default(),
                identity: identity(session_id),
                metadata: metadata(),
                history: HistorySuffix {
                    start: crate::HistoryIndex::ZERO,
                    final_len: HistoryLen::new(1),
                    items: vec![protocol::HistoryItem::user(protocol::Content::text(text))],
                },
                side_tables: crate::SideTableSuffixes::default(),
                transcript_records: None,
            },
            SessionBatchBarrier::Lifecycle,
        )
    }

    fn compact_session(session_id: &str) -> crate::CompactSessionCommit {
        crate::CompactSessionCommit {
            session_id: session_id.into(),
            expected: StoreHead::default(),
            identity: identity(session_id),
            scalars: crate::SessionScalars {
                title: None,
                slug: None,
                cwd: None,
                mode: None,
                reasoning_effort: None,
                model: None,
                fast_mode: None,
                accounting: crate::ValueEdit::Retain,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: crate::SessionCostUsd::new(0.0).unwrap(),
                updated_at: 1,
            },
            archive_base: None,
            archives: crate::CompactSessionArchives::default(),
            history: HistorySuffix {
                start: crate::HistoryIndex::ZERO,
                final_len: HistoryLen::new(1),
                items: vec![protocol::HistoryItem::user(protocol::Content::text(
                    "native",
                ))],
            },
            transcript_records: None,
        }
    }

    fn compact_next(
        writer: &mut SessionWriter,
        previous: &crate::CompactSessionCommit,
        result: &crate::SessionCommitResult,
    ) -> crate::CompactSessionCommit {
        let mut next = previous.clone();
        next.expected = result.receipt.current;
        next.archive_base = Some(crate::SessionArchiveBase {
            lineage_id: writer.lineage_writer_mut().lineage_id().into(),
            revision_id: result.revision_id.clone(),
            branch_sequence: result.receipt.current.revision,
        });
        next.history.start = crate::HistoryIndex::new(next.history.final_len.get());
        next.history.items.clear();
        next.archives = crate::CompactSessionArchives::default();
        next
    }

    #[test]
    fn native_batch_recovery_returns_exact_save_and_turn_results_after_rewind_and_gc() {
        let root = tempfile::tempdir().unwrap();
        let id = "3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let mut writer = SessionWriter::open(root.path(), id).unwrap();
        let initial = compact_session(id);
        let saved_batch =
            SessionEventBatch::compact_save(1, initial.clone(), SessionBatchBarrier::None);
        assert!(writer.recover_batch(&saved_batch).unwrap().is_none());
        let saved = writer.commit_batch(&saved_batch).unwrap();
        let noop_command = compact_next(&mut writer, &initial, saved.exact_session().unwrap());
        let noop_batch =
            SessionEventBatch::compact_save(2, noop_command.clone(), SessionBatchBarrier::None);
        let noop = writer.commit_batch(&noop_batch).unwrap();
        assert_eq!(noop.session().previous, noop.session().current);
        let mut submission = noop_command;
        submission.scalars.title = Some("submitted".into());
        submission.scalars.updated_at = 2;
        let submit_batch = SessionEventBatch::compact_submit_turn(
            3,
            crate::CompactSubmitTurn {
                session: submission.clone(),
                turn: crate::NewTurn {
                    kind: crate::TurnKind::Command,
                    submitted_history_idx: crate::HistoryIndex::ZERO,
                    continuation_of: None,
                    created_at_ms: 2,
                },
            },
        );
        assert!(writer.recover_batch(&submit_batch).unwrap().is_none());
        let submitted = writer.commit_batch(&submit_batch).unwrap();
        let SessionEventReceipt::CompactSubmitTurn(result) = &submitted else {
            panic!("native turn result")
        };
        let running_command = crate::CompactTurnTransition {
            session: compact_next(&mut writer, &submission, &result.session),
            turn_id: result.turn_id,
            state: crate::TurnState::Running,
            at_ms: 3,
            terminal_reason: None,
        };
        let running_batch = SessionEventBatch::compact_turn_transition(4, running_command.clone());
        assert!(writer.recover_batch(&running_batch).unwrap().is_none());
        let running = writer.commit_batch(&running_batch).unwrap();
        let mut completed_command = running_command.clone();
        completed_command.session = compact_next(
            &mut writer,
            &running_command.session,
            running.exact_session().unwrap(),
        );
        completed_command.session.scalars.title = Some("completed".into());
        completed_command.session.scalars.updated_at = 4;
        completed_command.state = crate::TurnState::Completed;
        completed_command.at_ms = 4;
        let completed_batch = SessionEventBatch::compact_turn_transition(5, completed_command);
        let completed = writer.commit_batch(&completed_batch).unwrap();
        let cases = [
            (saved_batch, saved),
            (noop_batch, noop),
            (submit_batch, submitted),
            (running_batch, running),
            (completed_batch, completed),
        ];
        writer
            .lineage_writer_mut()
            .rewind_to_sequence(1, 5)
            .unwrap();
        let mut reclaimed = false;
        for _ in 0..512 {
            if writer
                .lineage_writer_mut()
                .reclaim_step(16)
                .unwrap()
                .complete
            {
                reclaimed = true;
                break;
            }
        }
        assert!(reclaimed);
        writer.release().unwrap();
        let writer = SessionWriter::open_existing(root.path(), id).unwrap();
        let head = writer.store_head().unwrap();
        let marker = crate::catalog_session_pending_token(root.path(), id).unwrap();
        let journal = SessionJournal::new(root.path(), id);
        assert!(!journal.path.exists());
        for (batch, expected) in &cases {
            let recovered = writer.recover_batch(batch).unwrap().unwrap();
            assert_eq!(&recovered, expected);
            assert!(recovered.exact_session().is_some());
            assert!(recovered.session().current.revision < head.revision);
        }
        let mut changed = initial.clone();
        changed.scalars.title = Some("unrecorded".into());
        assert!(writer
            .recover_batch(&SessionEventBatch::compact_save(
                6,
                changed,
                SessionBatchBarrier::None
            ))
            .unwrap()
            .is_none());
        let same_command =
            SessionEventBatch::compact_save(99, initial, SessionBatchBarrier::Lifecycle);
        assert_eq!(
            writer.recover_batch(&same_command).unwrap().as_ref(),
            Some(&cases[0].1)
        );
        assert_eq!(writer.store_head().unwrap(), head);
        assert_eq!(
            crate::catalog_session_pending_token(root.path(), id).unwrap(),
            marker
        );
        assert!(!journal.path.exists());
        let foreign = SessionWriter::open(
            root.path(),
            "4123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        assert!(foreign.recover_batch(&cases[0].0).unwrap().is_none());
    }

    #[test]
    fn legacy_batch_recovery_does_not_upgrade_receipts_to_native_results() {
        let root = tempfile::tempdir().unwrap();
        let id = "4123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let mut writer = SessionWriter::open(root.path(), id).unwrap();
        let first = batch(id, "legacy");
        assert!(writer.recover_batch(&first).unwrap().is_none());
        let saved = writer.commit_batch(&first).unwrap();
        let mut session = first.legacy_session().unwrap().clone();
        session.expected = saved.session().current;
        session.history.start = crate::HistoryIndex::new(1);
        session.history.items.clear();
        session.metadata.title = Some("submitted".into());
        session.metadata.updated_at = 2;
        let submit_batch = SessionEventBatch::submit_turn(
            2,
            SubmitTurn {
                session: session.clone(),
                turn: crate::NewTurn {
                    kind: crate::TurnKind::Command,
                    submitted_history_idx: crate::HistoryIndex::ZERO,
                    continuation_of: None,
                    created_at_ms: 2,
                },
            },
        );
        assert!(writer.recover_batch(&submit_batch).unwrap().is_none());
        let submitted = writer.commit_batch(&submit_batch).unwrap();
        let SessionEventReceipt::SubmitTurn(receipt) = &submitted else {
            panic!("legacy turn receipt")
        };
        session.expected = receipt.session.current;
        let running_command = TurnTransition {
            session: session.clone(),
            turn_id: receipt.turn_id,
            state: crate::TurnState::Running,
            at_ms: 3,
            terminal_reason: None,
        };
        let running_batch = SessionEventBatch::turn_transition(3, running_command.clone());
        assert!(writer.recover_batch(&running_batch).unwrap().is_none());
        let running = writer.commit_batch(&running_batch).unwrap();
        let mut completed_command = running_command;
        completed_command.session.expected = running.session().current;
        completed_command.session.metadata.updated_at = 4;
        completed_command.state = crate::TurnState::Completed;
        completed_command.at_ms = 4;
        let completed_batch = SessionEventBatch::turn_transition(4, completed_command);
        let completed = writer.commit_batch(&completed_batch).unwrap();
        let head = writer.store_head().unwrap();
        for (batch, expected) in [
            (first, saved),
            (submit_batch, submitted),
            (running_batch, running),
            (completed_batch, completed),
        ] {
            let recovered = writer.recover_batch(&batch).unwrap().unwrap();
            assert_eq!(recovered, expected);
            assert!(recovered.exact_session().is_none());
        }
        assert_eq!(writer.store_head().unwrap(), head);
    }

    #[test]
    fn native_journal_writer_saves_noops_and_replays_exact_results_after_head_advances() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "3123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let first = compact_session(session_id);
        let batch = SessionEventBatch::compact_save(1, first.clone(), SessionBatchBarrier::None);
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let saved = writer.commit_batch(&batch).unwrap();
        let result = saved.exact_session().unwrap();
        let mut changed = compact_next(&mut writer, &first, result);
        let noop = SessionEventBatch::compact_save(2, changed.clone(), SessionBatchBarrier::None);
        assert_eq!(
            writer
                .commit_batch(&noop)
                .unwrap()
                .exact_session()
                .unwrap()
                .revision_id,
            result.revision_id
        );
        changed.scalars.title = Some("renamed".into());
        changed.scalars.updated_at = 2;
        let changed = SessionEventBatch::compact_save(3, changed, SessionBatchBarrier::Lifecycle);
        let newer = writer.commit_batch(&changed).unwrap();
        assert_eq!(newer.session().current.revision.get(), 2);
        assert_eq!(writer.commit_batch(&batch).unwrap(), saved);
        let journal = SessionJournal::new(root.path(), session_id);
        journal
            .append_many(&[batch.clone(), noop.clone(), changed.clone()])
            .unwrap();
        writer.release().unwrap();

        let mut writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
        assert_eq!(writer.startup_journal_recovery().complete_batches, 3);
        assert_eq!(writer.store_head().unwrap(), newer.session().current);
        assert_eq!(writer.commit_batch(&batch).unwrap(), saved);
        assert_eq!(writer.commit_batch(&changed).unwrap(), newer);
        assert!(!journal.path.exists());
        writer.release().unwrap();
    }

    #[test]
    fn native_checkpoint_reference_journal_settles_save_and_turn_lifecycle() {
        use crate::{CheckpointEdit, CheckpointEventsEdit, CheckpointRecord, CheckpointSummary};
        let root = tempfile::tempdir().unwrap();
        let session_id = "5123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let record = |summary, created| CheckpointRecord {
            fields: serde_json::json!({"first_live_index":0, "completed_at_history_len":1, "created_at_ms":created}),
            summary,
        };
        let summary = "shared α\0".repeat(8192);
        let mut initial = compact_session(session_id);
        initial.archives.checkpoint = CheckpointEdit::ReplaceRecord {
            record: record(
                CheckpointSummary::New {
                    text: summary.clone().into(),
                },
                1,
            ),
        };
        initial.archives.checkpoint_events = CheckpointEventsEdit::ReplaceRecordsSuffix {
            retain_records: 0,
            records: vec![record(CheckpointSummary::Checkpoint, 1)],
        };
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let saved = writer
            .commit_batch(&SessionEventBatch::compact_save(
                1,
                initial.clone(),
                SessionBatchBarrier::None,
            ))
            .unwrap();
        let mut session = compact_next(&mut writer, &initial, saved.exact_session().unwrap());
        session.archives.checkpoint = CheckpointEdit::ReplaceRecord {
            record: record(CheckpointSummary::BaseCheckpoint, 2),
        };
        session.archives.checkpoint_events = CheckpointEventsEdit::ReplaceRecordsSuffix {
            retain_records: 0,
            records: vec![record(CheckpointSummary::BaseEvent { record: 0 }, 2)],
        };
        let submitted_batch = SessionEventBatch::compact_submit_turn(
            2,
            crate::CompactSubmitTurn {
                session: session.clone(),
                turn: crate::NewTurn {
                    kind: crate::TurnKind::Command,
                    submitted_history_idx: crate::HistoryIndex::ZERO,
                    continuation_of: None,
                    created_at_ms: 2,
                },
            },
        );
        let submitted = writer.commit_batch(&submitted_batch).unwrap();
        let SessionEventReceipt::CompactSubmitTurn(result) = &submitted else {
            panic!("native submission")
        };
        let turn_id = result.turn_id;
        let mut running = crate::CompactTurnTransition {
            session: compact_next(&mut writer, &session, &result.session),
            turn_id,
            state: crate::TurnState::Running,
            at_ms: 3,
            terminal_reason: None,
        };
        running.session.archives.checkpoint = CheckpointEdit::ReplaceRecord {
            record: record(CheckpointSummary::BaseEvent { record: 0 }, 3),
        };
        running.session.archives.checkpoint_events = CheckpointEventsEdit::ReplaceRecordsSuffix {
            retain_records: 1,
            records: vec![record(CheckpointSummary::Checkpoint, 3)],
        };
        let running_batch = SessionEventBatch::compact_turn_transition(3, running.clone());
        let started = writer.commit_batch(&running_batch).unwrap();
        let mut completed = running.clone();
        completed.session = compact_next(
            &mut writer,
            &running.session,
            started.exact_session().unwrap(),
        );
        completed.session.archives.checkpoint = CheckpointEdit::ReplaceRecord {
            record: record(CheckpointSummary::BaseCheckpoint, 4),
        };
        completed.state = crate::TurnState::Completed;
        completed.at_ms = 4;
        let completed_batch = SessionEventBatch::compact_turn_transition(4, completed);
        let journal = SessionJournal::new(root.path(), session_id);
        journal
            .append_many(&[
                submitted_batch.clone(),
                running_batch.clone(),
                completed_batch.clone(),
            ])
            .unwrap();
        assert!(fs::metadata(&journal.path).unwrap().len() < 8192);
        writer.release().unwrap();
        let mut writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
        assert_eq!(writer.startup_journal_recovery().complete_batches, 3);
        assert_eq!(writer.commit_batch(&submitted_batch).unwrap(), submitted);
        assert_eq!(writer.commit_batch(&running_batch).unwrap(), started);
        let finished = writer.commit_batch(&completed_batch).unwrap();
        let snapshot = writer.lineage_writer_mut().snapshot().unwrap();
        assert_eq!(
            snapshot.revision_id,
            finished.exact_session().unwrap().revision_id
        );
        assert!(
            snapshot.metadata.checkpoint_json.as_ref().unwrap()["summary"].as_str()
                == Some(summary.as_str())
        );
        assert_eq!(
            snapshot.metadata.checkpoint_json.as_ref().unwrap()["created_at_ms"],
            4
        );
        let reader = crate::LineageSessionReader::open_existing(root.path(), session_id).unwrap();
        assert_eq!(
            reader.turns().unwrap()[0].state,
            crate::TurnState::Completed
        );
        assert!(!journal.path.exists());
        writer.release().unwrap();
    }

    #[test]
    fn native_journal_turn_group_replays_after_terminal_completion() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "4123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let first = compact_session(session_id);
        let submitted = SessionEventBatch::compact_submit_turn(
            1,
            crate::CompactSubmitTurn {
                session: first.clone(),
                turn: crate::NewTurn {
                    kind: crate::TurnKind::Command,
                    submitted_history_idx: crate::HistoryIndex::ZERO,
                    continuation_of: None,
                    created_at_ms: 1,
                },
            },
        );
        assert_eq!(submitted.barrier, SessionBatchBarrier::Turn);
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let receipt = writer.commit_batch(&submitted).unwrap();
        let SessionEventReceipt::CompactSubmitTurn(result) = &receipt else {
            panic!("native submission result")
        };
        let running = SessionEventBatch::compact_turn_transition(
            2,
            crate::CompactTurnTransition {
                session: compact_next(&mut writer, &first, &result.session),
                turn_id: result.turn_id,
                state: crate::TurnState::Running,
                at_ms: 2,
                terminal_reason: None,
            },
        );
        assert_eq!(running.barrier, SessionBatchBarrier::Turn);
        let started = writer.commit_batch(&running).unwrap();
        let completed = SessionEventBatch::compact_turn_transition(
            3,
            crate::CompactTurnTransition {
                session: compact_next(&mut writer, &first, started.exact_session().unwrap()),
                turn_id: result.turn_id,
                state: crate::TurnState::Completed,
                at_ms: 3,
                terminal_reason: Some("finished α".into()),
            },
        );
        assert_eq!(completed.barrier, SessionBatchBarrier::Lifecycle);
        let finished = writer.commit_batch(&completed).unwrap();
        let batches = [submitted.clone(), running.clone(), completed.clone()];
        assert_eq!(
            writer.commit_batches(&batches).unwrap(),
            [receipt.clone(), started.clone(), finished.clone()]
        );
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(&batches).unwrap();
        writer.release().unwrap();
        let mut writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
        assert_eq!(writer.startup_journal_recovery().complete_batches, 3);
        assert_eq!(writer.commit_batch(&submitted).unwrap(), receipt);
        assert_eq!(writer.commit_batch(&running).unwrap(), started);
        assert_eq!(writer.commit_batch(&completed).unwrap(), finished);
        let reader = crate::LineageSessionReader::open_existing(root.path(), session_id).unwrap();
        let turns = reader.turns().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].state, crate::TurnState::Completed);
        assert!(!journal.path.exists());
        writer.release().unwrap();
    }

    fn native_turn_group(writer: &mut SessionWriter) -> Vec<SessionEventBatch> {
        let first = compact_session(writer.lineage_writer_mut().session_id());
        let submitted = SessionEventBatch::compact_submit_turn(
            1,
            crate::CompactSubmitTurn {
                session: first.clone(),
                turn: crate::NewTurn {
                    kind: crate::TurnKind::Command,
                    submitted_history_idx: crate::HistoryIndex::ZERO,
                    continuation_of: None,
                    created_at_ms: 1,
                },
            },
        );
        let receipt = writer.commit_batch(&submitted).unwrap();
        let SessionEventReceipt::CompactSubmitTurn(result) = &receipt else {
            panic!("native submission result")
        };
        let next = compact_next(writer, &first, &result.session);
        let running = SessionEventBatch::compact_turn_transition(
            2,
            crate::CompactTurnTransition {
                session: next.clone(),
                turn_id: result.turn_id,
                state: crate::TurnState::Running,
                at_ms: 2,
                terminal_reason: None,
            },
        );
        let mut final_session = next;
        final_session.scalars.title = Some("completed after restart".into());
        final_session.history.final_len = HistoryLen::new(2);
        final_session.history.items = vec![protocol::HistoryItem::user(protocol::Content::text(
            "completed",
        ))];
        let completed = SessionEventBatch::compact_turn_transition(
            3,
            crate::CompactTurnTransition {
                session: final_session,
                turn_id: result.turn_id,
                state: crate::TurnState::Completed,
                at_ms: 3,
                terminal_reason: Some("finished".into()),
            },
        );
        vec![submitted, running, completed]
    }

    #[test]
    fn native_turn_journal_settles_before_startup_interruption() {
        for mode in 0..3 {
            for prefix in 1..=2 {
                let root = tempfile::tempdir().unwrap();
                let session_id = "e123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
                let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
                let batches = native_turn_group(&mut writer);
                let lineage = writer.lineage_writer_mut().lineage_id().to_owned();
                let journal = SessionJournal::new(root.path(), session_id);
                journal.append_many(&batches).unwrap();
                for batch in batches.iter().take(prefix) {
                    writer.commit_batch(batch).unwrap();
                }
                writer.release().unwrap();

                let mut writer = match mode {
                    0 => SessionWriter::open(root.path(), session_id),
                    1 => SessionWriter::open_existing(root.path(), session_id),
                    _ => SessionWriter::open_existing_in_lineage(root.path(), lineage, session_id),
                }
                .unwrap();
                assert_eq!(writer.startup_journal_recovery().complete_batches, 3);
                assert_eq!(
                    writer.recover_journal().unwrap(),
                    SessionJournalRecovery::default()
                );
                assert!(writer.startup_recovery().is_none());
                let finished = writer.commit_batch(&batches[2]).unwrap();
                assert_eq!(writer.store_head().unwrap(), finished.session().current);
                assert_eq!(writer.store_head().unwrap().history_len, HistoryLen::new(2));
                let reader =
                    crate::LineageSessionReader::open_existing(root.path(), session_id).unwrap();
                assert_eq!(
                    reader.turns().unwrap()[0].state,
                    crate::TurnState::Completed
                );
                assert_eq!(
                    reader.snapshot().unwrap().metadata.title.as_deref(),
                    Some("completed after restart")
                );
                assert!(!journal.path.exists());
                writer.release().unwrap();
            }
        }
    }

    #[test]
    fn native_turn_journal_nonterminal_result_is_interrupted_once_after_settlement() {
        for count in 1..=2 {
            let root = tempfile::tempdir().unwrap();
            let session_id = "e123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
            let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
            let batches = native_turn_group(&mut writer);
            let head = writer.store_head().unwrap();
            let journal = SessionJournal::new(root.path(), session_id);
            journal.append_many(&batches[..count]).unwrap();
            writer.release().unwrap();

            let mut writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
            assert_eq!(writer.startup_journal_recovery().complete_batches, count);
            let recovery = writer.take_startup_recovery().unwrap();
            assert_eq!(recovery.session.receipt.previous, head);
            assert_eq!(
                recovery.session.receipt.current.revision.get(),
                head.revision.get() + 1
            );
            assert_eq!(recovery.interrupted_turns.len(), 1);
            let reader =
                crate::LineageSessionReader::open_existing(root.path(), session_id).unwrap();
            let turns = reader.turns().unwrap();
            assert_eq!(turns.len(), 1);
            assert_eq!(turns[0].state, crate::TurnState::Interrupted);
            assert_eq!(
                turns[0].started_at_ms,
                if count == 2 { Some(2) } else { None }
            );
            assert_eq!(turns[0].terminal_reason.as_deref(), Some("process_restart"));
            assert!(!journal.path.exists());
            writer.release().unwrap();
            let writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
            assert_eq!(
                writer.startup_journal_recovery(),
                &SessionJournalRecovery::default()
            );
            assert!(writer.startup_recovery().is_none());
            assert_eq!(
                writer.store_head().unwrap(),
                recovery.session.receipt.current
            );
            writer.release().unwrap();
        }
    }

    #[test]
    fn native_turn_journal_failed_startup_preserves_intent_without_interruption() {
        for damage in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let session_id = "e123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
            let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
            let batches = native_turn_group(&mut writer);
            let head = writer.store_head().unwrap();
            let journal = SessionJournal::new(root.path(), session_id);
            journal.append_many(&batches).unwrap();
            let valid = fs::read(&journal.path).unwrap();
            if damage == 0 {
                let mut file = OpenOptions::new().append(true).open(&journal.path).unwrap();
                file.write_all(b"{\"version\":2,malformed}\n").unwrap();
                file.sync_all().unwrap();
            } else if damage == 1 {
                rewrite_first_journal_record(&journal, |record| record.checksum = "invalid".into());
            } else {
                let SessionEventCommand::CompactTurnTransition { mut command } =
                    batches[2].command.clone()
                else {
                    panic!("native transition")
                };
                command.state = crate::TurnState::Ready;
                command.terminal_reason = None;
                journal.clear().unwrap();
                journal
                    .append_many(&[
                        batches[0].clone(),
                        batches[1].clone(),
                        SessionEventBatch::compact_turn_transition(3, command),
                    ])
                    .unwrap();
            }
            let damaged = fs::read(&journal.path).unwrap();
            writer.release().unwrap();
            let error = SessionWriter::open_existing(root.path(), session_id).unwrap_err();
            let StoreError::JournalRecovery { failure } = error else {
                panic!("structured journal failure")
            };
            if damage == 2 {
                assert!(matches!(*failure, SessionCommitFailure::InvalidTurn { .. }));
            } else {
                assert!(matches!(*failure, SessionCommitFailure::Integrity { .. }));
            }
            assert!(fs::read(&journal.path).unwrap() == damaged);
            let reader =
                crate::LineageSessionReader::open_existing(root.path(), session_id).unwrap();
            assert_eq!(reader.store_head().unwrap(), head);
            assert_eq!(
                reader.turns().unwrap()[0].state,
                if damage == 2 {
                    crate::TurnState::Running
                } else {
                    crate::TurnState::Ready
                }
            );
            fs::write(&journal.path, valid).unwrap();
            let writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
            assert!(writer.startup_recovery().is_none());
            assert_eq!(
                reader.turns().unwrap()[0].state,
                crate::TurnState::Completed
            );
            assert!(!journal.path.exists());
            writer.release().unwrap();
        }
    }

    #[test]
    fn native_turn_journal_connection_reopen_preserves_live_recovery() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "e123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let batches = native_turn_group(&mut writer);
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(&batches).unwrap();
        writer.commit_batch(&batches[1]).unwrap();
        writer.invalidate_connection();
        writer.reopen_connection().unwrap();
        let reader = crate::LineageSessionReader::open_existing(root.path(), session_id).unwrap();
        assert_eq!(reader.turns().unwrap()[0].state, crate::TurnState::Running);
        assert!(writer.startup_recovery().is_none());
        assert_eq!(writer.recover_journal().unwrap().complete_batches, 3);
        assert_eq!(
            writer.startup_journal_recovery(),
            &SessionJournalRecovery::default()
        );
        assert_eq!(
            reader.turns().unwrap()[0].state,
            crate::TurnState::Completed
        );
        assert_eq!(
            writer.store_head().unwrap(),
            writer.commit_batch(&batches[2]).unwrap().session().current
        );
        assert!(!journal.path.exists());
        writer.release().unwrap();
    }

    #[test]
    fn native_turn_journal_startup_recovers_after_process_abort() {
        const ROOT: &str = "SMELT_TURN_JOURNAL_STARTUP_ABORT_ROOT";
        const BOUNDARY: &str = "SMELT_TURN_JOURNAL_STARTUP_ABORT_BOUNDARY";
        let session_id = "e123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        if let Ok(root) = std::env::var(ROOT) {
            let boundary: usize = std::env::var(BOUNDARY).unwrap().parse().unwrap();
            let mut writer = SessionWriter::open(&root, session_id).unwrap();
            let batches = native_turn_group(&mut writer);
            let journal = SessionJournal::new(Path::new(&root), session_id);
            journal.append_many(&batches).unwrap();
            for batch in batches.iter().skip(1).take(boundary) {
                writer.commit_batch(batch).unwrap();
            }
            if boundary == 3 {
                journal.clear().unwrap();
            }
            std::process::abort();
        }
        for boundary in 0..4 {
            let root = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "session_writer::tests::native_turn_journal_startup_recovers_after_process_abort", "--nocapture"])
                .env(ROOT, root.path()).env(BOUNDARY, boundary.to_string())
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(status.signal(), Some(6));
            }
            assert!(!status.success());
            let writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
            assert_eq!(
                writer.startup_journal_recovery().complete_batches,
                if boundary == 3 { 0 } else { 3 }
            );
            assert!(writer.startup_recovery().is_none());
            let reader =
                crate::LineageSessionReader::open_existing(root.path(), session_id).unwrap();
            let snapshot = reader.snapshot().unwrap();
            assert_eq!(
                snapshot.metadata.title.as_deref(),
                Some("completed after restart")
            );
            assert_eq!(
                reader.turns().unwrap()[0].state,
                crate::TurnState::Completed
            );
            assert_eq!(
                reader.history_range(0, 2).unwrap(),
                [
                    protocol::HistoryItem::user(protocol::Content::text("native")),
                    protocol::HistoryItem::user(protocol::Content::text("completed"))
                ]
            );
            let head = writer.store_head().unwrap();
            assert_eq!(head.revision.get(), 2);
            assert!(!SessionJournal::new(root.path(), session_id).path.exists());
            writer.release().unwrap();
            let writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
            assert_eq!(writer.store_head().unwrap(), head);
            assert_eq!(
                writer.startup_journal_recovery(),
                &SessionJournalRecovery::default()
            );
            assert!(writer.startup_recovery().is_none());
            writer.release().unwrap();
        }
    }

    #[test]
    fn journal_preserves_legacy_payloads_and_decodes_mixed_versions() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "5123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let legacy = batch(session_id, "one");
        let native = SessionEventBatch::compact_save(
            2,
            compact_session(session_id),
            SessionBatchBarrier::None,
        );
        let journal = SessionJournal::new(root.path(), session_id);
        journal
            .append_many(&[legacy.clone(), native.clone()])
            .unwrap();
        let bytes = fs::read(&journal.path).unwrap();
        let records: Vec<JournalRecord> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            legacy.batch_id,
            "95e77d8b86d8f440ae89e0231596e9a11c7ccd3adcbcfd5aa42ff639b10f2c9e"
        );
        assert_eq!(
            native.batch_id,
            "21ee489fc334ac7db34c54bc1be3676bdfac566ca53a8cec9c934df4ba33d8ca"
        );
        assert_eq!(records[0].version, 1);
        assert_eq!(records[1].version, 2);
        assert_eq!(records[0].payload, serde_json::to_value(&legacy).unwrap());
        assert_eq!(records[1].payload, serde_json::to_value(&native).unwrap());
        let legacy_seed = SessionEventBatchIdSeed {
            schema: "smelt-session-event-batch-v2",
            document_revision: legacy.document_revision,
            barrier: legacy.barrier,
            command: &legacy.command,
        };
        assert_eq!(
            legacy.batch_id,
            crate::object::sha256_hex(&serde_json::to_vec(&legacy_seed).unwrap())
        );
        let native_seed = SessionEventBatchIdSeed {
            schema: "smelt-session-event-batch-v2",
            document_revision: native.document_revision,
            barrier: native.barrier,
            command: &native.command,
        };
        assert_ne!(
            native.batch_id,
            crate::object::sha256_hex(&serde_json::to_vec(&native_seed).unwrap())
        );
        assert_eq!(
            journal.load_complete().unwrap(),
            (vec![legacy, native], false)
        );
    }

    #[test]
    fn writer_commits_and_recovers_a_mixed_legacy_native_group() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "a123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let first = batch(session_id, "legacy");
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let saved = writer.commit_batch(&first).unwrap();
        let snapshot = writer.lineage_writer_mut().snapshot().unwrap();
        let mut second = compact_next(
            &mut writer,
            &compact_session(session_id),
            &crate::SessionCommitResult {
                receipt: saved.session().clone(),
                revision_id: snapshot.revision_id,
            },
        );
        second.history.final_len = HistoryLen::new(2);
        second.history.items = compact_session(session_id).history.items;
        let second = SessionEventBatch::compact_save(2, second, SessionBatchBarrier::Lifecycle);
        let batches = [first, second];
        let receipts = writer.commit_batches(&batches).unwrap();
        assert!(receipts[0].exact_session().is_none());
        assert!(receipts[1].exact_session().is_some());
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(&batches).unwrap();
        writer.release().unwrap();
        let mut writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
        assert_eq!(writer.startup_journal_recovery().complete_batches, 2);
        assert_eq!(writer.commit_batches(&batches).unwrap(), receipts);
        assert_eq!(writer.store_head().unwrap().revision.get(), 2);
        let reader = crate::LineageSessionReader::open_existing(root.path(), session_id).unwrap();
        assert_eq!(
            reader.history_range(0, 2).unwrap(),
            [
                protocol::HistoryItem::user(protocol::Content::text("legacy")),
                protocol::HistoryItem::user(protocol::Content::text("native"))
            ]
        );
        writer.release().unwrap();
    }

    #[test]
    fn native_journal_group_recovers_after_process_abort() {
        const ROOT: &str = "SMELT_NATIVE_JOURNAL_ABORT_ROOT";
        const BOUNDARY: &str = "SMELT_NATIVE_JOURNAL_ABORT_BOUNDARY";
        let session_id = "b123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        if let Ok(root) = std::env::var(ROOT) {
            let boundary: usize = std::env::var(BOUNDARY).unwrap().parse().unwrap();
            let mut writer = SessionWriter::open(&root, session_id).unwrap();
            let initial = compact_session(session_id);
            let initial_batch =
                SessionEventBatch::compact_save(1, initial.clone(), SessionBatchBarrier::None);
            let initial_result = writer.commit_batch(&initial_batch).unwrap();
            let mut first = compact_next(
                &mut writer,
                &initial,
                initial_result.exact_session().unwrap(),
            );
            first.history.final_len = HistoryLen::new(2);
            first.history.items = initial.history.items.clone();
            first.scalars.updated_at = 2;
            let first_batch =
                SessionEventBatch::compact_save(2, first.clone(), SessionBatchBarrier::None);
            let mut second = first;
            second.expected.revision = Revision::new(2);
            second.expected.history_len = HistoryLen::new(2);
            second.history.start = crate::HistoryIndex::new(2);
            second.history.final_len = HistoryLen::new(3);
            second.scalars.updated_at = 3;
            let second_batch =
                SessionEventBatch::compact_save(3, second, SessionBatchBarrier::Lifecycle);
            let batches = [first_batch, second_batch];
            let journal = SessionJournal::new(Path::new(&root), session_id);
            journal.append_many(&batches).unwrap();
            for batch in batches.iter().take(boundary) {
                writer.commit_batch(batch).unwrap();
            }
            if boundary == 3 {
                journal.clear().unwrap();
            }
            std::process::abort();
        }
        for boundary in 0..4 {
            let root = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session_writer::tests::native_journal_group_recovers_after_process_abort",
                    "--nocapture",
                ])
                .env(ROOT, root.path())
                .env(BOUNDARY, boundary.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(!status.success());
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(
                    status.signal(),
                    Some(6),
                    "child must reach its abort boundary"
                );
            }
            let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
            let recovery = writer.startup_journal_recovery();
            assert_eq!(recovery.complete_batches, if boundary == 3 { 0 } else { 2 });
            assert!(!recovery.ignored_incomplete_tail);
            assert_eq!(writer.store_head().unwrap().revision.get(), 3);
            assert_eq!(writer.store_head().unwrap().history_len.get(), 3);
            assert_eq!(writer.recover_journal().unwrap().complete_batches, 0);
            assert!(!SessionJournal::new(root.path(), session_id).path.exists());
            let reader =
                crate::LineageSessionReader::open_existing(root.path(), session_id).unwrap();
            let snapshot = reader.snapshot().unwrap();
            assert_eq!(snapshot.metadata.updated_at, 3);
            assert_eq!(
                reader.history_range(0, 3).unwrap(),
                vec![protocol::HistoryItem::user(protocol::Content::text("native")); 3]
            );
            writer.release().unwrap();
        }
    }

    #[test]
    fn native_journal_corruption_fails_closed_and_preserves_pending_file() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "6123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let native = SessionEventBatch::compact_save(
            1,
            compact_session(session_id),
            SessionBatchBarrier::None,
        );
        let journal = SessionJournal::new(root.path(), session_id);
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        for damage in 0..6 {
            journal.clear().unwrap();
            journal.append_many(std::slice::from_ref(&native)).unwrap();
            rewrite_first_journal_record(&journal, |record| match damage {
                0 => record.payload_len += 1,
                1 => record.checksum = "invalid".into(),
                2 => record.batch_id = "invalid".into(),
                4 => record.version = 1,
                5 => record.version = 3,
                _ => {
                    record.payload["command"]["kind"] = "unknown".into();
                    let bytes = serde_json::to_vec(&record.payload).unwrap();
                    record.payload_len = bytes.len() as u64;
                    record.checksum = crate::object::sha256_hex(&bytes);
                }
            });
            let before = fs::read(&journal.path).unwrap();
            assert!(matches!(
                writer.recover_journal(),
                Err(SessionCommitFailure::Integrity { .. })
            ));
            assert_eq!(fs::read(&journal.path).unwrap(), before);
            assert_eq!(writer.store_head().unwrap(), StoreHead::default());
        }
        writer.release().unwrap();
    }

    #[test]
    fn mixed_journal_corrupt_legacy_prefix_cannot_discard_pending_native_commands() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "e123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let legacy = batch(session_id, "legacy");
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let saved = writer.commit_batch(&legacy).unwrap();
        let snapshot = writer.lineage_writer_mut().snapshot().unwrap();
        let mut native = compact_next(
            &mut writer,
            &compact_session(session_id),
            &crate::SessionCommitResult {
                receipt: saved.session().clone(),
                revision_id: snapshot.revision_id,
            },
        );
        native.scalars.title = Some("native pending".into());
        let native = SessionEventBatch::compact_save(2, native, SessionBatchBarrier::Lifecycle);
        let journal = SessionJournal::new(root.path(), session_id);
        let batches = [legacy, native];
        for reordered in [false, true] {
            journal.clear().unwrap();
            journal.append_many(&batches).unwrap();
            if reordered {
                let bytes = fs::read(&journal.path).unwrap();
                let end = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
                let record: serde_json::Value = serde_json::from_slice(&bytes[end..]).unwrap();
                let mut next = bytes[..end].to_vec();
                next.extend(serde_json::to_vec(&record).unwrap());
                next.push(b'\n');
                fs::write(&journal.path, next).unwrap();
            }
            rewrite_first_journal_record(&journal, |record| record.payload_len += 1);
            let before = fs::read(&journal.path).unwrap();
            assert!(matches!(
                writer.recover_journal(),
                Err(SessionCommitFailure::Integrity { .. })
            ));
            assert!(fs::read(&journal.path).unwrap() == before);
            assert_eq!(writer.store_head().unwrap(), saved.session().current);
        }
        journal.clear().unwrap();
        journal.append_many(&batches).unwrap();
        assert_eq!(writer.recover_journal().unwrap().complete_batches, 2);
        assert_eq!(writer.store_head().unwrap().revision.get(), 2);
        writer.release().unwrap();
    }

    #[test]
    fn mixed_journal_downgraded_native_record_preserves_pending_intent() {
        for malformed_prefix in [false, true] {
            for reordered in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let id = "f123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
                let mut writer = SessionWriter::open(root.path(), id).unwrap();
                let legacy = batch(id, "legacy");
                let saved = writer.commit_batch(&legacy).unwrap();
                let snapshot = writer.lineage_writer_mut().snapshot().unwrap();
                let mut native = compact_next(
                    &mut writer,
                    &compact_session(id),
                    &crate::SessionCommitResult {
                        receipt: saved.session().clone(),
                        revision_id: snapshot.revision_id,
                    },
                );
                native.scalars.title = Some("native pending".into());
                let batches = [
                    legacy,
                    SessionEventBatch::compact_save(2, native, SessionBatchBarrier::Lifecycle),
                ];
                let journal = SessionJournal::new(root.path(), id);
                journal.append_many(&batches).unwrap();
                let original = fs::read(&journal.path).unwrap();
                let end = original.iter().position(|byte| *byte == b'\n').unwrap() + 1;
                let mut record: JournalRecord = serde_json::from_slice(&original[end..]).unwrap();
                record.version = 1;
                let mut damaged = if malformed_prefix {
                    b"{\"version\":1,\n".to_vec()
                } else {
                    let mut prefix: JournalRecord =
                        serde_json::from_slice(&original[..end]).unwrap();
                    prefix.payload_len += 1;
                    let mut bytes = serde_json::to_vec(&prefix).unwrap();
                    bytes.push(b'\n');
                    bytes
                };
                let native = if reordered {
                    serde_json::to_vec(&serde_json::to_value(&record).unwrap()).unwrap()
                } else {
                    serde_json::to_vec(&record).unwrap()
                };
                damaged.extend(native);
                damaged.push(b'\n');
                fs::write(&journal.path, &damaged).unwrap();
                assert!(matches!(
                    writer.recover_journal(),
                    Err(SessionCommitFailure::Integrity { .. })
                ));
                assert_eq!(fs::read(&journal.path).unwrap(), damaged);
                assert_eq!(writer.store_head().unwrap(), saved.session().current);
                writer.release().unwrap();
                assert!(matches!(
                    SessionWriter::open_existing(root.path(), id),
                    Err(StoreError::JournalRecovery { .. })
                ));
                assert_eq!(fs::read(&journal.path).unwrap(), damaged);
                fs::write(&journal.path, original).unwrap();
                let mut writer = SessionWriter::open_existing(root.path(), id).unwrap();
                assert_eq!(writer.startup_journal_recovery().complete_batches, 2);
                assert_eq!(writer.store_head().unwrap().revision.get(), 2);
                assert_eq!(
                    writer
                        .lineage_writer_mut()
                        .snapshot()
                        .unwrap()
                        .metadata
                        .title,
                    Some("native pending".into())
                );
                assert_eq!(writer.commit_batches(&batches).unwrap().len(), 2);
                assert_eq!(writer.store_head().unwrap().revision.get(), 2);
                writer.release().unwrap();
            }
        }
    }

    #[test]
    fn native_journal_malformed_complete_records_preserve_the_pending_file() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "9123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let journal = SessionJournal::new(root.path(), session_id);
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        for bytes in [
            b"{\"version\":2}\n".as_slice(),
            b"{\"version\":2,\n".as_slice(),
            b"{\"payload\":{},\"version\":2}\n".as_slice(),
            b"{\"versioo\":2}\n".as_slice(),
            b"{\"version\":3,\n".as_slice(),
            b"{\n".as_slice(),
        ] {
            ensure_private_directory_all(journal.path.parent().unwrap()).unwrap();
            fs::write(&journal.path, bytes).unwrap();
            assert!(matches!(
                writer.recover_journal(),
                Err(SessionCommitFailure::Integrity { .. })
            ));
            assert_eq!(fs::read(&journal.path).unwrap(), bytes);
            assert_eq!(writer.store_head().unwrap(), StoreHead::default());
        }
        fs::write(&journal.path, b"{\"version\":1,\n").unwrap();
        assert_eq!(
            writer.recover_journal().unwrap(),
            SessionJournalRecovery {
                complete_batches: 0,
                ignored_incomplete_tail: true,
            }
        );
        assert!(!journal.path.exists());
        writer.release().unwrap();
    }

    #[test]
    fn native_journal_recovery_keeps_group_on_commit_failure() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "7123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let first = SessionEventBatch::compact_save(
            1,
            compact_session(session_id),
            SessionBatchBarrier::None,
        );
        let mut invalid = compact_session(session_id);
        invalid.scalars.title = Some("not a replay".into());
        let second = SessionEventBatch::compact_save(2, invalid, SessionBatchBarrier::None);
        let journal = SessionJournal::new(root.path(), session_id);
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let batches = [first.clone(), second];
        assert!(writer.commit_batches(&batches).is_err());
        let saved = writer.commit_batch(&first).unwrap();
        let before = fs::read(&journal.path).unwrap();
        assert!(writer.recover_journal().is_err());
        assert_eq!(fs::read(&journal.path).unwrap(), before);
        assert_eq!(writer.store_head().unwrap(), saved.session().current);
        assert_eq!(writer.commit_batch(&first).unwrap(), saved);
        assert!(writer.commit_batches(&batches).is_err());
        assert_eq!(
            fs::read(&journal.path).unwrap(),
            before,
            "retry must not append duplicate pending commands"
        );
        assert!(writer.commit_batches(&[]).unwrap().is_empty());
        assert_eq!(
            fs::read(&journal.path).unwrap(),
            before,
            "an empty group must not erase pending commands"
        );
        writer.release().unwrap();
    }

    #[test]
    fn writer_recovers_pending_group_before_accepting_a_new_group() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "d123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let initial = compact_session(session_id);
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let saved = writer
            .commit_batch(&SessionEventBatch::compact_save(
                1,
                initial.clone(),
                SessionBatchBarrier::None,
            ))
            .unwrap();
        let mut pending = compact_next(&mut writer, &initial, saved.exact_session().unwrap());
        pending.scalars.title = Some("pending".into());
        pending.scalars.updated_at = 2;
        let pending_batch =
            SessionEventBatch::compact_save(2, pending.clone(), SessionBatchBarrier::Lifecycle);
        let journal = SessionJournal::new(root.path(), session_id);
        journal
            .append_many(std::slice::from_ref(&pending_batch))
            .unwrap();
        let mut requested = pending;
        requested.expected.revision = Revision::new(2);
        requested.scalars.title = Some("requested".into());
        requested.scalars.updated_at = 3;
        let requested =
            SessionEventBatch::compact_save(3, requested, SessionBatchBarrier::Lifecycle);
        let receipts = writer
            .commit_batches(std::slice::from_ref(&requested))
            .unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].session().current.revision.get(), 3);
        assert_eq!(
            writer
                .commit_batch(&pending_batch)
                .unwrap()
                .session()
                .current
                .revision
                .get(),
            2
        );
        assert_eq!(writer.store_head().unwrap(), receipts[0].session().current);
        assert_eq!(
            writer
                .lineage_writer_mut()
                .snapshot()
                .unwrap()
                .metadata
                .title
                .as_deref(),
            Some("requested")
        );
        assert!(!journal.path.exists());
        writer.release().unwrap();
    }

    #[test]
    fn empty_journal_group_does_not_erase_pending_commands() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "c123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let pending = SessionEventBatch::compact_save(
            1,
            compact_session(session_id),
            SessionBatchBarrier::None,
        );
        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(std::slice::from_ref(&pending)).unwrap();
        let before = fs::read(&journal.path).unwrap();
        assert!(writer.commit_batches(&[]).unwrap().is_empty());
        assert!(
            journal.path.exists(),
            "empty commit must preserve the pending journal"
        );
        assert!(fs::read(&journal.path).unwrap() == before);
        assert_eq!(writer.store_head().unwrap(), StoreHead::default());
        assert_eq!(writer.recover_journal().unwrap().complete_batches, 1);
        assert_eq!(writer.store_head().unwrap().revision.get(), 1);
        writer.release().unwrap();
    }

    #[test]
    fn native_journal_incomplete_tail_does_not_lose_complete_prefix() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "8123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let native = SessionEventBatch::compact_save(
            1,
            compact_session(session_id),
            SessionBatchBarrier::None,
        );
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(std::slice::from_ref(&native)).unwrap();
        let mut file = OpenOptions::new().append(true).open(&journal.path).unwrap();
        file.write_all(b"{\"version\":2,\"payload\":").unwrap();
        file.sync_all().unwrap();
        let writer = SessionWriter::open(root.path(), session_id).unwrap();
        assert_eq!(
            writer.startup_journal_recovery(),
            &SessionJournalRecovery {
                complete_batches: 1,
                ignored_incomplete_tail: true
            }
        );
        assert_eq!(writer.store_head().unwrap().revision.get(), 1);
        assert!(!journal.path.exists());
        writer.release().unwrap();
    }

    #[test]
    fn batch_id_changes_when_save_metadata_changes_without_history_len_change() {
        let session_id = "0023456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let first = batch(session_id, "one");
        let mut second_session = first.legacy_session().unwrap().clone();
        second_session.metadata.title = Some("renamed".into());
        let second =
            SessionEventBatch::save(first.document_revision, second_session, first.barrier);

        assert_ne!(first.batch_id, second.batch_id);
    }

    #[test]
    fn journal_replays_complete_records_and_ignores_incomplete_tail() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(&[batch(session_id, "one")]).unwrap();
        {
            let mut file = OpenOptions::new().append(true).open(&journal.path).unwrap();
            file.write_all(b"{\"checksum\":\"truncated").unwrap();
        }

        let (loaded, ignored_tail) = journal.load_complete().unwrap();
        assert!(ignored_tail);
        assert_eq!(loaded, vec![batch(session_id, "one")]);
    }

    #[test]
    fn journal_ignores_record_without_final_newline() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "0223456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(&[batch(session_id, "one")]).unwrap();
        let bytes = fs::read(&journal.path).unwrap();
        fs::write(&journal.path, &bytes[..bytes.len() - 1]).unwrap();

        let (loaded, ignored_tail) = journal.load_complete().unwrap();
        assert!(ignored_tail);
        assert!(loaded.is_empty());
    }

    fn rewrite_first_journal_record(
        journal: &SessionJournal,
        mutate: impl FnOnce(&mut JournalRecord),
    ) {
        let bytes = fs::read(&journal.path).unwrap();
        let end = bytes.iter().position(|byte| *byte == b'\n').unwrap();
        let mut record = serde_json::from_slice::<JournalRecord>(&bytes[..end]).unwrap();
        mutate(&mut record);
        let mut next = serde_json::to_vec(&record).unwrap();
        next.push(b'\n');
        next.extend_from_slice(&bytes[end + 1..]);
        fs::write(&journal.path, next).unwrap();
    }

    #[test]
    fn journal_ignores_record_with_corrupt_payload_len() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "0323456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(&[batch(session_id, "one")]).unwrap();
        rewrite_first_journal_record(&journal, |record| {
            record.payload_len = record.payload_len.saturating_add(1);
        });

        let (loaded, ignored_tail) = journal.load_complete().unwrap();
        assert!(ignored_tail);
        assert!(loaded.is_empty());
    }

    #[test]
    fn journal_ignores_record_with_mismatched_batch_id() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "0423456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(&[batch(session_id, "one")]).unwrap();
        rewrite_first_journal_record(&journal, |record| {
            record.batch_id = "not-the-payload-batch".into();
        });

        let (loaded, ignored_tail) = journal.load_complete().unwrap();
        assert!(ignored_tail);
        assert!(loaded.is_empty());
    }

    #[test]
    fn writer_commits_multiple_batches_with_one_journal_group() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "1023456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let first = batch(session_id, "one");
        let mut second = batch(session_id, "two");
        second.document_revision = 2;
        second.barrier = SessionBatchBarrier::Lifecycle;
        if let SessionEventCommand::Save { session } = &mut second.command {
            session.expected = StoreHead {
                revision: Revision::new(1),
                history_len: HistoryLen::new(1),
                ..StoreHead::default()
            };
            session.history = HistorySuffix {
                start: crate::HistoryIndex::new(1),
                final_len: HistoryLen::new(2),
                items: vec![protocol::HistoryItem::user(protocol::Content::text("two"))],
            };
            session.side_tables.start = crate::HistoryIndex::new(1);
        }

        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let receipts = writer
            .commit_batches(&[first.clone(), second.clone()])
            .unwrap();

        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].session().current.revision, Revision::new(1));
        assert_eq!(receipts[1].session().current.revision, Revision::new(2));
        assert_eq!(writer.store_head().unwrap().history_len, HistoryLen::new(2));
        assert!(!SessionJournal::new(root.path(), session_id).path.exists());
        writer.release().unwrap();
    }

    #[test]
    fn writer_replays_journal_idempotently() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let batch = batch(session_id, "durable");
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(std::slice::from_ref(&batch)).unwrap();

        let writer = SessionWriter::open(root.path(), session_id).unwrap();
        let recovery = writer.startup_journal_recovery();
        assert_eq!(recovery.complete_batches, 1);
        assert!(!recovery.ignored_incomplete_tail);
        assert_eq!(writer.store_head().unwrap().revision, Revision::new(1));
        writer.release().unwrap();
    }

    #[test]
    fn committed_batch_with_uncleared_journal_replays_without_duplicate_revision() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let batch = batch(session_id, "already committed");
        let journal = SessionJournal::new(root.path(), session_id);
        journal.append_many(std::slice::from_ref(&batch)).unwrap();
        let mut direct = OwnedLineageWriter::open(root.path(), session_id).unwrap();
        direct
            .commit_session(batch.legacy_session().unwrap())
            .unwrap();
        direct.release().unwrap();

        let writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
        let recovery = writer.startup_journal_recovery();
        assert_eq!(recovery.complete_batches, 1);
        assert_eq!(writer.store_head().unwrap().revision, Revision::new(1));
        assert!(!journal.path.exists());
        writer.release().unwrap();
    }
}
