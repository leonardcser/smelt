use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) const MIGRATION_STATUS_FILE: &str = "migration.json";
const MAX_MIGRATION_FAILURE_LOGS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMigrationState {
    Pending,
    Failed,
}

impl SessionMigrationState {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionMigrationState::Pending => "pending",
            SessionMigrationState::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMigrationStatus {
    pub state: SessionMigrationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMigrationOutcome {
    Migrated,
    Repaired,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMigrationFailure {
    pub id: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMigrationBatchReport {
    pub scanned: usize,
    pub skipped: usize,
    pub migrated: usize,
    pub repaired: usize,
    pub failed: usize,
    pub failures: Vec<SessionMigrationFailure>,
}

// COMPAT(session-split-jsonl) / COMPAT(session-json-monolith): directory shapes accepted
// only while pre-SQLite sessions are imported to canonical SQLite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionDirKind {
    Empty,
    MigrationStatus,
    SqliteOnly,
    SqliteWithMetadata,
    SqliteWithLegacySidecars,
    LegacySidecars,
}

// COMPAT(session-split-jsonl) / COMPAT(session-json-monolith): detects pre-SQLite
// sidecars and partially migrated directories during the migration window.
pub(crate) fn classify_session_dir(dir_path: &Path) -> SessionDirKind {
    let has_db = dir_path.join("session.db").is_file();
    let has_meta = dir_path.join("meta.json").is_file();
    let has_split = dir_path.join("history.jsonl").is_file();
    let has_legacy = dir_path.join("session.json").is_file();
    let has_migration_status = dir_path.join(MIGRATION_STATUS_FILE).is_file();
    let has_legacy_sidecars = has_split || has_legacy;

    match (has_db, has_meta, has_legacy_sidecars, has_migration_status) {
        (true, _, true, _) => SessionDirKind::SqliteWithLegacySidecars,
        (true, true, false, _) => SessionDirKind::SqliteWithMetadata,
        (true, false, false, _) => SessionDirKind::SqliteOnly,
        (false, _, true, _) => SessionDirKind::LegacySidecars,
        (false, _, false, true) => SessionDirKind::MigrationStatus,
        (false, _, false, false) => SessionDirKind::Empty,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMigrationEvent {
    Started { pending: usize },
    Completed(SessionMigrationBatchReport),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMigrationError {
    MissingFile { path: PathBuf },
    ReadFile { path: PathBuf, message: String },
    ParseJson { path: PathBuf, message: String },
    UnsupportedSchema { path: PathBuf, version: u32 },
    ImportSqlite { message: String },
    SessionNotFound { id: String },
    MissingDatabase { id: String },
    OpenDatabase { message: String },
}

impl fmt::Display for SessionMigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionMigrationError::MissingFile { path } => {
                write!(f, "missing file {}", path.display())
            }
            SessionMigrationError::ReadFile { path, message } => {
                write!(f, "failed to read {}: {message}", path.display())
            }
            SessionMigrationError::ParseJson { path, message } => {
                write!(f, "failed to parse {}: {message}", path.display())
            }
            SessionMigrationError::UnsupportedSchema { path, version } => write!(
                f,
                "unsupported session schema version {version} in {}",
                path.display()
            ),
            SessionMigrationError::ImportSqlite { message } => {
                write!(f, "failed to import sqlite session: {message}")
            }
            SessionMigrationError::SessionNotFound { id } => {
                write!(f, "session not found or prefix is ambiguous: {id}")
            }
            SessionMigrationError::MissingDatabase { id } => {
                write!(f, "session {id} has no sqlite database")
            }
            SessionMigrationError::OpenDatabase { message } => {
                write!(f, "failed to open sqlite database: {message}")
            }
        }
    }
}

impl std::error::Error for SessionMigrationError {}

pub type SessionMigrationResult<T> = Result<T, SessionMigrationError>;

// COMPAT(session-split-jsonl): background-import pre-SQLite session directories
// into canonical SQLite storage during the alpha migration window.
pub fn migrate_session_dir_to_db(
    dir_path: &Path,
) -> SessionMigrationResult<SessionMigrationOutcome> {
    let result = migrate_session_dir_to_db_inner(dir_path);
    match &result {
        Ok(SessionMigrationOutcome::Migrated | SessionMigrationOutcome::Repaired) => {
            let _ = fs::remove_file(dir_path.join(MIGRATION_STATUS_FILE));
        }
        Ok(SessionMigrationOutcome::Skipped) if dir_path.join("session.db").is_file() => {
            let _ = fs::remove_file(dir_path.join(MIGRATION_STATUS_FILE));
        }
        Ok(SessionMigrationOutcome::Skipped) => {}
        Err(err) => write_migration_status(
            dir_path,
            &SessionMigrationStatus {
                state: SessionMigrationState::Failed,
                message: Some(err.to_string()),
                updated_at_ms: crate::session::now_ms(),
            },
        ),
    }
    result
}

fn migrate_session_dir_to_db_inner(
    dir_path: &Path,
) -> SessionMigrationResult<SessionMigrationOutcome> {
    crate::session::cleanup_stale_import_temp_files(dir_path);

    let kind = classify_session_dir(dir_path);

    match kind {
        SessionDirKind::SqliteOnly
        | SessionDirKind::SqliteWithMetadata
        | SessionDirKind::SqliteWithLegacySidecars => {
            let repaired = match crate::session::repair_sqlite_session_dir(dir_path) {
                Ok(repaired) => repaired,
                Err(err) if err.is_database_locked() => {
                    return Ok(SessionMigrationOutcome::Skipped)
                }
                Err(err) => {
                    return Err(SessionMigrationError::OpenDatabase {
                        message: err.to_string(),
                    });
                }
            };
            if matches!(kind, SessionDirKind::SqliteWithLegacySidecars) {
                crate::session::cleanup_migrated_legacy_artifacts(dir_path);
            }
            return Ok(if repaired {
                SessionMigrationOutcome::Repaired
            } else {
                SessionMigrationOutcome::Skipped
            });
        }
        SessionDirKind::LegacySidecars => {}
        SessionDirKind::Empty | SessionDirKind::MigrationStatus => {
            return Ok(SessionMigrationOutcome::Skipped);
        }
    }

    let has_split = dir_path.join("history.jsonl").is_file();
    let has_legacy = dir_path.join("session.json").is_file();
    let (session, loaded_legacy_json) = if has_split {
        match crate::session::read_jsonl_session(dir_path) {
            Ok(session) => (session, false),
            Err(_) if has_legacy => (crate::session::read_legacy_json_session(dir_path)?, true),
            Err(err) => return Err(err),
        }
    } else {
        (crate::session::read_legacy_json_session(dir_path)?, true)
    };

    crate::session::import_legacy_session_to_db(dir_path, &session).map_err(|err| {
        SessionMigrationError::ImportSqlite {
            message: err.to_string(),
        }
    })?;
    if loaded_legacy_json {
        crate::session::migrate_legacy_json_session(dir_path, &session);
    } else {
        crate::session::write_generated_sidecars(dir_path, &session);
        crate::session::cleanup_migrated_legacy_artifacts(dir_path);
    }
    Ok(SessionMigrationOutcome::Migrated)
}

pub fn ensure_session_db(dir_path: &Path) -> SessionMigrationResult<()> {
    if dir_path.join("session.db").is_file() {
        smelt_store::SessionDb::open(dir_path.join("session.db"))
            .map(|_| ())
            .map_err(|err| SessionMigrationError::OpenDatabase {
                message: err.to_string(),
            })?;
        return Ok(());
    }
    let _ = migrate_session_dir_to_db(dir_path)?;
    if dir_path.join("session.db").is_file() {
        Ok(())
    } else {
        Err(SessionMigrationError::MissingDatabase {
            id: session_dir_id(dir_path),
        })
    }
}

pub(crate) fn session_dir_needs_migration(dir_path: &Path) -> bool {
    matches!(
        classify_session_dir(dir_path),
        SessionDirKind::LegacySidecars
    )
}

fn session_dir_id(dir_path: &Path) -> String {
    dir_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<unknown>")
        .to_string()
}

pub fn migrate_all_sessions_once() -> SessionMigrationBatchReport {
    migrate_all_sessions_in_dir(&crate::session::sessions_dir())
}

pub(crate) fn migrate_all_sessions_in_dir(dir: &Path) -> SessionMigrationBatchReport {
    let Ok(entries) = fs::read_dir(dir) else {
        return SessionMigrationBatchReport::default();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            path.is_dir().then_some(path)
        })
        .collect();
    paths.sort();

    let mut report = SessionMigrationBatchReport::default();
    for path in paths {
        report.scanned += 1;
        match migrate_session_dir_to_db(&path) {
            Ok(SessionMigrationOutcome::Migrated) => report.migrated += 1,
            Ok(SessionMigrationOutcome::Repaired) => report.repaired += 1,
            Ok(SessionMigrationOutcome::Skipped) => report.skipped += 1,
            Err(err) => {
                report.failed += 1;
                if report.failures.len() < MAX_MIGRATION_FAILURE_LOGS {
                    report.failures.push(SessionMigrationFailure {
                        id: session_dir_id(&path),
                        message: err.to_string(),
                    });
                }
            }
        }
    }
    report
}

pub(crate) fn pending_session_migration_count_in_dir(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && session_dir_needs_migration(path))
        .count()
}

pub fn pending_session_migration_count() -> usize {
    pending_session_migration_count_in_dir(&crate::session::sessions_dir())
}

pub fn spawn_background_migration() {
    spawn_background_migration_with_event(|_| {});
}

pub fn spawn_background_migration_with_report(
    on_report: impl FnOnce(SessionMigrationBatchReport) + Send + 'static,
) {
    let mut on_report = Some(on_report);
    spawn_background_migration_with_event(move |event| {
        if let SessionMigrationEvent::Completed(report) = event {
            if let Some(on_report) = on_report.take() {
                on_report(report);
            }
        }
    });
}

pub fn spawn_background_migration_with_event(
    mut on_event: impl FnMut(SessionMigrationEvent) + Send + 'static,
) {
    let _ = std::thread::Builder::new()
        .name("smelt-session-migration".to_string())
        .spawn(move || {
            let pending = pending_session_migration_count();
            if pending == 0 {
                return;
            }
            on_event(SessionMigrationEvent::Started { pending });
            let report = migrate_all_sessions_once();
            log_migration_batch_report(&report);
            on_event(SessionMigrationEvent::Completed(report));
        });
}

fn log_migration_batch_report(report: &SessionMigrationBatchReport) {
    if report.migrated == 0 && report.repaired == 0 && report.failed == 0 {
        return;
    }

    let failures: Vec<_> = report
        .failures
        .iter()
        .map(|failure| {
            serde_json::json!({
                "id": failure.id,
                "message": failure.message,
            })
        })
        .collect();
    let omitted_failures = report.failed.saturating_sub(report.failures.len());
    let level = if report.failed > 0 {
        engine::log::Level::Warn
    } else {
        engine::log::Level::Info
    };
    engine::log::entry(
        level,
        "session_migration_batch",
        &serde_json::json!({
            "scanned": report.scanned,
            "migrated": report.migrated,
            "repaired": report.repaired,
            "skipped": report.skipped,
            "failed": report.failed,
            "failures": failures,
            "omitted_failures": omitted_failures,
        }),
    );
}

pub fn export_history_jsonl(id_or_prefix: &str, out: impl Write) -> Result<(), String> {
    let db = db_for_export(id_or_prefix).map_err(|err| err.to_string())?;
    db.export_history_jsonl(out).map_err(|err| err.to_string())
}

pub fn export_requests_jsonl(id_or_prefix: &str, out: impl Write) -> Result<(), String> {
    let db = db_for_export(id_or_prefix).map_err(|err| err.to_string())?;
    db.export_requests_jsonl(out).map_err(|err| err.to_string())
}

fn db_for_export(id_or_prefix: &str) -> SessionMigrationResult<smelt_store::SessionDb> {
    let id = crate::session::resolve_prefix(id_or_prefix).ok_or_else(|| {
        SessionMigrationError::SessionNotFound {
            id: id_or_prefix.to_string(),
        }
    })?;
    let dir = crate::session::sessions_dir().join(&id);
    ensure_session_db(&dir)?;
    smelt_store::SessionDb::open_read_only(dir.join("session.db")).map_err(|err| {
        SessionMigrationError::OpenDatabase {
            message: err.to_string(),
        }
    })
}

fn write_migration_status(dir_path: &Path, status: &SessionMigrationStatus) {
    if let Ok(json) = serde_json::to_vec(status) {
        crate::session::atomic_write(
            &dir_path.join(MIGRATION_STATUS_FILE),
            &json,
            crate::session::now_ms(),
        );
    }
}

pub(crate) fn read_migration_status(dir_path: &Path) -> Option<SessionMigrationStatus> {
    let contents = fs::read_to_string(dir_path.join(MIGRATION_STATUS_FILE)).ok()?;
    serde_json::from_str(&contents).ok()
}

pub(crate) fn migration_status_for_dir(dir_path: &Path) -> Option<SessionMigrationStatus> {
    match classify_session_dir(dir_path) {
        SessionDirKind::SqliteOnly
        | SessionDirKind::SqliteWithMetadata
        | SessionDirKind::SqliteWithLegacySidecars
        | SessionDirKind::Empty => None,
        SessionDirKind::MigrationStatus => read_migration_status(dir_path),
        SessionDirKind::LegacySidecars => {
            read_migration_status(dir_path).or(Some(SessionMigrationStatus {
                state: SessionMigrationState::Pending,
                message: None,
                updated_at_ms: 0,
            }))
        }
    }
}

#[cfg(test)]
pub(crate) fn max_migration_failure_logs() -> usize {
    MAX_MIGRATION_FAILURE_LOGS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_migration_is_silent_when_no_sessions_are_pending() {
        let state = tempfile::tempdir().unwrap();
        let _g = crate::test_util::isolate_xdg_state(state.path());
        let (tx, rx) = std::sync::mpsc::channel();

        spawn_background_migration_with_event(move |event| {
            tx.send(event).unwrap();
        });

        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
    }

    #[test]
    fn existing_invalid_db_with_legacy_sidecars_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("session.db"), b"not sqlite").unwrap();
        fs::write(dir.path().join("history.jsonl"), b"{}").unwrap();

        let err = migrate_session_dir_to_db(dir.path()).unwrap_err();
        assert!(matches!(err, SessionMigrationError::OpenDatabase { .. }));
        assert!(read_migration_status(dir.path()).is_some());
    }

    #[test]
    fn existing_invalid_db_without_legacy_sidecars_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("session.db"), b"not sqlite").unwrap();

        let err = migrate_session_dir_to_db(dir.path()).unwrap_err();
        assert!(matches!(err, SessionMigrationError::OpenDatabase { .. }));
        assert!(read_migration_status(dir.path()).is_some());
    }
}
