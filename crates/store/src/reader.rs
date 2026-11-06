use std::ops::Range;

use protocol::HistoryItem;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::db::{self, signed, unsigned};
use crate::error::{Result, StoreError};
use crate::transcript::*;
use crate::types::*;
use crate::writer::{cumulative_profile, profile_from_row, read_head};
use crate::Store;

/// Read access to one saved session. Each query sees the latest commit unless it
/// runs inside [`SessionReader::read_snapshot`].
pub struct SessionReader {
    conn: Connection,
    session_id: String,
    sid: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct StorageStats {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub history_rows: u64,
    pub history_text_bytes: u64,
    pub transcript_rows: u64,
    pub request_rows: u64,
}

const TRANSCRIPT_COLUMNS: &str = "block_idx, history_idx, kind, tool_call_id, tool_name,
    content_hash, text_bytes, preview, indexed_text, block, origin, tool_state,
    tool_render_revision";

// Per-id lookups run once per block a frame needs, so each must seek
// `transcript_blocks` or `transcript_history` rather than scan the session.
const RECORD_FOR_BLOCK: &str = "SELECT min(idx) FROM transcript WHERE sid = ?1 AND block_idx = ?2";
const RECORD_FOR_HISTORY: &str =
    "SELECT min(idx) FROM transcript WHERE sid = ?1 AND history_idx = ?2";

fn block_by_id_sql() -> String {
    format!(
        "SELECT idx, {TRANSCRIPT_COLUMNS} FROM transcript
         WHERE sid = ?1 AND block_idx = ?2 ORDER BY idx LIMIT 1"
    )
}

/// Decodes [`TRANSCRIPT_COLUMNS`] starting at column `first`.
fn transcript_from_row_at(row: &Row<'_>, first: usize) -> rusqlite::Result<StoredTranscriptBlock> {
    let column = |i: usize| first + i;
    let block: Vec<u8> = row.get(column(9))?;
    let block = db::decode_blob(&block).map_err(crate::error::to_sql_error)?;
    Ok(StoredTranscriptBlock {
        block_idx: unsigned(row.get(column(0))?),
        history_idx: row.get::<_, Option<i64>>(column(1))?.map(unsigned),
        kind: row.get(column(2))?,
        tool_call_id: row.get(column(3))?,
        tool_name: row.get(column(4))?,
        content_hash: row.get(column(5))?,
        estimated_text_bytes: unsigned(row.get(column(6))?),
        preview_text: row.get(column(7))?,
        indexed_text: row.get(column(8))?,
        block_json: String::from_utf8(block.into_owned()).map_err(crate::error::to_sql_error)?,
        origin_json: row.get(column(10))?,
        tool_state_json: row.get(column(11))?,
        tool_render_revision: unsigned(row.get(column(12))?),
    })
}

fn transcript_from_row(row: &Row<'_>) -> rusqlite::Result<StoredTranscriptBlock> {
    transcript_from_row_at(row, 0)
}

const PROFILE_COLUMNS: &str = "idx, block_idx, history_idx, kind, role, first_line, text_bytes,
    rows_20, rows_40, rows_80, rows_120, rows_160, rows_240";

impl SessionReader {
    pub(crate) fn open(store: &Store, session_id: &str) -> Result<Self> {
        let conn = store.connect()?;
        let sid = db::require_session_key(&conn, session_id)?;
        Ok(Self {
            conn,
            session_id: session_id.to_owned(),
            sid,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Runs related reads on one consistent snapshot.
    pub fn read_snapshot<T>(&self, read: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        if !self.conn.is_autocommit() {
            return read(self);
        }
        let tx = self.conn.unchecked_transaction()?;
        let result = read(self)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn head(&self) -> Result<StoreHead> {
        read_head(&self.conn, self.sid)
    }

    pub fn state(&self) -> Result<SessionState> {
        self.read_snapshot(|reader| reader.state_unchecked())
    }

    fn state_unchecked(&self) -> Result<SessionState> {
        let (identity, metadata, head, history_text_bytes) = self
            .conn
            .prepare_cached(
                "SELECT id, parent_id, created_at, updated_at, title, slug, cwd, mode,
                     reasoning_effort, model, fast_mode, accounting, context_tokens,
                     context_tokens_history_len, display_context_tokens, cost_usd,
                     history_len, record_count, text_bytes, last_turn_id
                 FROM sessions WHERE sid = ?1",
            )?
            .query_row([self.sid], |row| {
                let accounting: Option<String> = row.get(11)?;
                Ok((
                    SessionIdentity {
                        id: row.get(0)?,
                        parent_id: row.get(1)?,
                        created_at: row.get(2)?,
                    },
                    SessionMetadata {
                        updated_at: row.get(3)?,
                        title: row.get(4)?,
                        slug: row.get(5)?,
                        cwd: row.get(6)?,
                        mode: row.get(7)?,
                        reasoning_effort: row.get(8)?,
                        model: row.get(9)?,
                        fast_mode: row.get(10)?,
                        accounting: accounting
                            .as_deref()
                            .map(serde_json::from_str)
                            .transpose()
                            .map_err(crate::error::to_sql_error)?,
                        context_tokens: row.get::<_, Option<i64>>(12)?.map(unsigned),
                        context_tokens_history_len: row.get::<_, Option<i64>>(13)?.map(unsigned),
                        display_context_tokens: row.get::<_, Option<i64>>(14)?.map(unsigned),
                        cost_usd: row.get(15)?,
                        last_turn_id: unsigned(row.get(19)?),
                    },
                    StoreHead {
                        history_len: HistoryLen::new(unsigned(row.get(16)?)),
                        transcript_record_count: TranscriptRecordCount::new(unsigned(row.get(17)?)),
                    },
                    unsigned(row.get(18)?),
                ))
            })?;
        let checkpoint = self
            .value("checkpoint")?
            .map(|json| serde_json::from_str(&json))
            .transpose()?;
        Ok(SessionState {
            identity,
            metadata,
            first_user_message: self.value("first_user_message")?,
            checkpoint,
            head,
            history_text_bytes,
            turn_metas: self.side_rows(SideTable::TurnMetas)?,
            metadata_snapshots: self.side_rows(SideTable::MetadataSnapshots)?,
            context_snapshots: self.side_rows(SideTable::ContextSnapshots)?,
            checkpoint_events: self.side_rows(SideTable::CheckpointEvents)?,
        })
    }

    fn value(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT value FROM session_values WHERE sid = ?1 AND name = ?2")?
            .query_row(params![self.sid, name], |row| row.get(0))
            .optional()?)
    }

    fn side_rows(&self, table: SideTable) -> Result<Vec<(u64, serde_json::Value)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT key, value FROM side_rows WHERE sid = ?1 AND tbl = ?2 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![self.sid, table as i64], |row| {
            Ok((unsigned(row.get(0)?), row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (key, json) = row?;
            Ok((key, serde_json::from_str(&json)?))
        })
        .collect()
    }

    pub fn catalog_session(&self) -> Result<crate::CatalogSession> {
        crate::catalog::session(&self.conn, &self.session_id)?.ok_or_else(|| StoreError::NotFound {
            session_id: self.session_id.clone(),
        })
    }

    // History

    pub fn history_range(&self, start: u64, end: u64) -> Result<Vec<HistoryItem>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT body FROM history WHERE sid = ?1 AND idx >= ?2 AND idx < ?3 ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![self.sid, signed(start), signed(end)], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        let items: Vec<HistoryItem> = rows
            .map(|body| db::decode_json(&body?))
            .collect::<Result<_>>()?;
        smelt_perf::perf::record_value("store:history:rows_read", items.len() as u64);
        Ok(items)
    }

    /// The newest items before `end` that fit `max_items` and `max_bytes`.
    pub fn history_tail(
        &self,
        end: usize,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<Vec<HistoryItem>> {
        if end == 0 || max_items == 0 || max_bytes == Some(0) {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT body FROM history WHERE sid = ?1 AND idx < ?2 ORDER BY idx DESC LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![
            self.sid,
            signed(end as u64),
            signed(max_items as u64)
        ])?;
        let mut budget = protocol::HistoryTailBudget::new(max_items, max_bytes);
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            let item: HistoryItem = db::decode_json(&row.get::<_, Vec<u8>>(0)?)?;
            if !budget.try_prepend(&item)? {
                break;
            }
            items.push(item);
        }
        items.reverse();
        Ok(items)
    }

    fn last_index(&self, sql: &str, end: u64) -> Result<Option<u64>> {
        Ok(self
            .conn
            .prepare_cached(sql)?
            .query_row(params![self.sid, signed(end)], |row| {
                row.get::<_, Option<i64>>(0)
            })?
            .map(unsigned))
    }

    /// Latest context update or tombstone named `name` strictly before `end`.
    pub fn history_last_context_note_index_before(
        &self,
        end: u64,
        name: &str,
    ) -> Result<Option<u64>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT max(idx) FROM history WHERE sid = ?1 AND context = ?2 AND idx < ?3",
            )?
            .query_row(params![self.sid, name, signed(end)], |row| {
                row.get::<_, Option<i64>>(0)
            })?
            .map(unsigned))
    }

    pub fn history_note_projection_at(
        &self,
        index: u64,
    ) -> Result<Option<protocol::HistoryNoteProjection>> {
        Ok(self
            .conn
            .prepare_cached("SELECT note_kind, mode FROM history WHERE sid = ?1 AND idx = ?2")?
            .query_row(params![self.sid, signed(index)], |row| {
                Ok(crate::history::note_projection(row.get(0)?, row.get(1)?))
            })
            .optional()?
            .flatten())
    }

    pub fn history_user_turn_count_before(&self, end: u64) -> Result<u64> {
        if end == 0 {
            return Ok(0);
        }
        Ok(self
            .conn
            .prepare_cached(
                "SELECT users FROM history WHERE sid = ?1 AND idx < ?2 ORDER BY idx DESC LIMIT 1",
            )?
            .query_row(params![self.sid, signed(end)], |row| row.get::<_, i64>(0))
            .optional()?
            .map_or(0, unsigned))
    }

    pub fn history_last_user_index_before(&self, end: u64) -> Result<Option<u64>> {
        self.last_index(
            "SELECT max(idx) FROM history WHERE sid = ?1 AND user_turn AND idx < ?2",
            end,
        )
    }

    /// The mode set by the latest mode note strictly before `end`.
    pub fn history_mode_before(&self, end: u64) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT mode FROM history WHERE sid = ?1 AND mode IS NOT NULL AND idx < ?2
                 ORDER BY idx DESC LIMIT 1",
            )?
            .query_row(params![self.sid, signed(end)], |row| row.get(0))
            .optional()?)
    }

    /// The base mode of the first mode note in `range`.
    pub fn history_base_mode_range(&self, range: Range<u64>) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT base_mode FROM history WHERE sid = ?1 AND base_mode IS NOT NULL
                     AND idx >= ?2 AND idx < ?3
                 ORDER BY idx LIMIT 1",
            )?
            .query_row(
                params![self.sid, signed(range.start), signed(range.end)],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn history_any_transcript_visible_before(&self, end: u64) -> Result<bool> {
        Ok(self
            .last_index(
                "SELECT max(idx) FROM history WHERE sid = ?1 AND visible AND idx < ?2",
                end,
            )?
            .is_some())
    }

    // Transcript

    pub fn transcript_range(&self, start: u64, end: u64) -> Result<Vec<StoredTranscriptBlock>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {TRANSCRIPT_COLUMNS} FROM transcript
             WHERE sid = ?1 AND idx >= ?2 AND idx < ?3 ORDER BY idx"
        ))?;
        let rows = stmt.query_map(
            params![self.sid, signed(start), signed(end)],
            transcript_from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn transcript_record_slice_with_total(
        &self,
        range: TranscriptRecordRange,
        total_count: usize,
    ) -> Result<TranscriptRecordSlice> {
        let _perf = smelt_perf::perf::begin("store:transcript:read_record_slice");
        let start = range.start().get().min(total_count);
        let end = range.end().get().min(total_count).max(start);
        Ok(TranscriptRecordSlice::new(
            TranscriptRecordOffset::new(start),
            total_count,
            self.transcript_range(start as u64, end as u64)?,
        ))
    }

    pub fn transcript_extent_profile(
        &self,
        range: TranscriptRecordRange,
    ) -> Result<TranscriptExtentProfile> {
        let start = range.start().get() as u64;
        let end = (range.end().get() as u64).max(start);
        Ok(cumulative_profile(&self.conn, self.sid, end)?
            .sub(cumulative_profile(&self.conn, self.sid, start)?))
    }

    pub fn transcript_estimated_rows(
        &self,
        range: TranscriptRecordRange,
        width: u16,
    ) -> Result<u64> {
        Ok(self.transcript_extent_profile(range)?.estimated_rows(width))
    }

    pub fn transcript_total_estimated_rows(&self, width: u16) -> Result<u64> {
        let count = self.head()?.transcript_record_count.get();
        Ok(cumulative_profile(&self.conn, self.sid, count)?.estimated_rows(width))
    }

    /// The fewest newest records whose estimated rows reach `target_rows`.
    pub fn transcript_tail_for_rows_with_total(
        &self,
        total_count: usize,
        width: u16,
        target_rows: u16,
    ) -> Result<TranscriptRecordSlice> {
        if total_count == 0 {
            return Ok(TranscriptRecordSlice::new(
                TranscriptRecordOffset::new(0),
                0,
                Vec::new(),
            ));
        }
        let column = [
            "rows_20", "rows_40", "rows_80", "rows_120", "rows_160", "rows_240",
        ][profile_bucket(width)];
        let total =
            cumulative_profile(&self.conn, self.sid, total_count as u64)?.estimated_rows(width);
        let limit = total.saturating_sub(u64::from(target_rows.max(1)));
        // The last record whose cumulative rows still fit below the limit ends
        // the part that is left out.
        let start = self
            .conn
            .prepare_cached(&format!(
                "SELECT idx FROM transcript WHERE sid = ?1 AND idx < ?2 AND {column} <= ?3
                 ORDER BY idx DESC LIMIT 1"
            ))?
            .query_row(
                params![self.sid, signed(total_count as u64), signed(limit)],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map_or(0, |idx| unsigned(idx) as usize + 1);
        let start = if total <= u64::from(target_rows.max(1)) {
            0
        } else {
            start.min(total_count - 1)
        };
        self.transcript_record_slice_with_total((start..total_count).into(), total_count)
    }

    fn navigation_record(
        &self,
        sql: &str,
        key: &str,
        index: usize,
    ) -> Result<Option<TranscriptNavigationRecord>> {
        self.conn
            .prepare_cached(sql)?
            .query_row(
                params![self.sid, key, signed(index as u64)],
                navigation_from_row(self),
            )
            .optional()?
            .transpose()
    }

    pub fn transcript_record_before_kind(
        &self,
        kind: &str,
        before_or_at: usize,
    ) -> Result<Option<TranscriptNavigationRecord>> {
        self.navigation_record(
            &format!(
                "SELECT {PROFILE_COLUMNS} FROM transcript WHERE sid = ?1 AND kind = ?2 AND idx <= ?3
                 ORDER BY idx DESC LIMIT 1"
            ),
            kind,
            before_or_at,
        )
    }

    pub fn transcript_record_after_kind(
        &self,
        kind: &str,
        after_or_at: usize,
    ) -> Result<Option<TranscriptNavigationRecord>> {
        self.navigation_record(
            &format!(
                "SELECT {PROFILE_COLUMNS} FROM transcript WHERE sid = ?1 AND kind = ?2 AND idx >= ?3
                 ORDER BY idx LIMIT 1"
            ),
            kind,
            after_or_at,
        )
    }

    pub fn transcript_record_before_role(
        &self,
        role: &str,
        before_or_at: usize,
    ) -> Result<Option<TranscriptNavigationRecord>> {
        self.navigation_record(
            &format!(
                "SELECT {PROFILE_COLUMNS} FROM transcript WHERE sid = ?1 AND role = ?2 AND idx <= ?3
                 ORDER BY idx DESC LIMIT 1"
            ),
            role,
            before_or_at,
        )
    }

    pub fn transcript_record_after_role(
        &self,
        role: &str,
        after_or_at: usize,
    ) -> Result<Option<TranscriptNavigationRecord>> {
        self.navigation_record(
            &format!(
                "SELECT {PROFILE_COLUMNS} FROM transcript WHERE sid = ?1 AND role = ?2 AND idx >= ?3
                 ORDER BY idx LIMIT 1"
            ),
            role,
            after_or_at,
        )
    }

    /// The newest records of `kind` strictly before a history boundary, newest first.
    pub fn transcript_records_before_kind(
        &self,
        kind: &str,
        before_history_idx: u64,
        limit: usize,
    ) -> Result<Vec<TranscriptNavigationRecord>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {PROFILE_COLUMNS} FROM transcript
             WHERE sid = ?1 AND kind = ?2 AND history_idx < ?3
             ORDER BY idx DESC LIMIT ?4"
        ))?;
        let rows = stmt.query_map(
            params![
                self.sid,
                kind,
                signed(before_history_idx),
                signed(limit as u64)
            ],
            navigation_from_row(self),
        )?;
        rows.map(|row| row?).collect()
    }

    pub fn transcript_record_index_for_block_idx(&self, block_idx: u64) -> Result<Option<usize>> {
        Ok(self
            .conn
            .prepare_cached(RECORD_FOR_BLOCK)?
            .query_row(params![self.sid, signed(block_idx)], |row| {
                row.get::<_, Option<i64>>(0)
            })?
            .map(|idx| unsigned(idx) as usize))
    }

    /// The first record of a history item.
    pub fn transcript_record_index_for_history_idx(
        &self,
        history_idx: u64,
    ) -> Result<Option<usize>> {
        Ok(self
            .conn
            .prepare_cached(RECORD_FOR_HISTORY)?
            .query_row(params![self.sid, signed(history_idx)], |row| {
                row.get::<_, Option<i64>>(0)
            })?
            .map(|idx| unsigned(idx) as usize))
    }

    /// Reads records by block id, in request order with their current indices.
    /// Blocks the session no longer holds are left out.
    pub fn transcript_blocks_by_id(
        &self,
        blocks: &[TranscriptBlockLookup],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<(usize, StoredTranscriptBlock)>> {
        let mut stmt = self.conn.prepare_cached(&block_by_id_sql())?;
        let mut found = Vec::with_capacity(blocks.len());
        for block in blocks {
            if cancelled() {
                return Err(StoreError::Cancelled);
            }
            let record = stmt
                .query_row(params![self.sid, signed(block.block_idx)], |row| {
                    let idx = unsigned(row.get(0)?) as usize;
                    Ok((idx, transcript_from_row_at(row, 1)?))
                })
                .optional()?;
            found.extend(record);
        }
        Ok(found)
    }

    /// Records whose indexed text contains `query`, starting at the origin block
    /// and moving in `direction`. Returned in block order.
    pub fn search_transcript_candidate_page(
        &self,
        query: &str,
        origin_block_idx: Option<u64>,
        direction: TranscriptSearchDirection,
        limit: usize,
    ) -> Result<Vec<TranscriptSearchCandidate>> {
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let _perf = smelt_perf::perf::begin("store:search_candidates");
        let sql = match direction {
            TranscriptSearchDirection::Forward => {
                "SELECT block_idx, history_idx FROM transcript
                 WHERE sid = ?1 AND block_idx >= ?2 AND instr(indexed_text, ?3) > 0
                 ORDER BY block_idx LIMIT ?4"
            }
            TranscriptSearchDirection::Backward => {
                "SELECT block_idx, history_idx FROM transcript
                 WHERE sid = ?1 AND block_idx <= ?2 AND instr(indexed_text, ?3) > 0
                 ORDER BY block_idx DESC LIMIT ?4"
            }
        };
        let origin = match (origin_block_idx, direction) {
            (Some(origin), _) => signed(origin),
            (None, TranscriptSearchDirection::Forward) => 0,
            (None, TranscriptSearchDirection::Backward) => i64::MAX,
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(
            params![self.sid, origin, query, signed(limit as u64)],
            |row| {
                Ok(TranscriptSearchCandidate {
                    block_idx: unsigned(row.get(0)?),
                    history_idx: row.get::<_, Option<i64>>(1)?.map(unsigned),
                })
            },
        )?;
        let mut candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        candidates.sort_unstable_by_key(|candidate| candidate.block_idx);
        Ok(candidates)
    }

    // Requests

    pub fn requests(
        &self,
        query: &crate::RequestAuditQuery,
    ) -> Result<Vec<crate::RequestAuditSummary>> {
        crate::requests::query(&self.conn, self.sid, query)
    }

    pub fn request_stats(&self) -> Result<crate::RequestAuditStats> {
        crate::requests::stats(&self.conn, self.sid)
    }

    pub fn request_payloads(&self, id: i64) -> Result<Option<crate::RequestAuditPayloads>> {
        crate::requests::payloads(&self.conn, self.sid, id)
    }

    pub fn export_requests_jsonl(&self, out: impl std::io::Write) -> Result<()> {
        crate::export::requests_jsonl(&self.conn, self.sid, out)
    }

    pub fn export_history_jsonl(
        &self,
        out: impl std::io::Write,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        crate::export::history_jsonl(&self.conn, self.sid, out, cancelled)
    }

    /// User and assistant text of the whole session, oldest first.
    pub fn conversation_text(&self) -> Result<String> {
        crate::export::conversation_text(&self.conn, self.sid)
    }

    pub fn storage_stats(&self, store: &Store) -> Result<StorageStats> {
        let (history_rows, history_text_bytes): (i64, i64) = self.conn.query_row(
            "SELECT count(*), coalesce(sum(text_bytes), 0) FROM history WHERE sid = ?1",
            [self.sid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let count = |sql: &str| -> Result<u64> {
            Ok(unsigned(
                self.conn.query_row(sql, [self.sid], |row| row.get(0))?,
            ))
        };
        let size = |path: std::path::PathBuf| std::fs::metadata(path).map_or(0, |m| m.len());
        let database = store.database_path();
        Ok(StorageStats {
            database_bytes: size(database.clone()),
            wal_bytes: size(database.with_extension("db-wal")),
            history_rows: unsigned(history_rows),
            history_text_bytes: unsigned(history_text_bytes),
            transcript_rows: count("SELECT count(*) FROM transcript WHERE sid = ?1")?,
            request_rows: count("SELECT count(*) FROM requests WHERE sid = ?1")?,
        })
    }
}

/// Decodes one profile row, including its own extent from the previous cumulative row.
fn navigation_from_row(
    reader: &SessionReader,
) -> impl Fn(&Row<'_>) -> rusqlite::Result<Result<TranscriptNavigationRecord>> + '_ {
    move |row| {
        let idx = unsigned(row.get(0)?);
        let cumulative = profile_from_row(7)(row)?;
        let profile = (|| -> Result<TranscriptNavigationRecord> {
            let before = cumulative_profile(&reader.conn, reader.sid, idx)?;
            Ok(TranscriptNavigationRecord {
                record_index: TranscriptRecordOffset::new(idx as usize),
                profile: TranscriptRecordProfile {
                    block_idx: unsigned(row.get(1)?),
                    history_idx: row.get::<_, Option<i64>>(2)?.map(unsigned),
                    kind: row.get(3)?,
                    role: row.get(4)?,
                    first_line: row.get(5)?,
                    estimated_text_bytes: unsigned(row.get(6)?),
                    extent: cumulative.sub(before),
                },
            })
        })();
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_and_history_lookups_seek_an_index() {
        for sql in [
            RECORD_FOR_BLOCK.to_owned(),
            RECORD_FOR_HISTORY.to_owned(),
            block_by_id_sql(),
        ] {
            crate::db::assert_query_seeks(&sql);
        }
    }
}
