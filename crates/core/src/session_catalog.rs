use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
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
struct PendingWork {
    actions: HashMap<String, PendingAction>,
    reconcile_all: bool,
    shutdown: bool,
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
    state_root: PathBuf,
    handle: ServiceHandle,
    worker: Option<thread::JoinHandle<()>>,
}

impl ServiceOwner {
    fn spawn(state_root: PathBuf) -> Result<Self, String> {
        crate::session::create_private_dir_all(&state_root)
            .map_err(|error| format!("create session catalog state directory: {error}"))?;
        let sessions_root = state_root.join("sessions");
        let catalog_path = state_root.join("catalog.db");
        let lock_path = state_root.join(".catalog.lock");
        let pending = Arc::new(Mutex::new(PendingWork {
            reconcile_all: true,
            ..PendingWork::default()
        }));
        let overlays = Arc::new(Mutex::new(Overlays::default()));
        let status = Arc::new(Mutex::new(ServiceStatus::default()));
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
            state_root,
            handle,
            worker: Some(worker),
        };
        owner.handle.signal()?;
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

impl ServiceHandle {
    fn signal(&self) -> Result<(), String> {
        match self.wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => Ok(()),
            Err(TrySendError::Disconnected(())) => {
                Err("session catalog worker disconnected".into())
            }
        }
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

fn global_service() -> &'static Mutex<Option<ServiceOwner>> {
    static SERVICE: OnceLock<Mutex<Option<ServiceOwner>>> = OnceLock::new();
    SERVICE.get_or_init(|| Mutex::new(None))
}

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

pub(crate) fn request_reconciliation() {
    if let Some(service) = service() {
        service.request_reconciliation();
    }
}

pub(crate) fn publish_commit(
    command: &smelt_store::SessionCommit,
    receipt: &smelt_store::SaveReceipt,
    schedule_projection: bool,
) {
    let Some(service) = service() else {
        return;
    };
    service.publish_overlay(catalog_session_from_commit(command, receipt));
    if schedule_projection {
        service.request_action(
            receipt.session_id.clone(),
            PendingAction::Project(receipt.current.revision.get()),
        );
    }
}

pub(crate) fn request_projection(id: &str, revision: smelt_store::Revision) {
    let Some(service) = service() else {
        return;
    };
    service.request_action(id.to_string(), PendingAction::Project(revision.get()));
}

pub(crate) fn begin_delete(id: &str) {
    if let Some(service) = service() {
        service.begin_delete(id);
    }
}

pub(crate) fn cancel_delete(id: &str) {
    if let Some(service) = service() {
        service.cancel_delete(id);
    }
}

pub(crate) fn complete_delete(id: &str) {
    if let Some(service) = service() {
        service.request_action(id.to_string(), PendingAction::Remove);
    }
}

pub(crate) fn read_page(query: &CatalogQuery) -> ReadPage {
    let _perf = smelt_perf::perf::begin("session:catalog:query");
    let Some(service) = service() else {
        return ReadPage {
            sessions: Vec::new(),
            next_cursor: None,
            status: ServiceStatus {
                state: ServiceState::Degraded,
                completed_scan_id: 0,
                reconciled_at: None,
                last_error: Some("session catalog service is unavailable".into()),
            },
        };
    };
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
    let mut warning = WarningLimiter::default();
    while wakes.recv().is_ok() {
        loop {
            let (actions, reconcile_all, shutdown) = {
                let mut pending = handle
                    .pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                let actions = std::mem::take(&mut pending.actions);
                let reconcile_all = std::mem::take(&mut pending.reconcile_all);
                (actions, reconcile_all, pending.shutdown)
            };
            if shutdown {
                return;
            }
            if actions.is_empty() && !reconcile_all {
                break;
            }

            if reconcile_all {
                handle.set_reconciling();
                match reconcile_all_sessions(&handle) {
                    Ok(()) => {}
                    Err(error) => {
                        warning.warn(&error);
                        handle.set_degraded(error);
                        let mut pending = handle
                            .pending
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        pending.reconcile_all = true;
                        for (id, action) in actions {
                            pending.actions.entry(id).or_insert(action);
                        }
                        break;
                    }
                }
                continue;
            }

            let mut needs_reconciliation = false;
            for (id, action) in actions {
                let result = match action {
                    PendingAction::Project(minimum_revision) => {
                        project_session(&handle, &id, minimum_revision)
                    }
                    PendingAction::Remove => remove_session(&handle, &id),
                };
                if let Err(error) = result {
                    warning.warn(&error);
                    handle.set_degraded(error);
                    needs_reconciliation = true;
                }
            }
            if needs_reconciliation {
                handle
                    .pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .reconcile_all = true;
            }
        }
    }
}

fn project_session(handle: &ServiceHandle, id: &str, minimum_revision: u64) -> Result<(), String> {
    let _perf = smelt_perf::perf::begin("session:catalog:project");
    smelt_perf::perf::record_value(
        "session:catalog:project_requested_revision",
        minimum_revision,
    );
    let mut projected = load_projection(&handle.sessions_root, id);
    if projected
        .as_ref()
        .is_ok_and(|session| session.source_revision < minimum_revision)
    {
        smelt_perf::perf::record_value("session:catalog:post_publication_retry", 1);
        projected = load_projection(&handle.sessions_root, id);
    }

    let _lock = CatalogReconcileLock::acquire(&handle.lock_path)
        .map_err(|error| format!("lock session catalog projection: {error}"))?;
    let mut catalog = Catalog::open(&handle.catalog_path)
        .map_err(|error| format!("open session catalog for projection: {error}"))?;
    match projected {
        Ok(session) => {
            let revision = session.source_revision;
            smelt_perf::perf::record_value(
                "session:catalog:projection_revision_lag",
                minimum_revision.saturating_sub(revision),
            );
            catalog
                .upsert_available(&session)
                .map_err(|error| format!("project session {id}: {error}"))?;
            handle.clear_projected(id, revision);
            if revision < minimum_revision {
                handle
                    .pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .reconcile_all = true;
            }
        }
        Err(error) => {
            catalog
                .upsert_unavailable(id, &error.kind, &error.summary)
                .map_err(|catalog_error| {
                    format!("record unavailable session {id}: {catalog_error}")
                })?;
        }
    }
    Ok(())
}

fn remove_session(handle: &ServiceHandle, id: &str) -> Result<(), String> {
    let _lock = CatalogReconcileLock::acquire(&handle.lock_path)
        .map_err(|error| format!("lock session catalog removal: {error}"))?;
    let mut catalog = Catalog::open(&handle.catalog_path)
        .map_err(|error| format!("open session catalog for removal: {error}"))?;
    catalog
        .remove(id)
        .map_err(|error| format!("remove session {id} from catalog: {error}"))?;
    handle.clear_removed(id);
    Ok(())
}

fn reconcile_all_sessions(handle: &ServiceHandle) -> Result<(), String> {
    let _lock = CatalogReconcileLock::acquire(&handle.lock_path)
        .map_err(|error| format!("lock session catalog reconciliation: {error}"))?;
    let mut catalog = match Catalog::open(&handle.catalog_path) {
        Ok(catalog) => catalog,
        Err(open_error) => {
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
    let entries = session_directory_entries(&handle.sessions_root)?;
    let tombstones = handle
        .overlays
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .deleted
        .clone();
    let mut seen_tombstones = HashSet::with_capacity(tombstones.len());
    let mut candidates = 0_u64;
    let mut available = 0_u64;
    let mut unavailable = 0_u64;
    if let Some(entries) = entries {
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "enumerate session directory in {}: {error}",
                    handle.sessions_root.display()
                )
            })?;
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if crate::session_id::SessionId::parse(&id).is_err() {
                continue;
            }
            candidates += 1;
            if tombstones.contains(&id) {
                seen_tombstones.insert(id.clone());
            }
            match load_projection(&handle.sessions_root, &id) {
                Ok(session) => {
                    available += 1;
                    let revision = session.source_revision;
                    catalog
                        .upsert_available_for_reconciliation(&session, scan_id)
                        .map_err(|error| format!("reconcile session {id}: {error}"))?;
                    handle.clear_projected(&id, revision);
                }
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
                            format!("reconcile unavailable session {id}: {catalog_error}")
                        })?;
                }
            }
        }
    }
    smelt_perf::perf::record_value("session:catalog:reconcile_candidates", candidates);
    smelt_perf::perf::record_value("session:catalog:reconcile_available", available);
    smelt_perf::perf::record_value("session:catalog:reconcile_unavailable", unavailable);
    let reconciled_at = i64::try_from(crate::session::now_ms()).unwrap_or(i64::MAX);
    let deleted = catalog
        .complete_scan(scan_id, reconciled_at)
        .map_err(|error| format!("complete session catalog scan {scan_id}: {error}"))?;
    smelt_perf::perf::record_value("session:catalog:reconcile_deleted", deleted as u64);

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
    Ok(())
}

fn retain_reconciliation_tombstones(
    current: &mut HashSet<String>,
    snapshot: &HashSet<String>,
    seen: &HashSet<String>,
) {
    current.retain(|id| !snapshot.contains(id) || seen.contains(id));
}

fn session_directory_entries(root: &Path) -> Result<Option<fs::ReadDir>, String> {
    crate::session_store::reject_symlink(root, "reconcile catalog")
        .map_err(|error| error.to_string())?;
    match fs::read_dir(root) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "enumerate session directories in {}: {error}",
            root.display()
        )),
    }
}

struct ProjectionError {
    kind: String,
    summary: String,
}

fn load_projection(root: &Path, id: &str) -> Result<CatalogSession, ProjectionError> {
    let session_dir = root.join(id);
    let db_path = session_dir.join("session.db");
    if let Err(error) = crate::session_store::reject_symlink(&session_dir, "project catalog") {
        return Err(projection_error(error));
    }
    if let Err(error) = crate::session_store::reject_symlink(&db_path, "project catalog") {
        return Err(projection_error(error));
    }
    if !db_path.is_file() {
        return Err(ProjectionError {
            kind: "missing_database".into(),
            summary: format!("session {id} has no sqlite database"),
        });
    }
    let reader = smelt_store::SessionReader::open_database(&db_path).map_err(|error| {
        projection_error(crate::session_store::store_error(
            "open for catalog projection",
            &db_path,
            error,
        ))
    })?;
    let stored = reader
        .stored_session()
        .map_err(|error| {
            projection_error(crate::session_store::store_error(
                "read for catalog projection",
                &db_path,
                error,
            ))
        })?
        .ok_or_else(|| ProjectionError {
            kind: "corrupt".into(),
            summary: format!("session {id} has no canonical metadata"),
        })?;
    if stored.identity.id != id {
        return Err(ProjectionError {
            kind: "corrupt".into(),
            summary: format!(
                "persisted session id {} does not match directory {id}",
                stored.identity.id
            ),
        });
    }
    let text_bytes = reader.history_text_bytes().map_err(|error| {
        projection_error(crate::session_store::store_error(
            "read history size for catalog projection",
            &db_path,
            error,
        ))
    })?;
    let metadata = stored.metadata;
    Ok(CatalogSession {
        id: stored.identity.id,
        title: metadata.title,
        slug: metadata.slug,
        first_user_message: metadata.first_user_message,
        cwd: metadata.cwd,
        mode: metadata.mode,
        reasoning_effort: metadata.reasoning_effort,
        model: metadata.model,
        fast_mode: metadata.fast_mode,
        parent_id: stored.identity.parent_id,
        context_tokens: metadata.display_context_tokens.or(metadata.context_tokens),
        history_len: Some(stored.head.history_len.get()),
        text_bytes: Some(text_bytes),
        created_at: stored.identity.created_at,
        updated_at: metadata.updated_at,
        source_revision: stored.head.revision.get(),
        availability: CatalogAvailability::Available,
        error_kind: None,
        error_summary: None,
        last_seen_scan: 0,
    })
}

fn projection_error(error: crate::session_store::SessionStoreError) -> ProjectionError {
    ProjectionError {
        kind: error.code().to_string(),
        summary: error.to_string(),
    }
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
pub(crate) fn wait_until_ready(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let page = read_page(&CatalogQuery {
            limit: 1,
            ..CatalogQuery::default()
        });
        if page.status.state == ServiceState::Ready {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "1000000000000000000000000000000000000000000000000000000000000001";

    fn canonical_session(title: &str) -> crate::session::Session {
        let mut session = crate::session::Session::new(1, PathBuf::from("/workspace"));
        session.id = SESSION_ID.into();
        session.title = Some(title.into());
        session.updated_at_ms = 1_700_000_000_000;
        session
    }

    fn stale_catalog_row() -> CatalogSession {
        CatalogSession {
            id: SESSION_ID.into(),
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
        assert!(wait_until_ready(Duration::from_secs(2)));

        let session = canonical_session("current");
        crate::session::create_private_dir_all(&crate::session::sessions_dir()).unwrap();
        let command = crate::session::initial_store_commit_from_session(&session).unwrap();
        let dir = crate::session::sessions_dir().join(SESSION_ID);
        let mut db = smelt_store::SessionDb::open(dir.join("session.db")).unwrap();
        let receipt = db.apply_session_commit(&command).unwrap();
        drop(db);

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
        assert!(wait_until_ready(Duration::from_secs(2)));
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
        assert!(wait_until_ready(Duration::from_secs(2)));
        assert_eq!(read_page(&CatalogQuery::default()).sessions.len(), 1);

        smelt_store::rebuild_catalog(&catalog_path).unwrap();
        fs::write(&catalog_path, b"corrupt catalog").unwrap();
        request_reconciliation();
        assert!(wait_until_ready(Duration::from_secs(2)));
        let rebuilt = read_page(&CatalogQuery::default());
        assert_eq!(rebuilt.sessions.len(), 1);
        assert_eq!(rebuilt.sessions[0].title.as_deref(), Some("canonical"));
    }
}
