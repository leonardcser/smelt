//! Background session persistence.
//!
//! Serialisation and disk I/O run on a worker thread. The main loop sends
//! a `PersistRequest`; the worker writes requests in FIFO order. Call
//! [`Persister::flush`] when the on-disk state must be current (session load,
//! fork, shutdown).

use crate::content::transcript_search_text::descriptor_search_text;
use smelt_core::TranscriptBlockRecordWithId;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// One image blob to write alongside the session.
pub(crate) struct Blob {
    pub(crate) filename: String,
    pub(crate) data_url: String,
}

pub(crate) struct PersistRequest {
    pub(crate) save_id: u64,
    pub(crate) session_id: String,
    pub(crate) session_dir: PathBuf,
    pub(crate) delta: PersistDelta,
    pub(crate) blobs: Vec<Blob>,
}

#[derive(Clone)]
pub(crate) struct PersistDelta {
    pub(crate) history: smelt_store::SessionHistorySuffix,
    pub(crate) descriptors: Option<PersistDescriptorDelta>,
}

#[derive(Clone)]
pub(crate) struct PersistDescriptorDelta {
    pub(crate) start_descriptor_idx: usize,
    pub(crate) records: Vec<TranscriptBlockRecordWithId>,
}

pub(crate) struct PersistMetadataRequest {
    pub(crate) save_id: u64,
    pub(crate) session_id: String,
    pub(crate) session_dir: PathBuf,
    pub(crate) state: smelt_store::SessionState,
    pub(crate) side_tables: smelt_store::SessionSideTableSuffixes,
}

pub(crate) struct PersistRequestAudit {
    pub(crate) session_id: String,
    pub(crate) session_dir: PathBuf,
    pub(crate) entry: protocol::request_log::RequestLogEntry,
    pub(crate) payload_mode: smelt_store::RequestAuditPayloadMode,
}

#[derive(Clone, Debug)]
pub(crate) struct PersistRequestAuditFailure {
    pub(crate) session_id: String,
    pub(crate) message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistSaveKind {
    History,
    Metadata,
}

#[derive(Clone, Debug)]
pub(crate) struct PersistAck {
    pub(crate) save_id: u64,
    pub(crate) session_id: String,
    pub(crate) kind: PersistSaveKind,
    pub(crate) history_len: usize,
    pub(crate) revision: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct PersistFailure {
    pub(crate) save_id: u64,
    pub(crate) session_id: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug)]
pub(crate) enum PersistReport {
    Saved(PersistAck),
    Failed(PersistFailure),
    RequestAuditFailed(PersistRequestAuditFailure),
}

enum Cmd {
    Save(Box<PersistRequest>),
    SaveMetadata(Box<PersistMetadataRequest>),
    AppendRequestAudit(Box<PersistRequestAudit>),
    Flush(Sender<()>),
}

pub(crate) struct Persister {
    tx: Option<Sender<Cmd>>,
    reports: Receiver<PersistReport>,
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

    pub(crate) fn save(&self, req: PersistRequest) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Save(Box::new(req)));
        }
    }

    pub(crate) fn save_metadata(&self, req: PersistMetadataRequest) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::SaveMetadata(Box::new(req)));
        }
    }

    pub(crate) fn append_request_audit(&self, req: PersistRequestAudit) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::AppendRequestAudit(Box::new(req)));
        }
    }

    pub(crate) fn drain_reports(&self) -> Vec<PersistReport> {
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
        if tx.send(Cmd::Flush(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }
}

impl Drop for Persister {
    fn drop(&mut self) {
        self.flush();
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct PersistDbCache {
    current: Option<(PathBuf, smelt_store::SessionDb)>,
}

impl PersistDbCache {
    fn db(&mut self, path: &Path) -> Result<&smelt_store::SessionDb, smelt_store::StoreError> {
        if self
            .current
            .as_ref()
            .is_some_and(|(current_path, _)| current_path == path)
        {
            smelt_perf::perf::record_value("store:db:cached_read_write", 1);
        } else {
            self.current = Some((path.to_path_buf(), smelt_store::SessionDb::open(path)?));
        }
        Ok(&self.current.as_ref().expect("database cache populated").1)
    }
}

fn worker_loop(rx: Receiver<Cmd>, reports: Sender<PersistReport>) {
    let mut db_cache = PersistDbCache { current: None };
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Save(req) => {
                report_history_result(write(&req, &mut db_cache), &req, &reports);
            }
            Cmd::SaveMetadata(req) => {
                report_metadata_result(write_metadata(&req, &mut db_cache), &req, &reports);
            }
            Cmd::AppendRequestAudit(req) => {
                report_request_audit_result(
                    write_request_audit(&req, &mut db_cache),
                    &req,
                    &reports,
                );
            }
            Cmd::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn report_history_result(
    result: Result<smelt_store::SessionSaveReport, String>,
    req: &PersistRequest,
    reports: &Sender<PersistReport>,
) {
    let report = match result {
        Ok(save_report) => PersistReport::Saved(PersistAck {
            save_id: req.save_id,
            session_id: req.session_id.clone(),
            kind: PersistSaveKind::History,
            history_len: req.delta.history.history_len,
            revision: save_report.revision,
        }),
        Err(message) => PersistReport::Failed(PersistFailure {
            save_id: req.save_id,
            session_id: req.session_id.clone(),
            message,
        }),
    };
    let _ = reports.send(report);
}

fn report_metadata_result(
    result: Result<smelt_store::SessionSaveReport, String>,
    req: &PersistMetadataRequest,
    reports: &Sender<PersistReport>,
) {
    let report = match result {
        Ok(save_report) => PersistReport::Saved(PersistAck {
            save_id: req.save_id,
            session_id: req.session_id.clone(),
            kind: PersistSaveKind::Metadata,
            history_len: req.state.history_len as usize,
            revision: save_report.revision,
        }),
        Err(message) => PersistReport::Failed(PersistFailure {
            save_id: req.save_id,
            session_id: req.session_id.clone(),
            message,
        }),
    };
    let _ = reports.send(report);
}

fn report_request_audit_result(
    result: Result<i64, String>,
    req: &PersistRequestAudit,
    reports: &Sender<PersistReport>,
) {
    if let Err(message) = result {
        let _ = reports.send(PersistReport::RequestAuditFailed(
            PersistRequestAuditFailure {
                session_id: req.session_id.clone(),
                message,
            },
        ));
    }
}

fn record_save_report(save_report: &smelt_store::SessionSaveReport) {
    smelt_perf::perf::record_value("persist:write:history_deleted", save_report.history_deleted);
    smelt_perf::perf::record_value(
        "persist:write:history_inserted",
        save_report.history_inserted,
    );
    smelt_perf::perf::record_value(
        "persist:write:history_unchanged",
        save_report.history_unchanged,
    );
}

fn write(
    req: &PersistRequest,
    db_cache: &mut PersistDbCache,
) -> Result<smelt_store::SessionSaveReport, String> {
    let _perf = smelt_perf::perf::begin("persist:write");
    smelt_perf::perf::record_value(
        "persist:write:history_items",
        req.delta.history.history.len() as u64,
    );
    smelt_perf::perf::record_value("persist:write:blobs", req.blobs.len() as u64);
    std::fs::create_dir_all(&req.session_dir)
        .map_err(|err| format!("create session directory: {err}"))?;
    let blob_dir = req.session_dir.join("blobs");
    let url_to_blob = write_blobs(&blob_dir, &req.blobs)?;
    let mut delta = req.delta.clone();
    if !url_to_blob.is_empty() {
        smelt_core::session::externalize_blobs(&mut delta.history.history, &url_to_blob);
    }
    let db_path = req.session_dir.join("session.db");
    let db = db_cache
        .db(&db_path)
        .map_err(|err| format!("open session database: {err}"))?;
    let descriptor_delta = delta
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
                start_descriptor_idx: descriptors.start_descriptor_idx,
                records,
            })
        })
        .transpose()?;
    let store_delta = smelt_store::SessionDelta {
        history: delta.history,
        descriptors: descriptor_delta,
    };
    let save_report = db
        .apply_session_delta_as_writer(&store_delta)
        .map_err(|err| format!("save session database: {err}"))?;
    record_save_report(&save_report);
    let (descriptor_start_idx, descriptor_records) = store_delta
        .descriptors
        .as_ref()
        .map(|descriptors| {
            (
                descriptors.start_descriptor_idx as u64,
                descriptors.records.len() as u64,
            )
        })
        .unwrap_or((0, 0));
    smelt_perf::perf::record_value("persist:write:descriptor_start_idx", descriptor_start_idx);
    smelt_perf::perf::record_value("persist:write:descriptor_records", descriptor_records);
    smelt_core::session::write_db_meta_sidecar(&req.session_dir)
        .map_err(|err| format!("write session metadata: {err}"))?;
    Ok(save_report)
}

fn write_metadata(
    req: &PersistMetadataRequest,
    db_cache: &mut PersistDbCache,
) -> Result<smelt_store::SessionSaveReport, String> {
    let _perf = smelt_perf::perf::begin("persist:write_metadata");
    smelt_perf::perf::record_value("persist:write:history_items", 0);
    smelt_perf::perf::record_value("persist:write:blobs", 0);
    smelt_perf::perf::record_value("persist:write:descriptor_records", 0);
    smelt_perf::perf::record_value("persist:write:metadata_only", 1);
    std::fs::create_dir_all(&req.session_dir)
        .map_err(|err| format!("create session directory: {err}"))?;
    let db_path = req.session_dir.join("session.db");
    let db = db_cache
        .db(&db_path)
        .map_err(|err| format!("open session database: {err}"))?;
    let save_report = db
        .save_session_state_and_side_table_suffixes_as_writer(&req.state, &req.side_tables)
        .map_err(|err| format!("save session metadata: {err}"))?;
    record_save_report(&save_report);
    smelt_core::session::write_db_meta_sidecar(&req.session_dir)
        .map_err(|err| format!("write session metadata: {err}"))?;
    Ok(save_report)
}

fn write_request_audit(
    req: &PersistRequestAudit,
    db_cache: &mut PersistDbCache,
) -> Result<i64, String> {
    let _perf = smelt_perf::perf::begin("persist:request_audit");
    std::fs::create_dir_all(&req.session_dir)
        .map_err(|err| format!("create session directory: {err}"))?;
    let db_path = req.session_dir.join("session.db");
    let db = db_cache
        .db(&db_path)
        .map_err(|err| format!("open session database: {err}"))?;
    db.append_request_attempt(&req.entry, req.payload_mode)
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
    std::fs::create_dir_all(blob_dir).map_err(|err| format!("create blob directory: {err}"))?;
    for b in blobs {
        let path: PathBuf = blob_dir.join(&b.filename);
        if !path.exists() {
            std::fs::write(&path, b.data_url.as_bytes())
                .map_err(|err| format!("write blob {}: {err}", b.filename))?;
        }
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
    let db = smelt_store::SessionDb::open(session_dir.join("session.db"))?;
    let rows = records
        .iter()
        .enumerate()
        .map(|(offset, record)| {
            let descriptor_idx = start_descriptor_idx + offset;
            let record = TranscriptBlockRecordWithId {
                block_id: smelt_core::BlockId::new(descriptor_idx as u64),
                record: record.clone(),
            };
            let search_text = descriptor_search_text(
                &record.record.descriptor,
                record.record.tool_state.as_ref().map(|(_, state)| state),
            );
            smelt_core::transcript_model::transcript_descriptor_row_with_block_idx(
                descriptor_idx,
                record.block_id.get(),
                &record.record,
                search_text,
            )
        })
        .collect::<Result<Vec<_>, smelt_store::StoreError>>()?;
    db.replace_transcript_descriptor_suffix(start_descriptor_idx, &rows)
}

fn transcript_descriptor_row(
    descriptor_idx: usize,
    record: &TranscriptBlockRecordWithId,
    history: &smelt_store::SessionHistorySuffix,
) -> Result<smelt_store::TranscriptDescriptorRecord, smelt_store::StoreError> {
    let search_text = descriptor_search_text(
        &record.record.descriptor,
        record.record.tool_state.as_ref().map(|(_, state)| state),
    );
    let owned_record;
    let record_ref = match record.record.origin {
        Some(smelt_core::BlockOrigin::History(idx))
            if !history_suffix_contains_matching_descriptor_origin(history, idx, record) =>
        {
            owned_record = smelt_core::TranscriptBlockRecord {
                origin: None,
                ..record.record.clone()
            };
            &owned_record
        }
        _ => &record.record,
    };
    smelt_core::transcript_model::transcript_descriptor_row_with_block_idx(
        descriptor_idx,
        record.block_id.get(),
        record_ref,
        search_text,
    )
}

fn history_suffix_contains_matching_descriptor_origin(
    history: &smelt_store::SessionHistorySuffix,
    history_idx: usize,
    record: &TranscriptBlockRecordWithId,
) -> bool {
    if history_idx >= history.history_len {
        return false;
    }
    if history_idx < history.history_start_idx {
        return true;
    }
    history
        .history
        .get(history_idx - history.history_start_idx)
        .is_some_and(|item| descriptor_origin_matches_history_item(&record.record.descriptor, item))
}

fn descriptor_origin_matches_history_item(
    descriptor: &smelt_core::TranscriptBlockDescriptor,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn suffix(
        history_start_idx: usize,
        history_len: usize,
        history: Vec<protocol::HistoryItem>,
    ) -> smelt_store::SessionHistorySuffix {
        smelt_store::SessionHistorySuffix {
            state: smelt_store::SessionState {
                id: "test".into(),
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
                history_len: history_len as u64,
                created_at: 0,
                updated_at: 0,
            },
            history_start_idx,
            history_len,
            history,
            side_tables: None,
        }
    }

    #[test]
    fn transcript_descriptor_row_preserves_sparse_block_id() {
        let record = TranscriptBlockRecordWithId {
            block_id: smelt_core::BlockId::new(302),
            record: smelt_core::TranscriptBlockRecord {
                descriptor: smelt_core::TranscriptBlockDescriptor::User {
                    text: "follow up".to_string(),
                    image_labels: Vec::new(),
                },
                content_hash: 0,
                origin: Some(smelt_core::BlockOrigin::History(11)),
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
        let record = TranscriptBlockRecordWithId {
            block_id: smelt_core::BlockId::new(303),
            record: smelt_core::TranscriptBlockRecord {
                descriptor: smelt_core::TranscriptBlockDescriptor::User {
                    text: "follow up".to_string(),
                    image_labels: Vec::new(),
                },
                content_hash: 0,
                origin: Some(smelt_core::BlockOrigin::History(12)),
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
        let record = TranscriptBlockRecordWithId {
            block_id: smelt_core::BlockId::new(304),
            record: smelt_core::TranscriptBlockRecord {
                descriptor: smelt_core::TranscriptBlockDescriptor::User {
                    text: "follow up".to_string(),
                    image_labels: Vec::new(),
                },
                content_hash: 0,
                origin: Some(smelt_core::BlockOrigin::History(3)),
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
    fn request_audit_is_written_by_worker() {
        let dir = tempfile::tempdir().unwrap();
        let persister = Persister::spawn();
        persister.append_request_audit(PersistRequestAudit {
            session_id: "session-a".into(),
            session_dir: dir.path().to_path_buf(),
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

        let db = smelt_store::SessionDb::open_read_only(dir.path().join("session.db")).unwrap();
        let attempts = db
            .query_request_attempts(&smelt_store::RequestAuditQuery::default())
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].request_id.as_deref(), Some("42"));
    }
}
