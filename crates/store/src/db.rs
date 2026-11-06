use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::error::{Result, StoreError};

pub const SCHEMA_VERSION: i32 = 1;
const SCHEMA: &str = include_str!("schema.sql");
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens the shared database, creating it and its schema on first use.
pub(crate) fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags)?;
    restrict_permissions(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.set_prepared_statement_cache_capacity(64);
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0 {
        // Auto-vacuum must be chosen before WAL and the first table. Setting it
        // takes the write lock, so only a new database does it.
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL")?;
    }
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    match version {
        SCHEMA_VERSION => Ok(conn),
        0 => {
            create_schema(&conn)?;
            Ok(conn)
        }
        found => Err(StoreError::UnsupportedSchema {
            found,
            expected: SCHEMA_VERSION,
        }),
    }
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    // Another process may have created the schema while this one waited.
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let result = if version == 0 {
        conn.execute_batch(SCHEMA)
            .and_then(|()| conn.pragma_update(None, "user_version", SCHEMA_VERSION))
    } else {
        Ok(())
    };
    match result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(err.into());
        }
    }
    Ok(())
}

pub(crate) fn session_key(conn: &Connection, session_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT sid FROM sessions WHERE id = ?1")?
        .query_row([session_id], |row| row.get(0))
        .optional()?)
}

pub(crate) fn require_session_key(conn: &Connection, session_id: &str) -> Result<i64> {
    session_key(conn, session_id)?.ok_or_else(|| StoreError::NotFound {
        session_id: session_id.to_owned(),
    })
}

pub fn create_private_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o600 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Compressed JSON bodies start with a tag byte so small ones stay uncompressed.
const RAW: u8 = 0;
const ZSTD: u8 = 1;
const COMPRESS_MIN_BYTES: usize = 1024;
const ZSTD_LEVEL: i32 = 1;

pub(crate) fn encode_blob(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() >= COMPRESS_MIN_BYTES {
        if let Ok(compressed) = zstd::bulk::compress(bytes, ZSTD_LEVEL) {
            // Keep compression only when it saves at least an eighth.
            if compressed.len() + bytes.len() / 8 < bytes.len() {
                let mut out = Vec::with_capacity(compressed.len() + 1);
                out.push(ZSTD);
                out.extend_from_slice(&compressed);
                return out;
            }
        }
    }
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(RAW);
    out.extend_from_slice(bytes);
    out
}

pub(crate) fn decode_blob(blob: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    match blob.split_first() {
        Some((&RAW, body)) => Ok(std::borrow::Cow::Borrowed(body)),
        Some((&ZSTD, body)) => Ok(std::borrow::Cow::Owned(zstd::stream::decode_all(body)?)),
        _ => Err(StoreError::Integrity("unknown blob encoding".into())),
    }
}

pub(crate) fn encode_json(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    Ok(encode_blob(&serde_json::to_vec(value)?))
}

pub(crate) fn decode_json<T: serde::de::DeserializeOwned>(blob: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(&decode_blob(blob)?)?)
}

/// Converts a stored nonnegative integer.
pub(crate) fn unsigned(value: i64) -> u64 {
    value.max(0) as u64
}

pub(crate) fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Fails when `sql` scans a table or walks a whole session's primary key
/// instead of seeking an index that matches its filter.
#[cfg(test)]
pub(crate) fn assert_query_seeks(sql: &str) {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(&dir.path().join("smelt.db")).unwrap();
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    // Unbound parameters plan as NULL, which keeps the same index choice.
    let mut rows = stmt.raw_query();
    let mut plan = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        plan.push(row.get::<_, String>(3).unwrap());
    }
    for step in &plan {
        assert!(
            !step.starts_with("SCAN") && !step.ends_with("(sid=?)"),
            "{sql}\nplans {plan:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_database_uses_incremental_auto_vacuum() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("smelt.db")).unwrap();
        let mode: i32 = conn
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, 2);
    }

    #[test]
    fn opening_an_existing_database_does_not_wait_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("smelt.db");
        let writer = open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let reader = open(&path).unwrap();
        reader
            .query_row("SELECT count(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        writer.execute_batch("ROLLBACK").unwrap();
    }
}
