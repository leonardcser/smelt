use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::error::{Result, StoreError};
use crate::Store;

pub const SCHEMA_VERSION: i32 = 1;
/// `PRAGMA application_id` of a smelt database: "smlt".
const APPLICATION_ID: i32 = 0x736d_6c74;
const SCHEMA: &str = include_str!("schema.sql");
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// SQLite keeps a checkpointed WAL at its largest size unless this caps it.
const WAL_SIZE_LIMIT: i64 = 1024 * 1024;
/// Checkpoint before the WAL outgrows its size limit, in 4 KiB pages.
const WAL_CHECKPOINT_PAGES: i64 = WAL_SIZE_LIMIT / 4096;

/// Opens the shared database, creating it and its schema on first use. A file
/// with another schema is refused without writing to it.
pub(crate) fn open(store: &Store) -> Result<Connection> {
    let path = &store.database_path();
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
    match schema_of(&conn)? {
        (APPLICATION_ID, SCHEMA_VERSION) => {}
        (0, 0) => create(&conn)?,
        (application_id, version) => {
            return Err(StoreError::UnsupportedSchema {
                found: if application_id == APPLICATION_ID {
                    version
                } else {
                    0
                },
                expected: SCHEMA_VERSION,
            })
        }
    }
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)?;
    conn.pragma_update(None, "wal_autocheckpoint", WAL_CHECKPOINT_PAGES)?;
    Ok(conn)
}

fn schema_of(conn: &Connection) -> Result<(i32, i32)> {
    Ok((
        conn.query_row("PRAGMA application_id", [], |row| row.get(0))?,
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))?,
    ))
}

/// Creates the schema in a new database, unless another process did while this
/// one waited for the write lock.
fn create(conn: &Connection) -> Result<()> {
    // Auto-vacuum must be chosen before WAL and the first table.
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL")?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        match schema_of(conn)? {
            (0, 0) => {}
            (APPLICATION_ID, SCHEMA_VERSION) => return Ok(()),
            (_, found) => {
                return Err(StoreError::UnsupportedSchema {
                    found,
                    expected: SCHEMA_VERSION,
                })
            }
        }
        let tables: i64 =
            conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
        if tables > 0 {
            return Err(StoreError::UnsupportedSchema {
                found: 0,
                expected: SCHEMA_VERSION,
            });
        }
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "application_id", APPLICATION_ID)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(err);
        }
    }
    Ok(())
}

/// Returns up to `pages` free pages, or all of them, to the file system. The
/// pragma frees one page per step, so it must run to completion.
pub(crate) fn incremental_vacuum(conn: &Connection, pages: Option<u32>) -> Result<()> {
    let sql = match pages {
        Some(pages) => format!("PRAGMA incremental_vacuum({pages})"),
        None => "PRAGMA incremental_vacuum".to_owned(),
    };
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.raw_query();
    while rows.next()?.is_some() {}
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
    let conn = open(&Store::new(dir.path())).unwrap();
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
        let conn = open(&Store::new(dir.path())).unwrap();
        let mode: i32 = conn
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, 2);
    }

    #[test]
    fn incremental_vacuum_returns_every_requested_page() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&Store::new(dir.path())).unwrap();
        conn.execute_batch(
            "CREATE TABLE filler (body BLOB);
             INSERT INTO filler VALUES (zeroblob(4 * 1024 * 1024));
             DROP TABLE filler;",
        )
        .unwrap();
        let free = || -> i64 {
            conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))
                .unwrap()
        };
        let before = free();
        incremental_vacuum(&conn, Some(100)).unwrap();
        assert_eq!(free(), before - 100);
        incremental_vacuum(&conn, None).unwrap();
        assert_eq!(free(), 0);
    }

    #[test]
    fn wal_stays_near_its_size_limit_while_writing() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&Store::new(dir.path())).unwrap();
        conn.execute_batch("CREATE TABLE filler (body BLOB)")
            .unwrap();
        let wal = dir.path().join("smelt.db-wal");
        let mut largest = 0;
        for _ in 0..200 {
            conn.execute("INSERT INTO filler VALUES (zeroblob(65536))", [])
                .unwrap();
            largest = largest.max(std::fs::metadata(&wal).unwrap().len());
        }
        // 200 saves of 64 KiB write 12.5 MiB in all.
        assert!(
            largest <= 2 * WAL_SIZE_LIMIT as u64,
            "WAL reached {largest} bytes"
        );
    }

    #[test]
    fn other_databases_are_refused_without_writing() {
        for setup in [
            "PRAGMA user_version = 3; CREATE TABLE sessions (id TEXT);",
            "CREATE TABLE notes (body TEXT);",
            "PRAGMA application_id = 1936551028; PRAGMA user_version = 99;",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path());
            Connection::open(store.database_path())
                .unwrap()
                .execute_batch(setup)
                .unwrap();
            let before = fs::read(store.database_path()).unwrap();
            assert!(
                matches!(open(&store), Err(StoreError::UnsupportedSchema { .. })),
                "{setup}"
            );
            assert_eq!(fs::read(store.database_path()).unwrap(), before, "{setup}");
        }
    }

    #[test]
    fn opening_an_existing_database_does_not_wait_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let writer = open(&store).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let reader = open(&store).unwrap();
        reader
            .query_row("SELECT count(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        writer.execute_batch("ROLLBACK").unwrap();
    }
}
