use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

use super::transcript::{LoadedTranscript, TranscriptDocument, TranscriptStoreAddress};
use super::transcript_hydration::{TranscriptHydrationRequest, TranscriptHydrationWorkerResult};

#[derive(Clone)]
pub(crate) struct SessionPreviewRender {
    pub(crate) id: String,
    pub(crate) cache_key: String,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) scroll_top: Option<u64>,
    pub(crate) buffer: crate::smelt_edit::BufId,
    pub(crate) window: Option<crate::smelt_edit::WinId>,
}

pub(crate) enum SessionPreviewRenderOutcome {
    Ready(crate::smelt_edit::MaterializedRows),
    Pending,
    Unavailable(super::transcript::TranscriptProjectionHydrationError),
}

#[derive(Clone)]
struct ActiveSessionPreview {
    generation: u64,
    render: SessionPreviewRender,
}

enum SessionPreviewWorkerOperation {
    Load {
        sessions: smelt_core::session::SessionStorage,
        id: String,
        width: u16,
        height: u16,
    },
    Hydrate(TranscriptHydrationRequest),
}

struct SessionPreviewWorkerRequest {
    worker_generation: u64,
    preview_generation: u64,
    operation: SessionPreviewWorkerOperation,
}

pub(crate) enum SessionPreviewWorkerOutcome {
    Loaded(Option<Box<LoadedTranscript>>),
    Hydrated(Box<TranscriptHydrationWorkerResult>),
}

pub struct SessionPreviewWorkerResult {
    worker_generation: u64,
    preview_generation: u64,
    outcome: SessionPreviewWorkerOutcome,
}

impl std::fmt::Debug for SessionPreviewWorkerResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let outcome = match self.outcome {
            SessionPreviewWorkerOutcome::Loaded(Some(_)) => "loaded",
            SessionPreviewWorkerOutcome::Loaded(None) => "missing",
            SessionPreviewWorkerOutcome::Hydrated(_) => "hydrated",
        };
        f.debug_struct("SessionPreviewWorkerResult")
            .field("worker_generation", &self.worker_generation)
            .field("preview_generation", &self.preview_generation)
            .field("outcome", &outcome)
            .finish()
    }
}

#[derive(Default)]
struct SessionPreviewWorkerState {
    pending: Option<SessionPreviewWorkerRequest>,
    shutdown: bool,
}

struct SessionPreviewWorkerShared {
    state: Mutex<SessionPreviewWorkerState>,
    changed: Condvar,
    latest_generation: AtomicU64,
    #[cfg(test)]
    delay_ms: AtomicU64,
    #[cfg(test)]
    open_count: AtomicU64,
}

struct SessionPreviewWorker {
    shared: Arc<SessionPreviewWorkerShared>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SessionPreviewWorker {
    fn spawn(event_tx: tokio::sync::mpsc::UnboundedSender<super::AppEvent>) -> Self {
        let shared = Arc::new(SessionPreviewWorkerShared {
            state: Mutex::new(SessionPreviewWorkerState::default()),
            changed: Condvar::new(),
            latest_generation: AtomicU64::new(0),
            #[cfg(test)]
            delay_ms: AtomicU64::new(0),
            #[cfg(test)]
            open_count: AtomicU64::new(0),
        });
        let worker_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("smelt-session-preview".into())
            .spawn(move || session_preview_worker_loop(worker_shared, event_tx))
            .expect("failed to spawn session preview worker");
        Self {
            shared,
            thread: Some(thread),
        }
    }

    fn request(&self, mut request: SessionPreviewWorkerRequest) {
        let generation = self.shared.latest_generation.fetch_add(1, Ordering::AcqRel) + 1;
        request.worker_generation = generation;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending = Some(request);
        self.shared.changed.notify_one();
    }

    fn cancel(&self) {
        self.shared.latest_generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending = None;
        self.shared.changed.notify_one();
    }

    fn is_current(&self, result: &SessionPreviewWorkerResult) -> bool {
        self.shared.latest_generation.load(Ordering::Acquire) == result.worker_generation
    }

    #[cfg(test)]
    fn set_delay(&self, delay: std::time::Duration) {
        self.shared.delay_ms.store(
            u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
    }

    #[cfg(test)]
    fn open_count(&self) -> u64 {
        self.shared.open_count.load(Ordering::Acquire)
    }
}

impl Drop for SessionPreviewWorker {
    fn drop(&mut self) {
        self.shared.latest_generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown = true;
        state.pending = None;
        self.shared.changed.notify_one();
        drop(state);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
pub(super) struct SessionPreviewRuntime {
    worker: Option<SessionPreviewWorker>,
    active: Option<ActiveSessionPreview>,
    next_generation: u64,
}

impl SessionPreviewRuntime {
    fn begin(&mut self, render: SessionPreviewRender) -> ActiveSessionPreview {
        if let Some(worker) = self.worker.as_ref() {
            worker.cancel();
        }
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("session preview generation overflow");
        let active = ActiveSessionPreview {
            generation: self.next_generation,
            render,
        };
        self.active = Some(active.clone());
        active
    }

    fn request(
        &mut self,
        event_tx: tokio::sync::mpsc::UnboundedSender<super::AppEvent>,
        preview_generation: u64,
        operation: SessionPreviewWorkerOperation,
    ) {
        let worker = self
            .worker
            .get_or_insert_with(|| SessionPreviewWorker::spawn(event_tx));
        worker.request(SessionPreviewWorkerRequest {
            worker_generation: 0,
            preview_generation,
            operation,
        });
    }

    fn finish(&mut self, generation: u64) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            self.active = None;
        }
    }

    #[cfg(any(test, feature = "harness"))]
    pub(super) fn is_pending(&self) -> bool {
        self.active.is_some()
    }
}

fn session_preview_worker_loop(
    shared: Arc<SessionPreviewWorkerShared>,
    event_tx: tokio::sync::mpsc::UnboundedSender<super::AppEvent>,
) {
    let mut retained_reader: Option<(TranscriptStoreAddress, smelt_store::LineageSessionReader)> =
        None;
    loop {
        let request = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.pending.is_none() && !state.shutdown {
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
            if state.shutdown {
                return;
            }
            state
                .pending
                .take()
                .expect("pending session preview request")
        };
        let cancelled =
            || shared.latest_generation.load(Ordering::Acquire) != request.worker_generation;
        #[cfg(test)]
        {
            let mut remaining = shared.delay_ms.load(Ordering::Acquire);
            while remaining > 0 {
                if cancelled() {
                    break;
                }
                let sleep_ms = remaining.min(5);
                thread::sleep(std::time::Duration::from_millis(sleep_ms));
                remaining -= sleep_ms;
            }
        }
        if cancelled() {
            continue;
        }

        #[cfg(test)]
        let retained_address_before = retained_reader.as_ref().map(|(address, _)| address.clone());
        let outcome = match request.operation {
            SessionPreviewWorkerOperation::Load {
                sessions,
                id,
                width,
                height,
            } => SessionPreviewWorkerOutcome::Loaded(
                load_session_preview(
                    &sessions,
                    &id,
                    width,
                    height,
                    &mut retained_reader,
                    &cancelled,
                )
                .map(Box::new),
            ),
            SessionPreviewWorkerOperation::Hydrate(hydration) => {
                let started_at = Instant::now();
                let needs_open = retained_reader
                    .as_ref()
                    .is_none_or(|(address, _)| address != &hydration.store_address);
                if needs_open {
                    retained_reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
                        &hydration.store_address.sessions_root,
                        &hydration.store_address.lineage_id,
                        &hydration.store_address.session_id,
                    )
                    .ok()
                    .map(|reader| (hydration.store_address.clone(), reader));
                }
                let hydrated = retained_reader
                    .as_ref()
                    .filter(|(address, _)| address == &hydration.store_address)
                    .and_then(|(_, reader)| {
                        super::transcript_hydration::execute_hydration_request(
                            reader,
                            hydration.clone(),
                            cancelled,
                        )
                    })
                    .unwrap_or_else(|| {
                        super::transcript_hydration::failed_hydration_result(
                            hydration, false, false, 0, started_at,
                        )
                    });
                SessionPreviewWorkerOutcome::Hydrated(Box::new(hydrated))
            }
        };
        #[cfg(test)]
        if retained_reader.as_ref().map(|(address, _)| address) != retained_address_before.as_ref()
            && retained_reader.is_some()
        {
            shared.open_count.fetch_add(1, Ordering::AcqRel);
        }
        if cancelled() {
            continue;
        }
        if event_tx
            .send(super::AppEvent::SessionPreviewCompleted(
                SessionPreviewWorkerResult {
                    worker_generation: request.worker_generation,
                    preview_generation: request.preview_generation,
                    outcome,
                },
            ))
            .is_err()
        {
            return;
        }
    }
}

fn load_session_preview(
    sessions: &smelt_core::session::SessionStorage,
    id: &str,
    width: u16,
    height: u16,
    retained_reader: &mut Option<(TranscriptStoreAddress, smelt_store::LineageSessionReader)>,
    cancelled: &dyn Fn() -> bool,
) -> Option<LoadedTranscript> {
    let resolved = sessions.resolve_session_for_read_result(id).ok()?;
    if cancelled() {
        return None;
    }
    let address =
        TranscriptStoreAddress::new(resolved.sessions_root, resolved.id, resolved.lineage_id);
    let needs_open = retained_reader
        .as_ref()
        .is_none_or(|(current, _)| current != &address);
    if needs_open {
        let reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
            &address.sessions_root,
            &address.lineage_id,
            &address.session_id,
        )
        .ok()?;
        *retained_reader = Some((address.clone(), reader));
    }
    let reader = &retained_reader.as_ref()?.1;
    let total_count = usize::try_from(reader.snapshot().ok()?.transcript_len).ok()?;
    if total_count == 0 {
        let lua = crate::lua::LuaRuntime::new();
        return super::history::materialize_full_transcript_read_only_result(sessions, &lua, id)
            .ok()
            .flatten();
    }
    let target_rows = super::transcript::record_tail_target_rows(height);
    let slice = reader
        .transcript_tail_for_rows_with_total(total_count, width, target_rows)
        .ok()?;
    if cancelled() {
        return None;
    }
    LoadedTranscript::from_record_slice(slice, address)
}

impl super::TuiApp {
    pub(crate) fn render_session_preview(
        &mut self,
        request: SessionPreviewRender,
    ) -> SessionPreviewRenderOutcome {
        let active = self.session_preview.begin(request);
        let cached = self
            .conversation
            .take_resume_preview(&active.render.cache_key);
        smelt_perf::perf::record_value(
            "session:render_preview_into:cache_hit",
            u64::from(cached.is_some()),
        );
        if let Some(view) = cached {
            return self.continue_session_preview(active, view);
        }

        self.request_session_preview_worker(
            active.generation,
            SessionPreviewWorkerOperation::Load {
                sessions: self.conversation.sessions().clone(),
                id: active.render.id.clone(),
                width: active.render.width,
                height: active.render.height,
            },
        );
        SessionPreviewRenderOutcome::Pending
    }

    fn continue_session_preview(
        &mut self,
        active: ActiveSessionPreview,
        mut view: TranscriptDocument,
    ) -> SessionPreviewRenderOutcome {
        let inline_options = self.inline_options();
        let theme = self.ui.theme().clone();
        let execution = self.lua.execution();
        view.set_inline_options(inline_options);
        let scroll_target = active
            .render
            .scroll_top
            .map(crate::content::transcript_buf::ScrollTarget::visible_row)
            .unwrap_or_else(crate::content::transcript_buf::ScrollTarget::visible_tail);
        let plan = match view.plan_projection_measured(
            &execution,
            active.render.width,
            &theme,
            scroll_target,
            active.render.height,
        ) {
            Ok(plan) => plan,
            Err(error) => {
                let hydration = view.take_pending_hydration_request_for_preview();
                self.conversation
                    .store_resume_preview(active.render.cache_key.clone(), view);
                if let Some(hydration) = hydration {
                    self.session_preview.active = Some(active.clone());
                    self.request_session_preview_worker(
                        active.generation,
                        SessionPreviewWorkerOperation::Hydrate(hydration),
                    );
                    return SessionPreviewRenderOutcome::Pending;
                }
                self.session_preview.finish(active.generation);
                return SessionPreviewRenderOutcome::Unavailable(error);
            }
        };
        let output = {
            let Some(target) = self.ui.buf_mut(active.render.buffer) else {
                self.session_preview.finish(active.generation);
                self.conversation
                    .store_resume_preview(active.render.cache_key, view);
                return SessionPreviewRenderOutcome::Pending;
            };
            view.project_planned(&execution, target, &theme, plan)
        };
        if let Some(window) = active
            .render
            .window
            .and_then(|window| self.ui.win_mut(window))
        {
            window.apply_materialized_rows(output);
            window.pin_scroll(output.clamped_scroll);
        }
        self.conversation
            .store_resume_preview(active.render.cache_key, view);
        self.session_preview.finish(active.generation);
        SessionPreviewRenderOutcome::Ready(output)
    }

    fn request_session_preview_worker(
        &mut self,
        preview_generation: u64,
        operation: SessionPreviewWorkerOperation,
    ) {
        let event_tx = self.platform.app_event_sender();
        self.session_preview
            .request(event_tx, preview_generation, operation);
    }

    pub(super) fn handle_session_preview_worker_result(
        &mut self,
        result: SessionPreviewWorkerResult,
    ) {
        if !self
            .session_preview
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_current(&result))
        {
            return;
        }
        let Some(active) = self.session_preview.active.clone() else {
            return;
        };
        if active.generation != result.preview_generation {
            return;
        }

        let outcome = match result.outcome {
            SessionPreviewWorkerOutcome::Loaded(Some(loaded)) => self.continue_session_preview(
                active.clone(),
                TranscriptDocument::from_deferred_loaded_transcript(*loaded),
            ),
            SessionPreviewWorkerOutcome::Loaded(None) => {
                self.session_preview.finish(active.generation);
                self.install_session_preview_message(&active, vec!["  (session missing)".into()]);
                self.request_session_preview_redraw();
                return;
            }
            SessionPreviewWorkerOutcome::Hydrated(hydrated) => {
                let hydrated = *hydrated;
                let complete = hydrated.record_complete && hydrated.blocks_complete;
                let Some(mut view) = self
                    .conversation
                    .take_resume_preview(&active.render.cache_key)
                else {
                    self.session_preview.finish(active.generation);
                    return;
                };
                view.install_hydration_result(hydrated);
                if !complete {
                    self.conversation
                        .store_resume_preview(active.render.cache_key.clone(), view);
                    self.session_preview.finish(active.generation);
                    self.install_unavailable_session_preview(&active);
                    self.request_session_preview_redraw();
                    return;
                }
                self.continue_session_preview(active.clone(), view)
            }
        };

        match outcome {
            SessionPreviewRenderOutcome::Ready(_) => self.request_session_preview_redraw(),
            SessionPreviewRenderOutcome::Pending => {}
            SessionPreviewRenderOutcome::Unavailable(_) => {
                self.install_unavailable_session_preview(&active);
                self.request_session_preview_redraw();
            }
        }
    }

    fn install_unavailable_session_preview(&mut self, active: &ActiveSessionPreview) {
        self.install_session_preview_message(
            active,
            vec![
                "  (session preview unavailable)".into(),
                String::new(),
                "  persisted content could not be hydrated".into(),
            ],
        );
    }

    fn install_session_preview_message(
        &mut self,
        active: &ActiveSessionPreview,
        lines: Vec<String>,
    ) {
        let total_rows = lines.len() as crate::smelt_edit::RowIndex;
        if let Some(buffer) = self.ui.buf_mut(active.render.buffer) {
            buffer.set_all_lines(lines);
        }
        if let Some(window) = active
            .render
            .window
            .and_then(|window| self.ui.win_mut(window))
        {
            window.apply_materialized_rows(crate::smelt_edit::MaterializedRows {
                clamped_scroll: 0,
                row_base: 0,
                total_rows,
                materialized_rows: total_rows,
            });
            window.pin_scroll(0);
        }
    }

    fn request_session_preview_redraw(&mut self) {
        self.ui.force_redraw();
        self.request_urgent_render();
    }

    #[cfg(any(test, feature = "harness"))]
    pub(super) fn session_preview_is_pending(&self) -> bool {
        self.session_preview.is_pending()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_session(
        state_root: &std::path::Path,
        marker: char,
    ) -> (smelt_core::session::SessionStorage, String) {
        let storage = smelt_core::session::SessionStorage::new(state_root.to_path_buf());
        let session_id = marker.to_string().repeat(64);
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.id.clone_from(&session_id);
        storage.save_result(&session).expect("save preview session");
        let resolved = storage
            .resolve_session_for_read_result(&session_id)
            .expect("resolve preview session");
        let address =
            TranscriptStoreAddress::new(resolved.sessions_root, resolved.id, resolved.lineage_id);
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        for index in 0..64 {
            transcript.push(smelt_core::transcript_model::Block::Text {
                content: format!("{marker} preview record {index}").into(),
            });
        }
        crate::persist::write_transcript_record_suffix(
            &address,
            0,
            &transcript.history.block_records(),
        )
        .expect("persist preview transcript");
        (storage, session_id)
    }

    fn load_request(
        preview_generation: u64,
        sessions: smelt_core::session::SessionStorage,
        id: String,
        width: u16,
        height: u16,
    ) -> SessionPreviewWorkerRequest {
        SessionPreviewWorkerRequest {
            worker_generation: 0,
            preview_generation,
            operation: SessionPreviewWorkerOperation::Load {
                sessions,
                id,
                width,
                height,
            },
        }
    }

    fn receive_preview_event(
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<super::super::AppEvent>,
    ) -> SessionPreviewWorkerResult {
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match receiver.try_recv() {
                Ok(super::super::AppEvent::SessionPreviewCompleted(result)) => return result,
                Ok(other) => panic!("unexpected app event: {other:?}"),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "session preview worker timed out"
                    );
                    thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    panic!("session preview worker disconnected")
                }
            }
        }
    }

    #[test]
    fn worker_supersedes_a_stale_preview_selection() {
        let root = tempfile::tempdir().expect("preview state root");
        let (first_storage, first_id) = seed_session(&root.path().join("first"), 'a');
        let (second_storage, second_id) = seed_session(&root.path().join("second"), 'b');
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = SessionPreviewWorker::spawn(event_tx);
        worker.set_delay(std::time::Duration::from_millis(60));
        worker.request(load_request(1, first_storage, first_id, 80, 20));
        thread::sleep(std::time::Duration::from_millis(10));
        worker.request(load_request(2, second_storage, second_id.clone(), 80, 20));

        let result = receive_preview_event(&mut event_rx);

        assert!(worker.is_current(&result));
        assert_eq!(result.preview_generation, 2);
        let SessionPreviewWorkerOutcome::Loaded(Some(loaded)) = result.outcome else {
            panic!("latest preview selection did not load")
        };
        assert_eq!(
            loaded
                .store_address
                .as_ref()
                .map(|address| &address.session_id),
            Some(&second_id)
        );
        thread::sleep(std::time::Duration::from_millis(80));
        assert!(matches!(
            event_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn worker_supersedes_a_stale_preview_resize() {
        let root = tempfile::tempdir().expect("preview state root");
        let (storage, session_id) = seed_session(root.path(), 'c');
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = SessionPreviewWorker::spawn(event_tx);
        worker.set_delay(std::time::Duration::from_millis(60));
        worker.request(load_request(7, storage.clone(), session_id.clone(), 24, 4));
        thread::sleep(std::time::Duration::from_millis(10));
        worker.request(load_request(8, storage, session_id, 120, 40));

        let result = receive_preview_event(&mut event_rx);

        assert!(worker.is_current(&result));
        assert_eq!(result.preview_generation, 8);
        assert!(matches!(
            result.outcome,
            SessionPreviewWorkerOutcome::Loaded(Some(_))
        ));
        thread::sleep(std::time::Duration::from_millis(80));
        assert!(matches!(
            event_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn worker_reuses_one_reader_for_sequential_requests_to_one_session() {
        let root = tempfile::tempdir().expect("preview state root");
        let (storage, session_id) = seed_session(root.path(), 'd');
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = SessionPreviewWorker::spawn(event_tx);

        worker.request(load_request(
            11,
            storage.clone(),
            session_id.clone(),
            80,
            20,
        ));
        let first = receive_preview_event(&mut event_rx);
        assert!(worker.is_current(&first));
        worker.request(load_request(12, storage, session_id, 100, 30));
        let second = receive_preview_event(&mut event_rx);

        assert!(worker.is_current(&second));
        assert_eq!(worker.open_count(), 1);
        assert!(matches!(
            first.outcome,
            SessionPreviewWorkerOutcome::Loaded(Some(_))
        ));
        assert!(matches!(
            second.outcome,
            SessionPreviewWorkerOutcome::Loaded(Some(_))
        ));
    }
}
