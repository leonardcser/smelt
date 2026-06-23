//! Background session persistence.
//!
//! Serialisation and disk I/O run on a worker thread. The main loop sends
//! a `PersistRequest`; the worker writes requests in FIFO order. Call
//! [`Persister::flush`] when the on-disk state must be current (session load,
//! fork, shutdown).

use crate::content::transcript_search_text::descriptor_search_text;
use smelt_core::TranscriptBlockRecord;
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
    pub(crate) records: Vec<TranscriptBlockRecord>,
}

pub(crate) struct PersistMetadataRequest {
    pub(crate) save_id: u64,
    pub(crate) session_id: String,
    pub(crate) session_dir: PathBuf,
    pub(crate) state: smelt_store::SessionState,
    pub(crate) side_tables: smelt_store::SessionSideTableSuffixes,
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
}

enum Cmd {
    Save(Box<PersistRequest>),
    SaveMetadata(Box<PersistMetadataRequest>),
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
        Ok(_save_report) => PersistReport::Saved(PersistAck {
            save_id: req.save_id,
            session_id: req.session_id.clone(),
            kind: PersistSaveKind::History,
            history_len: req.delta.history.history_len,
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
        Ok(_save_report) => PersistReport::Saved(PersistAck {
            save_id: req.save_id,
            session_id: req.session_id.clone(),
            kind: PersistSaveKind::Metadata,
            history_len: req.state.history_len as usize,
        }),
        Err(message) => PersistReport::Failed(PersistFailure {
            save_id: req.save_id,
            session_id: req.session_id.clone(),
            message,
        }),
    };
    let _ = reports.send(report);
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
                    transcript_descriptor_row(descriptors.start_descriptor_idx + offset, record)
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
    records: &[TranscriptBlockRecord],
) -> Result<(), smelt_store::StoreError> {
    let db = smelt_store::SessionDb::open(session_dir.join("session.db"))?;
    let rows = records
        .iter()
        .enumerate()
        .map(|(offset, record)| transcript_descriptor_row(start_descriptor_idx + offset, record))
        .collect::<Result<Vec<_>, smelt_store::StoreError>>()?;
    db.replace_transcript_descriptor_suffix(start_descriptor_idx, &rows)
}

fn transcript_descriptor_row(
    descriptor_idx: usize,
    record: &TranscriptBlockRecord,
) -> Result<smelt_store::TranscriptDescriptorRecord, smelt_store::StoreError> {
    let search_text = descriptor_search_text(
        &record.descriptor,
        record.tool_state.as_ref().map(|(_, state)| state),
    );
    smelt_core::transcript_model::transcript_descriptor_row(descriptor_idx, record, search_text)
}
