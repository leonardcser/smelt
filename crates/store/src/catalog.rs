use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::types::Value;
use rusqlite::{named_params, params, params_from_iter, Connection, OpenFlags, OptionalExtension};

use crate::{Result, StoreError};

pub const CATALOG_SCHEMA_VERSION: i32 = 1;
pub const MAX_CATALOG_PAGE_SIZE: u32 = 10_000;

const CATALOG_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS catalog_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL,
    next_scan_id INTEGER NOT NULL,
    completed_scan_id INTEGER NOT NULL,
    reconciled_at INTEGER
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    title TEXT,
    slug TEXT,
    first_user_message TEXT,
    cwd TEXT,
    mode TEXT,
    reasoning_effort TEXT,
    model TEXT,
    fast_mode INTEGER,
    parent_id TEXT,
    context_tokens INTEGER,
    history_len INTEGER,
    text_bytes INTEGER,
    created_at INTEGER,
    updated_at INTEGER,
    source_revision INTEGER,
    status TEXT NOT NULL CHECK (status IN ('available', 'unavailable')),
    error_kind TEXT,
    error_summary TEXT,
    last_seen_scan INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS sessions_updated_idx ON sessions(updated_at DESC, id);
CREATE INDEX IF NOT EXISTS sessions_cwd_updated_idx ON sessions(cwd, updated_at DESC, id);
CREATE INDEX IF NOT EXISTS sessions_status_updated_idx ON sessions(status, updated_at DESC, id);
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogAvailability {
    Available,
    Unavailable,
}

impl CatalogAvailability {
    fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "available" => Ok(Self::Available),
            "unavailable" => Ok(Self::Unavailable),
            other => Err(StoreError::Integrity(format!(
                "unknown catalog availability {other:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSession {
    pub id: String,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub first_user_message: Option<String>,
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model: Option<String>,
    pub fast_mode: Option<bool>,
    pub parent_id: Option<String>,
    pub context_tokens: Option<u64>,
    pub history_len: Option<u64>,
    pub text_bytes: Option<u64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub source_revision: u64,
    pub availability: CatalogAvailability,
    pub error_kind: Option<String>,
    pub error_summary: Option<String>,
    pub last_seen_scan: u64,
}

impl CatalogSession {
    pub fn unavailable(
        id: impl Into<String>,
        error_kind: impl Into<String>,
        error_summary: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: None,
            slug: None,
            first_user_message: None,
            cwd: None,
            mode: None,
            reasoning_effort: None,
            model: None,
            fast_mode: None,
            parent_id: None,
            context_tokens: None,
            history_len: None,
            text_bytes: None,
            created_at: 0,
            updated_at: 0,
            source_revision: 0,
            availability: CatalogAvailability::Unavailable,
            error_kind: Some(error_kind.into()),
            error_summary: Some(error_summary.into()),
            last_seen_scan: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogCursor {
    pub updated_at: i64,
    pub id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogQuery {
    pub limit: u32,
    pub cursor: Option<CatalogCursor>,
    pub cwd: Option<String>,
    pub availability: Option<CatalogAvailability>,
}

impl Default for CatalogQuery {
    fn default() -> Self {
        Self {
            limit: 200,
            cursor: None,
            cwd: None,
            availability: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogPage {
    pub sessions: Vec<CatalogSession>,
    pub next_cursor: Option<CatalogCursor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogMetadata {
    pub next_scan_id: u64,
    pub completed_scan_id: u64,
    pub reconciled_at: Option<i64>,
}

pub struct Catalog {
    path: PathBuf,
    conn: Connection,
}

impl Catalog {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        set_private_file_permissions(path)?;
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(CATALOG_SCHEMA)?;
        conn.execute(
            "INSERT OR IGNORE INTO catalog_meta
             (singleton, schema_version, next_scan_id, completed_scan_id, reconciled_at)
             VALUES (1, ?1, 1, 0, NULL)",
            [CATALOG_SCHEMA_VERSION],
        )?;
        validate_schema(&conn)?;
        Ok(Self {
            path: path.to_path_buf(),
            conn,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> Result<CatalogMetadata> {
        catalog_metadata(&self.conn)
    }

    pub fn page(&self, query: &CatalogQuery) -> Result<CatalogPage> {
        query_page(&self.conn, query)
    }

    pub fn upsert_available(&mut self, session: &CatalogSession) -> Result<bool> {
        self.upsert_available_with_scan(session, self.current_scan_marker()?, false)
    }

    pub fn upsert_available_for_reconciliation(
        &mut self,
        session: &CatalogSession,
        scan_id: u64,
    ) -> Result<bool> {
        self.upsert_available_with_scan(session, scan_id, true)
    }

    fn upsert_available_with_scan(
        &mut self,
        session: &CatalogSession,
        scan_id: u64,
        replace_ahead_revision: bool,
    ) -> Result<bool> {
        if session.availability != CatalogAvailability::Available {
            return Err(StoreError::Integrity(format!(
                "catalog row {} is not available",
                session.id
            )));
        }
        let context_tokens = optional_sql_u64(session.context_tokens, "context_tokens")?;
        let history_len = optional_sql_u64(session.history_len, "history_len")?;
        let text_bytes = optional_sql_u64(session.text_bytes, "text_bytes")?;
        let source_revision = sql_u64(session.source_revision, "source_revision")?;
        let last_seen_scan = sql_u64(scan_id, "last_seen_scan")?;
        let fast_mode = session.fast_mode.map(i64::from);
        let revision_guard = if replace_ahead_revision {
            ""
        } else {
            " WHERE excluded.source_revision >= sessions.source_revision"
        };
        let sql = format!(
            "INSERT INTO sessions (
                id, title, slug, first_user_message, cwd, mode, reasoning_effort, model,
                fast_mode, parent_id, context_tokens, history_len, text_bytes, created_at,
                updated_at, source_revision, status, error_kind, error_summary, last_seen_scan
             ) VALUES (
                :id, :title, :slug, :first_user_message, :cwd, :mode, :reasoning_effort, :model,
                :fast_mode, :parent_id, :context_tokens, :history_len, :text_bytes, :created_at,
                :updated_at, :source_revision, 'available', NULL, NULL, :last_seen_scan
             ) ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                slug = excluded.slug,
                first_user_message = excluded.first_user_message,
                cwd = excluded.cwd,
                mode = excluded.mode,
                reasoning_effort = excluded.reasoning_effort,
                model = excluded.model,
                fast_mode = excluded.fast_mode,
                parent_id = excluded.parent_id,
                context_tokens = excluded.context_tokens,
                history_len = excluded.history_len,
                text_bytes = excluded.text_bytes,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at,
                source_revision = excluded.source_revision,
                status = 'available',
                error_kind = NULL,
                error_summary = NULL,
                last_seen_scan = MAX(sessions.last_seen_scan, excluded.last_seen_scan){revision_guard}"
        );
        let changed = self.conn.execute(
            &sql,
            named_params! {
                ":id": session.id,
                ":title": session.title,
                ":slug": session.slug,
                ":first_user_message": session.first_user_message,
                ":cwd": session.cwd,
                ":mode": session.mode,
                ":reasoning_effort": session.reasoning_effort,
                ":model": session.model,
                ":fast_mode": fast_mode,
                ":parent_id": session.parent_id,
                ":context_tokens": context_tokens,
                ":history_len": history_len,
                ":text_bytes": text_bytes,
                ":created_at": session.created_at,
                ":updated_at": session.updated_at,
                ":source_revision": source_revision,
                ":last_seen_scan": last_seen_scan,
            },
        )?;
        Ok(changed != 0)
    }

    pub fn upsert_unavailable(
        &mut self,
        id: &str,
        error_kind: &str,
        error_summary: &str,
    ) -> Result<()> {
        let scan_id = self.current_scan_marker()?;
        self.upsert_unavailable_for_reconciliation(id, error_kind, error_summary, scan_id)
    }

    pub fn upsert_unavailable_for_reconciliation(
        &mut self,
        id: &str,
        error_kind: &str,
        error_summary: &str,
        scan_id: u64,
    ) -> Result<()> {
        let scan_id = sql_u64(scan_id, "last_seen_scan")?;
        self.conn.execute(
            "INSERT INTO sessions (
                id, created_at, updated_at, source_revision, status,
                error_kind, error_summary, last_seen_scan
             ) VALUES (?1, 0, 0, 0, 'unavailable', ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                status = 'unavailable',
                error_kind = excluded.error_kind,
                error_summary = excluded.error_summary,
                last_seen_scan = MAX(sessions.last_seen_scan, excluded.last_seen_scan)",
            params![id, error_kind, error_summary, scan_id],
        )?;
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM sessions WHERE id = ?1", [id])?
            != 0)
    }

    pub fn allocate_scan(&mut self) -> Result<u64> {
        let tx = self.conn.transaction()?;
        let next: i64 = tx.query_row(
            "SELECT next_scan_id FROM catalog_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let scan_id = nonnegative_u64(next, "next_scan_id")?;
        let following = scan_id
            .checked_add(1)
            .ok_or_else(|| StoreError::Integrity("catalog scan id overflow".into()))?;
        tx.execute(
            "UPDATE catalog_meta SET next_scan_id = ?1 WHERE singleton = 1",
            [sql_u64(following, "next_scan_id")?],
        )?;
        tx.commit()?;
        Ok(scan_id)
    }

    pub fn complete_scan(&mut self, scan_id: u64, reconciled_at: i64) -> Result<usize> {
        let scan_id_sql = sql_u64(scan_id, "scan_id")?;
        let tx = self.conn.transaction()?;
        let completed: i64 = tx.query_row(
            "SELECT completed_scan_id FROM catalog_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if nonnegative_u64(completed, "completed_scan_id")? > scan_id {
            return Err(StoreError::Integrity(format!(
                "catalog scan {scan_id} completed after a newer scan"
            )));
        }
        let deleted = tx.execute(
            "DELETE FROM sessions WHERE last_seen_scan < ?1",
            [scan_id_sql],
        )?;
        tx.execute(
            "UPDATE catalog_meta
             SET completed_scan_id = ?1, reconciled_at = ?2
             WHERE singleton = 1",
            params![scan_id_sql, reconciled_at],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    fn current_scan_marker(&self) -> Result<u64> {
        let metadata = self.metadata()?;
        Ok(metadata
            .next_scan_id
            .saturating_sub(1)
            .max(metadata.completed_scan_id))
    }
}

pub struct CatalogReader {
    path: PathBuf,
    conn: Connection,
}

impl CatalogReader {
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Option<Self>> {
        let path = path.as_ref();
        if !path.is_file() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(2))?;
        validate_schema(&conn)?;
        Ok(Some(Self {
            path: path.to_path_buf(),
            conn,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> Result<CatalogMetadata> {
        catalog_metadata(&self.conn)
    }

    pub fn page(&self, query: &CatalogQuery) -> Result<CatalogPage> {
        query_page(&self.conn, query)
    }

    pub fn session(&self, id: &str) -> Result<Option<CatalogSession>> {
        query_session(&self.conn, id)
    }
}

pub struct CatalogReconcileLock {
    file: File,
}

impl CatalogReconcileLock {
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        set_private_file_permissions(path)?;
        file.lock()?;
        Ok(Self { file })
    }
}

impl Drop for CatalogReconcileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn archive_corrupt_catalog(path: impl AsRef<Path>) -> Result<Option<PathBuf>> {
    let path = path.as_ref();
    let archived = if path.exists() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("catalog.db");
        let archived = path.with_file_name(format!(
            "{file_name}.corrupt-{}-{timestamp}",
            std::process::id()
        ));
        fs::rename(path, &archived)?;
        Some(archived)
    } else {
        None
    };
    for candidate in [
        sqlite_companion_path(path, "-wal"),
        sqlite_companion_path(path, "-shm"),
    ] {
        match fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(archived)
}

pub fn rebuild_catalog(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    for candidate in [
        path.to_path_buf(),
        sqlite_companion_path(path, "-wal"),
        sqlite_companion_path(path, "-shm"),
    ] {
        match fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_schema(conn: &Connection) -> Result<()> {
    let version = conn
        .query_row(
            "SELECT schema_version FROM catalog_meta WHERE singleton = 1",
            [],
            |row| row.get::<_, i32>(0),
        )
        .optional()?;
    match version {
        Some(found) if found == CATALOG_SCHEMA_VERSION => Ok(()),
        Some(found) => Err(StoreError::UnsupportedSchema {
            found,
            expected: CATALOG_SCHEMA_VERSION,
        }),
        None => Err(StoreError::Integrity(
            "catalog metadata row is missing".into(),
        )),
    }
}

fn catalog_metadata(conn: &Connection) -> Result<CatalogMetadata> {
    let (next_scan_id, completed_scan_id, reconciled_at): (i64, i64, Option<i64>) = conn
        .query_row(
            "SELECT next_scan_id, completed_scan_id, reconciled_at
             FROM catalog_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    Ok(CatalogMetadata {
        next_scan_id: nonnegative_u64(next_scan_id, "next_scan_id")?,
        completed_scan_id: nonnegative_u64(completed_scan_id, "completed_scan_id")?,
        reconciled_at,
    })
}

fn query_session(conn: &Connection, id: &str) -> Result<Option<CatalogSession>> {
    let mut statement = conn.prepare(
        "SELECT id, title, slug, first_user_message, cwd, mode, reasoning_effort, model,
                fast_mode, parent_id, context_tokens, history_len, text_bytes, created_at,
                updated_at, source_revision, status, error_kind, error_summary, last_seen_scan
         FROM sessions WHERE id = ?1",
    )?;
    let mut rows = statement.query([id])?;
    rows.next()?.map(catalog_session_from_row).transpose()
}

fn query_page(conn: &Connection, query: &CatalogQuery) -> Result<CatalogPage> {
    if query.limit == 0 || query.limit > MAX_CATALOG_PAGE_SIZE {
        return Err(StoreError::Integrity(format!(
            "catalog page limit must be between 1 and {MAX_CATALOG_PAGE_SIZE}"
        )));
    }
    let mut sql = String::from(
        "SELECT id, title, slug, first_user_message, cwd, mode, reasoning_effort, model,
                fast_mode, parent_id, context_tokens, history_len, text_bytes, created_at,
                updated_at, source_revision, status, error_kind, error_summary, last_seen_scan
         FROM sessions",
    );
    let mut clauses = Vec::new();
    let mut values = Vec::<Value>::new();
    if let Some(cwd) = &query.cwd {
        clauses.push("cwd = ?");
        values.push(Value::Text(cwd.clone()));
    }
    if let Some(availability) = query.availability {
        clauses.push("status = ?");
        values.push(Value::Text(availability.as_str().to_string()));
    }
    if let Some(cursor) = &query.cursor {
        clauses.push("(updated_at < ? OR (updated_at = ? AND id > ?))");
        values.push(Value::Integer(cursor.updated_at));
        values.push(Value::Integer(cursor.updated_at));
        values.push(Value::Text(cursor.id.clone()));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY updated_at DESC, id ASC LIMIT ?");
    values.push(Value::Integer(i64::from(query.limit) + 1));

    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(values))?;
    let mut sessions = Vec::with_capacity(query.limit as usize + 1);
    while let Some(row) = rows.next()? {
        sessions.push(catalog_session_from_row(row)?);
    }
    let has_more = sessions.len() > query.limit as usize;
    if has_more {
        sessions.pop();
    }
    let next_cursor = has_more.then(|| {
        let last = sessions.last().expect("nonzero catalog page limit");
        CatalogCursor {
            updated_at: last.updated_at,
            id: last.id.clone(),
        }
    });
    Ok(CatalogPage {
        sessions,
        next_cursor,
    })
}

fn catalog_session_from_row(row: &rusqlite::Row<'_>) -> Result<CatalogSession> {
    let fast_mode = match row.get::<_, Option<i64>>(8)? {
        None => None,
        Some(0) => Some(false),
        Some(1) => Some(true),
        Some(value) => {
            return Err(StoreError::Integrity(format!(
                "catalog fast_mode must be 0 or 1, got {value}"
            )))
        }
    };
    let status: String = row.get(16)?;
    Ok(CatalogSession {
        id: row.get(0)?,
        title: row.get(1)?,
        slug: row.get(2)?,
        first_user_message: row.get(3)?,
        cwd: row.get(4)?,
        mode: row.get(5)?,
        reasoning_effort: row.get(6)?,
        model: row.get(7)?,
        fast_mode,
        parent_id: row.get(9)?,
        context_tokens: optional_nonnegative_u64(row.get(10)?, "context_tokens")?,
        history_len: optional_nonnegative_u64(row.get(11)?, "history_len")?,
        text_bytes: optional_nonnegative_u64(row.get(12)?, "text_bytes")?,
        created_at: row.get::<_, Option<i64>>(13)?.unwrap_or(0),
        updated_at: row.get::<_, Option<i64>>(14)?.unwrap_or(0),
        source_revision: nonnegative_u64(
            row.get::<_, Option<i64>>(15)?.unwrap_or(0),
            "source_revision",
        )?,
        availability: CatalogAvailability::from_str(&status)?,
        error_kind: row.get(17)?,
        error_summary: row.get(18)?,
        last_seen_scan: nonnegative_u64(row.get(19)?, "last_seen_scan")?,
    })
}

fn sql_u64(value: u64, field: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| StoreError::Integrity(format!("{field} exceeds SQLite integer range")))
}

fn optional_sql_u64(value: Option<u64>, field: &str) -> Result<Option<i64>> {
    value.map(|value| sql_u64(value, field)).transpose()
}

fn nonnegative_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| StoreError::Integrity(format!("{field} must be nonnegative, got {value}")))
}

fn optional_nonnegative_u64(value: Option<i64>, field: &str) -> Result<Option<u64>> {
    value.map(|value| nonnegative_u64(value, field)).transpose()
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sqlite_companion_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG_CRASH_ROLE: &str = "SMELT_CATALOG_CRASH_ROLE";
    const CATALOG_CRASH_PATH: &str = "SMELT_CATALOG_CRASH_PATH";

    fn row(id: &str, revision: u64, updated_at: i64) -> CatalogSession {
        CatalogSession {
            id: id.into(),
            title: Some(format!("title-{id}")),
            slug: None,
            first_user_message: Some(format!("message-{id}")),
            cwd: Some(if id.starts_with('a') { "/a" } else { "/b" }.into()),
            mode: Some("agent".into()),
            reasoning_effort: Some("medium".into()),
            model: Some("model".into()),
            fast_mode: Some(false),
            parent_id: None,
            context_tokens: Some(100),
            history_len: Some(2),
            text_bytes: Some(20),
            created_at: updated_at - 1,
            updated_at,
            source_revision: revision,
            availability: CatalogAvailability::Available,
            error_kind: None,
            error_summary: None,
            last_seen_scan: 0,
        }
    }

    #[test]
    fn creates_exact_schema_and_indexes() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(temp.path().join("catalog.db")).unwrap();
        assert_eq!(catalog.metadata().unwrap().next_scan_id, 1);
        let indexes = catalog
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(indexes.contains(&"sessions_updated_idx".to_string()));
        assert!(indexes.contains(&"sessions_cwd_updated_idx".to_string()));
        assert!(indexes.contains(&"sessions_status_updated_idx".to_string()));
        assert_eq!(
            catalog
                .conn
                .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
                .unwrap()
                .to_ascii_lowercase(),
            "wal"
        );
    }

    #[test]
    fn reader_returns_one_exact_session_without_scanning_pages() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("catalog.db");
        let mut catalog = Catalog::open(&path).unwrap();
        let expected = row("exact", 7, 42);
        catalog.upsert_available(&expected).unwrap();
        catalog.upsert_available(&row("other", 8, 43)).unwrap();
        drop(catalog);

        let reader = CatalogReader::open_existing(&path).unwrap().unwrap();
        assert_eq!(reader.session("exact").unwrap(), Some(expected));
        assert_eq!(reader.session("missing").unwrap(), None);
    }

    #[test]
    fn catalog_projection_crash_probe() {
        let Ok(role) = std::env::var(CATALOG_CRASH_ROLE) else {
            return;
        };
        let path = PathBuf::from(
            std::env::var_os(CATALOG_CRASH_PATH).expect("catalog crash database path"),
        );
        let mut catalog = Catalog::open(path).unwrap();
        if role == "during-upsert" {
            catalog
                .conn
                .create_scalar_function(
                    "smelt_test_crash",
                    0,
                    rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                    |_| -> rusqlite::Result<i64> { std::process::abort() },
                )
                .unwrap();
            catalog
                .conn
                .execute_batch(
                    "CREATE TEMP TRIGGER crash_catalog AFTER INSERT ON sessions
                     BEGIN SELECT smelt_test_crash(); END;",
                )
                .unwrap();
        } else if role != "after-upsert" {
            panic!("unknown catalog crash role {role}");
        }
        catalog.upsert_available(&row("crash", 7, 42)).unwrap();
        std::process::abort();
    }

    #[test]
    fn subprocess_crashes_leave_catalog_projection_absent_or_complete() {
        for role in ["during-upsert", "after-upsert"] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("catalog.db");
            drop(Catalog::open(&path).unwrap());
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("catalog::tests::catalog_projection_crash_probe")
                .arg("--nocapture")
                .env(CATALOG_CRASH_ROLE, role)
                .env(CATALOG_CRASH_PATH, &path)
                .status()
                .unwrap();
            assert!(
                !status.success(),
                "catalog crash probe unexpectedly succeeded"
            );
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(status.signal(), Some(libc::SIGABRT));
            }

            let reader = CatalogReader::open_existing(&path).unwrap().unwrap();
            reader
                .conn
                .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                .map(|status| assert_eq!(status, "ok"))
                .unwrap();
            assert_eq!(
                reader.session("crash").unwrap(),
                (role == "after-upsert").then(|| row("crash", 7, 42))
            );
        }
    }

    #[test]
    fn pages_in_stable_order_and_filters_in_sqlite() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(temp.path().join("catalog.db")).unwrap();
        for entry in [row("b", 1, 20), row("a2", 1, 30), row("a1", 1, 30)] {
            catalog.upsert_available(&entry).unwrap();
        }
        catalog
            .upsert_unavailable("missing", "missing_database", "gone")
            .unwrap();

        let first = catalog
            .page(&CatalogQuery {
                limit: 2,
                ..CatalogQuery::default()
            })
            .unwrap();
        assert_eq!(
            first
                .sessions
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["a1", "a2"]
        );
        let second = catalog
            .page(&CatalogQuery {
                limit: 2,
                cursor: first.next_cursor,
                ..CatalogQuery::default()
            })
            .unwrap();
        assert_eq!(
            second
                .sessions
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "missing"]
        );

        let filtered = catalog
            .page(&CatalogQuery {
                limit: 10,
                cwd: Some("/a".into()),
                availability: Some(CatalogAvailability::Available),
                ..CatalogQuery::default()
            })
            .unwrap();
        assert_eq!(filtered.sessions.len(), 2);
        assert!(filtered
            .sessions
            .iter()
            .all(|row| row.cwd.as_deref() == Some("/a")));
    }

    #[test]
    fn revision_guard_rejects_stale_and_equal_revision_repairs_status() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(temp.path().join("catalog.db")).unwrap();
        let mut current = row("same", 5, 50);
        catalog.upsert_available(&current).unwrap();
        catalog
            .upsert_unavailable("same", "sqlite", "temporarily unavailable")
            .unwrap();

        current.title = Some("repaired".into());
        assert!(catalog.upsert_available(&current).unwrap());
        let mut stale = row("same", 4, 100);
        stale.title = Some("stale".into());
        assert!(!catalog.upsert_available(&stale).unwrap());

        let page = catalog.page(&CatalogQuery::default()).unwrap();
        assert_eq!(page.sessions[0].title.as_deref(), Some("repaired"));
        assert_eq!(page.sessions[0].source_revision, 5);
        assert_eq!(
            page.sessions[0].availability,
            CatalogAvailability::Available
        );
    }

    #[test]
    fn reconciliation_replaces_impossibly_ahead_rows() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(temp.path().join("catalog.db")).unwrap();
        catalog.upsert_available(&row("same", 99, 99)).unwrap();
        let scan = catalog.allocate_scan().unwrap();
        let canonical = row("same", 3, 30);
        catalog
            .upsert_available_for_reconciliation(&canonical, scan)
            .unwrap();
        catalog.complete_scan(scan, 100).unwrap();
        let projected = &catalog.page(&CatalogQuery::default()).unwrap().sessions[0];
        assert_eq!(projected.source_revision, 3);
        assert_eq!(projected.updated_at, 30);
    }

    #[test]
    fn only_a_completed_scan_deletes_unseen_rows() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(temp.path().join("catalog.db")).unwrap();
        catalog.upsert_available(&row("stale", 1, 10)).unwrap();
        let abandoned_scan = catalog.allocate_scan().unwrap();
        assert_eq!(abandoned_scan, 1);
        drop(catalog);

        let mut reopened = Catalog::open(temp.path().join("catalog.db")).unwrap();
        assert_eq!(
            reopened
                .page(&CatalogQuery::default())
                .unwrap()
                .sessions
                .len(),
            1
        );
        let completed_scan = reopened.allocate_scan().unwrap();
        reopened.complete_scan(completed_scan, 200).unwrap();
        assert!(reopened
            .page(&CatalogQuery::default())
            .unwrap()
            .sessions
            .is_empty());
        assert_eq!(
            reopened.metadata().unwrap().completed_scan_id,
            completed_scan
        );
    }

    #[test]
    fn unavailable_projection_retains_prior_summary() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(temp.path().join("catalog.db")).unwrap();
        catalog.upsert_available(&row("retained", 7, 70)).unwrap();
        catalog
            .upsert_unavailable("retained", "sqlite", "database is busy")
            .unwrap();
        let projected = &catalog.page(&CatalogQuery::default()).unwrap().sessions[0];
        assert_eq!(projected.title.as_deref(), Some("title-retained"));
        assert_eq!(projected.source_revision, 7);
        assert_eq!(projected.availability, CatalogAvailability::Unavailable);
        assert_eq!(projected.error_kind.as_deref(), Some("sqlite"));
    }

    #[test]
    fn remove_then_recreate_is_visible() {
        let temp = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(temp.path().join("catalog.db")).unwrap();
        catalog.upsert_available(&row("recreated", 1, 10)).unwrap();
        assert!(catalog.remove("recreated").unwrap());
        assert!(catalog
            .page(&CatalogQuery::default())
            .unwrap()
            .sessions
            .is_empty());
        catalog.upsert_available(&row("recreated", 1, 20)).unwrap();
        assert_eq!(
            catalog
                .page(&CatalogQuery::default())
                .unwrap()
                .sessions
                .len(),
            1
        );
    }

    #[test]
    fn reconciliation_lock_serializes_catalog_mutations() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".catalog.lock");
        let _held = CatalogReconcileLock::acquire(&path).unwrap();
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        assert!(contender.try_lock().is_err());
    }

    #[test]
    fn hundred_thousand_rows_use_indexed_pagination_and_filters() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(temp.path().join("catalog.db")).unwrap();
        catalog
            .conn
            .execute_batch(
                "BEGIN;
                 WITH RECURSIVE sequence(value) AS (
                     VALUES(1)
                     UNION ALL
                     SELECT value + 1 FROM sequence WHERE value < 100000
                 )
                 INSERT INTO sessions (
                     id, cwd, created_at, updated_at, source_revision, status, last_seen_scan
                 )
                 SELECT printf('%064x', value),
                        CASE value % 2 WHEN 0 THEN '/even' ELSE '/odd' END,
                        value,
                        value / 2,
                        1,
                        CASE value % 10 WHEN 0 THEN 'unavailable' ELSE 'available' END,
                        0
                 FROM sequence;
                 COMMIT;",
            )
            .unwrap();

        let first = catalog
            .page(&CatalogQuery {
                limit: 100,
                ..CatalogQuery::default()
            })
            .unwrap();
        assert_eq!(first.sessions.len(), 100);
        let second = catalog
            .page(&CatalogQuery {
                limit: 100,
                cursor: first.next_cursor.clone(),
                ..CatalogQuery::default()
            })
            .unwrap();
        assert_eq!(second.sessions.len(), 100);
        assert!(first.sessions.last().unwrap().updated_at >= second.sessions[0].updated_at);

        let filtered = catalog
            .page(&CatalogQuery {
                limit: 100,
                cwd: Some("/even".into()),
                availability: Some(CatalogAvailability::Available),
                ..CatalogQuery::default()
            })
            .unwrap();
        assert_eq!(filtered.sessions.len(), 100);
        assert!(filtered.sessions.iter().all(|row| {
            row.cwd.as_deref() == Some("/even")
                && row.availability == CatalogAvailability::Available
        }));

        for (sql, expected_index) in [
            (
                "EXPLAIN QUERY PLAN SELECT id FROM sessions ORDER BY updated_at DESC, id LIMIT 101",
                "sessions_updated_idx",
            ),
            (
                "EXPLAIN QUERY PLAN SELECT id FROM sessions WHERE cwd = '/even' ORDER BY updated_at DESC, id LIMIT 101",
                "sessions_cwd_updated_idx",
            ),
            (
                "EXPLAIN QUERY PLAN SELECT id FROM sessions WHERE status = 'available' ORDER BY updated_at DESC, id LIMIT 101",
                "sessions_status_updated_idx",
            ),
        ] {
            let plan = catalog
                .conn
                .prepare(sql)
                .unwrap()
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("\n");
            assert!(plan.contains(expected_index), "query plan did not use {expected_index}: {plan}");
        }
    }

    #[test]
    #[ignore = "manual catalog query benchmark"]
    fn catalog_query_benchmark_suite() {
        if std::env::var("SMELT_CATALOG_BENCH").ok().as_deref() != Some("1") {
            eprintln!("CATALOG_BENCH_SKIPPED");
            return;
        }
        let row_count = std::env::var("SMELT_CATALOG_BENCH_ROWS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|count| *count >= 1_000)
            .unwrap_or(100_000);
        let runs = std::env::var("SMELT_CATALOG_BENCH_RUNS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|count| *count > 0)
            .unwrap_or(101);

        let temp = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(temp.path().join("catalog.db")).unwrap();
        catalog
            .conn
            .execute_batch(&format!(
                "BEGIN;
                 WITH RECURSIVE sequence(value) AS (
                     VALUES(1)
                     UNION ALL
                     SELECT value + 1 FROM sequence WHERE value < {row_count}
                 )
                 INSERT INTO sessions (
                     id, cwd, created_at, updated_at, source_revision, status, last_seen_scan
                 )
                 SELECT printf('%064x', value),
                        CASE value % 2 WHEN 0 THEN '/even' ELSE '/odd' END,
                        value,
                        value / 2,
                        1,
                        CASE value % 10 WHEN 0 THEN 'unavailable' ELSE 'available' END,
                        0
                 FROM sequence;
                 COMMIT;"
            ))
            .unwrap();
        catalog
            .conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();

        let query = CatalogQuery {
            limit: 100,
            ..CatalogQuery::default()
        };
        let first = catalog.page(&query).unwrap();
        let second_query = CatalogQuery {
            limit: 100,
            cursor: first.next_cursor,
            ..CatalogQuery::default()
        };
        let filtered_query = CatalogQuery {
            limit: 100,
            cwd: Some("/even".into()),
            availability: Some(CatalogAvailability::Available),
            ..CatalogQuery::default()
        };
        catalog.page(&second_query).unwrap();
        catalog.page(&filtered_query).unwrap();

        let mut first_page_us = Vec::with_capacity(runs);
        let mut second_page_us = Vec::with_capacity(runs);
        let mut filtered_page_us = Vec::with_capacity(runs);
        for _ in 0..runs {
            for (query, samples) in [
                (&query, &mut first_page_us),
                (&second_query, &mut second_page_us),
                (&filtered_query, &mut filtered_page_us),
            ] {
                let started = std::time::Instant::now();
                assert_eq!(catalog.page(query).unwrap().sessions.len(), 100);
                samples.push(started.elapsed().as_micros() as u64);
            }
        }
        for samples in [
            &mut first_page_us,
            &mut second_page_us,
            &mut filtered_page_us,
        ] {
            samples.sort_unstable();
        }
        let median = |samples: &[u64]| samples[samples.len() / 2];
        println!(
            "CATALOG_BENCH_JSON {}",
            serde_json::json!({
                "rows": row_count,
                "runs": runs,
                "page_size": 100,
                "first_page_median_us": median(&first_page_us),
                "second_page_median_us": median(&second_page_us),
                "filtered_page_median_us": median(&filtered_page_us),
                "database_bytes": fs::metadata(temp.path().join("catalog.db")).unwrap().len(),
            })
        );
    }

    #[test]
    fn corrupt_catalog_can_be_deleted_and_rebuilt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("catalog.db");
        fs::write(&path, b"not sqlite").unwrap();
        assert!(Catalog::open(&path).is_err());
        let _lock = CatalogReconcileLock::acquire(temp.path().join(".catalog.lock")).unwrap();
        let archived = archive_corrupt_catalog(&path).unwrap().unwrap();
        assert!(archived.is_file());
        let mut catalog = Catalog::open(&path).unwrap();
        catalog.upsert_available(&row("restored", 1, 10)).unwrap();
        assert_eq!(
            catalog
                .page(&CatalogQuery::default())
                .unwrap()
                .sessions
                .len(),
            1
        );
    }
}
