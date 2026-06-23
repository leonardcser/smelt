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
    pub(crate) session_id: String,
    pub(crate) session_dir: PathBuf,
    pub(crate) history_suffix: smelt_store::SessionHistorySuffix,
    pub(crate) blobs: Vec<Blob>,
    pub(crate) descriptor_start_idx: usize,
    pub(crate) descriptor_records: Vec<TranscriptBlockRecord>,
}

pub(crate) struct PersistMetadataRequest {
    pub(crate) session_id: String,
    pub(crate) session_dir: PathBuf,
    pub(crate) state: smelt_store::SessionState,
    pub(crate) side_tables: smelt_store::SessionSideTableSuffixes,
}

enum Cmd {
    Save(Box<PersistRequest>),
    SaveMetadata(Box<PersistMetadataRequest>),
    Flush(Sender<()>),
}

#[derive(Clone, Debug)]
pub(crate) struct PersistError {
    pub(crate) session_id: String,
    pub(crate) message: String,
}

pub(crate) struct Persister {
    tx: Option<Sender<Cmd>>,
    errors: Receiver<PersistError>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Persister {
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let (err_tx, errors) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("smelt-persist".into())
            .spawn(move || worker_loop(rx, err_tx))
            .expect("spawn persist worker");
        Self {
            tx: Some(tx),
            errors,
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

    pub(crate) fn drain_errors(&self) -> Vec<PersistError> {
        let mut errors = Vec::new();
        while let Ok(err) = self.errors.try_recv() {
            errors.push(err);
        }
        errors
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

fn worker_loop(rx: Receiver<Cmd>, errors: Sender<PersistError>) {
    let mut db_cache = PersistDbCache { current: None };
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Save(req) => {
                report_error(write(&req, &mut db_cache), &req.session_id, &errors);
            }
            Cmd::SaveMetadata(req) => {
                report_error(
                    write_metadata(&req, &mut db_cache),
                    &req.session_id,
                    &errors,
                );
            }
            Cmd::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn report_error(result: Result<(), String>, session_id: &str, errors: &Sender<PersistError>) {
    if let Err(message) = result {
        let _ = errors.send(PersistError {
            session_id: session_id.to_string(),
            message,
        });
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

fn write(req: &PersistRequest, db_cache: &mut PersistDbCache) -> Result<(), String> {
    let _perf = smelt_perf::perf::begin("persist:write");
    smelt_perf::perf::record_value(
        "persist:write:history_items",
        req.history_suffix.history.len() as u64,
    );
    smelt_perf::perf::record_value("persist:write:blobs", req.blobs.len() as u64);
    std::fs::create_dir_all(&req.session_dir)
        .map_err(|err| format!("create session directory: {err}"))?;
    let blob_dir = req.session_dir.join("blobs");
    let url_to_blob = write_blobs(&blob_dir, &req.blobs)?;
    let mut history_suffix = req.history_suffix.clone();
    if !url_to_blob.is_empty() {
        smelt_core::session::externalize_blobs(&mut history_suffix.history, &url_to_blob);
    }
    let db_path = req.session_dir.join("session.db");
    let db = db_cache
        .db(&db_path)
        .map_err(|err| format!("open session database: {err}"))?;
    let descriptor_rows = req
        .descriptor_records
        .iter()
        .enumerate()
        .map(|(offset, record)| {
            transcript_descriptor_row(req.descriptor_start_idx + offset, record)
        })
        .collect::<Result<Vec<_>, smelt_store::StoreError>>()
        .map_err(|err| format!("prepare transcript descriptors: {err}"))?;
    let save_report = db
        .save_history_suffix_and_transcript_descriptor_suffix_as_writer(
            &history_suffix,
            req.descriptor_start_idx,
            &descriptor_rows,
        )
        .map_err(|err| format!("save session database: {err}"))?;
    record_save_report(&save_report);
    smelt_perf::perf::record_value(
        "persist:write:descriptor_start_idx",
        req.descriptor_start_idx as u64,
    );
    smelt_perf::perf::record_value(
        "persist:write:descriptor_records",
        req.descriptor_records.len() as u64,
    );
    smelt_core::session::write_db_meta_sidecar(&req.session_dir)
        .map_err(|err| format!("write session metadata: {err}"))?;
    Ok(())
}

fn write_metadata(
    req: &PersistMetadataRequest,
    db_cache: &mut PersistDbCache,
) -> Result<(), String> {
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
    Ok(())
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
