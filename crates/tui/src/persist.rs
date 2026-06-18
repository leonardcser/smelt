//! Background session persistence.
//!
//! Serialisation and disk I/O run on a worker thread. The main loop sends
//! a `PersistRequest`; the worker coalesces adjacent saves for the same
//! session id before writing. Call [`Persister::flush`] when the on-disk
//! state must be current (session load, fork, shutdown).

use crate::content::transcript_search_text::descriptor_search_text;
use smelt_core::session::{self, Session};
use smelt_core::{BlockOrigin, TranscriptBlockRecord};
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
    pub(crate) descriptor_start_idx: usize,
    pub(crate) descriptor_records: Vec<TranscriptBlockRecord>,
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
                // Drain queued saves and fold same-session suffix requests into the newest session snapshot.
                let mut others: Vec<Box<PersistRequest>> = Vec::new();
                while let Ok(next) = rx.try_recv() {
                    match next {
                        Cmd::Save(r) if r.session.id == req.session.id => {
                            req = match merge_same_session_requests(req, r) {
                                Ok(merged) => merged,
                                Err((previous, next)) => {
                                    report_error(write(&previous), &previous.session.id, &errors);
                                    next
                                }
                            };
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

fn merge_same_session_requests(
    mut older: Box<PersistRequest>,
    mut newer: Box<PersistRequest>,
) -> Result<Box<PersistRequest>, (Box<PersistRequest>, Box<PersistRequest>)> {
    debug_assert_eq!(older.session.id, newer.session.id);
    newer.history_start_idx = newer.history_start_idx.min(older.history_start_idx);

    if newer.descriptor_start_idx <= older.descriptor_start_idx {
        older.blobs.append(&mut newer.blobs);
        newer.blobs = older.blobs;
        return Ok(newer);
    }

    let keep = newer.descriptor_start_idx - older.descriptor_start_idx;
    if older.descriptor_records.len() < keep {
        return Err((older, newer));
    }
    older.descriptor_records.truncate(keep);
    older
        .descriptor_records
        .append(&mut newer.descriptor_records);
    older.blobs.append(&mut newer.blobs);
    newer.blobs = older.blobs;
    newer.descriptor_start_idx = older.descriptor_start_idx;
    newer.descriptor_records = older.descriptor_records;
    Ok(newer)
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
    write_transcript_descriptor_suffix(
        &session_dir,
        req.descriptor_start_idx,
        &req.descriptor_records,
    )
    .map_err(|err| format!("save transcript descriptors: {err}"))?;
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

#[cfg(test)]
pub(crate) fn write_transcript_descriptors(
    session_dir: &std::path::Path,
    records: &[TranscriptBlockRecord],
) -> Result<(), smelt_store::StoreError> {
    write_transcript_descriptor_suffix(session_dir, 0, records)
}

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
    let descriptor_json = serde_json::to_string(&record.descriptor)?;
    let origin_json = record
        .origin
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let tool_state_json = record
        .tool_state
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let search_text = descriptor_search_text(
        &record.descriptor,
        record.tool_state.as_ref().map(|(_, state)| state),
    );
    let history_idx = match record.origin {
        Some(BlockOrigin::History(idx)) => Some(idx as u64),
        _ => None,
    };
    Ok(smelt_store::TranscriptDescriptorRecord {
        block_idx: descriptor_idx as u64,
        history_idx,
        kind: record.descriptor.kind().to_string(),
        tool_call_id: record.descriptor.tool_call_id().map(str::to_string),
        tool_name: record.descriptor.tool_name().map(str::to_string),
        content_hash: record.content_hash.to_string(),
        estimated_text_bytes: search_text.len() as u64,
        preview_text: preview(&search_text, 512),
        search_text,
        descriptor_json,
        origin_json,
        tool_state_json,
    })
}

fn preview(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = 0;
    for (idx, _) in text.char_indices() {
        if idx > max_bytes {
            break;
        }
        end = idx;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::TranscriptBlockDescriptor;

    fn session(id: &str, title: &str) -> Session {
        let mut session = Session::new(0, std::path::PathBuf::from("/tmp"));
        session.id = id.to_string();
        session.title = Some(title.to_string());
        session
    }

    fn record(content: &str) -> TranscriptBlockRecord {
        TranscriptBlockRecord {
            descriptor: TranscriptBlockDescriptor::Text {
                content: content.to_string(),
            },
            content_hash: content.len() as u64,
            origin: None,
            tool_state: None,
        }
    }

    fn blob(filename: &str) -> Blob {
        Blob {
            filename: filename.to_string(),
            data_url: format!("data:{filename}"),
        }
    }

    fn request(
        title: &str,
        history_start_idx: usize,
        descriptor_start_idx: usize,
        descriptor_records: &[&str],
        blobs: &[&str],
    ) -> Box<PersistRequest> {
        Box::new(PersistRequest {
            session: session("session-1", title),
            history_start_idx,
            blobs: blobs.iter().map(|filename| blob(filename)).collect(),
            descriptor_start_idx,
            descriptor_records: descriptor_records
                .iter()
                .map(|content| record(content))
                .collect(),
        })
    }

    fn record_texts(req: &PersistRequest) -> Vec<String> {
        req.descriptor_records
            .iter()
            .filter_map(|record| match &record.descriptor {
                TranscriptBlockDescriptor::Text { content } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    fn blob_filenames(req: &PersistRequest) -> Vec<String> {
        req.blobs.iter().map(|blob| blob.filename.clone()).collect()
    }

    #[test]
    fn merge_request_newer_earlier_suffix_wins_with_history_min_and_blobs() {
        let older = request("older", 8, 5, &["old-tail"], &["old.png"]);
        let newer = request("newer", 3, 2, &["new-a", "new-b"], &["new.png"]);

        let merged = match merge_same_session_requests(older, newer) {
            Ok(merged) => merged,
            Err(_) => panic!("expected merge request"),
        };

        assert_eq!(merged.session.title.as_deref(), Some("newer"));
        assert_eq!(merged.history_start_idx, 3);
        assert_eq!(merged.descriptor_start_idx, 2);
        assert_eq!(record_texts(&merged), ["new-a", "new-b"]);
        assert_eq!(blob_filenames(&merged), ["old.png", "new.png"]);
    }

    #[test]
    fn merge_request_older_suffix_bridges_to_newer_suffix() {
        let older = request("older", 2, 4, &["keep-a", "keep-b", "drop-c"], &["old.png"]);
        let newer = request("newer", 9, 6, &["new-c", "new-d"], &["new.png"]);

        let merged = match merge_same_session_requests(older, newer) {
            Ok(merged) => merged,
            Err(_) => panic!("expected merge request"),
        };

        assert_eq!(merged.session.title.as_deref(), Some("newer"));
        assert_eq!(merged.history_start_idx, 2);
        assert_eq!(merged.descriptor_start_idx, 4);
        assert_eq!(
            record_texts(&merged),
            ["keep-a", "keep-b", "new-c", "new-d"]
        );
        assert_eq!(blob_filenames(&merged), ["old.png", "new.png"]);
    }

    #[test]
    fn merge_request_returns_both_requests_when_suffix_gap_cannot_be_bridged() {
        let older = request("older", 2, 4, &["keep-a"], &["old.png"]);
        let newer = request("newer", 9, 6, &["new-c"], &["new.png"]);

        let Err((older, newer)) = merge_same_session_requests(older, newer) else {
            panic!("expected unmergeable suffix gap");
        };

        assert_eq!(older.session.title.as_deref(), Some("older"));
        assert_eq!(older.history_start_idx, 2);
        assert_eq!(older.descriptor_start_idx, 4);
        assert_eq!(record_texts(&older), ["keep-a"]);
        assert_eq!(blob_filenames(&older), ["old.png"]);

        assert_eq!(newer.session.title.as_deref(), Some("newer"));
        assert_eq!(newer.history_start_idx, 2);
        assert_eq!(newer.descriptor_start_idx, 6);
        assert_eq!(record_texts(&newer), ["new-c"]);
        assert_eq!(blob_filenames(&newer), ["new.png"]);
    }
}
