use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use smelt_store::{
    Catalog, CatalogAvailability, CatalogCursor, CatalogQuery, CatalogReader, CatalogReconcileLock,
    CatalogSession,
};

const MAX_PENDING_SESSIONS: usize = 1_024;
const MAX_OVERLAY_SESSIONS: usize = 1_024;
const WARNING_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServiceState {
    Reconciling,
    Ready,
    Degraded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServiceStatus {
    pub state: ServiceState,
    pub completed_scan_id: u64,
    pub reconciled_at: Option<i64>,
    pub last_error: Option<String>,
}

impl Default for ServiceStatus {
    fn default() -> Self {
        Self {
            state: ServiceState::Reconciling,
            completed_scan_id: 0,
            reconciled_at: None,
            last_error: None,
        }
    }
}

struct ReusableCatalog {
    status: ServiceStatus,
    pending_session_ids: Vec<String>,
}

fn reusable_catalog(path: &Path, sessions_root: &Path) -> Option<ReusableCatalog> {
    let reader = CatalogReader::open_existing(path).ok()??;
    let metadata = reader.metadata().ok()?;
    // Scan IDs are allocated before scanning. A larger gap means the last rebuild was interrupted.
    if !metadata.is_reconciled() {
        return None;
    }
    Some(ReusableCatalog {
        status: ServiceStatus {
            state: ServiceState::Ready,
            completed_scan_id: metadata.completed_scan_id,
            reconciled_at: metadata.reconciled_at,
            last_error: None,
        },
        pending_session_ids: smelt_store::pending_catalog_session_ids(sessions_root).ok()?,
    })
}

pub(crate) struct ReadPage {
    pub sessions: Vec<CatalogSession>,
    pub next_cursor: Option<CatalogCursor>,
    pub status: ServiceStatus,
}

#[derive(Clone, Debug)]
enum PendingAction {
    Project(u64),
    Remove,
}

#[derive(Default)]
struct WorkBatch {
    actions: HashMap<String, PendingAction>,
    reconcile_all: bool,
    barriers: Vec<mpsc::Sender<()>>,
}

#[derive(Default)]
struct PendingWork {
    batches: VecDeque<WorkBatch>,
    actions: HashMap<String, PendingAction>,
    reconcile_all: bool,
    shutdown: bool,
}

impl PendingWork {
    fn take_current(&mut self, barriers: Vec<mpsc::Sender<()>>) -> WorkBatch {
        WorkBatch {
            actions: std::mem::take(&mut self.actions),
            reconcile_all: std::mem::take(&mut self.reconcile_all),
            barriers,
        }
    }

    fn next_batch(&mut self) -> Option<WorkBatch> {
        self.batches.pop_front().or_else(|| {
            (!self.actions.is_empty() || self.reconcile_all).then(|| self.take_current(Vec::new()))
        })
    }
}

#[derive(Default)]
struct Overlays {
    active: HashMap<String, CatalogSession>,
    deleted: HashSet<String>,
}

#[derive(Clone)]
struct ServiceHandle {
    sessions_root: PathBuf,
    catalog_path: PathBuf,
    lock_path: PathBuf,
    pending: Arc<Mutex<PendingWork>>,
    overlays: Arc<Mutex<Overlays>>,
    status: Arc<Mutex<ServiceStatus>>,
    wake: SyncSender<()>,
}

struct ServiceOwner {
    #[cfg(test)]
    state_root: PathBuf,
    handle: ServiceHandle,
    worker: Option<thread::JoinHandle<()>>,
}

impl ServiceOwner {
    fn spawn(state_root: PathBuf) -> Result<Self, String> {
        crate::session::create_private_dir_all_in(&state_root, &state_root)
            .map_err(|error| format!("create session catalog state directory: {error}"))?;
        let sessions_root = state_root.join("sessions");
        let catalog_path = state_root.join("catalog.db");
        let lock_path = smelt_store::catalog_reconcile_lock_path(&sessions_root);
        let reusable = reusable_catalog(&catalog_path, &sessions_root);
        let mut startup_actions = HashMap::new();
        if let Some(reusable) = &reusable {
            for id in &reusable.pending_session_ids {
                startup_actions.insert(id.clone(), PendingAction::Project(0));
            }
        }
        let reconcile_all = reusable.is_none() || startup_actions.len() > MAX_PENDING_SESSIONS;
        if reconcile_all {
            startup_actions.clear();
        }
        let has_startup_work = reconcile_all || !startup_actions.is_empty();
        let pending = Arc::new(Mutex::new(PendingWork {
            actions: startup_actions,
            reconcile_all,
            ..PendingWork::default()
        }));
        let overlays = Arc::new(Mutex::new(Overlays::default()));
        let status = Arc::new(Mutex::new(
            reusable.map(|reusable| reusable.status).unwrap_or_default(),
        ));
        let (wake, wakes) = mpsc::sync_channel(1);
        let handle = ServiceHandle {
            sessions_root,
            catalog_path,
            lock_path,
            pending,
            overlays,
            status,
            wake,
        };
        let worker_handle = handle.clone();
        let worker = thread::Builder::new()
            .name("smelt-session-catalog".into())
            .spawn(move || catalog_worker(worker_handle, wakes))
            .map_err(|error| format!("spawn session catalog worker: {error}"))?;
        let owner = Self {
            #[cfg(test)]
            state_root,
            handle,
            worker: Some(worker),
        };
        if has_startup_work {
            owner.handle.signal()?;
        }
        Ok(owner)
    }
}

impl Drop for ServiceOwner {
    fn drop(&mut self) {
        self.handle
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .shutdown = true;
        let _ = self.handle.wake.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone)]
pub(crate) struct SessionCatalog {
    owner: Arc<ServiceOwner>,
}

impl SessionCatalog {
    pub(crate) fn open(state_root: PathBuf) -> Result<Self, String> {
        ServiceOwner::spawn(state_root).map(|owner| Self {
            owner: Arc::new(owner),
        })
    }

    pub(crate) fn request_reconciliation(&self) {
        self.owner.handle.request_reconciliation();
    }

    pub(crate) fn publish_commit(
        &self,
        command: &smelt_store::SessionCommit,
        receipt: &smelt_store::SaveReceipt,
        schedule_projection: bool,
    ) {
        publish_commit_to(&self.owner.handle, command, receipt, schedule_projection);
    }

    pub(crate) fn request_projection(&self, id: &str, revision: smelt_store::Revision) {
        self.owner
            .handle
            .request_action(id.to_string(), PendingAction::Project(revision.get()));
    }

    pub(crate) fn begin_delete(&self, id: &str) {
        self.owner.handle.begin_delete(id);
    }

    pub(crate) fn cancel_delete(&self, id: &str) {
        self.owner.handle.cancel_delete(id);
    }

    pub(crate) fn complete_delete(&self, id: &str) {
        self.owner
            .handle
            .request_action(id.to_string(), PendingAction::Remove);
    }

    pub(crate) fn read_page(&self, query: &CatalogQuery) -> ReadPage {
        read_page_from(&self.owner.handle, query)
    }

    pub(crate) fn wait_for_queued_work(&self, timeout: Duration) -> bool {
        self.owner.handle.wait_for_barrier(timeout)
    }
}

impl ServiceHandle {
    fn signal(&self) -> Result<(), String> {
        match self.wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => Ok(()),
            Err(TrySendError::Disconnected(())) => {
                Err("session catalog worker disconnected".into())
            }
        }
    }

    fn enqueue_barrier(&self) -> Result<mpsc::Receiver<()>, String> {
        let (complete, completed) = mpsc::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if pending.shutdown {
                return Err("session catalog worker is shutting down".into());
            }
            let batch = pending.take_current(vec![complete]);
            pending.batches.push_back(batch);
        }
        self.signal()?;
        Ok(completed)
    }

    fn wait_for_barrier(&self, timeout: Duration) -> bool {
        self.enqueue_barrier()
            .is_ok_and(|completed| completed.recv_timeout(timeout).is_ok())
    }

    fn request_reconciliation(&self) {
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            pending.reconcile_all = true;
        }
        self.set_reconciling();
        if let Err(error) = self.signal() {
            self.set_degraded(error);
        }
    }

    fn request_action(&self, id: String, action: PendingAction) {
        let overflowed = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if pending.reconcile_all {
                false
            } else if let Some(current) = pending.actions.get_mut(&id) {
                match (&mut *current, action) {
                    (
                        PendingAction::Project(current_revision),
                        PendingAction::Project(revision),
                    ) => {
                        *current_revision = (*current_revision).max(revision);
                    }
                    (current, replacement) => *current = replacement,
                }
                smelt_perf::perf::record_value("session:catalog:coalesced", 1);
                false
            } else if pending.actions.len() >= MAX_PENDING_SESSIONS {
                pending.actions.clear();
                pending.reconcile_all = true;
                true
            } else {
                pending.actions.insert(id, action);
                false
            }
        };
        if overflowed {
            smelt_perf::perf::record_value("session:catalog:queue_overflow", 1);
            self.set_reconciling();
        }
        let pending_depth = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .actions
            .len();
        smelt_perf::perf::record_value(
            "session:catalog:pending_distinct_sessions",
            pending_depth as u64,
        );
        if let Err(error) = self.signal() {
            self.set_degraded(error);
        }
    }

    fn publish_overlay(&self, session: CatalogSession) {
        let mut overlays = self
            .overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !overlays.active.contains_key(&session.id)
            && overlays.active.len() >= MAX_OVERLAY_SESSIONS
        {
            overlays.active.clear();
        }
        overlays.deleted.remove(&session.id);
        overlays.active.insert(session.id.clone(), session);
    }

    fn begin_delete(&self, id: &str) {
        let mut overlays = self
            .overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !overlays.deleted.contains(id) && overlays.deleted.len() >= MAX_OVERLAY_SESSIONS {
            overlays.deleted.clear();
        }
        overlays.deleted.insert(id.to_string());
    }

    fn cancel_delete(&self, id: &str) {
        self.overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .deleted
            .remove(id);
    }

    fn clear_projected(&self, id: &str, revision: u64) {
        let mut overlays = self
            .overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if overlays
            .active
            .get(id)
            .is_some_and(|row| row.source_revision <= revision)
        {
            overlays.active.remove(id);
        }
    }

    fn clear_missing(&self, id: &str, revision: u64) {
        self.clear_projected(id, revision);
        self.overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .deleted
            .remove(id);
    }

    fn clear_removed(&self, id: &str) {
        let mut overlays = self
            .overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        overlays.active.remove(id);
        overlays.deleted.remove(id);
    }

    fn set_reconciling(&self) {
        self.status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .state = ServiceState::Reconciling;
    }

    fn set_ready(&self, completed_scan_id: u64, reconciled_at: Option<i64>) {
        *self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = ServiceStatus {
            state: ServiceState::Ready,
            completed_scan_id,
            reconciled_at,
            last_error: None,
        };
    }

    fn set_degraded(&self, error: String) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        status.state = ServiceState::Degraded;
        status.last_error = Some(error);
    }
}

#[cfg(test)]
fn global_service() -> &'static Mutex<Option<ServiceOwner>> {
    static SERVICE: OnceLock<Mutex<Option<ServiceOwner>>> = OnceLock::new();
    SERVICE.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn service() -> Option<ServiceHandle> {
    let state_root = crate::config::state_dir();
    let mut service = global_service()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if service
        .as_ref()
        .is_some_and(|owner| owner.state_root != state_root)
    {
        service.take();
    }
    if service.is_none() {
        match ServiceOwner::spawn(state_root) {
            Ok(owner) => *service = Some(owner),
            Err(error) => {
                eprintln!("smelt: failed to start session catalog: {error}");
                return None;
            }
        }
    }
    service.as_ref().map(|owner| owner.handle.clone())
}

#[cfg(test)]
pub(crate) fn request_reconciliation() {
    if let Some(service) = service() {
        service.request_reconciliation();
    }
}

#[cfg(test)]
pub(crate) fn publish_commit(
    command: &smelt_store::SessionCommit,
    receipt: &smelt_store::SaveReceipt,
    schedule_projection: bool,
) {
    let Some(service) = service() else {
        return;
    };
    publish_commit_to(&service, command, receipt, schedule_projection);
}

fn publish_commit_to(
    service: &ServiceHandle,
    command: &smelt_store::SessionCommit,
    receipt: &smelt_store::SaveReceipt,
    schedule_projection: bool,
) {
    service.publish_overlay(catalog_session_from_commit(command, receipt));
    if schedule_projection {
        service.request_action(
            receipt.session_id.clone(),
            PendingAction::Project(receipt.current.revision.get()),
        );
    }
}

#[cfg(test)]
pub(crate) fn request_projection(id: &str, revision: smelt_store::Revision) {
    let Some(service) = service() else {
        return;
    };
    service.request_action(id.to_string(), PendingAction::Project(revision.get()));
}

#[cfg(test)]
pub(crate) fn begin_delete(id: &str) {
    if let Some(service) = service() {
        service.begin_delete(id);
    }
}

#[cfg(test)]
pub(crate) fn cancel_delete(id: &str) {
    if let Some(service) = service() {
        service.cancel_delete(id);
    }
}

#[cfg(test)]
pub(crate) fn read_page(query: &CatalogQuery) -> ReadPage {
    let Some(service) = service() else {
        return unavailable_read_page("session catalog service is unavailable");
    };
    read_page_from(&service, query)
}

pub(crate) fn unavailable_read_page(error: impl Into<String>) -> ReadPage {
    ReadPage {
        sessions: Vec::new(),
        next_cursor: None,
        status: ServiceStatus {
            state: ServiceState::Degraded,
            completed_scan_id: 0,
            reconciled_at: None,
            last_error: Some(error.into()),
        },
    }
}

fn read_page_from(service: &ServiceHandle, query: &CatalogQuery) -> ReadPage {
    let _perf = smelt_perf::perf::begin("session:catalog:query");
    let (active, deleted) = {
        let overlays = service
            .overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        (overlays.active.clone(), overlays.deleted.clone())
    };
    let extra = active.len().saturating_add(deleted.len());
    let internal_limit = (query.limit as usize)
        .saturating_add(extra)
        .saturating_add(1)
        .min(smelt_store::MAX_CATALOG_PAGE_SIZE as usize) as u32;
    let internal_query = CatalogQuery {
        limit: internal_limit.max(query.limit),
        cursor: query.cursor.clone(),
        cwd: query.cwd.clone(),
        availability: query.availability,
    };

    let (mut sessions, catalog_has_more, metadata) =
        match CatalogReader::open_existing(&service.catalog_path).and_then(|reader| {
            reader.map_or_else(
                || Ok(None),
                |reader| {
                    let metadata = reader.metadata()?;
                    let page = reader.page(&internal_query)?;
                    Ok(Some((
                        page.sessions,
                        page.next_cursor.is_some(),
                        Some(metadata),
                    )))
                },
            )
        }) {
            Ok(Some(result)) => result,
            Ok(None) => {
                service.request_reconciliation();
                (Vec::new(), false, None)
            }
            Err(error) => {
                service.set_degraded(format!("read session catalog: {error}"));
                service.request_reconciliation();
                (Vec::new(), false, None)
            }
        };

    let mut by_id = sessions
        .drain(..)
        .map(|session| (session.id.clone(), session))
        .collect::<HashMap<_, _>>();
    for (id, session) in active {
        if row_matches_query(&session, query) {
            by_id.insert(id, session);
        } else {
            by_id.remove(&id);
        }
    }
    for id in deleted {
        by_id.remove(&id);
    }
    sessions = by_id.into_values().collect();
    sessions.retain(|session| row_matches_query(session, query));
    sessions.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.id.cmp(&right.id))
    });

    let has_more = catalog_has_more || sessions.len() > query.limit as usize;
    sessions.truncate(query.limit as usize);
    let next_cursor = has_more
        .then(|| {
            sessions.last().map(|last| CatalogCursor {
                updated_at: last.updated_at,
                id: last.id.clone(),
            })
        })
        .flatten();

    let mut status = service
        .status
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(metadata) = metadata {
        status.completed_scan_id = metadata.completed_scan_id;
        status.reconciled_at = metadata.reconciled_at;
    }
    smelt_perf::perf::record_value("session:catalog:query_returned_rows", sessions.len() as u64);
    ReadPage {
        sessions,
        next_cursor,
        status,
    }
}

fn row_matches_query(session: &CatalogSession, query: &CatalogQuery) -> bool {
    if query
        .cwd
        .as_ref()
        .is_some_and(|cwd| session.cwd.as_ref() != Some(cwd))
    {
        return false;
    }
    if query
        .availability
        .is_some_and(|availability| session.availability != availability)
    {
        return false;
    }
    query.cursor.as_ref().is_none_or(|cursor| {
        session.updated_at < cursor.updated_at
            || (session.updated_at == cursor.updated_at && session.id > cursor.id)
    })
}

fn catalog_session_from_commit(
    command: &smelt_store::SessionCommit,
    receipt: &smelt_store::SaveReceipt,
) -> CatalogSession {
    let metadata = &command.metadata;
    CatalogSession {
        id: receipt.session_id.clone(),
        lineage_id: None,
        title: metadata.title.clone(),
        slug: metadata.slug.clone(),
        first_user_message: metadata.first_user_message.clone(),
        cwd: metadata.cwd.clone(),
        mode: metadata.mode.clone(),
        reasoning_effort: metadata.reasoning_effort.clone(),
        model: metadata.model.clone(),
        fast_mode: metadata.fast_mode,
        parent_id: command.identity.parent_id.clone(),
        context_tokens: metadata.display_context_tokens.or(metadata.context_tokens),
        history_len: Some(receipt.current.history_len.get()),
        text_bytes: None,
        created_at: command.identity.created_at,
        updated_at: metadata.updated_at,
        source_revision: receipt.current.revision.get(),
        availability: CatalogAvailability::Available,
        error_kind: None,
        error_summary: None,
        last_seen_scan: 0,
    }
}

fn catalog_worker(handle: ServiceHandle, wakes: mpsc::Receiver<()>) {
    const RETRY_DELAY: Duration = Duration::from_millis(50);

    let mut warning = WarningLimiter::default();
    while wakes.recv().is_ok() {
        loop {
            let Some(mut batch) = ({
                let mut pending = handle
                    .pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if pending.shutdown {
                    pending.batches.clear();
                    return;
                }
                pending.next_batch()
            }) else {
                break;
            };

            if batch.reconcile_all {
                handle.set_reconciling();
                match reconcile_all_sessions(&handle) {
                    Ok(pending_ids) => {
                        let mut recovery_error = None;
                        for id in pending_ids {
                            if let Err(error) = project_session(&handle, &id, 0) {
                                recovery_error = Some(error);
                                break;
                            }
                        }
                        if let Some(error) = recovery_error {
                            warning.warn(&error);
                            handle.set_degraded(error);
                            batch.reconcile_all = true;
                            requeue_failed_batch(&handle, batch, RETRY_DELAY);
                            break;
                        }
                        complete_barriers(batch.barriers);
                    }
                    Err(error) => {
                        warning.warn(&error);
                        handle.set_degraded(error);
                        batch.reconcile_all = true;
                        requeue_failed_batch(&handle, batch, RETRY_DELAY);
                        break;
                    }
                }
                continue;
            }

            let mut needs_reconciliation = false;
            for (id, action) in std::mem::take(&mut batch.actions) {
                let result = match action {
                    PendingAction::Project(minimum_revision) => {
                        project_session(&handle, &id, minimum_revision)
                    }
                    PendingAction::Remove => remove_session(&handle, &id).map(|()| false),
                };
                match result {
                    Ok(reconcile) => needs_reconciliation |= reconcile,
                    Err(error) => {
                        warning.warn(&error);
                        handle.set_degraded(error);
                        needs_reconciliation = true;
                    }
                }
            }
            if needs_reconciliation {
                batch.reconcile_all = true;
                requeue_failed_batch(&handle, batch, RETRY_DELAY);
                break;
            }
            complete_barriers(batch.barriers);
        }
    }
}

fn requeue_failed_batch(handle: &ServiceHandle, batch: WorkBatch, retry_delay: Duration) {
    handle
        .pending
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .batches
        .push_front(batch);
    thread::sleep(retry_delay);
    let _ = handle.signal();
}

fn complete_barriers(barriers: Vec<mpsc::Sender<()>>) {
    for barrier in barriers {
        let _ = barrier.send(());
    }
}

fn project_session(
    handle: &ServiceHandle,
    id: &str,
    minimum_revision: u64,
) -> Result<bool, String> {
    let _duration = smelt_perf::perf::begin_value_ms("session:catalog:projection_duration_ms");
    let _perf = smelt_perf::perf::begin("session:catalog:project");
    smelt_perf::perf::record_value(
        "session:catalog:project_requested_revision",
        minimum_revision,
    );
    let (needs_reconciliation, projection_completed, pending_token) = {
        let _lock = CatalogReconcileLock::acquire(&handle.lock_path)
            .map_err(|error| format!("lock session catalog projection: {error}"))?;
        let pending_token =
            smelt_store::catalog_session_pending_token(&handle.sessions_root, id)
                .map_err(|error| format!("read pending catalog projection for {id}: {error}"))?;
        let mut projected = load_projection(&handle.sessions_root, id);
        if projected.as_ref().is_ok_and(|session| {
            session
                .as_ref()
                .is_some_and(|session| session.source_revision < minimum_revision)
        }) {
            smelt_perf::perf::record_value("session:catalog:post_publication_retry", 1);
            projected = load_projection(&handle.sessions_root, id);
        }
        let mut catalog = Catalog::open(&handle.catalog_path)
            .map_err(|error| format!("open session catalog for projection: {error}"))?;
        let (needs_reconciliation, projection_completed) = match projected {
            Ok(Some(session)) => {
                let revision = session.source_revision;
                smelt_perf::perf::record_value(
                    "session:catalog:projection_revision_lag",
                    minimum_revision.saturating_sub(revision),
                );
                catalog
                    .upsert_available(&session)
                    .map_err(|error| format!("project session {id}: {error}"))?;
                handle.clear_projected(id, revision);
                let revision_lagged = revision < minimum_revision;
                (revision_lagged, !revision_lagged)
            }
            Ok(None) => {
                catalog.remove(id).map_err(|error| {
                    format!("remove missing session {id} from catalog: {error}")
                })?;
                handle.clear_missing(id, minimum_revision);
                let expected_commit_missing = minimum_revision > 0;
                (expected_commit_missing, !expected_commit_missing)
            }
            Err(error) => {
                catalog
                    .upsert_unavailable(id, &error.kind, &error.summary)
                    .map_err(|catalog_error| {
                        format!("record unavailable session {id}: {catalog_error}")
                    })?;
                handle.clear_projected(id, minimum_revision);
                (false, false)
            }
        };
        (needs_reconciliation, projection_completed, pending_token)
    };
    if let (true, Some(token)) = (projection_completed, pending_token) {
        smelt_store::clear_catalog_session_pending(&handle.sessions_root, id, &token)
            .map_err(|error| format!("clear pending catalog projection for {id}: {error}"))?;
    }
    Ok(needs_reconciliation)
}

fn remove_session(handle: &ServiceHandle, id: &str) -> Result<(), String> {
    let pending_token = {
        let _lock = CatalogReconcileLock::acquire(&handle.lock_path)
            .map_err(|error| format!("lock session catalog removal: {error}"))?;
        let pending_token =
            smelt_store::catalog_session_pending_token(&handle.sessions_root, id)
                .map_err(|error| format!("read pending catalog removal for {id}: {error}"))?;
        let mut catalog = Catalog::open(&handle.catalog_path)
            .map_err(|error| format!("open session catalog for removal: {error}"))?;
        catalog
            .remove(id)
            .map_err(|error| format!("remove session {id} from catalog: {error}"))?;
        handle.clear_removed(id);
        pending_token
    };
    if let Some(token) = pending_token {
        smelt_store::clear_catalog_session_pending(&handle.sessions_root, id, &token)
            .map_err(|error| format!("clear pending catalog removal for {id}: {error}"))?;
    }
    Ok(())
}

fn reconcile_all_sessions(handle: &ServiceHandle) -> Result<Vec<String>, String> {
    let _duration = smelt_perf::perf::begin_value_ms("session:catalog:reconciliation_duration_ms");
    let lock = CatalogReconcileLock::acquire(&handle.lock_path)
        .map_err(|error| format!("lock session catalog reconciliation: {error}"))?;
    let mut catalog = match Catalog::open(&handle.catalog_path) {
        Ok(catalog) => catalog,
        Err(open_error) => {
            smelt_perf::perf::record_value("session:catalog:integrity_failures", 1);
            smelt_perf::perf::record_value("session:catalog:rebuilds", 1);
            smelt_store::archive_corrupt_catalog(&handle.catalog_path)
                .map_err(|error| format!("archive session catalog after {open_error}: {error}"))?;
            Catalog::open(&handle.catalog_path)
                .map_err(|error| format!("open rebuilt session catalog: {error}"))?
        }
    };
    let scan_id = catalog
        .allocate_scan()
        .map_err(|error| format!("allocate session catalog scan: {error}"))?;
    let (active_overlays, tombstones) = {
        let overlays = handle
            .overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let active = overlays
            .active
            .iter()
            .map(|(id, session)| (id.clone(), session.source_revision))
            .collect::<Vec<_>>();
        (active, overlays.deleted.clone())
    };
    let mut seen_tombstones = HashSet::with_capacity(tombstones.len());
    let mut candidates = 0_u64;
    let mut available = 0_u64;
    let mut unavailable = 0_u64;
    let mut completed_pending = Vec::new();
    let lineage_ids = smelt_store::lineage_session_ids(&handle.sessions_root)
        .map_err(|error| format!("enumerate lineage branches for catalog: {error}"))?;
    for id in lineage_ids {
        candidates += 1;
        if tombstones.contains(&id) {
            seen_tombstones.insert(id.clone());
        }
        let pending_token = smelt_store::catalog_session_pending_token(&handle.sessions_root, &id)
            .map_err(|error| format!("read pending catalog projection for {id}: {error}"))?;
        match load_projection(&handle.sessions_root, &id) {
            Ok(Some(session)) => {
                available += 1;
                let revision = session.source_revision;
                catalog
                    .upsert_available_for_reconciliation(&session, scan_id)
                    .map_err(|error| format!("reconcile lineage session {id}: {error}"))?;
                handle.clear_projected(&id, revision);
                if let Some(token) = pending_token {
                    completed_pending.push((id.clone(), token));
                }
            }
            Ok(None) => {}
            Err(error) => {
                unavailable += 1;
                catalog
                    .upsert_unavailable_for_reconciliation(
                        &id,
                        &error.kind,
                        &error.summary,
                        scan_id,
                    )
                    .map_err(|catalog_error| {
                        format!("reconcile unavailable lineage session {id}: {catalog_error}")
                    })?;
            }
        }
    }
    smelt_perf::perf::record_value("session:catalog:reconcile_scanned", candidates);
    smelt_perf::perf::record_value("session:catalog:reconcile_available", available);
    smelt_perf::perf::record_value("session:catalog:reconcile_unavailable", unavailable);
    let reconciled_at = i64::try_from(crate::session::now_ms()).unwrap_or(i64::MAX);
    let deleted = catalog
        .complete_scan(scan_id, reconciled_at)
        .map_err(|error| format!("complete session catalog scan {scan_id}: {error}"))?;
    smelt_perf::perf::record_value("session:catalog:reconcile_removed", deleted as u64);

    for (id, revision) in active_overlays {
        handle.clear_projected(&id, revision);
    }
    {
        let mut overlays = handle
            .overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        retain_reconciliation_tombstones(&mut overlays.deleted, &tombstones, &seen_tombstones);
    }
    let metadata = catalog
        .metadata()
        .map_err(|error| format!("read reconciled session catalog metadata: {error}"))?;
    handle.set_ready(metadata.completed_scan_id, metadata.reconciled_at);
    drop(catalog);
    drop(lock);

    for (id, token) in completed_pending {
        smelt_store::clear_catalog_session_pending(&handle.sessions_root, &id, &token)
            .map_err(|error| format!("clear pending catalog projection for {id}: {error}"))?;
    }
    smelt_store::pending_catalog_session_ids(&handle.sessions_root)
        .map_err(|error| format!("enumerate pending catalog projections: {error}"))
}

fn retain_reconciliation_tombstones(
    current: &mut HashSet<String>,
    snapshot: &HashSet<String>,
    seen: &HashSet<String>,
) {
    current.retain(|id| !snapshot.contains(id) || seen.contains(id));
}

#[derive(Debug)]
struct ProjectionError {
    kind: String,
    summary: String,
}

fn load_projection(root: &Path, id: &str) -> Result<Option<CatalogSession>, ProjectionError> {
    let Some(reader) =
        smelt_store::LineageSessionReader::try_open_existing(root, id).map_err(|error| {
            ProjectionError {
                kind: "corrupt".into(),
                summary: format!("open lineage session {id} for catalog projection: {error}"),
            }
        })?
    else {
        return Ok(None);
    };
    let state = reader.snapshot().map_err(|error| ProjectionError {
        kind: "corrupt".into(),
        summary: format!("read lineage session {id} for catalog projection: {error}"),
    })?;
    if state.identity.id != id {
        return Err(ProjectionError {
            kind: "corrupt".into(),
            summary: format!(
                "persisted lineage branch id {} does not match requested session {id}",
                state.identity.id
            ),
        });
    }
    let metadata = state.metadata;
    Ok(Some(CatalogSession {
        id: state.identity.id,
        lineage_id: Some(state.lineage_id),
        title: metadata.title,
        slug: metadata.slug,
        first_user_message: metadata.first_user_message,
        cwd: metadata.cwd,
        mode: metadata.mode,
        reasoning_effort: metadata.reasoning_effort,
        model: metadata.model,
        fast_mode: metadata.fast_mode,
        parent_id: state.identity.parent_id,
        context_tokens: metadata.display_context_tokens.or(metadata.context_tokens),
        history_len: Some(state.head.history_len.get()),
        text_bytes: Some(state.history_text_bytes),
        created_at: state.identity.created_at,
        updated_at: metadata.updated_at,
        source_revision: state.head.revision.get(),
        availability: CatalogAvailability::Available,
        error_kind: None,
        error_summary: None,
        last_seen_scan: 0,
    }))
}

#[derive(Default)]
struct WarningLimiter {
    last: Option<(String, Instant)>,
}

impl WarningLimiter {
    fn warn(&mut self, message: &str) {
        let now = Instant::now();
        let should_warn = self.last.as_ref().is_none_or(|(last, at)| {
            last != message || now.duration_since(*at) >= WARNING_INTERVAL
        });
        if should_warn {
            eprintln!("smelt: session catalog warning: {message}");
            self.last = Some((message.to_string(), now));
        }
    }
}

#[cfg(test)]
pub(crate) fn wait_for_queued_work(timeout: Duration) -> bool {
    service().is_some_and(|service| service.wait_for_barrier(timeout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const SESSION_ID: &str = "1000000000000000000000000000000000000000000000000000000000000001";

    fn canonical_session(title: &str) -> crate::session::Session {
        let mut session = crate::session::Session::new(1, PathBuf::from("/workspace"));
        session.id = SESSION_ID.into();
        session.title = Some(title.into());
        session.updated_at_ms = 1_700_000_000_000;
        session
    }

    fn test_worker(root: &Path) -> (ServiceHandle, thread::JoinHandle<()>) {
        test_worker_with_lock(
            root,
            smelt_store::catalog_reconcile_lock_path(root.join("sessions")),
        )
    }

    fn test_worker_with_lock(
        root: &Path,
        lock_path: PathBuf,
    ) -> (ServiceHandle, thread::JoinHandle<()>) {
        let (wake, wakes) = mpsc::sync_channel(1);
        let handle = ServiceHandle {
            sessions_root: root.join("sessions"),
            catalog_path: root.join("catalog.db"),
            lock_path,
            pending: Arc::new(Mutex::new(PendingWork::default())),
            overlays: Arc::new(Mutex::new(Overlays::default())),
            status: Arc::new(Mutex::new(ServiceStatus::default())),
            wake,
        };
        let worker_handle = handle.clone();
        let worker = thread::spawn(move || catalog_worker(worker_handle, wakes));
        (handle, worker)
    }

    fn stop_test_worker(handle: &ServiceHandle, worker: thread::JoinHandle<()>) {
        handle
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .shutdown = true;
        handle.signal().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn completed_catalog_is_reused_without_startup_reconciliation() {
        let temp = tempfile::tempdir().unwrap();
        let catalog_path = temp.path().join("catalog.db");
        let mut catalog = Catalog::open(&catalog_path).unwrap();
        let scan_id = catalog.allocate_scan().unwrap();
        catalog.complete_scan(scan_id, 1_700_000_000_000).unwrap();
        drop(catalog);

        let service = SessionCatalog::open(temp.path().to_path_buf()).unwrap();
        assert!(service.wait_for_queued_work(Duration::from_secs(2)));
        let page = service.read_page(&CatalogQuery::default());
        assert_eq!(page.status.state, ServiceState::Ready);
        assert_eq!(page.status.completed_scan_id, scan_id);
        drop(service);

        let metadata = CatalogReader::open_existing(catalog_path)
            .unwrap()
            .unwrap()
            .metadata()
            .unwrap();
        assert_eq!(metadata.completed_scan_id, scan_id);
        assert_eq!(metadata.next_scan_id, scan_id + 1);
    }

    #[test]
    fn interrupted_catalog_scan_is_rebuilt_on_startup() {
        let temp = tempfile::tempdir().unwrap();
        let catalog_path = temp.path().join("catalog.db");
        let mut catalog = Catalog::open(&catalog_path).unwrap();
        let completed_scan = catalog.allocate_scan().unwrap();
        catalog
            .complete_scan(completed_scan, 1_700_000_000_000)
            .unwrap();
        let interrupted_scan = catalog.allocate_scan().unwrap();
        drop(catalog);

        let service = SessionCatalog::open(temp.path().to_path_buf()).unwrap();
        assert!(service.wait_for_queued_work(Duration::from_secs(2)));
        drop(service);

        let metadata = CatalogReader::open_existing(catalog_path)
            .unwrap()
            .unwrap()
            .metadata()
            .unwrap();
        assert_eq!(metadata.completed_scan_id, interrupted_scan + 1);
        assert_eq!(metadata.next_scan_id, interrupted_scan + 2);
    }

    #[test]
    fn startup_replays_a_commit_left_dirty_before_catalog_projection() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join("sessions");
        let catalog_path = temp.path().join("catalog.db");
        let mut catalog = Catalog::open(&catalog_path).unwrap();
        let scan_id = catalog.allocate_scan().unwrap();
        catalog.complete_scan(scan_id, 1_700_000_000_000).unwrap();
        drop(catalog);

        let session = canonical_session("committed before crash");
        let command = crate::session::initial_store_commit_from_session(&session).unwrap();
        let mut writer = smelt_store::OwnedLineageWriter::open(&sessions_root, SESSION_ID).unwrap();
        writer.commit_session(&command).unwrap();
        writer.release().unwrap();
        let pending = smelt_store::pending_catalog_session_ids(&sessions_root).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0], SESSION_ID);

        let service = SessionCatalog::open(temp.path().to_path_buf()).unwrap();
        assert!(service.wait_for_queued_work(Duration::from_secs(2)));
        drop(service);

        let reader = CatalogReader::open_existing(&catalog_path)
            .unwrap()
            .unwrap();
        assert_eq!(
            reader
                .session(SESSION_ID)
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("committed before crash")
        );
        let metadata = reader.metadata().unwrap();
        assert_eq!(metadata.completed_scan_id, scan_id);
        assert_eq!(metadata.next_scan_id, scan_id + 1);
        assert!(smelt_store::pending_catalog_session_ids(&sessions_root)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn startup_replays_a_delete_left_dirty_before_catalog_projection() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join("sessions");
        let catalog_path = temp.path().join("catalog.db");
        let session = canonical_session("deleted before crash");
        let command = crate::session::initial_store_commit_from_session(&session).unwrap();
        let mut writer = smelt_store::OwnedLineageWriter::open(&sessions_root, SESSION_ID).unwrap();
        writer.commit_session(&command).unwrap();
        let projected = load_projection(&sessions_root, SESSION_ID)
            .unwrap()
            .unwrap();
        let mut catalog = Catalog::open(&catalog_path).unwrap();
        let scan_id = catalog.allocate_scan().unwrap();
        catalog
            .upsert_available_for_reconciliation(&projected, scan_id)
            .unwrap();
        catalog.complete_scan(scan_id, 1_700_000_000_000).unwrap();
        drop(catalog);
        let token = smelt_store::catalog_session_pending_token(&sessions_root, SESSION_ID)
            .unwrap()
            .unwrap();
        smelt_store::clear_catalog_session_pending(&sessions_root, SESSION_ID, &token).unwrap();

        writer
            .delete_branch(session.created_at_ms.saturating_add(1))
            .unwrap();
        let service = SessionCatalog::open(temp.path().to_path_buf()).unwrap();
        assert!(service.wait_for_queued_work(Duration::from_secs(2)));
        drop(service);

        let reader = CatalogReader::open_existing(&catalog_path)
            .unwrap()
            .unwrap();
        assert!(reader.session(SESSION_ID).unwrap().is_none());
        let metadata = reader.metadata().unwrap();
        assert_eq!(metadata.completed_scan_id, scan_id);
        assert_eq!(metadata.next_scan_id, scan_id + 1);
        assert!(smelt_store::pending_catalog_session_ids(&sessions_root)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn startup_replays_a_fork_left_dirty_before_catalog_projection() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join("sessions");
        let catalog_path = temp.path().join("catalog.db");
        let target_id = "2".repeat(64);
        let session = canonical_session("fork source");
        let command = crate::session::initial_store_commit_from_session(&session).unwrap();
        let mut writer = smelt_store::OwnedLineageWriter::open(&sessions_root, SESSION_ID).unwrap();
        writer.commit_session(&command).unwrap();
        let projected = load_projection(&sessions_root, SESSION_ID)
            .unwrap()
            .unwrap();
        let mut catalog = Catalog::open(&catalog_path).unwrap();
        let scan_id = catalog.allocate_scan().unwrap();
        catalog
            .upsert_available_for_reconciliation(&projected, scan_id)
            .unwrap();
        catalog.complete_scan(scan_id, 1_700_000_000_000).unwrap();
        drop(catalog);
        let token = smelt_store::catalog_session_pending_token(&sessions_root, SESSION_ID)
            .unwrap()
            .unwrap();
        smelt_store::clear_catalog_session_pending(&sessions_root, SESSION_ID, &token).unwrap();

        writer
            .fork_current(&target_id, session.created_at_ms.saturating_add(1))
            .unwrap();
        writer.release().unwrap();
        let pending = smelt_store::pending_catalog_session_ids(&sessions_root).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0], target_id);

        let service = SessionCatalog::open(temp.path().to_path_buf()).unwrap();
        assert!(service.wait_for_queued_work(Duration::from_secs(2)));
        drop(service);

        let reader = CatalogReader::open_existing(&catalog_path)
            .unwrap()
            .unwrap();
        let fork = reader.session(&target_id).unwrap().unwrap();
        assert_eq!(fork.parent_id.as_deref(), Some(SESSION_ID));
        let metadata = reader.metadata().unwrap();
        assert_eq!(metadata.completed_scan_id, scan_id);
        assert_eq!(metadata.next_scan_id, scan_id + 1);
        assert!(smelt_store::pending_catalog_session_ids(&sessions_root)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reconciliation_ignores_noncanonical_session_directories() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("sessions").join(SESSION_ID)).unwrap();
        let (handle, worker) = test_worker(temp.path());

        reconcile_all_sessions(&handle).unwrap();

        let page = CatalogReader::open_existing(&handle.catalog_path)
            .unwrap()
            .unwrap()
            .page(&CatalogQuery::default())
            .unwrap();
        assert!(page.sessions.is_empty());
        stop_test_worker(&handle, worker);
    }

    fn stale_catalog_row() -> CatalogSession {
        CatalogSession {
            id: SESSION_ID.into(),
            lineage_id: None,
            title: Some("stale".into()),
            slug: None,
            first_user_message: None,
            cwd: Some("/workspace".into()),
            mode: None,
            reasoning_effort: None,
            model: None,
            fast_mode: None,
            parent_id: None,
            context_tokens: None,
            history_len: Some(0),
            text_bytes: Some(0),
            created_at: 1,
            updated_at: 1,
            source_revision: 0,
            availability: CatalogAvailability::Available,
            error_kind: None,
            error_summary: None,
            last_seen_scan: 0,
        }
    }

    #[test]
    fn pending_projection_map_coalesces_and_overflow_requests_reconciliation() {
        let temp = tempfile::tempdir().unwrap();
        let (wake, _wakes) = mpsc::sync_channel(1);
        let handle = ServiceHandle {
            sessions_root: temp.path().join("sessions"),
            catalog_path: temp.path().join("catalog.db"),
            lock_path: temp.path().join(".catalog.lock"),
            pending: Arc::new(Mutex::new(PendingWork::default())),
            overlays: Arc::new(Mutex::new(Overlays::default())),
            status: Arc::new(Mutex::new(ServiceStatus::default())),
            wake,
        };

        handle.request_action(SESSION_ID.into(), PendingAction::Project(2));
        handle.request_action(SESSION_ID.into(), PendingAction::Project(5));
        assert!(matches!(
            handle.pending.lock().unwrap().actions.get(SESSION_ID),
            Some(PendingAction::Project(5))
        ));

        handle.pending.lock().unwrap().actions.clear();
        for value in 0..=MAX_PENDING_SESSIONS {
            handle.request_action(format!("{value:064x}"), PendingAction::Project(1));
        }
        let pending = handle.pending.lock().unwrap();
        assert!(pending.reconcile_all);
        assert!(pending.actions.is_empty());
    }

    #[test]
    fn failed_projection_retires_only_the_overlay_it_attempted() {
        let temp = tempfile::tempdir().unwrap();
        let (handle, worker) = test_worker(temp.path());
        let mut overlay = stale_catalog_row();
        overlay.source_revision = 5;
        handle.publish_overlay(overlay);

        assert!(project_session(&handle, SESSION_ID, 5).unwrap());
        assert!(!handle
            .overlays
            .lock()
            .unwrap()
            .active
            .contains_key(SESSION_ID));

        let mut newer_overlay = stale_catalog_row();
        newer_overlay.source_revision = 6;
        handle.publish_overlay(newer_overlay);
        assert!(project_session(&handle, SESSION_ID, 5).unwrap());
        assert_eq!(
            handle.overlays.lock().unwrap().active[SESSION_ID].source_revision,
            6
        );
        stop_test_worker(&handle, worker);
    }

    #[test]
    fn reconciliation_retires_active_overlays_missing_from_canonical_storage() {
        let temp = tempfile::tempdir().unwrap();
        let (handle, worker) = test_worker(temp.path());
        let mut overlay = stale_catalog_row();
        overlay.source_revision = 5;
        handle.publish_overlay(overlay);

        reconcile_all_sessions(&handle).unwrap();

        assert!(!handle
            .overlays
            .lock()
            .unwrap()
            .active
            .contains_key(SESSION_ID));
        stop_test_worker(&handle, worker);
    }

    #[test]
    fn barrier_waits_for_work_queued_before_it() {
        let temp = tempfile::tempdir().unwrap();
        let (handle, worker) = test_worker(temp.path());
        handle.request_action(SESSION_ID.into(), PendingAction::Remove);
        let completed = handle.enqueue_barrier().unwrap();

        completed.recv_timeout(Duration::from_secs(2)).unwrap();

        let catalog = CatalogReader::open_existing(&handle.catalog_path)
            .unwrap()
            .expect("remove action created catalog");
        assert!(catalog.session(SESSION_ID).unwrap().is_none());
        stop_test_worker(&handle, worker);
    }

    #[test]
    fn work_queued_after_barrier_does_not_delay_it() {
        let temp = tempfile::tempdir().unwrap();
        let blocked_parent = temp.path().join("blocked");
        fs::write(&blocked_parent, b"not a directory").unwrap();
        let (handle, worker) =
            test_worker_with_lock(temp.path(), blocked_parent.join("catalog.lock"));
        let completed = handle.enqueue_barrier().unwrap();
        handle.request_reconciliation();

        completed.recv_timeout(Duration::from_secs(1)).unwrap();

        stop_test_worker(&handle, worker);
    }

    #[test]
    fn failed_reconciliation_retries_without_acknowledging_barrier() {
        let temp = tempfile::tempdir().unwrap();
        let blocked_parent = temp.path().join("blocked");
        fs::write(&blocked_parent, b"not a directory").unwrap();
        let (handle, worker) =
            test_worker_with_lock(temp.path(), blocked_parent.join("catalog.lock"));
        handle.request_reconciliation();
        let completed = handle.enqueue_barrier().unwrap();

        assert!(matches!(
            completed.recv_timeout(Duration::from_millis(150)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        fs::remove_file(&blocked_parent).unwrap();
        completed.recv_timeout(Duration::from_secs(2)).unwrap();

        stop_test_worker(&handle, worker);
    }

    #[test]
    fn shutdown_rejects_new_barriers() {
        let temp = tempfile::tempdir().unwrap();
        let (handle, worker) = test_worker(temp.path());
        handle.pending.lock().unwrap().shutdown = true;

        assert!(handle.enqueue_barrier().is_err());

        handle.signal().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn reconciliation_preserves_tombstones_added_after_its_snapshot() {
        let snapshot = HashSet::from(["gone".to_string(), "present".to_string()]);
        let seen = HashSet::from(["present".to_string()]);
        let mut current =
            HashSet::from(["gone".to_string(), "present".to_string(), "new".to_string()]);

        retain_reconciliation_tombstones(&mut current, &snapshot, &seen);

        assert_eq!(
            current,
            HashSet::from(["present".to_string(), "new".to_string()])
        );
    }

    #[test]
    fn active_overlay_wins_and_delete_overlay_can_be_cancelled() {
        let state = tempfile::tempdir().unwrap();
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        request_reconciliation();
        assert!(wait_for_queued_work(Duration::from_secs(2)));

        let session = canonical_session("current");
        crate::session::create_private_dir_all(&crate::session::sessions_dir()).unwrap();
        let command = crate::session::initial_store_commit_from_session(&session).unwrap();
        let mut writer =
            smelt_store::OwnedLineageWriter::open(crate::session::sessions_dir(), SESSION_ID)
                .unwrap();
        let receipt = writer.commit_session(&command).unwrap();
        writer.release().unwrap();

        let catalog_path = crate::config::state_dir().join("catalog.db");
        let mut catalog = Catalog::open(&catalog_path).unwrap();
        catalog.upsert_available(&stale_catalog_row()).unwrap();
        drop(catalog);
        publish_commit(&command, &receipt, false);

        let page = read_page(&CatalogQuery::default());
        assert_eq!(page.sessions.len(), 1);
        assert_eq!(page.sessions[0].title.as_deref(), Some("current"));
        begin_delete(SESSION_ID);
        assert!(read_page(&CatalogQuery::default()).sessions.is_empty());
        cancel_delete(SESSION_ID);
        assert_eq!(
            read_page(&CatalogQuery::default()).sessions[0]
                .title
                .as_deref(),
            Some("current")
        );
    }

    #[test]
    fn projection_repairs_stale_rows_and_catalog_rebuilds_from_canonical_data() {
        let state = tempfile::tempdir().unwrap();
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let session = canonical_session("canonical");
        let receipt = crate::session::save_result(&session).unwrap();
        assert!(wait_for_queued_work(Duration::from_secs(2)));
        let catalog_path = crate::config::state_dir().join("catalog.db");

        let mut catalog = Catalog::open(&catalog_path).unwrap();
        catalog.remove(SESSION_ID).unwrap();
        catalog.upsert_available(&stale_catalog_row()).unwrap();
        drop(catalog);
        request_projection(SESSION_ID, receipt.current.revision);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let page = read_page(&CatalogQuery::default());
            if page.sessions[0].title.as_deref() == Some("canonical") {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }

        smelt_store::rebuild_catalog(&catalog_path).unwrap();
        request_reconciliation();
        assert!(wait_for_queued_work(Duration::from_secs(2)));
        assert_eq!(read_page(&CatalogQuery::default()).sessions.len(), 1);

        smelt_store::rebuild_catalog(&catalog_path).unwrap();
        fs::write(&catalog_path, b"corrupt catalog").unwrap();
        request_reconciliation();
        assert!(wait_for_queued_work(Duration::from_secs(2)));
        let rebuilt = read_page(&CatalogQuery::default());
        assert_eq!(rebuilt.sessions.len(), 1);
        assert_eq!(rebuilt.sessions[0].title.as_deref(), Some("canonical"));
    }
}
