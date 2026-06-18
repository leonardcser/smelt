use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::{to_sql_error, Result, StoreError};
use crate::object::checked_i64;
use crate::schema::SCHEMA_VERSION;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionState {
    pub id: String,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub model: Option<String>,
    pub accounting_json: Option<serde_json::Value>,
    pub checkpoint_json: Option<serde_json::Value>,
    pub revision: u64,
    pub history_len: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub model: Option<String>,
    pub revision: u64,
    pub history_len: u64,
    pub updated_at: i64,
    pub schema_version: i32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriterLease {
    pub owner_id: String,
    pub hostname: String,
    pub pid: u32,
    pub app_version: String,
    pub started_at: i64,
    pub heartbeat_at: i64,
}

pub(crate) fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO store_meta (key, value, updated_at)
         VALUES (?1, ?2, unixepoch())
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (key, value),
    )?;
    Ok(())
}

pub(crate) fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}

pub(crate) fn set_writer_lease(conn: &Connection, lease: &WriterLease) -> Result<()> {
    set_meta(conn, "writer_lease", &serde_json::to_string(lease)?)
}

pub(crate) fn writer_lease(conn: &Connection) -> Result<Option<WriterLease>> {
    meta(conn, "writer_lease")?
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

pub(crate) fn acquire_writer_lease(
    conn: &Connection,
    lease: &WriterLease,
    stale_after_secs: i64,
) -> Result<()> {
    if let Some(existing) = writer_lease(conn)? {
        let stale = lease.heartbeat_at.saturating_sub(existing.heartbeat_at) > stale_after_secs
            || same_host_dead_writer(&existing, lease);
        let same_owner = existing.owner_id == lease.owner_id;
        if !same_owner && !stale {
            return Err(StoreError::Integrity(format!(
                "session has active writer lease from pid {} on {}",
                existing.pid, existing.hostname
            )));
        }
    }
    set_writer_lease(conn, lease)
}

fn same_host_dead_writer(existing: &WriterLease, lease: &WriterLease) -> bool {
    let same_host = existing.hostname == lease.hostname
        || existing.hostname == "unknown-host"
        || lease.hostname == "unknown-host";
    same_host && !process_is_alive(existing.pid)
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    std::path::Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

pub(crate) fn clear_writer_lease(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM store_meta WHERE key = 'writer_lease'", [])?;
    Ok(())
}

pub(crate) fn upsert_session_state(conn: &Connection, state: &SessionState) -> Result<()> {
    let accounting_json = optional_json_string(&state.accounting_json)?;
    let checkpoint_json = optional_json_string(&state.checkpoint_json)?;
    conn.execute(
        "INSERT INTO session_state (
            singleton, id, title, slug, cwd, mode, model, accounting_json, checkpoint_json,
            revision, history_len, created_at, updated_at
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(singleton) DO UPDATE SET
            id = excluded.id,
            title = excluded.title,
            slug = excluded.slug,
            cwd = excluded.cwd,
            mode = excluded.mode,
            model = excluded.model,
            accounting_json = excluded.accounting_json,
            checkpoint_json = excluded.checkpoint_json,
            revision = excluded.revision,
            history_len = excluded.history_len,
            updated_at = excluded.updated_at",
        (
            &state.id,
            &state.title,
            &state.slug,
            &state.cwd,
            &state.mode,
            &state.model,
            &accounting_json,
            &checkpoint_json,
            checked_i64(state.revision, "revision")?,
            checked_i64(state.history_len, "history_len")?,
            state.created_at,
            state.updated_at,
        ),
    )?;
    Ok(())
}

pub(crate) fn session_state(conn: &Connection) -> Result<Option<SessionState>> {
    conn.query_row(
        "SELECT id, title, slug, cwd, mode, model, accounting_json, checkpoint_json,
                revision, history_len, created_at, updated_at
         FROM session_state
         WHERE singleton = 1",
        [],
        |row| {
            let accounting_json: Option<String> = row.get(6)?;
            let checkpoint_json: Option<String> = row.get(7)?;
            Ok(SessionState {
                id: row.get(0)?,
                title: row.get(1)?,
                slug: row.get(2)?,
                cwd: row.get(3)?,
                mode: row.get(4)?,
                model: row.get(5)?,
                accounting_json: parse_optional_json(accounting_json)?,
                checkpoint_json: parse_optional_json(checkpoint_json)?,
                revision: row.get::<_, i64>(8)? as u64,
                history_len: row.get::<_, i64>(9)? as u64,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn session_meta(conn: &Connection) -> Result<Option<SessionMeta>> {
    Ok(session_state(conn)?.map(|state| SessionMeta {
        id: state.id,
        title: state.title,
        slug: state.slug,
        cwd: state.cwd,
        mode: state.mode,
        model: state.model,
        revision: state.revision,
        history_len: state.history_len,
        updated_at: state.updated_at,
        schema_version: SCHEMA_VERSION,
    }))
}

pub(crate) fn write_meta_sidecar(
    conn: &Connection,
    path: impl AsRef<Path>,
) -> Result<Option<SessionMeta>> {
    let Some(meta) = session_meta(conn)? else {
        return Ok(None);
    };
    let bytes = serde_json::to_vec_pretty(&meta)?;
    fs::write(path, bytes)?;
    Ok(Some(meta))
}

fn optional_json_string(value: &Option<serde_json::Value>) -> Result<Option<String>> {
    value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

fn parse_optional_json(value: Option<String>) -> rusqlite::Result<Option<serde_json::Value>> {
    value
        .map(|text| serde_json::from_str(&text).map_err(to_sql_error))
        .transpose()
}
