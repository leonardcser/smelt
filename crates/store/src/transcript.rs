use std::ops::Range;

use smelt_buffer::cell_width;

use crate::db;
use crate::error::{Result, StoreError};

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

/// A transcript block to read by id, with the record index it was last known at.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TranscriptBlockLookup {
    pub block_idx: u64,
    pub record_index_hint: Option<usize>,
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
const PROFILE_LEN: usize = TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len();

/// Estimated rows of some records at each profile width.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TranscriptExtentProfile {
    rows: [u64; PROFILE_LEN],
}

impl TranscriptExtentProfile {
    pub fn new(rows: [u64; PROFILE_LEN]) -> Self {
        let mut rows = rows;
        for index in 1..rows.len() {
            rows[index] = rows[index].min(rows[index - 1]);
        }
        Self { rows }
    }

    pub fn rows(self) -> [u64; PROFILE_LEN] {
        self.rows
    }

    /// Rows at the widest profile width that fits `width`.
    ///
    /// Snapping (instead of interpolating) keeps estimates additive: the rows of
    /// a range always equal the sum of its records' rows.
    pub fn estimated_rows(self, width: u16) -> u64 {
        self.rows[profile_bucket(width)]
    }

    pub(crate) fn add(self, other: Self) -> Self {
        Self {
            rows: std::array::from_fn(|i| self.rows[i].saturating_add(other.rows[i])),
        }
    }

    pub(crate) fn sub(self, other: Self) -> Self {
        Self {
            rows: std::array::from_fn(|i| self.rows[i].saturating_sub(other.rows[i])),
        }
    }
}

pub(crate) fn profile_bucket(width: u16) -> usize {
    TRANSCRIPT_EXTENT_PROFILE_WIDTHS
        .iter()
        .rposition(|bucket| *bucket <= width)
        .unwrap_or(0)
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

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StoredTranscriptBlock {
    pub block_idx: u64,
    pub history_idx: Option<u64>,
    pub kind: String,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub content_hash: String,
    pub estimated_text_bytes: u64,
    /// Encoded compact record; see [`TranscriptRecordSummary`].
    #[serde(default)]
    pub summary: Vec<u8>,
    pub indexed_text: String,
    pub block_json: String,
    pub origin_json: Option<String>,
    pub tool_state_json: Option<String>,
    #[serde(default)]
    pub tool_render_revision: u64,
}

impl StoredTranscriptBlock {
    pub fn extent_profile(&self) -> TranscriptExtentProfile {
        transcript_text_extent_profile(
            &self.kind,
            self.preview_text(),
            &self.indexed_text,
            self.estimated_text_bytes,
        )
    }

    /// The start of the search text, at most [`PREVIEW_MAX_BYTES`].
    pub fn preview_text(&self) -> &str {
        smelt_buffer::text::grapheme_prefix(&self.indexed_text, PREVIEW_MAX_BYTES)
    }

    pub(crate) fn first_line(&self) -> String {
        self.preview_text()
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .chars()
            .take(512)
            .collect()
    }
}

pub const PREVIEW_MAX_BYTES: usize = 512;

/// The `payload` column: block, origin and tool state as one line of compact
/// JSON each, an empty line for a missing value, compressed together.
pub(crate) fn encode_payload(
    block_json: &str,
    origin_json: Option<&str>,
    tool_state_json: Option<&str>,
) -> Result<Vec<u8>> {
    let lines = [Some(block_json), origin_json, tool_state_json];
    if lines.iter().flatten().any(|line| line.contains('\n')) {
        return Err(StoreError::Integrity(
            "transcript payload JSON must be compact".to_owned(),
        ));
    }
    let text = lines.map(Option::unwrap_or_default).join("\n");
    Ok(db::encode_blob(text.as_bytes()))
}

/// Block, origin and tool state JSON from a `payload` column.
pub(crate) fn decode_payload(blob: &[u8]) -> Result<(String, Option<String>, Option<String>)> {
    let text = String::from_utf8(db::decode_blob(blob)?.into_owned())
        .map_err(|_| StoreError::Integrity("transcript payload is not UTF-8".to_owned()))?;
    let mut lines = text.split('\n').map(str::to_owned);
    let (Some(block), Some(origin), Some(tool_state), None) =
        (lines.next(), lines.next(), lines.next(), lines.next())
    else {
        return Err(StoreError::Integrity(
            "transcript payload must have three lines".to_owned(),
        ));
    };
    let optional = |line: String| (!line.is_empty()).then_some(line);
    Ok((block, optional(origin), optional(tool_state)))
}

/// Navigation role of a transcript record kind.
pub fn transcript_role(kind: &str) -> &'static str {
    match kind {
        "user" => "user",
        "mode" => "mode",
        "process_status" => "process_status",
        _ => "assistant",
    }
}

/// The compact form of one record, read without its block, tool state or
/// indexed text. The store keeps `summary` opaque; the transcript model encodes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptRecordSummary {
    pub block_idx: u64,
    pub summary: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptRecordSlice {
    pub start: TranscriptRecordOffset,
    pub total_count: usize,
    pub records: Vec<TranscriptRecordSummary>,
}

impl TranscriptRecordSlice {
    pub fn new(
        start: TranscriptRecordOffset,
        total_count: usize,
        records: Vec<TranscriptRecordSummary>,
    ) -> Self {
        Self {
            start,
            total_count,
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

    pub fn into_records(self) -> Vec<TranscriptRecordSummary> {
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

fn estimated_text_row_profile(text: &str) -> [u64; PROFILE_LEN] {
    let mut rows = [0_u64; PROFILE_LEN];
    for line in text.lines() {
        let cells = cell_width::text_width(line).max(1) as u64;
        for (total, width) in rows.iter_mut().zip(TRANSCRIPT_EXTENT_PROFILE_WIDTHS) {
            *total = total.saturating_add(cells.div_ceil(u64::from(width)));
        }
    }
    rows.map(|rows| rows.max(1))
}

/// Row estimate profile of one transcript record from its indexed text.
pub fn transcript_text_extent_profile(
    kind: &str,
    preview_text: &str,
    indexed_text: &str,
    estimated_text_bytes: u64,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extent_profile_estimates_shrink_with_width() {
        let profile = TranscriptExtentProfile::new([120, 80, 40, 30, 20, 10]);
        let estimates = [20, 30, 40, 60, 80, 120, 240].map(|width| profile.estimated_rows(width));
        assert!(estimates.windows(2).all(|pair| pair[0] >= pair[1]));
    }
}
