//! The session writer: a thread that owns the session's store writer and commits
//! captured session changes in the order the document captured them.
//!
//! Each [`SessionSave`] holds only what changed since the previous capture, so the
//! cost of a save follows the new content, not the session size. When a commit
//! fails, the saves captured after it no longer apply; the writer skips them until
//! the document sends a resync save, which rewrites everything after the last
//! durable state.

#[cfg(test)]
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_PERSISTENCE_DEADLINE: Duration = Duration::from_secs(5);
pub(crate) const INTERACTIVE_PERSISTENCE_DEADLINE: Duration = Duration::from_millis(500);

/// Request audits held while the session has no stored row yet.
const MAX_PENDING_AUDITS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistenceFailureClass {
    /// The document produced a save the store rejects.
    Invariant,
    /// The filesystem or database failed.
    Environment,
    /// Another smelt process has the session open.
    Ownership,
    /// The writer is not running.
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistenceCause {
    pub(crate) class: PersistenceFailureClass,
    pub(crate) message: String,
}

impl PersistenceCause {
    fn new(class: PersistenceFailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(PersistenceFailureClass::Unavailable, message)
    }

    pub(crate) fn invariant(message: impl Into<String>) -> Self {
        Self::new(PersistenceFailureClass::Invariant, message)
    }

    fn from_store(operation: &str, error: smelt_store::StoreError) -> Self {
        let class = match &error {
            smelt_store::StoreError::Owned { .. } => PersistenceFailureClass::Ownership,
            smelt_store::StoreError::InvalidSuffix { .. }
            | smelt_store::StoreError::Integrity(_)
            | smelt_store::StoreError::AlreadyExists { .. }
            | smelt_store::StoreError::NotFound { .. }
            | smelt_store::StoreError::Json(_) => PersistenceFailureClass::Invariant,
            smelt_store::StoreError::Io(_)
            | smelt_store::StoreError::Sqlite(_)
            | smelt_store::StoreError::Cancelled
            | smelt_store::StoreError::UnsupportedSchema { .. }
            | smelt_store::StoreError::UpgradeBlocked { .. } => {
                PersistenceFailureClass::Environment
            }
        };
        Self::new(class, format!("{operation}: {error}"))
    }
}

impl std::fmt::Display for PersistenceCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClosePolicy {
    RequireDurable,
    AllowUnsaved,
}

/// One captured change to the session.
#[derive(Clone, Debug)]
pub(crate) struct SessionSave {
    /// Capture order; each save builds on all earlier ones.
    pub(crate) seq: u64,
    /// Rewrites everything after the last durable save, so it applies after a failure.
    pub(crate) resync: bool,
    pub(crate) identity: smelt_store::SessionIdentity,
    pub(crate) commit: smelt_store::SessionCommit,
}

/// The newest save the store holds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DurableSave {
    pub(crate) seq: u64,
    pub(crate) head: smelt_store::StoreHead,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistenceStatus {
    /// Set once the writer has the session open, or failed to.
    pub(crate) opened: Option<Result<(), PersistenceCause>>,
    pub(crate) durable: Option<DurableSave>,
    /// The latest save that failed since the last durable one.
    pub(crate) failure: Option<(u64, PersistenceCause)>,
    /// The newest save the writer has committed, failed or skipped.
    pub(crate) handled: u64,
    /// How many request audits the writer has taken.
    pub(crate) audits_handled: u64,
    pub(crate) audit_warning: Option<String>,
    pub(crate) finished: bool,
}

impl PersistenceStatus {
    pub(crate) fn durable_seq(&self) -> u64 {
        self.durable.map_or(0, |durable| durable.seq)
    }

    /// Whether every save through `seq` is durable, or why it is not.
    fn outcome(&self, seq: u64) -> Option<PersistenceFlushOutcome> {
        if self.durable_seq() >= seq {
            return Some(PersistenceFlushOutcome::Durable);
        }
        if let Some(Err(cause)) = &self.opened {
            return Some(PersistenceFlushOutcome::Failed(cause.clone()));
        }
        if let Some((_, cause)) = self.failure.as_ref().filter(|_| self.handled >= seq) {
            return Some(PersistenceFlushOutcome::Failed(cause.clone()));
        }
        self.finished.then(|| {
            PersistenceFlushOutcome::Failed(PersistenceCause::unavailable(
                "session writer stopped before saving",
            ))
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PersistenceFlushOutcome {
    Durable,
    Failed(PersistenceCause),
    Deadline,
}

impl PersistenceFlushOutcome {
    pub(crate) fn into_result(self) -> Result<(), String> {
        match self {
            Self::Durable => Ok(()),
            Self::Failed(cause) => Err(cause.message),
            Self::Deadline => Err("session save did not finish in time".into()),
        }
    }
}

enum Command {
    Save(Box<SessionSave>),
    Audit(Box<QueuedAudit>),
    #[cfg(test)]
    Pause(Receiver<()>),
    #[cfg(test)]
    FailNextSave(smelt_store::StoreError),
    Close,
}

struct QueuedAudit {
    entry: protocol::request_log::RequestLogEntry,
    mode: smelt_store::RequestAuditPayloadMode,
}

struct Shared {
    state: Mutex<Published>,
    changed: Condvar,
    wake: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct Published {
    status: PersistenceStatus,
    unseen: bool,
}

impl Shared {
    fn update(&self, change: impl FnOnce(&mut PersistenceStatus)) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        change(&mut state.status);
        state.unseen = true;
        drop(state);
        self.changed.notify_all();
        self.wake.notify_one();
    }

    fn status(&self) -> PersistenceStatus {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
    }

    fn wait_until<T>(
        &self,
        deadline: Instant,
        mut ready: impl FnMut(&PersistenceStatus) -> Option<T>,
    ) -> Option<T> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(value) = ready(&state.status) {
                return Some(value);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// A close that has been requested and is waiting for the writer to finish.
pub(crate) struct PendingPersistenceClose {
    target: u64,
    deadline: Instant,
}

/// Handle to the session's writer thread.
pub(crate) struct SessionPersistence {
    tx: Sender<Command>,
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    submitted: u64,
    audits_submitted: u64,
}

impl SessionPersistence {
    /// Starts the writer. `writer` is a writer the caller already opened for this
    /// session; without one, the thread opens it.
    pub(crate) fn spawn(
        store: smelt_store::Store,
        session_id: String,
        writer: Option<smelt_store::SessionWriter>,
        wake: Arc<tokio::sync::Notify>,
    ) -> Result<Self, PersistenceCause> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(Published::default()),
            changed: Condvar::new(),
            wake,
        });
        let thread = std::thread::Builder::new()
            .name("smelt-session-writer".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || run(store, session_id, writer, rx, &shared)
            })
            .map_err(|error| {
                PersistenceCause::unavailable(format!("start session writer: {error}"))
            })?;
        Ok(Self {
            tx,
            shared,
            thread: Some(thread),
            submitted: 0,
            audits_submitted: 0,
        })
    }

    pub(crate) fn submit(&mut self, save: SessionSave) -> Result<(), PersistenceCause> {
        let seq = save.seq;
        self.tx
            .send(Command::Save(Box::new(save)))
            .map_err(|_| self.stopped_cause())?;
        self.submitted = self.submitted.max(seq);
        Ok(())
    }

    pub(crate) fn append_request_audit(
        &mut self,
        entry: protocol::request_log::RequestLogEntry,
        mode: smelt_store::RequestAuditPayloadMode,
    ) -> Result<(), PersistenceCause> {
        self.tx
            .send(Command::Audit(Box::new(QueuedAudit { entry, mode })))
            .map_err(|_| self.stopped_cause())?;
        self.audits_submitted += 1;
        Ok(())
    }

    pub(crate) fn status(&self) -> PersistenceStatus {
        self.shared.status()
    }

    /// Why the writer thread is gone: the session failed to open, or it stopped.
    fn stopped_cause(&self) -> PersistenceCause {
        match self.shared.status().opened {
            Some(Err(cause)) => cause,
            _ => PersistenceCause::unavailable("session writer stopped"),
        }
    }

    /// The status, if it changed since the last call.
    pub(crate) fn take_changed_status(&self) -> Option<PersistenceStatus> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut state.unseen).then(|| state.status.clone())
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }

    /// Waits until every submitted save is durable and every audit is taken.
    pub(crate) fn flush(&self, deadline: Instant) -> PersistenceFlushOutcome {
        self.shared
            .wait_until(deadline, |status| self.flushed(status))
            .unwrap_or(PersistenceFlushOutcome::Deadline)
    }

    /// The flush result, if every submitted save and audit is already handled.
    pub(crate) fn try_flush(&self) -> Option<PersistenceFlushOutcome> {
        self.flushed(&self.shared.status())
    }

    fn flushed(&self, status: &PersistenceStatus) -> Option<PersistenceFlushOutcome> {
        (status.audits_handled >= self.audits_submitted || status.finished)
            .then(|| status.outcome(self.submitted))
            .flatten()
    }

    /// Asks the writer to finish; poll with [`Self::poll_close`].
    pub(crate) fn begin_close(&self, deadline: Instant) -> PendingPersistenceClose {
        let _ = self.tx.send(Command::Close);
        PendingPersistenceClose {
            target: self.submitted,
            deadline,
        }
    }

    /// The close result once the writer finished or the deadline passed at `now`.
    pub(crate) fn poll_close(
        &mut self,
        pending: &PendingPersistenceClose,
        now: Instant,
    ) -> Option<PersistenceFlushOutcome> {
        if self.is_finished() {
            self.join();
            let status = self.status();
            return Some(
                status
                    .outcome(pending.target)
                    .unwrap_or(PersistenceFlushOutcome::Deadline),
            );
        }
        (now >= pending.deadline).then_some(PersistenceFlushOutcome::Deadline)
    }

    /// Finishes the writer, waiting until `deadline` for pending saves.
    pub(crate) fn close(&mut self, deadline: Instant) -> PersistenceFlushOutcome {
        let pending = self.begin_close(deadline);
        let finished = self
            .shared
            .wait_until(deadline, |status| status.finished.then_some(()))
            .is_some();
        if !finished {
            return PersistenceFlushOutcome::Deadline;
        }
        self.join();
        self.status()
            .outcome(pending.target)
            .unwrap_or(PersistenceFlushOutcome::Deadline)
    }

    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Stops the writer before it handles the next command, until the returned
    /// sender sends or drops.
    #[cfg(test)]
    pub(crate) fn pause(&self) -> Sender<()> {
        let (resume, paused) = mpsc::channel();
        let _ = self.tx.send(Command::Pause(paused));
        resume
    }

    /// Makes the next save that is not skipped fail with `error` instead of
    /// committing. Queued failures apply to successive saves.
    #[cfg(test)]
    pub(crate) fn fail_next_save(&self, error: smelt_store::StoreError) {
        let _ = self.tx.send(Command::FailNextSave(error));
    }
}

impl Drop for SessionPersistence {
    fn drop(&mut self) {
        // The thread commits what is queued, then releases the session.
        let _ = self.tx.send(Command::Close);
    }
}

fn run(
    store: smelt_store::Store,
    session_id: String,
    writer: Option<smelt_store::SessionWriter>,
    commands: Receiver<Command>,
    shared: &Shared,
) {
    let writer = match writer.map_or_else(|| store.writer(&session_id), Ok) {
        Ok(writer) => {
            shared.update(|status| status.opened = Some(Ok(())));
            writer
        }
        Err(error) => {
            let cause = PersistenceCause::from_store("open session", error);
            shared.update(|status| {
                status.opened = Some(Err(cause));
                status.finished = true;
            });
            return;
        }
    };
    let mut actor = Actor {
        writer,
        failed: false,
        pending_audits: Vec::new(),
        shared,
        #[cfg(test)]
        fail_next_saves: VecDeque::new(),
    };
    while let Ok(command) = commands.recv() {
        match command {
            Command::Save(save) => actor.save(*save),
            Command::Audit(audit) => actor.audit(*audit),
            #[cfg(test)]
            Command::Pause(resume) => {
                let _ = resume.recv();
            }
            #[cfg(test)]
            Command::FailNextSave(error) => actor.fail_next_saves.push_back(error),
            Command::Close => break,
        }
    }
    drop(actor);
    shared.update(|status| status.finished = true);
}

struct Actor<'a> {
    writer: smelt_store::SessionWriter,
    /// A save failed; later saves build on it and must wait for a resync.
    failed: bool,
    pending_audits: Vec<QueuedAudit>,
    shared: &'a Shared,
    #[cfg(test)]
    fail_next_saves: VecDeque<smelt_store::StoreError>,
}

impl Actor<'_> {
    fn save(&mut self, save: SessionSave) {
        if self.failed && !save.resync {
            self.shared.update(|status| status.handled = save.seq);
            return;
        }
        let _perf = smelt_perf::perf::begin("persist:save");
        #[cfg(test)]
        if let Some(error) = self.fail_next_saves.pop_front() {
            self.failed = true;
            let cause = PersistenceCause::from_store("save session", error);
            self.shared.update(|status| {
                status.failure = Some((save.seq, cause));
                status.handled = save.seq;
            });
            return;
        }
        match self.writer.commit(&save.identity, &save.commit) {
            Ok(head) => {
                self.failed = false;
                self.shared.update(|status| {
                    status.durable = Some(DurableSave {
                        seq: save.seq,
                        head,
                    });
                    status.failure = None;
                    status.handled = save.seq;
                });
                for audit in std::mem::take(&mut self.pending_audits) {
                    self.audit(audit);
                }
            }
            Err(error) => {
                self.failed = true;
                let cause = PersistenceCause::from_store("save session", error);
                self.shared.update(|status| {
                    status.failure = Some((save.seq, cause));
                    status.handled = save.seq;
                });
            }
        }
    }

    fn audit(&mut self, audit: QueuedAudit) {
        if !self.writer.exists() {
            if self.pending_audits.len() < MAX_PENDING_AUDITS {
                self.pending_audits.push(audit);
            }
        } else if let Err(error) = self.writer.append_request(&audit.entry, audit.mode) {
            let message = format!("request audit was not saved: {error}");
            self.shared
                .update(|status| status.audit_warning = Some(message));
        }
        self.shared.update(|status| status.audits_handled += 1);
    }
}

/// Saves `session` under `state_root` with `records` as its transcript, for test fixtures.
#[cfg(test)]
pub(crate) fn save_session_fixture(
    state_root: &std::path::Path,
    session: &smelt_core::session::Session,
    records: &[smelt_core::TranscriptBlockRecord],
) -> smelt_core::session::SessionStoreAddress {
    let storage = smelt_core::session::SessionStorage::new(state_root.to_path_buf());
    storage.save(session).expect("save session fixture");
    let address = storage.address(session.id.clone());
    write_transcript_record_suffix(&address, 0, records).expect("save transcript fixture");
    address
}

#[cfg(any(test, feature = "harness"))]
pub(crate) fn write_transcript_record_suffix(
    store: &smelt_core::session::SessionStoreAddress,
    start_record_idx: usize,
    records: &[smelt_core::TranscriptBlockRecord],
) -> Result<(), smelt_store::StoreError> {
    let records = records
        .iter()
        .enumerate()
        .map(|(offset, record)| smelt_core::TranscriptBlockRecordWithId {
            block_id: smelt_core::BlockId::new((start_record_idx + offset) as u64),
            record: record.clone(),
        })
        .collect::<Vec<_>>();
    write_transcript_record_suffix_with_ids(store, start_record_idx, &records)
}

/// Replaces stored transcript records from `start_record_idx` on, for test fixtures.
#[cfg(any(test, feature = "harness"))]
pub(crate) fn write_transcript_record_suffix_with_ids(
    store: &smelt_core::session::SessionStoreAddress,
    start_record_idx: usize,
    records: &[smelt_core::TranscriptBlockRecordWithId],
) -> Result<(), smelt_store::StoreError> {
    let state = store.reader()?.state()?;
    let rows = records
        .iter()
        .enumerate()
        .map(|(offset, record)| {
            smelt_core::transcript_model::transcript_block_row_with_block_idx(
                start_record_idx + offset,
                record.block_id.get(),
                &record.record,
            )
        })
        .collect::<Result<Vec<_>, smelt_store::StoreError>>()?;
    store
        .store
        .writer(&store.session_id)?
        .commit(
            &state.identity,
            &smelt_store::SessionCommit {
                metadata: state.metadata,
                transcript: Some(smelt_store::TranscriptRecordSuffix {
                    start: start_record_idx as u64,
                    records: rows,
                }),
                ..Default::default()
            },
        )
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn identity() -> smelt_store::SessionIdentity {
        smelt_store::SessionIdentity {
            id: SESSION_ID.into(),
            created_at: 1,
            parent_id: None,
        }
    }

    fn history_save(seq: u64, start: u64, texts: &[&str], resync: bool) -> SessionSave {
        SessionSave {
            seq,
            resync,
            identity: identity(),
            commit: smelt_store::SessionCommit {
                history: Some(smelt_store::HistorySuffix {
                    start,
                    items: texts
                        .iter()
                        .map(|text| protocol::HistoryItem::user(protocol::Content::text(*text)))
                        .collect(),
                }),
                ..Default::default()
            },
        }
    }

    fn spawn(store: &smelt_store::Store) -> SessionPersistence {
        SessionPersistence::spawn(
            store.clone(),
            SESSION_ID.into(),
            None,
            Arc::new(tokio::sync::Notify::new()),
        )
        .unwrap()
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn saves_commit_in_order_and_flush_waits_for_them() {
        let root = tempfile::tempdir().unwrap();
        let store = smelt_store::Store::new(root.path());
        let mut persistence = spawn(&store);
        persistence
            .submit(history_save(1, 0, &["a"], false))
            .unwrap();
        persistence
            .submit(history_save(2, 1, &["b", "c"], false))
            .unwrap();

        assert_eq!(
            persistence.flush(deadline()),
            PersistenceFlushOutcome::Durable
        );
        let status = persistence.status();
        assert_eq!(status.durable_seq(), 2);
        assert_eq!(status.durable.unwrap().head.history_len.get(), 3);
        assert_eq!(
            persistence.close(deadline()),
            PersistenceFlushOutcome::Durable
        );
        assert_eq!(
            store
                .reader(SESSION_ID)
                .unwrap()
                .head()
                .unwrap()
                .history_len
                .get(),
            3
        );
    }

    #[test]
    fn saves_after_a_failure_wait_for_a_resync() {
        let root = tempfile::tempdir().unwrap();
        let store = smelt_store::Store::new(root.path());
        let mut persistence = spawn(&store);
        persistence
            .submit(history_save(1, 0, &["a"], false))
            .unwrap();
        // Starts past the stored history, so the store rejects it.
        persistence
            .submit(history_save(2, 5, &["x"], false))
            .unwrap();
        persistence
            .submit(history_save(3, 1, &["b"], false))
            .unwrap();
        assert!(matches!(
            persistence.flush(deadline()),
            PersistenceFlushOutcome::Failed(_)
        ));
        assert_eq!(persistence.status().failure.as_ref().unwrap().0, 2);

        persistence
            .submit(history_save(4, 1, &["b", "c"], true))
            .unwrap();
        assert_eq!(
            persistence.flush(deadline()),
            PersistenceFlushOutcome::Durable
        );
        assert_eq!(persistence.status().failure, None);
        assert_eq!(
            store
                .reader(SESSION_ID)
                .unwrap()
                .head()
                .unwrap()
                .history_len
                .get(),
            3
        );
    }

    #[test]
    fn a_second_writer_reports_ownership() {
        let root = tempfile::tempdir().unwrap();
        let store = smelt_store::Store::new(root.path());
        let _owner = store.writer(SESSION_ID).unwrap();
        let mut persistence = spawn(&store);
        let opened = persistence
            .shared
            .wait_until(deadline(), |status| status.opened.clone())
            .expect("writer reports whether it opened");
        assert_eq!(
            opened.unwrap_err().class,
            PersistenceFailureClass::Ownership
        );
        while !persistence.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let save = persistence
            .submit(history_save(1, 0, &["a"], false))
            .unwrap_err();
        assert_eq!(save.class, PersistenceFailureClass::Ownership);
        let entry: protocol::request_log::RequestLogEntry =
            serde_json::from_value(serde_json::json!({
                "request_id": 1,
                "kind": "turn",
                "timestamp_ms": 1,
                "provider_kind": "openai",
                "api_base": "https://api.example.test",
                "model": "m",
                "url": "u",
                "body": {},
                "attempt": 1
            }))
            .unwrap();
        let audit = persistence
            .append_request_audit(entry, smelt_store::RequestAuditPayloadMode::Full)
            .unwrap_err();
        assert_eq!(audit.class, PersistenceFailureClass::Ownership);
    }

    #[test]
    fn audits_wait_for_the_first_save() {
        let root = tempfile::tempdir().unwrap();
        let store = smelt_store::Store::new(root.path());
        let mut persistence = spawn(&store);
        let entry: protocol::request_log::RequestLogEntry =
            serde_json::from_value(serde_json::json!({
                "request_id": 1,
                "kind": "turn",
                "timestamp_ms": 1,
                "provider_kind": "openai",
                "api_base": "https://api.example.test",
                "model": "m",
                "url": "u",
                "body": {},
                "attempt": 1
            }))
            .unwrap();
        persistence
            .append_request_audit(entry, smelt_store::RequestAuditPayloadMode::Full)
            .unwrap();
        persistence
            .submit(history_save(1, 0, &["a"], false))
            .unwrap();
        assert_eq!(
            persistence.close(deadline()),
            PersistenceFlushOutcome::Durable
        );
        let reader = store.reader(SESSION_ID).unwrap();
        assert_eq!(reader.request_stats().unwrap().request_count, 1);
    }
}
