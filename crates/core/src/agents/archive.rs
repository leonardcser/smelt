//! Coalesced observation snapshots, using the existing canonical session store.
use crate::session::{Session, SessionStorage};
use std::sync::{Arc, Condvar, Mutex};

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct SavedAgent {
    pub info: protocol::AgentInfo,
    pub parent_session_id: String,
    pub session_id: String,
}

struct Snapshot {
    session: Session,
    saved: SavedAgent,
    revision: u64,
}
#[derive(Default)]
struct Pending {
    snapshot: Option<Snapshot>,
    submitted: u64,
    closed: bool,
}
pub(super) struct Archive {
    pending: Arc<(Mutex<Pending>, Condvar)>,
    completed: tokio::sync::watch::Receiver<(u64, Result<(), String>)>,
}

impl Archive {
    pub fn new(storage: SessionStorage) -> Self {
        let pending = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let worker = pending.clone();
        let (tx, completed) = tokio::sync::watch::channel((0, Ok(())));
        std::thread::Builder::new()
            .name("smelt-agent-archive".into())
            .spawn(move || loop {
                let snapshot = {
                    let (lock, wake) = &*worker;
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while state.snapshot.is_none() && !state.closed {
                        state = wake.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    let Some(snapshot) = state.snapshot.take() else {
                        break;
                    };
                    snapshot
                };
                let result = (|| -> Result<(), String> {
                    storage
                        .save_result(&snapshot.session)
                        .map_err(|error| error.to_string())?;
                    let path = storage
                        .artifact_dir_for_id(&snapshot.saved.session_id)
                        .join("agent-view.json");
                    let bytes =
                        serde_json::to_vec(&snapshot.saved).map_err(|error| error.to_string())?;
                    storage
                        .create_private_dir_all(path.parent().expect("agent artifact parent"))
                        .map_err(|error| error.to_string())?;
                    let temporary = path.with_extension("json.tmp");
                    storage
                        .write_private_file(&temporary, &bytes)
                        .map_err(|error| error.to_string())?;
                    std::fs::rename(temporary, path).map_err(|error| error.to_string())
                })();
                let _ = tx.send_replace((snapshot.revision, result));
                if snapshot.saved.info.status.is_terminal() {
                    let mut state = worker.0.lock().unwrap_or_else(|e| e.into_inner());
                    if state.snapshot.is_none() {
                        state.closed = true;
                        break;
                    }
                }
            })
            .expect("agent archive worker starts");
        Self { pending, completed }
    }

    pub fn update(&self, agent: &super::Agent) -> bool {
        let (lock, wake) = &*self.pending;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return false;
        }
        state.submitted += 1;
        state.snapshot = Some(Snapshot {
            session: agent.session.clone(),
            saved: SavedAgent {
                info: agent.info.clone(),
                parent_session_id: agent.parent_session_id.clone(),
                session_id: agent.session.id.clone(),
            },
            revision: state.submitted,
        });
        wake.notify_one();
        true
    }

    pub fn error(&self) -> Option<String> {
        self.completed.borrow().1.as_ref().err().cloned()
    }

    pub async fn flush(&self) -> Result<(), String> {
        let submitted = self
            .pending
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .submitted;
        let mut completed = self.completed.clone();
        let result = completed
            .wait_for(|(revision, _)| *revision >= submitted)
            .await
            .map_err(|_| "agent archive worker stopped".to_string())?
            .1
            .clone();
        result
    }
}

impl Drop for Archive {
    fn drop(&mut self) {
        let (lock, wake) = &*self.pending;
        lock.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        wake.notify_one();
    }
}

pub(super) fn load(
    storage: &SessionStorage,
    parent_id: &str,
) -> Result<Vec<(SavedAgent, Session)>, String> {
    if !storage.wait_for_session_catalog(std::time::Duration::from_secs(10)) {
        return Err("agent session catalog readiness timed out".into());
    }
    let mut cursor = None;
    let mut restored = Vec::new();
    loop {
        let page = storage
            .list_session_page_result(crate::session::SessionListQuery {
                limit: 512,
                cursor,
                ..Default::default()
            })
            .map_err(|error| error.to_string())?;
        if let Some(error) = page.catalog.last_error {
            return Err(error);
        }
        if page.catalog.state != crate::session::SessionCatalogState::Ready {
            return Err("agent session catalog is not ready; retry restoration".into());
        }
        for entry in page.entries {
            let crate::session::SessionListStatus::Available(meta) = entry.status else {
                continue;
            };
            if meta.parent_id.as_deref() != Some(parent_id) {
                continue;
            }
            let path = storage
                .artifact_dir_for_id(&meta.id)
                .join("agent-view.json");
            let bytes = match storage.read_private_file(&path, 1024 * 1024) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.to_string()),
            };
            let mut saved: SavedAgent =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            if saved.parent_session_id != parent_id || saved.session_id != meta.id {
                return Err("agent observation identity mismatch".into());
            }
            saved.info.capabilities = Default::default();
            if !saved.info.status.is_terminal() {
                saved.info.status = protocol::AgentStatus::Cancelled;
                saved.info.error =
                    Some("Agent execution was interrupted before its outcome was recorded".into());
            }
            if let Some(session) = storage
                .load_full_result(&meta.id)
                .map_err(|error| error.to_string())?
            {
                restored.push((saved, session));
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    Ok(restored)
}
