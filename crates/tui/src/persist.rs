//! Background session persistence.
//!
//! Serialisation and disk I/O run on a worker thread. The main loop sends
//! a `PersistRequest`; the worker coalesces adjacent saves for the same
//! session id before writing. Call [`Persister::flush`] when the on-disk
//! state must be current (session load, fork, shutdown).

use smelt_core::session::{self, Session};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// One image blob to write alongside the session.
pub(crate) struct Blob {
    pub(crate) filename: String,
    pub(crate) data_url: String,
}

pub(crate) struct PersistRequest {
    pub(crate) session: Session,
    pub(crate) history_start_idx: usize,
    pub(crate) blobs: Vec<Blob>,
    pub(crate) display_cache: crate::content::display_cache::DisplayCacheData,
}

enum Cmd {
    Save(Box<PersistRequest>),
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

fn worker_loop(rx: Receiver<Cmd>, errors: Sender<PersistError>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Save(mut req) => {
                // Drain queued saves: keep only the latest for this session id.
                let mut others: Vec<Box<PersistRequest>> = Vec::new();
                while let Ok(next) = rx.try_recv() {
                    match next {
                        Cmd::Save(mut r) if r.session.id == req.session.id => {
                            r.history_start_idx = r.history_start_idx.min(req.history_start_idx);
                            req = r;
                        }
                        Cmd::Save(r) => others.push(r),
                        Cmd::Flush(done) => {
                            report_error(write(&req), &req.session.id, &errors);
                            for o in others.drain(..) {
                                report_error(write(&o), &o.session.id, &errors);
                            }
                            let _ = done.send(());
                            continue;
                        }
                    }
                }
                report_error(write(&req), &req.session.id, &errors);
                for o in others {
                    report_error(write(&o), &o.session.id, &errors);
                }
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

fn write(req: &PersistRequest) -> Result<(), String> {
    let _perf = smelt_perf::perf::begin("persist:write");
    smelt_perf::perf::record_value(
        "persist:write:history_items",
        req.session.history.len() as u64,
    );
    smelt_perf::perf::record_value(
        "persist:write:display_cache_row_indexes",
        req.display_cache.row_indexes.len() as u64,
    );
    smelt_perf::perf::record_value(
        "persist:write:display_cache_display_layouts",
        req.display_cache.display_layouts.len() as u64,
    );
    smelt_perf::perf::record_value("persist:write:blobs", req.blobs.len() as u64);
    let session_dir = session::dir_for(&req.session);
    std::fs::create_dir_all(&session_dir)
        .map_err(|err| format!("create session directory: {err}"))?;
    let blob_dir = session_dir.join("blobs");
    let url_to_blob = write_blobs(&blob_dir, &req.blobs)?;
    session::save_with_blobs_result_with_history_start(
        &req.session,
        &url_to_blob,
        req.history_start_idx,
    )
    .map_err(|err| format!("save session database: {err}"))?;
    crate::content::display_cache::write_for_session(&req.session, &req.display_cache);
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
