use protocol::HistoryItem;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Statement};
use serde_json::{json, Value};
use smelt_perf::perf;
use std::ops::Range;

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::object::{self, checked_i64, sha256_hex};
use rusqlite::types::Value as SqlValue;

pub(crate) const METADATA_OBJECT_MIN_BYTES: usize = 4 * 1024;
pub(crate) const LARGE_REWIND_GC_MIN_ROWS: usize = 128;
pub(crate) const OBJECT_REF_KEY: &str = "$smelt_object_ref";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HistoryObjectRole {
    AttachmentImage,
    Metadata,
}

impl HistoryObjectRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AttachmentImage => "attachment_image",
            Self::Metadata => "metadata",
        }
    }

    pub(crate) fn from_str(role: &str) -> Result<Self> {
        match role {
            "attachment_image" => Ok(Self::AttachmentImage),
            "metadata" => Ok(Self::Metadata),
            _ => Err(StoreError::Integrity(format!(
                "unknown history object role {role:?}"
            ))),
        }
    }
}

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

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TranscriptDescriptorRecord {
    pub block_idx: u64,
    pub history_idx: Option<u64>,
    pub kind: String,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub content_hash: String,
    pub estimated_text_bytes: u64,
    pub preview_text: String,
    pub indexed_text: String,
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

pub(crate) fn incoming_object_hashes(
    items: &[HistoryItem],
    descriptors: Option<&[TranscriptDescriptorRecord]>,
) -> Result<Vec<String>> {
    let mut hashes = std::collections::BTreeSet::new();
    for item in items {
        let normalized = normalized_history_value(item, ObjectCompression::none(), None)?;
        hashes.extend(normalized.refs.into_iter().map(|(hash, _)| hash));
    }
    for record in descriptors.into_iter().flatten() {
        let mut descriptor: Value = serde_json::from_str(&record.descriptor_json)?;
        let mut refs = Vec::new();
        normalize_metadata(None, &mut descriptor, ObjectCompression::none(), &mut refs)?;
        if let Some(tool_state_json) = &record.tool_state_json {
            let mut tool_state: Value = serde_json::from_str(tool_state_json)?;
            normalize_metadata(None, &mut tool_state, ObjectCompression::none(), &mut refs)?;
        }
        hashes.extend(refs.into_iter().map(|(hash, _)| hash));
    }
    Ok(hashes.into_iter().collect())
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
    let detached_search = conn.execute(
        "UPDATE transcript_search SET history_idx = NULL WHERE history_idx >= ?1",
        [start_idx_sql],
    )?;
    let detached_blocks = conn.execute(
        "UPDATE transcript_blocks
         SET history_idx = NULL, origin_json = NULL
         WHERE history_idx >= ?1",
        [start_idx_sql],
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
    if history_deleted.saturating_sub(items.len()) >= LARGE_REWIND_GC_MIN_ROWS {
        object::delete_unreachable_objects(conn)?;
    }
    perf::record_value("store:history:db_rows_deleted", history_deleted as u64);
    perf::record_value(
        "store:history:transcript_rows_detached",
        detached_search.saturating_add(detached_blocks) as u64,
    );
    perf::record_value("store:history:db_rows_inserted", items.len() as u64);
    Ok(())
}

pub(crate) fn read_history_items(conn: &Connection) -> Result<Vec<HistoryItem>> {
    let _perf = perf::begin("store:history:read_all");
    let mut stmt = conn.prepare("SELECT idx, kind, json, hash FROM history_items ORDER BY idx")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    let mut json_bytes = 0u64;
    for row in rows {
        let (idx, kind, json, hash) = row?;
        json_bytes = json_bytes.saturating_add(json.len() as u64);
        out.push(decode_history_row(conn, idx, &kind, &json, &hash)?);
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
    let mut stmt = conn.prepare(
        "SELECT idx, kind, json, hash
         FROM history_items
         WHERE idx >= ?1 AND idx < ?2
         ORDER BY idx",
    )?;
    let rows = stmt.query_map(params![start, end], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    let mut json_bytes = 0u64;
    for row in rows {
        let (idx, kind, json, hash) = row?;
        json_bytes = json_bytes.saturating_add(json.len() as u64);
        out.push(decode_history_row(conn, idx, &kind, &json, &hash)?);
    }
    perf::record_value("store:history:rows_read", out.len() as u64);
    perf::record_value("store:history:read_range_rows", out.len() as u64);
    perf::record_value("store:history:json_bytes_read", json_bytes);
    Ok(out)
}

fn decode_history_row(
    conn: &Connection,
    idx: i64,
    stored_kind: &str,
    json: &str,
    stored_hash: &str,
) -> Result<HistoryItem> {
    let actual_hash = sha256_hex(json.as_bytes());
    if actual_hash != stored_hash {
        return Err(StoreError::Integrity(format!(
            "history row {idx} hash mismatch: stored {stored_hash}, actual {actual_hash}"
        )));
    }
    let mut value: Value = serde_json::from_str(json)?;
    let actual_kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if actual_kind != stored_kind {
        return Err(StoreError::Integrity(format!(
            "history row {idx} kind mismatch: stored {stored_kind:?}, decoded {actual_kind:?}"
        )));
    }
    rehydrate_object_refs(conn, &mut value)?;
    serde_json::from_value(value).map_err(Into::into)
}

pub(crate) fn legacy_attachment_references(
    conn: &Connection,
    history_end: usize,
) -> Result<Vec<String>> {
    let history_end = checked_i64(history_end as u64, "history_end")?;
    let mut stmt = conn.prepare("SELECT json FROM history_items WHERE idx < ?1 ORDER BY idx")?;
    let rows = stmt.query_map([history_end], |row| row.get::<_, String>(0))?;
    let mut references = std::collections::BTreeSet::new();
    for row in rows {
        let value: Value = serde_json::from_str(&row?)?;
        collect_legacy_attachment_references(&value, &mut references);
    }
    Ok(references.into_iter().collect())
}

fn collect_legacy_attachment_references(
    value: &Value,
    references: &mut std::collections::BTreeSet<String>,
) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image_url") {
                if let Some(reference) = map
                    .get("image_url")
                    .and_then(Value::as_object)
                    .and_then(|image| image.get("url"))
                    .and_then(Value::as_str)
                    .filter(|reference| reference.starts_with("blob:"))
                {
                    references.insert(reference.to_string());
                }
            }
            for child in map.values() {
                collect_legacy_attachment_references(child, references);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_legacy_attachment_references(child, references);
            }
        }
        _ => {}
    }
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
    let mut stmt = conn.prepare(
        "SELECT indexed_text FROM transcript_search WHERE indexed_text != '' ORDER BY block_idx",
    )?;
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

pub(crate) fn transcript_descriptor_suffix_matches(
    conn: &Connection,
    start_descriptor_idx: usize,
    records: &[TranscriptDescriptorRecord],
    compression: ObjectCompression,
) -> Result<bool> {
    let current_count = transcript_descriptor_count(conn)?;
    let expected_count = start_descriptor_idx
        .checked_add(records.len())
        .ok_or_else(|| StoreError::Integrity("descriptor suffix length overflows usize".into()))?;
    if current_count != expected_count {
        return Ok(false);
    }
    let limit = checked_i64(records.len() as u64, "descriptor_suffix_len")?;
    let offset = checked_i64(start_descriptor_idx as u64, "descriptor_suffix_start")?;
    let mut stmt = conn.prepare(
        "SELECT b.block_idx, b.history_idx, b.kind, b.tool_call_id, b.tool_name, b.content_hash,
                b.estimated_text_bytes, b.preview_text, COALESCE(s.indexed_text, '') AS indexed_text,
                b.descriptor_json, b.origin_json, b.tool_state_json
         FROM transcript_blocks b
         LEFT JOIN transcript_search s ON s.block_idx = b.block_idx
         WHERE b.descriptor_json IS NOT NULL
         ORDER BY b.descriptor_idx
         LIMIT ?1 OFFSET ?2",
    )?;
    let current = read_transcript_descriptor_records_from_stmt(
        conn,
        &mut stmt,
        params![limit, offset],
        TranscriptDescriptorHydration::Hydrated,
    )?;
    let mut expected = records.to_vec();
    for record in &mut expected {
        let mut descriptor: Value = serde_json::from_str(&record.descriptor_json)?;
        normalize_metadata(None, &mut descriptor, compression, &mut Vec::new())?;
        record.descriptor_json = serde_json::to_string(&descriptor)?;
        if let Some(tool_state_json) = &mut record.tool_state_json {
            let mut tool_state: Value = serde_json::from_str(tool_state_json)?;
            normalize_metadata(None, &mut tool_state, compression, &mut Vec::new())?;
            *tool_state_json = serde_json::to_string(&tool_state)?;
        }
    }
    Ok(current == expected)
}

pub(crate) fn replace_transcript_descriptor_suffix_in_transaction(
    conn: &Connection,
    start_descriptor_idx: usize,
    records: &[TranscriptDescriptorRecord],
    compression: ObjectCompression,
) -> Result<()> {
    let _perf = perf::begin("store:transcript:replace_descriptor_suffix");
    compact_transcript_descriptor_indices(conn)?;
    let current_descriptor_count = transcript_descriptor_count(conn)?;
    if start_descriptor_idx > current_descriptor_count {
        return Err(StoreError::Integrity(format!(
            "transcript descriptor suffix starts past dense end: start {start_descriptor_idx}, count {current_descriptor_count}",
        )));
    }
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
                    params![
                        checked_i64(history_idx, "history_idx")?,
                        hash,
                        role.as_str()
                    ],
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

#[derive(Clone, Copy, Debug)]
struct TranscriptDescriptorIndexStats {
    count: i64,
    indexed_count: i64,
    distinct_index_count: i64,
    min_descriptor_idx: i64,
    max_descriptor_idx: i64,
}

impl TranscriptDescriptorIndexStats {
    fn is_dense(self) -> bool {
        self.count == self.indexed_count
            && self.count == self.distinct_index_count
            && self.min_descriptor_idx == 0
            && self.max_descriptor_idx == self.count.saturating_sub(1)
    }
}

fn transcript_descriptor_index_stats(conn: &Connection) -> Result<TranscriptDescriptorIndexStats> {
    conn.query_row(
        "SELECT COUNT(*), COUNT(descriptor_idx), COUNT(DISTINCT descriptor_idx),
                COALESCE(MIN(descriptor_idx), 0), COALESCE(MAX(descriptor_idx), -1)
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL",
        [],
        |row| {
            Ok(TranscriptDescriptorIndexStats {
                count: row.get::<_, i64>(0)?,
                indexed_count: row.get::<_, i64>(1)?,
                distinct_index_count: row.get::<_, i64>(2)?,
                min_descriptor_idx: row.get::<_, i64>(3)?,
                max_descriptor_idx: row.get::<_, i64>(4)?,
            })
        },
    )
    .map_err(Into::into)
}

fn compact_transcript_descriptor_indices(conn: &Connection) -> Result<bool> {
    let stats = transcript_descriptor_index_stats(conn)?;
    if stats.is_dense() {
        return Ok(false);
    }

    let mut stmt = conn.prepare(
        "SELECT block_idx, descriptor_idx
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL
         ORDER BY descriptor_idx",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    let rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;

    conn.execute(
        "UPDATE transcript_blocks SET descriptor_idx = NULL
         WHERE descriptor_json IS NOT NULL",
        [],
    )?;
    for (idx, (block_idx, _)) in rows.iter().enumerate() {
        conn.execute(
            "UPDATE transcript_blocks SET descriptor_idx = ?1 WHERE block_idx = ?2",
            params![idx as i64, block_idx],
        )?;
    }
    Ok(true)
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
            preview_text
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
        ],
    )?;
    insert_transcript_search(conn, block_idx, history_idx, &record.indexed_text)?;
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
        "SELECT b.block_idx, b.history_idx, b.kind, b.tool_call_id, b.tool_name, b.content_hash,
                b.estimated_text_bytes, b.preview_text, COALESCE(s.indexed_text, '') AS indexed_text,
                b.descriptor_json, b.origin_json, b.tool_state_json
         FROM transcript_blocks b
         LEFT JOIN transcript_search s ON s.block_idx = b.block_idx
         WHERE b.descriptor_json IS NOT NULL
         ORDER BY b.descriptor_idx",
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
                estimated_text_bytes, preview_text, '' AS indexed_text,
                descriptor_json, origin_json, tool_state_json
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
                estimated_text_bytes, preview_text, '' AS indexed_text,
                descriptor_json, origin_json, tool_state_json
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
                estimated_text_bytes, preview_text, '' AS indexed_text,
                descriptor_json, origin_json, tool_state_json
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
                estimated_text_bytes, preview_text, '' AS indexed_text,
                descriptor_json, origin_json, tool_state_json
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
            indexed_text: row.get(8)?,
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
    let use_fts = query.chars().count() >= 3;
    perf::record_value("store:transcript:search_fts", u64::from(use_fts));

    loop {
        let batch = search_transcript_candidate_batch(
            conn,
            TranscriptCandidateBatchQuery {
                query,
                bound,
                inclusive,
                direction,
                page_size,
                use_fts,
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

struct TranscriptCandidateBatchQuery<'a> {
    query: &'a str,
    bound: Option<u64>,
    inclusive: bool,
    direction: TranscriptSearchDirection,
    page_size: usize,
    use_fts: bool,
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
        (Some(_), true, TranscriptSearchDirection::Forward) => "AND s.block_idx >= ?",
        (Some(_), false, TranscriptSearchDirection::Forward) => "AND s.block_idx > ?",
        (Some(_), true, TranscriptSearchDirection::Backward) => "AND s.block_idx <= ?",
        (Some(_), false, TranscriptSearchDirection::Backward) => "AND s.block_idx < ?",
        (None, _, _) => "",
    };
    let sql = if query.use_fts {
        format!(
            "SELECT s.block_idx, s.history_idx
             FROM transcript_search_fts f
             JOIN transcript_search s ON s.block_idx = f.rowid
             WHERE f.indexed_text MATCH ?
               AND instr(s.indexed_text, ?) > 0 {bound_filter}
             ORDER BY s.block_idx {order}
             LIMIT ?"
        )
    } else {
        format!(
            "SELECT s.block_idx, s.history_idx
             FROM transcript_search s
             WHERE instr(s.indexed_text, ?) > 0 {bound_filter}
             ORDER BY s.block_idx {order}
             LIMIT ?"
        )
    };
    let mut values = Vec::with_capacity(3 + usize::from(query.bound.is_some()));
    if query.use_fts {
        values.push(SqlValue::from(fts5_phrase_query(query.query)));
    }
    values.push(SqlValue::from(query.query.to_string()));
    if let Some(bound) = query.bound {
        values.push(SqlValue::from(checked_i64(
            bound,
            "search_candidate_bound",
        )?));
    }
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

pub(crate) fn fts5_phrase_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    out.push('"');
    for ch in query.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

fn insert_transcript_search(
    conn: &Connection,
    block_idx: i64,
    history_idx: Option<i64>,
    indexed_text: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO transcript_search (block_idx, history_idx, indexed_text)
         VALUES (?1, ?2, ?3)",
        params![block_idx, history_idx, indexed_text],
    )?;
    Ok(())
}

struct NormalizedHistoryItem {
    value: Value,
    json: String,
    hash: String,
    kind: String,
    search_text: String,
    refs: Vec<(String, HistoryObjectRole)>,
}

fn normalized_history_value(
    item: &HistoryItem,
    compression: ObjectCompression,
    conn: Option<&Connection>,
) -> Result<NormalizedHistoryItem> {
    let mut value = serde_json::to_value(item)?;
    let mut refs = Vec::new();
    normalize_attachments(conn, &mut value, compression, &mut refs)?;
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
            params![idx, hash, role.as_str()],
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

fn normalize_attachments(
    conn: Option<&Connection>,
    value: &mut Value,
    compression: ObjectCompression,
    refs: &mut Vec<(String, HistoryObjectRole)>,
) -> Result<()> {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image_url") {
                let url = map
                    .get_mut("image_url")
                    .and_then(Value::as_object_mut)
                    .and_then(|image| image.get_mut("url"));
                if let Some(url @ Value::String(_)) = url {
                    let data_url = url.as_str().expect("matched image URL string");
                    if data_url.starts_with("data:image/") {
                        let bytes = data_url.as_bytes();
                        let hash = if let Some(conn) = conn {
                            object::put_object(conn, bytes, compression)?
                                .hash()
                                .to_string()
                        } else {
                            sha256_hex(bytes)
                        };
                        refs.push((hash.clone(), HistoryObjectRole::AttachmentImage));
                        *url = object_ref_json(&hash, bytes.len() as u64);
                        return Ok(());
                    }
                }
            }
            for child in map.values_mut() {
                normalize_attachments(conn, child, compression, refs)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                normalize_attachments(conn, child, compression, refs)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn normalize_metadata(
    conn: Option<&Connection>,
    value: &mut Value,
    compression: ObjectCompression,
    refs: &mut Vec<(String, HistoryObjectRole)>,
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
                            object::put_object(conn, &bytes, compression)?
                                .hash()
                                .to_string()
                        } else {
                            sha256_hex(&bytes)
                        };
                        refs.push((hash.clone(), HistoryObjectRole::Metadata));
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
            estimated_text_bytes, preview_text
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            block_idx,
            history_idx,
            kind,
            tool_call_id,
            tool_name,
            content_hash,
            checked_i64(search_text.len() as u64, "estimated_text_bytes")?,
            preview
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
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                collect_text_inner(&map[key], out, max_bytes);
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
            if let Some(reference) = map.get(OBJECT_REF_KEY) {
                let hash = reference
                    .get("hash")
                    .and_then(Value::as_str)
                    .ok_or_else(|| StoreError::Integrity("object reference has no hash".into()))?;
                let declared_size = reference
                    .get("raw_size")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        StoreError::Integrity("object reference has invalid raw_size".into())
                    })?;
                if declared_size > object::MAX_OBJECT_RAW_SIZE {
                    return Err(StoreError::ObjectTooLarge {
                        size: declared_size,
                        max: object::MAX_OBJECT_RAW_SIZE,
                    });
                }
                let bytes = object::object_bytes_by_hash(conn, hash)?.ok_or_else(|| {
                    StoreError::MissingObject {
                        reference: hash.to_string(),
                    }
                })?;
                if bytes.len() as u64 != declared_size {
                    return Err(StoreError::Integrity(format!(
                        "object reference {hash} declares {declared_size} bytes but contains {}",
                        bytes.len()
                    )));
                }
                *value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                    Value::String(String::from_utf8_lossy(&bytes).into_owned())
                });
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
    fn history_reads_verify_hash_kind_and_required_objects() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let item = HistoryItem::user(protocol::Content::with_images(
            "attached".into(),
            vec![("attachment.png".into(), "data:image/png;base64,AAAA".into())],
        ));
        replace_history_suffix(
            &conn,
            0,
            std::slice::from_ref(&item),
            ObjectCompression::none(),
        )
        .unwrap();

        conn.execute("UPDATE history_items SET hash = 'bad' WHERE idx = 0", [])
            .unwrap();
        assert!(matches!(
            read_history_items_range(&conn, 0..1),
            Err(StoreError::Integrity(message)) if message.contains("hash mismatch")
        ));
        let normalized = normalized_history_value(&item, ObjectCompression::none(), None).unwrap();
        conn.execute(
            "UPDATE history_items SET hash = ?1, kind = 'wrong' WHERE idx = 0",
            [normalized.hash],
        )
        .unwrap();
        assert!(matches!(
            read_history_items_range(&conn, 0..1),
            Err(StoreError::Integrity(message)) if message.contains("kind mismatch")
        ));
        conn.execute(
            "UPDATE history_items SET kind = ?1 WHERE idx = 0",
            [normalized.kind],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF; DELETE FROM objects;")
            .unwrap();
        assert!(matches!(
            read_history_items_range(&conn, 0..1),
            Err(StoreError::MissingObject { .. })
        ));
    }

    #[test]
    fn transcript_search_candidate_plan_uses_fts_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let details = query_plan_details(
            &conn,
            "SELECT s.block_idx, s.history_idx
             FROM transcript_search_fts f
             JOIN transcript_search s ON s.block_idx = f.rowid
             WHERE f.indexed_text MATCH ?1
               AND instr(s.indexed_text, ?2) > 0
             ORDER BY s.block_idx ASC
             LIMIT ?3",
            rusqlite::params!["\"abcdef\"", "abcdef", 64_i64],
        );

        assert!(
            details
                .iter()
                .any(|detail| detail.contains("SCAN f VIRTUAL TABLE")),
            "{details:#?}"
        );
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("SEARCH s USING INTEGER PRIMARY KEY")),
            "{details:#?}"
        );
    }

    #[test]
    fn fts5_phrase_query_quotes_literal_search_text() {
        assert_eq!(fts5_phrase_query("foo_bar%"), "\"foo_bar%\"");
        assert_eq!(fts5_phrase_query("say \"hi\""), "\"say \"\"hi\"\"\"");
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
