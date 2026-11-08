use protocol::request_log::RequestLogEntry;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::db::{self, signed, unsigned};
use crate::document::{self, Owner, RowKey};
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
        let mut batch = self.batch(identity)?;
        batch.apply(commit)?;
        batch.commit()
    }

    /// Starts one atomic save made of several commits, so a large session can be
    /// written without holding all of it in memory. Dropping the batch discards it.
    pub fn batch<'w>(&'w mut self, identity: &'w SessionIdentity) -> Result<SessionBatch<'w>> {
        if identity.id != self.session_id {
            return Err(StoreError::Integrity(format!(
                "commit for session {} reached the writer of {}",
                identity.id, self.session_id
            )));
        }
        Ok(SessionBatch {
            perf: smelt_perf::perf::begin("store:commit"),
            tx: self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?,
            identity,
            sid: self.sid,
            head: self.head,
            released: Vec::new(),
            writer_sid: &mut self.sid,
            writer_head: &mut self.head,
        })
    }

    pub fn append_request(
        &mut self,
        entry: &RequestLogEntry,
        mode: crate::RequestAuditPayloadMode,
    ) -> Result<i64> {
        let sid = self.sid.ok_or_else(|| StoreError::NotFound {
            session_id: self.session_id.clone(),
        })?;
        let tx = self.conn.transaction()?;
        let id = crate::requests::append(&tx, sid, entry, mode)?;
        tx.commit()?;
        Ok(id)
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
    pub fn delete(mut self, store: &Store) -> Result<()> {
        delete(&mut self.conn, &self.session_id)?;
        let _ = std::fs::remove_dir_all(store.session_artifact_dir(&self.session_id));
        Ok(())
    }
}

/// An atomic save in progress; see [`SessionWriter::batch`].
pub struct SessionBatch<'w> {
    perf: Option<smelt_perf::perf::Guard>,
    tx: Transaction<'w>,
    identity: &'w SessionIdentity,
    sid: Option<i64>,
    head: StoreHead,
    /// Blobs that rows of this save stopped holding.
    released: Vec<i64>,
    writer_sid: &'w mut Option<i64>,
    writer_head: &'w mut StoreHead,
}

impl SessionBatch<'_> {
    /// Applies the next commit. The first one creates the session.
    pub fn apply(&mut self, commit: &SessionCommit) -> Result<StoreHead> {
        let sid = match self.sid {
            Some(sid) => sid,
            None => *self.sid.insert(create_session(&self.tx, self.identity)?),
        };
        self.head = apply(&self.tx, sid, commit, &mut self.released)?;
        Ok(self.head)
    }

    /// Makes every applied commit durable at once.
    pub fn commit(self) -> Result<StoreHead> {
        document::collect(&self.tx, self.released)?;
        self.tx.commit()?;
        *self.writer_sid = self.sid;
        *self.writer_head = self.head;
        drop(self.perf);
        Ok(self.head)
    }
}

/// Writes one save into an open transaction, adding the blobs that rows stopped
/// holding to `released`.
fn apply(
    tx: &Transaction,
    sid: i64,
    commit: &SessionCommit,
    released: &mut Vec<i64>,
) -> Result<StoreHead> {
    let mut head = read_head(tx, sid)?;
    let mut text_bytes: i64 = tx
        .prepare_cached("SELECT text_bytes FROM sessions WHERE sid = ?1")?
        .query_row([sid], |row| row.get(0))?;
    if let Some(history) = &commit.history {
        smelt_perf::perf::record_value("store:commit:history_rows", history.items.len() as u64);
        let (len, bytes) = write_history(tx, sid, head.history_len.get(), history, released)?;
        head.history_len = HistoryLen::new(len);
        text_bytes = text_bytes.saturating_add(bytes);
    }
    for table in SideTable::ALL {
        if let Some(rows) = commit.side_rows(table) {
            write_side_rows(tx, sid, table, rows)?;
        }
    }
    if let Some(transcript) = &commit.transcript {
        smelt_perf::perf::record_value(
            "store:commit:transcript_rows",
            transcript.records.len() as u64,
        );
        let count = write_transcript(
            tx,
            sid,
            head.transcript_record_count.get(),
            transcript,
            released,
        )?;
        head.transcript_record_count = TranscriptRecordCount::new(count);
    }
    if let Some(message) = &commit.first_user_message {
        write_value(tx, sid, "first_user_message", message.as_deref())?;
    }
    if let Some(checkpoint) = &commit.checkpoint {
        let json = checkpoint.as_ref().map(serde_json::to_string).transpose()?;
        write_value(tx, sid, "checkpoint", json.as_deref())?;
    }
    write_metadata(tx, sid, &commit.metadata, head, text_bytes)?;
    Ok(head)
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
    released: &mut Vec<i64>,
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
    document::release_from(tx, sid, Owner::History, start, released)?;
    tx.prepare_cached("DELETE FROM search_text WHERE sid = ?1 AND idx >= ?2")?
        .execute(params![sid, start])?;
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
    let mut insert_text =
        tx.prepare_cached("INSERT INTO search_text (sid, idx, text) VALUES (?1, ?2, ?3)")?;
    for (offset, item) in suffix.items.iter().enumerate() {
        let idx = start + offset as i64;
        let json = serde_json::to_vec(item)?;
        let row = HistoryRow::of(item);
        users += i64::from(row.user_turn);
        let bytes = json.len() as i64;
        added += bytes;
        let body = document::store(
            tx,
            RowKey {
                sid,
                owner: Owner::History,
                key: idx,
            },
            &json,
        )?;
        insert.execute(params![
            sid,
            idx,
            users,
            row.user_turn,
            row.visible,
            row.note_kind,
            row.context,
            row.mode,
            row.base_mode,
            bytes,
            body,
        ])?;
        if let Some(text) = crate::history::conversation_text(item).filter(|text| !text.is_empty())
        {
            insert_text.execute(params![sid, idx, text.as_ref()])?;
        }
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
    released: &mut Vec<i64>,
) -> Result<u64> {
    if suffix.start > len {
        return Err(StoreError::InvalidSuffix {
            sequence: "transcript",
            start: suffix.start,
            len,
        });
    }
    let start = signed(suffix.start);
    document::release_from(tx, sid, Owner::Transcript, start, released)?;
    let deleted = tx
        .prepare_cached("DELETE FROM transcript WHERE sid = ?1 AND idx >= ?2")?
        .execute(params![sid, start])?;
    smelt_perf::perf::record_value("store:commit:transcript_rows_deleted", deleted as u64);
    let mut cumulative = cumulative_profile(tx, sid, suffix.start)?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO transcript (sid, idx, block_idx, history_idx, kind, role, rows_20, rows_40,
             rows_80, rows_120, rows_160, rows_240, text_bytes, tool_render_revision,
             tool_call_id, tool_name, content_hash, first_line, summary, indexed_text, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
             ?18, ?19, ?20, ?21)",
    )?;
    for (offset, record) in suffix.records.iter().enumerate() {
        let idx = start + offset as i64;
        let row = RowKey {
            sid,
            owner: Owner::Transcript,
            key: idx,
        };
        cumulative = cumulative.add(record.extent_profile());
        let rows = cumulative.rows().map(signed);
        insert.execute(params![
            sid,
            idx,
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
            document::compress(&record.summary),
            document::store_text(tx, row, &record.indexed_text)?,
            transcript::encode_payload(
                tx,
                row,
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
    let _perf = smelt_perf::perf::begin("store:fork");
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
        "INSERT INTO search_text (sid, idx, text)
         SELECT ?2, idx, text FROM search_text WHERE sid = ?1",
        "INSERT INTO blob_refs (sid, tbl, key, blob)
         SELECT ?2, tbl, key, blob FROM blob_refs WHERE sid = ?1 AND tbl IN (0, 1)",
        "INSERT INTO transcript (sid, idx, block_idx, history_idx, kind, role, rows_20, rows_40,
             rows_80, rows_120, rows_160, rows_240, text_bytes, tool_render_revision,
             tool_call_id, tool_name, content_hash, first_line, summary, indexed_text, payload)
         SELECT ?2, idx, block_idx, history_idx, kind, role, rows_20, rows_40, rows_80,
             rows_120, rows_160, rows_240, text_bytes, tool_render_revision, tool_call_id,
             tool_name, content_hash, first_line, summary, indexed_text, payload
         FROM transcript WHERE sid = ?1",
    ] {
        tx.execute(sql, params![source, target])?;
    }
    let head = read_head(&tx, target)?;
    tx.commit()?;
    Ok(head)
}

pub(crate) fn delete(conn: &mut Connection, session_id: &str) -> Result<()> {
    let _perf = smelt_perf::perf::begin("store:delete");
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let sid = db::require_session_key(&tx, session_id)?;
    let mut released = Vec::new();
    document::release_session(&tx, sid, &mut released)?;
    tx.prepare_cached("DELETE FROM sessions WHERE sid = ?1")?
        .execute([sid])?;
    document::collect(&tx, released)?;
    tx.commit()?;
    // Give the deleted pages back in bounded steps instead of growing the file forever.
    db::incremental_vacuum(conn, Some(4096))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use protocol::{AssistantStep, Content, HistoryItem};

    use super::*;
    use crate::document::SPLIT_MIN_BYTES;
    use crate::StoredTranscriptBlock;

    fn identity(id: &str) -> SessionIdentity {
        SessionIdentity {
            id: id.to_owned(),
            created_at: 0,
            parent_id: None,
        }
    }

    fn reply(text: &str, reasoning: &str) -> HistoryItem {
        HistoryItem::assistant(AssistantStep::terminal(
            Some(Content::text(text)),
            Some(reasoning.to_owned()),
            Vec::new(),
        ))
    }

    fn record(block_idx: u64, indexed_text: &str) -> StoredTranscriptBlock {
        StoredTranscriptBlock {
            block_idx,
            history_idx: None,
            kind: "tool".to_owned(),
            tool_call_id: None,
            tool_name: None,
            content_hash: String::new(),
            estimated_text_bytes: indexed_text.len() as u64,
            summary: b"summary".to_vec(),
            indexed_text: indexed_text.to_owned(),
            block_json: serde_json::json!({ "output": indexed_text }).to_string(),
            origin_json: None,
            tool_state_json: None,
            tool_render_revision: 0,
        }
    }

    fn history(start: u64, items: Vec<HistoryItem>) -> SessionCommit {
        SessionCommit {
            history: Some(HistorySuffix { start, items }),
            ..SessionCommit::default()
        }
    }

    fn blobs(store: &Store) -> i64 {
        store
            .connect()
            .unwrap()
            .query_row("SELECT count(*) FROM blobs", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn blobs_live_while_any_row_of_any_session_holds_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let thinking = "deliberation ".repeat(SPLIT_MIN_BYTES);
        let items = vec![
            HistoryItem::user(Content::text("question")),
            reply("answer", &thinking),
            reply("again", &thinking),
        ];
        let mut writer = store.writer("a").unwrap();
        writer
            .commit(&identity("a"), &history(0, items.clone()))
            .unwrap();
        assert_eq!(blobs(&store), 1, "equal reasoning is stored once");

        writer.fork_to("b", 1, &store).unwrap();
        assert_eq!(blobs(&store), 1, "a fork shares its source's blobs");

        // Rewinding the source drops its references but not the fork's.
        writer
            .commit(&identity("a"), &history(1, Vec::new()))
            .unwrap();
        assert_eq!(blobs(&store), 1);
        writer.delete(&store).unwrap();
        assert_eq!(
            store.reader("b").unwrap().history_range(0, 3).unwrap(),
            items
        );

        store.delete("b").unwrap();
        assert_eq!(blobs(&store), 0);
    }

    #[test]
    fn session_search_follows_saves_forks_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let mut writer = store.writer("a").unwrap();
        let items = vec![
            HistoryItem::user(Content::text("Where is the \"Frobnicator\" config?")),
            HistoryItem::user(Content::text(format!(
                "{}switched to plan mode",
                protocol::note::MODE_NOTE_PREFIX
            ))),
            reply("In settings.lua", ""),
            HistoryItem::user(protocol::compaction_summary_content("We found the config.")),
        ];
        writer.commit(&identity("a"), &history(0, items)).unwrap();
        let search = |query: &str| {
            let mut ids = store.search_sessions(query).unwrap();
            ids.sort();
            ids
        };
        assert_eq!(search("frobnicator"), ["a"]);
        assert_eq!(search("the \"frob"), ["a"]);
        assert_eq!(search("SETTINGS.LUA"), ["a"]);
        assert!(search("fr").is_empty(), "shorter than the trigram minimum");
        assert!(search("deliberation").is_empty());
        assert!(
            search("plan mode").is_empty(),
            "notices are not conversation"
        );
        assert_eq!(search("found the config"), ["a"]);

        writer.fork_to("b", 1, &store).unwrap();
        assert_eq!(search("settings"), ["a", "b"]);

        writer
            .commit(&identity("a"), &history(2, vec![reply("elsewhere", "")]))
            .unwrap();
        assert_eq!(search("settings"), ["b"]);
        assert_eq!(search("elsewhere"), ["a"]);
        assert_eq!(
            store.reader("a").unwrap().conversation_text().unwrap(),
            "Where is the \"Frobnicator\" config?\nelsewhere\n"
        );

        store.delete("b").unwrap();
        assert!(search("settings").is_empty());
    }

    #[test]
    fn transcript_search_reads_split_and_escaped_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let long = format!("{}needle in \"quotes\"\n", "hay\t".repeat(SPLIT_MIN_BYTES));
        let commit = SessionCommit {
            transcript: Some(TranscriptRecordSuffix {
                start: 0,
                records: vec![
                    record(0, "short needle"),
                    record(1, &long),
                    record(2, "\n"),
                    record(3, "a\\b"),
                ],
            }),
            ..SessionCommit::default()
        };
        store
            .writer("a")
            .unwrap()
            .commit(&identity("a"), &commit)
            .unwrap();
        let reader = store.reader("a").unwrap();
        let hits = |query: &str| {
            reader
                .search_transcript_candidate_page(
                    query,
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    16,
                )
                .unwrap()
                .into_iter()
                .map(|hit| hit.block_idx)
                .collect::<Vec<_>>()
        };
        assert_eq!(hits("needle"), [0, 1]);
        assert_eq!(hits("in \"quotes\"\n"), [1]);
        // The newline is stored as the escape `\n`, which holds no `n`.
        assert_eq!(hits("n"), [0, 1]);
        assert_eq!(hits("\n"), [1, 2]);
        assert_eq!(hits("\\"), [3]);
        assert_eq!(reader.transcript_range(1, 2).unwrap()[0], record(1, &long));
    }

    #[test]
    fn a_batch_saves_all_its_commits_or_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let mut writer = store.writer("a").unwrap();
        let id = identity("a");
        let save_both = |batch: &mut SessionBatch<'_>| {
            for (at, text) in ["first", "second"].into_iter().enumerate() {
                let item = HistoryItem::user(Content::text(text));
                batch.apply(&history(at as u64, vec![item])).unwrap();
            }
        };
        let mut batch = writer.batch(&id).unwrap();
        save_both(&mut batch);
        drop(batch);
        assert!(!writer.exists());
        assert!(!store.exists("a").unwrap());

        let mut batch = writer.batch(&id).unwrap();
        save_both(&mut batch);
        let head = batch.commit().unwrap();
        assert_eq!(writer.head(), head);
        assert_eq!(head.history_len.get(), 2);
        assert_eq!(store.search_sessions("second").unwrap(), ["a"]);
    }
}
