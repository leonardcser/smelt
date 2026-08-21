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
    Save { session: SessionCommit },
    SubmitTurn { command: SubmitTurn },
    TurnTransition { command: TurnTransition },
}

impl SessionEventCommand {
    pub fn session(&self) -> &SessionCommit {
        match self {
            Self::Save { session } => session,
            Self::SubmitTurn { command } => &command.session,
            Self::TurnTransition { command } => &command.session,
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
        schema: "smelt-session-event-batch-v2",
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
    pub fn save(
        document_revision: u64,
        session: SessionCommit,
        barrier: SessionBatchBarrier,
    ) -> Self {
        let command = SessionEventCommand::Save { session };
        Self {
            document_revision,
            batch_id: session_event_batch_id(document_revision, barrier, &command),
            barrier,
            command,
        }
    }

    pub fn submit_turn(document_revision: u64, command: SubmitTurn) -> Self {
        let barrier = SessionBatchBarrier::Turn;
        let command = SessionEventCommand::SubmitTurn { command };
        Self {
            document_revision,
            batch_id: session_event_batch_id(document_revision, barrier, &command),
            barrier,
            command,
        }
    }

    pub fn turn_transition(document_revision: u64, command: TurnTransition) -> Self {
        let barrier = if command.state.is_terminal() {
            SessionBatchBarrier::Lifecycle
        } else {
            SessionBatchBarrier::Turn
        };
        let command = SessionEventCommand::TurnTransition { command };
        Self {
            document_revision,
            batch_id: session_event_batch_id(document_revision, barrier, &command),
            barrier,
            command,
        }
    }

    pub fn session(&self) -> &SessionCommit {
        self.command.session()
    }

    fn expected_batch_id(&self) -> String {
        session_event_batch_id(self.document_revision, self.barrier, &self.command)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionJournalRecovery {
    pub complete_batches: usize,
    pub ignored_incomplete_tail: bool,
}

const JOURNAL_RECORD_VERSION: u16 = 1;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct JournalRecord {
    version: u16,
    batch_id: String,
    payload_len: u64,
    checksum: String,
    payload: serde_json::Value,
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
            let mut payload_batch = batch.clone();
            payload_batch.batch_id = payload_batch.expected_batch_id();
            let payload = serde_json::to_value(&payload_batch)?;
            let payload_bytes = serde_json::to_vec(&payload)?;
            let record = JournalRecord {
                version: JOURNAL_RECORD_VERSION,
                batch_id: payload_batch.batch_id.clone(),
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
        let mut batches = Vec::new();
        let mut ignored_tail = missing_final_newline;
        for line in bytes[..complete_len].split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<JournalRecord>(line) else {
                ignored_tail = true;
                break;
            };
            if record.version != JOURNAL_RECORD_VERSION {
                ignored_tail = true;
                break;
            }
            let payload_bytes = serde_json::to_vec(&record.payload)?;
            if record.payload_len != payload_bytes.len() as u64 {
                ignored_tail = true;
                break;
            }
            if crate::object::sha256_hex(&payload_bytes) != record.checksum {
                ignored_tail = true;
                break;
            }
            let Ok(batch) = serde_json::from_value::<SessionEventBatch>(record.payload) else {
                ignored_tail = true;
                break;
            };
            if batch.batch_id != batch.expected_batch_id() || record.batch_id != batch.batch_id {
                ignored_tail = true;
                break;
            }
            batches.push(batch);
        }
        Ok((batches, ignored_tail))
    }
}

#[derive(Debug)]
pub struct SessionWriter {
    inner: OwnedLineageWriter,
    journal: SessionJournal,
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
        let inner =
            OwnedLineageWriter::open_existing_in_lineage(root.as_ref(), lineage_id, &session_id)?;
        Ok(Self {
            journal: SessionJournal::new(root.as_ref(), &session_id),
            inner,
        })
    }

    fn open_inner(root: &Path, session_id: String, create: bool) -> Result<Self> {
        let inner = if create {
            OwnedLineageWriter::open(root, session_id.clone())?
        } else {
            OwnedLineageWriter::open_existing(root, session_id.clone())?
        };
        Ok(Self {
            inner,
            journal: SessionJournal::new(root, &session_id),
        })
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

    pub fn take_startup_recovery(&mut self) -> Option<crate::StartupRecoveryReceipt> {
        self.inner.take_startup_recovery()
    }

    pub fn startup_recovery(&self) -> Option<&crate::StartupRecoveryReceipt> {
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
            let receipt = self.apply_batch(batch)?;
            self.publish_catalog(batch, &receipt)?;
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

    pub fn commit_batch(
        &mut self,
        batch: &SessionEventBatch,
    ) -> std::result::Result<SessionEventReceipt, SessionCommitFailure> {
        let mut receipts = self.commit_batches(std::slice::from_ref(batch))?;
        receipts.pop().ok_or_else(|| {
            crate::session_command::commit_failure_from_store_error(StoreError::Integrity(
                "session event batch produced no receipt".into(),
            ))
        })
    }

    pub fn commit_batches(
        &mut self,
        batches: &[SessionEventBatch],
    ) -> std::result::Result<Vec<SessionEventReceipt>, SessionCommitFailure> {
        self.journal
            .append_many(batches)
            .map_err(crate::session_command::commit_failure_from_store_error)?;
        let mut receipts = Vec::with_capacity(batches.len());
        for batch in batches {
            let receipt = self.apply_batch(batch)?;
            self.publish_catalog(batch, &receipt)?;
            receipts.push(receipt);
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
        }
    }

    pub fn refresh_catalog(&self) -> Result<()> {
        self.inner.refresh_catalog()
    }

    fn publish_catalog(
        &self,
        batch: &SessionEventBatch,
        receipt: &SessionEventReceipt,
    ) -> std::result::Result<(), SessionCommitFailure> {
        match (&batch.command, receipt) {
            (SessionEventCommand::Save { .. }, SessionEventReceipt::Save(_)) => Ok(()),
            (
                SessionEventCommand::SubmitTurn { command },
                SessionEventReceipt::SubmitTurn(receipt),
            ) => self
                .inner
                .publish_catalog_for_commit(&command.session, &receipt.session)
                .map_err(crate::session_command::commit_failure_from_store_error),
            (
                SessionEventCommand::TurnTransition { command },
                SessionEventReceipt::TurnTransition(receipt),
            ) => self
                .inner
                .publish_catalog_for_commit(&command.session, &receipt.session)
                .map_err(crate::session_command::commit_failure_from_store_error),
            _ => Err(crate::session_command::commit_failure_from_store_error(
                StoreError::Integrity("session event receipt did not match its batch".into()),
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEventReceipt {
    Save(SaveReceipt),
    SubmitTurn(SubmitTurnReceipt),
    TurnTransition(TurnTransitionReceipt),
}

impl SessionEventReceipt {
    pub fn session(&self) -> &SaveReceipt {
        match self {
            Self::Save(receipt) => receipt,
            Self::SubmitTurn(receipt) => &receipt.session,
            Self::TurnTransition(receipt) => &receipt.session,
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

    #[test]
    fn batch_id_changes_when_save_metadata_changes_without_history_len_change() {
        let session_id = "0023456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let first = batch(session_id, "one");
        let mut second_session = first.session().clone();
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
        let line = std::str::from_utf8(&bytes).unwrap().lines().next().unwrap();
        let mut record = serde_json::from_str::<JournalRecord>(line).unwrap();
        mutate(&mut record);
        let mut next = serde_json::to_vec(&record).unwrap();
        next.push(b'\n');
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

        let mut writer = SessionWriter::open(root.path(), session_id).unwrap();
        let recovery = writer.recover_journal().unwrap();
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
        direct.commit_session(batch.session()).unwrap();
        direct.release().unwrap();

        let mut writer = SessionWriter::open_existing(root.path(), session_id).unwrap();
        let recovery = writer.recover_journal().unwrap();
        assert_eq!(recovery.complete_batches, 1);
        assert_eq!(writer.store_head().unwrap().revision, Revision::new(1));
        assert!(!journal.path.exists());
        writer.release().unwrap();
    }
}
