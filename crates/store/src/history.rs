use protocol::HistoryItem;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Statement};
use serde_json::{json, Value};
use smelt_perf::perf;
use std::io::{Read, Write};
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::object::{self, checked_i64, sha256_hex};
use rusqlite::types::Value as SqlValue;

pub(crate) const METADATA_OBJECT_MIN_BYTES: usize = 4 * 1024;
pub(crate) const LARGE_REWIND_GC_MIN_ROWS: usize = 128;
pub(crate) const OBJECT_REF_KEY: &str = "$smelt_object_ref";
const HISTORY_INDEX_READ_BATCH_SIZE: usize = 500;

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
pub struct TranscriptRecordOffset(usize);

impl TranscriptRecordOffset {
    pub fn new(index: usize) -> Self {
        Self(index)
    }

    pub fn get(self) -> usize {
        self.0
    }
}

impl From<usize> for TranscriptRecordOffset {
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TranscriptRecordRange {
    start: TranscriptRecordOffset,
    end: TranscriptRecordOffset,
}

impl TranscriptRecordRange {
    pub fn new(start: TranscriptRecordOffset, end: TranscriptRecordOffset) -> Self {
        Self { start, end }
    }

    pub fn start(self) -> TranscriptRecordOffset {
        self.start
    }

    pub fn end(self) -> TranscriptRecordOffset {
        self.end
    }
}

impl From<Range<usize>> for TranscriptRecordRange {
    fn from(value: Range<usize>) -> Self {
        Self::new(value.start.into(), value.end.into())
    }
}

pub const TRANSCRIPT_EXTENT_PROFILE_WIDTHS: [u16; 6] = [20, 40, 80, 120, 160, 240];
pub const TRANSCRIPT_EXTENT_CHUNK_RECORDS: usize = 64;
const TRANSCRIPT_EXTENT_PROFILE_VERSION: i64 = 1;
const TRANSCRIPT_EXTENT_BACKFILL_BATCH_RECORDS: usize = 256;
const TRANSCRIPT_EXTENT_BACKFILL_BATCH_SQL: &str =
    "SELECT b.block_idx, b.record_idx, b.kind, b.estimated_text_bytes,
            COALESCE(b.preview_text, ''),
            COALESCE((SELECT s.indexed_text
                      FROM transcript_search s
                      WHERE s.block_idx = b.block_idx), '')
     FROM transcript_blocks b
     WHERE b.block_json IS NOT NULL
       AND b.extent_profile_version != ?1
       AND b.record_idx > ?2
     ORDER BY b.record_idx
     LIMIT ?3";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TranscriptExtentProfile {
    rows: [u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()],
}

impl TranscriptExtentProfile {
    pub fn new(rows: [u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()]) -> Self {
        let mut rows = rows;
        for index in 1..rows.len() {
            rows[index] = rows[index].min(rows[index - 1]);
        }
        Self { rows }
    }

    pub fn rows(self) -> [u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()] {
        self.rows
    }

    pub fn estimated_rows(self, width: u16) -> u64 {
        let width = width.max(1);
        let first_width = TRANSCRIPT_EXTENT_PROFILE_WIDTHS[0];
        if width < first_width {
            let slope = self.rows[0].saturating_sub(self.rows[1]);
            let extra = slope
                .saturating_mul(u64::from(first_width - width))
                .div_ceil(u64::from(TRANSCRIPT_EXTENT_PROFILE_WIDTHS[1] - first_width));
            return self.rows[0].saturating_add(extra);
        }

        for index in 0..TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len() - 1 {
            let lower_width = TRANSCRIPT_EXTENT_PROFILE_WIDTHS[index];
            let upper_width = TRANSCRIPT_EXTENT_PROFILE_WIDTHS[index + 1];
            if width <= upper_width {
                let lower_rows = self.rows[index];
                let upper_rows = self.rows[index + 1];
                let row_drop = lower_rows.saturating_sub(upper_rows);
                let width_offset = u64::from(width - lower_width);
                let width_span = u64::from(upper_width - lower_width);
                let interpolated_drop = row_drop
                    .saturating_mul(width_offset)
                    .saturating_add(width_span / 2)
                    / width_span;
                return lower_rows.saturating_sub(interpolated_drop);
            }
        }

        self.rows.last().copied().unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranscriptExtentChunk {
    pub start: TranscriptRecordOffset,
    pub record_count: usize,
    pub profile: TranscriptExtentProfile,
}

impl TranscriptExtentChunk {
    pub fn end(self) -> TranscriptRecordOffset {
        TranscriptRecordOffset::new(self.start.get().saturating_add(self.record_count))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StoredTranscriptBlock {
    pub block_idx: u64,
    pub history_idx: Option<u64>,
    pub kind: String,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub content_hash: String,
    pub estimated_text_bytes: u64,
    pub preview_text: String,
    pub indexed_text: String,
    pub block_json: String,
    pub origin_json: Option<String>,
    pub tool_state_json: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptBlockMetadataRecord {
    pub block_idx: u64,
    pub record_idx: Option<u64>,
    pub history_idx: Option<u64>,
    pub kind: String,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub content_hash: Option<String>,
    pub estimated_text_bytes: u64,
    pub estimated_rows: Option<u64>,
    pub preview_text: String,
    pub has_block: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptRecordHydration {
    Hydrated,
    ObjectBacked,
}

impl TranscriptRecordHydration {
    fn hydrates_objects(self) -> bool {
        matches!(self, Self::Hydrated)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptRecordSlice {
    pub start: TranscriptRecordOffset,
    pub total_count: usize,
    pub hydration: TranscriptRecordHydration,
    pub records: Vec<StoredTranscriptBlock>,
}

impl TranscriptRecordSlice {
    pub fn new(
        start: TranscriptRecordOffset,
        total_count: usize,
        hydration: TranscriptRecordHydration,
        records: Vec<StoredTranscriptBlock>,
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

    pub fn end(&self) -> TranscriptRecordOffset {
        TranscriptRecordOffset::new(self.start.get().saturating_add(self.records.len()))
    }

    pub fn into_records(self) -> Vec<StoredTranscriptBlock> {
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

#[cfg(debug_assertions)]
pub(crate) fn incoming_object_hashes(
    items: &[HistoryItem],
    records: Option<&[StoredTranscriptBlock]>,
) -> Result<Vec<String>> {
    let mut hashes = std::collections::BTreeSet::new();
    for item in items {
        let normalized = normalized_history_value(item, ObjectCompression::none(), None)?;
        hashes.extend(normalized.refs.into_iter().map(|(hash, _)| hash));
    }
    for record in records.into_iter().flatten() {
        let mut block: Value = serde_json::from_str(&record.block_json)?;
        let mut refs = Vec::new();
        normalize_metadata(None, &mut block, ObjectCompression::none(), &mut refs)?;
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

pub(crate) fn read_history_items_at_indices(
    conn: &Connection,
    indices: &[u64],
) -> Result<std::collections::HashMap<u64, HistoryItem>> {
    let _perf = perf::begin("store:history:read_indices");
    let mut out = std::collections::HashMap::with_capacity(indices.len());
    let mut json_bytes = 0u64;
    for batch in indices.chunks(HISTORY_INDEX_READ_BATCH_SIZE) {
        let placeholders = vec!["?"; batch.len()].join(",");
        let sql = format!(
            "SELECT idx, kind, json, hash FROM history_items WHERE idx IN ({placeholders})"
        );
        let sql_indices = batch
            .iter()
            .map(|idx| checked_i64(*idx, "history_idx"))
            .collect::<Result<Vec<_>>>()?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(sql_indices), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (idx, kind, json, hash) = row?;
            let key = u64::try_from(idx)
                .map_err(|_| StoreError::Integrity(format!("negative history item index {idx}")))?;
            json_bytes = json_bytes.saturating_add(json.len() as u64);
            out.insert(key, decode_history_row(conn, idx, &kind, &json, &hash)?);
        }
    }
    perf::record_value("store:history:rows_read", out.len() as u64);
    perf::record_value("store:history:read_indices_rows", out.len() as u64);
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

pub(crate) fn read_history_items_tail(
    conn: &Connection,
    end: usize,
    max_items: usize,
    max_bytes: Option<usize>,
) -> Result<Vec<HistoryItem>> {
    let _perf = perf::begin("store:history:read_tail");
    if end == 0 || max_items == 0 || max_bytes == Some(0) {
        record_history_tail_read(0, 0, 0);
        return Ok(Vec::new());
    }

    let end = checked_i64(end as u64, "end_idx")?;
    let limit = checked_i64(max_items as u64, "max_items")?;
    let mut stmt = conn.prepare(
        "SELECT idx, kind, json, hash
         FROM history_items
         WHERE idx < ?1
         ORDER BY idx DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![end, limit], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::with_capacity(max_items.min(end as usize));
    let mut budget = protocol::HistoryTailBudget::new(max_items, max_bytes);
    let mut stored_json_bytes = 0u64;
    let mut rows_considered = 0usize;
    for row in rows {
        let (idx, kind, json, hash) = row?;
        rows_considered = rows_considered.saturating_add(1);
        stored_json_bytes = stored_json_bytes.saturating_add(json.len() as u64);
        let mut value = validate_history_row(idx, &kind, &json, &hash)?;
        if !budget.can_prepend_bytes(history_object_bytes(&value)) {
            break;
        }
        rehydrate_object_refs(conn, &mut value)?;
        let item: HistoryItem = serde_json::from_value(value)?;
        if !budget.try_prepend(&item)? {
            break;
        }
        out.push(item);
    }
    out.reverse();
    record_history_tail_read(rows_considered, out.len(), stored_json_bytes);
    Ok(out)
}

fn record_history_tail_read(rows_considered: usize, rows_returned: usize, json_bytes: u64) {
    perf::record_value("store:history:rows_read", rows_considered as u64);
    perf::record_value(
        "store:history:read_tail_rows_considered",
        rows_considered as u64,
    );
    perf::record_value(
        "store:history:read_tail_rows_returned",
        rows_returned as u64,
    );
    perf::record_value("store:history:json_bytes_read", json_bytes);
}

pub(crate) fn history_object_bytes(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get(OBJECT_REF_KEY) {
                return reference
                    .get("raw_size")
                    .and_then(Value::as_u64)
                    .and_then(|size| usize::try_from(size).ok())
                    .unwrap_or(0);
            }
            map.values().fold(0usize, |total, child| {
                total.saturating_add(history_object_bytes(child))
            })
        }
        Value::Array(values) => values.iter().fold(0usize, |total, child| {
            total.saturating_add(history_object_bytes(child))
        }),
        _ => 0,
    }
}

fn validate_history_row(
    idx: i64,
    stored_kind: &str,
    json: &str,
    stored_hash: &str,
) -> Result<Value> {
    let actual_hash = sha256_hex(json.as_bytes());
    if actual_hash != stored_hash {
        return Err(StoreError::Integrity(format!(
            "history row {idx} hash mismatch: stored {stored_hash}, actual {actual_hash}"
        )));
    }
    let value: Value = serde_json::from_str(json)?;
    let actual_kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if actual_kind != stored_kind {
        return Err(StoreError::Integrity(format!(
            "history row {idx} kind mismatch: stored {stored_kind:?}, decoded {actual_kind:?}"
        )));
    }
    Ok(value)
}

fn decode_history_row(
    conn: &Connection,
    idx: i64,
    stored_kind: &str,
    json: &str,
    stored_hash: &str,
) -> Result<HistoryItem> {
    let mut value = validate_history_row(idx, stored_kind, json, stored_hash)?;
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

pub(crate) fn history_any_transcript_visible_before(conn: &Connection, end: usize) -> Result<bool> {
    let _perf = perf::begin("store:history:any_visible_before");
    let end = checked_i64(end as u64, "end_idx")?;
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM history_items INDEXED BY history_items_kind_idx
             WHERE kind IN ('user', 'assistant') AND idx < ?1
             UNION ALL
             SELECT 1
             FROM history_items INDEXED BY history_items_kind_idx
             WHERE kind = 'note'
               AND idx < ?1
               AND COALESCE(json_extract(json, '$.note_kind'), '') != 'context'
         )",
        [end],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

pub(crate) fn history_note_projection_at(
    conn: &Connection,
    index: usize,
) -> Result<Option<protocol::HistoryNoteProjection>> {
    let _perf = perf::begin("store:history:note_projection_at");
    let index = checked_i64(index as u64, "history_idx")?;
    let fields = conn
        .query_row(
            "SELECT json_extract(json, '$.note_kind'),
                    json_type(json, '$.mode'),
                    CASE WHEN json_type(json, '$.mode') = 'text'
                         THEN json_extract(json, '$.mode')
                    END
             FROM history_items
             WHERE idx = ?1 AND kind = 'note'",
            [index],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, mode_type, mode)) = fields else {
        return Ok(None);
    };
    let kind = match kind.as_deref() {
        Some("mode_change") => protocol::HistoryNoteKind::ModeChange,
        Some("context") => protocol::HistoryNoteKind::Context,
        Some("process_status") => protocol::HistoryNoteKind::ProcessStatus,
        other => {
            return Err(StoreError::Integrity(format!(
                "history note {index} has invalid note kind {other:?}"
            )))
        }
    };
    if mode_type.as_deref().is_some_and(|kind| kind != "text") {
        return Err(StoreError::Integrity(format!(
            "history note {index} has non-text mode"
        )));
    }
    Ok(Some(protocol::HistoryNoteProjection { kind, mode }))
}

pub(crate) fn history_last_context_note_index_before(
    conn: &Connection,
    end: usize,
    name: &str,
) -> Result<Option<usize>> {
    let _perf = perf::begin("store:history:last_context_note_index_before");
    let end = checked_i64(end as u64, "end_idx")?;
    let index = conn
        .query_row(
            "SELECT idx
             FROM history_items
             WHERE idx < ?1
               AND kind = 'note'
               AND json_extract(json, '$.note_kind') = 'context'
               AND COALESCE(
                   json_extract(json, '$.name'),
                   ?2
               ) = ?3
             ORDER BY idx DESC
             LIMIT 1",
            params![end, protocol::DEFAULT_CONTEXT_NOTE_NAME, name],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    index
        .map(|index| {
            usize::try_from(index).map_err(|_| {
                StoreError::Integrity(format!("negative history context index {index}"))
            })
        })
        .transpose()
}

pub(crate) fn history_mode_before(conn: &Connection, end: usize) -> Result<Option<String>> {
    let _perf = perf::begin("store:history:mode_before");
    let end = checked_i64(end as u64, "end_idx")?;
    conn.query_row(
        "SELECT json_extract(json, '$.mode')
         FROM history_items
         WHERE idx < ?1
           AND kind = 'note'
           AND json_type(json, '$.mode') = 'text'
         ORDER BY idx DESC
         LIMIT 1",
        [end],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn history_base_mode_range(
    conn: &Connection,
    range: Range<usize>,
) -> Result<Option<String>> {
    let _perf = perf::begin("store:history:base_mode_range");
    if range.end <= range.start {
        return Ok(None);
    }
    let start = checked_i64(range.start as u64, "start_idx")?;
    let end = checked_i64(range.end as u64, "end_idx")?;
    conn.query_row(
        "SELECT json_extract(json, '$.base_mode')
         FROM history_items
         WHERE idx >= ?1
           AND idx < ?2
           AND kind = 'note'
           AND json_type(json, '$.base_mode') = 'text'
         ORDER BY idx
         LIMIT 1",
        params![start, end],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
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

pub(crate) fn transcript_missing_block_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM transcript_blocks WHERE block_json IS NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

pub(crate) fn transcript_record_max_history_idx(conn: &Connection) -> Result<Option<usize>> {
    let idx: Option<i64> = conn.query_row(
        "SELECT MAX(history_idx) FROM transcript_blocks WHERE block_json IS NOT NULL",
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
        "SELECT block_idx, record_idx, history_idx, kind, tool_call_id, tool_name,
                content_hash, estimated_text_bytes, estimated_rows, preview_text,
                block_json IS NOT NULL AS has_block
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
        "SELECT block_idx, record_idx, history_idx, kind, tool_call_id, tool_name,
                content_hash, estimated_text_bytes, estimated_rows, preview_text,
                block_json IS NOT NULL AS has_block
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
        record_idx: row.get::<_, Option<i64>>(1)?.map(|idx| idx as u64),
        history_idx: row.get::<_, Option<i64>>(2)?.map(|idx| idx as u64),
        kind: row.get(3)?,
        tool_call_id: row.get(4)?,
        tool_name: row.get(5)?,
        content_hash: row.get(6)?,
        estimated_text_bytes: row.get::<_, i64>(7)? as u64,
        estimated_rows: row.get::<_, Option<i64>>(8)?.map(|rows| rows as u64),
        preview_text: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        has_block: row.get::<_, i64>(10)? != 0,
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
    let mut out = Vec::new();
    write_search_blob_rows(conn, &mut out)?;
    Ok(String::from_utf8(out).expect("transcript search text is valid UTF-8"))
}

pub(crate) fn write_search_blob(conn: &Connection, writer: &mut impl Write) -> Result<()> {
    let _perf = perf::begin("store:transcript:search_blob_full");
    write_search_blob_rows(conn, writer)
}

fn write_search_blob_rows(conn: &Connection, writer: &mut impl Write) -> Result<()> {
    const WRITE_CHUNK_BYTES: usize = 64 * 1024;

    let mut stmt = conn.prepare(
        "SELECT block_idx FROM transcript_search WHERE indexed_text != '' ORDER BY block_idx",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    let mut row_count = 0u64;
    let mut byte_count = 0u64;
    let mut chunk = [0_u8; WRITE_CHUNK_BYTES];
    for row in rows {
        let block_idx = row?;
        let mut text = conn.blob_open(
            rusqlite::MAIN_DB,
            "transcript_search",
            "indexed_text",
            block_idx,
            true,
        )?;
        row_count = row_count.saturating_add(1);
        let mut last_byte = None;
        loop {
            let read = text.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            writer.write_all(&chunk[..read])?;
            byte_count = byte_count.saturating_add(read as u64);
            last_byte = Some(chunk[read - 1]);
        }
        if last_byte != Some(b'\n') {
            writer.write_all(b"\n")?;
            byte_count = byte_count.saturating_add(1);
        }
    }
    perf::record_value("store:transcript:search_blob_rows_read", row_count);
    perf::record_value("store:transcript:search_blob_bytes_read", byte_count);
    Ok(())
}

pub(crate) fn transcript_record_suffix_matches(
    conn: &Connection,
    start_record_idx: usize,
    records: &[StoredTranscriptBlock],
    compression: ObjectCompression,
) -> Result<bool> {
    let current_count = transcript_record_count(conn)?;
    let expected_count = start_record_idx
        .checked_add(records.len())
        .ok_or_else(|| StoreError::Integrity("block suffix length overflows usize".into()))?;
    if current_count != expected_count {
        return Ok(false);
    }
    let limit = checked_i64(records.len() as u64, "block_suffix_len")?;
    let offset = checked_i64(start_record_idx as u64, "block_suffix_start")?;
    let mut stmt = conn.prepare(
        "SELECT b.block_idx, b.history_idx, b.kind, b.tool_call_id, b.tool_name, b.content_hash,
                b.estimated_text_bytes, b.preview_text, COALESCE(s.indexed_text, '') AS indexed_text,
                b.block_json, b.origin_json, b.tool_state_json
         FROM transcript_blocks b
         LEFT JOIN transcript_search s ON s.block_idx = b.block_idx
         WHERE b.block_json IS NOT NULL
         ORDER BY b.record_idx
         LIMIT ?1 OFFSET ?2",
    )?;
    let current = read_transcript_records_from_stmt(
        conn,
        &mut stmt,
        params![limit, offset],
        TranscriptRecordHydration::Hydrated,
    )?;
    let mut expected = records.to_vec();
    for record in &mut expected {
        let mut block: Value = serde_json::from_str(&record.block_json)?;
        normalize_metadata(None, &mut block, compression, &mut Vec::new())?;
        record.block_json = serde_json::to_string(&block)?;
        if let Some(tool_state_json) = &mut record.tool_state_json {
            let mut tool_state: Value = serde_json::from_str(tool_state_json)?;
            normalize_metadata(None, &mut tool_state, compression, &mut Vec::new())?;
            *tool_state_json = serde_json::to_string(&tool_state)?;
        }
    }
    Ok(current == expected)
}

pub(crate) fn replace_transcript_record_suffix_in_transaction(
    conn: &Connection,
    start_record_idx: usize,
    records: &[StoredTranscriptBlock],
    compression: ObjectCompression,
) -> Result<()> {
    let _perf = perf::begin("store:transcript:replace_block_suffix");
    let current_block_count = transcript_record_count(conn)?;
    let compacted = transcript_record_dense_extent(conn)? != current_block_count;
    if compacted {
        compact_transcript_record_indices(conn)?;
    }
    if start_record_idx > current_block_count {
        return Err(StoreError::Integrity(format!(
            "transcript block suffix starts past dense end: start {start_record_idx}, count {current_block_count}",
        )));
    }
    perf::record_value(
        "store:transcript:dirty_block_suffix_rows",
        records.len() as u64,
    );
    let start_record_idx = checked_i64(start_record_idx as u64, "start_record_idx")?;
    let first_replacement_block_idx = records
        .first()
        .map(|record| checked_i64(record.block_idx, "block_idx"))
        .transpose()?;
    let search_deleted = conn.execute(
        "DELETE FROM transcript_search
         WHERE block_idx IN (
             SELECT block_idx FROM transcript_blocks
             WHERE record_idx >= ?1
                OR (?2 IS NOT NULL AND block_idx >= ?2)
         )",
        params![start_record_idx, first_replacement_block_idx],
    )?;
    let block_deleted = conn.execute(
        "DELETE FROM transcript_blocks
         WHERE record_idx >= ?1
            OR (?2 IS NOT NULL AND block_idx >= ?2)",
        params![start_record_idx, first_replacement_block_idx],
    )?;
    for (offset, record) in records.iter().enumerate() {
        let record_idx = checked_i64(start_record_idx as u64 + offset as u64, "record_idx")?;
        let mut block: Value = serde_json::from_str(&record.block_json)?;
        let mut refs = Vec::new();
        normalize_metadata(Some(conn), &mut block, compression, &mut refs)?;
        let block_json = serde_json::to_string(&block)?;
        let tool_state_json = match &record.tool_state_json {
            Some(json) => {
                let mut value: Value = serde_json::from_str(json)?;
                normalize_metadata(Some(conn), &mut value, compression, &mut refs)?;
                Some(serde_json::to_string(&value)?)
            }
            None => None,
        };
        insert_transcript_record_record(
            conn,
            record_idx,
            record,
            &block_json,
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
        "store:transcript:block_db_rows_deleted",
        search_deleted.saturating_add(block_deleted) as u64,
    );
    perf::record_value(
        "store:transcript:block_db_rows_inserted",
        records.len() as u64,
    );
    rebuild_transcript_extent_chunks(
        conn,
        if compacted {
            0
        } else {
            start_record_idx as usize
        },
    )?;
    Ok(())
}

fn compact_transcript_record_indices(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT block_idx
         FROM transcript_blocks
         WHERE block_json IS NOT NULL
         ORDER BY record_idx",
    )?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    conn.execute(
        "UPDATE transcript_blocks SET record_idx = NULL
         WHERE block_json IS NOT NULL",
        [],
    )?;
    for (idx, block_idx) in rows.into_iter().enumerate() {
        conn.execute(
            "UPDATE transcript_blocks SET record_idx = ?1 WHERE block_idx = ?2",
            params![idx as i64, block_idx],
        )?;
    }
    Ok(())
}

fn insert_transcript_record_record(
    conn: &Connection,
    record_idx: i64,
    record: &StoredTranscriptBlock,
    block_json: &str,
    tool_state_json: Option<&str>,
) -> Result<()> {
    let block_idx = checked_i64(record.block_idx, "block_idx")?;
    let history_idx = record
        .history_idx
        .map(|idx| checked_i64(idx, "history_idx"))
        .transpose()?;
    let extent = transcript_record_extent_profile(
        &record.kind,
        record.estimated_text_bytes,
        &record.preview_text,
        &record.indexed_text,
    );
    let extent_rows = extent.rows();
    conn.execute(
        "INSERT INTO transcript_blocks (
            block_idx, record_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
            estimated_text_bytes, block_json, origin_json, tool_state_json, preview_text,
            extent_profile_version, extent_rows_20, extent_rows_40, extent_rows_80,
            extent_rows_120, extent_rows_160, extent_rows_240
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16, ?17, ?18, ?19
         )",
        params![
            block_idx,
            record_idx,
            history_idx,
            record.kind,
            record.tool_call_id,
            record.tool_name,
            record.content_hash,
            checked_i64(record.estimated_text_bytes, "estimated_text_bytes")?,
            block_json,
            record.origin_json,
            tool_state_json,
            record.preview_text,
            TRANSCRIPT_EXTENT_PROFILE_VERSION,
            checked_i64(extent_rows[0], "extent_rows_20")?,
            checked_i64(extent_rows[1], "extent_rows_40")?,
            checked_i64(extent_rows[2], "extent_rows_80")?,
            checked_i64(extent_rows[3], "extent_rows_120")?,
            checked_i64(extent_rows[4], "extent_rows_160")?,
            checked_i64(extent_rows[5], "extent_rows_240")?,
        ],
    )?;
    insert_transcript_search(conn, block_idx, history_idx, &record.indexed_text)?;
    Ok(())
}

pub(crate) fn transcript_record_count(conn: &Connection) -> Result<usize> {
    let _perf = perf::begin("store:transcript:record_count");
    let cached = conn
        .query_row(
            "SELECT transcript_record_count FROM session_state WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let count = match cached {
        Some(count) => count,
        None => conn.query_row(
            "SELECT COUNT(*) FROM transcript_blocks WHERE block_json IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )?,
    };
    perf::record_value("store:transcript:record_count_total", count as u64);
    Ok(count as usize)
}

pub(crate) fn transcript_record_dense_extent(conn: &Connection) -> Result<usize> {
    let _perf = perf::begin("store:transcript:record_dense_extent");
    let count: i64 = conn.query_row(
        "SELECT COALESCE(MAX(record_idx) + 1, 0)
         FROM transcript_blocks
         WHERE block_json IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    perf::record_value("store:transcript:record_dense_extent_total", count as u64);
    Ok(count as usize)
}

pub(crate) fn transcript_record_index_for_block_idx(
    conn: &Connection,
    block_idx: u64,
) -> Result<Option<TranscriptRecordOffset>> {
    let _perf = perf::begin("store:transcript:block_index_for_block");
    let block_idx = checked_i64(block_idx, "block_idx")?;
    let index: Option<i64> = conn
        .query_row(
            "SELECT record_idx
         FROM transcript_blocks
         WHERE block_json IS NOT NULL AND block_idx = ?1",
            [block_idx],
            |row| row.get(0),
        )
        .optional()?;
    perf::record_value(
        "store:transcript:block_block_found",
        u64::from(index.is_some()),
    );
    Ok(index.map(|index| TranscriptRecordOffset::new(index.max(0) as usize)))
}

fn estimated_text_row_profile(text: &str) -> [u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()] {
    let mut rows = [0u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()];
    for line in text.lines() {
        let cells = if line.is_ascii() {
            line.len()
        } else {
            UnicodeWidthStr::width(line)
        }
        .max(1) as u64;
        for (total, width) in rows.iter_mut().zip(TRANSCRIPT_EXTENT_PROFILE_WIDTHS) {
            *total = total.saturating_add(cells.div_ceil(u64::from(width)));
        }
    }
    rows.map(|rows| rows.max(1))
}

fn transcript_record_extent_profile(
    kind: &str,
    estimated_text_bytes: u64,
    preview_text: &str,
    indexed_text: &str,
) -> TranscriptExtentProfile {
    let compact = matches!(kind, "tool" | "thinking" | "process_status" | "mode");
    let text = if matches!(kind, "tool" | "thinking") && !preview_text.is_empty() {
        preview_text
            .lines()
            .find(|line| !line.is_empty())
            .unwrap_or_default()
    } else if compact && !preview_text.is_empty() {
        preview_text
    } else {
        indexed_text
    };
    let omitted_bytes = if compact {
        0
    } else {
        estimated_text_bytes.saturating_sub(text.len() as u64)
    };
    let text_rows = estimated_text_row_profile(text);
    TranscriptExtentProfile::new(std::array::from_fn(|index| {
        text_rows[index]
            .saturating_add(
                omitted_bytes.div_ceil(u64::from(TRANSCRIPT_EXTENT_PROFILE_WIDTHS[index])),
            )
            .saturating_add(1)
    }))
}

fn extent_profile_rows_from_row(
    row: &rusqlite::Row<'_>,
    start: usize,
) -> rusqlite::Result<[i64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()]> {
    Ok([
        row.get(start)?,
        row.get(start + 1)?,
        row.get(start + 2)?,
        row.get(start + 3)?,
        row.get(start + 4)?,
        row.get(start + 5)?,
    ])
}

fn valid_extent_profile_rows(
    rows: [i64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()],
    record_count: usize,
) -> bool {
    let Ok(minimum_rows) = i64::try_from(record_count) else {
        return false;
    };
    rows.iter().all(|rows| *rows >= minimum_rows)
        && rows.windows(2).all(|widths| widths[0] >= widths[1])
}

fn extent_profile_from_validated_rows(
    rows: [i64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()],
) -> TranscriptExtentProfile {
    TranscriptExtentProfile::new(rows.map(|rows| rows as u64))
}

pub(crate) fn transcript_extent_chunks(conn: &Connection) -> Result<Vec<TranscriptExtentChunk>> {
    let _perf = perf::begin("store:transcript:extent_chunks");
    let mut stmt = conn.prepare(
        "SELECT chunk_idx, record_count,
                rows_20, rows_40, rows_80, rows_120, rows_160, rows_240
         FROM transcript_extent_chunks
         ORDER BY chunk_idx",
    )?;
    let stored_chunks = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            extent_profile_rows_from_row(row, 2)?,
        ))
    })?;
    let mut chunks = Vec::new();
    for chunk in stored_chunks {
        let (chunk_idx, record_count, rows) = chunk?;
        let (Ok(chunk_idx), Ok(record_count)) =
            (usize::try_from(chunk_idx), usize::try_from(record_count))
        else {
            return Err(StoreError::Integrity(
                "invalid transcript extent chunk coordinates".to_string(),
            ));
        };
        if !valid_extent_profile_rows(rows, record_count) {
            return Err(StoreError::Integrity(format!(
                "invalid transcript extent profile for chunk {chunk_idx}"
            )));
        }
        chunks.push(TranscriptExtentChunk {
            start: TranscriptRecordOffset::new(
                chunk_idx.saturating_mul(TRANSCRIPT_EXTENT_CHUNK_RECORDS),
            ),
            record_count,
            profile: extent_profile_from_validated_rows(rows),
        });
    }
    perf::record_value("store:transcript:extent_chunk_count", chunks.len() as u64);
    Ok(chunks)
}

pub(crate) fn transcript_record_estimated_rows(
    conn: &Connection,
    range: TranscriptRecordRange,
    width: u16,
) -> Result<u64> {
    let _perf = perf::begin("store:transcript:block_estimated_rows");
    let start = range.start().get();
    let end = range.end().get();
    if start >= end {
        perf::record_value("store:transcript:block_estimated_rows_requested", 0);
        perf::record_value("store:transcript:block_estimated_rows_total", 0);
        return Ok(0);
    }
    let (record_count, current_profile_count, profile_rows) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN extent_profile_version = ?3 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(extent_rows_20), 0),
                COALESCE(SUM(extent_rows_40), 0),
                COALESCE(SUM(extent_rows_80), 0),
                COALESCE(SUM(extent_rows_120), 0),
                COALESCE(SUM(extent_rows_160), 0),
                COALESCE(SUM(extent_rows_240), 0)
         FROM transcript_blocks
         WHERE block_json IS NOT NULL
           AND record_idx >= ?1
           AND record_idx < ?2",
        params![
            checked_i64(start as u64, "block_estimated_rows_start")?,
            checked_i64(end as u64, "block_estimated_rows_end")?,
            TRANSCRIPT_EXTENT_PROFILE_VERSION,
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?.max(0) as usize,
                row.get::<_, i64>(1)?.max(0) as usize,
                extent_profile_rows_from_row(row, 2)?,
            ))
        },
    )?;
    if current_profile_count != record_count
        || !valid_extent_profile_rows(profile_rows, record_count)
    {
        return Err(StoreError::Integrity(format!(
            "invalid transcript extent profiles in record range {start}..{end}"
        )));
    }
    let rows = extent_profile_from_validated_rows(profile_rows).estimated_rows(width);
    perf::record_value(
        "store:transcript:block_estimated_rows_requested",
        end.saturating_sub(start) as u64,
    );
    perf::record_value("store:transcript:block_estimated_rows_total", rows);
    Ok(rows)
}

pub(crate) fn backfill_transcript_extent_profiles(conn: &Connection) -> Result<()> {
    let _perf = perf::begin("store:transcript:extent_profile_backfill");
    let mut backfilled = 0usize;
    let mut last_record_idx = -1i64;
    loop {
        let profiles = {
            let _perf = perf::begin("store:transcript:extent_profile_backfill:read_compute");
            let mut stmt = conn.prepare(TRANSCRIPT_EXTENT_BACKFILL_BATCH_SQL)?;
            let profiles = stmt
                .query_map(
                    params![
                        TRANSCRIPT_EXTENT_PROFILE_VERSION,
                        last_record_idx,
                        TRANSCRIPT_EXTENT_BACKFILL_BATCH_RECORDS as i64
                    ],
                    |row| {
                        let block_idx = row.get::<_, i64>(0)?;
                        let record_idx = row.get::<_, i64>(1)?;
                        let kind = row.get::<_, String>(2)?;
                        let estimated_text_bytes = row.get::<_, i64>(3)?.max(0) as u64;
                        let preview_text = row.get::<_, String>(4)?;
                        let indexed_text = row.get::<_, String>(5)?;
                        Ok((
                            block_idx,
                            record_idx,
                            transcript_record_extent_profile(
                                &kind,
                                estimated_text_bytes,
                                &preview_text,
                                &indexed_text,
                            ),
                        ))
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            profiles
        };
        let Some((_, batch_last_record_idx, _)) = profiles.last() else {
            break;
        };
        last_record_idx = *batch_last_record_idx;

        let _perf = perf::begin("store:transcript:extent_profile_backfill:update");
        let mut update = conn.prepare(
            "UPDATE transcript_blocks
             SET extent_profile_version = ?1,
                 extent_rows_20 = ?2, extent_rows_40 = ?3, extent_rows_80 = ?4,
                 extent_rows_120 = ?5, extent_rows_160 = ?6, extent_rows_240 = ?7
             WHERE block_idx = ?8",
        )?;
        for (block_idx, _, profile) in &profiles {
            let rows = profile.rows();
            update.execute(params![
                TRANSCRIPT_EXTENT_PROFILE_VERSION,
                checked_i64(rows[0], "extent_rows_20")?,
                checked_i64(rows[1], "extent_rows_40")?,
                checked_i64(rows[2], "extent_rows_80")?,
                checked_i64(rows[3], "extent_rows_120")?,
                checked_i64(rows[4], "extent_rows_160")?,
                checked_i64(rows[5], "extent_rows_240")?,
                block_idx,
            ])?;
        }
        backfilled = backfilled.saturating_add(profiles.len());
    }
    perf::record_value(
        "store:transcript:extent_profiles_backfilled",
        backfilled as u64,
    );
    rebuild_transcript_extent_chunks(conn, 0)
}

pub(crate) fn rebuild_transcript_extent_chunks(
    conn: &Connection,
    start_record_idx: usize,
) -> Result<()> {
    let _perf = perf::begin("store:transcript:extent_chunks_rebuild");
    let first_chunk = start_record_idx / TRANSCRIPT_EXTENT_CHUNK_RECORDS;
    conn.execute(
        "DELETE FROM transcript_extent_chunks WHERE chunk_idx >= ?1",
        [checked_i64(first_chunk as u64, "extent_first_chunk")?],
    )?;
    conn.execute(
        "INSERT INTO transcript_extent_chunks (
             chunk_idx, record_count, rows_20, rows_40, rows_80,
             rows_120, rows_160, rows_240
         )
         SELECT record_idx / ?1, COUNT(*),
                SUM(extent_rows_20), SUM(extent_rows_40), SUM(extent_rows_80),
                SUM(extent_rows_120), SUM(extent_rows_160), SUM(extent_rows_240)
         FROM transcript_blocks
         WHERE block_json IS NOT NULL AND record_idx >= ?2
         GROUP BY record_idx / ?1
         ORDER BY record_idx / ?1",
        params![
            checked_i64(
                TRANSCRIPT_EXTENT_CHUNK_RECORDS as u64,
                "extent_chunk_records"
            )?,
            checked_i64(
                first_chunk.saturating_mul(TRANSCRIPT_EXTENT_CHUNK_RECORDS) as u64,
                "extent_chunk_start"
            )?,
        ],
    )?;
    Ok(())
}

pub(crate) fn read_transcript_records(conn: &Connection) -> Result<Vec<StoredTranscriptBlock>> {
    let _perf = perf::begin("store:transcript:read_records_full");
    let mut stmt = conn.prepare(
        "SELECT b.block_idx, b.history_idx, b.kind, b.tool_call_id, b.tool_name, b.content_hash,
                b.estimated_text_bytes, b.preview_text, COALESCE(s.indexed_text, '') AS indexed_text,
                b.block_json, b.origin_json, b.tool_state_json
         FROM transcript_blocks b
         LEFT JOIN transcript_search s ON s.block_idx = b.block_idx
         WHERE b.block_json IS NOT NULL
         ORDER BY b.record_idx",
    )?;
    let records = read_transcript_records_from_stmt(
        conn,
        &mut stmt,
        [],
        TranscriptRecordHydration::Hydrated,
    )?;
    perf::record_value("store:transcript:records_full_loaded", records.len() as u64);
    Ok(records)
}

pub(crate) fn read_transcript_record_slice(
    conn: &Connection,
    range: TranscriptRecordRange,
) -> Result<TranscriptRecordSlice> {
    let _perf = perf::begin("store:transcript:read_block_slice");
    let total_count = transcript_record_count(conn)?;
    read_transcript_record_slice_with_total(conn, range, total_count)
}

pub(crate) fn read_transcript_record_slice_with_total(
    conn: &Connection,
    range: TranscriptRecordRange,
    total_count: usize,
) -> Result<TranscriptRecordSlice> {
    let start = range.start().get().min(total_count);
    let end = range.end().get().min(total_count);
    if start >= end {
        perf::record_value("store:transcript:block_slice_requested", 0);
        perf::record_value("store:transcript:records_loaded", 0);
        perf::record_value("store:transcript:block_json_bytes_loaded", 0);
        return Ok(TranscriptRecordSlice::new(
            TranscriptRecordOffset::new(start),
            total_count,
            TranscriptRecordHydration::ObjectBacked,
            Vec::new(),
        ));
    }
    let limit = checked_i64((end - start) as u64, "block_range_len")?;
    let offset = checked_i64(start as u64, "block_range_start")?;
    let mut stmt = conn.prepare(
        "SELECT block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
                estimated_text_bytes, preview_text, '' AS indexed_text,
                block_json, origin_json, tool_state_json
         FROM transcript_blocks
         WHERE block_json IS NOT NULL
           AND record_idx >= ?2
           AND record_idx < ?2 + ?1
         ORDER BY record_idx",
    )?;
    let records = read_transcript_records_from_stmt(
        conn,
        &mut stmt,
        params![limit, offset],
        TranscriptRecordHydration::ObjectBacked,
    )?;
    perf::record_value(
        "store:transcript:block_slice_requested",
        end.saturating_sub(start) as u64,
    );
    Ok(TranscriptRecordSlice::new(
        TranscriptRecordOffset::new(start),
        total_count,
        TranscriptRecordHydration::ObjectBacked,
        records,
    ))
}

pub(crate) fn read_transcript_record_tail_slice(
    conn: &Connection,
    count: usize,
) -> Result<TranscriptRecordSlice> {
    let _perf = perf::begin("store:transcript:read_block_tail_slice");
    perf::record_value("store:transcript:block_tail_requested", count as u64);
    let total_count = transcript_record_count(conn)?;
    read_transcript_record_tail_slice_with_total(conn, total_count, count)
}

pub(crate) fn read_transcript_record_tail_slice_with_total(
    conn: &Connection,
    total_count: usize,
    count: usize,
) -> Result<TranscriptRecordSlice> {
    let count = count.min(total_count);
    let start = total_count.saturating_sub(count);
    if count == 0 {
        perf::record_value("store:transcript:block_slice_requested", 0);
        perf::record_value("store:transcript:records_loaded", 0);
        perf::record_value("store:transcript:block_json_bytes_loaded", 0);
        return Ok(TranscriptRecordSlice::new(
            TranscriptRecordOffset::new(start),
            total_count,
            TranscriptRecordHydration::ObjectBacked,
            Vec::new(),
        ));
    }
    let limit = checked_i64(count as u64, "block_tail_len")?;
    let mut stmt = conn.prepare(
        "SELECT block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
                estimated_text_bytes, preview_text, '' AS indexed_text,
                block_json, origin_json, tool_state_json
         FROM transcript_blocks
         WHERE block_json IS NOT NULL
         ORDER BY record_idx DESC
         LIMIT ?1",
    )?;
    let mut records = read_transcript_records_from_stmt(
        conn,
        &mut stmt,
        params![limit],
        TranscriptRecordHydration::ObjectBacked,
    )?;
    records.reverse();
    perf::record_value("store:transcript:block_slice_requested", count as u64);
    Ok(TranscriptRecordSlice::new(
        TranscriptRecordOffset::new(start),
        total_count,
        TranscriptRecordHydration::ObjectBacked,
        records,
    ))
}

pub(crate) fn read_transcript_record_centered_slice(
    conn: &Connection,
    center_record_idx: u64,
    before: usize,
    after: usize,
) -> Result<TranscriptRecordSlice> {
    let _perf = perf::begin("store:transcript:read_block_centered_slice");
    let total_count = transcript_record_count(conn)?;
    if total_count == 0 {
        return read_transcript_record_slice_with_total(conn, (0..0).into(), total_count);
    }
    let center = (center_record_idx as usize).min(total_count.saturating_sub(1));
    let start = center.saturating_sub(before);
    let end = center
        .saturating_add(after)
        .saturating_add(1)
        .min(total_count);
    read_transcript_record_slice_with_total(conn, (start..end).into(), total_count)
}

pub(crate) fn read_transcript_record_before_kind_at_index(
    conn: &Connection,
    kind: &str,
    before_or_at_block_index: u64,
) -> Result<Option<StoredTranscriptBlock>> {
    let _perf = perf::begin("store:transcript:read_block_before_kind");
    let before_or_at = checked_i64(before_or_at_block_index, "before_or_at_block_index")?;
    let mut stmt = conn.prepare(
        "SELECT block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
                estimated_text_bytes, preview_text, '' AS indexed_text,
                block_json, origin_json, tool_state_json
         FROM transcript_blocks
         WHERE block_json IS NOT NULL
           AND kind = ?1
           AND record_idx <= ?2
         ORDER BY record_idx DESC
         LIMIT 1",
    )?;
    let mut records = read_transcript_records_from_stmt(
        conn,
        &mut stmt,
        params![kind, before_or_at],
        TranscriptRecordHydration::ObjectBacked,
    )?;
    Ok(records.pop())
}

pub(crate) fn read_transcript_record_after_kind_at_index(
    conn: &Connection,
    kind: &str,
    after_or_at_block_index: u64,
) -> Result<Option<StoredTranscriptBlock>> {
    let _perf = perf::begin("store:transcript:read_block_after_kind");
    let after_or_at = checked_i64(after_or_at_block_index, "after_or_at_block_index")?;
    let mut stmt = conn.prepare(
        "SELECT block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
                estimated_text_bytes, preview_text, '' AS indexed_text,
                block_json, origin_json, tool_state_json
         FROM transcript_blocks
         WHERE block_json IS NOT NULL
           AND kind = ?1
           AND record_idx >= ?2
         ORDER BY record_idx ASC
         LIMIT 1",
    )?;
    let mut records = read_transcript_records_from_stmt(
        conn,
        &mut stmt,
        params![kind, after_or_at],
        TranscriptRecordHydration::ObjectBacked,
    )?;
    Ok(records.pop())
}

fn read_transcript_records_from_stmt<P>(
    conn: &Connection,
    stmt: &mut Statement<'_>,
    params: P,
    hydration: TranscriptRecordHydration,
) -> Result<Vec<StoredTranscriptBlock>>
where
    P: rusqlite::Params,
{
    let rows = stmt.query_map(params, |row| {
        Ok(StoredTranscriptBlock {
            block_idx: row.get::<_, i64>(0)? as u64,
            history_idx: row.get::<_, Option<i64>>(1)?.map(|idx| idx as u64),
            kind: row.get(2)?,
            tool_call_id: row.get(3)?,
            tool_name: row.get(4)?,
            content_hash: row.get(5)?,
            estimated_text_bytes: row.get::<_, i64>(6)? as u64,
            preview_text: row.get(7)?,
            indexed_text: row.get(8)?,
            block_json: row.get(9)?,
            origin_json: row.get(10)?,
            tool_state_json: row.get(11)?,
        })
    })?;
    let mut records = Vec::new();
    let mut json_bytes = 0u64;
    for row in rows {
        let mut record = row?;
        json_bytes = json_bytes.saturating_add(record.block_json.len() as u64);
        if let Some(json) = &record.tool_state_json {
            json_bytes = json_bytes.saturating_add(json.len() as u64);
        }
        if hydration.hydrates_objects() {
            let mut block: Value = serde_json::from_str(&record.block_json)?;
            rehydrate_object_refs(conn, &mut block)?;
            record.block_json = serde_json::to_string(&block)?;
            if let Some(json) = &record.tool_state_json {
                let mut value: Value = serde_json::from_str(json)?;
                rehydrate_object_refs(conn, &mut value)?;
                record.tool_state_json = Some(serde_json::to_string(&value)?);
            }
        }
        records.push(record);
    }
    perf::record_value("store:transcript:records_loaded", records.len() as u64);
    perf::record_value("store:transcript:block_json_bytes_loaded", json_bytes);
    match hydration {
        TranscriptRecordHydration::Hydrated => perf::record_value(
            "store:transcript:records_hydrated_loaded",
            records.len() as u64,
        ),
        TranscriptRecordHydration::ObjectBacked => perf::record_value(
            "store:transcript:records_object_backed_loaded",
            records.len() as u64,
        ),
    }
    Ok(records)
}

const TRANSCRIPT_SEARCH_CHAR_MASK_BITS: u32 = 63;
const TRANSCRIPT_SEARCH_CHAR_MASK_COUNT: usize = 4;
const TRANSCRIPT_SEARCH_CHAR_LIMIT: u32 =
    TRANSCRIPT_SEARCH_CHAR_MASK_BITS * TRANSCRIPT_SEARCH_CHAR_MASK_COUNT as u32;

pub(crate) fn transcript_search_char_masks(text: &str) -> [i64; TRANSCRIPT_SEARCH_CHAR_MASK_COUNT] {
    let mut masks = [0i64; TRANSCRIPT_SEARCH_CHAR_MASK_COUNT];
    for ch in text.chars() {
        let codepoint = ch as u32;
        if codepoint >= TRANSCRIPT_SEARCH_CHAR_LIMIT {
            continue;
        }
        let bucket = (codepoint / TRANSCRIPT_SEARCH_CHAR_MASK_BITS) as usize;
        let bit = codepoint % TRANSCRIPT_SEARCH_CHAR_MASK_BITS;
        masks[bucket] |= 1i64 << bit;
    }
    masks
}

fn transcript_search_char_mask(query: &str) -> Option<(usize, i64)> {
    let mut chars = query.chars();
    let codepoint = chars.next()? as u32;
    if chars.next().is_some() || codepoint >= TRANSCRIPT_SEARCH_CHAR_LIMIT {
        return None;
    }
    let bucket = (codepoint / TRANSCRIPT_SEARCH_CHAR_MASK_BITS) as usize;
    let bit = codepoint % TRANSCRIPT_SEARCH_CHAR_MASK_BITS;
    Some((bucket, 1i64 << bit))
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
    let char_mask = (!use_fts)
        .then(|| transcript_search_char_mask(query))
        .flatten();
    perf::record_value("store:transcript:search_fts", u64::from(use_fts));
    perf::record_value(
        "store:transcript:search_char_mask",
        u64::from(char_mask.is_some()),
    );

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
                char_mask,
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
    char_mask: Option<(usize, i64)>,
}

fn search_transcript_candidate_batch(
    conn: &Connection,
    query: TranscriptCandidateBatchQuery<'_>,
) -> Result<Vec<TranscriptCandidateRow>> {
    let order = match query.direction {
        TranscriptSearchDirection::Forward => "ASC",
        TranscriptSearchDirection::Backward => "DESC",
    };
    let bound_column = if query.use_fts {
        "f.rowid"
    } else if query.char_mask.is_some() {
        "c.block_idx"
    } else {
        "s.block_idx"
    };
    let bound_filter = match (query.bound, query.inclusive, query.direction) {
        (Some(_), true, TranscriptSearchDirection::Forward) => {
            format!("AND {bound_column} >= ?")
        }
        (Some(_), false, TranscriptSearchDirection::Forward) => {
            format!("AND {bound_column} > ?")
        }
        (Some(_), true, TranscriptSearchDirection::Backward) => {
            format!("AND {bound_column} <= ?")
        }
        (Some(_), false, TranscriptSearchDirection::Backward) => {
            format!("AND {bound_column} < ?")
        }
        (None, _, _) => String::new(),
    };
    let sql = if query.use_fts {
        format!(
            "SELECT f.rowid, s.history_idx
             FROM transcript_search_fts f
             JOIN transcript_search s ON s.block_idx = f.rowid
             WHERE f.indexed_text MATCH ?
               AND instr(s.indexed_text, ?) > 0 {bound_filter}
             ORDER BY f.rowid {order}
             LIMIT ?"
        )
    } else if let Some((bucket, _)) = query.char_mask {
        format!(
            "SELECT c.block_idx, s.history_idx
             FROM transcript_search_chars c
             CROSS JOIN transcript_search s ON s.block_idx = c.block_idx
             WHERE (c.mask_{bucket} & ?) != 0
               AND instr(s.indexed_text, ?) > 0 {bound_filter}
             ORDER BY c.block_idx {order}
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
    } else if let Some((_, mask)) = query.char_mask {
        values.push(SqlValue::from(mask));
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
    let masks = transcript_search_char_masks(indexed_text);
    conn.execute(
        "INSERT INTO transcript_search_chars (block_idx, mask_0, mask_1, mask_2, mask_3)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![block_idx, masks[0], masks[1], masks[2], masks[3]],
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

pub(crate) fn history_search_text(item: &HistoryItem) -> Result<String> {
    Ok(collect_text(&serde_json::to_value(item)?, 64 * 1024))
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

pub(crate) fn serialize_normalized_history_item(
    conn: &Connection,
    item: &HistoryItem,
    compression: ObjectCompression,
) -> Result<Vec<u8>> {
    Ok(normalized_history_value(item, compression, Some(conn))?
        .json
        .into_bytes())
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

pub(crate) fn normalize_metadata(
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
    fn text_row_profile_matches_independent_width_counts() {
        for text in ["", "short\na much longer line", "ASCII and unicode 界🙂\n"] {
            let profile = estimated_text_row_profile(text);
            let expected = TRANSCRIPT_EXTENT_PROFILE_WIDTHS.map(|width| {
                let width = usize::from(width);
                text.lines()
                    .map(|line| UnicodeWidthStr::width(line).max(1).div_ceil(width) as u64)
                    .sum::<u64>()
                    .max(1)
            });
            assert_eq!(profile, expected);
        }
    }

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
    fn bounded_history_tail_stops_before_hydrating_an_over_budget_object() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let object_backed = HistoryItem::Assistant(protocol::AssistantStep::with_invocations(
            None,
            None,
            Vec::new(),
            vec![protocol::ToolInvocation {
                call_id: "large-call".into(),
                name: "large-tool".into(),
                arguments: "{}".into(),
                result: protocol::ToolOutcome {
                    content: "large result".into(),
                    is_error: false,
                    metadata: Some(json!({ "payload": "x".repeat(32 * 1024) })),
                },
                elapsed_ms: None,
            }],
        ));
        let newest = vec![
            HistoryItem::user(protocol::Content::text("newer")),
            HistoryItem::user(protocol::Content::text("newest")),
        ];
        let history = vec![
            HistoryItem::user(protocol::Content::text("oldest")),
            object_backed,
            newest[0].clone(),
            newest[1].clone(),
        ];
        replace_history_suffix(&conn, 0, &history, ObjectCompression::none()).unwrap();
        let newest_bytes = newest
            .iter()
            .map(|item| serde_json::to_vec(item).unwrap().len())
            .sum();

        conn.execute_batch("PRAGMA foreign_keys = OFF; DELETE FROM objects;")
            .unwrap();
        assert_eq!(
            read_history_items_tail(&conn, history.len(), 10, Some(newest_bytes)).unwrap(),
            newest
        );
    }

    #[test]
    fn bounded_history_tail_preserves_newest_suffix_semantics() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let history = vec![
            HistoryItem::user(protocol::Content::text("one")),
            HistoryItem::user(protocol::Content::text("two")),
            HistoryItem::user(protocol::Content::text("three")),
        ];
        replace_history_suffix(&conn, 0, &history, ObjectCompression::none()).unwrap();
        let newest_bytes = serde_json::to_vec(&history[2]).unwrap().len();

        assert_eq!(
            read_history_items_tail(&conn, history.len(), 2, None).unwrap(),
            history[1..].to_vec()
        );
        assert_eq!(
            read_history_items_tail(&conn, history.len(), 3, Some(newest_bytes)).unwrap(),
            history[2..].to_vec()
        );
        assert!(read_history_items_tail(
            &conn,
            history.len(),
            3,
            Some(newest_bytes.saturating_sub(1))
        )
        .unwrap()
        .is_empty());
        assert!(read_history_items_tail(&conn, history.len(), 0, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn history_semantic_projections_do_not_require_payload_hydration() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let history = vec![
            HistoryItem::system("system"),
            HistoryItem::note(protocol::HistoryNote::context("hidden")),
            HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                "normal",
                "plan",
                "switch mode",
            )),
        ];
        replace_history_suffix(&conn, 0, &history, ObjectCompression::none()).unwrap();

        assert!(!history_any_transcript_visible_before(&conn, 2).unwrap());
        assert!(history_any_transcript_visible_before(&conn, 3).unwrap());
        assert_eq!(history_mode_before(&conn, 2).unwrap(), None);
        assert_eq!(
            history_mode_before(&conn, 3).unwrap().as_deref(),
            Some("plan")
        );
        assert_eq!(history_note_projection_at(&conn, 0).unwrap(), None);
        assert_eq!(
            history_note_projection_at(&conn, 1).unwrap(),
            Some(protocol::HistoryNoteProjection {
                kind: protocol::HistoryNoteKind::Context,
                mode: None,
            })
        );
        assert_eq!(
            history_note_projection_at(&conn, 2).unwrap(),
            Some(protocol::HistoryNoteProjection {
                kind: protocol::HistoryNoteKind::ModeChange,
                mode: Some("plan".into()),
            })
        );
        assert_eq!(
            history_last_context_note_index_before(&conn, 3, protocol::DEFAULT_CONTEXT_NOTE_NAME)
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            history_last_context_note_index_before(&conn, 3, "goal").unwrap(),
            None
        );
        assert_eq!(
            history_base_mode_range(&conn, 0..3).unwrap().as_deref(),
            Some("normal")
        );
    }

    #[test]
    fn indexed_history_reads_batch_large_block_origin_sets() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let items = (0..=HISTORY_INDEX_READ_BATCH_SIZE)
            .map(|idx| HistoryItem::user(protocol::Content::text(format!("item {idx}"))))
            .collect::<Vec<_>>();
        replace_history_suffix(&conn, 0, &items, ObjectCompression::none()).unwrap();
        let indices = (0..items.len() as u64).collect::<Vec<_>>();

        let loaded = read_history_items_at_indices(&conn, &indices).unwrap();

        assert_eq!(loaded.len(), items.len());
        assert_eq!(loaded.get(&0), Some(&items[0]));
        assert_eq!(loaded.get(&(items.len() as u64 - 1)), items.last());
    }

    #[test]
    fn transcript_search_candidate_plan_uses_fts_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let details = query_plan_details(
            &conn,
            "SELECT f.rowid, s.history_idx
             FROM transcript_search_fts f
             JOIN transcript_search s ON s.block_idx = f.rowid
             WHERE f.indexed_text MATCH ?1
               AND instr(s.indexed_text, ?2) > 0
             ORDER BY f.rowid ASC
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
        assert!(
            details
                .iter()
                .all(|detail| !detail.contains("USE TEMP B-TREE FOR ORDER BY")),
            "{details:#?}"
        );
    }

    #[test]
    fn transcript_single_character_plan_scans_compact_masks_before_text() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let details = query_plan_details(
            &conn,
            "SELECT c.block_idx, s.history_idx
             FROM transcript_search_chars c
             CROSS JOIN transcript_search s ON s.block_idx = c.block_idx
             WHERE (c.mask_2 & ?1) != 0
               AND instr(s.indexed_text, ?2) > 0
             ORDER BY c.block_idx ASC
             LIMIT ?3",
            rusqlite::params![1_i64 << 41, "§", 64_i64],
        );

        assert!(
            details.iter().any(|detail| detail.contains("SCAN c")),
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
                .all(|detail| !detail.contains("USE TEMP B-TREE FOR ORDER BY")),
            "{details:#?}"
        );
    }

    #[test]
    fn extent_profile_backfill_uses_indexed_keyset_and_search_lookups() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let details = query_plan_details(
            &conn,
            TRANSCRIPT_EXTENT_BACKFILL_BATCH_SQL,
            rusqlite::params![TRANSCRIPT_EXTENT_PROFILE_VERSION, -1_i64, 256_i64],
        );

        assert!(
            details.iter().any(|detail| {
                detail.contains("SEARCH b USING INDEX") && detail.contains("record_idx>?")
            }),
            "{details:#?}"
        );
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("SEARCH s USING INTEGER PRIMARY KEY")),
            "{details:#?}"
        );
        assert!(
            details.iter().all(|detail| !detail.contains("SCAN s")),
            "{details:#?}"
        );
    }

    #[test]
    fn extent_profile_backfill_handles_gaps_and_interleaved_current_profiles() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();
        let row_count = TRANSCRIPT_EXTENT_BACKFILL_BATCH_RECORDS * 2 + 18;
        for block_idx in 0..row_count {
            let record_idx = block_idx * 2;
            let current = block_idx % 3 == 0;
            let indexed_text = format!(
                "record {record_idx}\n{}",
                "heterogeneous wrapped content 界 ".repeat(block_idx % 9 + 1)
            );
            conn.execute(
                "INSERT INTO transcript_blocks (
                     block_idx, record_idx, kind, estimated_text_bytes, preview_text, block_json,
                     extent_profile_version, extent_rows_20, extent_rows_40, extent_rows_80,
                     extent_rows_120, extent_rows_160, extent_rows_240
                 ) VALUES (?1, ?2, 'text', ?3, '', '{}', ?4, ?5, ?5, ?5, ?5, ?5, ?5)",
                params![
                    block_idx as i64,
                    record_idx as i64,
                    indexed_text.len() as i64,
                    if current {
                        TRANSCRIPT_EXTENT_PROFILE_VERSION
                    } else {
                        0
                    },
                    if current { 7 } else { 0 },
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO transcript_search (block_idx, indexed_text) VALUES (?1, ?2)",
                params![block_idx as i64, indexed_text],
            )
            .unwrap();
        }

        backfill_transcript_extent_profiles(&conn).unwrap();

        let stale: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transcript_blocks
                 WHERE block_json IS NOT NULL AND extent_profile_version != ?1",
                [TRANSCRIPT_EXTENT_PROFILE_VERSION],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0);
        let preserved: [i64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()] = conn
            .query_row(
                "SELECT extent_rows_20, extent_rows_40, extent_rows_80,
                        extent_rows_120, extent_rows_160, extent_rows_240
                 FROM transcript_blocks WHERE block_idx = 0",
                [],
                |row| extent_profile_rows_from_row(row, 0),
            )
            .unwrap();
        assert_eq!(preserved, [7; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()]);
        let backfilled_version: i64 = conn
            .query_row(
                "SELECT extent_profile_version FROM transcript_blocks WHERE block_idx = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(backfilled_version, TRANSCRIPT_EXTENT_PROFILE_VERSION);
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
