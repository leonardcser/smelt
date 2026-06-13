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
    pub(crate) blobs: Vec<Blob>,
    pub(crate) display_cache: Vec<crate::content::display_block::DisplayCacheEntry>,
}

enum Cmd {
    Save(Box<PersistRequest>),
    Flush(Sender<()>),
}

pub(crate) struct Persister {
    tx: Option<Sender<Cmd>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Persister {
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("smelt-persist".into())
            .spawn(move || worker_loop(rx))
            .expect("spawn persist worker");
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    pub(crate) fn save(&self, req: PersistRequest) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Save(Box::new(req)));
        }
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

fn worker_loop(rx: Receiver<Cmd>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Save(mut req) => {
                // Drain queued saves: keep only the latest for this session id.
                let mut others: Vec<Box<PersistRequest>> = Vec::new();
                while let Ok(next) = rx.try_recv() {
                    match next {
                        Cmd::Save(r) if r.session.id == req.session.id => req = r,
                        Cmd::Save(r) => others.push(r),
                        Cmd::Flush(done) => {
                            write(&req);
                            for o in others.drain(..) {
                                write(&o);
                            }
                            let _ = done.send(());
                            continue;
                        }
                    }
                }
                write(&req);
                for o in others {
                    write(&o);
                }
            }
            Cmd::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn write(req: &PersistRequest) {
    let _perf = smelt_perf::perf::begin("persist:write");
    smelt_perf::perf::record_value(
        "persist:write:history_items",
        req.session.history.len() as u64,
    );
    smelt_perf::perf::record_value(
        "persist:write:display_cache_entries",
        req.display_cache.len() as u64,
    );
    smelt_perf::perf::record_value("persist:write:blobs", req.blobs.len() as u64);
    let session_dir = session::dir_for(&req.session);
    let _ = std::fs::create_dir_all(&session_dir);
    let blob_dir = session_dir.join("blobs");
    let url_to_blob = write_blobs(&blob_dir, &req.blobs);
    session::save_with_blobs(&req.session, &url_to_blob);
    crate::content::display_cache::write_for_session(&req.session, &req.display_cache);
}

fn write_blobs(
    blob_dir: &std::path::Path,
    blobs: &[Blob],
) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;
    let mut url_to_blob = HashMap::new();
    if blobs.is_empty() {
        return url_to_blob;
    }
    let _ = std::fs::create_dir_all(blob_dir);
    for b in blobs {
        let path: PathBuf = blob_dir.join(&b.filename);
        if !path.exists() {
            let _ = std::fs::write(&path, b.data_url.as_bytes());
        }
        url_to_blob.insert(b.data_url.clone(), format!("blob:{}", b.filename));
    }
    url_to_blob
}
