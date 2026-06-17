use protocol::HistoryItem;
use rusqlite::{params, Connection};
use serde_json::Value;

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::history;
use crate::meta::{self, SessionState};
use crate::object::checked_i64;

pub const SESSION_META_JSON_KEY: &str = "session_meta_json";

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub state: SessionState,
    pub meta_json: Option<Value>,
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
    compression: ObjectCompression,
) -> Result<SessionSaveReport> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = save_session_snapshot_inner(conn, snapshot, expected_revision, compression);
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
    compression: ObjectCompression,
) -> Result<SessionSaveReport> {
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
    let new_hashes = snapshot
        .history
        .iter()
        .map(history::item_hash)
        .collect::<Result<Vec<_>>>()?;
    let common_len = current_hashes
        .iter()
        .zip(new_hashes.iter())
        .take_while(|(current, new)| current.hash == **new)
        .count();
    let history_deleted = current_hashes.len().saturating_sub(common_len) as u64;
    let history_inserted = new_hashes.len().saturating_sub(common_len) as u64;

    if history_deleted > 0 || history_inserted > 0 {
        history::replace_history_suffix(
            conn,
            common_len,
            &snapshot.history[common_len..],
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
    state.history_len = snapshot.history.len() as u64;
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

    conn.execute("DELETE FROM turn_metas", [])?;
    conn.execute("DELETE FROM turn_tool_elapsed", [])?;
    for (idx, value) in &snapshot.turn_metas {
        conn.execute(
            "INSERT INTO turn_metas (turn_idx, meta_json) VALUES (?1, ?2)",
            params![
                checked_i64(*idx, "turn_idx")?,
                serde_json::to_string(value)?
            ],
        )?;
    }

    conn.execute("DELETE FROM metadata_snapshots", [])?;
    for (idx, value) in &snapshot.metadata_snapshots {
        conn.execute(
            "INSERT INTO metadata_snapshots (history_idx, metadata_json) VALUES (?1, ?2)",
            params![
                checked_i64(*idx, "metadata_history_idx")?,
                serde_json::to_string(value)?
            ],
        )?;
    }

    conn.execute("DELETE FROM accounting_snapshots", [])?;
    for (idx, value) in &snapshot.accounting_snapshots {
        conn.execute(
            "INSERT INTO accounting_snapshots (history_idx, accounting_json) VALUES (?1, ?2)",
            params![
                checked_i64(*idx, "accounting_history_idx")?,
                serde_json::to_string(value)?
            ],
        )?;
    }
    Ok(true)
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
    Ok(Some(SessionSnapshot {
        state,
        meta_json,
        history: history::read_history_items(conn)?,
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
