use std::ops::Range;

use protocol::HistoryItem;
use rusqlite::Connection;
use serde_json::{json, Value};
use smelt_buffer::{cell_width, text};

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::object::{self, sha256_hex};

pub(crate) const METADATA_OBJECT_MIN_BYTES: usize = 4 * 1024;
pub(crate) const OBJECT_REF_KEY: &str = "$smelt_object_ref";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HistoryObjectRole {
    AttachmentImage,
    Metadata,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptRecordProfile {
    pub block_idx: u64,
    pub history_idx: Option<u64>,
    pub kind: String,
    pub role: String,
    pub first_line: String,
    pub estimated_text_bytes: u64,
    pub extent: TranscriptExtentProfile,
}

impl TranscriptRecordProfile {
    pub fn estimated_rows(&self, width: u16) -> u64 {
        self.extent.estimated_rows(width)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptNavigationRecord {
    pub record_index: TranscriptRecordOffset,
    pub profile: TranscriptRecordProfile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranscriptRowLocation {
    pub record_index: TranscriptRecordOffset,
    pub row_offset: u64,
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
    #[serde(default)]
    pub tool_render_revision: u64,
}

fn estimated_text_row_profile(text: &str) -> [u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()] {
    let mut rows = [0_u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()];
    for line in text.lines() {
        let cells = cell_width::text_width(line).max(1) as u64;
        for (total, width) in rows.iter_mut().zip(TRANSCRIPT_EXTENT_PROFILE_WIDTHS) {
            *total = total.saturating_add(cells.div_ceil(u64::from(width)));
        }
    }
    rows.map(|rows| rows.max(1))
}

pub(crate) fn transcript_record_extent_profile(
    record: &StoredTranscriptBlock,
) -> TranscriptExtentProfile {
    let compact = matches!(
        record.kind.as_str(),
        "tool" | "thinking" | "process_status" | "mode"
    );
    let text =
        if matches!(record.kind.as_str(), "tool" | "thinking") && !record.preview_text.is_empty() {
            record
                .preview_text
                .lines()
                .find(|line| !line.is_empty())
                .unwrap_or_default()
        } else if compact && !record.preview_text.is_empty() {
            &record.preview_text
        } else {
            &record.indexed_text
        };
    let omitted_bytes = if compact {
        0
    } else {
        record
            .estimated_text_bytes
            .saturating_sub(text.len() as u64)
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

pub(crate) fn transcript_extent_profile(
    records: &[StoredTranscriptBlock],
) -> TranscriptExtentProfile {
    let mut rows = [0_u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()];
    for record in records {
        for (total, value) in rows
            .iter_mut()
            .zip(transcript_record_extent_profile(record).rows())
        {
            *total = total.saturating_add(value);
        }
    }
    TranscriptExtentProfile::new(rows)
}

pub(crate) fn estimated_transcript_record_rows(
    records: &[StoredTranscriptBlock],
    width: u16,
) -> u64 {
    transcript_extent_profile(records).estimated_rows(width)
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

pub(crate) fn item_hash(item: &HistoryItem) -> Result<String> {
    let normalized = normalized_history_value(item, ObjectCompression::none(), None)?;
    let json = serde_json::to_string(&normalized.value)?;
    Ok(sha256_hex(json.as_bytes()))
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

pub(crate) fn history_search_text(item: &HistoryItem) -> Result<String> {
    Ok(collect_text(&serde_json::to_value(item)?, 64 * 1024))
}

struct NormalizedHistoryItem {
    value: Value,
    json: String,
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
    Ok(NormalizedHistoryItem { value, json })
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
                                .to_owned()
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
                                .to_owned()
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

pub(crate) fn collect_text(value: &Value, max_bytes: usize) -> String {
    let mut out = String::new();
    let mut truncated = false;
    collect_text_inner(value, &mut out, max_bytes, &mut truncated);
    out
}

fn collect_text_inner(value: &Value, out: &mut String, max_bytes: usize, truncated: &mut bool) {
    if *truncated || out.len() >= max_bytes {
        return;
    }
    match value {
        Value::String(value) => {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(value);
            if out.len() > max_bytes {
                let keep = text::grapheme_prefix(out, max_bytes).len();
                text::replace_range(out, keep..out.len(), "");
                *truncated = true;
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_text_inner(value, out, max_bytes, truncated);
            }
        }
        Value::Object(map) => {
            if map.contains_key(OBJECT_REF_KEY) {
                return;
            }
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                collect_text_inner(&map[key], out, max_bytes, truncated);
            }
        }
        _ => {}
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
                        reference: hash.to_owned(),
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
    fn collect_text_truncates_at_grapheme_boundaries() {
        let value = json!({"text": "ab🙂cd"});
        assert_eq!(collect_text(&value, 5), "ab");
        assert_eq!(collect_text(&value, 6), "ab🙂");

        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let value = json!({"text": format!("a{grapheme}b")});
            assert_eq!(collect_text(&value, grapheme.len()), "a", "{grapheme:?}");
        }

        let values = json!(["abce\u{301}", "later"]);
        assert_eq!(collect_text(&values, 4), "abc");
    }

    #[test]
    fn extent_profile_interpolates_monotonically() {
        let profile = TranscriptExtentProfile::new([120, 80, 40, 30, 20, 10]);
        let estimates = [20, 30, 40, 60, 80, 120, 240].map(|width| profile.estimated_rows(width));
        assert!(estimates.windows(2).all(|pair| pair[0] >= pair[1]));
    }

    #[test]
    fn stored_transcript_block_defaults_legacy_render_revision() {
        let record: StoredTranscriptBlock = serde_json::from_value(json!({
            "block_idx": 1,
            "history_idx": null,
            "kind": "tool",
            "tool_call_id": "call-1",
            "tool_name": "bash",
            "content_hash": "1",
            "estimated_text_bytes": 0,
            "preview_text": "",
            "indexed_text": "",
            "block_json": "{}",
            "origin_json": null,
            "tool_state_json": null
        }))
        .unwrap();

        assert_eq!(record.tool_render_revision, 0);
    }
}
