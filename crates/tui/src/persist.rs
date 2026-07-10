//! Background session persistence.
//!
//! Serialisation and disk I/O run on a worker thread. The main loop sends
//! a session backend command; the worker writes requests in FIFO order. Call
//! [`Persister::flush`] when the on-disk state must be current (session load,
//! fork, shutdown).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// One image blob to write alongside the session.
pub(crate) struct Blob {
    pub(crate) filename: String,
    pub(crate) data_url: String,
}

pub(crate) struct PersistRequest {
    pub(crate) command: smelt_store::SessionCommit,
    pub(crate) blobs: Vec<Blob>,
}

pub(crate) struct PersistRequestAudit {
    pub(crate) session_id: String,
    pub(crate) entry: protocol::request_log::RequestLogEntry,
    pub(crate) payload_mode: smelt_store::RequestAuditPayloadMode,
}

#[derive(Clone, Debug)]
pub(crate) struct PersistRequestAuditFailure {
    pub(crate) session_id: String,
    pub(crate) message: String,
}

pub(crate) type PersistSaveKind = smelt_core::session_save::SessionSaveKind;

#[derive(Clone, Debug)]
pub(crate) struct PersistFailure {
    pub(crate) save_id: u64,
    pub(crate) session_id: String,
    pub(crate) message: String,
    pub(crate) commit_failure: Option<smelt_store::SessionCommitFailure>,
}

#[derive(Clone, Debug)]
pub(crate) enum SessionBackendEvent {
    Saved(smelt_store::SaveReceipt),
    Failed(PersistFailure),
    RequestAuditFailed(PersistRequestAuditFailure),
}

enum SessionBackendCommand {
    OpenOwned(
        smelt_core::session_id::SessionId,
        Sender<Result<(), String>>,
    ),
    CommitSession(Box<PersistRequest>),
    AppendRequestAudit(Box<PersistRequestAudit>),
    Flush(Sender<()>),
    Release(Sender<Result<(), String>>),
}

pub(crate) struct Persister {
    tx: Option<Sender<SessionBackendCommand>>,
    reports: Receiver<SessionBackendEvent>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Persister {
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let (report_tx, reports) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("smelt-persist".into())
            .spawn(move || worker_loop(rx, report_tx))
            .expect("spawn persist worker");
        Self {
            tx: Some(tx),
            reports,
            handle: Some(handle),
        }
    }

    pub(crate) fn open_owned(&self, session_id: &str) -> Result<(), String> {
        let session_id = smelt_core::session_id::SessionId::parse(session_id)
            .map_err(|err| format!("invalid session id: {err}"))?;
        let Some(tx) = &self.tx else {
            return Err("persistence worker is closed".into());
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(SessionBackendCommand::OpenOwned(session_id, reply_tx))
            .map_err(|_| "persistence worker is closed".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "persistence worker stopped during open".to_string())?
    }

    pub(crate) fn release(&self) -> Result<(), String> {
        let Some(tx) = &self.tx else {
            return Ok(());
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(SessionBackendCommand::Release(reply_tx))
            .map_err(|_| "persistence worker is closed".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "persistence worker stopped during release".to_string())?
    }

    pub(crate) fn save(&self, req: PersistRequest) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(SessionBackendCommand::CommitSession(Box::new(req)));
        }
    }

    pub(crate) fn append_request_audit(&self, req: PersistRequestAudit) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(SessionBackendCommand::AppendRequestAudit(Box::new(req)));
        }
    }

    pub(crate) fn drain_reports(&self) -> Vec<SessionBackendEvent> {
        let mut reports = Vec::new();
        while let Ok(report) = self.reports.try_recv() {
            reports.push(report);
        }
        reports
    }

    /// Block until all queued saves are written. No-op if the worker has exited.
    pub(crate) fn flush(&self) {
        let Some(tx) = &self.tx else { return };
        if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        let (done_tx, done_rx) = mpsc::channel();
        if tx.send(SessionBackendCommand::Flush(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }
}

impl Drop for Persister {
    fn drop(&mut self) {
        self.flush();
        let _ = self.release();
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

enum SessionBackendState {
    Closed,
    ReadOnly {
        session_id: smelt_core::session_id::SessionId,
        reason: String,
    },
    Owned {
        session_id: smelt_core::session_id::SessionId,
        writer: smelt_store::OwnedSessionWriter,
    },
}

struct SessionBackend {
    state: SessionBackendState,
}

impl SessionBackend {
    fn new() -> Self {
        Self {
            state: SessionBackendState::Closed,
        }
    }

    fn open_owned(&mut self, session_id: smelt_core::session_id::SessionId) -> Result<(), String> {
        if matches!(
            &self.state,
            SessionBackendState::Owned {
                session_id: current,
                ..
            } if current == &session_id
        ) {
            return Ok(());
        }
        self.release()?;
        let session_dir = smelt_core::session::session_dir(&session_id);
        smelt_core::session::create_private_dir_all(&session_dir)
            .map_err(|err| format!("create session directory: {err}"))?;
        match smelt_store::OwnedSessionWriter::open(&session_dir, session_id.as_str()) {
            Ok(writer) => {
                self.state = SessionBackendState::Owned { session_id, writer };
                Ok(())
            }
            Err(err) => {
                let reason = err.to_string();
                self.state = SessionBackendState::ReadOnly {
                    session_id,
                    reason: reason.clone(),
                };
                Err(reason)
            }
        }
    }

    fn writer(
        &mut self,
        session_id: &str,
    ) -> Result<(&smelt_store::OwnedSessionWriter, PathBuf), String> {
        let session_id = smelt_core::session_id::SessionId::parse(session_id)
            .map_err(|err| format!("invalid session id: {err}"))?;
        let already_owned = match &self.state {
            SessionBackendState::ReadOnly {
                session_id: current,
                reason,
            } if current == &session_id => return Err(reason.clone()),
            SessionBackendState::Owned {
                session_id: current,
                ..
            } if current == &session_id => true,
            _ => false,
        };
        if already_owned {
            smelt_perf::perf::record_value("store:db:cached_read_write", 1);
        } else {
            self.open_owned(session_id.clone())?;
        }
        match &self.state {
            SessionBackendState::Owned { writer, .. } => {
                Ok((writer, smelt_core::session::session_dir(&session_id)))
            }
            _ => Err("persistence backend did not enter owned state".into()),
        }
    }

    fn release(&mut self) -> Result<(), String> {
        match std::mem::replace(&mut self.state, SessionBackendState::Closed) {
            SessionBackendState::Owned { writer, .. } => {
                writer.release().map_err(|err| err.to_string())
            }
            SessionBackendState::Closed | SessionBackendState::ReadOnly { .. } => Ok(()),
        }
    }
}

fn worker_loop(rx: Receiver<SessionBackendCommand>, reports: Sender<SessionBackendEvent>) {
    let mut backend = SessionBackend::new();
    while let Ok(cmd) = rx.recv() {
        match cmd {
            SessionBackendCommand::OpenOwned(session_id, reply) => {
                let _ = reply.send(backend.open_owned(session_id));
            }
            SessionBackendCommand::CommitSession(req) => {
                let result = backend
                    .writer(&req.command.session_id)
                    .map_err(persist_write_error)
                    .and_then(|(writer, session_dir)| write(&req, writer, &session_dir));
                report_save_result(result, &req, &reports);
            }
            SessionBackendCommand::AppendRequestAudit(req) => {
                let result = backend
                    .writer(&req.session_id)
                    .and_then(|(writer, _)| write_request_audit(&req, writer));
                report_request_audit_result(result, &req, &reports);
            }
            SessionBackendCommand::Flush(done) => {
                let _ = done.send(());
            }
            SessionBackendCommand::Release(done) => {
                let _ = done.send(backend.release());
            }
        }
    }
    let _ = backend.release();
}

fn report_save_result(
    result: Result<smelt_store::SaveReceipt, PersistWriteError>,
    req: &PersistRequest,
    reports: &Sender<SessionBackendEvent>,
) {
    let report = match result {
        Ok(receipt) => SessionBackendEvent::Saved(receipt),
        Err(err) => SessionBackendEvent::Failed(PersistFailure {
            save_id: req.command.save_id.get(),
            session_id: req.command.session_id.clone(),
            message: err.message(),
            commit_failure: err.commit_failure(),
        }),
    };
    let _ = reports.send(report);
}

fn report_request_audit_result(
    result: Result<i64, String>,
    req: &PersistRequestAudit,
    reports: &Sender<SessionBackendEvent>,
) {
    if let Err(message) = result {
        let _ = reports.send(SessionBackendEvent::RequestAuditFailed(
            PersistRequestAuditFailure {
                session_id: req.session_id.clone(),
                message,
            },
        ));
    }
}

#[derive(Clone, Debug)]
enum PersistWriteError {
    Message(String),
    Commit(smelt_store::SessionCommitFailure),
}

impl PersistWriteError {
    fn message(&self) -> String {
        match self {
            Self::Message(message) => message.clone(),
            Self::Commit(failure) => format!(
                "save session database: {}",
                describe_commit_failure(failure)
            ),
        }
    }

    fn commit_failure(&self) -> Option<smelt_store::SessionCommitFailure> {
        match self {
            Self::Commit(failure) => Some(failure.clone()),
            Self::Message(_) => None,
        }
    }
}

fn persist_write_error(message: impl Into<String>) -> PersistWriteError {
    PersistWriteError::Message(message.into())
}

fn describe_commit_failure(failure: &smelt_store::SessionCommitFailure) -> String {
    match failure {
        smelt_store::SessionCommitFailure::SessionMismatch { expected, actual } => {
            format!(
                "session id mismatch: expected {expected}, actual {:?}",
                actual
            )
        }
        smelt_store::SessionCommitFailure::StaleRevision { base, current } => {
            format!(
                "stale revision: base {}, current {}",
                base.get(),
                current.get()
            )
        }
        smelt_store::SessionCommitFailure::StaleHistoryBase { base, current } => {
            format!(
                "stale history base: base {}, current {}",
                base.get(),
                current.get()
            )
        }
        smelt_store::SessionCommitFailure::StaleDescriptorBase { base, current } => format!(
            "stale descriptor base: base {}, current {}",
            base.get(),
            current.get()
        ),
        smelt_store::SessionCommitFailure::InvalidHistorySuffix {
            start,
            final_len,
            item_count,
        } => format!(
            "invalid history suffix: start {}, final_len {}, item_count {}",
            start.get(),
            final_len.get(),
            item_count
        ),
        smelt_store::SessionCommitFailure::InvalidDescriptorSuffix { start, current_len } => {
            format!(
                "invalid descriptor suffix: start {}, current_len {}",
                start.get(),
                current_len.get()
            )
        }
        smelt_store::SessionCommitFailure::InvalidSideTableSuffix { start, final_len } => {
            format!(
                "invalid side-table suffix: start {}, final history length {}",
                start.get(),
                final_len.get()
            )
        }
        smelt_store::SessionCommitFailure::InvalidSideTableRow {
            table,
            index,
            final_len,
            bound,
        } => {
            let boundary = match bound {
                smelt_store::HistoryIndexBound::BeforeFinalLen => "before",
                smelt_store::HistoryIndexBound::AtOrBeforeFinalLen => "at or before",
            };
            format!(
                "invalid side-table row: {table} index {} must be {boundary} final history length {}",
                index.get(),
                final_len.get()
            )
        }
        smelt_store::SessionCommitFailure::OwnershipLost => {
            "session writer ownership was lost".into()
        }
        smelt_store::SessionCommitFailure::Integrity { message } => message.clone(),
    }
}

fn record_save_receipt(receipt: &smelt_store::SaveReceipt) {
    smelt_perf::perf::record_value(
        "persist:write:previous_revision",
        receipt.previous_revision.get(),
    );
    smelt_perf::perf::record_value("persist:write:revision", receipt.revision.get());
    smelt_perf::perf::record_value("persist:write:history_len", receipt.history_len.get());
    smelt_perf::perf::record_value("persist:write:descriptor_len", receipt.descriptor_len.get());
}

fn write(
    req: &PersistRequest,
    writer: &smelt_store::OwnedSessionWriter,
    session_dir: &Path,
) -> Result<smelt_store::SaveReceipt, PersistWriteError> {
    let _perf = smelt_perf::perf::begin("persist:write");
    smelt_perf::perf::record_value(
        "persist:write:history_items",
        req.command.history.items.len() as u64,
    );
    smelt_perf::perf::record_value("persist:write:blobs", req.blobs.len() as u64);
    let descriptor_records = req
        .command
        .descriptors
        .as_ref()
        .map_or(0, |descriptors| descriptors.records.len() as u64);
    smelt_perf::perf::record_value("persist:write:descriptor_records", descriptor_records);

    let blob_dir = session_dir.join("blobs");
    let url_to_blob = write_blobs(&blob_dir, &req.blobs).map_err(persist_write_error)?;
    let mut command = req.command.clone();
    if !url_to_blob.is_empty() {
        smelt_core::session::externalize_blobs(&mut command.history.items, &url_to_blob);
    }
    let receipt = writer
        .commit_session(&command)
        .map_err(PersistWriteError::Commit)?;
    record_save_receipt(&receipt);
    let (descriptor_start_idx, descriptor_records) = command
        .descriptors
        .as_ref()
        .map(|descriptors| (descriptors.start.get(), descriptors.records.len() as u64))
        .unwrap_or((0, 0));
    smelt_perf::perf::record_value("persist:write:descriptor_start_idx", descriptor_start_idx);
    smelt_perf::perf::record_value("persist:write:descriptor_records", descriptor_records);
    if let Err(err) = smelt_core::session::refresh_derived_files(session_dir) {
        smelt_perf::perf::record_value("persist:write:derived_refresh_failed", 1);
        eprintln!(
            "smelt: failed to refresh derived files for session {}: {err}",
            receipt.session_id
        );
    }
    Ok(receipt)
}

fn write_request_audit(
    req: &PersistRequestAudit,
    writer: &smelt_store::OwnedSessionWriter,
) -> Result<i64, String> {
    let _perf = smelt_perf::perf::begin("persist:request_audit");
    writer
        .append_request_attempt(&req.entry, req.payload_mode)
        .map_err(|err| err.to_string())
}

fn write_blobs(
    blob_dir: &std::path::Path,
    blobs: &[Blob],
) -> Result<std::collections::HashMap<String, String>, String> {
    use std::collections::HashMap;
    let mut url_to_blob = HashMap::new();
    if blobs.is_empty() {
        return Ok(url_to_blob);
    }
    smelt_core::session::create_private_dir_all(blob_dir)
        .map_err(|err| format!("create blob directory: {err}"))?;
    for b in blobs {
        let path: PathBuf = blob_dir.join(&b.filename);
        smelt_core::session::write_private_file(&path, b.data_url.as_bytes())
            .map_err(|err| format!("write blob {}: {err}", b.filename))?;
        url_to_blob.insert(b.data_url.clone(), format!("blob:{}", b.filename));
    }
    Ok(url_to_blob)
}

#[cfg(any(test, feature = "harness"))]
pub(crate) fn write_transcript_descriptor_suffix(
    session_dir: &std::path::Path,
    start_descriptor_idx: usize,
    records: &[smelt_core::TranscriptBlockRecord],
) -> Result<(), smelt_store::StoreError> {
    let session_id = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| smelt_store::StoreError::Integrity("session directory has no id".into()))?;
    let maintenance = smelt_store::SessionMaintenance::open(session_dir, session_id)?;
    let rows = records
        .iter()
        .enumerate()
        .map(|(offset, record)| {
            let descriptor_idx = start_descriptor_idx + offset;
            let record = smelt_core::TranscriptBlockRecordWithId {
                block_id: smelt_core::BlockId::new(descriptor_idx as u64),
                record: record.clone(),
            };
            smelt_core::transcript_model::transcript_descriptor_row_with_block_idx(
                descriptor_idx,
                record.block_id.get(),
                &record.record,
            )
        })
        .collect::<Result<Vec<_>, smelt_store::StoreError>>()?;
    maintenance.replace_transcript_descriptor_suffix(start_descriptor_idx, &rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SECOND_SESSION_ID: &str =
        "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    struct StateHomeGuard(Option<std::ffi::OsString>);

    impl StateHomeGuard {
        fn install(path: &Path) -> Self {
            let previous = std::env::var_os("XDG_STATE_HOME");
            std::env::set_var("XDG_STATE_HOME", path);
            Self(previous)
        }
    }

    impl Drop for StateHomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("XDG_STATE_HOME", value),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
    }

    fn commit(session_id: &str, base_revision: u64) -> smelt_store::SessionCommit {
        smelt_store::SessionCommit {
            session_id: session_id.into(),
            save_id: smelt_store::SaveId::new(1),
            base_revision: smelt_store::Revision::new(base_revision),
            base_history_len: smelt_store::HistoryLen::ZERO,
            base_descriptor_len: smelt_store::DescriptorLen::ZERO,
            state: smelt_store::SessionState {
                id: session_id.into(),
                title: None,
                slug: None,
                first_user_message: None,
                cwd: None,
                mode: None,
                reasoning_effort: None,
                model: None,
                parent_id: None,
                accounting_json: None,
                checkpoint_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: 0.0,
                revision: 0,
                history_len: 1,
                created_at: 1,
                updated_at: 1,
            },
            history: smelt_store::HistorySuffix {
                start: smelt_store::HistoryIndex::ZERO,
                final_len: smelt_store::HistoryLen::new(1),
                items: vec![protocol::HistoryItem::user(protocol::Content::text(
                    "saved",
                ))],
            },
            side_tables: smelt_store::SideTableSuffixes {
                start: smelt_store::HistoryIndex::ZERO,
                turn_metas: Vec::new(),
                metadata_snapshots: Vec::new(),
                context_snapshots: Vec::new(),
            },
            descriptors: None,
        }
    }

    #[test]
    fn failed_commit_currently_leaves_published_directory_and_attachment_blob() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("session-a");
        std::fs::create_dir(&session_dir).unwrap();
        let request = PersistRequest {
            command: commit("session-a", 9),
            blobs: vec![Blob {
                filename: "attachment.png".into(),
                data_url: "data:image/png;base64,AAAA".into(),
            }],
        };
        let writer = smelt_store::OwnedSessionWriter::open(&session_dir, "session-a").unwrap();

        let result = write(&request, &writer, &session_dir);

        assert!(result.is_err(), "stale commit should fail");
        assert!(
            session_dir.join("session.db").is_file(),
            "database creation publishes the destination before a valid first commit"
        );
        assert_eq!(
            std::fs::read_to_string(session_dir.join("blobs/attachment.png")).unwrap(),
            "data:image/png;base64,AAAA",
            "attachment is written before the failed canonical transaction"
        );
        let db = smelt_store::SessionReader::open_existing(&session_dir).unwrap();
        assert!(db.session_state().unwrap().is_none());
    }

    #[test]
    fn derived_refresh_failure_does_not_undo_canonical_commit() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("session-a");
        std::fs::create_dir_all(session_dir.join("meta.json")).unwrap();
        let request = PersistRequest {
            command: commit("session-a", 0),
            blobs: Vec::new(),
        };
        let writer = smelt_store::OwnedSessionWriter::open(&session_dir, "session-a").unwrap();

        let receipt = write(&request, &writer, &session_dir).expect("canonical commit succeeds");

        assert_eq!(receipt.revision.get(), 1);
        assert!(session_dir.join("meta.json").is_dir());
        let db = smelt_store::SessionReader::open_existing(&session_dir).unwrap();
        assert_eq!(db.session_state().unwrap().unwrap().history_len, 1);
        assert_eq!(db.read_history_items_range(0..1).unwrap().len(), 1);
    }

    #[test]
    fn opening_another_session_releases_the_previous_session_lock() {
        let _serial = crate::app::test_harness::test_home_guard();
        let dir = tempfile::tempdir().unwrap();
        let _state_home = StateHomeGuard::install(dir.path());
        let first_dir = smelt_core::session::dir_for_id(SESSION_ID);
        let second_dir = smelt_core::session::dir_for_id(SECOND_SESSION_ID);
        let persister = Persister::spawn();

        persister.open_owned(SESSION_ID).unwrap();
        assert!(smelt_store::OwnedSessionWriter::open(&first_dir, SESSION_ID).is_err());

        persister.open_owned(SECOND_SESSION_ID).unwrap();
        let first_replacement =
            smelt_store::OwnedSessionWriter::open(&first_dir, SESSION_ID).unwrap();
        assert!(smelt_store::OwnedSessionWriter::open(&second_dir, SECOND_SESSION_ID).is_err());

        persister.release().unwrap();
        let second_replacement =
            smelt_store::OwnedSessionWriter::open(&second_dir, SECOND_SESSION_ID).unwrap();
        drop((first_replacement, second_replacement));
    }

    #[test]
    fn request_audit_is_written_by_worker() {
        let _serial = crate::app::test_harness::test_home_guard();
        let dir = tempfile::tempdir().unwrap();
        let _state_home = StateHomeGuard::install(dir.path());
        let persister = Persister::spawn();
        persister.append_request_audit(PersistRequestAudit {
            session_id: SESSION_ID.into(),
            payload_mode: smelt_store::RequestAuditPayloadMode::Summary,
            entry: protocol::request_log::RequestLogEntry {
                request_id: 42,
                kind: "turn".into(),
                turn_id: Some(42),
                ask_id: None,
                history_len: Some(1),
                timestamp_ms: 1000,
                provider_kind: "openai".into(),
                api_base: "https://api.example.test".into(),
                model: "model-a".into(),
                url: "https://api.example.test/v1/chat/completions".into(),
                http_status: Some(200),
                body: serde_json::json!({"model": "model-a"}),
                prompt_cache_key: None,
                stream: true,
                system_prompt: None,
                messages: None,
                tools: None,
                response: None,
                usage: None,
                cost_usd: None,
                tokens_per_sec: None,
                elapsed_ms: Some(250),
                attempt: 1,
                error: None,
                background: false,
            },
        });

        persister.flush();
        assert!(persister.drain_reports().is_empty());

        let session_dir = smelt_core::session::dir_for_id(SESSION_ID);
        let db = smelt_store::SessionReader::open_existing(&session_dir).unwrap();
        let attempts = db
            .query_request_attempts(&smelt_store::RequestAuditQuery::default())
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].request_id.as_deref(), Some("42"));
    }
}
