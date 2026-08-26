use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

pub(super) struct PreparedSessionLoad {
    header: smelt_core::session::SessionHeader,
    session: smelt_core::session::Session,
    store_address: smelt_core::session::SessionStoreAddress,
    head: smelt_store::StoreHead,
    transcript: crate::app::transcript::LoadedTranscript,
}

impl PreparedSessionLoad {
    fn finish(
        self,
    ) -> (
        crate::app::session_document::StoreBackedSessionDocument,
        Vec<String>,
    ) {
        let warnings = self.header.degraded_warnings.clone();
        let document = crate::app::session_document::SessionDocument::from_store(
            self.header,
            self.session,
            self.store_address,
            self.head,
            self.transcript,
        )
        .into_store_backed();
        (document, warnings)
    }
}

struct SessionLoadRequest {
    sessions: smelt_core::session::SessionStorage,
    id: String,
    width: u16,
    target_rows: u16,
}

pub struct SessionLoadWorkerResult {
    generation: u64,
    id: String,
    outcome: Result<PreparedSessionLoad, String>,
}

impl std::fmt::Debug for SessionLoadWorkerResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionLoadWorkerResult")
            .field("generation", &self.generation)
            .field("id", &self.id)
            .field("outcome", &self.outcome.as_ref().map(|_| "loaded"))
            .finish()
    }
}

pub(super) struct SessionLoadRuntime {
    latest_generation: Arc<AtomicU64>,
    #[cfg(test)]
    delay: std::time::Duration,
}

impl Default for SessionLoadRuntime {
    fn default() -> Self {
        Self {
            latest_generation: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            delay: std::time::Duration::ZERO,
        }
    }
}

impl SessionLoadRuntime {
    fn request(
        &mut self,
        event_tx: tokio::sync::mpsc::UnboundedSender<super::AppEvent>,
        request: SessionLoadRequest,
    ) -> Result<(), String> {
        let generation = self.latest_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let latest_generation = Arc::clone(&self.latest_generation);
        #[cfg(test)]
        let delay = self.delay;
        thread::Builder::new()
            .name(format!("smelt-session-load-{generation}"))
            .spawn(move || {
                #[cfg(test)]
                thread::sleep(delay);
                let cancelled = || latest_generation.load(Ordering::Acquire) != generation;
                if cancelled() {
                    return;
                }
                let outcome = prepare_session_load(
                    &request.sessions,
                    &request.id,
                    request.width,
                    request.target_rows,
                    &cancelled,
                );
                if cancelled() {
                    return;
                }
                let _ = event_tx.send(super::AppEvent::SessionLoadCompleted(Box::new(
                    SessionLoadWorkerResult {
                        generation,
                        id: request.id,
                        outcome,
                    },
                )));
            })
            .map(|_| ())
            .map_err(|error| format!("failed to start session load: {error}"))
    }

    fn cancel(&self) {
        self.latest_generation.fetch_add(1, Ordering::AcqRel);
    }

    fn is_current(&self, result: &SessionLoadWorkerResult) -> bool {
        self.latest_generation.load(Ordering::Acquire) == result.generation
    }

    #[cfg(test)]
    pub(super) fn set_delay(&mut self, delay: std::time::Duration) {
        self.delay = delay;
    }
}

impl Drop for SessionLoadRuntime {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(super) fn prepare_session_load(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
    width: u16,
    target_rows: u16,
    cancelled: &dyn Fn() -> bool,
) -> Result<PreparedSessionLoad, String> {
    let resume = match sessions.load_store_resume_result(id, width, target_rows) {
        Ok(Some(resume)) => resume,
        Ok(None) => return Err(format!("session {id:?} has no stored state")),
        Err(error) => return Err(format!("failed to load session: {error}")),
    };
    if cancelled() {
        return Err("session load cancelled".into());
    }
    let smelt_core::session::SessionStoreResume {
        header,
        session,
        store_address,
        head,
        transcript_record_tail,
    } = resume;
    let address = crate::app::transcript::TranscriptStoreAddress::new(
        store_address.sessions_root.clone(),
        store_address.session_id.clone(),
        store_address.lineage_id.clone(),
    );
    let transcript = crate::app::transcript::LoadedTranscript::from_record_slice(
        transcript_record_tail,
        address,
    )
    .ok_or_else(|| format!("session {id:?} has no stored transcript records"))?;
    Ok(PreparedSessionLoad {
        header,
        session,
        store_address,
        head,
        transcript,
    })
}

impl super::TuiApp {
    pub(crate) fn request_session_load(&mut self, id: &str) {
        let target_rows = super::transcript::record_tail_target_rows(self.last_height);
        let request = SessionLoadRequest {
            sessions: self.core.sessions.clone(),
            id: id.to_string(),
            width: self.last_width,
            target_rows,
        };
        match self
            .session_load
            .request(self.platform.app_event_sender(), request)
        {
            Ok(()) => self.notify(format!("loading session {id}...")),
            Err(error) => {
                self.notify_operation_error_sticky(super::NotificationOperation::SessionLoad, error)
            }
        }
    }

    pub(super) fn cancel_pending_session_load(&self) {
        self.session_load.cancel();
    }

    pub(super) fn install_prepared_session_load(&mut self, prepared: PreparedSessionLoad) -> bool {
        let (document, degraded_warnings) = prepared.finish();
        if !self.load_store_backed_session(document) {
            return false;
        }
        if !degraded_warnings.is_empty() {
            self.notify_session_error_sticky(format!(
                "session loaded with unavailable attachments: {}",
                degraded_warnings.join("; ")
            ));
        }
        self.finish_transcript_turn();
        self.transcript_win_mut().follow_tail();
        true
    }

    pub(super) fn handle_session_load_worker_result(&mut self, result: SessionLoadWorkerResult) {
        if !self.session_load.is_current(&result) {
            return;
        }
        match result.outcome {
            Ok(prepared) => {
                self.install_prepared_session_load(prepared);
            }
            Err(error) => {
                self.notify_operation_error_sticky(
                    super::NotificationOperation::SessionLoad,
                    error,
                );
            }
        }
    }
}
