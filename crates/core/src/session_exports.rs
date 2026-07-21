//! COMPAT(session-derived-sidecar-exports): deprecated alpha compatibility exporter.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const MAX_PENDING_EXPORTS: usize = 1_024;
const QUIET_PERIOD: Duration = Duration::from_millis(75);
const LOCK_RETRY: Duration = Duration::from_millis(10);
const SHUTDOWN_WAIT: Duration = Duration::from_millis(250);
const WARNING_INTERVAL: Duration = Duration::from_secs(60);
const MAX_META_REVISION_BYTES: u64 = 1024 * 1024;
const MAX_CONTENT_HEADER_BYTES: u64 = 256;
const MAX_TEMP_CREATE_ATTEMPTS: usize = 64;
pub(crate) const EXPORT_FORMAT_VERSION: u32 = 1;

static TEMP_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExportOutcome {
    Missing,
    Current { revision: u64 },
    Written { revision: u64 },
    Cancelled,
    SourceBehind { actual: u64 },
    Superseded { source: u64, exported: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExportFailureClass {
    Io,
    Permission,
    InvalidTarget,
    SourceBehind,
}

impl ExportFailureClass {
    fn from_io(error: &std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            Self::Permission
        } else {
            Self::Io
        }
    }

    const fn metric(self) -> &'static str {
        match self {
            Self::Io => "session:compat_export:failures:io",
            Self::Permission => "session:compat_export:failures:permission",
            Self::InvalidTarget => "session:compat_export:failures:invalid_target",
            Self::SourceBehind => "session:compat_export:failures:source_behind",
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExportError {
    class: ExportFailureClass,
    message: String,
}

impl ExportError {
    fn new(class: ExportFailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }

    fn io(context: impl std::fmt::Display, error: std::io::Error) -> Self {
        Self::new(
            ExportFailureClass::from_io(&error),
            format!("{context}: {error}"),
        )
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(ExportFailureClass::InvalidTarget, message)
    }
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExportError {}

#[derive(Clone, Copy, Debug)]
struct ExportRequest {
    revision: u64,
    retries: u8,
}

#[derive(Default)]
struct PendingWork {
    exports: BTreeMap<String, ExportRequest>,
    reconcile_all: bool,
    shutdown: bool,
}

#[derive(Clone)]
struct ExporterHandle {
    sessions_root: PathBuf,
    pending: Arc<Mutex<PendingWork>>,
    wake: SyncSender<()>,
}

struct ExporterOwner {
    state_root: PathBuf,
    handle: ExporterHandle,
    worker: Option<thread::JoinHandle<()>>,
    done: Receiver<()>,
}

impl ExporterOwner {
    fn spawn(state_root: PathBuf) -> Result<Self, String> {
        crate::session::create_private_dir_all(&state_root)
            .map_err(|error| format!("create compatibility exporter state directory: {error}"))?;
        let pending = Arc::new(Mutex::new(PendingWork::default()));
        let (wake, wakes) = mpsc::sync_channel(1);
        let (finished, done) = mpsc::sync_channel(1);
        let handle = ExporterHandle {
            sessions_root: state_root.join("sessions"),
            pending,
            wake,
        };
        let worker_handle = handle.clone();
        let worker = thread::Builder::new()
            .name("smelt-compat-export".into())
            .spawn(move || {
                export_worker(worker_handle, wakes);
                let _ = finished.send(());
            })
            .map_err(|error| format!("spawn compatibility exporter: {error}"))?;
        Ok(Self {
            state_root,
            handle,
            worker: Some(worker),
            done,
        })
    }
}

impl Drop for ExporterOwner {
    fn drop(&mut self) {
        self.handle
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .shutdown = true;
        let _ = self.handle.wake.try_send(());
        if self.done.recv_timeout(SHUTDOWN_WAIT).is_ok() {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        } else {
            self.worker.take();
            smelt_perf::perf::record_value("session:compat_export:shutdown_detached", 1);
        }
    }
}

impl ExporterHandle {
    fn request(&self, id: String, revision: u64) -> Result<(), String> {
        let overflowed = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if pending.reconcile_all {
                false
            } else if let Some(current) = pending.exports.get_mut(&id) {
                if revision > current.revision {
                    *current = ExportRequest {
                        revision,
                        retries: 0,
                    };
                }
                smelt_perf::perf::record_value("session:compat_export:coalesced", 1);
                false
            } else if pending.exports.len() >= MAX_PENDING_EXPORTS {
                pending.exports.clear();
                pending.reconcile_all = true;
                true
            } else {
                pending.exports.insert(
                    id,
                    ExportRequest {
                        revision,
                        retries: 0,
                    },
                );
                false
            }
        };
        if overflowed {
            smelt_perf::perf::record_value("session:compat_export:queue_overflow", 1);
        }
        let depth = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .exports
            .len();
        smelt_perf::perf::record_value(
            "session:compat_export:pending_distinct_sessions",
            depth as u64,
        );
        self.signal()
    }

    fn signal(&self) -> Result<(), String> {
        match self.wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => Ok(()),
            Err(TrySendError::Disconnected(())) => {
                Err("compatibility export worker disconnected".into())
            }
        }
    }

    fn is_shutdown(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .shutdown
    }

    fn should_cancel(&self, id: &str, source_revision: u64) -> bool {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        pending.shutdown
            || pending
                .exports
                .get(id)
                .is_some_and(|request| request.revision > source_revision)
    }

    fn retry_source_behind(&self, id: String, request: ExportRequest) -> bool {
        if request.retries != 0 {
            return false;
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if pending.shutdown || pending.reconcile_all {
            return false;
        }
        pending.exports.entry(id).or_insert(ExportRequest {
            revision: request.revision,
            retries: 1,
        });
        true
    }

    fn resume_reconciliation(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !pending.shutdown {
            pending.reconcile_all = true;
        }
    }
}

fn global_exporter() -> &'static Mutex<Option<ExporterOwner>> {
    static EXPORTER: OnceLock<Mutex<Option<ExporterOwner>>> = OnceLock::new();
    EXPORTER.get_or_init(|| Mutex::new(None))
}

fn exporter() -> Result<ExporterHandle, String> {
    let state_root = crate::config::state_dir();
    let mut exporter = global_exporter()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if exporter
        .as_ref()
        .is_some_and(|owner| owner.state_root != state_root)
    {
        exporter.take();
    }
    if exporter.is_none() {
        *exporter = Some(ExporterOwner::spawn(state_root)?);
    }
    Ok(exporter
        .as_ref()
        .expect("compatibility exporter")
        .handle
        .clone())
}

pub(crate) fn request(id: &str, revision: smelt_store::Revision) {
    if let Err(error) = crate::session_id::SessionId::parse(id) {
        global_warning().warn(
            ExportFailureClass::InvalidTarget,
            &format!("invalid compatibility export session id: {error}"),
        );
        return;
    }
    smelt_perf::perf::record_value("session:compat_export:requested_revision", revision.get());
    if let Err(error) =
        exporter().and_then(|exporter| exporter.request(id.to_string(), revision.get()))
    {
        global_warning().warn(ExportFailureClass::Io, &error);
    }
}

fn global_warning() -> std::sync::MutexGuard<'static, WarningLimiter> {
    static WARNING: OnceLock<Mutex<WarningLimiter>> = OnceLock::new();
    WARNING
        .get_or_init(|| Mutex::new(WarningLimiter::default()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn export_worker(handle: ExporterHandle, wakes: Receiver<()>) {
    let mut warnings = WarningLimiter::default();
    let mut last_exported_id = None;
    while wakes.recv().is_ok() {
        if !wait_for_quiet(&handle, &wakes) {
            return;
        }
        loop {
            let work = {
                let mut pending = handle
                    .pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if pending.shutdown {
                    return;
                }
                if let Some((id, request)) =
                    pop_next_export(&mut pending.exports, &mut last_exported_id)
                {
                    Some(WorkerAction::Export { id, request })
                } else if std::mem::take(&mut pending.reconcile_all) {
                    Some(WorkerAction::Reconcile)
                } else {
                    None
                }
            };
            let Some(work) = work else {
                break;
            };
            match work {
                WorkerAction::Reconcile => reconcile_all(&handle, &mut warnings),
                WorkerAction::Export { id, request } => {
                    let dir = handle.sessions_root.join(&id);
                    let outcome = export_compatibility_files(&dir, request.revision, |source| {
                        handle.should_cancel(&id, source)
                    });
                    match outcome {
                        Ok(ExportOutcome::Cancelled) => break,
                        Ok(ExportOutcome::SourceBehind { actual }) => {
                            smelt_perf::perf::record_value(
                                "session:compat_export:source_revision_lag",
                                request.revision.saturating_sub(actual),
                            );
                            if handle.retry_source_behind(id.clone(), request) {
                                let _ = handle.signal();
                                break;
                            }
                            warnings.warn(
                                ExportFailureClass::SourceBehind,
                                &format!(
                                    "session {id} remained at revision {actual} below requested compatibility export revision {}",
                                    request.revision
                                ),
                            );
                        }
                        Ok(
                            ExportOutcome::Current { revision }
                            | ExportOutcome::Written { revision },
                        ) => {
                            smelt_perf::perf::record_value(
                                "session:compat_export:source_revision_lag",
                                request.revision.saturating_sub(revision),
                            );
                        }
                        Ok(ExportOutcome::Superseded { source, .. }) => {
                            smelt_perf::perf::record_value(
                                "session:compat_export:source_revision_lag",
                                request.revision.saturating_sub(source),
                            );
                        }
                        Ok(ExportOutcome::Missing) => {}
                        Err(error) => warnings.warn(error.class, &format!("session {id}: {error}")),
                    }
                }
            }
        }
    }
}

fn pop_next_export(
    exports: &mut BTreeMap<String, ExportRequest>,
    last_exported_id: &mut Option<String>,
) -> Option<(String, ExportRequest)> {
    let id = exports
        .keys()
        .find(|id| {
            last_exported_id
                .as_ref()
                .is_none_or(|last| id.as_str() > last.as_str())
        })
        .cloned()
        .or_else(|| exports.first_key_value().map(|(id, _)| id.clone()))?;
    let request = exports
        .remove(&id)
        .expect("selected compatibility export is pending");
    *last_exported_id = Some(id.clone());
    Some((id, request))
}

enum WorkerAction {
    Export { id: String, request: ExportRequest },
    Reconcile,
}

fn wait_for_quiet(handle: &ExporterHandle, wakes: &Receiver<()>) -> bool {
    loop {
        if handle.is_shutdown() {
            return false;
        }
        match wakes.recv_timeout(QUIET_PERIOD) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn reconcile_all(handle: &ExporterHandle, warnings: &mut WarningLimiter) {
    let entries = match fs::read_dir(&handle.sessions_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            warnings.warn(
                ExportFailureClass::from_io(&error),
                &format!(
                    "enumerate compatibility export sessions in {}: {error}",
                    handle.sessions_root.display()
                ),
            );
            return;
        }
    };
    let mut scanned = 0_u64;
    for entry in entries {
        if handle.is_shutdown() {
            return;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.warn(
                    ExportFailureClass::from_io(&error),
                    &format!("enumerate compatibility export session: {error}"),
                );
                return;
            }
        };
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if crate::session_id::SessionId::parse(&id).is_err() {
            continue;
        }
        scanned = scanned.saturating_add(1);
        match export_compatibility_files(&entry.path(), 0, |source| {
            handle.should_cancel(&id, source)
        }) {
            Ok(ExportOutcome::Cancelled) => {
                handle.resume_reconciliation();
                return;
            }
            Ok(_) => {}
            Err(error) => {
                warnings.warn(error.class, &format!("session {id}: {error}"));
            }
        }
    }
    smelt_perf::perf::record_value("session:compat_export:reconcile_scanned", scanned);
}

// COMPAT(session-derived-sidecar-exports): revision-pinned, streamed alpha exports.
pub(crate) fn export_compatibility_files(
    session_dir: &Path,
    minimum_revision: u64,
    mut should_cancel: impl FnMut(u64) -> bool,
) -> Result<ExportOutcome, ExportError> {
    let _duration = smelt_perf::perf::begin_value_ms("session:compat_export:duration_ms");
    crate::session_store::reject_symlink(session_dir, "export compatibility files")
        .map_err(|error| ExportError::invalid(error.to_string()))?;
    let db_path = session_dir.join("session.db");
    crate::session_store::reject_symlink(&db_path, "export compatibility files")
        .map_err(|error| ExportError::invalid(error.to_string()))?;
    if !db_path.is_file() {
        return Ok(ExportOutcome::Missing);
    }

    let Some(_lock) =
        CompatibilityExportLock::acquire(session_dir, || should_cancel(minimum_revision))?
    else {
        return Ok(ExportOutcome::Cancelled);
    };
    let reader = smelt_store::SessionReader::open_database(&db_path).map_err(|error| {
        ExportError::new(
            ExportFailureClass::Io,
            format!("open canonical database for compatibility export: {error}"),
        )
    })?;
    let Some(snapshot) = reader.compatibility_export_snapshot().map_err(|error| {
        ExportError::new(
            ExportFailureClass::Io,
            format!("open compatibility export snapshot: {error}"),
        )
    })?
    else {
        return Ok(ExportOutcome::Missing);
    };
    let metadata = snapshot.metadata().clone();
    let source_revision = metadata.revision;
    if source_revision < minimum_revision {
        return Ok(ExportOutcome::SourceBehind {
            actual: source_revision,
        });
    }
    if should_cancel(source_revision) {
        return Ok(ExportOutcome::Cancelled);
    }

    let meta_path = session_dir.join("meta.json");
    let content_path = session_dir.join("content.txt");
    let meta_revision = exported_revision(&meta_path, ExportKind::Metadata)?;
    let content_revision = exported_revision(&content_path, ExportKind::Content)?;
    if let (ExistingRevision::Valid(meta), ExistingRevision::Valid(content)) =
        (meta_revision, content_revision)
    {
        if meta == content && meta > source_revision {
            smelt_perf::perf::record_value("session:compat_export:stale_write_skips", 1);
            return Ok(ExportOutcome::Superseded {
                source: source_revision,
                exported: meta,
            });
        }
    }
    if meta_revision == ExistingRevision::Valid(source_revision)
        && content_revision == ExistingRevision::Valid(source_revision)
    {
        return Ok(ExportOutcome::Current {
            revision: source_revision,
        });
    }

    let metadata_duration =
        smelt_perf::perf::begin_value_ms("session:compat_export:metadata_duration_ms");
    let meta_json = derived_meta_json(&metadata)?;
    let Some(meta_file) = prepare_atomic_write(&meta_path, |file| {
        if should_cancel(source_revision) {
            return Ok(false);
        }
        file.write_all(&meta_json)?;
        Ok(!should_cancel(source_revision))
    })?
    else {
        return Ok(ExportOutcome::Cancelled);
    };
    drop(metadata_duration);

    let content_duration =
        smelt_perf::perf::begin_value_ms("session:compat_export:content_duration_ms");
    let mut content_bytes = 0_u64;
    let Some(content_file) = prepare_atomic_write(&content_path, |file| {
        let header = format!("# smelt-revision:{source_revision}\n");
        file.write_all(header.as_bytes())?;
        content_bytes = content_bytes.saturating_add(header.len() as u64);
        let mut writer = CountingWriter::new(file, &mut content_bytes);
        let completed = snapshot
            .write_search_blob_cancellable(&mut writer, || should_cancel(source_revision))
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(completed && !should_cancel(source_revision))
    })?
    else {
        return Ok(ExportOutcome::Cancelled);
    };
    drop(content_duration);
    if should_cancel(source_revision) {
        return Ok(ExportOutcome::Cancelled);
    }
    snapshot.finish().map_err(|error| {
        ExportError::new(
            ExportFailureClass::Io,
            format!("finish compatibility export snapshot: {error}"),
        )
    })?;

    let export_dir = meta_file.parent().to_path_buf();
    let meta_bytes = meta_file.install()?;
    content_file.install()?;
    sync_directory(&export_dir)
        .map_err(|error| ExportError::io("sync compatibility export directory", error))?;

    smelt_perf::perf::record_value("session:compat_export:metadata_bytes", meta_bytes);
    smelt_perf::perf::record_value("session:compat_export:content_bytes", content_bytes);
    smelt_perf::perf::record_value("session:compat_export:source_revision", source_revision);
    Ok(ExportOutcome::Written {
        revision: source_revision,
    })
}

fn derived_meta_json(metadata: &smelt_store::SessionMeta) -> Result<Vec<u8>, ExportError> {
    let mut value =
        serde_json::to_value(metadata).map_err(|error| ExportError::invalid(error.to_string()))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| ExportError::invalid("session metadata must serialize as an object"))?;
    object.insert("cache_format_version".into(), EXPORT_FORMAT_VERSION.into());
    object.insert("source_revision".into(), metadata.revision.into());
    serde_json::to_vec_pretty(&value).map_err(|error| ExportError::invalid(error.to_string()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExistingRevision {
    Missing,
    Malformed,
    Valid(u64),
}

pub(crate) fn compatibility_export_status(
    session_dir: &Path,
) -> Result<crate::session::CompatibilityExportStatus, String> {
    let revision = |path: &Path,
                    kind|
     -> Result<crate::session::CompatibilityExportRevision, String> {
        Ok(
            match exported_revision(path, kind).map_err(|error| error.to_string())? {
                ExistingRevision::Missing => crate::session::CompatibilityExportRevision::Missing,
                ExistingRevision::Malformed => {
                    crate::session::CompatibilityExportRevision::Malformed
                }
                ExistingRevision::Valid(source_revision) => {
                    crate::session::CompatibilityExportRevision::Valid { source_revision }
                }
            },
        )
    };
    Ok(crate::session::CompatibilityExportStatus {
        metadata: revision(&session_dir.join("meta.json"), ExportKind::Metadata)?,
        content: revision(&session_dir.join("content.txt"), ExportKind::Content)?,
    })
}

#[derive(Clone, Copy)]
enum ExportKind {
    Metadata,
    Content,
}

// COMPAT(session-derived-sidecar-exports): target reads only prevent stale replacement.
fn exported_revision(path: &Path, kind: ExportKind) -> Result<ExistingRevision, ExportError> {
    crate::session_store::reject_symlink(path, "inspect compatibility export")
        .map_err(|error| ExportError::invalid(error.to_string()))?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExistingRevision::Missing);
        }
        Err(error) => return Err(ExportError::io(format!("read {}", path.display()), error)),
    };
    let revision = match kind {
        ExportKind::Metadata => {
            let mut bytes = Vec::new();
            file.take(MAX_META_REVISION_BYTES)
                .read_to_end(&mut bytes)
                .map_err(|error| ExportError::io(format!("read {}", path.display()), error))?;
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|value| {
                    let source_revision = value.get("source_revision")?.as_u64()?;
                    let format_version = value.get("cache_format_version")?.as_u64()?;
                    let metadata =
                        serde_json::from_value::<smelt_store::SessionMeta>(value).ok()?;
                    (format_version == u64::from(EXPORT_FORMAT_VERSION)
                        && metadata.revision == source_revision)
                        .then_some(source_revision)
                })
        }
        ExportKind::Content => {
            let mut header = String::new();
            BufReader::new(file.take(MAX_CONTENT_HEADER_BYTES))
                .read_line(&mut header)
                .map_err(|error| ExportError::io(format!("read {}", path.display()), error))?;
            header
                .strip_prefix("# smelt-revision:")
                .and_then(|value| value.trim_end().parse::<u64>().ok())
        }
    };
    Ok(revision.map_or(ExistingRevision::Malformed, ExistingRevision::Valid))
}

struct CompatibilityExportLock {
    file: File,
}

impl CompatibilityExportLock {
    fn acquire(
        session_dir: &Path,
        mut should_cancel: impl FnMut() -> bool,
    ) -> Result<Option<Self>, ExportError> {
        let path = session_dir.join(".compat-export.lock");
        crate::session_store::reject_symlink(&path, "lock compatibility export")
            .map_err(|error| ExportError::invalid(error.to_string()))?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path).map_err(|error| {
            ExportError::io(
                format!("open compatibility export lock {}", path.display()),
                error,
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| ExportError::io("secure compatibility export lock", error))?;
        }
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some(Self { file })),
                Err(fs::TryLockError::WouldBlock) if should_cancel() => return Ok(None),
                Err(fs::TryLockError::WouldBlock) => thread::sleep(LOCK_RETRY),
                Err(fs::TryLockError::Error(error)) => {
                    return Err(ExportError::io("lock compatibility export", error));
                }
            }
        }
    }
}

impl Drop for CompatibilityExportLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

struct PreparedAtomicWrite {
    temporary: PathBuf,
    target: PathBuf,
    bytes: u64,
    installed: bool,
}

impl PreparedAtomicWrite {
    fn parent(&self) -> &Path {
        self.target
            .parent()
            .expect("prepared compatibility export has a parent")
    }

    fn install(mut self) -> Result<u64, ExportError> {
        crate::session_store::reject_symlink(&self.target, "install compatibility export")
            .map_err(|error| ExportError::invalid(error.to_string()))?;
        fs::rename(&self.temporary, &self.target).map_err(|error| {
            ExportError::io(
                format!(
                    "replace compatibility export {} from {}",
                    self.target.display(),
                    self.temporary.display()
                ),
                error,
            )
        })?;
        self.installed = true;
        Ok(self.bytes)
    }
}

impl Drop for PreparedAtomicWrite {
    fn drop(&mut self) {
        if !self.installed {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

fn prepare_atomic_write(
    path: &Path,
    write_contents: impl FnOnce(&mut File) -> std::io::Result<bool>,
) -> Result<Option<PreparedAtomicWrite>, ExportError> {
    let dir = path.parent().ok_or_else(|| {
        ExportError::invalid(format!(
            "compatibility export has no parent: {}",
            path.display()
        ))
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ExportError::invalid(format!(
                "compatibility export has no valid name: {}",
                path.display()
            ))
        })?;
    crate::session_store::reject_symlink(dir, "write compatibility export")
        .map_err(|error| ExportError::invalid(error.to_string()))?;
    crate::session_store::reject_symlink(path, "write compatibility export")
        .map_err(|error| ExportError::invalid(error.to_string()))?;
    let (temporary, mut file) = create_atomic_temp(dir, name)?;
    let result = (|| {
        let complete = write_contents(&mut file)
            .map_err(|error| ExportError::io(format!("write {}", temporary.display()), error))?;
        if !complete {
            return Ok(None);
        }
        file.sync_all()
            .map_err(|error| ExportError::io(format!("sync {}", temporary.display()), error))?;
        let bytes = file
            .metadata()
            .map_err(|error| ExportError::io(format!("inspect {}", temporary.display()), error))?
            .len();
        Ok(Some(PreparedAtomicWrite {
            temporary: temporary.clone(),
            target: path.to_path_buf(),
            bytes,
            installed: false,
        }))
    })();
    if result.is_err() || matches!(result, Ok(None)) {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_atomic_temp(dir: &Path, name: &str) -> Result<(PathBuf, File), ExportError> {
    for _ in 0..MAX_TEMP_CREATE_ATTEMPTS {
        let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".{name}.{}-{nonce}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(ExportError::io(format!("create {}", path.display()), error));
            }
        }
    }
    Err(ExportError::new(
        ExportFailureClass::Io,
        format!(
            "create compatibility export temporary file in {} after {MAX_TEMP_CREATE_ATTEMPTS} attempts",
            dir.display()
        ),
    ))
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

struct CountingWriter<'a, W> {
    writer: W,
    bytes: &'a mut u64,
}

impl<'a, W> CountingWriter<'a, W> {
    fn new(writer: W, bytes: &'a mut u64) -> Self {
        Self { writer, bytes }
    }
}

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.writer.write(buffer)?;
        *self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

#[derive(Default)]
struct WarningLimiter {
    last_warning: Option<Instant>,
}

impl WarningLimiter {
    fn warn(&mut self, class: ExportFailureClass, message: &str) {
        let now = Instant::now();
        let should_warn = self
            .last_warning
            .is_none_or(|at| now.duration_since(at) >= WARNING_INTERVAL);
        if should_warn {
            eprintln!("smelt: compatibility export warning: {message}");
            self.last_warning = Some(now);
        }
        smelt_perf::perf::record_value("session:compat_export:failures", 1);
        smelt_perf::perf::record_value(class.metric(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "1000000000000000000000000000000000000000000000000000000000000001";
    const ATOMIC_CRASH_ROLE: &str = "SMELT_COMPAT_ATOMIC_CRASH_ROLE";
    const ATOMIC_CRASH_TARGET: &str = "SMELT_COMPAT_ATOMIC_CRASH_TARGET";

    fn canonical_fixture(state_root: &Path, text: &str) -> (PathBuf, smelt_store::SaveReceipt) {
        let session_dir = state_root.join("sessions").join(SESSION_ID);
        fs::create_dir_all(&session_dir).unwrap();
        let mut session = crate::session::Session::new(1, PathBuf::from("/workspace"));
        session.id = SESSION_ID.into();
        session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(text)));
        let command = crate::session::initial_store_commit_from_session(&session).unwrap();
        let mut db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
        let receipt = db.apply_session_commit(&command).unwrap();
        (session_dir, receipt)
    }

    fn revisions(session_dir: &Path) -> (ExistingRevision, ExistingRevision) {
        (
            exported_revision(&session_dir.join("meta.json"), ExportKind::Metadata).unwrap(),
            exported_revision(&session_dir.join("content.txt"), ExportKind::Content).unwrap(),
        )
    }

    fn append_second_revision(
        session_dir: &Path,
        first: smelt_store::SaveReceipt,
    ) -> smelt_store::SaveReceipt {
        let stored = smelt_store::SessionReader::open_existing(session_dir)
            .unwrap()
            .stored_session()
            .unwrap()
            .unwrap();
        let mut session = crate::session::Session::new(1, PathBuf::from("/workspace"));
        session.id = SESSION_ID.into();
        session.created_at_ms = u64::try_from(stored.identity.created_at).unwrap();
        session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(
                "first",
            )));
        session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(
                "second",
            )));
        let command =
            crate::session::store_commit_from_session(&session, first.current, 1).unwrap();
        let mut db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
        db.apply_session_commit(&command).unwrap()
    }

    #[test]
    fn compatibility_atomic_write_crash_probe() {
        let Ok(role) = std::env::var(ATOMIC_CRASH_ROLE) else {
            return;
        };
        let target = PathBuf::from(
            std::env::var_os(ATOMIC_CRASH_TARGET).expect("compatibility crash target"),
        );
        let prepared = prepare_atomic_write(&target, |file| {
            file.write_all(b"new-complete-revision")?;
            Ok(true)
        })
        .unwrap()
        .unwrap();
        match role.as_str() {
            "before-rename" => {}
            "after-rename" => {
                prepared.install().unwrap();
            }
            other => panic!("unknown compatibility atomic crash role {other}"),
        }
        std::process::abort();
    }

    #[test]
    fn atomic_write_crashes_leave_the_old_or_new_complete_revision() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("content.txt");
        for (role, expected) in [
            ("before-rename", "old-complete-revision"),
            ("after-rename", "new-complete-revision"),
        ] {
            fs::write(&target, b"old-complete-revision").unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("session_exports::tests::compatibility_atomic_write_crash_probe")
                .arg("--nocapture")
                .env(ATOMIC_CRASH_ROLE, role)
                .env(ATOMIC_CRASH_TARGET, &target)
                .status()
                .unwrap();
            assert!(!status.success(), "crash probe unexpectedly succeeded");
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(status.signal(), Some(libc::SIGABRT));
            }
            assert_eq!(fs::read_to_string(&target).unwrap(), expected);
        }
    }

    #[test]
    fn compatibility_status_classifies_missing_malformed_and_valid_exports() {
        let state = tempfile::tempdir().unwrap();
        let (session_dir, receipt) = canonical_fixture(state.path(), "status");
        assert_eq!(
            compatibility_export_status(&session_dir).unwrap(),
            crate::session::CompatibilityExportStatus {
                metadata: crate::session::CompatibilityExportRevision::Missing,
                content: crate::session::CompatibilityExportRevision::Missing,
            }
        );

        fs::write(session_dir.join("meta.json"), b"malformed").unwrap();
        fs::write(session_dir.join("content.txt"), b"malformed").unwrap();
        assert_eq!(
            compatibility_export_status(&session_dir).unwrap(),
            crate::session::CompatibilityExportStatus {
                metadata: crate::session::CompatibilityExportRevision::Malformed,
                content: crate::session::CompatibilityExportRevision::Malformed,
            }
        );

        export_compatibility_files(&session_dir, receipt.current.revision.get(), |_| false)
            .unwrap();
        let valid = crate::session::CompatibilityExportRevision::Valid {
            source_revision: receipt.current.revision.get(),
        };
        assert_eq!(
            compatibility_export_status(&session_dir).unwrap(),
            crate::session::CompatibilityExportStatus {
                metadata: valid,
                content: valid,
            }
        );
    }

    #[test]
    fn one_snapshot_writes_revision_stamped_streamed_exports() {
        let state = tempfile::tempdir().unwrap();
        let (session_dir, receipt) = canonical_fixture(state.path(), "exported text");

        let outcome =
            export_compatibility_files(&session_dir, receipt.current.revision.get(), |_| false)
                .unwrap();

        assert_eq!(
            outcome,
            ExportOutcome::Written {
                revision: receipt.current.revision.get()
            }
        );
        let expected = ExistingRevision::Valid(receipt.current.revision.get());
        assert_eq!(revisions(&session_dir), (expected, expected));
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(session_dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(metadata["cache_format_version"], EXPORT_FORMAT_VERSION);
        assert!(fs::read_to_string(session_dir.join("content.txt"))
            .unwrap()
            .contains("exported text"));
        assert!(fs::read_dir(&session_dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            for name in ["meta.json", "content.txt", ".compat-export.lock"] {
                let mode = fs::metadata(session_dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "unexpected mode for {name}");
            }
        }
    }

    #[test]
    fn malformed_exports_are_replaced_but_newer_exports_win() {
        let state = tempfile::tempdir().unwrap();
        let (session_dir, receipt) = canonical_fixture(state.path(), "canonical");
        fs::write(session_dir.join("meta.json"), "malformed").unwrap();
        fs::write(session_dir.join("content.txt"), "malformed").unwrap();
        export_compatibility_files(&session_dir, 0, |_| false).unwrap();
        let expected = ExistingRevision::Valid(receipt.current.revision.get());
        assert_eq!(revisions(&session_dir), (expected, expected));

        let newer = receipt.current.revision.get() + 1;
        let meta_path = session_dir.join("meta.json");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        metadata["revision"] = newer.into();
        metadata["source_revision"] = newer.into();
        fs::write(&meta_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        fs::write(
            session_dir.join("content.txt"),
            format!("# smelt-revision:{newer}\nnewer"),
        )
        .unwrap();
        assert_eq!(
            export_compatibility_files(&session_dir, 0, |_| false).unwrap(),
            ExportOutcome::Superseded {
                source: receipt.current.revision.get(),
                exported: newer,
            }
        );
        assert_eq!(
            revisions(&session_dir),
            (
                ExistingRevision::Valid(newer),
                ExistingRevision::Valid(newer),
            )
        );

        fs::write(session_dir.join("content.txt"), "malformed").unwrap();
        assert_eq!(
            export_compatibility_files(&session_dir, 0, |_| false).unwrap(),
            ExportOutcome::Written {
                revision: receipt.current.revision.get()
            }
        );
        assert_eq!(revisions(&session_dir), (expected, expected));
    }

    #[test]
    fn cancellation_preserves_existing_exports_and_discards_temporary_output() {
        let state = tempfile::tempdir().unwrap();
        let (session_dir, first) = canonical_fixture(state.path(), "first");
        let first_revision = first.current.revision.get();
        export_compatibility_files(&session_dir, first_revision, |_| false).unwrap();
        append_second_revision(&session_dir, first);

        let outcome = export_compatibility_files(&session_dir, 0, |_| {
            fs::read_dir(&session_dir).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".content.txt.")
            })
        })
        .unwrap();

        assert_eq!(outcome, ExportOutcome::Cancelled);
        let expected = ExistingRevision::Valid(first_revision);
        assert_eq!(revisions(&session_dir), (expected, expected));
        assert!(fs::read_to_string(session_dir.join("content.txt"))
            .unwrap()
            .contains("first"));
        assert!(fs::read_dir(&session_dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn failed_content_rebuild_preserves_existing_exports() {
        let state = tempfile::tempdir().unwrap();
        let (session_dir, first) = canonical_fixture(state.path(), "first");
        let first_revision = first.current.revision.get();
        export_compatibility_files(&session_dir, first_revision, |_| false).unwrap();
        append_second_revision(&session_dir, first);
        let db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
        db.connection()
            .execute_batch("DROP TABLE transcript_search")
            .unwrap();
        drop(db);

        let error = export_compatibility_files(&session_dir, 0, |_| false).unwrap_err();
        assert_eq!(error.class, ExportFailureClass::Io);
        let expected = ExistingRevision::Valid(first_revision);
        assert_eq!(revisions(&session_dir), (expected, expected));
        assert!(fs::read_to_string(session_dir.join("content.txt"))
            .unwrap()
            .contains("first"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_export_target_is_rejected_without_touching_its_destination() {
        use std::os::unix::fs::symlink;

        let state = tempfile::tempdir().unwrap();
        let (session_dir, _) = canonical_fixture(state.path(), "canonical");
        let destination = state.path().join("outside");
        fs::write(&destination, "keep").unwrap();
        symlink(&destination, session_dir.join("meta.json")).unwrap();

        let error = export_compatibility_files(&session_dir, 0, |_| false).unwrap_err();
        assert_eq!(error.class, ExportFailureClass::InvalidTarget);
        assert_eq!(fs::read_to_string(destination).unwrap(), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn permissions_failure_leaves_canonical_session_usable() {
        use std::os::unix::fs::PermissionsExt;

        let state = tempfile::tempdir().unwrap();
        let (session_dir, receipt) = canonical_fixture(state.path(), "canonical");
        fs::set_permissions(&session_dir, fs::Permissions::from_mode(0o500)).unwrap();
        let result =
            export_compatibility_files(&session_dir, receipt.current.revision.get(), |_| false);
        fs::set_permissions(&session_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let error = result.expect_err("read-only session directory must reject export writes");
        assert_eq!(error.class, ExportFailureClass::Permission);
        assert!(
            error
                .to_string()
                .to_ascii_lowercase()
                .contains("permission denied"),
            "unexpected export error: {error}"
        );
        let reader = smelt_store::SessionReader::open_existing(&session_dir).unwrap();
        assert_eq!(
            reader.store_head().unwrap().revision,
            receipt.current.revision
        );
        reader.quick_check().unwrap();
    }

    fn linux_resident_bytes() -> Option<u64> {
        let status = fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
        line.split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()?
            .checked_mul(1024)
    }

    #[test]
    #[ignore = "manual compatibility export benchmark"]
    fn compatibility_export_benchmark_suite() {
        if std::env::var("SMELT_COMPAT_EXPORT_BENCH").ok().as_deref() != Some("1") {
            eprintln!("COMPAT_EXPORT_BENCH_SKIPPED");
            return;
        }
        const ROW_BYTES: usize = 1024 * 1024;
        let target_bytes = std::env::var("SMELT_COMPAT_EXPORT_BENCH_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|bytes| *bytes > 0)
            .unwrap_or(50 * 1024 * 1024);
        let state = tempfile::tempdir().unwrap();
        let (session_dir, receipt) = canonical_fixture(state.path(), "export benchmark seed");
        let db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
        let conn = db.connection();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let payload = "x".repeat(ROW_BYTES);
        let mut remaining = target_bytes;
        let mut row_count = 0_u64;
        {
            let mut insert_block = conn
                .prepare(
                    "INSERT INTO transcript_blocks (block_idx, kind, estimated_text_bytes)
                     VALUES (?1, 'text', ?2)",
                )
                .unwrap();
            let mut insert_search = conn
                .prepare(
                    "INSERT INTO transcript_search (block_idx, history_idx, indexed_text)
                     VALUES (?1, NULL, ?2)",
                )
                .unwrap();
            while remaining > 0 {
                let bytes = remaining.min(ROW_BYTES);
                let block_idx = 10_000_i64 + i64::try_from(row_count).unwrap();
                insert_block
                    .execute([block_idx, i64::try_from(bytes).unwrap()])
                    .unwrap();
                insert_search
                    .execute((block_idx, &payload[..bytes]))
                    .unwrap();
                remaining -= bytes;
                row_count += 1;
            }
        }
        conn.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(db);

        let rss_before = linux_resident_bytes();
        let started = Instant::now();
        let outcome =
            export_compatibility_files(&session_dir, receipt.current.revision.get(), |_| false)
                .unwrap();
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let rss_after = linux_resident_bytes();
        let output_bytes = fs::metadata(session_dir.join("content.txt")).unwrap().len();
        assert!(matches!(outcome, ExportOutcome::Written { .. }));
        assert!(output_bytes >= target_bytes as u64);
        println!(
            "COMPAT_EXPORT_BENCH_JSON {}",
            serde_json::json!({
                "target_bytes": target_bytes,
                "rows": row_count,
                "row_bytes": ROW_BYTES,
                "output_bytes": output_bytes,
                "elapsed_ms": elapsed_ms,
                "rss_before_bytes": rss_before,
                "rss_after_bytes": rss_after,
            })
        );
    }

    #[test]
    fn overlapping_exporters_are_serialized_and_higher_revision_wins() {
        let state = tempfile::tempdir().unwrap();
        let (session_dir, first) = canonical_fixture(state.path(), "first");
        let first_revision = first.current.revision.get();
        let low_dir = session_dir.clone();
        let (snapshot_ready, snapshot_wait) = mpsc::sync_channel(1);
        let (release_snapshot, release_wait) = mpsc::sync_channel(1);
        let low = thread::spawn(move || {
            let mut paused = false;
            export_compatibility_files(&low_dir, first_revision, |source| {
                if !paused {
                    paused = true;
                    snapshot_ready.send(source).unwrap();
                    release_wait.recv().unwrap();
                }
                false
            })
            .unwrap()
        });
        assert_eq!(
            snapshot_wait.recv_timeout(Duration::from_secs(1)).unwrap(),
            first_revision
        );

        let second = append_second_revision(&session_dir, first);
        let second_revision = second.current.revision.get();
        let high_dir = session_dir.clone();
        let (lock_blocked, lock_wait) = mpsc::sync_channel(1);
        let high = thread::spawn(move || {
            let mut reported_block = false;
            export_compatibility_files(&high_dir, second_revision, |_| {
                if !reported_block {
                    reported_block = true;
                    lock_blocked.send(()).unwrap();
                }
                false
            })
            .unwrap()
        });
        lock_wait.recv_timeout(Duration::from_secs(1)).unwrap();
        release_snapshot.send(()).unwrap();

        assert_eq!(
            low.join().unwrap(),
            ExportOutcome::Written {
                revision: first_revision
            }
        );
        assert_eq!(
            high.join().unwrap(),
            ExportOutcome::Written {
                revision: second_revision
            }
        );
        let expected = ExistingRevision::Valid(second_revision);
        assert_eq!(revisions(&session_dir), (expected, expected));
    }

    #[test]
    fn rapid_requests_coalesce_to_the_latest_canonical_revision() {
        let state = tempfile::tempdir().unwrap();
        let state_root = state.path().join("smelt");
        let (session_dir, first) = canonical_fixture(&state_root, "first");
        let owner = ExporterOwner::spawn(state_root).unwrap();
        owner
            .handle
            .request(SESSION_ID.into(), first.current.revision.get())
            .unwrap();

        let second = append_second_revision(&session_dir, first);
        owner
            .handle
            .request(SESSION_ID.into(), second.current.revision.get())
            .unwrap();

        let expected = ExistingRevision::Valid(second.current.revision.get());
        let deadline = Instant::now() + Duration::from_secs(2);
        while revisions(&session_dir) != (expected, expected) {
            assert!(Instant::now() < deadline, "exporter did not converge");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(fs::read_to_string(session_dir.join("content.txt"))
            .unwrap()
            .contains("second"));
        drop(owner);
    }

    #[test]
    fn pending_requests_coalesce_and_overflow_to_reconciliation() {
        let (wake, _wakes) = mpsc::sync_channel(1);
        let handle = ExporterHandle {
            sessions_root: PathBuf::new(),
            pending: Arc::new(Mutex::new(PendingWork::default())),
            wake,
        };
        handle.request(SESSION_ID.into(), 1).unwrap();
        handle.request(SESSION_ID.into(), 3).unwrap();
        assert_eq!(
            handle.pending.lock().unwrap().exports[SESSION_ID].revision,
            3
        );
        handle.pending.lock().unwrap().exports.clear();

        for value in 0..=MAX_PENDING_EXPORTS {
            handle.request(format!("{value:064x}"), 1).unwrap();
        }
        let pending = handle.pending.lock().unwrap();
        assert!(pending.reconcile_all);
        assert!(pending.exports.is_empty());
    }

    #[test]
    fn reconciliation_rebuilds_exports_from_canonical_sessions() {
        let state = tempfile::tempdir().unwrap();
        let state_root = state.path().join("smelt");
        let (session_dir, receipt) = canonical_fixture(&state_root, "reconciled");
        let (wake, _wakes) = mpsc::sync_channel(1);
        let handle = ExporterHandle {
            sessions_root: state_root.join("sessions"),
            pending: Arc::new(Mutex::new(PendingWork::default())),
            wake,
        };

        reconcile_all(&handle, &mut WarningLimiter::default());

        let expected = ExistingRevision::Valid(receipt.current.revision.get());
        assert_eq!(revisions(&session_dir), (expected, expected));
    }

    #[test]
    fn interrupted_reconciliation_keeps_the_fallback_scheduled() {
        let state = tempfile::tempdir().unwrap();
        let state_root = state.path().join("smelt");
        let (session_dir, receipt) = canonical_fixture(&state_root, "interrupted");
        let (wake, _wakes) = mpsc::sync_channel(1);
        let handle = ExporterHandle {
            sessions_root: state_root.join("sessions"),
            pending: Arc::new(Mutex::new(PendingWork::default())),
            wake,
        };
        handle
            .request(SESSION_ID.into(), receipt.current.revision.get() + 1)
            .unwrap();

        reconcile_all(&handle, &mut WarningLimiter::default());

        let pending = handle.pending.lock().unwrap();
        assert!(pending.reconcile_all);
        assert!(pending.exports.contains_key(SESSION_ID));
        assert!(!session_dir.join("meta.json").exists());
    }

    #[test]
    fn exporter_shutdown_is_bounded_while_an_export_lock_is_held() {
        let state = tempfile::tempdir().unwrap();
        let state_root = state.path().join("smelt");
        let (session_dir, receipt) = canonical_fixture(&state_root, "blocked export");
        let held = CompatibilityExportLock::acquire(&session_dir, || false)
            .unwrap()
            .unwrap();
        let owner = ExporterOwner::spawn(state_root).unwrap();
        owner
            .handle
            .request(SESSION_ID.into(), receipt.current.revision.get())
            .unwrap();
        thread::sleep(QUIET_PERIOD + Duration::from_millis(25));

        let started = Instant::now();
        drop(owner);

        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held);
    }
}
