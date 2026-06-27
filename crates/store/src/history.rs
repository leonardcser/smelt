use protocol::HistoryItem;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Statement};
use serde_json::{json, Value};
use smelt_perf::perf;
use std::ops::Range;

use crate::compression::ObjectCompression;
use crate::error::Result;
use crate::object::{self, checked_i64, sha256_hex};
use rusqlite::types::Value as SqlValue;

pub(crate) const METADATA_OBJECT_MIN_BYTES: usize = 4 * 1024;
pub(crate) const OBJECT_REF_KEY: &str = "$smelt_object_ref";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HistoryRowInfo {
    pub idx: u64,
    pub hash: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TranscriptDescriptorIndex(usize);

impl TranscriptDescriptorIndex {
    pub fn new(index: usize) -> Self {
        Self(index)
    }

    pub fn get(self) -> usize {
        self.0
    }
}

impl From<usize> for TranscriptDescriptorIndex {
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TranscriptDescriptorRange {
    start: TranscriptDescriptorIndex,
    end: TranscriptDescriptorIndex,
}

impl TranscriptDescriptorRange {
    pub fn new(start: TranscriptDescriptorIndex, end: TranscriptDescriptorIndex) -> Self {
        Self { start, end }
    }

    pub fn start(self) -> TranscriptDescriptorIndex {
        self.start
    }

    pub fn end(self) -> TranscriptDescriptorIndex {
        self.end
    }
}

impl From<Range<usize>> for TranscriptDescriptorRange {
    fn from(value: Range<usize>) -> Self {
        Self::new(value.start.into(), value.end.into())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptDescriptorRecord {
    pub block_idx: u64,
    pub history_idx: Option<u64>,
    pub kind: String,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub content_hash: String,
    pub estimated_text_bytes: u64,
    pub preview_text: String,
    pub search_text: String,
    pub descriptor_json: String,
    pub origin_json: Option<String>,
    pub tool_state_json: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptBlockMetadataRecord {
    pub block_idx: u64,
    pub descriptor_idx: Option<u64>,
    pub history_idx: Option<u64>,
    pub kind: String,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub content_hash: Option<String>,
    pub estimated_text_bytes: u64,
    pub estimated_rows: Option<u64>,
    pub preview_text: String,
    pub has_descriptor: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptDescriptorHydration {
    Hydrated,
    ObjectBacked,
}

impl TranscriptDescriptorHydration {
    fn hydrates_objects(self) -> bool {
        matches!(self, Self::Hydrated)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptDescriptorSlice {
    pub start: TranscriptDescriptorIndex,
    pub total_count: usize,
    pub hydration: TranscriptDescriptorHydration,
    pub records: Vec<TranscriptDescriptorRecord>,
}

impl TranscriptDescriptorSlice {
    pub fn new(
        start: TranscriptDescriptorIndex,
        total_count: usize,
        hydration: TranscriptDescriptorHydration,
        records: Vec<TranscriptDescriptorRecord>,
    ) -> Self {
        Self {
            start,
            total_count,
            hydration,
            records,
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn end(&self) -> TranscriptDescriptorIndex {
        TranscriptDescriptorIndex::new(self.start.get().saturating_add(self.records.len()))
    }

    pub fn into_records(self) -> Vec<TranscriptDescriptorRecord> {
        self.records
    }
}

const SEARCH_DRIVER_TERM_POSTING_CAP: u64 = 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptSearchCandidate {
    pub block_idx: u64,
    pub history_idx: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptSearchDirection {
    Forward,
    Backward,
}

pub(crate) fn history_hashes_from(
    conn: &Connection,
    start_idx: usize,
) -> Result<Vec<HistoryRowInfo>> {
    let start_idx = checked_i64(start_idx as u64, "start_idx")?;
    let mut stmt =
        conn.prepare("SELECT idx, hash FROM history_items WHERE idx >= ?1 ORDER BY idx")?;
    let rows = stmt.query_map([start_idx], |row| {
        Ok(HistoryRowInfo {
            idx: row.get::<_, i64>(0)? as u64,
            hash: row.get(1)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub(crate) fn item_hash(item: &HistoryItem) -> Result<String> {
    let normalized = normalized_history_value(item, ObjectCompression::none(), None)?;
    let json = serde_json::to_string(&normalized.value)?;
    Ok(sha256_hex(json.as_bytes()))
}

pub(crate) fn write_history_item(
    conn: &Connection,
    idx: usize,
    item: &HistoryItem,
    compression: ObjectCompression,
) -> Result<()> {
    write_history_item_at_block(conn, idx, idx, item, compression)
}

fn write_history_item_at_block(
    conn: &Connection,
    idx: usize,
    block_idx: usize,
    item: &HistoryItem,
    compression: ObjectCompression,
) -> Result<()> {
    let normalized = normalized_history_value(item, compression, Some(conn))?;
    insert_normalized_history_item(conn, idx, block_idx, &normalized)
}

pub(crate) fn replace_history_suffix(
    conn: &Connection,
    start_idx: usize,
    items: &[HistoryItem],
    compression: ObjectCompression,
) -> Result<()> {
    let _perf = perf::begin("store:history:replace_suffix");
    perf::record_value("store:history:dirty_suffix_rows", items.len() as u64);
    let start_idx_sql = checked_i64(start_idx as u64, "start_idx")?;
    let preserve_through_block_idx = conn
        .query_row(
            "SELECT MAX(block_idx) FROM transcript_blocks WHERE history_idx < ?1",
            [start_idx_sql],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .unwrap_or(-1);
    let search_deleted = conn.execute(
        "DELETE FROM transcript_search
         WHERE block_idx IN (
             SELECT block_idx FROM transcript_blocks
             WHERE history_idx >= ?1
                OR (history_idx IS NULL AND block_idx > ?2)
         )",
        params![start_idx_sql, preserve_through_block_idx],
    )?;
    let transcript_deleted = conn.execute(
        "DELETE FROM transcript_blocks
         WHERE history_idx >= ?1
            OR (history_idx IS NULL AND block_idx > ?2)",
        params![start_idx_sql, preserve_through_block_idx],
    )?;
    let history_deleted =
        conn.execute("DELETE FROM history_items WHERE idx >= ?1", [start_idx_sql])?;
    let first_block_idx = conn.query_row(
        "SELECT COALESCE(MAX(block_idx) + 1, 0) FROM transcript_blocks",
        [],
        |row| row.get::<_, i64>(0),
    )? as usize;
    for (block_idx, (offset, item)) in (first_block_idx..).zip(items.iter().enumerate()) {
        write_history_item_at_block(conn, start_idx + offset, block_idx, item, compression)?;
    }
    perf::record_value(
        "store:history:db_rows_deleted",
        search_deleted
            .saturating_add(transcript_deleted)
            .saturating_add(history_deleted) as u64,
    );
    perf::record_value("store:history:db_rows_inserted", items.len() as u64);
    Ok(())
}

pub(crate) fn read_history_items(conn: &Connection) -> Result<Vec<HistoryItem>> {
    let _perf = perf::begin("store:history:read_all");
    let mut stmt = conn.prepare("SELECT json FROM history_items ORDER BY idx")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    let mut json_bytes = 0u64;
    for row in rows {
        let json = row?;
        json_bytes = json_bytes.saturating_add(json.len() as u64);
        let mut value: Value = serde_json::from_str(&json)?;
        rehydrate_object_refs(conn, &mut value)?;
        out.push(serde_json::from_value(value)?);
    }
    perf::record_value("store:history:rows_read", out.len() as u64);
    perf::record_value("store:history:read_all_rows", out.len() as u64);
    perf::record_value("store:history:json_bytes_read", json_bytes);
    Ok(out)
}

pub(crate) fn read_history_items_range(
    conn: &Connection,
    range: Range<usize>,
) -> Result<Vec<HistoryItem>> {
    let _perf = perf::begin("store:history:read_range");
    if range.end <= range.start {
        perf::record_value("store:history:rows_read", 0);
        perf::record_value("store:history:read_range_rows", 0);
        perf::record_value("store:history:json_bytes_read", 0);
        return Ok(Vec::new());
    }
    let start = checked_i64(range.start as u64, "start_idx")?;
    let end = checked_i64(range.end as u64, "end_idx")?;
    let mut stmt =
        conn.prepare("SELECT json FROM history_items WHERE idx >= ?1 AND idx < ?2 ORDER BY idx")?;
    let rows = stmt.query_map(params![start, end], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    let mut json_bytes = 0u64;
    for row in rows {
        let json = row?;
        json_bytes = json_bytes.saturating_add(json.len() as u64);
        let mut value: Value = serde_json::from_str(&json)?;
        rehydrate_object_refs(conn, &mut value)?;
        out.push(serde_json::from_value(value)?);
    }
    perf::record_value("store:history:rows_read", out.len() as u64);
    perf::record_value("store:history:read_range_rows", out.len() as u64);
    perf::record_value("store:history:json_bytes_read", json_bytes);
    Ok(out)
}

pub(crate) fn history_item_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM history_items", [], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

pub(crate) fn transcript_block_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM transcript_blocks", [], |row| {
        row.get(0)
    })?;
    Ok(count.max(0) as usize)
}

pub(crate) fn transcript_missing_descriptor_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM transcript_blocks WHERE descriptor_json IS NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

pub(crate) fn transcript_descriptor_max_history_idx(conn: &Connection) -> Result<Option<usize>> {
    let idx: Option<i64> = conn.query_row(
        "SELECT MAX(history_idx) FROM transcript_blocks WHERE descriptor_json IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(idx.map(|idx| idx.max(0) as usize))
}

pub(crate) fn transcript_max_block_idx(conn: &Connection) -> Result<Option<u64>> {
    let idx: Option<i64> =
        conn.query_row("SELECT MAX(block_idx) FROM transcript_blocks", [], |row| {
            row.get(0)
        })?;
    Ok(idx.map(|idx| idx.max(0) as u64))
}

pub(crate) fn read_transcript_block_metadata_range(
    conn: &Connection,
    range: Range<usize>,
) -> Result<Vec<TranscriptBlockMetadataRecord>> {
    let _perf = perf::begin("store:transcript:read_block_metadata_range");
    if range.end <= range.start {
        return Ok(Vec::new());
    }
    let start = checked_i64(range.start as u64, "block_start_idx")?;
    let end = checked_i64(range.end as u64, "block_end_idx")?;
    let mut stmt = conn.prepare(
        "SELECT block_idx, descriptor_idx, history_idx, kind, tool_call_id, tool_name,
                content_hash, estimated_text_bytes, estimated_rows, preview_text,
                descriptor_json IS NOT NULL AS has_descriptor
         FROM transcript_blocks
         WHERE block_idx >= ?1 AND block_idx < ?2
         ORDER BY block_idx",
    )?;
    let rows = stmt.query_map(params![start, end], transcript_block_metadata_from_row)?;
    let records = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    perf::record_value(
        "store:transcript:block_metadata_rows_read",
        records.len() as u64,
    );
    Ok(records)
}

pub(crate) fn read_transcript_block_metadata_tail(
    conn: &Connection,
    count: usize,
) -> Result<Vec<TranscriptBlockMetadataRecord>> {
    let _perf = perf::begin("store:transcript:read_block_metadata_tail");
    if count == 0 {
        return Ok(Vec::new());
    }
    let limit = checked_i64(count as u64, "block_tail_count")?;
    let mut stmt = conn.prepare(
        "SELECT block_idx, descriptor_idx, history_idx, kind, tool_call_id, tool_name,
                content_hash, estimated_text_bytes, estimated_rows, preview_text,
                descriptor_json IS NOT NULL AS has_descriptor
         FROM transcript_blocks
         ORDER BY block_idx DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], transcript_block_metadata_from_row)?;
    let mut records = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    records.reverse();
    perf::record_value(
        "store:transcript:block_metadata_rows_read",
        records.len() as u64,
    );
    Ok(records)
}

fn transcript_block_metadata_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<TranscriptBlockMetadataRecord> {
    Ok(TranscriptBlockMetadataRecord {
        block_idx: row.get::<_, i64>(0)? as u64,
        descriptor_idx: row.get::<_, Option<i64>>(1)?.map(|idx| idx as u64),
        history_idx: row.get::<_, Option<i64>>(2)?.map(|idx| idx as u64),
        kind: row.get(3)?,
        tool_call_id: row.get(4)?,
        tool_name: row.get(5)?,
        content_hash: row.get(6)?,
        estimated_text_bytes: row.get::<_, i64>(7)? as u64,
        estimated_rows: row.get::<_, Option<i64>>(8)?.map(|rows| rows as u64),
        preview_text: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        has_descriptor: row.get::<_, i64>(10)? != 0,
    })
}

pub(crate) fn history_text_bytes(conn: &Connection) -> Result<u64> {
    let _perf = perf::begin("store:history:text_bytes");
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(estimated_text_bytes), 0) FROM transcript_blocks",
        [],
        |row| row.get(0),
    )?;
    perf::record_value("store:history:text_bytes", total.max(0) as u64);
    Ok(total as u64)
}

pub(crate) fn search_blob(conn: &Connection) -> Result<String> {
    let _perf = perf::begin("store:transcript:search_blob_full");
    let mut stmt =
        conn.prepare("SELECT text FROM transcript_search WHERE text != '' ORDER BY block_idx")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = String::new();
    let mut row_count = 0u64;
    for row in rows {
        let text = row?;
        if text.is_empty() {
            continue;
        }
        row_count = row_count.saturating_add(1);
        out.push_str(&text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    perf::record_value("store:transcript:search_blob_rows_read", row_count);
    perf::record_value("store:transcript:search_blob_bytes_read", out.len() as u64);
    Ok(out)
}

pub(crate) fn replace_transcript_descriptor_records(
    conn: &Connection,
    records: &[TranscriptDescriptorRecord],
    compression: ObjectCompression,
) -> Result<()> {
    replace_transcript_descriptor_suffix(conn, 0, records, compression)
}

pub(crate) fn replace_transcript_descriptor_suffix(
    conn: &Connection,
    start_descriptor_idx: usize,
    records: &[TranscriptDescriptorRecord],
    compression: ObjectCompression,
) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = replace_transcript_descriptor_suffix_in_transaction(
        conn,
        start_descriptor_idx,
        records,
        compression,
    );
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

pub(crate) fn replace_transcript_descriptor_suffix_in_transaction(
    conn: &Connection,
    start_descriptor_idx: usize,
    records: &[TranscriptDescriptorRecord],
    compression: ObjectCompression,
) -> Result<()> {
    let _perf = perf::begin("store:transcript:replace_descriptor_suffix");
    compact_transcript_descriptor_indices(conn)?;
    perf::record_value(
        "store:transcript:dirty_descriptor_suffix_rows",
        records.len() as u64,
    );
    let start_descriptor_idx = checked_i64(start_descriptor_idx as u64, "start_descriptor_idx")?;
    let first_replacement_block_idx = records
        .first()
        .map(|record| checked_i64(record.block_idx, "block_idx"))
        .transpose()?;
    let search_deleted = conn.execute(
        "DELETE FROM transcript_search
         WHERE block_idx IN (
             SELECT block_idx FROM transcript_blocks
             WHERE descriptor_idx >= ?1
                OR (?2 IS NOT NULL AND block_idx >= ?2)
         )",
        params![start_descriptor_idx, first_replacement_block_idx],
    )?;
    let descriptor_deleted = conn.execute(
        "DELETE FROM transcript_blocks
         WHERE descriptor_idx >= ?1
            OR (?2 IS NOT NULL AND block_idx >= ?2)",
        params![start_descriptor_idx, first_replacement_block_idx],
    )?;
    for (offset, record) in records.iter().enumerate() {
        let descriptor_idx = checked_i64(
            start_descriptor_idx as u64 + offset as u64,
            "descriptor_idx",
        )?;
        let mut descriptor: Value = serde_json::from_str(&record.descriptor_json)?;
        let mut refs = Vec::new();
        normalize_metadata(Some(conn), &mut descriptor, compression, &mut refs)?;
        let descriptor_json = serde_json::to_string(&descriptor)?;
        let tool_state_json = match &record.tool_state_json {
            Some(json) => {
                let mut value: Value = serde_json::from_str(json)?;
                normalize_metadata(Some(conn), &mut value, compression, &mut refs)?;
                Some(serde_json::to_string(&value)?)
            }
            None => None,
        };
        insert_transcript_descriptor_record(
            conn,
            descriptor_idx,
            record,
            &descriptor_json,
            tool_state_json.as_deref(),
        )?;
        for (hash, role) in refs {
            if let Some(history_idx) = record.history_idx {
                conn.execute(
                    "INSERT OR IGNORE INTO history_object_refs (history_idx, object_hash, role)
                     VALUES (?1, ?2, ?3)",
                    params![checked_i64(history_idx, "history_idx")?, hash, role],
                )?;
            }
        }
    }
    perf::record_value(
        "store:transcript:descriptor_db_rows_deleted",
        search_deleted.saturating_add(descriptor_deleted) as u64,
    );
    perf::record_value(
        "store:transcript:descriptor_db_rows_inserted",
        records.len() as u64,
    );
    Ok(())
}

fn compact_transcript_descriptor_indices(conn: &Connection) -> Result<()> {
    let (count, indexed_count, max_descriptor_idx) = conn.query_row(
        "SELECT COUNT(*), COUNT(descriptor_idx), COALESCE(MAX(descriptor_idx), -1)
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;
    if count == indexed_count && max_descriptor_idx == count.saturating_sub(1) {
        return Ok(());
    }

    let mut stmt = conn.prepare(
        "SELECT block_idx, descriptor_idx
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL
         ORDER BY descriptor_idx IS NULL, descriptor_idx, block_idx",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    let rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;

    for (idx, (block_idx, _)) in rows.iter().enumerate() {
        conn.execute(
            "UPDATE transcript_blocks SET descriptor_idx = ?1 WHERE block_idx = ?2",
            params![-((idx as i64) + 1), block_idx],
        )?;
    }
    for (idx, (block_idx, _)) in rows.iter().enumerate() {
        conn.execute(
            "UPDATE transcript_blocks SET descriptor_idx = ?1 WHERE block_idx = ?2",
            params![idx as i64, block_idx],
        )?;
    }
    Ok(())
}

fn insert_transcript_descriptor_record(
    conn: &Connection,
    descriptor_idx: i64,
    record: &TranscriptDescriptorRecord,
    descriptor_json: &str,
    tool_state_json: Option<&str>,
) -> Result<()> {
    let block_idx = checked_i64(record.block_idx, "block_idx")?;
    let history_idx = record
        .history_idx
        .map(|idx| checked_i64(idx, "history_idx"))
        .transpose()?;
    conn.execute(
        "INSERT INTO transcript_blocks (
            block_idx, descriptor_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
            estimated_text_bytes, descriptor_json, origin_json, tool_state_json,
            preview_text, search_text
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            block_idx,
            descriptor_idx,
            history_idx,
            record.kind,
            record.tool_call_id,
            record.tool_name,
            record.content_hash,
            checked_i64(record.estimated_text_bytes, "estimated_text_bytes")?,
            descriptor_json,
            record.origin_json,
            tool_state_json,
            record.preview_text,
            record.search_text,
        ],
    )?;
    insert_transcript_search(conn, block_idx, history_idx, &record.search_text)?;
    Ok(())
}

pub(crate) fn transcript_descriptor_count(conn: &Connection) -> Result<usize> {
    let _perf = perf::begin("store:transcript:descriptor_count");
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM transcript_blocks WHERE descriptor_json IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    perf::record_value("store:transcript:descriptor_count_total", count as u64);
    Ok(count as usize)
}

pub(crate) fn transcript_descriptor_dense_extent(conn: &Connection) -> Result<usize> {
    let _perf = perf::begin("store:transcript:descriptor_dense_extent");
    let count: i64 = conn.query_row(
        "SELECT COALESCE(MAX(descriptor_idx) + 1, 0)
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    perf::record_value(
        "store:transcript:descriptor_dense_extent_total",
        count as u64,
    );
    Ok(count as usize)
}

pub(crate) fn transcript_descriptor_index_for_block_idx(
    conn: &Connection,
    block_idx: u64,
) -> Result<Option<TranscriptDescriptorIndex>> {
    let _perf = perf::begin("store:transcript:descriptor_index_for_block");
    let block_idx = checked_i64(block_idx, "block_idx")?;
    let index: Option<i64> = conn
        .query_row(
            "SELECT descriptor_idx
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL AND block_idx = ?1",
            [block_idx],
            |row| row.get(0),
        )
        .optional()?;
    perf::record_value(
        "store:transcript:descriptor_block_found",
        u64::from(index.is_some()),
    );
    Ok(index.map(|index| TranscriptDescriptorIndex::new(index.max(0) as usize)))
}

pub(crate) fn transcript_descriptor_estimated_rows(
    conn: &Connection,
    range: TranscriptDescriptorRange,
    width: u16,
) -> Result<u64> {
    let _perf = perf::begin("store:transcript:descriptor_estimated_rows");
    let start = range.start().get();
    let end = range.end().get();
    if start >= end {
        perf::record_value("store:transcript:descriptor_estimated_rows_requested", 0);
        perf::record_value("store:transcript:descriptor_estimated_rows_total", 0);
        return Ok(0);
    }
    let width = width.max(1) as u64;
    let limit = checked_i64((end - start) as u64, "descriptor_estimated_rows_len")?;
    let offset = checked_i64(start as u64, "descriptor_estimated_rows_start")?;
    let rows: i64 = conn.query_row(
        "SELECT COALESCE(SUM(
             COALESCE(
                 estimated_rows,
                 CASE
                     WHEN kind IN ('tool', 'process_status', 'mode') THEN
                         ((MAX(LENGTH(COALESCE(preview_text, '')), 1) + ?1 - 1) / ?1) + 1
                     ELSE
                         ((MAX(estimated_text_bytes, 1) + ?1 - 1) / ?1) + 1
                 END
             )
         ), 0)
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL
           AND descriptor_idx >= ?3
           AND descriptor_idx < ?3 + ?2",
        params![
            checked_i64(width, "descriptor_estimated_rows_width")?,
            limit,
            offset
        ],
        |row| row.get(0),
    )?;
    perf::record_value(
        "store:transcript:descriptor_estimated_rows_requested",
        end.saturating_sub(start) as u64,
    );
    perf::record_value(
        "store:transcript:descriptor_estimated_rows_total",
        rows as u64,
    );
    Ok(rows as u64)
}

pub(crate) fn read_transcript_descriptor_records(
    conn: &Connection,
) -> Result<Vec<TranscriptDescriptorRecord>> {
    let _perf = perf::begin("store:transcript:read_descriptors_full");
    let mut stmt = conn.prepare(
        "SELECT block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
                estimated_text_bytes, preview_text, search_text, descriptor_json,
                origin_json, tool_state_json
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL
         ORDER BY descriptor_idx",
    )?;
    let records = read_transcript_descriptor_records_from_stmt(
        conn,
        &mut stmt,
        [],
        TranscriptDescriptorHydration::Hydrated,
    )?;
    perf::record_value(
        "store:transcript:descriptors_full_loaded",
        records.len() as u64,
    );
    Ok(records)
}

pub(crate) fn read_transcript_descriptor_slice(
    conn: &Connection,
    range: TranscriptDescriptorRange,
) -> Result<TranscriptDescriptorSlice> {
    let _perf = perf::begin("store:transcript:read_descriptor_slice");
    let total_count = transcript_descriptor_count(conn)?;
    read_transcript_descriptor_slice_with_total(conn, range, total_count)
}

pub(crate) fn read_transcript_descriptor_slice_with_total(
    conn: &Connection,
    range: TranscriptDescriptorRange,
    total_count: usize,
) -> Result<TranscriptDescriptorSlice> {
    let start = range.start().get().min(total_count);
    let end = range.end().get().min(total_count);
    if start >= end {
        perf::record_value("store:transcript:descriptor_slice_requested", 0);
        perf::record_value("store:transcript:descriptors_loaded", 0);
        perf::record_value("store:transcript:descriptor_json_bytes_loaded", 0);
        return Ok(TranscriptDescriptorSlice::new(
            TranscriptDescriptorIndex::new(start),
            total_count,
            TranscriptDescriptorHydration::ObjectBacked,
            Vec::new(),
        ));
    }
    let limit = checked_i64((end - start) as u64, "descriptor_range_len")?;
    let offset = checked_i64(start as u64, "descriptor_range_start")?;
    let mut stmt = conn.prepare(
        "SELECT block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
                estimated_text_bytes, preview_text, '' AS search_text, descriptor_json,
                origin_json, tool_state_json
         FROM transcript_blocks
         WHERE descriptor_idx >= ?2
           AND descriptor_idx < ?2 + ?1
         ORDER BY descriptor_idx",
    )?;
    let records = read_transcript_descriptor_records_from_stmt(
        conn,
        &mut stmt,
        params![limit, offset],
        TranscriptDescriptorHydration::ObjectBacked,
    )?;
    perf::record_value(
        "store:transcript:descriptor_slice_requested",
        end.saturating_sub(start) as u64,
    );
    Ok(TranscriptDescriptorSlice::new(
        TranscriptDescriptorIndex::new(start),
        total_count,
        TranscriptDescriptorHydration::ObjectBacked,
        records,
    ))
}

pub(crate) fn read_transcript_descriptor_tail_slice(
    conn: &Connection,
    count: usize,
) -> Result<TranscriptDescriptorSlice> {
    let _perf = perf::begin("store:transcript:read_descriptor_tail_slice");
    perf::record_value("store:transcript:descriptor_tail_requested", count as u64);
    let total_count = transcript_descriptor_count(conn)?;
    read_transcript_descriptor_tail_slice_with_total(conn, total_count, count)
}

pub(crate) fn read_transcript_descriptor_tail_slice_with_total(
    conn: &Connection,
    total_count: usize,
    count: usize,
) -> Result<TranscriptDescriptorSlice> {
    let count = count.min(total_count);
    let start = total_count.saturating_sub(count);
    if count == 0 {
        perf::record_value("store:transcript:descriptor_slice_requested", 0);
        perf::record_value("store:transcript:descriptors_loaded", 0);
        perf::record_value("store:transcript:descriptor_json_bytes_loaded", 0);
        return Ok(TranscriptDescriptorSlice::new(
            TranscriptDescriptorIndex::new(start),
            total_count,
            TranscriptDescriptorHydration::ObjectBacked,
            Vec::new(),
        ));
    }
    let limit = checked_i64(count as u64, "descriptor_tail_len")?;
    let mut stmt = conn.prepare(
        "SELECT block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
                estimated_text_bytes, preview_text, '' AS search_text, descriptor_json,
                origin_json, tool_state_json
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL
         ORDER BY descriptor_idx DESC
         LIMIT ?1",
    )?;
    let mut records = read_transcript_descriptor_records_from_stmt(
        conn,
        &mut stmt,
        params![limit],
        TranscriptDescriptorHydration::ObjectBacked,
    )?;
    records.reverse();
    perf::record_value("store:transcript:descriptor_slice_requested", count as u64);
    Ok(TranscriptDescriptorSlice::new(
        TranscriptDescriptorIndex::new(start),
        total_count,
        TranscriptDescriptorHydration::ObjectBacked,
        records,
    ))
}

pub(crate) fn read_transcript_descriptor_centered_slice(
    conn: &Connection,
    center_descriptor_idx: u64,
    before: usize,
    after: usize,
) -> Result<TranscriptDescriptorSlice> {
    let _perf = perf::begin("store:transcript:read_descriptor_centered_slice");
    let total_count = transcript_descriptor_count(conn)?;
    if total_count == 0 {
        return read_transcript_descriptor_slice_with_total(conn, (0..0).into(), total_count);
    }
    let center = (center_descriptor_idx as usize).min(total_count.saturating_sub(1));
    let start = center.saturating_sub(before);
    let end = center
        .saturating_add(after)
        .saturating_add(1)
        .min(total_count);
    read_transcript_descriptor_slice_with_total(conn, (start..end).into(), total_count)
}

pub(crate) fn read_transcript_descriptor_before_kind_at_index(
    conn: &Connection,
    kind: &str,
    before_or_at_descriptor_index: u64,
) -> Result<Option<TranscriptDescriptorRecord>> {
    let _perf = perf::begin("store:transcript:read_descriptor_before_kind");
    let before_or_at = checked_i64(
        before_or_at_descriptor_index,
        "before_or_at_descriptor_index",
    )?;
    let mut stmt = conn.prepare(
        "SELECT block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
                estimated_text_bytes, preview_text, '' AS search_text, descriptor_json,
                origin_json, tool_state_json
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL
           AND kind = ?1
           AND descriptor_idx <= ?2
         ORDER BY descriptor_idx DESC
         LIMIT 1",
    )?;
    let mut records = read_transcript_descriptor_records_from_stmt(
        conn,
        &mut stmt,
        params![kind, before_or_at],
        TranscriptDescriptorHydration::ObjectBacked,
    )?;
    Ok(records.pop())
}

pub(crate) fn read_transcript_descriptor_after_kind_at_index(
    conn: &Connection,
    kind: &str,
    after_or_at_descriptor_index: u64,
) -> Result<Option<TranscriptDescriptorRecord>> {
    let _perf = perf::begin("store:transcript:read_descriptor_after_kind");
    let after_or_at = checked_i64(after_or_at_descriptor_index, "after_or_at_descriptor_index")?;
    let mut stmt = conn.prepare(
        "SELECT block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
                estimated_text_bytes, preview_text, '' AS search_text, descriptor_json,
                origin_json, tool_state_json
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL
           AND kind = ?1
           AND descriptor_idx >= ?2
         ORDER BY descriptor_idx ASC
         LIMIT 1",
    )?;
    let mut records = read_transcript_descriptor_records_from_stmt(
        conn,
        &mut stmt,
        params![kind, after_or_at],
        TranscriptDescriptorHydration::ObjectBacked,
    )?;
    Ok(records.pop())
}

fn read_transcript_descriptor_records_from_stmt<P>(
    conn: &Connection,
    stmt: &mut Statement<'_>,
    params: P,
    hydration: TranscriptDescriptorHydration,
) -> Result<Vec<TranscriptDescriptorRecord>>
where
    P: rusqlite::Params,
{
    let rows = stmt.query_map(params, |row| {
        Ok(TranscriptDescriptorRecord {
            block_idx: row.get::<_, i64>(0)? as u64,
            history_idx: row.get::<_, Option<i64>>(1)?.map(|idx| idx as u64),
            kind: row.get(2)?,
            tool_call_id: row.get(3)?,
            tool_name: row.get(4)?,
            content_hash: row.get(5)?,
            estimated_text_bytes: row.get::<_, i64>(6)? as u64,
            preview_text: row.get(7)?,
            search_text: row.get(8)?,
            descriptor_json: row.get(9)?,
            origin_json: row.get(10)?,
            tool_state_json: row.get(11)?,
        })
    })?;
    let mut records = Vec::new();
    let mut json_bytes = 0u64;
    for row in rows {
        let mut record = row?;
        json_bytes = json_bytes.saturating_add(record.descriptor_json.len() as u64);
        if let Some(json) = &record.tool_state_json {
            json_bytes = json_bytes.saturating_add(json.len() as u64);
        }
        if hydration.hydrates_objects() {
            let mut descriptor: Value = serde_json::from_str(&record.descriptor_json)?;
            rehydrate_object_refs(conn, &mut descriptor)?;
            record.descriptor_json = serde_json::to_string(&descriptor)?;
            if let Some(json) = &record.tool_state_json {
                let mut value: Value = serde_json::from_str(json)?;
                rehydrate_object_refs(conn, &mut value)?;
                record.tool_state_json = Some(serde_json::to_string(&value)?);
            }
        }
        records.push(record);
    }
    perf::record_value("store:transcript:descriptors_loaded", records.len() as u64);
    perf::record_value("store:transcript:descriptor_json_bytes_loaded", json_bytes);
    match hydration {
        TranscriptDescriptorHydration::Hydrated => perf::record_value(
            "store:transcript:descriptors_hydrated_loaded",
            records.len() as u64,
        ),
        TranscriptDescriptorHydration::ObjectBacked => perf::record_value(
            "store:transcript:descriptors_object_backed_loaded",
            records.len() as u64,
        ),
    }
    Ok(records)
}

pub(crate) fn search_transcript_candidates(
    conn: &Connection,
    query: &str,
) -> Result<Vec<TranscriptSearchCandidate>> {
    search_transcript_candidate_page(
        conn,
        query,
        None,
        TranscriptSearchDirection::Forward,
        usize::MAX,
    )
}

pub(crate) fn search_transcript_candidate_page(
    conn: &Connection,
    query: &str,
    origin_block_idx: Option<u64>,
    direction: TranscriptSearchDirection,
    limit: usize,
) -> Result<Vec<TranscriptSearchCandidate>> {
    let _perf = perf::begin("store:transcript:search_candidates");
    if query.is_empty() || limit == 0 {
        perf::record_value("store:transcript:search_candidate_rows_scanned", 0);
        perf::record_value("store:transcript:search_candidates_loaded", 0);
        return Ok(Vec::new());
    }
    let terms = search_terms(query);
    if terms.is_empty() {
        perf::record_value("store:transcript:search_candidate_rows_scanned", 0);
        perf::record_value("store:transcript:search_candidates_loaded", 0);
        return Ok(Vec::new());
    }

    let driver = search_driver_term(conn, &terms)?;
    let other_terms = terms
        .iter()
        .filter(|term| *term != &driver)
        .map(String::as_str)
        .collect::<Vec<_>>();
    let page_size = if limit == usize::MAX {
        1024usize
    } else {
        limit.max(64)
    };
    let mut out = Vec::new();
    let mut bound = origin_block_idx;
    let mut inclusive = origin_block_idx.is_some();
    let mut scanned = 0usize;
    let mut batches = 0usize;

    loop {
        let batch = search_transcript_candidate_batch(
            conn,
            TranscriptCandidateBatchQuery {
                driver: &driver,
                other_terms: &other_terms,
                bound,
                inclusive,
                direction,
                query,
                page_size,
            },
        )?;
        batches = batches.saturating_add(1);
        let fetched = batch.len();
        if fetched == 0 {
            break;
        }
        scanned = scanned.saturating_add(fetched);
        bound = batch.last().map(|row| row.block_idx);
        inclusive = false;
        for row in batch {
            out.push(TranscriptSearchCandidate {
                block_idx: row.block_idx,
                history_idx: row.history_idx,
            });
            if out.len() >= limit {
                break;
            }
        }
        if out.len() >= limit || fetched < page_size {
            break;
        }
    }

    if matches!(direction, TranscriptSearchDirection::Backward) {
        out.reverse();
    }
    perf::record_value("store:transcript:search_candidate_batches", batches as u64);
    perf::record_value(
        "store:transcript:search_candidate_rows_scanned",
        scanned as u64,
    );
    perf::record_value(
        "store:transcript:search_candidates_loaded",
        out.len() as u64,
    );
    Ok(out)
}

#[derive(Debug)]
struct TranscriptCandidateRow {
    block_idx: u64,
    history_idx: Option<u64>,
}

fn search_driver_term(conn: &Connection, terms: &[String]) -> Result<String> {
    if terms.len() == 1 {
        perf::record_value("store:transcript:search_driver_terms", 1);
        return Ok(terms[0].clone());
    }
    let _perf = perf::begin("store:transcript:search_driver_term");
    let cap = checked_i64(
        SEARCH_DRIVER_TERM_POSTING_CAP,
        "search_driver_term_posting_cap",
    )?;
    let mut stmt = conn.prepare(
        "SELECT COUNT(*) FROM (
             SELECT 1 FROM transcript_search_terms WHERE term = ?1 LIMIT ?2
         )",
    )?;
    let mut best: Option<(String, u64)> = None;
    for term in terms {
        let postings = stmt
            .query_row(params![term, cap], |row| row.get::<_, i64>(0))?
            .max(0) as u64;
        if best
            .as_ref()
            .is_none_or(|(_, best_postings)| postings < *best_postings)
        {
            best = Some((term.clone(), postings));
        }
    }
    let (term, postings) = best.expect("search terms are non-empty");
    perf::record_value("store:transcript:search_driver_terms", terms.len() as u64);
    perf::record_value(
        "store:transcript:search_driver_posting_cap",
        SEARCH_DRIVER_TERM_POSTING_CAP,
    );
    perf::record_value("store:transcript:search_driver_postings", postings);
    Ok(term)
}

struct TranscriptCandidateBatchQuery<'a> {
    driver: &'a str,
    other_terms: &'a [&'a str],
    bound: Option<u64>,
    inclusive: bool,
    direction: TranscriptSearchDirection,
    query: &'a str,
    page_size: usize,
}

fn search_transcript_candidate_batch(
    conn: &Connection,
    query: TranscriptCandidateBatchQuery<'_>,
) -> Result<Vec<TranscriptCandidateRow>> {
    let order = match query.direction {
        TranscriptSearchDirection::Forward => "ASC",
        TranscriptSearchDirection::Backward => "DESC",
    };
    let bound_filter = match (query.bound, query.inclusive, query.direction) {
        (Some(_), true, TranscriptSearchDirection::Forward) => "AND d.block_idx >= ?",
        (Some(_), false, TranscriptSearchDirection::Forward) => "AND d.block_idx > ?",
        (Some(_), true, TranscriptSearchDirection::Backward) => "AND d.block_idx <= ?",
        (Some(_), false, TranscriptSearchDirection::Backward) => "AND d.block_idx < ?",
        (None, _, _) => "",
    };
    let exists_filters = query
        .other_terms
        .iter()
        .map(|_| {
            "AND EXISTS (
                SELECT 1 FROM transcript_search_terms t
                WHERE t.term = ? AND t.block_idx = d.block_idx
            )"
        })
        .collect::<Vec<_>>()
        .join(" ");
    let sql = format!(
        "SELECT s.block_idx, s.history_idx
         FROM transcript_search_terms d
         JOIN transcript_search s ON s.block_idx = d.block_idx
         WHERE d.term = ? {bound_filter} {exists_filters}
           AND instr(s.text, ?) > 0
         ORDER BY d.block_idx {order}
         LIMIT ?"
    );
    let mut values =
        Vec::with_capacity(3 + query.other_terms.len() + usize::from(query.bound.is_some()));
    values.push(SqlValue::from(query.driver.to_string()));
    if let Some(bound) = query.bound {
        values.push(SqlValue::from(checked_i64(
            bound,
            "search_candidate_bound",
        )?));
    }
    values.extend(
        query
            .other_terms
            .iter()
            .map(|term| SqlValue::from((*term).to_string())),
    );
    values.push(SqlValue::from(query.query.to_string()));
    values.push(SqlValue::from(checked_i64(
        query.page_size as u64,
        "search_candidate_page_size",
    )?));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| {
        Ok(TranscriptCandidateRow {
            block_idx: row.get::<_, i64>(0)? as u64,
            history_idx: row.get::<_, Option<i64>>(1)?.map(|idx| idx as u64),
        })
    })?;
    let out = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    perf::record_value(
        "store:transcript:search_candidate_batch_rows",
        out.len() as u64,
    );
    Ok(out)
}

fn insert_transcript_search(
    conn: &Connection,
    block_idx: i64,
    history_idx: Option<i64>,
    text: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO transcript_search (block_idx, history_idx, text)
         VALUES (?1, ?2, ?3)",
        params![block_idx, history_idx, text],
    )?;
    insert_transcript_search_terms(conn, block_idx, text)
}

fn insert_transcript_search_terms(conn: &Connection, block_idx: i64, text: &str) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO transcript_search_terms (term, block_idx)
         VALUES (?1, ?2)",
    )?;
    for term in index_terms(text) {
        stmt.execute(params![term, block_idx])?;
    }
    Ok(())
}

fn search_terms(text: &str) -> Vec<String> {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return Vec::new();
    }
    grams_for_sizes(&chars, std::iter::once(chars.len().min(3)))
}

fn index_terms(text: &str) -> Vec<String> {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return Vec::new();
    }
    grams_for_sizes(&chars, 1..=chars.len().min(3))
}

fn grams_for_sizes(chars: &[char], sizes: impl IntoIterator<Item = usize>) -> Vec<String> {
    let mut terms = Vec::new();
    for n in sizes {
        terms.extend(
            chars
                .windows(n)
                .map(|window| window.iter().collect::<String>()),
        );
    }
    terms.sort_unstable();
    terms.dedup();
    terms
}

struct NormalizedHistoryItem {
    value: Value,
    json: String,
    hash: String,
    kind: String,
    search_text: String,
    refs: Vec<(String, &'static str)>,
}

fn normalized_history_value(
    item: &HistoryItem,
    compression: ObjectCompression,
    conn: Option<&Connection>,
) -> Result<NormalizedHistoryItem> {
    let mut value = serde_json::to_value(item)?;
    let mut refs = Vec::new();
    normalize_metadata(conn, &mut value, compression, &mut refs)?;
    let json = serde_json::to_string(&value)?;
    let hash = sha256_hex(json.as_bytes());
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let search_text = collect_text(&value, 64 * 1024);
    Ok(NormalizedHistoryItem {
        value,
        json,
        hash,
        kind,
        search_text,
        refs,
    })
}

fn insert_normalized_history_item(
    conn: &Connection,
    idx: usize,
    block_idx: usize,
    item: &NormalizedHistoryItem,
) -> Result<()> {
    let idx = checked_i64(idx as u64, "history_idx")?;
    let block_idx = checked_i64(block_idx as u64, "block_idx")?;
    conn.execute(
        "INSERT INTO history_items (idx, kind, json, hash, search_text, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())",
        params![idx, item.kind, item.json, item.hash, item.search_text],
    )?;
    for (hash, role) in &item.refs {
        conn.execute(
            "INSERT OR IGNORE INTO history_object_refs (history_idx, object_hash, role)
             VALUES (?1, ?2, ?3)",
            params![idx, hash, role],
        )?;
    }
    insert_transcript_block(
        conn,
        block_idx,
        idx,
        &item.kind,
        &item.value,
        &item.search_text,
        &item.hash,
    )
}

fn normalize_metadata(
    conn: Option<&Connection>,
    value: &mut Value,
    compression: ObjectCompression,
    refs: &mut Vec<(String, &'static str)>,
) -> Result<()> {
    match value {
        Value::Object(map) => {
            let keys = map.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                let Some(child) = map.get_mut(&key) else {
                    continue;
                };
                if key == "metadata" && !child.is_null() {
                    let bytes = serde_json::to_vec(child)?;
                    if bytes.len() >= METADATA_OBJECT_MIN_BYTES {
                        let hash = if let Some(conn) = conn {
                            let object =
                                object::put_object(conn, "tool_metadata", &bytes, compression)?;
                            refs.push((object.hash().to_string(), "metadata"));
                            object.hash().to_string()
                        } else {
                            sha256_hex(&bytes)
                        };
                        *child = object_ref_json(&hash, bytes.len() as u64);
                        continue;
                    }
                }
                normalize_metadata(conn, child, compression, refs)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                normalize_metadata(conn, child, compression, refs)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn object_ref_json(hash: &str, raw_size: u64) -> Value {
    json!({ OBJECT_REF_KEY: { "hash": hash, "raw_size": raw_size } })
}

fn insert_transcript_block(
    conn: &Connection,
    block_idx: i64,
    history_idx: i64,
    kind: &str,
    value: &Value,
    search_text: &str,
    content_hash: &str,
) -> Result<()> {
    let tool_call_id =
        find_string_key(value, "call_id").or_else(|| find_string_key(value, "tool_call_id"));
    let tool_name = find_string_key(value, "name");
    let preview = preview(search_text, 512);
    conn.execute(
        "INSERT INTO transcript_blocks (
            block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
            estimated_text_bytes, preview_text, search_text
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            block_idx,
            history_idx,
            kind,
            tool_call_id,
            tool_name,
            content_hash,
            checked_i64(search_text.len() as u64, "estimated_text_bytes")?,
            preview,
            search_text
        ],
    )?;
    insert_transcript_search(conn, block_idx, Some(history_idx), search_text)?;
    Ok(())
}

pub(crate) fn collect_text(value: &Value, max_bytes: usize) -> String {
    let mut out = String::new();
    collect_text_inner(value, &mut out, max_bytes);
    out
}

fn collect_text_inner(value: &Value, out: &mut String, max_bytes: usize) {
    if out.len() >= max_bytes {
        return;
    }
    match value {
        Value::String(text) => {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
            truncate_utf8(out, max_bytes);
        }
        Value::Array(values) => {
            for value in values {
                collect_text_inner(value, out, max_bytes);
            }
        }
        Value::Object(map) => {
            if map.contains_key(OBJECT_REF_KEY) {
                return;
            }
            for value in map.values() {
                collect_text_inner(value, out, max_bytes);
            }
        }
        _ => {}
    }
}

fn truncate_utf8(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = 0;
    for (idx, _) in text.char_indices() {
        if idx > max_bytes {
            break;
        }
        end = idx;
    }
    text.truncate(end);
}

fn preview(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = 0;
    for (idx, _) in text.char_indices() {
        if idx > max_bytes {
            break;
        }
        end = idx;
    }
    text[..end].to_string()
}

fn find_string_key(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(value) = map.get(key).and_then(Value::as_str) {
                return Some(value.to_string());
            }
            map.values().find_map(|value| find_string_key(value, key))
        }
        Value::Array(values) => values.iter().find_map(|value| find_string_key(value, key)),
        _ => None,
    }
}

pub(crate) fn rehydrate_object_refs(conn: &Connection, value: &mut Value) -> Result<()> {
    match value {
        Value::Object(map) => {
            if let Some(hash) = map
                .get(OBJECT_REF_KEY)
                .and_then(|value| value.get("hash"))
                .and_then(Value::as_str)
            {
                if let Some(bytes) = object::object_bytes_by_hash(conn, hash)? {
                    *value = serde_json::from_slice(&bytes)?;
                }
                return Ok(());
            }
            for child in map.values_mut() {
                rehydrate_object_refs(conn, child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                rehydrate_object_refs(conn, child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_search_candidate_plan_uses_term_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let details = query_plan_details(
            &conn,
            "SELECT s.block_idx, s.history_idx
             FROM transcript_search_terms d
             JOIN transcript_search s ON s.block_idx = d.block_idx
             WHERE d.term = ?1
               AND instr(s.text, ?2) > 0
             ORDER BY d.block_idx ASC
             LIMIT ?3",
            rusqlite::params!["abc", "abcdef", 64_i64],
        );

        assert!(
            details
                .iter()
                .any(|detail| detail.contains("SEARCH d USING COVERING INDEX")),
            "{details:#?}"
        );
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("SEARCH s USING INTEGER PRIMARY KEY")),
            "{details:#?}"
        );
        assert!(
            details
                .iter()
                .all(|detail| !detail.contains("USE TEMP B-TREE")),
            "{details:#?}"
        );
    }

    fn query_plan_details<P: rusqlite::Params>(
        conn: &Connection,
        sql: &str,
        params: P,
    ) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map(params, |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }
}
