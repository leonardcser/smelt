use protocol::request_log::RequestLogEntry;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::db::{self, signed, unsigned};
use crate::error::{Result, StoreError};
use crate::history::HistoryRow;
use crate::lock::SessionLock;
use crate::transcript::{self, transcript_role, TranscriptExtentProfile};
use crate::types::*;
use crate::Store;

/// Exclusive writer of one session.
pub struct SessionWriter {
    conn: Connection,
    session_id: String,
    sid: Option<i64>,
    head: StoreHead,
    _lock: SessionLock,
}

impl SessionWriter {
    pub(crate) fn open(store: &Store, session_id: &str) -> Result<Self> {
        let lock = SessionLock::acquire(store, session_id)?;
        let conn = store.connect()?;
        let sid = db::session_key(&conn, session_id)?;
        let mut writer = Self {
            conn,
            session_id: session_id.to_owned(),
            sid,
            head: StoreHead::default(),
            _lock: lock,
        };
        if let Some(sid) = sid {
            writer.head = read_head(&writer.conn, sid)?;
        }
        Ok(writer)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn exists(&self) -> bool {
        self.sid.is_some()
    }

    pub fn head(&self) -> StoreHead {
        self.head
    }

    /// Applies one save atomically. The first commit creates the session.
    pub fn commit(
        &mut self,
        identity: &SessionIdentity,
        commit: &SessionCommit,
    ) -> Result<StoreHead> {
        if identity.id != self.session_id {
            return Err(StoreError::Integrity(format!(
                "commit for session {} reached the writer of {}",
                identity.id, self.session_id
            )));
        }
        let _perf = smelt_perf::perf::begin("store:commit");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sid = match self.sid {
            Some(sid) => sid,
            None => create_session(&tx, identity)?,
        };
        let mut head = read_head(&tx, sid)?;
        let mut text_bytes: i64 = tx
            .prepare_cached("SELECT text_bytes FROM sessions WHERE sid = ?1")?
            .query_row([sid], |row| row.get(0))?;
        if let Some(history) = &commit.history {
            smelt_perf::perf::record_value("store:commit:history_rows", history.items.len() as u64);
            let (len, bytes) = write_history(&tx, sid, head.history_len.get(), history)?;
            head.history_len = HistoryLen::new(len);
            text_bytes = text_bytes.saturating_add(bytes);
        }
        for table in SideTable::ALL {
            if let Some(rows) = commit.side_rows(table) {
                write_side_rows(&tx, sid, table, rows)?;
            }
        }
        if let Some(transcript) = &commit.transcript {
            smelt_perf::perf::record_value(
                "store:commit:transcript_rows",
                transcript.records.len() as u64,
            );
            let count = write_transcript(&tx, sid, head.transcript_record_count.get(), transcript)?;
            head.transcript_record_count = TranscriptRecordCount::new(count);
        }
        if let Some(message) = &commit.first_user_message {
            write_value(&tx, sid, "first_user_message", message.as_deref())?;
        }
        if let Some(checkpoint) = &commit.checkpoint {
            let json = checkpoint.as_ref().map(serde_json::to_string).transpose()?;
            write_value(&tx, sid, "checkpoint", json.as_deref())?;
        }
        write_metadata(&tx, sid, &commit.metadata, head, text_bytes)?;
        tx.commit()?;
        self.sid = Some(sid);
        self.head = head;
        Ok(head)
    }

    pub fn append_request(
        &mut self,
        entry: &RequestLogEntry,
        mode: crate::RequestAuditPayloadMode,
    ) -> Result<i64> {
        let sid = self.sid.ok_or_else(|| StoreError::NotFound {
            session_id: self.session_id.clone(),
        })?;
        crate::requests::append(&self.conn, sid, entry, mode)
    }

    /// Copies this session into a new one.
    pub fn fork_to(
        &mut self,
        target_id: &str,
        created_at: i64,
        store: &Store,
    ) -> Result<StoreHead> {
        let _target = SessionLock::acquire(store, target_id)?;
        fork(&mut self.conn, &self.session_id, target_id, created_at)
    }

    /// Deletes this session and releases it.
    pub fn delete(self, store: &Store) -> Result<()> {
        delete(&self.conn, &self.session_id)?;
        let _ = std::fs::remove_dir_all(store.session_artifact_dir(&self.session_id));
        Ok(())
    }
}

pub(crate) fn read_head(conn: &Connection, sid: i64) -> Result<StoreHead> {
    Ok(conn
        .prepare_cached("SELECT history_len, record_count FROM sessions WHERE sid = ?1")?
        .query_row([sid], |row| {
            Ok(StoreHead {
                history_len: HistoryLen::new(unsigned(row.get(0)?)),
                transcript_record_count: TranscriptRecordCount::new(unsigned(row.get(1)?)),
            })
        })?)
}

fn create_session(tx: &Transaction, identity: &SessionIdentity) -> Result<i64> {
    tx.prepare_cached(
        "INSERT INTO sessions (id, parent_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
    )?
    .execute(params![
        identity.id,
        identity.parent_id,
        identity.created_at
    ])
    .map_err(|err| match err {
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::AlreadyExists {
                session_id: identity.id.clone(),
            }
        }
        err => err.into(),
    })?;
    Ok(tx.last_insert_rowid())
}

fn write_metadata(
    tx: &Transaction,
    sid: i64,
    metadata: &SessionMetadata,
    head: StoreHead,
    text_bytes: i64,
) -> Result<()> {
    let accounting = metadata
        .accounting
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    tx.prepare_cached(
        "UPDATE sessions SET updated_at = ?2, title = ?3, slug = ?4, cwd = ?5, mode = ?6,
             reasoning_effort = ?7, model = ?8, fast_mode = ?9, accounting = ?10,
             context_tokens = ?11, context_tokens_history_len = ?12,
             display_context_tokens = ?13, cost_usd = ?14, history_len = ?15,
             record_count = ?16, text_bytes = ?17, last_turn_id = ?18
         WHERE sid = ?1",
    )?
    .execute(params![
        sid,
        metadata.updated_at,
        metadata.title,
        metadata.slug,
        metadata.cwd,
        metadata.mode,
        metadata.reasoning_effort,
        metadata.model,
        metadata.fast_mode,
        accounting,
        metadata.context_tokens.map(signed),
        metadata.context_tokens_history_len.map(signed),
        metadata.display_context_tokens.map(signed),
        if metadata.cost_usd.is_finite() {
            metadata.cost_usd.max(0.0)
        } else {
            0.0
        },
        signed(head.history_len.get()),
        signed(head.transcript_record_count.get()),
        text_bytes.max(0),
        signed(metadata.last_turn_id),
    ])?;
    Ok(())
}

fn write_value(tx: &Transaction, sid: i64, name: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => tx
            .prepare_cached(
                "INSERT INTO session_values (sid, name, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT (sid, name) DO UPDATE SET value = excluded.value",
            )?
            .execute(params![sid, name, value])?,
        None => tx
            .prepare_cached("DELETE FROM session_values WHERE sid = ?1 AND name = ?2")?
            .execute(params![sid, name])?,
    };
    Ok(())
}

/// Replaces the history suffix. Returns the new length and the change in text bytes.
fn write_history(
    tx: &Transaction,
    sid: i64,
    len: u64,
    suffix: &HistorySuffix,
) -> Result<(u64, i64)> {
    if suffix.start > len {
        return Err(StoreError::InvalidSuffix {
            sequence: "history",
            start: suffix.start,
            len,
        });
    }
    let start = signed(suffix.start);
    let removed: i64 = tx
        .prepare_cached(
            "SELECT coalesce(sum(text_bytes), 0) FROM history WHERE sid = ?1 AND idx >= ?2",
        )?
        .query_row(params![sid, start], |row| row.get(0))?;
    let deleted = tx
        .prepare_cached("DELETE FROM history WHERE sid = ?1 AND idx >= ?2")?
        .execute(params![sid, start])?;
    smelt_perf::perf::record_value("store:commit:history_rows_deleted", deleted as u64);
    let mut users: i64 = if suffix.start == 0 {
        0
    } else {
        tx.prepare_cached("SELECT users FROM history WHERE sid = ?1 AND idx = ?2")?
            .query_row(params![sid, start - 1], |row| row.get(0))?
    };
    let mut added = 0_i64;
    let mut insert = tx.prepare_cached(
        "INSERT INTO history (sid, idx, users, user_turn, visible, note_kind, context, mode,
             base_mode, text_bytes, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for (offset, item) in suffix.items.iter().enumerate() {
        let json = serde_json::to_vec(item)?;
        let row = HistoryRow::of(item);
        users += i64::from(row.user_turn);
        let bytes = json.len() as i64;
        added += bytes;
        insert.execute(params![
            sid,
            start + offset as i64,
            users,
            row.user_turn,
            row.visible,
            row.note_kind,
            row.context,
            row.mode,
            row.base_mode,
            bytes,
            db::encode_blob(&json),
        ])?;
    }
    Ok((suffix.start + suffix.items.len() as u64, added - removed))
}

fn write_side_rows(tx: &Transaction, sid: i64, table: SideTable, suffix: &SideRows) -> Result<()> {
    let start = signed(suffix.start);
    tx.prepare_cached("DELETE FROM side_rows WHERE sid = ?1 AND tbl = ?2 AND key >= ?3")?
        .execute(params![sid, table as i64, start])?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO side_rows (sid, tbl, key, value) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (sid, tbl, key) DO UPDATE SET value = excluded.value",
    )?;
    for (key, value) in &suffix.rows {
        if *key < suffix.start {
            return Err(StoreError::Integrity(format!(
                "side row {key} precedes its suffix start {}",
                suffix.start
            )));
        }
        insert.execute(params![
            sid,
            table as i64,
            signed(*key),
            serde_json::to_string(value)?
        ])?;
    }
    Ok(())
}

pub(crate) fn cumulative_profile(
    conn: &Connection,
    sid: i64,
    before: u64,
) -> Result<TranscriptExtentProfile> {
    if before == 0 {
        return Ok(TranscriptExtentProfile::default());
    }
    Ok(conn
        .prepare_cached(
            "SELECT rows_20, rows_40, rows_80, rows_120, rows_160, rows_240
             FROM transcript WHERE sid = ?1 AND idx = ?2",
        )?
        .query_row(params![sid, signed(before - 1)], profile_from_row(0))
        .optional()?
        .unwrap_or_default())
}

pub(crate) fn profile_from_row(
    first: usize,
) -> impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<TranscriptExtentProfile> {
    move |row| {
        let mut rows = [0_u64; 6];
        for (i, value) in rows.iter_mut().enumerate() {
            *value = unsigned(row.get(first + i)?);
        }
        Ok(TranscriptExtentProfile::new(rows))
    }
}

/// Replaces the transcript suffix. Returns the new record count.
fn write_transcript(
    tx: &Transaction,
    sid: i64,
    len: u64,
    suffix: &TranscriptRecordSuffix,
) -> Result<u64> {
    if suffix.start > len {
        return Err(StoreError::InvalidSuffix {
            sequence: "transcript",
            start: suffix.start,
            len,
        });
    }
    let start = signed(suffix.start);
    let deleted = tx
        .prepare_cached("DELETE FROM transcript WHERE sid = ?1 AND idx >= ?2")?
        .execute(params![sid, start])?;
    smelt_perf::perf::record_value("store:commit:transcript_rows_deleted", deleted as u64);
    let mut cumulative = cumulative_profile(tx, sid, suffix.start)?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO transcript (sid, idx, block_idx, history_idx, kind, role, rows_20, rows_40,
             rows_80, rows_120, rows_160, rows_240, text_bytes, tool_render_revision,
             tool_call_id, tool_name, content_hash, first_line, indexed_text, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
             ?18, ?19, ?20)",
    )?;
    for (offset, record) in suffix.records.iter().enumerate() {
        cumulative = cumulative.add(record.extent_profile());
        let rows = cumulative.rows().map(signed);
        insert.execute(params![
            sid,
            start + offset as i64,
            signed(record.block_idx),
            record.history_idx.map(signed),
            record.kind,
            transcript_role(&record.kind),
            rows[0],
            rows[1],
            rows[2],
            rows[3],
            rows[4],
            rows[5],
            signed(record.estimated_text_bytes),
            signed(record.tool_render_revision),
            record.tool_call_id,
            record.tool_name,
            record.content_hash,
            record.first_line(),
            record.indexed_text,
            transcript::encode_payload(
                &record.block_json,
                record.origin_json.as_deref(),
                record.tool_state_json.as_deref(),
            )?,
        ])?;
    }
    Ok(suffix.start + suffix.records.len() as u64)
}

pub(crate) fn fork(
    conn: &mut Connection,
    source_id: &str,
    target_id: &str,
    created_at: i64,
) -> Result<StoreHead> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let source = db::require_session_key(&tx, source_id)?;
    if db::session_key(&tx, target_id)?.is_some() {
        return Err(StoreError::AlreadyExists {
            session_id: target_id.to_owned(),
        });
    }
    tx.execute(
        "INSERT INTO sessions (id, parent_id, created_at, updated_at, title, slug, cwd, mode,
             reasoning_effort, model, fast_mode, accounting, context_tokens,
             context_tokens_history_len, display_context_tokens, cost_usd, last_turn_id,
             history_len, record_count, text_bytes)
         SELECT ?2, id, ?3, ?3, title, slug, cwd, mode, reasoning_effort, model, fast_mode,
             accounting, context_tokens, context_tokens_history_len, display_context_tokens,
             cost_usd, last_turn_id, history_len, record_count, text_bytes
         FROM sessions WHERE sid = ?1",
        params![source, target_id, created_at],
    )?;
    let target = tx.last_insert_rowid();
    for sql in [
        "INSERT INTO session_values (sid, name, value)
         SELECT ?2, name, value FROM session_values WHERE sid = ?1",
        "INSERT INTO history (sid, idx, users, user_turn, visible, note_kind, context, mode,
             base_mode, text_bytes, body)
         SELECT ?2, idx, users, user_turn, visible, note_kind, context, mode, base_mode,
             text_bytes, body FROM history WHERE sid = ?1",
        "INSERT INTO side_rows (sid, tbl, key, value)
         SELECT ?2, tbl, key, value FROM side_rows WHERE sid = ?1",
        "INSERT INTO transcript (sid, idx, block_idx, history_idx, kind, role, rows_20, rows_40,
             rows_80, rows_120, rows_160, rows_240, text_bytes, tool_render_revision,
             tool_call_id, tool_name, content_hash, first_line, indexed_text, payload)
         SELECT ?2, idx, block_idx, history_idx, kind, role, rows_20, rows_40, rows_80,
             rows_120, rows_160, rows_240, text_bytes, tool_render_revision, tool_call_id,
             tool_name, content_hash, first_line, indexed_text, payload
         FROM transcript WHERE sid = ?1",
    ] {
        tx.execute(sql, params![source, target])?;
    }
    let head = read_head(&tx, target)?;
    tx.commit()?;
    Ok(head)
}

pub(crate) fn delete(conn: &Connection, session_id: &str) -> Result<()> {
    let deleted = conn
        .prepare_cached("DELETE FROM sessions WHERE id = ?1")?
        .execute([session_id])?;
    if deleted == 0 {
        return Err(StoreError::NotFound {
            session_id: session_id.to_owned(),
        });
    }
    // Give the deleted pages back in bounded steps instead of growing the file forever.
    db::incremental_vacuum(conn, Some(4096))?;
    Ok(())
}
