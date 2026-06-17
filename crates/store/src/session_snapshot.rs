use protocol::HistoryItem;
use rusqlite::{params, Connection};
use serde_json::Value;

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::history;
use crate::meta::{self, SessionState, WriterLease};
use crate::object::checked_i64;

pub const SESSION_META_JSON_KEY: &str = "session_meta_json";

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub state: SessionState,
    pub meta_json: Option<Value>,
    /// Changed history suffix. `history_start_idx == 0` means this is a full
    /// snapshot; otherwise rows below `history_start_idx` are an expected
    /// unchanged prefix already present in SQLite.
    pub history_start_idx: usize,
    pub history_len: usize,
    pub history: Vec<HistoryItem>,
    pub turn_metas: Vec<(u64, Value)>,
    pub metadata_snapshots: Vec<(u64, Value)>,
    pub accounting_snapshots: Vec<(u64, Value)>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionSaveReport {
    pub history_deleted: u64,
    pub history_inserted: u64,
    pub history_unchanged: u64,
    pub revision: u64,
    pub changed: bool,
}

pub(crate) fn save_session_snapshot(
    conn: &Connection,
    snapshot: &SessionSnapshot,
    expected_revision: Option<u64>,
    writer_lease: Option<&WriterLease>,
    compression: ObjectCompression,
) -> Result<SessionSaveReport> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result =
        save_session_snapshot_inner(conn, snapshot, expected_revision, writer_lease, compression);
    match result {
        Ok(report) => {
            conn.execute_batch("COMMIT")?;
            Ok(report)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

fn save_session_snapshot_inner(
    conn: &Connection,
    snapshot: &SessionSnapshot,
    expected_revision: Option<u64>,
    writer_lease: Option<&WriterLease>,
    compression: ObjectCompression,
) -> Result<SessionSaveReport> {
    if let Some(lease) = writer_lease {
        meta::acquire_writer_lease(conn, lease, 30 * 60)?;
    }
    let current_state = meta::session_state(conn)?;
    if let Some(expected_revision) = expected_revision {
        let current_revision = current_state.as_ref().map_or(0, |state| state.revision);
        if current_revision != expected_revision {
            return Err(StoreError::Integrity(format!(
                "session revision changed: expected {expected_revision}, found {current_revision}"
            )));
        }
    }

    let current_hashes = history::history_hashes(conn)?;
    let history_start = snapshot.history_start_idx.min(snapshot.history_len);
    if snapshot.history_len != history_start + snapshot.history.len() {
        return Err(StoreError::Integrity(format!(
            "history suffix shape is invalid: start {history_start}, suffix {}, final {}",
            snapshot.history.len(),
            snapshot.history_len
        )));
    }
    if history_start > current_hashes.len() {
        return Err(StoreError::Integrity(format!(
            "history unchanged prefix exceeds stored rows: prefix {history_start}, stored {}",
            current_hashes.len()
        )));
    }

    let new_hashes = snapshot
        .history
        .iter()
        .map(history::item_hash)
        .collect::<Result<Vec<_>>>()?;
    let suffix_common = current_hashes[history_start..]
        .iter()
        .zip(new_hashes.iter())
        .take_while(|(current, new)| current.hash == **new)
        .count();
    let common_len = history_start + suffix_common;
    let history_deleted = current_hashes.len().saturating_sub(common_len) as u64;
    let history_inserted = snapshot.history_len.saturating_sub(common_len) as u64;

    if history_deleted > 0 || history_inserted > 0 {
        let suffix_offset = common_len.saturating_sub(history_start);
        history::replace_history_suffix(
            conn,
            common_len,
            &snapshot.history[suffix_offset..],
            compression,
        )?;
    }

    let meta_json = snapshot
        .meta_json
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let current_meta_json = meta::meta(conn, SESSION_META_JSON_KEY)?;
    let snapshot_tables_changed = replace_snapshot_tables_if_changed(conn, snapshot)?;
    let state_changed = session_state_changed(current_state.as_ref(), &snapshot.state);
    let changed = history_deleted > 0
        || history_inserted > 0
        || state_changed
        || current_meta_json != meta_json
        || snapshot_tables_changed;

    let mut state = snapshot.state.clone();
    state.history_len = snapshot.history_len as u64;
    state.revision = if changed {
        current_state.as_ref().map_or(1, |state| state.revision + 1)
    } else {
        current_state
            .as_ref()
            .map_or(snapshot.state.revision, |state| state.revision)
    };
    meta::upsert_session_state(conn, &state)?;
    match meta_json {
        Some(meta_json) => meta::set_meta(conn, SESSION_META_JSON_KEY, &meta_json)?,
        None => {
            conn.execute(
                "DELETE FROM store_meta WHERE key = ?1",
                [SESSION_META_JSON_KEY],
            )?;
        }
    }

    Ok(SessionSaveReport {
        history_deleted,
        history_inserted,
        history_unchanged: common_len as u64,
        revision: state.revision,
        changed,
    })
}

fn session_state_changed(current: Option<&SessionState>, next: &SessionState) -> bool {
    let Some(current) = current else {
        return true;
    };
    current.id != next.id
        || current.title != next.title
        || current.slug != next.slug
        || current.cwd != next.cwd
        || current.mode != next.mode
        || current.model != next.model
        || current.accounting_json != next.accounting_json
        || current.checkpoint_json != next.checkpoint_json
        || current.history_len != next.history_len
        || current.created_at != next.created_at
        || current.updated_at != next.updated_at
}

fn replace_snapshot_tables_if_changed(
    conn: &Connection,
    snapshot: &SessionSnapshot,
) -> Result<bool> {
    let next_turn_metas = snapshot_rows_json(&snapshot.turn_metas)?;
    let next_metadata = snapshot_rows_json(&snapshot.metadata_snapshots)?;
    let next_accounting = snapshot_rows_json(&snapshot.accounting_snapshots)?;
    let changed = table_rows_json(conn, "turn_metas", "turn_idx", "meta_json")? != next_turn_metas
        || table_rows_json(conn, "metadata_snapshots", "history_idx", "metadata_json")?
            != next_metadata
        || table_rows_json(
            conn,
            "accounting_snapshots",
            "history_idx",
            "accounting_json",
        )? != next_accounting;
    if !changed {
        return Ok(false);
    }

    sync_snapshot_table(
        conn,
        "turn_metas",
        "turn_idx",
        "meta_json",
        &next_turn_metas,
    )?;
    sync_snapshot_table(
        conn,
        "metadata_snapshots",
        "history_idx",
        "metadata_json",
        &next_metadata,
    )?;
    sync_snapshot_table(
        conn,
        "accounting_snapshots",
        "history_idx",
        "accounting_json",
        &next_accounting,
    )?;
    Ok(true)
}

fn sync_snapshot_table(
    conn: &Connection,
    table: &'static str,
    idx_col: &'static str,
    json_col: &'static str,
    rows: &[(u64, String)],
) -> Result<()> {
    let keep = rows
        .iter()
        .map(|(idx, _)| checked_i64(*idx, idx_col))
        .collect::<Result<Vec<_>>>()?;
    if keep.is_empty() {
        let sql = format!("DELETE FROM {table}");
        conn.execute(&sql, [])?;
    } else {
        let placeholders = std::iter::repeat_n("?", keep.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("DELETE FROM {table} WHERE {idx_col} NOT IN ({placeholders})");
        conn.execute(&sql, rusqlite::params_from_iter(keep.iter()))?;
    }

    let sql = format!(
        "INSERT INTO {table} ({idx_col}, {json_col}) VALUES (?1, ?2)
         ON CONFLICT({idx_col}) DO UPDATE SET {json_col} = excluded.{json_col}"
    );
    for (idx, value) in rows {
        conn.execute(&sql, params![checked_i64(*idx, idx_col)?, value])?;
    }
    Ok(())
}

fn snapshot_rows_json(rows: &[(u64, Value)]) -> Result<Vec<(u64, String)>> {
    rows.iter()
        .map(|(idx, value)| Ok((*idx, serde_json::to_string(value)?)))
        .collect()
}

fn table_rows_json(
    conn: &Connection,
    table: &'static str,
    idx_col: &'static str,
    json_col: &'static str,
) -> Result<Vec<(u64, String)>> {
    let sql = format!("SELECT {idx_col}, {json_col} FROM {table} ORDER BY {idx_col}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?))
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub(crate) fn load_session_snapshot(conn: &Connection) -> Result<Option<SessionSnapshot>> {
    let Some(state) = meta::session_state(conn)? else {
        return Ok(None);
    };
    let meta_json = meta::meta(conn, SESSION_META_JSON_KEY)?
        .map(|value| serde_json::from_str(&value).map_err(StoreError::from))
        .transpose()?;
    let history = history::read_history_items(conn)?;
    Ok(Some(SessionSnapshot {
        state,
        meta_json,
        history_start_idx: 0,
        history_len: history.len(),
        history,
        turn_metas: read_snapshot_rows(conn, "turn_metas", "turn_idx", "meta_json")?,
        metadata_snapshots: read_snapshot_rows(
            conn,
            "metadata_snapshots",
            "history_idx",
            "metadata_json",
        )?,
        accounting_snapshots: read_snapshot_rows(
            conn,
            "accounting_snapshots",
            "history_idx",
            "accounting_json",
        )?,
    }))
}

fn read_snapshot_rows(
    conn: &Connection,
    table: &'static str,
    idx_col: &'static str,
    json_col: &'static str,
) -> Result<Vec<(u64, Value)>> {
    let sql = format!("SELECT {idx_col}, {json_col} FROM {table} ORDER BY {idx_col}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let json: String = row.get(1)?;
        Ok((
            row.get::<_, i64>(0)? as u64,
            serde_json::from_str(&json).map_err(crate::error::to_sql_error)?,
        ))
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub(crate) fn history_text_bytes(conn: &Connection) -> Result<u64> {
    history::history_text_bytes(conn)
}

pub(crate) fn search_blob(conn: &Connection) -> Result<String> {
    history::search_blob(conn)
}
