use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::error::{Result, StoreError};
use crate::{lock, transcript, Store};

pub const SCHEMA_VERSION: i32 = 2;
const SCHEMA: &str = include_str!("schema.sql");
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// SQLite keeps a checkpointed WAL at its largest size unless this caps it.
const WAL_SIZE_LIMIT: i64 = 1024 * 1024;
/// Checkpoint before the WAL outgrows its size limit, in 4 KiB pages.
const WAL_CHECKPOINT_PAGES: i64 = WAL_SIZE_LIMIT / 4096;

/// Opens the shared database, creating it and its schema on first use.
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
    conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)?;
    conn.pragma_update(None, "wal_autocheckpoint", WAL_CHECKPOINT_PAGES)?;
    match version {
        SCHEMA_VERSION => Ok(conn),
        0 => {
            upgrade(store, &conn, 0, |conn| Ok(conn.execute_batch(SCHEMA)?))?;
            Ok(conn)
        }
        1 => {
            upgrade(store, &conn, 1, upgrade_v1)?;
            // Give the pages of the dropped columns back to the file system.
            incremental_vacuum(&conn, None)?;
            Ok(conn)
        }
        found => Err(StoreError::UnsupportedSchema {
            found,
            expected: SCHEMA_VERSION,
        }),
    }
}

pub(crate) fn upgrade_pending(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok((1..SCHEMA_VERSION).contains(&version))
}

/// Runs `step` and marks the schema current, in one write transaction, if the
/// database is still at version `from`. Another process may have done it while
/// this one waited for the lock. An existing schema stays as it is while other
/// processes own sessions, since older builds keep writing it until they exit.
fn upgrade(
    store: &Store,
    conn: &Connection,
    from: i32,
    step: impl FnOnce(&Connection) -> Result<()>,
) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == from {
            if from > 0 {
                let sessions = lock::owned_sessions(&store.locks_dir())?;
                if sessions > 0 {
                    return Err(StoreError::UpgradeBlocked { sessions });
                }
            }
            step(conn)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
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

// COMPAT(store-schema-v1): databases from smelt 0.8 development builds keep the
// transcript preview, origin and tool state as plain columns.
fn upgrade_v1(conn: &Connection) -> Result<()> {
    conn.execute_batch("ALTER TABLE transcript ADD COLUMN payload BLOB NOT NULL DEFAULT x''")?;
    let mut select = conn.prepare(
        "SELECT rowid, block, origin, tool_state FROM transcript
         WHERE rowid > ?1 ORDER BY rowid LIMIT 256",
    )?;
    let mut update = conn.prepare("UPDATE transcript SET payload = ?2 WHERE rowid = ?1")?;
    let mut after = 0_i64;
    loop {
        let rows = select
            .query_map([after], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Some(&(last, ..)) = rows.last() else {
            break;
        };
        after = last;
        for (rowid, block, origin, tool_state) in rows {
            let block = String::from_utf8(decode_blob(&block)?.into_owned())
                .map_err(|_| StoreError::Integrity("transcript block is not UTF-8".to_owned()))?;
            let payload =
                transcript::encode_payload(&block, origin.as_deref(), tool_state.as_deref())?;
            update.execute(rusqlite::params![rowid, payload])?;
        }
    }
    conn.execute_batch(
        "ALTER TABLE transcript DROP COLUMN preview;
         ALTER TABLE transcript DROP COLUMN origin;
         ALTER TABLE transcript DROP COLUMN tool_state;
         ALTER TABLE transcript DROP COLUMN block;",
    )?;
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

    // COMPAT(store-schema-v1)
    #[test]
    fn version_1_transcript_rows_move_into_the_payload() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let path = store.database_path();
        let conn = open(&store).unwrap();
        conn.execute_batch(
            "DROP TABLE transcript;
             CREATE TABLE transcript (
                 sid INTEGER NOT NULL REFERENCES sessions (sid) ON DELETE CASCADE,
                 idx INTEGER NOT NULL,
                 block_idx INTEGER NOT NULL,
                 history_idx INTEGER,
                 kind TEXT NOT NULL,
                 role TEXT NOT NULL,
                 rows_20 INTEGER NOT NULL,
                 rows_40 INTEGER NOT NULL,
                 rows_80 INTEGER NOT NULL,
                 rows_120 INTEGER NOT NULL,
                 rows_160 INTEGER NOT NULL,
                 rows_240 INTEGER NOT NULL,
                 text_bytes INTEGER NOT NULL,
                 tool_render_revision INTEGER NOT NULL,
                 tool_call_id TEXT,
                 tool_name TEXT,
                 content_hash TEXT NOT NULL,
                 first_line TEXT NOT NULL,
                 preview TEXT NOT NULL,
                 origin TEXT,
                 tool_state TEXT,
                 indexed_text TEXT NOT NULL,
                 block BLOB NOT NULL,
                 PRIMARY KEY (sid, idx)
             );
             CREATE INDEX transcript_blocks ON transcript (sid, block_idx, idx);
             CREATE INDEX transcript_history ON transcript (sid, history_idx, idx)
                 WHERE history_idx IS NOT NULL;
             CREATE INDEX transcript_kinds ON transcript (sid, kind, idx);
             CREATE INDEX transcript_roles ON transcript (sid, role, idx);
             INSERT INTO sessions (id, created_at, updated_at) VALUES ('s', 0, 0);
             PRAGMA user_version = 1;",
        )
        .unwrap();
        let output = "x".repeat(4096);
        let rows = [
            ("{\"Text\":{}}", None, None),
            (
                "{\"ToolCall\":{}}",
                Some("{\"History\":3}"),
                Some(format!("{{\"output\":\"{output}\"}}")),
            ),
        ];
        for (idx, (block, origin, tool_state)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO transcript VALUES (1, ?1, ?1, NULL, 'tool', 'tool', 1, 1, 1, 1, 1,
                     1, 0, 0, NULL, NULL, '', '', 'preview', ?2, ?3, 'text', ?4)",
                rusqlite::params![
                    idx as i64,
                    origin,
                    tool_state,
                    encode_blob(block.as_bytes())
                ],
            )
            .unwrap();
        }
        drop(conn);

        assert!(upgrade_pending(&path).unwrap());
        let owner = lock::SessionLock::acquire(&store, "s").unwrap();
        assert!(matches!(
            open(&store),
            Err(StoreError::UpgradeBlocked { sessions: 1 })
        ));
        assert!(upgrade_pending(&path).unwrap());
        drop(owner);
        let conn = open(&store).unwrap();
        assert!(!upgrade_pending(&path).unwrap());
        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let payloads = conn
            .prepare("SELECT payload FROM transcript ORDER BY idx")
            .unwrap()
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|payload| transcript::decode_payload(&payload.unwrap()).unwrap())
            .collect::<Vec<_>>();
        let expected = rows
            .iter()
            .map(|(block, origin, tool_state)| {
                (
                    block.to_string(),
                    origin.map(str::to_owned),
                    tool_state.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(payloads, expected);
        let columns = conn
            .prepare("SELECT name FROM pragma_table_info('transcript')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for dropped in ["preview", "origin", "tool_state", "block"] {
            assert!(!columns.iter().any(|name| name == dropped), "{columns:?}");
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
