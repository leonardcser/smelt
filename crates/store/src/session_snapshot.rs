use protocol::HistoryItem;
use rusqlite::{params, Connection};
use serde_json::Value;
use smelt_perf::perf;

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::history;
use crate::meta::{self, SessionState};
use crate::object::checked_i64;
use crate::session_commit::{HistorySuffix, SideTableSuffixes};

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub state: SessionState,
    /// Changed history suffix. `history_start_idx == 0` means this is a full
    /// snapshot; otherwise rows below `history_start_idx` are an expected
    /// unchanged prefix already present in SQLite.
    pub history_start_idx: usize,
    pub history_len: usize,
    pub history: Vec<HistoryItem>,
    pub turn_metas: Vec<(u64, Value)>,
    pub metadata_snapshots: Vec<(u64, Value)>,
    pub context_snapshots: Vec<(u64, Value)>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionSaveReport {
    pub history_deleted: u64,
    pub history_inserted: u64,
    pub history_unchanged: u64,
    pub revision: u64,
    pub changed: bool,
}

pub(crate) fn save_session_snapshot_in_transaction(
    conn: &Connection,
    snapshot: &SessionSnapshot,
    expected_revision: Option<u64>,
    owner_token: Option<&str>,
    compression: ObjectCompression,
) -> Result<SessionSaveReport> {
    let _perf = perf::begin("store:session:save_snapshot_transaction");
    perf::record_value(
        "store:session:dirty_suffix_history_rows",
        snapshot.history.len() as u64,
    );
    perf::record_value(
        "store:session:total_history_rows",
        snapshot.history_len as u64,
    );
    if let Some(token) = owner_token {
        meta::verify_writer_owner(conn, token)?;
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

    let current_len = current_state
        .as_ref()
        .map_or(0, |state| state.history_len as usize);
    let history_start = snapshot.history_start_idx.min(snapshot.history_len);
    if snapshot.history_len != history_start + snapshot.history.len() {
        return Err(StoreError::Integrity(format!(
            "history suffix shape is invalid: start {history_start}, suffix {}, final {}",
            snapshot.history.len(),
            snapshot.history_len
        )));
    }
    if history_start > current_len {
        return Err(StoreError::Integrity(format!(
            "history unchanged prefix exceeds stored rows: prefix {history_start}, stored {current_len}",
        )));
    }

    let current_suffix_hashes = history::history_hashes_from(conn, history_start)?;
    let new_hashes = snapshot
        .history
        .iter()
        .map(history::item_hash)
        .collect::<Result<Vec<_>>>()?;
    let suffix_common = current_suffix_hashes
        .iter()
        .zip(new_hashes.iter())
        .take_while(|(current, new)| current.hash == **new)
        .count();
    let common_len = history_start + suffix_common;
    let history_deleted = current_len.saturating_sub(common_len) as u64;
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

    let snapshot_tables_changed = replace_snapshot_tables_if_changed(conn, snapshot)?;
    let state_changed = session_state_changed(current_state.as_ref(), &snapshot.state);
    let changed =
        history_deleted > 0 || history_inserted > 0 || state_changed || snapshot_tables_changed;

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

    let report = SessionSaveReport {
        history_deleted,
        history_inserted,
        history_unchanged: common_len as u64,
        revision: state.revision,
        changed,
    };
    record_session_save_report(&report);
    Ok(report)
}

pub(crate) fn apply_session_commit_history_in_transaction(
    conn: &Connection,
    state: &SessionState,
    history: &HistorySuffix,
    side_tables: &SideTableSuffixes,
    descriptors_changed: bool,
    expected_revision: Option<u64>,
    compression: ObjectCompression,
) -> Result<SessionSaveReport> {
    let _perf = perf::begin("store:session:save_history_suffix_transaction");
    perf::record_value(
        "store:session:dirty_suffix_history_rows",
        history.items.len() as u64,
    );
    perf::record_value("store:session:total_history_rows", history.final_len.get());
    let current_state = meta::session_state(conn)?;
    if let Some(expected_revision) = expected_revision {
        let current_revision = current_state.as_ref().map_or(0, |state| state.revision);
        if current_revision != expected_revision {
            return Err(StoreError::Integrity(format!(
                "session revision changed: expected {expected_revision}, found {current_revision}"
            )));
        }
    }

    let current_len = current_state
        .as_ref()
        .map_or(0, |state| state.history_len as usize);
    let history_start = history.start.as_usize().ok_or_else(|| {
        StoreError::Integrity(format!(
            "history index {} does not fit usize",
            history.start.get()
        ))
    })?;
    let history_len = history.final_len.as_usize().ok_or_else(|| {
        StoreError::Integrity(format!(
            "history length {} does not fit usize",
            history.final_len.get()
        ))
    })?;
    if history_start.checked_add(history.items.len()) != Some(history_len) {
        return Err(StoreError::Integrity(format!(
            "history suffix shape is invalid: start {history_start}, suffix {}, final {history_len}",
            history.items.len(),
        )));
    }
    if history_start > current_len {
        return Err(StoreError::Integrity(format!(
            "history unchanged prefix exceeds stored rows: prefix {history_start}, stored {current_len}",
        )));
    }

    let history_deleted = current_len.saturating_sub(history_start) as u64;
    let history_inserted = history_len.saturating_sub(history_start) as u64;
    if history_deleted > 0 || history_inserted > 0 {
        history::replace_history_suffix(conn, history_start, &history.items, compression)?;
    }

    let state_changed = session_state_changed(current_state.as_ref(), state);
    let side_tables_changed = replace_typed_side_table_suffixes_if_changed(conn, side_tables)?;
    let changed = history_deleted > 0
        || history_inserted > 0
        || state_changed
        || side_tables_changed
        || descriptors_changed;
    let mut state = state.clone();
    state.history_len = history.final_len.get();
    state.revision = if changed {
        current_state.as_ref().map_or(1, |state| state.revision + 1)
    } else {
        current_state
            .as_ref()
            .map_or(state.revision, |state| state.revision)
    };
    meta::upsert_session_state(conn, &state)?;

    let report = SessionSaveReport {
        history_deleted,
        history_inserted,
        history_unchanged: history_start as u64,
        revision: state.revision,
        changed,
    };
    record_session_save_report(&report);
    Ok(report)
}

fn record_session_save_report(report: &SessionSaveReport) {
    perf::record_value("store:session:history_rows_deleted", report.history_deleted);
    perf::record_value(
        "store:session:history_rows_inserted",
        report.history_inserted,
    );
    perf::record_value(
        "store:session:history_rows_unchanged",
        report.history_unchanged,
    );
    perf::record_value(
        "store:session:db_writes_changed",
        if report.changed { 1 } else { 0 },
    );
}

fn session_state_changed(current: Option<&SessionState>, next: &SessionState) -> bool {
    let Some(current) = current else {
        return true;
    };
    current.id != next.id
        || current.title != next.title
        || current.slug != next.slug
        || current.first_user_message != next.first_user_message
        || current.cwd != next.cwd
        || current.mode != next.mode
        || current.reasoning_effort != next.reasoning_effort
        || current.model != next.model
        || current.fast_mode != next.fast_mode
        || current.parent_id != next.parent_id
        || current.accounting_json != next.accounting_json
        || current.checkpoint_json != next.checkpoint_json
        || current.context_tokens != next.context_tokens
        || current.context_tokens_history_len != next.context_tokens_history_len
        || current.display_context_tokens != next.display_context_tokens
        || current.session_cost_usd != next.session_cost_usd
        || current.history_len != next.history_len
        || current.created_at != next.created_at
        || current.updated_at != next.updated_at
}

fn replace_snapshot_tables_if_changed(
    conn: &Connection,
    snapshot: &SessionSnapshot,
) -> Result<bool> {
    let snapshot_start = snapshot.history_start_idx.min(snapshot.history_len) as u64;
    replace_side_table_suffixes_if_changed(
        conn,
        snapshot_start,
        &snapshot.turn_metas,
        &snapshot.metadata_snapshots,
        &snapshot.context_snapshots,
    )
}

fn replace_typed_side_table_suffixes_if_changed(
    conn: &Connection,
    side_tables: &SideTableSuffixes,
) -> Result<bool> {
    let start = side_tables.start.get();
    let turn_metas = typed_snapshot_rows_json_from(&side_tables.turn_metas, start)?;
    let metadata = typed_snapshot_rows_json_from(&side_tables.metadata_snapshots, start)?;
    let context = typed_snapshot_rows_json_from(&side_tables.context_snapshots, start)?;
    sync_serialized_side_table_suffixes(conn, start, &turn_metas, &metadata, &context)
}

fn replace_side_table_suffixes_if_changed(
    conn: &Connection,
    snapshot_start: u64,
    turn_metas: &[(u64, Value)],
    metadata_snapshots: &[(u64, Value)],
    context_snapshots: &[(u64, Value)],
) -> Result<bool> {
    let turn_metas = snapshot_rows_json_from(turn_metas, snapshot_start)?;
    let metadata = snapshot_rows_json_from(metadata_snapshots, snapshot_start)?;
    let context = snapshot_rows_json_from(context_snapshots, snapshot_start)?;
    sync_serialized_side_table_suffixes(conn, snapshot_start, &turn_metas, &metadata, &context)
}

fn sync_serialized_side_table_suffixes(
    conn: &Connection,
    start: u64,
    turn_metas: &[(u64, String)],
    metadata: &[(u64, String)],
    context: &[(u64, String)],
) -> Result<bool> {
    let mut changed = sync_side_table_suffix(conn, SnapshotTable::TurnMetas, start, turn_metas)?;
    changed |= sync_side_table_suffix(conn, SnapshotTable::MetadataSnapshots, start, metadata)?;
    changed |= sync_side_table_suffix(conn, SnapshotTable::ContextSnapshots, start, context)?;
    Ok(changed)
}

#[derive(Clone, Copy)]
enum SnapshotTable {
    TurnMetas,
    MetadataSnapshots,
    ContextSnapshots,
}

impl SnapshotTable {
    fn idx_col(self) -> &'static str {
        match self {
            Self::TurnMetas => "turn_idx",
            Self::MetadataSnapshots | Self::ContextSnapshots => "history_idx",
        }
    }

    fn select_from_sql(self) -> &'static str {
        match self {
            Self::TurnMetas => {
                "SELECT turn_idx, meta_json FROM turn_metas WHERE turn_idx >= ?1 ORDER BY turn_idx"
            }
            Self::MetadataSnapshots => {
                "SELECT history_idx, metadata_json FROM metadata_snapshots WHERE history_idx >= ?1 ORDER BY history_idx"
            }
            Self::ContextSnapshots => {
                "SELECT history_idx, accounting_json FROM accounting_snapshots WHERE history_idx >= ?1 ORDER BY history_idx"
            }
        }
    }

    fn select_all_sql(self) -> &'static str {
        match self {
            Self::TurnMetas => "SELECT turn_idx, meta_json FROM turn_metas ORDER BY turn_idx",
            Self::MetadataSnapshots => {
                "SELECT history_idx, metadata_json FROM metadata_snapshots ORDER BY history_idx"
            }
            Self::ContextSnapshots => {
                "SELECT history_idx, accounting_json FROM accounting_snapshots ORDER BY history_idx"
            }
        }
    }

    fn delete_from_sql(self) -> &'static str {
        match self {
            Self::TurnMetas => "DELETE FROM turn_metas WHERE turn_idx >= ?1",
            Self::MetadataSnapshots => "DELETE FROM metadata_snapshots WHERE history_idx >= ?1",
            Self::ContextSnapshots => "DELETE FROM accounting_snapshots WHERE history_idx >= ?1",
        }
    }

    fn insert_sql(self) -> &'static str {
        match self {
            Self::TurnMetas => {
                "INSERT INTO turn_metas (turn_idx, meta_json) VALUES (?1, ?2)
                 ON CONFLICT(turn_idx) DO UPDATE SET meta_json = excluded.meta_json"
            }
            Self::MetadataSnapshots => {
                "INSERT INTO metadata_snapshots (history_idx, metadata_json) VALUES (?1, ?2)
                 ON CONFLICT(history_idx) DO UPDATE SET metadata_json = excluded.metadata_json"
            }
            Self::ContextSnapshots => {
                "INSERT INTO accounting_snapshots (history_idx, accounting_json) VALUES (?1, ?2)
                 ON CONFLICT(history_idx) DO UPDATE SET accounting_json = excluded.accounting_json"
            }
        }
    }
}

fn sync_side_table_suffix(
    conn: &Connection,
    table: SnapshotTable,
    start_idx: u64,
    rows: &[(u64, String)],
) -> Result<bool> {
    let _perf = perf::begin("store:session:sync_side_table_suffix");
    perf::record_value(
        "store:session:dirty_suffix_side_table_rows",
        rows.len() as u64,
    );
    let existing = read_snapshot_rows_json_from(conn, table, start_idx)?;
    let common = existing
        .iter()
        .zip(rows.iter())
        .take_while(|(current, next)| current == next)
        .count();
    if common == existing.len() && common == rows.len() {
        return Ok(false);
    }

    let delete_from = match (existing.get(common), rows.get(common)) {
        (Some((current_idx, _)), Some((next_idx, _))) => (*current_idx).min(*next_idx),
        (Some((current_idx, _)), None) => *current_idx,
        (None, Some((next_idx, _))) => *next_idx,
        (None, None) => return Ok(false),
    };
    let delete_from = checked_i64(delete_from, table.idx_col())?;
    let deleted = conn.execute(table.delete_from_sql(), [delete_from])?;

    for (idx, value) in &rows[common..] {
        conn.execute(
            table.insert_sql(),
            params![checked_i64(*idx, table.idx_col())?, value],
        )?;
    }
    perf::record_value("store:session:side_table_rows_deleted", deleted as u64);
    perf::record_value(
        "store:session:side_table_rows_inserted",
        rows[common..].len() as u64,
    );
    Ok(true)
}

fn read_snapshot_rows_json_from(
    conn: &Connection,
    table: SnapshotTable,
    start_idx: u64,
) -> Result<Vec<(u64, String)>> {
    let start_idx = checked_i64(start_idx, table.idx_col())?;
    let mut stmt = conn.prepare(table.select_from_sql())?;
    let rows = stmt.query_map([start_idx], |row| {
        Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?))
    })?;
    let rows = rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(crate::error::StoreError::from)?;
    perf::record_value("store:session:side_table_rows_read", rows.len() as u64);
    Ok(rows)
}

fn typed_snapshot_rows_json_from(
    rows: &[(crate::session_commit::HistoryIndex, Value)],
    start_idx: u64,
) -> Result<Vec<(u64, String)>> {
    rows.iter()
        .map(|(idx, value)| (idx.get(), value))
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_iter()
        .filter(|(idx, _)| *idx >= start_idx)
        .map(|(idx, value)| Ok((idx, serde_json::to_string(value)?)))
        .collect()
}

fn snapshot_rows_json_from(rows: &[(u64, Value)], start_idx: u64) -> Result<Vec<(u64, String)>> {
    rows.iter()
        .filter(|(idx, _)| *idx >= start_idx)
        .map(|(idx, value)| Ok((*idx, serde_json::to_string(value)?)))
        .collect()
}

pub(crate) fn load_session_snapshot(conn: &Connection) -> Result<Option<SessionSnapshot>> {
    let _perf = perf::begin("store:session:load_full_snapshot");
    let Some(state) = meta::session_state(conn)? else {
        perf::record_value("store:session:full_snapshot_rows_read", 0);
        return Ok(None);
    };
    let history = history::read_history_items(conn)?;
    let turn_metas = read_snapshot_rows(conn, SnapshotTable::TurnMetas)?;
    let metadata_snapshots = read_snapshot_rows(conn, SnapshotTable::MetadataSnapshots)?;
    let context_snapshots = read_snapshot_rows(conn, SnapshotTable::ContextSnapshots)?;
    perf::record_value(
        "store:session:full_snapshot_rows_read",
        history.len() as u64,
    );
    perf::record_value(
        "store:session:full_snapshot_table_rows_read",
        turn_metas
            .len()
            .saturating_add(metadata_snapshots.len())
            .saturating_add(context_snapshots.len()) as u64,
    );
    Ok(Some(SessionSnapshot {
        state,
        history_start_idx: 0,
        history_len: history.len(),
        history,
        turn_metas,
        metadata_snapshots,
        context_snapshots,
    }))
}

fn read_snapshot_rows(conn: &Connection, table: SnapshotTable) -> Result<Vec<(u64, Value)>> {
    let mut stmt = conn.prepare(table.select_all_sql())?;
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
