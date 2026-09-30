//! Coalesced child-session persistence. Disk work never runs on the frontend.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use super::AgentInfo;
use crate::session::{Session, SessionStorage};

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct SavedAgent {
    pub info: AgentInfo,
    pub inherited_history_len: usize,
}

struct Snapshot {
    session: Session,
    saved: SavedAgent,
    first_changed: usize,
}

#[derive(Default)]
struct Pending {
    snapshot: Option<Snapshot>,
    audits: VecDeque<(
        Box<protocol::request_log::RequestLogEntry>,
        smelt_store::RequestAuditPayloadMode,
    )>,
    closed: bool,
    audit_overflow: bool,
    audit_bytes: usize,
}

const MAX_AUDIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;

struct AuditSize(usize);

impl std::io::Write for AuditSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_AUDIT_BYTES {
            return Err(std::io::Error::other(
                "subagent audit payload exceeds limit",
            ));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_saved(storage: &SessionStorage, saved: &SavedAgent) -> Result<(), String> {
    let path = storage
        .artifact_dir_for_id(&saved.info.session_id)
        .join("agent.json");
    storage
        .create_private_dir_all(path.parent().expect("agent artifact parent"))
        .map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(saved).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_METADATA_BYTES {
        return Err("subagent metadata exceeds its size limit".into());
    }
    storage
        .write_private_file(&temporary, &bytes)
        .map_err(|error| error.to_string())?;
    std::fs::rename(temporary, path).map_err(|error| error.to_string())
}

pub(super) struct AgentArchive {
    pending: Arc<(Mutex<Pending>, Condvar)>,
}

impl AgentArchive {
    pub fn new(
        storage: SessionStorage,
        completion: tokio::sync::watch::Sender<Option<AgentInfo>>,
    ) -> Self {
        let pending = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let worker = Arc::clone(&pending);
        tokio::task::spawn_blocking(move || {
            let mut writer: Option<smelt_store::SessionWriter> = None;
            let mut history_len = 0;
            let mut failure: Option<String> = None;
            loop {
                let (snapshot, audits, closed, overflow) = {
                    let (lock, wake) = &*worker;
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while state.snapshot.is_none() && state.audits.is_empty() && !state.closed {
                        state = wake.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    state.audit_bytes = 0;
                    (
                        state.snapshot.take(),
                        std::mem::take(&mut state.audits),
                        state.closed,
                        std::mem::take(&mut state.audit_overflow),
                    )
                };
                if overflow {
                    failure = Some("subagent request audit queue exceeded its limit".into());
                }
                let mut terminal = None;
                if let Some(snapshot) = snapshot {
                    let result = (|| -> Result<(), String> {
                        if writer.is_none() {
                            storage
                                .create_private_dir_all(&storage.sessions_dir())
                                .map_err(|e| e.to_string())?;
                            writer = Some(
                                smelt_store::SessionWriter::open(
                                    storage.sessions_dir(),
                                    snapshot.session.id.clone(),
                                )
                                .map_err(|e| e.to_string())?,
                            );
                        }
                        let writer = writer.as_mut().expect("archive writer is open");
                        let expected = writer.store_head().map_err(|e| e.to_string())?;
                        let command = crate::session::store_commit_from_session(
                            &snapshot.session,
                            expected,
                            snapshot.first_changed.min(history_len),
                        )
                        .map_err(|e| e.to_string())?;
                        let receipt = writer
                            .commit_session(&command)
                            .map_err(|e| format!("{e:?}"))?;
                        history_len = snapshot.session.history.len();
                        storage.publish_session_catalog_commit(&command, &receipt);
                        if matches!(snapshot.saved.info.status.as_str(), "queued" | "running") {
                            write_saved(&storage, &snapshot.saved)?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        failure = Some(error);
                    }
                    if !matches!(snapshot.saved.info.status.as_str(), "queued" | "running") {
                        terminal = Some(snapshot.saved);
                    }
                }
                for (entry, mode) in audits {
                    if let Some(writer) = writer.as_mut() {
                        if let Err(error) = writer.append_request_attempt(&entry, mode) {
                            failure = Some(error.to_string());
                        }
                    }
                }
                if let Some(mut saved) = terminal {
                    saved.info.persistence_error = failure.clone();
                    // Release ownership before a follow-up or a resumed frontend opens the session.
                    if let Some(writer) = writer.take() {
                        if let Err(error) = writer.release() {
                            saved.info.persistence_error = Some(error.to_string());
                        }
                    }
                    if let Err(error) = write_saved(&storage, &saved) {
                        saved.info.persistence_error = Some(error);
                    }
                    completion.send_replace(Some(saved.info));
                }
                if closed {
                    break;
                }
            }
        });
        Self { pending }
    }

    pub fn update(
        &self,
        session: &Session,
        info: &AgentInfo,
        inherited_history_len: usize,
        first_changed: usize,
    ) {
        let mut snapshot = Snapshot {
            session: session.clone(),
            saved: SavedAgent {
                info: info.clone(),
                inherited_history_len,
            },
            first_changed,
        };
        let (lock, wake) = &*self.pending;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = &state.snapshot {
            snapshot.first_changed = old.first_changed.min(first_changed);
        }
        state.snapshot = Some(snapshot);
        wake.notify_one();
    }

    pub fn audit(
        &self,
        entry: Box<protocol::request_log::RequestLogEntry>,
        mode: smelt_store::RequestAuditPayloadMode,
    ) {
        let mut size = AuditSize(0);
        let oversized = serde_json::to_writer(&mut size, &entry).is_err();
        let (lock, wake) = &*self.pending;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.audits.len() == 64
            || oversized
            || size.0 > MAX_AUDIT_BYTES.saturating_sub(state.audit_bytes)
        {
            state.audit_overflow = true;
        } else {
            state.audit_bytes += size.0;
            state.audits.push_back((entry, mode));
        }
        wake.notify_one();
    }
}

impl Drop for AgentArchive {
    fn drop(&mut self) {
        let (lock, wake) = &*self.pending;
        lock.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        wake.notify_one();
    }
}

pub(super) fn load(
    storage: &SessionStorage,
    parent_id: &str,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<SavedAgent>, String> {
    if cancelled() {
        return Err("subagent restoration cancelled".into());
    }
    storage.wait_for_catalog_ready(std::time::Duration::from_secs(10), &cancelled)?;
    let mut cursor = None;
    let mut sessions = Vec::new();
    loop {
        if cancelled() {
            return Err("subagent restoration cancelled".into());
        }
        let page = storage
            .list_session_page_result(crate::session::SessionListQuery {
                limit: 512,
                cursor: cursor.clone(),
                ..Default::default()
            })
            .map_err(|error| error.to_string())?;
        if let Some(error) = page.catalog.last_error {
            return Err(format!("subagent session catalog: {error}"));
        }
        if page.catalog.state != crate::session::SessionCatalogState::Ready {
            return Err(
                "subagent session catalog changed during restoration; retry when ready".into(),
            );
        }
        sessions.extend(
            page.entries
                .into_iter()
                .filter_map(|entry| match entry.status {
                    crate::session::SessionListStatus::Available(meta)
                        if meta.parent_id.as_deref() == Some(parent_id) =>
                    {
                        Some(meta)
                    }
                    _ => None,
                }),
        );
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let mut saved = Vec::new();
    for session in sessions {
        if cancelled() {
            return Err("subagent restoration cancelled".into());
        }
        let path = storage.artifact_dir_for_id(&session.id).join("agent.json");
        match storage.read_private_file(&path, MAX_METADATA_BYTES) {
            Ok(bytes) => {
                let mut record: SavedAgent = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                if record.info.parent_id != parent_id || record.info.session_id != session.id {
                    return Err("subagent archive identity mismatch".into());
                }
                if matches!(record.info.status.as_str(), "queued" | "running") {
                    record.info.status = "cancelled".into();
                    record.info.error = Some("smelt exited before the subagent completed".into());
                }
                saved.push(record);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    saved.sort_by_key(|saved| saved.info.id);
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_restoration_cancellation_avoids_catalog_work() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().join("smelt"));
        assert_eq!(
            load(&storage, "parent", || true).err().unwrap(),
            "subagent restoration cancelled"
        );
        assert!(!storage.sessions_dir().exists());
    }

    #[test]
    fn subagent_metadata_reads_are_bounded() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().join("smelt"));
        let path = storage
            .artifact_dir_for_id(&"a".repeat(64))
            .join("agent.json");
        storage
            .create_private_dir_all(path.parent().unwrap())
            .unwrap();
        storage.write_private_file(&path, b"12345").unwrap();
        assert_eq!(storage.read_private_file(&path, 5).unwrap(), b"12345");
        assert!(storage.read_private_file(&path, 4).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn subagent_metadata_reads_reject_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().join("smelt"));
        let path = storage
            .artifact_dir_for_id(&"a".repeat(64))
            .join("agent.json");
        storage
            .create_private_dir_all(path.parent().unwrap())
            .unwrap();
        let target = root.path().join("outside.json");
        std::fs::write(&target, b"not agent metadata").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(
            storage
                .read_private_file(&path, MAX_METADATA_BYTES)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
