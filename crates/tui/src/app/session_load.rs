use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

type SessionOperation = Box<dyn FnOnce(SessionOperationToken) + Send>;

pub(super) struct SessionOperationToken {
    latest: Arc<AtomicU64>,
    pub(super) generation: u64,
}

impl SessionOperationToken {
    pub(super) fn cancelled(&self) -> bool {
        self.latest.load(Ordering::Acquire) != self.generation
    }
}

#[derive(Default)]
struct SessionOperationInbox {
    pending: Option<(SessionOperationToken, SessionOperation)>,
    stopped: bool,
}

/// Two workers and one replaceable waiting request. A new operation can pass one
/// stalled read; supersession invalidates running work between reads. Dropping never
/// joins a storage worker on UI.
#[derive(Default)]
pub(super) struct SessionOperationRuntime {
    latest: Arc<AtomicU64>,
    inbox: Option<Arc<(Mutex<SessionOperationInbox>, Condvar)>>,
}

impl SessionOperationRuntime {
    pub(super) fn request(
        &mut self,
        operation: impl FnOnce(SessionOperationToken) + Send + 'static,
    ) -> Result<u64, String> {
        if self.inbox.is_none() {
            let inbox = Arc::new((Mutex::new(SessionOperationInbox::default()), Condvar::new()));
            for index in 0..2 {
                let worker = Arc::clone(&inbox);
                if let Err(error) = thread::Builder::new()
                    .name(format!("smelt-session-operation-{index}"))
                    .spawn(move || loop {
                        let (lock, ready) = &*worker;
                        let mut inbox = lock.lock().unwrap_or_else(|error| error.into_inner());
                        while inbox.pending.is_none() && !inbox.stopped {
                            inbox = ready.wait(inbox).unwrap_or_else(|error| error.into_inner());
                        }
                        if inbox.stopped {
                            return;
                        }
                        let (token, operation) =
                            inbox.pending.take().expect("pending session operation");
                        drop(inbox);
                        if !token.cancelled() {
                            operation(token);
                        }
                    })
                {
                    inbox
                        .0
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .stopped = true;
                    inbox.1.notify_all();
                    return Err(format!("failed to start session operation: {error}"));
                }
            }
            self.inbox = Some(inbox);
        }
        let generation = self.latest.fetch_add(1, Ordering::AcqRel) + 1;
        let token = SessionOperationToken {
            latest: Arc::clone(&self.latest),
            generation,
        };
        let (lock, ready) = &**self.inbox.as_ref().expect("session worker started");
        lock.lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending = Some((token, Box::new(operation)));
        ready.notify_one();
        Ok(generation)
    }

    pub(super) fn cancel(&self) {
        self.latest.fetch_add(1, Ordering::AcqRel);
        if let Some(inbox) = &self.inbox {
            inbox
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending = None;
        }
    }

    pub(super) fn is_current(&self, generation: u64) -> bool {
        self.latest.load(Ordering::Acquire) == generation
    }
}

impl Drop for SessionOperationRuntime {
    fn drop(&mut self) {
        self.cancel();
        if let Some(inbox) = &self.inbox {
            inbox
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .stopped = true;
            inbox.1.notify_all();
        }
    }
}

pub(super) struct PreparedSessionLoad {
    header: smelt_core::session::SessionHeader,
    user_turn_count: usize,
    session: smelt_core::session::Session,
    store_address: smelt_core::session::SessionStoreAddress,
    head: smelt_store::StoreHead,
    transcript: crate::app::transcript::LoadedTranscript,
    layout: (u16, u16),
}

impl PreparedSessionLoad {
    fn refresh(
        self: Box<Self>,
        sessions: &smelt_core::session::SessionStorage,
        width: u16,
        target_rows: u16,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, String> {
        if cancelled() {
            return Err("session load cancelled".into());
        }
        if self.layout == (width, target_rows) {
            let store = &self.store_address;
            let reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
                &store.sessions_root,
                &store.lineage_id,
                &store.session_id,
            )
            .map_err(|error| format!("open resume destination: {error}"))?;
            let head = reader
                .store_head()
                .map_err(|error| format!("refresh resume destination: {error}"))?;
            if head == self.head {
                smelt_perf::perf::record_value("session:resume:reused", 1);
                return Ok(*self);
            }
        }
        prepare_session_load(sessions, &self.session.id, width, target_rows, cancelled)
    }

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
            self.user_turn_count,
        )
        .into_store_backed();
        (document, warnings)
    }
}

pub(super) struct SessionForkRequest {
    pub(super) source_id: String,
    pub(super) forked: smelt_core::session::Session,
    pub(super) expected_source: Option<smelt_store::StoreHead>,
    pub(super) preserved_projection: Option<crate::app::session_document::ProjectedSessionSave>,
}

struct SessionLoadRequest {
    sessions: smelt_core::session::SessionStorage,
    id: String,
    width: u16,
    target_rows: u16,
    fork: Option<SessionForkRequest>,
    prepared: Option<Box<PreparedSessionLoad>>,
}

pub struct SessionLoadWorkerResult {
    generation: u64,
    id: String,
    outcome: Result<PreparedSessionLoad, String>,
    forked_from: Option<String>,
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

struct SessionAdoption {
    generation: u64,
    source_generation: super::session_document::PersistenceGeneration,
    busy: super::BusyToken,
    prepared: Option<Box<PreparedSessionLoad>>,
    forked_from: Option<String>,
    stage: SessionAdoptionStage,
}

enum SessionAdoptionStage {
    Settling {
        deadline: std::time::Instant,
    },
    Closing {
        close: Option<Box<crate::persist::PendingPersistenceClose>>,
    },
    Reloading,
}

pub(super) struct SessionLoadRuntime {
    operations: SessionOperationRuntime,
    pending: std::cell::Cell<bool>,
    adoption: Option<SessionAdoption>,
    ready: Option<SessionLoadWorkerResult>,
    #[cfg(test)]
    delay: std::time::Duration,
}

impl Default for SessionLoadRuntime {
    fn default() -> Self {
        Self {
            operations: SessionOperationRuntime::default(),
            pending: std::cell::Cell::new(false),
            adoption: None,
            ready: None,
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
    ) -> Result<u64, String> {
        #[cfg(test)]
        let delay = self.delay;
        self.operations
            .request(move |token| {
                #[cfg(test)]
                thread::sleep(delay);
                let generation = token.generation;
                let cancelled = || token.cancelled();
                if cancelled() {
                    return;
                }
                let forked_from = request.fork.as_ref().map(|fork| fork.source_id.clone());
                let outcome = request
                    .fork
                    .map_or(Ok(()), |fork| {
                        create_session_fork(&request.sessions, fork, &cancelled)
                    })
                    .and_then(|()| match request.prepared {
                        Some(prepared) => prepared.refresh(
                            &request.sessions,
                            request.width,
                            request.target_rows,
                            &cancelled,
                        ),
                        None => prepare_session_load(
                            &request.sessions,
                            &request.id,
                            request.width,
                            request.target_rows,
                            &cancelled,
                        ),
                    });
                if cancelled() {
                    return;
                }
                let _ = event_tx.send(super::AppEvent::SessionLoadCompleted(Box::new(
                    SessionLoadWorkerResult {
                        generation,
                        id: request.id,
                        outcome,
                        forked_from,
                    },
                )));
            })
            .inspect(|_| self.pending.set(true))
    }

    fn cancel(&self) {
        self.operations.cancel();
        self.pending.set(false);
    }

    pub(super) fn is_pending(&self) -> bool {
        self.pending.get() || self.adoption.is_some() || self.ready.is_some()
    }

    pub(super) fn has_handoff(&self) -> bool {
        self.adoption.is_some()
    }

    fn is_current(&self, result: &SessionLoadWorkerResult) -> bool {
        self.operations.is_current(result.generation)
    }

    #[cfg(test)]
    pub(super) fn set_delay(&mut self, delay: std::time::Duration) {
        self.delay = delay;
    }
}

fn create_session_fork(
    sessions: &smelt_core::session::SessionStorage,
    mut request: SessionForkRequest,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    if cancelled() {
        return Err("session fork cancelled".into());
    }
    let (mut destination, imported) = smelt_store::OwnedLineageWriter::fork_from(
        sessions.sessions_dir(),
        &request.source_id,
        &request.forked.id,
        request.forked.created_at_ms,
        request.expected_source,
        cancelled,
    )
    .map_err(|error| format!("failed to fork session store: {error}"))?;
    let outcome = (|| {
        if !request.forked.bind_fork_archives(&imported) {
            return Err("copied fork archives do not match the captured source".to_string());
        }
        let expected = imported.session.receipt.current;
        let frame = if let Some(projection) = request.preserved_projection {
            projection.prepare_archives(&request.forked, expected)
        } else {
            request.forked.prepare_archive_save(
                expected,
                smelt_store::HistorySuffix {
                    start: smelt_store::HistoryIndex::new(expected.history_len.get()),
                    final_len: expected.history_len,
                    items: Vec::new(),
                },
                None,
            )
        }
        .map_err(|error| format!("{error:?}"))?;
        let result = destination
            .commit_compact_session(frame.command())
            .map_err(|error| format!("{error:?}"))?;
        sessions
            .publish_archive_save_catalog(&destination, &frame, &result)
            .map_err(|error| format!("failed to publish fork session: {error}"))
    })();
    if let Err(error) = outcome {
        let cleanup = destination.delete_branch(smelt_core::session::now_ms());
        return Err(match cleanup {
            Ok(()) => format!("failed to preserve fork state: {error}"),
            Err(cleanup) => format!("failed to preserve fork state: {error}; failed to remove incomplete fork: {cleanup}"),
        });
    }
    destination
        .release()
        .map_err(|error| format!("failed to release fork writer: {error}"))
}

pub(super) fn prepare_session_load(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
    width: u16,
    target_rows: u16,
    cancelled: &dyn Fn() -> bool,
) -> Result<PreparedSessionLoad, String> {
    let _perf = smelt_perf::perf::begin("session:resume:prepare");
    let resume = match sessions.load_store_resume_cancellable(id, width, target_rows, cancelled) {
        Ok(Some(resume)) => resume,
        Ok(None) => return Err(format!("session {id:?} has no stored state")),
        Err(error) => return Err(format!("failed to load session: {error}")),
    };
    if cancelled() {
        return Err("session load cancelled".into());
    }
    let smelt_core::session::SessionStoreResume {
        header,
        user_turn_count,
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
        user_turn_count,
        session,
        store_address,
        head,
        transcript,
        layout: (width, target_rows),
    })
}

pub(super) struct RecoveredSession {
    prepared: PreparedSessionLoad,
    writer: crate::persist::SessionWriterHandoff,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OwnershipRecoveryEvent {
    Waiting,
    Ready,
    Stopped,
}

#[derive(Default)]
pub(super) struct OwnershipRecovery {
    operations: SessionOperationRuntime,
    result: Option<std::sync::mpsc::Receiver<Result<RecoveredSession, String>>>,
    #[cfg(test)]
    observer: Option<std::sync::mpsc::Receiver<OwnershipRecoveryEvent>>,
}

impl OwnershipRecovery {
    pub(super) fn request(
        &mut self,
        sessions: smelt_core::session::SessionStorage,
        id: String,
        width: u16,
        target_rows: u16,
        events: tokio::sync::mpsc::UnboundedSender<super::AppEvent>,
    ) -> Result<(), String> {
        self.cancel();
        let (send, receive) = std::sync::mpsc::channel();
        #[cfg(test)]
        let (observe, observer) = std::sync::mpsc::channel();
        self.operations.request(move |token| {
            let cancelled = || token.cancelled();
            let outcome = (|| {
                while !cancelled() {
                    match smelt_store::SessionWriter::open_existing(sessions.sessions_dir(), &id) {
                        Ok(mut writer) => {
                            let prepared = prepare_session_load(
                                &sessions,
                                &id,
                                width,
                                target_rows,
                                &cancelled,
                            )?;
                            // The snapshot already includes interrupted-turn recovery.
                            if writer.take_startup_recovery().is_some() {
                                sessions.request_session_catalog_repair(
                                    &id,
                                    prepared.head.revision.get(),
                                );
                            }
                            return Ok((prepared, writer));
                        }
                        Err(smelt_store::StoreError::OwnershipConflict { .. }) => {
                            #[cfg(test)]
                            let _ = observe.send(OwnershipRecoveryEvent::Waiting);
                            thread::sleep(std::time::Duration::from_millis(250));
                        }
                        Err(error) => return Err(format!("recover session writer: {error}")),
                    }
                }
                Err("session ownership recovery cancelled".into())
            })();
            let (outcome, handoff) = match outcome {
                Ok((prepared, writer)) => {
                    let (transfer, receive) = crate::persist::SessionWriterHandoff::channel();
                    (
                        Ok(RecoveredSession {
                            prepared,
                            writer: receive,
                        }),
                        Some((transfer, writer)),
                    )
                }
                Err(error) => (Err(error), None),
            };
            let delivered = if cancelled() {
                drop(outcome);
                false
            } else {
                send.send(outcome).is_ok()
            };
            if delivered {
                #[cfg(test)]
                let _ = observe.send(OwnershipRecoveryEvent::Ready);
                let _ = events.send(super::AppEvent::OwnershipRecoveryReady);
            }
            if let Some((transfer, writer)) = handoff {
                let _ = transfer.send(writer);
            }
            #[cfg(test)]
            let _ = observe.send(OwnershipRecoveryEvent::Stopped);
        })?;
        self.result = Some(receive);
        #[cfg(test)]
        {
            self.observer = Some(observer);
        }
        Ok(())
    }

    pub(super) fn cancel(&mut self) {
        self.operations.cancel();
        self.result = None;
    }

    pub(super) fn take_result(&mut self) -> Option<Result<RecoveredSession, String>> {
        let result = match self.result.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("session ownership recovery stopped unexpectedly".into())
            }
        };
        self.result = None;
        Some(result)
    }

    #[cfg(test)]
    pub(super) fn take_observer(&mut self) -> std::sync::mpsc::Receiver<OwnershipRecoveryEvent> {
        self.observer.take().expect("ownership recovery observer")
    }
}

impl super::TuiApp {
    pub(super) fn poll_ownership_recovery(&mut self) {
        if self.session_load.is_pending() {
            return;
        }
        let Some(outcome) = self.conversation.take_ownership_recovery() else {
            return;
        };
        match outcome {
            Ok(recovered) => {
                let (document, warnings) = recovered.prepared.finish();
                if !self.load_store_backed_session_with_writer(document, Some(recovered.writer)) {
                    self.conversation
                        .mark_read_only("failed to restore session writer".into());
                    return;
                }
                self.finish_loaded_transcript(&warnings);
            }
            Err(error) => {
                self.conversation.mark_read_only(error.clone());
                self.notify_session_error_sticky(error);
            }
        }
    }

    pub(crate) fn request_session_load(&mut self, id: &str) {
        let target_rows = super::transcript::record_tail_target_rows(self.last_height);
        let request = SessionLoadRequest {
            sessions: self.core.sessions.clone(),
            id: id.to_string(),
            width: self.last_width,
            target_rows,
            fork: None,
            prepared: None,
        };
        match self
            .session_load
            .request(self.platform.app_event_sender(), request)
        {
            Ok(_) => self.notify(format!("loading session {id}...")),
            Err(error) => {
                self.notify_operation_error_sticky(super::NotificationOperation::SessionLoad, error)
            }
        }
    }

    pub(super) fn request_session_fork(&mut self, fork: SessionForkRequest) {
        let request = SessionLoadRequest {
            sessions: self.core.sessions.clone(),
            id: fork.forked.id.clone(),
            width: self.last_width,
            target_rows: super::transcript::record_tail_target_rows(self.last_height),
            fork: Some(fork),
            prepared: None,
        };
        match self
            .session_load
            .request(self.platform.app_event_sender(), request)
        {
            Ok(_) => self.notify("forking session...".into()),
            Err(error) => self.notify_session_error_sticky(error),
        }
    }

    pub(super) fn cancel_pending_session_load(&self) {
        self.session_load.cancel();
    }

    pub(crate) fn cancel_session_handoff(&self) -> bool {
        if !self.session_load.has_handoff() {
            return false;
        }
        self.cancel_pending_session_load();
        true
    }

    pub(super) fn install_prepared_session_load(&mut self, prepared: PreparedSessionLoad) -> bool {
        let (document, degraded_warnings) = prepared.finish();
        if !self.load_store_backed_session(document) {
            return false;
        }
        self.finish_loaded_transcript(&degraded_warnings);
        true
    }

    fn finish_loaded_transcript(&mut self, warnings: &[String]) {
        if !warnings.is_empty() {
            self.notify_session_error_sticky(format!(
                "session loaded with unavailable attachments: {}",
                warnings.join("; ")
            ));
        }
        self.finish_transcript_turn();
        self.transcript_win_mut().follow_tail();
    }

    pub(super) fn handle_session_load_worker_result(&mut self, result: SessionLoadWorkerResult) {
        if !self.session_load.is_current(&result) {
            return;
        }
        if let Some(adoption) = self.session_load.adoption.as_ref() {
            if adoption.generation == result.generation
                && matches!(adoption.stage, SessionAdoptionStage::Reloading)
            {
                let adoption = self.session_load.adoption.take().expect("session adoption");
                adoption.busy.release();
                self.session_load.pending.set(false);
                if adoption.source_generation != self.conversation.persistence_generation() {
                    self.restore_source_writer();
                    self.notify_session_load_error(
                        "session changed while loading; resume cancelled".into(),
                    );
                    return;
                }
                let forked_from = adoption.forked_from;
                match result.outcome {
                    Ok(prepared) => {
                        self.cancel_pending_rewind();
                        self.cancel_live_search(false);
                        self.lua.cancel_tasks();
                        self.finish_session_replacement();
                        let (document, warnings) = prepared.finish();
                        self.adopt_store_backed_session(document, None);
                        self.finish_loaded_transcript(&warnings);
                        if let Some(source_id) = forked_from {
                            self.publish_history_delta(super::history::HistoryDeltaKind::Forked);
                            self.notify(format!("forked from {source_id}"));
                        }
                    }
                    Err(error) => {
                        self.restore_source_writer();
                        self.notify_session_load_error(error);
                    }
                }
            } else {
                self.session_load.ready = Some(result);
            }
            return;
        }
        let prepared = match result.outcome {
            Ok(prepared) => Box::new(prepared),
            Err(error) => {
                self.session_load.pending.set(false);
                self.notify_session_load_error(error);
                return;
            }
        };
        self.quiesce_session_bound_work();
        if self.overlays.notification_is_sticky() == Some(false) {
            self.dismiss_notification();
        }
        self.session_load.adoption = Some(SessionAdoption {
            generation: result.generation,
            source_generation: self.conversation.persistence_generation(),
            busy: self.busy_stack.push_token("loading session".into()),
            prepared: Some(prepared),
            forked_from: result.forked_from,
            stage: SessionAdoptionStage::Settling {
                deadline: std::time::Instant::now()
                    + crate::persist::INTERACTIVE_PERSISTENCE_DEADLINE,
            },
        });
        self.poll_session_adoption();
    }

    fn restore_source_writer(&mut self) {
        if !self.conversation.has_persistence() && !self.conversation.is_read_only() {
            self.claim_writer_access_for_current_session();
        }
    }

    fn notify_session_load_error(&mut self, error: String) {
        self.notify_operation_error_sticky(super::NotificationOperation::SessionLoad, error);
    }

    pub(super) fn poll_session_adoption(&mut self) {
        let Some(mut adoption) = self.session_load.adoption.take() else {
            return;
        };
        let current = self.session_load.operations.is_current(adoption.generation);
        let changed = !matches!(adoption.stage, SessionAdoptionStage::Settling { .. })
            && adoption.source_generation != self.conversation.persistence_generation();
        let cancelled = !current || changed;
        let outcome = match &mut adoption.stage {
            SessionAdoptionStage::Settling { deadline } => {
                if cancelled {
                    Ok(None)
                } else if self.conversation.canonical_operations_are_pending()
                    || !self
                        .conversation
                        .submissions
                        .settling_cancellations
                        .is_empty()
                {
                    if std::time::Instant::now() >= *deadline {
                        Err("session cancellation commands did not settle before the close deadline".into())
                    } else {
                        self.session_load.adoption = Some(adoption);
                        return;
                    }
                } else {
                    self.save_session();
                    match self.conversation.begin_persistence_close() {
                        Ok(close) => {
                            adoption.source_generation = self.conversation.persistence_generation();
                            adoption.stage = SessionAdoptionStage::Closing {
                                close: close.map(Box::new),
                            };
                            self.session_load.adoption = Some(adoption);
                            return;
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            SessionAdoptionStage::Closing { close, .. } => {
                if let Some(close) = close {
                    match self.conversation.poll_persistence_close(close, cancelled) {
                        Some(outcome) => outcome,
                        None => {
                            self.session_load.adoption = Some(adoption);
                            return;
                        }
                    }
                } else {
                    Ok(None)
                }
            }
            SessionAdoptionStage::Reloading => {
                if !cancelled {
                    self.session_load.adoption = Some(adoption);
                    return;
                }
                Ok(None)
            }
        };
        if let Ok(Some(warning)) = outcome.as_ref() {
            self.notify_warn(warning.clone());
        }
        if cancelled || outcome.is_err() {
            adoption.busy.release();
            self.restore_source_writer();
            if current {
                self.session_load.cancel();
                self.notify_session_load_error(if changed {
                    "session changed while loading; resume cancelled".into()
                } else {
                    outcome.expect_err("failed close")
                });
            }
            if let Some(result) = self.session_load.ready.take() {
                self.handle_session_load_worker_result(result);
            }
            return;
        }
        let SessionAdoptionStage::Closing { .. } = adoption.stage else {
            unreachable!("only a closing session starts refresh")
        };
        // The old writer's final save may advance the destination. Revalidate the
        // captured head after physical release before reusing its immutable data.
        let prepared = adoption
            .prepared
            .take()
            .expect("prepared resume destination");
        let request = SessionLoadRequest {
            sessions: self.core.sessions.clone(),
            id: prepared.session.id.clone(),
            width: self.last_width,
            target_rows: super::transcript::record_tail_target_rows(self.last_height),
            fork: None,
            prepared: Some(prepared),
        };
        match self
            .session_load
            .request(self.platform.app_event_sender(), request)
        {
            Ok(generation) => {
                adoption.generation = generation;
                adoption.stage = SessionAdoptionStage::Reloading;
                self.session_load.adoption = Some(adoption);
            }
            Err(error) => {
                adoption.busy.release();
                self.session_load.pending.set(false);
                self.restore_source_writer();
                self.notify_session_load_error(error);
            }
        }
    }
}

#[cfg(test)]
mod operation_tests {
    use super::*;

    #[test]
    fn superseded_session_operations_keep_a_bounded_pool_and_one_waiting_slot() {
        let mut runtime = SessionOperationRuntime::default();
        let (done, results) = std::sync::mpsc::channel();
        let mut releases = Vec::new();
        for index in 0..2 {
            let (started, start) = std::sync::mpsc::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let done = done.clone();
            runtime
                .request(move |token| {
                    started.send(()).unwrap();
                    wait.recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    done.send((index, token.cancelled())).unwrap();
                })
                .unwrap();
            start
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            releases.push(release);
        }
        let superseded_done = done.clone();
        runtime
            .request(move |token| {
                superseded_done.send((2, token.cancelled())).unwrap();
            })
            .unwrap();
        runtime
            .request(move |token| {
                done.send((3, token.cancelled())).unwrap();
            })
            .unwrap();
        for release in releases {
            release.send(()).unwrap();
        }
        let mut completed = (0..3)
            .map(|_| {
                results
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        completed.sort();
        assert_eq!(completed, vec![(0, true), (1, true), (3, false)]);
        assert!(results.try_recv().is_err());
    }
}
