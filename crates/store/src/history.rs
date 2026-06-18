use protocol::HistoryItem;
use rusqlite::{params, params_from_iter, Connection, Statement};
use serde_json::{json, Value};
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
    let start_idx_sql = checked_i64(start_idx as u64, "start_idx")?;
    let preserve_through_block_idx = conn
        .query_row(
            "SELECT MAX(block_idx) FROM transcript_blocks WHERE history_idx < ?1",
            [start_idx_sql],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .unwrap_or(-1);
    conn.execute(
        "DELETE FROM transcript_search
         WHERE block_idx IN (
             SELECT block_idx FROM transcript_blocks
             WHERE history_idx >= ?1
                OR (history_idx IS NULL AND block_idx > ?2)
         )",
        params![start_idx_sql, preserve_through_block_idx],
    )?;
    conn.execute(
        "DELETE FROM transcript_blocks
         WHERE history_idx >= ?1
            OR (history_idx IS NULL AND block_idx > ?2)",
        params![start_idx_sql, preserve_through_block_idx],
    )?;
    conn.execute("DELETE FROM history_items WHERE idx >= ?1", [start_idx_sql])?;
    let first_block_idx = conn.query_row(
        "SELECT COALESCE(MAX(block_idx) + 1, 0) FROM transcript_blocks",
        [],
        |row| row.get::<_, i64>(0),
    )? as usize;
    for (block_idx, (offset, item)) in (first_block_idx..).zip(items.iter().enumerate()) {
        write_history_item_at_block(conn, start_idx + offset, block_idx, item, compression)?;
    }
    Ok(())
}

pub(crate) fn read_history_items(conn: &Connection) -> Result<Vec<HistoryItem>> {
    let mut stmt = conn.prepare("SELECT json FROM history_items ORDER BY idx")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let mut value: Value = serde_json::from_str(&row?)?;
        rehydrate_object_refs(conn, &mut value)?;
        out.push(serde_json::from_value(value)?);
    }
    Ok(out)
}

pub(crate) fn history_text_bytes(conn: &Connection) -> Result<u64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(estimated_text_bytes), 0) FROM transcript_blocks",
        [],
        |row| row.get(0),
    )?;
    Ok(total as u64)
}

pub(crate) fn search_blob(conn: &Connection) -> Result<String> {
    let mut stmt =
        conn.prepare("SELECT text FROM transcript_search WHERE text != '' ORDER BY block_idx")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = String::new();
    for row in rows {
        let text = row?;
        if text.is_empty() {
            continue;
        }
        out.push_str(&text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
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
    let result = replace_transcript_descriptor_suffix_inner(
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

fn replace_transcript_descriptor_suffix_inner(
    conn: &Connection,
    start_descriptor_idx: usize,
    records: &[TranscriptDescriptorRecord],
    compression: ObjectCompression,
) -> Result<()> {
    let start_descriptor_idx = checked_i64(start_descriptor_idx as u64, "start_descriptor_idx")?;
    conn.execute(
        "DELETE FROM transcript_search WHERE block_idx >= ?1",
        [start_descriptor_idx],
    )?;
    conn.execute(
        "DELETE FROM transcript_blocks WHERE block_idx >= ?1",
        [start_descriptor_idx],
    )?;
    for record in records {
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
    Ok(())
}

fn insert_transcript_descriptor_record(
    conn: &Connection,
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
            block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
            estimated_text_bytes, descriptor_json, origin_json, tool_state_json,
            preview_text, search_text
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            block_idx,
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
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM transcript_blocks WHERE descriptor_json IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}

pub(crate) fn read_transcript_descriptor_records(
    conn: &Connection,
) -> Result<Vec<TranscriptDescriptorRecord>> {
    let mut stmt = conn.prepare(
        "SELECT block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
                estimated_text_bytes, preview_text, search_text, descriptor_json,
                origin_json, tool_state_json
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL
         ORDER BY block_idx",
    )?;
    read_transcript_descriptor_records_from_stmt(
        conn,
        &mut stmt,
        [],
        TranscriptDescriptorHydration::Hydrated,
    )
}

pub(crate) fn read_transcript_descriptor_slice(
    conn: &Connection,
    range: TranscriptDescriptorRange,
) -> Result<TranscriptDescriptorSlice> {
    let total_count = transcript_descriptor_count(conn)?;
    let start = range.start().get().min(total_count);
    let end = range.end().get().min(total_count);
    if start >= end {
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
                estimated_text_bytes, preview_text, search_text, descriptor_json,
                origin_json, tool_state_json
         FROM transcript_blocks
         WHERE descriptor_json IS NOT NULL
         ORDER BY block_idx
         LIMIT ?1 OFFSET ?2",
    )?;
    let records = read_transcript_descriptor_records_from_stmt(
        conn,
        &mut stmt,
        params![limit, offset],
        TranscriptDescriptorHydration::ObjectBacked,
    )?;
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
    let total_count = transcript_descriptor_count(conn)?;
    let start = total_count.saturating_sub(count);
    read_transcript_descriptor_slice(
        conn,
        TranscriptDescriptorRange::new(
            TranscriptDescriptorIndex::new(start),
            TranscriptDescriptorIndex::new(total_count),
        ),
    )
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
    for row in rows {
        let mut record = row?;
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
    if query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let terms = search_terms(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let term_count = terms.len();
    let placeholders = std::iter::repeat_n("?", terms.len())
        .collect::<Vec<_>>()
        .join(", ");
    let order = match direction {
        TranscriptSearchDirection::Forward => "ASC",
        TranscriptSearchDirection::Backward => "DESC",
    };
    let origin_filter = match (origin_block_idx, direction) {
        (Some(_), TranscriptSearchDirection::Forward) => "AND s.block_idx >= ?",
        (Some(_), TranscriptSearchDirection::Backward) => "AND s.block_idx <= ?",
        (None, _) => "",
    };
    let sql = format!(
        "WITH matched(block_idx) AS (
             SELECT block_idx
             FROM transcript_search_terms
             WHERE term IN ({placeholders})
             GROUP BY block_idx
             HAVING COUNT(*) = ?
         )
         SELECT s.block_idx, s.history_idx, s.text
         FROM matched m
         JOIN transcript_search s ON s.block_idx = m.block_idx
         WHERE 1 = 1 {origin_filter}
         ORDER BY s.block_idx {order}
         LIMIT ? OFFSET ?"
    );
    let base_values = {
        let mut values = terms.into_iter().map(SqlValue::from).collect::<Vec<_>>();
        values.push(SqlValue::from(term_count as i64));
        if let Some(origin) = origin_block_idx {
            values.push(SqlValue::from(checked_i64(origin, "origin_block_idx")?));
        }
        values
    };
    let page_size = if limit == usize::MAX {
        1024usize
    } else {
        limit.max(64)
    };
    let mut stmt = conn.prepare(&sql)?;
    let mut out = Vec::new();
    let mut offset = 0usize;
    loop {
        let mut values = base_values.clone();
        values.push(SqlValue::from(checked_i64(
            page_size as u64,
            "search_candidate_page_size",
        )?));
        values.push(SqlValue::from(checked_i64(
            offset as u64,
            "search_candidate_offset",
        )?));
        let rows = stmt.query_map(params_from_iter(values), |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, Option<i64>>(1)?.map(|idx| idx as u64),
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut fetched = 0usize;
        for row in rows {
            fetched += 1;
            let (block_idx, history_idx, text) = row?;
            if text.contains(query) {
                out.push(TranscriptSearchCandidate {
                    block_idx,
                    history_idx,
                });
                if out.len() >= limit {
                    break;
                }
            }
        }
        if out.len() >= limit || fetched < page_size {
            break;
        }
        offset = offset.saturating_add(fetched);
    }
    if matches!(direction, TranscriptSearchDirection::Backward) {
        out.reverse();
    }
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
                if let Some(bytes) = object_bytes_by_hash(conn, hash)? {
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

pub(crate) fn object_bytes_by_hash(conn: &Connection, hash: &str) -> Result<Option<Vec<u8>>> {
    let Some(meta) = object::object_meta(conn, hash)? else {
        return Ok(None);
    };
    object::object_bytes(conn, &meta).map(Some)
}

pub(crate) fn insert_json_object(
    conn: &Connection,
    value: &mut Value,
    key: &str,
    hash: &str,
) -> Result<()> {
    let Some(bytes) = object_bytes_by_hash(conn, hash)? else {
        return Ok(());
    };
    value[key] = serde_json::from_slice(&bytes)?;
    Ok(())
}
