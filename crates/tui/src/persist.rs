//! Background session persistence.
//!
//! Serialisation and disk I/O run on a worker thread. The main loop sends
//! a session backend command; the worker writes requests in FIFO order. Call
//! [`Persister::flush`] when the on-disk state must be current (session load,
//! fork, shutdown).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

const MAX_PENDING_FULL_AUDIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_AUDIT_SUMMARY_TEXT_BYTES: usize = 512;

pub(crate) struct PersistRequest {
    pub(crate) command: smelt_store::SessionCommit,
}

pub(crate) struct PersistRequestAudit {
    pub(crate) session_id: String,
    pub(crate) entry: protocol::request_log::RequestLogEntry,
    pub(crate) payload_mode: smelt_store::RequestAuditPayloadMode,
    pub(crate) payload_capture_skipped_bytes: Option<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct PersistRequestAuditFailure {
    pub(crate) session_id: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PersistRequestAuditPayloadSkipped {
    pub(crate) session_id: String,
    pub(crate) estimated_bytes: usize,
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
    Saved {
        receipt: smelt_store::SaveReceipt,
        warning: Option<String>,
    },
    Failed(PersistFailure),
    RequestAuditFailed(PersistRequestAuditFailure),
    RequestAuditPayloadSkipped(PersistRequestAuditPayloadSkipped),
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

impl SessionBackendCommand {
    fn estimated_payload_bytes(&self) -> usize {
        match self {
            Self::CommitSession(req) => serialized_size(&req.command),
            Self::AppendRequestAudit(req) => serialized_size(&req.entry),
            Self::OpenOwned(..) | Self::Flush(_) | Self::Release(_) => 0,
        }
    }
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("serialized payload size overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_size(value: &impl serde::Serialize) -> usize {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, value).map_or(0, |()| writer.bytes)
}

struct QueuedCommand {
    command: SessionBackendCommand,
    estimated_payload_bytes: usize,
    reserved_full_audit_bytes: usize,
}

fn reserve_bytes(counter: &AtomicUsize, bytes: usize, limit: usize) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(bytes).filter(|next| *next <= limit)
        })
        .is_ok()
}

fn compact_request_audit(req: &mut PersistRequestAudit, raw_payload_bytes: usize) {
    let raw_body_size = serialized_size(&req.entry.body) as u64;
    req.entry.body = serde_json::Value::Null;
    if let Some(response) = &mut req.entry.response {
        response.content = response
            .content
            .take()
            .map(|text| audit_summary_text(&text));
        response.reasoning = response
            .reasoning
            .take()
            .map(|text| audit_summary_text(&text));
        response.tool_calls = None;
        response.raw = None;
    }
    if let Some(error) = &mut req.entry.error {
        error.message = audit_summary_text(&error.message);
        error.body = None;
    }
    req.payload_mode = smelt_store::RequestAuditPayloadMode::Summary {
        raw_body_size: Some(raw_body_size),
    };
    req.payload_capture_skipped_bytes = Some(raw_payload_bytes);
}

fn audit_summary_text(text: &str) -> String {
    smelt_buffer::text::slice(text, 0..MAX_AUDIT_SUMMARY_TEXT_BYTES).to_string()
}

pub(crate) struct Persister {
    tx: Option<Sender<QueuedCommand>>,
    reports: Receiver<SessionBackendEvent>,
    queued_commands: Arc<AtomicUsize>,
    queued_payload_bytes: Arc<AtomicUsize>,
    pending_full_audit_bytes: Arc<AtomicUsize>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Persister {
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let (report_tx, reports) = mpsc::channel();
        let queued_commands = Arc::new(AtomicUsize::new(0));
        let queued_payload_bytes = Arc::new(AtomicUsize::new(0));
        let pending_full_audit_bytes = Arc::new(AtomicUsize::new(0));
        let worker_queue = Arc::clone(&queued_commands);
        let worker_payload_bytes = Arc::clone(&queued_payload_bytes);
        let worker_full_audit_bytes = Arc::clone(&pending_full_audit_bytes);
        let handle = thread::Builder::new()
            .name("smelt-persist".into())
            .spawn(move || {
                worker_loop(
                    rx,
                    report_tx,
                    worker_queue,
                    worker_payload_bytes,
                    worker_full_audit_bytes,
                )
            })
            .expect("spawn persist worker");
        Self {
            tx: Some(tx),
            reports,
            queued_commands,
            queued_payload_bytes,
            pending_full_audit_bytes,
            handle: Some(handle),
        }
    }

    fn enqueue(
        &self,
        command: SessionBackendCommand,
        reserved_full_audit_bytes: usize,
    ) -> Result<(), ()> {
        let Some(tx) = &self.tx else {
            return Err(());
        };
        let estimated_payload_bytes = if smelt_perf::perf::enabled() {
            command.estimated_payload_bytes()
        } else {
            0
        };
        let depth = self.queued_commands.fetch_add(1, Ordering::AcqRel) + 1;
        let payload_bytes = self
            .queued_payload_bytes
            .fetch_add(estimated_payload_bytes, Ordering::AcqRel)
            .saturating_add(estimated_payload_bytes);
        smelt_perf::perf::record_value("persist:queue:depth", depth as u64);
        smelt_perf::perf::record_value(
            "persist:queue:command_payload_bytes",
            estimated_payload_bytes as u64,
        );
        smelt_perf::perf::record_value("persist:queue:payload_bytes", payload_bytes as u64);
        if tx
            .send(QueuedCommand {
                command,
                estimated_payload_bytes,
                reserved_full_audit_bytes,
            })
            .is_err()
        {
            self.queued_commands.fetch_sub(1, Ordering::AcqRel);
            self.queued_payload_bytes
                .fetch_sub(estimated_payload_bytes, Ordering::AcqRel);
            self.pending_full_audit_bytes
                .fetch_sub(reserved_full_audit_bytes, Ordering::AcqRel);
            return Err(());
        }
        Ok(())
    }

    pub(crate) fn open_owned(&self, session_id: &str) -> Result<(), String> {
        let session_id = smelt_core::session_id::SessionId::parse(session_id)
            .map_err(|err| format!("invalid session id: {err}"))?;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.enqueue(SessionBackendCommand::OpenOwned(session_id, reply_tx), 0)
            .map_err(|()| "persistence worker is closed".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "persistence worker stopped during open".to_string())?
    }

    pub(crate) fn release(&self) -> Result<(), String> {
        if self.tx.is_none() {
            return Ok(());
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.enqueue(SessionBackendCommand::Release(reply_tx), 0)
            .map_err(|()| "persistence worker is closed".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "persistence worker stopped during release".to_string())?
    }

    pub(crate) fn save(&self, req: PersistRequest) {
        let _ = self.enqueue(SessionBackendCommand::CommitSession(Box::new(req)), 0);
    }

    pub(crate) fn append_request_audit(&self, mut req: PersistRequestAudit) {
        if self.tx.is_none() {
            return;
        }
        req.entry.system_prompt = None;
        req.entry.messages = None;
        req.entry.tools = None;
        let estimated_bytes = serialized_size(&req.entry);
        let mut reserved_full_audit_bytes = 0;
        let mut payload_capture_skipped = false;
        if req.payload_mode == smelt_store::RequestAuditPayloadMode::Full {
            if reserve_bytes(
                &self.pending_full_audit_bytes,
                estimated_bytes,
                MAX_PENDING_FULL_AUDIT_BYTES,
            ) {
                reserved_full_audit_bytes = estimated_bytes;
            } else {
                compact_request_audit(&mut req, estimated_bytes);
                payload_capture_skipped = true;
                smelt_perf::perf::record_value("persist:queue:audit_payload_skipped", 1);
            }
        }
        if payload_capture_skipped {
            req.payload_capture_skipped_bytes = Some(estimated_bytes);
        }
        let _ = self.enqueue(
            SessionBackendCommand::AppendRequestAudit(Box::new(req)),
            reserved_full_audit_bytes,
        );
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
        if self.tx.is_none() || self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        let (done_tx, done_rx) = mpsc::channel();
        if self
            .enqueue(SessionBackendCommand::Flush(done_tx), 0)
            .is_ok()
        {
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
        writer: Box<smelt_store::OwnedSessionWriter>,
        staged: Option<smelt_core::session::StagedSessionDir>,
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
        let staged = match std::fs::symlink_metadata(&session_dir) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => None,
            Ok(_) => {
                return Err(format!(
                    "session path is not a directory: {}",
                    session_dir.display()
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(
                smelt_core::session::StagedSessionDir::create(&session_id)
                    .map_err(|err| format!("stage session directory: {err}"))?,
            ),
            Err(err) => return Err(format!("inspect session directory: {err}")),
        };
        let writer_dir = staged
            .as_ref()
            .map_or(session_dir.as_path(), |staged| staged.path());
        match smelt_store::OwnedSessionWriter::open(writer_dir, session_id.as_str()) {
            Ok(writer) => {
                self.state = SessionBackendState::Owned {
                    session_id,
                    writer: Box::new(writer),
                    staged,
                };
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
                let session_dir = writer
                    .path()
                    .parent()
                    .expect("owned session database parent")
                    .to_path_buf();
                Ok((writer, session_dir))
            }
            _ => Err("persistence backend did not enter owned state".into()),
        }
    }

    fn publish_staged(&mut self) -> Result<Option<String>, String> {
        let state = std::mem::replace(&mut self.state, SessionBackendState::Closed);
        let (session_id, writer, staged) = match state {
            SessionBackendState::Owned {
                session_id,
                writer,
                staged,
            } => (session_id, writer, staged),
            state => {
                self.state = state;
                return Ok(None);
            }
        };
        let Some(staged) = staged else {
            self.state = SessionBackendState::Owned {
                session_id,
                writer,
                staged: None,
            };
            return Ok(None);
        };
        (*writer)
            .release()
            .map_err(|err| format!("release staged session writer: {err}"))?;
        let destination = staged
            .publish()
            .map_err(|err| format!("publish staged session: {err}"))?;
        match smelt_store::OwnedSessionWriter::open(&destination, session_id.as_str()) {
            Ok(writer) => {
                self.state = SessionBackendState::Owned {
                    session_id,
                    writer: Box::new(writer),
                    staged: None,
                };
                Ok(None)
            }
            Err(err) => {
                let reason = format!(
                    "session was saved and published, but write ownership could not be reacquired: {err}"
                );
                self.state = SessionBackendState::ReadOnly {
                    session_id,
                    reason: reason.clone(),
                };
                Ok(Some(reason))
            }
        }
    }

    fn release(&mut self) -> Result<(), String> {
        match std::mem::replace(&mut self.state, SessionBackendState::Closed) {
            SessionBackendState::Owned { writer, .. } => {
                (*writer).release().map_err(|err| err.to_string())
            }
            SessionBackendState::Closed | SessionBackendState::ReadOnly { .. } => Ok(()),
        }
    }
}

fn worker_loop(
    rx: Receiver<QueuedCommand>,
    reports: Sender<SessionBackendEvent>,
    queued_commands: Arc<AtomicUsize>,
    queued_payload_bytes: Arc<AtomicUsize>,
    pending_full_audit_bytes: Arc<AtomicUsize>,
) {
    smelt_core::session::cleanup_abandoned_session_artifacts();
    let mut backend = SessionBackend::new();
    while let Ok(queued) = rx.recv() {
        let depth = queued_commands
            .fetch_sub(1, Ordering::AcqRel)
            .saturating_sub(1);
        let payload_bytes = queued_payload_bytes
            .fetch_sub(queued.estimated_payload_bytes, Ordering::AcqRel)
            .saturating_sub(queued.estimated_payload_bytes);
        smelt_perf::perf::record_value("persist:queue:remaining", depth as u64);
        smelt_perf::perf::record_value(
            "persist:queue:remaining_payload_bytes",
            payload_bytes as u64,
        );
        let reserved_full_audit_bytes = queued.reserved_full_audit_bytes;
        match queued.command {
            SessionBackendCommand::OpenOwned(session_id, reply) => {
                let _ = reply.send(backend.open_owned(session_id));
            }
            SessionBackendCommand::CommitSession(req) => {
                let result = backend
                    .writer(&req.command.session_id)
                    .map_err(persist_write_error)
                    .and_then(|(writer, session_dir)| write(&req, writer, &session_dir));
                let result = result.and_then(|mut success| {
                    let publish_warning = backend.publish_staged().map_err(persist_write_error)?;
                    if let Some(publish_warning) = publish_warning {
                        success.warning = Some(match success.warning.take() {
                            Some(warning) => format!("{warning}; {publish_warning}"),
                            None => publish_warning,
                        });
                    }
                    Ok(success)
                });
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
        let full_audit_bytes = pending_full_audit_bytes
            .fetch_sub(reserved_full_audit_bytes, Ordering::AcqRel)
            .saturating_sub(reserved_full_audit_bytes);
        smelt_perf::perf::record_value(
            "persist:queue:remaining_full_audit_bytes",
            full_audit_bytes as u64,
        );
    }
    let _ = backend.release();
}

struct PersistSuccess {
    receipt: smelt_store::SaveReceipt,
    warning: Option<String>,
}

fn report_save_result(
    result: Result<PersistSuccess, PersistWriteError>,
    req: &PersistRequest,
    reports: &Sender<SessionBackendEvent>,
) {
    let report = match result {
        Ok(success) => SessionBackendEvent::Saved {
            receipt: success.receipt,
            warning: success.warning,
        },
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
    match result {
        Err(message) => {
            let _ = reports.send(SessionBackendEvent::RequestAuditFailed(
                PersistRequestAuditFailure {
                    session_id: req.session_id.clone(),
                    message,
                },
            ));
        }
        Ok(_) => {
            if let Some(estimated_bytes) = req.payload_capture_skipped_bytes {
                let _ = reports.send(SessionBackendEvent::RequestAuditPayloadSkipped(
                    PersistRequestAuditPayloadSkipped {
                        session_id: req.session_id.clone(),
                        estimated_bytes,
                    },
                ));
            }
        }
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
) -> Result<PersistSuccess, PersistWriteError> {
    let _perf = smelt_perf::perf::begin("persist:write");
    smelt_perf::perf::record_value(
        "persist:write:history_items",
        req.command.history.items.len() as u64,
    );
    let descriptor_records = req
        .command
        .descriptors
        .as_ref()
        .map_or(0, |descriptors| descriptors.records.len() as u64);
    smelt_perf::perf::record_value("persist:write:descriptor_records", descriptor_records);

    let command = &req.command;
    let receipt = writer
        .commit_session(command)
        .map_err(PersistWriteError::Commit)?;
    let mut warnings = Vec::new();
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
        warnings.push(format!(
            "session {} was saved, but derived cache refresh failed: {err}",
            receipt.session_id
        ));
    }
    let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    Ok(PersistSuccess { receipt, warning })
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

    fn request_audit(body: serde_json::Value) -> PersistRequestAudit {
        PersistRequestAudit {
            session_id: SESSION_ID.into(),
            payload_mode: smelt_store::RequestAuditPayloadMode::Full,
            payload_capture_skipped_bytes: None,
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
                body,
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
        }
    }

    #[test]
    fn queue_payload_estimate_accounts_for_serialized_commit_data() {
        let mut command = commit(SESSION_ID, 0);
        command.history.items = vec![protocol::HistoryItem::user(protocol::Content::text(
            "a queued payload",
        ))];
        let expected = serde_json::to_vec(&command).unwrap().len();
        let queued = SessionBackendCommand::CommitSession(Box::new(PersistRequest { command }));

        assert_eq!(queued.estimated_payload_bytes(), expected);
    }

    #[test]
    fn full_audit_budget_overflow_compacts_payload_without_losing_body_size() {
        let counter = AtomicUsize::new(0);
        assert!(reserve_bytes(&counter, 10, 16));
        assert!(!reserve_bytes(&counter, 7, 16));
        assert_eq!(counter.load(Ordering::Acquire), 10);

        let body = serde_json::json!({"prompt": "x".repeat(1024)});
        let raw_body_size = serialized_size(&body);
        let mut req = request_audit(body);
        let full_size = serialized_size(&req.entry);

        compact_request_audit(&mut req, full_size);

        assert_eq!(req.entry.body, serde_json::Value::Null);
        assert_eq!(req.payload_capture_skipped_bytes, Some(full_size));
        assert_eq!(
            req.payload_mode,
            smelt_store::RequestAuditPayloadMode::Summary {
                raw_body_size: Some(raw_body_size as u64),
            }
        );
        assert!(serialized_size(&req.entry) < full_size);
    }

    #[test]
    fn failed_commit_does_not_publish_attachment_or_session_state() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("session-a");
        std::fs::create_dir(&session_dir).unwrap();
        let request = PersistRequest {
            command: commit("session-a", 9),
        };
        let writer = smelt_store::OwnedSessionWriter::open(&session_dir, "session-a").unwrap();

        let result = write(&request, &writer, &session_dir);

        assert!(result.is_err(), "stale commit should fail");
        assert!(
            session_dir.join("session.db").is_file(),
            "database creation publishes the destination before a valid first commit"
        );
        assert!(
            !session_dir.join("blobs/attachment.png").exists(),
            "failed canonical transaction must not publish its attachment"
        );
        let staging_root = session_dir.join(".blob-staging");
        if staging_root.exists() {
            assert_eq!(std::fs::read_dir(staging_root).unwrap().count(), 0);
        }
        let db = smelt_store::SessionDb::open_read_only(session_dir.join("session.db")).unwrap();
        assert!(db.session_state().unwrap().is_none());
        assert_eq!(
            db.connection()
                .query_row("SELECT COUNT(*) FROM objects", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn attachment_is_stored_in_the_canonical_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("session-a");
        std::fs::create_dir(&session_dir).unwrap();
        let data_url = "data:image/png;base64,AAAA";
        let mut command = commit("session-a", 0);
        command.history.items = vec![protocol::HistoryItem::user(protocol::Content::with_images(
            "attached".into(),
            vec![("attachment.png".into(), data_url.into())],
        ))];
        let request = PersistRequest { command };
        let writer = smelt_store::OwnedSessionWriter::open(&session_dir, "session-a").unwrap();

        let success = write(&request, &writer, &session_dir).expect("canonical commit succeeds");

        assert_eq!(success.receipt.revision.get(), 1);
        assert!(success.warning.is_none());
        assert!(!session_dir.join("blobs").exists());
        let db = smelt_store::SessionDb::open_read_only(session_dir.join("session.db")).unwrap();
        assert_eq!(
            db.connection()
                .query_row("SELECT COUNT(*) FROM objects", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let rows = smelt_store::SessionReader::open_existing(&session_dir)
            .unwrap()
            .read_history_items_range(0..1)
            .unwrap();
        let protocol::HistoryItem::User { content, .. } = &rows[0] else {
            panic!("expected user history item");
        };
        assert!(matches!(
            content,
            protocol::Content::Parts(parts)
                if parts.iter().any(|part| matches!(
                    part,
                    protocol::ContentPart::ImageUrl { url, .. } if url == data_url
                ))
        ));
    }

    #[test]
    fn derived_refresh_failure_does_not_undo_canonical_commit() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("session-a");
        std::fs::create_dir_all(session_dir.join("meta.json")).unwrap();
        let request = PersistRequest {
            command: commit("session-a", 0),
        };
        let writer = smelt_store::OwnedSessionWriter::open(&session_dir, "session-a").unwrap();

        let success = write(&request, &writer, &session_dir).expect("canonical commit succeeds");

        assert_eq!(success.receipt.revision.get(), 1);
        assert!(success
            .warning
            .is_some_and(|warning| warning.contains("derived cache refresh failed")));
        assert!(session_dir.join("meta.json").is_dir());
        let db = smelt_store::SessionReader::open_existing(&session_dir).unwrap();
        assert_eq!(db.session_state().unwrap().unwrap().history_len, 1);
        assert_eq!(db.read_history_items_range(0..1).unwrap().len(), 1);
    }

    #[test]
    fn failed_first_commit_does_not_publish_session_directory() {
        let _serial = crate::app::test_harness::test_home_guard();
        let dir = tempfile::tempdir().unwrap();
        let _state_home = StateHomeGuard::install(dir.path());
        let session_dir = smelt_core::session::dir_for_id(SESSION_ID);
        let persister = Persister::spawn();

        persister.save(PersistRequest {
            command: commit(SESSION_ID, 9),
        });
        persister.flush();

        assert!(matches!(
            persister.drain_reports().as_slice(),
            [SessionBackendEvent::Failed(_)]
        ));
        assert!(!session_dir.exists());
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
        assert!(!first_dir.exists(), "uncommitted session must stay hidden");
        persister.save(PersistRequest {
            command: commit(SESSION_ID, 0),
        });
        persister.flush();
        assert!(matches!(
            persister.drain_reports().as_slice(),
            [SessionBackendEvent::Saved { .. }]
        ));
        assert!(smelt_store::OwnedSessionWriter::open(&first_dir, SESSION_ID).is_err());

        persister.open_owned(SECOND_SESSION_ID).unwrap();
        persister.save(PersistRequest {
            command: commit(SECOND_SESSION_ID, 0),
        });
        persister.flush();
        assert!(matches!(
            persister.drain_reports().as_slice(),
            [SessionBackendEvent::Saved { .. }]
        ));
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
        persister.save(PersistRequest {
            command: commit(SESSION_ID, 0),
        });
        persister.flush();
        assert!(matches!(
            persister.drain_reports().as_slice(),
            [SessionBackendEvent::Saved { .. }]
        ));
        persister.append_request_audit(PersistRequestAudit {
            session_id: SESSION_ID.into(),
            payload_mode: smelt_store::RequestAuditPayloadMode::SUMMARY,
            payload_capture_skipped_bytes: None,
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
