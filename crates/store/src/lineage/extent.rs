use super::*;

use crate::history::{
    transcript_record_extent_profile, TranscriptExtentProfile, TranscriptNavigationRecord,
    TranscriptRecordOffset, TranscriptRecordProfile, TranscriptRecordRange, TranscriptRowLocation,
    TRANSCRIPT_EXTENT_PROFILE_WIDTHS,
};

const TRANSCRIPT_KINDS: [&str; 10] = [
    "user",
    "mode",
    "process_status",
    "thinking",
    "assistant",
    "code",
    "tool",
    "exec",
    "compacted",
    "compaction_preview",
];
const TRANSCRIPT_ROLES: [&str; 4] = ["user", "assistant", "mode", "process_status"];

#[derive(Clone, Debug, Eq, PartialEq)]
struct TranscriptNodeProfile {
    record_count: u64,
    first_block_idx: u64,
    last_block_idx: u64,
    kind_mask: u16,
    role_mask: u8,
    extent: TranscriptExtentProfile,
    min_history_idx: Option<u64>,
    max_history_idx: Option<u64>,
}

impl TranscriptNodeProfile {
    fn from_record(profile: &TranscriptRecordProfile) -> Result<Self> {
        Ok(Self {
            record_count: 1,
            first_block_idx: profile.block_idx,
            last_block_idx: profile.block_idx,
            kind_mask: semantic_bit(&TRANSCRIPT_KINDS, &profile.kind, "transcript kind")?,
            role_mask: semantic_bit::<u8>(&TRANSCRIPT_ROLES, &profile.role, "transcript role")?,
            extent: profile.extent,
            min_history_idx: profile.history_idx,
            max_history_idx: profile.history_idx,
        })
    }

    fn append(&mut self, next: &Self) -> Result<()> {
        self.record_count = self
            .record_count
            .checked_add(next.record_count)
            .ok_or_else(|| StoreError::Integrity("transcript record count overflows u64".into()))?;
        self.first_block_idx = self.first_block_idx.min(next.first_block_idx);
        self.last_block_idx = self.last_block_idx.max(next.last_block_idx);
        self.kind_mask |= next.kind_mask;
        self.role_mask |= next.role_mask;
        self.extent = add_extent_profiles(self.extent, next.extent);
        self.min_history_idx = match (self.min_history_idx, next.min_history_idx) {
            (Some(current), Some(next)) => Some(current.min(next)),
            (bound, None) | (None, bound) => bound,
        };
        self.max_history_idx = match (self.max_history_idx, next.max_history_idx) {
            (Some(current), Some(next)) => Some(current.max(next)),
            (bound, None) | (None, bound) => bound,
        };
        Ok(())
    }

    fn contains_history_idx(&self, history_idx: u64) -> bool {
        self.min_history_idx
            .zip(self.max_history_idx)
            .is_some_and(|(min, max)| min <= history_idx && history_idx <= max)
    }
}

fn semantic_bit<T>(values: &[&str], value: &str, field: &str) -> Result<T>
where
    T: TryFrom<u16>,
{
    let index = values
        .iter()
        .position(|candidate| *candidate == value)
        .ok_or_else(|| StoreError::Integrity(format!("unknown {field} {value:?}")))?;
    T::try_from(1_u16 << index)
        .map_err(|_| StoreError::Integrity(format!("{field} bit exceeds its storage type")))
}

fn transcript_role(kind: &str) -> Result<&'static str> {
    match kind {
        "user" => Ok("user"),
        "mode" => Ok("mode"),
        "process_status" => Ok("process_status"),
        kind if TRANSCRIPT_KINDS.contains(&kind) => Ok("assistant"),
        other => Err(StoreError::Integrity(format!(
            "unknown transcript kind {other:?}"
        ))),
    }
}

fn first_preview_line(record: &StoredTranscriptBlock) -> String {
    record
        .preview_text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .chars()
        .take(512)
        .collect()
}

fn record_profile(record: &StoredTranscriptBlock) -> Result<TranscriptRecordProfile> {
    let role = transcript_role(&record.kind)?.to_owned();
    let profile = TranscriptRecordProfile {
        block_idx: record.block_idx,
        history_idx: record.history_idx,
        kind: record.kind.clone(),
        role,
        first_line: first_preview_line(record),
        estimated_text_bytes: record.estimated_text_bytes,
        extent: transcript_record_extent_profile(record),
    };
    let _ = TranscriptNodeProfile::from_record(&profile)?;
    Ok(profile)
}

pub(crate) fn install_transcript_record_profile(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadId,
    bytes: &[u8],
) -> Result<()> {
    let record = serde_json::from_slice::<StoredTranscriptBlock>(bytes)?;
    let expected = record_profile(&record)?;
    let rows = expected.extent.rows();
    conn.execute(
        "INSERT OR IGNORE INTO lineage_transcript_record_profiles (
             lineage_id, payload_id, block_idx, history_idx, kind, role, first_line,
             estimated_text_bytes, rows_20, rows_40, rows_80, rows_120, rows_160, rows_240
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        rusqlite::params![
            lineage.as_str(),
            payload.as_str(),
            checked_i64(expected.block_idx, "transcript profile block_idx")?,
            expected
                .history_idx
                .map(|value| checked_i64(value, "transcript profile history_idx"))
                .transpose()?,
            expected.kind,
            expected.role,
            expected.first_line,
            checked_i64(
                expected.estimated_text_bytes,
                "transcript profile estimated_text_bytes"
            )?,
            checked_i64(rows[0], "transcript profile rows 20")?,
            checked_i64(rows[1], "transcript profile rows 40")?,
            checked_i64(rows[2], "transcript profile rows 80")?,
            checked_i64(rows[3], "transcript profile rows 120")?,
            checked_i64(rows[4], "transcript profile rows 160")?,
            checked_i64(rows[5], "transcript profile rows 240")?,
        ],
    )?;
    let stored = load_record_profile(conn, lineage, payload)?;
    if stored != expected {
        return Err(StoreError::Integrity(format!(
            "transcript profile for payload {} conflicts with immutable content",
            payload.as_str()
        )));
    }
    Ok(())
}

fn load_record_profile(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadId,
) -> Result<TranscriptRecordProfile> {
    let row = conn
        .query_row(
            "SELECT block_idx, history_idx, kind, role, first_line, estimated_text_bytes,
                    rows_20, rows_40, rows_80, rows_120, rows_160, rows_240
             FROM lineage_transcript_record_profiles
             WHERE lineage_id = ?1 AND payload_id = ?2",
            (lineage.as_str(), payload.as_str()),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    [
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, i64>(10)?,
                        row.get::<_, i64>(11)?,
                    ],
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::MissingObject {
            reference: format!("transcript profile for payload {}", payload.as_str()),
        })?;
    let profile = TranscriptRecordProfile {
        block_idx: nonnegative_u64(row.0, "transcript profile block_idx")?,
        history_idx: row
            .1
            .map(|value| nonnegative_u64(value, "transcript profile history_idx"))
            .transpose()?,
        kind: row.2,
        role: row.3,
        first_line: row.4,
        estimated_text_bytes: nonnegative_u64(row.5, "transcript profile estimated_text_bytes")?,
        extent: TranscriptExtentProfile::new(validated_profile_rows(row.6, 1)?),
    };
    let _ = TranscriptNodeProfile::from_record(&profile)?;
    Ok(profile)
}

fn load_node_profile(
    conn: &Connection,
    lineage: &LineageId,
    node: &NodeId,
) -> Result<TranscriptNodeProfile> {
    let row = conn
        .query_row(
            "SELECT record_count, first_block_idx, last_block_idx, kind_mask, role_mask,
                    rows_20, rows_40, rows_80, rows_120, rows_160, rows_240,
                    min_history_idx, max_history_idx
             FROM lineage_transcript_extent_nodes
             WHERE lineage_id = ?1 AND node_id = ?2",
            (lineage.as_str(), node.as_str()),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    [
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, i64>(10)?,
                    ],
                    row.get::<_, Option<i64>>(11)?,
                    row.get::<_, Option<i64>>(12)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::MissingObject {
            reference: format!("transcript extent node {}", node.as_str()),
        })?;
    let record_count = nonnegative_u64(row.0, "transcript extent record count")?;
    if record_count == 0 {
        return Err(StoreError::Integrity(
            "transcript extent node has no records".into(),
        ));
    }
    let first_block_idx = nonnegative_u64(row.1, "transcript extent first block_idx")?;
    let last_block_idx = nonnegative_u64(row.2, "transcript extent last block_idx")?;
    let kind_mask = u16::try_from(row.3)
        .map_err(|_| StoreError::Integrity("transcript kind mask is invalid".into()))?;
    let role_mask = u8::try_from(row.4)
        .map_err(|_| StoreError::Integrity("transcript role mask is invalid".into()))?;
    if first_block_idx > last_block_idx || kind_mask == 0 || role_mask == 0 {
        return Err(StoreError::Integrity(
            "transcript extent node has invalid semantic bounds".into(),
        ));
    }
    let record_count_usize = usize::try_from(record_count).map_err(|_| {
        StoreError::Integrity("transcript extent record count exceeds platform limits".into())
    })?;
    let min_history_idx = row
        .6
        .map(|value| nonnegative_u64(value, "transcript extent min history_idx"))
        .transpose()?;
    let max_history_idx = row
        .7
        .map(|value| nonnegative_u64(value, "transcript extent max history_idx"))
        .transpose()?;
    if min_history_idx.is_some() != max_history_idx.is_some()
        || min_history_idx
            .zip(max_history_idx)
            .is_some_and(|(min, max)| min > max)
    {
        return Err(StoreError::Integrity(
            "transcript extent node has invalid history bounds".into(),
        ));
    }
    Ok(TranscriptNodeProfile {
        record_count,
        first_block_idx,
        last_block_idx,
        kind_mask,
        role_mask,
        extent: TranscriptExtentProfile::new(validated_profile_rows(row.5, record_count_usize)?),
        min_history_idx,
        max_history_idx,
    })
}

fn entry_profile(
    conn: &Connection,
    lineage: &LineageId,
    entry: &NodeEntry,
) -> Result<TranscriptNodeProfile> {
    match &entry.target {
        EntryTarget::Item(payload) => {
            TranscriptNodeProfile::from_record(&load_record_profile(conn, lineage, payload)?)
        }
        EntryTarget::Child(node) => load_node_profile(conn, lineage, node),
    }
}

fn profile_sequence_node(
    conn: &Connection,
    lineage: &LineageId,
    node: &SequenceNode,
) -> Result<TranscriptNodeProfile> {
    let mut profiles = node
        .entries
        .iter()
        .map(|entry| {
            let profile = entry_profile(conn, lineage, entry)?;
            if profile.record_count != entry.item_count {
                return Err(StoreError::Integrity(format!(
                    "transcript extent for node {} disagrees with its sequence entry",
                    node.id.as_str()
                )));
            }
            Ok(profile)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter();
    let mut expected = profiles.next().ok_or_else(|| {
        StoreError::Integrity("transcript sequence node has no extent entries".into())
    })?;
    for profile in profiles {
        expected.append(&profile)?;
    }
    if expected.record_count != node.item_count {
        return Err(StoreError::Integrity(format!(
            "transcript extent for node {} has the wrong record count",
            node.id.as_str()
        )));
    }
    Ok(expected)
}

pub(crate) fn install_transcript_node_profile(
    conn: &Connection,
    lineage: &LineageId,
    node: &SequenceNode,
) -> Result<()> {
    if node.kind != SequenceKind::Transcript {
        return Ok(());
    }
    let expected = profile_sequence_node(conn, lineage, node)?;
    let rows = expected.extent.rows();
    conn.execute(
        "INSERT OR IGNORE INTO lineage_transcript_extent_nodes (
             lineage_id, node_id, record_count, first_block_idx, last_block_idx,
             kind_mask, role_mask, rows_20, rows_40, rows_80, rows_120, rows_160, rows_240,
             min_history_idx, max_history_idx
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        rusqlite::params![
            lineage.as_str(),
            node.id.as_str(),
            checked_i64(expected.record_count, "transcript extent record count")?,
            checked_i64(
                expected.first_block_idx,
                "transcript extent first block_idx"
            )?,
            checked_i64(expected.last_block_idx, "transcript extent last block_idx")?,
            i64::from(expected.kind_mask),
            i64::from(expected.role_mask),
            checked_i64(rows[0], "transcript extent rows 20")?,
            checked_i64(rows[1], "transcript extent rows 40")?,
            checked_i64(rows[2], "transcript extent rows 80")?,
            checked_i64(rows[3], "transcript extent rows 120")?,
            checked_i64(rows[4], "transcript extent rows 160")?,
            checked_i64(rows[5], "transcript extent rows 240")?,
            expected
                .min_history_idx
                .map(|value| checked_i64(value, "transcript extent min history_idx"))
                .transpose()?,
            expected
                .max_history_idx
                .map(|value| checked_i64(value, "transcript extent max history_idx"))
                .transpose()?,
        ],
    )?;
    let stored = load_node_profile(conn, lineage, &node.id)?;
    if stored != expected {
        return Err(StoreError::Integrity(format!(
            "transcript extent node {} conflicts with immutable sequence content",
            node.id.as_str()
        )));
    }
    Ok(())
}

pub(crate) fn backfill_transcript_indexes(conn: &Connection) -> Result<()> {
    let mut payload_statement = conn.prepare(
        "SELECT lineage_id, payload_id
         FROM lineage_payload_object_refs
         WHERE payload_kind = 'transcript'
         ORDER BY lineage_id, payload_id",
    )?;
    let payload_rows = payload_statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let payloads = payload_rows.collect::<std::result::Result<Vec<_>, _>>()?;
    drop(payload_statement);
    for (lineage, payload) in payloads {
        let lineage = LineageId::from_hex(lineage)?;
        let payload = PayloadId::from_db(payload)?;
        let bytes = hydrate_payload(
            conn,
            &lineage,
            &payload,
            PayloadKind::Transcript,
            &mut OperationStats::default(),
        )?;
        install_transcript_record_profile(conn, &lineage, &payload, &bytes)?;
    }

    let mut node_statement = conn.prepare(
        "SELECT lineage_id, node_id
         FROM lineage_sequence_nodes
         WHERE sequence_kind = 'transcript'
         ORDER BY lineage_id, level, node_id",
    )?;
    let node_rows = node_statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let nodes = node_rows.collect::<std::result::Result<Vec<_>, _>>()?;
    drop(node_statement);
    for (lineage, node) in nodes {
        let lineage = LineageId::from_hex(lineage)?;
        let node = NodeId::from_db(node)?;
        let node = load_node_shallow(conn, &lineage, &node, None)?;
        install_transcript_node_profile(conn, &lineage, &node)?;
    }
    Ok(())
}

pub(crate) fn backfill_transcript_history_bounds(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT lineage_id, node_id
         FROM lineage_sequence_nodes
         WHERE sequence_kind = 'transcript'
         ORDER BY lineage_id, level, node_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let nodes = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);

    for (lineage, node) in nodes {
        let lineage = LineageId::from_hex(lineage)?;
        let node = NodeId::from_db(node)?;
        let node = load_node_shallow(conn, &lineage, &node, None)?;
        let profile = profile_sequence_node(conn, &lineage, &node)?;
        let updated = conn.execute(
            "UPDATE lineage_transcript_extent_nodes
             SET min_history_idx = ?3, max_history_idx = ?4
             WHERE lineage_id = ?1 AND node_id = ?2",
            rusqlite::params![
                lineage.as_str(),
                node.id.as_str(),
                profile
                    .min_history_idx
                    .map(|value| checked_i64(value, "transcript extent min history_idx"))
                    .transpose()?,
                profile
                    .max_history_idx
                    .map(|value| checked_i64(value, "transcript extent max history_idx"))
                    .transpose()?,
            ],
        )?;
        if updated != 1 {
            return Err(StoreError::Integrity(format!(
                "transcript extent node {} is missing during history-bound backfill",
                node.id.as_str()
            )));
        }
    }
    Ok(())
}

fn validate_node_profile(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &NodeId,
    expected_level: u32,
) -> Result<TranscriptNodeProfile> {
    let node = load_node_shallow(conn, lineage, node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != expected_level {
        return Err(StoreError::Integrity(format!(
            "transcript index reached invalid node {}",
            node_id.as_str()
        )));
    }
    let mut computed: Option<TranscriptNodeProfile> = None;
    for entry in &node.entries {
        let profile = match &entry.target {
            EntryTarget::Item(payload) => {
                TranscriptNodeProfile::from_record(&load_record_profile(conn, lineage, payload)?)?
            }
            EntryTarget::Child(child) => {
                validate_node_profile(conn, lineage, child, expected_level.saturating_sub(1))?
            }
        };
        if profile.record_count != entry.item_count {
            return Err(StoreError::Integrity(format!(
                "transcript node {} has an invalid indexed entry extent",
                node_id.as_str()
            )));
        }
        if let Some(current) = computed.as_mut() {
            current.append(&profile)?;
        } else {
            computed = Some(profile);
        }
    }
    let computed = computed.ok_or_else(|| {
        StoreError::Integrity(format!(
            "transcript node {} has no indexed entries",
            node_id.as_str()
        ))
    })?;
    if computed != load_node_profile(conn, lineage, node_id)? {
        return Err(StoreError::Integrity(format!(
            "transcript node {} has an invalid aggregate index",
            node_id.as_str()
        )));
    }
    Ok(computed)
}

pub(crate) fn validate_transcript_indexes(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
) -> Result<()> {
    validate_transcript_root(root)?;
    let Some(node) = &root.node_id else {
        if root.item_count == 0 {
            return Ok(());
        }
        return Err(StoreError::Integrity(
            "nonempty transcript root has no indexed node".into(),
        ));
    };
    let profile = validate_node_profile(conn, lineage, node, root.depth.saturating_sub(1))?;
    if profile.record_count != root.item_count {
        return Err(StoreError::Integrity(
            "transcript root and aggregate index disagree".into(),
        ));
    }
    Ok(())
}

fn add_extent_profiles(
    left: TranscriptExtentProfile,
    right: TranscriptExtentProfile,
) -> TranscriptExtentProfile {
    let mut rows = left.rows();
    for (target, source) in rows.iter_mut().zip(right.rows()) {
        *target = target.saturating_add(source);
    }
    TranscriptExtentProfile::new(rows)
}

fn validate_transcript_root(root: &SequenceRoot) -> Result<()> {
    if root.kind != SequenceKind::Transcript {
        return Err(StoreError::Integrity(
            "transcript extent query requires a transcript root".into(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_node_range_extent(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &NodeId,
    expected_level: u32,
    start: u64,
    end: u64,
    output: &mut TranscriptExtentProfile,
) -> Result<()> {
    if start >= end {
        return Ok(());
    }
    let node_profile = load_node_profile(conn, lineage, node_id)?;
    if start == 0 && end == node_profile.record_count {
        *output = add_extent_profiles(*output, node_profile.extent);
        return Ok(());
    }
    let node = load_node_shallow(conn, lineage, node_id, None)?;
    if node.kind != SequenceKind::Transcript
        || node.level != expected_level
        || node.item_count != node_profile.record_count
        || end > node.item_count
    {
        return Err(StoreError::Integrity(format!(
            "transcript extent traversal reached invalid node {}",
            node_id.as_str()
        )));
    }
    let mut entry_start = 0_u64;
    for entry in &node.entries {
        let entry_end = entry.cumulative_item_count;
        if start < entry_end && end > entry_start {
            let local_start = start.saturating_sub(entry_start);
            let local_end = end.min(entry_end).saturating_sub(entry_start);
            if local_start == 0 && local_end == entry.item_count {
                *output = add_extent_profiles(*output, entry_profile(conn, lineage, entry)?.extent);
            } else {
                let EntryTarget::Child(child) = &entry.target else {
                    return Err(StoreError::Integrity(
                        "partial transcript extent intersected one record".into(),
                    ));
                };
                add_node_range_extent(
                    conn,
                    lineage,
                    child,
                    expected_level.saturating_sub(1),
                    local_start,
                    local_end,
                    output,
                )?;
            }
        }
        entry_start = entry_end;
        if entry_start >= end {
            break;
        }
    }
    Ok(())
}

pub(crate) fn lineage_transcript_extent_profile(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    range: TranscriptRecordRange,
) -> Result<TranscriptExtentProfile> {
    validate_transcript_root(root)?;
    let start = (range.start().get() as u64).min(root.item_count);
    let end = (range.end().get() as u64).min(root.item_count).max(start);
    let mut profile = TranscriptExtentProfile::default();
    if let Some(node) = &root.node_id {
        add_node_range_extent(
            conn,
            lineage,
            node,
            root.depth.saturating_sub(1),
            start,
            end,
            &mut profile,
        )?;
    }
    Ok(profile)
}

pub(crate) fn lineage_transcript_estimated_rows(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    range: TranscriptRecordRange,
    width: u16,
) -> Result<u64> {
    let _perf = smelt_perf::perf::begin("store:extent:estimated_rows");
    smelt_perf::perf::record_value(
        "store:extent:estimated_rows:records",
        range.end().get().saturating_sub(range.start().get()) as u64,
    );
    Ok(lineage_transcript_extent_profile(conn, lineage, root, range)?.estimated_rows(width))
}

pub(crate) fn lineage_transcript_total_estimated_rows(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    width: u16,
) -> Result<u64> {
    validate_transcript_root(root)?;
    let Some(node) = &root.node_id else {
        return Ok(0);
    };
    let profile = load_node_profile(conn, lineage, node)?;
    if profile.record_count != root.item_count {
        return Err(StoreError::Integrity(
            "transcript root and extent index disagree".into(),
        ));
    }
    Ok(profile.extent.estimated_rows(width))
}

pub(crate) fn lineage_transcript_row_location(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    width: u16,
    row: u64,
) -> Result<Option<TranscriptRowLocation>> {
    validate_transcript_root(root)?;
    let Some(mut node_id) = root.node_id.clone() else {
        return Ok(None);
    };
    let mut expected_level = root.depth.saturating_sub(1);
    let mut record_base = 0_u64;
    let mut target_row = row;
    loop {
        let node = load_node_shallow(conn, lineage, &node_id, None)?;
        if node.kind != SequenceKind::Transcript || node.level != expected_level {
            return Err(StoreError::Integrity(
                "transcript row lookup reached an invalid node".into(),
            ));
        }
        let mut entry_start = 0_u64;
        let mut selected = None;
        for entry in &node.entries {
            let profile = entry_profile(conn, lineage, entry)?;
            let rows = profile.extent.estimated_rows(width).max(1);
            if target_row < rows || entry == node.entries.last().expect("nonempty node") {
                selected = Some((entry, profile, entry_start, rows));
                break;
            }
            target_row = target_row.saturating_sub(rows);
            entry_start = entry.cumulative_item_count;
        }
        let Some((entry, profile, entry_start, rows)) = selected else {
            return Err(StoreError::Integrity(
                "transcript row lookup selected no sequence entry".into(),
            ));
        };
        record_base = record_base.saturating_add(entry_start);
        match &entry.target {
            EntryTarget::Item(_) => {
                let record_index = usize::try_from(record_base).map_err(|_| {
                    StoreError::Integrity(
                        "transcript row lookup index exceeds platform limits".into(),
                    )
                })?;
                return Ok(Some(TranscriptRowLocation {
                    record_index: TranscriptRecordOffset::new(record_index),
                    row_offset: target_row.min(rows.saturating_sub(1)),
                }));
            }
            EntryTarget::Child(child) => {
                if profile.record_count != entry.item_count || expected_level == 0 {
                    return Err(StoreError::Integrity(
                        "transcript row lookup found an invalid child extent".into(),
                    ));
                }
                node_id = child.clone();
                expected_level -= 1;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum SemanticTarget<'a> {
    Kind { value: &'a str, bit: u16 },
    Role { value: &'a str, bit: u8 },
}

impl SemanticTarget<'_> {
    fn node_matches(self, profile: &TranscriptNodeProfile) -> bool {
        match self {
            Self::Kind { bit, .. } => profile.kind_mask & bit != 0,
            Self::Role { bit, .. } => profile.role_mask & bit != 0,
        }
    }

    fn record_matches(self, profile: &TranscriptRecordProfile) -> bool {
        match self {
            Self::Kind { value, .. } => profile.kind == value,
            Self::Role { value, .. } => profile.role == value,
        }
    }
}

fn navigation_record(
    record_index: u64,
    profile: TranscriptRecordProfile,
) -> Result<TranscriptNavigationRecord> {
    let record_index = usize::try_from(record_index).map_err(|_| {
        StoreError::Integrity("transcript navigation index exceeds platform limits".into())
    })?;
    Ok(TranscriptNavigationRecord {
        record_index: TranscriptRecordOffset::new(record_index),
        profile,
    })
}

fn previous_in_node(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &NodeId,
    expected_level: u32,
    before_or_at: u64,
    record_base: u64,
    target: SemanticTarget<'_>,
) -> Result<Option<TranscriptNavigationRecord>> {
    let node = load_node_shallow(conn, lineage, node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != expected_level {
        return Err(StoreError::Integrity(
            "transcript navigation reached an invalid node".into(),
        ));
    }
    for entry in node.entries.iter().rev() {
        let entry_start = entry.cumulative_item_count.saturating_sub(entry.item_count);
        if entry_start > before_or_at {
            continue;
        }
        let profile = entry_profile(conn, lineage, entry)?;
        if !target.node_matches(&profile) {
            continue;
        }
        match &entry.target {
            EntryTarget::Item(payload) => {
                let profile = load_record_profile(conn, lineage, payload)?;
                if target.record_matches(&profile) {
                    return navigation_record(record_base.saturating_add(entry_start), profile)
                        .map(Some);
                }
            }
            EntryTarget::Child(child) => {
                let child_limit = before_or_at
                    .saturating_sub(entry_start)
                    .min(entry.item_count.saturating_sub(1));
                if let Some(found) = previous_in_node(
                    conn,
                    lineage,
                    child,
                    expected_level.saturating_sub(1),
                    child_limit,
                    record_base.saturating_add(entry_start),
                    target,
                )? {
                    return Ok(Some(found));
                }
            }
        }
    }
    Ok(None)
}

fn next_in_node(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &NodeId,
    expected_level: u32,
    after_or_at: u64,
    record_base: u64,
    target: SemanticTarget<'_>,
) -> Result<Option<TranscriptNavigationRecord>> {
    let node = load_node_shallow(conn, lineage, node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != expected_level {
        return Err(StoreError::Integrity(
            "transcript navigation reached an invalid node".into(),
        ));
    }
    let mut entry_start = 0_u64;
    for entry in &node.entries {
        let entry_end = entry.cumulative_item_count;
        if entry_end <= after_or_at {
            entry_start = entry_end;
            continue;
        }
        let profile = entry_profile(conn, lineage, entry)?;
        if target.node_matches(&profile) {
            match &entry.target {
                EntryTarget::Item(payload) => {
                    let profile = load_record_profile(conn, lineage, payload)?;
                    if target.record_matches(&profile) {
                        return navigation_record(record_base.saturating_add(entry_start), profile)
                            .map(Some);
                    }
                }
                EntryTarget::Child(child) => {
                    let child_start = after_or_at.saturating_sub(entry_start);
                    if let Some(found) = next_in_node(
                        conn,
                        lineage,
                        child,
                        expected_level.saturating_sub(1),
                        child_start,
                        record_base.saturating_add(entry_start),
                        target,
                    )? {
                        return Ok(Some(found));
                    }
                }
            }
        }
        entry_start = entry_end;
    }
    Ok(None)
}

fn semantic_target<'a>(values: &[&str], value: &'a str, role: bool) -> Result<SemanticTarget<'a>> {
    let bit = semantic_bit::<u16>(
        values,
        value,
        if role {
            "transcript role"
        } else {
            "transcript kind"
        },
    )?;
    if role {
        Ok(SemanticTarget::Role {
            value,
            bit: u8::try_from(bit).map_err(|_| {
                StoreError::Integrity("transcript role bit exceeds its storage type".into())
            })?,
        })
    } else {
        Ok(SemanticTarget::Kind { value, bit })
    }
}

fn lineage_transcript_record_before(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    target: SemanticTarget<'_>,
    before_or_at: usize,
) -> Result<Option<TranscriptNavigationRecord>> {
    validate_transcript_root(root)?;
    let Some(node) = &root.node_id else {
        return Ok(None);
    };
    let before_or_at = (before_or_at as u64).min(root.item_count.saturating_sub(1));
    previous_in_node(
        conn,
        lineage,
        node,
        root.depth.saturating_sub(1),
        before_or_at,
        0,
        target,
    )
}

fn lineage_transcript_record_after(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    target: SemanticTarget<'_>,
    after_or_at: usize,
) -> Result<Option<TranscriptNavigationRecord>> {
    validate_transcript_root(root)?;
    let Some(node) = &root.node_id else {
        return Ok(None);
    };
    let after_or_at = after_or_at as u64;
    if after_or_at >= root.item_count {
        return Ok(None);
    }
    next_in_node(
        conn,
        lineage,
        node,
        root.depth.saturating_sub(1),
        after_or_at,
        0,
        target,
    )
}

pub(crate) fn lineage_transcript_record_before_kind(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    kind: &str,
    before_or_at: usize,
) -> Result<Option<TranscriptNavigationRecord>> {
    lineage_transcript_record_before(
        conn,
        lineage,
        root,
        semantic_target(&TRANSCRIPT_KINDS, kind, false)?,
        before_or_at,
    )
}

pub(crate) fn lineage_transcript_record_after_kind(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    kind: &str,
    after_or_at: usize,
) -> Result<Option<TranscriptNavigationRecord>> {
    lineage_transcript_record_after(
        conn,
        lineage,
        root,
        semantic_target(&TRANSCRIPT_KINDS, kind, false)?,
        after_or_at,
    )
}

pub(crate) fn lineage_transcript_record_before_role(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    role: &str,
    before_or_at: usize,
) -> Result<Option<TranscriptNavigationRecord>> {
    lineage_transcript_record_before(
        conn,
        lineage,
        root,
        semantic_target(&TRANSCRIPT_ROLES, role, true)?,
        before_or_at,
    )
}

pub(crate) fn lineage_transcript_record_after_role(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    role: &str,
    after_or_at: usize,
) -> Result<Option<TranscriptNavigationRecord>> {
    lineage_transcript_record_after(
        conn,
        lineage,
        root,
        semantic_target(&TRANSCRIPT_ROLES, role, true)?,
        after_or_at,
    )
}

fn record_index_for_block_in_node(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &NodeId,
    expected_level: u32,
    block_idx: u64,
    record_base: u64,
) -> Result<Option<u64>> {
    let node = load_node_shallow(conn, lineage, node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != expected_level {
        return Err(StoreError::Integrity(
            "transcript block lookup reached an invalid node".into(),
        ));
    }
    let mut entry_start = 0_u64;
    for entry in &node.entries {
        let profile = entry_profile(conn, lineage, entry)?;
        if profile.first_block_idx <= block_idx && block_idx <= profile.last_block_idx {
            match &entry.target {
                EntryTarget::Item(payload) => {
                    if load_record_profile(conn, lineage, payload)?.block_idx == block_idx {
                        return Ok(Some(record_base.saturating_add(entry_start)));
                    }
                }
                EntryTarget::Child(child) => {
                    if let Some(index) = record_index_for_block_in_node(
                        conn,
                        lineage,
                        child,
                        expected_level.saturating_sub(1),
                        block_idx,
                        record_base.saturating_add(entry_start),
                    )? {
                        return Ok(Some(index));
                    }
                }
            }
        }
        entry_start = entry.cumulative_item_count;
    }
    Ok(None)
}

pub(crate) fn lineage_transcript_record_index_for_block_idx(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    block_idx: u64,
) -> Result<Option<usize>> {
    validate_transcript_root(root)?;
    let Some(node) = &root.node_id else {
        return Ok(None);
    };
    record_index_for_block_in_node(
        conn,
        lineage,
        node,
        root.depth.saturating_sub(1),
        block_idx,
        0,
    )?
    .map(|index| {
        usize::try_from(index).map_err(|_| {
            StoreError::Integrity("transcript block index exceeds platform limits".into())
        })
    })
    .transpose()
}

fn record_index_for_history_in_node(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &NodeId,
    expected_level: u32,
    history_idx: u64,
    record_base: u64,
    profiles_read: &mut u64,
) -> Result<Option<u64>> {
    let node = load_node_shallow(conn, lineage, node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != expected_level {
        return Err(StoreError::Integrity(
            "transcript history lookup reached an invalid node".into(),
        ));
    }
    let mut entry_start = 0_u64;
    for entry in &node.entries {
        *profiles_read = profiles_read.saturating_add(1);
        let profile = entry_profile(conn, lineage, entry)?;
        if profile.record_count != entry.item_count {
            return Err(StoreError::Integrity(
                "transcript history lookup reached an invalid indexed entry".into(),
            ));
        }
        if profile.contains_history_idx(history_idx) {
            match &entry.target {
                EntryTarget::Item(_) => {
                    return Ok(Some(record_base.saturating_add(entry_start)));
                }
                EntryTarget::Child(child) => {
                    if let Some(index) = record_index_for_history_in_node(
                        conn,
                        lineage,
                        child,
                        expected_level.saturating_sub(1),
                        history_idx,
                        record_base.saturating_add(entry_start),
                        profiles_read,
                    )? {
                        return Ok(Some(index));
                    }
                }
            }
        }
        entry_start = entry.cumulative_item_count;
    }
    Ok(None)
}

fn transcript_record_index_for_history_idx_profiled(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    history_idx: u64,
) -> Result<(Option<usize>, u64)> {
    validate_transcript_root(root)?;
    let Some(node) = &root.node_id else {
        return Ok((None, 0));
    };
    let mut profiles_read = 1;
    if !load_node_profile(conn, lineage, node)?.contains_history_idx(history_idx) {
        return Ok((None, profiles_read));
    }
    let record_index = record_index_for_history_in_node(
        conn,
        lineage,
        node,
        root.depth.saturating_sub(1),
        history_idx,
        0,
        &mut profiles_read,
    )?
    .map(|index| {
        usize::try_from(index).map_err(|_| {
            StoreError::Integrity("transcript record index exceeds platform limits".into())
        })
    })
    .transpose()?;
    Ok((record_index, profiles_read))
}

pub(crate) fn lineage_transcript_record_index_for_history_idx(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    history_idx: u64,
) -> Result<Option<usize>> {
    let (record_index, profiles_read) =
        transcript_record_index_for_history_idx_profiled(conn, lineage, root, history_idx)?;
    smelt_perf::perf::record_value("store:extent:history_lookup:profiles_read", profiles_read);
    Ok(record_index)
}

fn validated_profile_rows(
    rows: [i64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()],
    record_count: usize,
) -> Result<[u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()]> {
    let rows = rows.map(|value| {
        u64::try_from(value)
            .map_err(|_| StoreError::Integrity("transcript extent rows are negative".into()))
    });
    let rows = rows.into_iter().collect::<Result<Vec<_>>>()?;
    let rows: [u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()] = rows.try_into().map_err(|_| {
        StoreError::Integrity("transcript extent profile has the wrong width".into())
    })?;
    if rows.iter().any(|rows| *rows < record_count as u64)
        || !rows.windows(2).all(|pair| pair[0] >= pair[1])
    {
        return Err(StoreError::Integrity(
            "transcript extent profile is invalid".into(),
        ));
    }
    Ok(rows)
}

#[cfg(test)]
mod benchmark_tests {
    use super::*;

    const BENCHMARK_PAYLOAD_BASE: u64 = 0x1_0000_0000;
    const BENCHMARK_SEQUENCE_FANOUT: usize = 32;
    const BENCHMARK_OBJECT_HASH: &str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    #[derive(Clone)]
    struct BenchmarkNode {
        id: NodeId,
        level: u32,
        record_start: u64,
        item_count: u64,
        byte_count: u64,
    }

    struct SparseIndexTimings {
        previous_kind_us: Vec<u64>,
        next_role_us: Vec<u64>,
        block_lookup_us: Vec<u64>,
        row_lookup_us: Vec<u64>,
        extent_range_us: Vec<u64>,
        extent_total_us: Vec<u64>,
    }

    impl SparseIndexTimings {
        fn with_capacity(capacity: usize) -> Self {
            Self {
                previous_kind_us: Vec::with_capacity(capacity),
                next_role_us: Vec::with_capacity(capacity),
                block_lookup_us: Vec::with_capacity(capacity),
                row_lookup_us: Vec::with_capacity(capacity),
                extent_range_us: Vec::with_capacity(capacity),
                extent_total_us: Vec::with_capacity(capacity),
            }
        }
    }

    fn benchmark_counts() -> Vec<usize> {
        std::env::var("SMELT_SPARSE_INDEX_BENCH_COUNTS")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|count| count.trim().parse::<usize>().ok())
                    .filter(|count| *count >= 1_000)
                    .collect::<Vec<_>>()
            })
            .filter(|counts| !counts.is_empty())
            .unwrap_or_else(|| vec![10_000, 100_000, 1_000_000])
    }

    fn benchmark_runs() -> usize {
        std::env::var("SMELT_SPARSE_INDEX_BENCH_RUNS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|runs| *runs >= 100)
            .unwrap_or(1_001)
    }

    fn payload_id(index: u64) -> Result<PayloadId> {
        PayloadId::from_db(format!("{:064x}", BENCHMARK_PAYLOAD_BASE + index))
    }

    fn semantic_masks(record_start: u64, record_count: u64) -> (u16, u8) {
        let mut kind_mask = 0_u16;
        let mut role_mask = 0_u8;
        let semantic_span = record_count.min(TRANSCRIPT_KINDS.len() as u64);
        for offset in 0..semantic_span {
            let kind_index = ((record_start + offset) % TRANSCRIPT_KINDS.len() as u64) as usize;
            kind_mask |= 1_u16 << kind_index;
            let role_index = match kind_index {
                0 => 0,
                1 => 2,
                2 => 3,
                _ => 1,
            };
            role_mask |= 1_u8 << role_index;
        }
        (kind_mask, role_mask)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_benchmark_node(
        node_statement: &mut rusqlite::Statement<'_>,
        entry_statement: &mut rusqlite::Statement<'_>,
        extent_statement: &mut rusqlite::Statement<'_>,
        lineage: &LineageId,
        level: u32,
        record_start: u64,
        entries: Vec<NodeEntry>,
    ) -> Result<BenchmarkNode> {
        let entries = make_entries(entries)?;
        let item_count = entries
            .last()
            .expect("benchmark node entries")
            .cumulative_item_count;
        let byte_count = entries
            .last()
            .expect("benchmark node entries")
            .cumulative_byte_count;
        let id = node_id(
            lineage,
            SequenceKind::Transcript,
            level,
            &entries,
            item_count,
            byte_count,
        );
        node_statement.execute(rusqlite::params![
            lineage.as_str(),
            id.as_str(),
            if level == 0 { "leaf" } else { "internal" },
            i64::from(level),
            entries.len() as i64,
            item_count as i64,
            byte_count as i64,
        ])?;
        for (entry_index, entry) in entries.iter().enumerate() {
            let (entry_kind, payload_id, child_node_id) = match &entry.target {
                EntryTarget::Item(payload) => ("item", Some(payload.as_str()), None),
                EntryTarget::Child(child) => ("child", None, Some(child.as_str())),
            };
            entry_statement.execute(rusqlite::params![
                lineage.as_str(),
                id.as_str(),
                entry_index as i64,
                entry_kind,
                payload_id,
                child_node_id,
                entry.item_count as i64,
                entry.byte_count as i64,
                entry.cumulative_item_count as i64,
                entry.cumulative_byte_count as i64,
            ])?;
        }
        let (kind_mask, role_mask) = semantic_masks(record_start, item_count);
        let first_block_idx = record_start.saturating_mul(2);
        let last_block_idx = record_start
            .saturating_add(item_count)
            .saturating_sub(1)
            .saturating_mul(2);
        extent_statement.execute(rusqlite::params![
            lineage.as_str(),
            id.as_str(),
            item_count as i64,
            first_block_idx as i64,
            last_block_idx as i64,
            i64::from(kind_mask),
            i64::from(role_mask),
            record_start as i64,
            record_start.saturating_add(item_count).saturating_sub(1) as i64,
        ])?;
        Ok(BenchmarkNode {
            id,
            level,
            record_start,
            item_count,
            byte_count,
        })
    }

    fn build_sparse_index_fixture(
        database_path: &std::path::Path,
        record_count: usize,
    ) -> (Connection, LineageId, SequenceRoot) {
        let mut conn =
            Connection::open(database_path).expect("open sparse index benchmark database");
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;",
        )
        .expect("configure sparse index benchmark database");
        crate::schema::initialize_lineage_schema(&mut conn)
            .expect("initialize sparse index benchmark schema");
        let lineage = LineageId::from_hex("1".repeat(32)).expect("benchmark lineage id");
        conn.execute(
            "INSERT INTO lineage_identity (singleton, lineage_id, created_at)
             VALUES (1, ?1, 1)",
            [lineage.as_str()],
        )
        .expect("insert sparse index benchmark lineage");
        conn.execute(
            "INSERT INTO objects (hash, codec, raw_size, stored_size, bytes)
             VALUES (?1, 'none', 0, 0, X'')",
            [BENCHMARK_OBJECT_HASH],
        )
        .expect("insert sparse index benchmark object");

        let lineage_id = lineage.as_str();
        let tx = conn.transaction().expect("start sparse index fixture");
        tx.execute_batch(&format!(
            "WITH RECURSIVE records(record_index) AS (
                 VALUES(0)
                 UNION ALL
                 SELECT record_index + 1 FROM records
                 WHERE record_index + 1 < {record_count}
             )
             INSERT INTO lineage_payload_object_refs (
                 lineage_id, payload_id, payload_kind, object_hash, byte_count
             )
             SELECT '{lineage_id}', printf('%064x', {BENCHMARK_PAYLOAD_BASE} + record_index),
                    'transcript', '{BENCHMARK_OBJECT_HASH}', 1
             FROM records;

             WITH RECURSIVE records(record_index) AS (
                 VALUES(0)
                 UNION ALL
                 SELECT record_index + 1 FROM records
                 WHERE record_index + 1 < {record_count}
             )
             INSERT INTO lineage_transcript_record_profiles (
                 lineage_id, payload_id, block_idx, history_idx, kind, role, first_line,
                 estimated_text_bytes, rows_20, rows_40, rows_80, rows_120, rows_160, rows_240
             )
             SELECT '{lineage_id}', printf('%064x', {BENCHMARK_PAYLOAD_BASE} + record_index),
                    record_index * 2, record_index,
                    CASE record_index % 10
                        WHEN 0 THEN 'user'
                        WHEN 1 THEN 'mode'
                        WHEN 2 THEN 'process_status'
                        WHEN 3 THEN 'thinking'
                        WHEN 4 THEN 'assistant'
                        WHEN 5 THEN 'code'
                        WHEN 6 THEN 'tool'
                        WHEN 7 THEN 'exec'
                        WHEN 8 THEN 'compacted'
                        ELSE 'compaction_preview'
                    END,
                    CASE record_index % 10
                        WHEN 0 THEN 'user'
                        WHEN 1 THEN 'mode'
                        WHEN 2 THEN 'process_status'
                        ELSE 'assistant'
                    END,
                    printf('record %d', record_index), 1, 1, 1, 1, 1, 1, 1
             FROM records;"
        ))
        .expect("insert sparse index benchmark profiles");

        let root = {
            let mut node_statement = tx
                .prepare(
                    "INSERT INTO lineage_sequence_nodes (
                         lineage_id, node_id, sequence_kind, node_kind, level,
                         entry_count, item_count, byte_count
                     ) VALUES (?1, ?2, 'transcript', ?3, ?4, ?5, ?6, ?7)",
                )
                .expect("prepare benchmark node insert");
            let mut entry_statement = tx
                .prepare(
                    "INSERT INTO lineage_sequence_entries (
                         lineage_id, node_id, entry_index, entry_kind, payload_id, child_node_id,
                         item_count, byte_count, cumulative_item_count, cumulative_byte_count
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                )
                .expect("prepare benchmark entry insert");
            let mut extent_statement = tx
                .prepare(
                    "INSERT INTO lineage_transcript_extent_nodes (
                         lineage_id, node_id, record_count, first_block_idx, last_block_idx,
                         kind_mask, role_mask, rows_20, rows_40, rows_80,
                         rows_120, rows_160, rows_240, min_history_idx, max_history_idx
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?3, ?3, ?3, ?3, ?3, ?3, ?8, ?9)",
                )
                .expect("prepare benchmark extent insert");

            let mut nodes = Vec::with_capacity(record_count.div_ceil(BENCHMARK_SEQUENCE_FANOUT));
            for record_start in (0..record_count).step_by(BENCHMARK_SEQUENCE_FANOUT) {
                let record_end = record_start
                    .saturating_add(BENCHMARK_SEQUENCE_FANOUT)
                    .min(record_count);
                let entries = (record_start..record_end)
                    .map(|index| {
                        Ok(NodeEntry {
                            target: EntryTarget::Item(payload_id(index as u64)?),
                            item_count: 1,
                            byte_count: 1,
                            cumulative_item_count: 0,
                            cumulative_byte_count: 0,
                        })
                    })
                    .collect::<Result<Vec<_>>>()
                    .expect("build benchmark leaf entries");
                nodes.push(
                    insert_benchmark_node(
                        &mut node_statement,
                        &mut entry_statement,
                        &mut extent_statement,
                        &lineage,
                        0,
                        record_start as u64,
                        entries,
                    )
                    .expect("insert benchmark leaf"),
                );
            }

            while nodes.len() > 1 {
                let level = nodes[0].level.saturating_add(1);
                let mut parents =
                    Vec::with_capacity(nodes.len().div_ceil(BENCHMARK_SEQUENCE_FANOUT));
                for children in nodes.chunks(BENCHMARK_SEQUENCE_FANOUT) {
                    let record_start = children[0].record_start;
                    let entries = children
                        .iter()
                        .map(|child| NodeEntry {
                            target: EntryTarget::Child(child.id.clone()),
                            item_count: child.item_count,
                            byte_count: child.byte_count,
                            cumulative_item_count: 0,
                            cumulative_byte_count: 0,
                        })
                        .collect();
                    parents.push(
                        insert_benchmark_node(
                            &mut node_statement,
                            &mut entry_statement,
                            &mut extent_statement,
                            &lineage,
                            level,
                            record_start,
                            entries,
                        )
                        .expect("insert benchmark internal node"),
                    );
                }
                nodes = parents;
            }

            let node = nodes.pop().expect("benchmark root node");
            let depth = node.level.saturating_add(1);
            let id = root_id(
                &lineage,
                SequenceKind::Transcript,
                Some(&node.id),
                depth,
                node.item_count,
                node.byte_count,
            );
            let root = SequenceRoot {
                id,
                kind: SequenceKind::Transcript,
                node_id: Some(node.id),
                depth,
                item_count: node.item_count,
                byte_count: node.byte_count,
            };
            tx.execute(
                "INSERT INTO lineage_sequence_roots (
                     lineage_id, root_id, root_kind, root_node_id, depth, item_count, byte_count
                 ) VALUES (?1, ?2, 'transcript', ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    lineage.as_str(),
                    root.id.as_str(),
                    root.node_id.as_ref().map(NodeId::as_str),
                    i64::from(root.depth),
                    root.item_count as i64,
                    root.byte_count as i64,
                ],
            )
            .expect("insert sparse index benchmark root");
            root
        };
        tx.commit().expect("commit sparse index fixture");
        (conn, lineage, root)
    }

    fn elapsed_us(started_at: std::time::Instant) -> u64 {
        u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    fn p99(samples: &mut [u64]) -> u64 {
        samples.sort_unstable();
        let rank = samples.len().saturating_mul(99).div_ceil(100).max(1);
        samples[rank - 1]
    }

    fn payloads_loaded(snapshot: &smelt_perf::perf::Snapshot) -> u64 {
        snapshot
            .values
            .iter()
            .find(|row| row.label == "store:object:payloads_loaded")
            .map_or(0, |row| row.total)
    }

    #[test]
    fn history_lookup_prunes_unrelated_transcript_subtrees() {
        const RECORD_COUNT: usize = 4_096;
        let temp = tempfile::tempdir().expect("sparse history lookup directory");
        let (conn, lineage, root) =
            build_sparse_index_fixture(&temp.path().join("lineage.db"), RECORD_COUNT);
        let target = RECORD_COUNT / 2 + 7;

        let (found, profiles_read) =
            transcript_record_index_for_history_idx_profiled(&conn, &lineage, &root, target as u64)
                .expect("lookup transcript history coordinate");

        assert_eq!(found, Some(target));
        assert!(
            profiles_read
                <= u64::from(root.depth).saturating_mul(BENCHMARK_SEQUENCE_FANOUT as u64 + 1),
            "history lookup read {profiles_read} profiles at depth {}",
            root.depth
        );
    }

    #[test]
    #[ignore = "manual sparse index scaling benchmark"]
    fn sparse_index_scaling_benchmark_suite() {
        if std::env::var("SMELT_SPARSE_INDEX_BENCH").as_deref() != Ok("1") {
            eprintln!("SPARSE_INDEX_BENCH_SKIPPED");
            return;
        }
        let runs = benchmark_runs();
        for record_count in benchmark_counts() {
            let temp = tempfile::tempdir().expect("sparse index benchmark directory");
            let fixture_started_at = std::time::Instant::now();
            let (conn, lineage, root) =
                build_sparse_index_fixture(&temp.path().join("lineage.db"), record_count);
            let fixture_ms = fixture_started_at.elapsed().as_millis();

            let midpoint = record_count / 2;
            assert!(lineage_transcript_record_before_kind(
                &conn, &lineage, &root, "tool", midpoint,
            )
            .expect("warm previous-kind navigation")
            .is_some());
            assert!(
                lineage_transcript_record_after_role(&conn, &lineage, &root, "user", midpoint,)
                    .expect("warm next-role navigation")
                    .is_some()
            );
            assert_eq!(
                lineage_transcript_total_estimated_rows(&conn, &lineage, &root, 80)
                    .expect("warm total extent"),
                record_count as u64
            );

            smelt_perf::perf::clear();
            smelt_perf::perf::set_enabled(true);
            let mut timings = SparseIndexTimings::with_capacity(runs);
            let mut random = 0x9e37_79b9_u64;
            for _ in 0..runs {
                random = random
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let index = 10 + random as usize % record_count.saturating_sub(20);

                let started_at = std::time::Instant::now();
                assert!(lineage_transcript_record_before_kind(
                    &conn, &lineage, &root, "tool", index,
                )
                .expect("previous-kind navigation")
                .is_some());
                timings.previous_kind_us.push(elapsed_us(started_at));

                let started_at = std::time::Instant::now();
                assert!(lineage_transcript_record_after_role(
                    &conn, &lineage, &root, "user", index,
                )
                .expect("next-role navigation")
                .is_some());
                timings.next_role_us.push(elapsed_us(started_at));

                let started_at = std::time::Instant::now();
                assert_eq!(
                    lineage_transcript_record_index_for_block_idx(
                        &conn,
                        &lineage,
                        &root,
                        (index as u64).saturating_mul(2),
                    )
                    .expect("block-index lookup"),
                    Some(index)
                );
                timings.block_lookup_us.push(elapsed_us(started_at));

                let started_at = std::time::Instant::now();
                let row_location =
                    lineage_transcript_row_location(&conn, &lineage, &root, 80, index as u64)
                        .expect("row lookup")
                        .expect("row location");
                assert_eq!(row_location.record_index.get(), index);
                assert_eq!(row_location.row_offset, 0);
                timings.row_lookup_us.push(elapsed_us(started_at));

                let range_end = index.saturating_add(129).min(record_count);
                let started_at = std::time::Instant::now();
                let profile = lineage_transcript_extent_profile(
                    &conn,
                    &lineage,
                    &root,
                    (index..range_end).into(),
                )
                .expect("extent range lookup");
                assert_eq!(profile.estimated_rows(80), (range_end - index) as u64);
                timings.extent_range_us.push(elapsed_us(started_at));

                let started_at = std::time::Instant::now();
                assert_eq!(
                    lineage_transcript_total_estimated_rows(&conn, &lineage, &root, 80)
                        .expect("total extent lookup"),
                    record_count as u64
                );
                timings.extent_total_us.push(elapsed_us(started_at));
            }
            let snapshot = smelt_perf::perf::snapshot();
            smelt_perf::perf::set_enabled(false);
            assert_eq!(
                payloads_loaded(&snapshot),
                0,
                "sparse metadata queries must not hydrate transcript payloads"
            );

            let previous_kind_p99_us = p99(&mut timings.previous_kind_us);
            let next_role_p99_us = p99(&mut timings.next_role_us);
            let block_lookup_p99_us = p99(&mut timings.block_lookup_us);
            let row_lookup_p99_us = p99(&mut timings.row_lookup_us);
            let extent_range_p99_us = p99(&mut timings.extent_range_us);
            let extent_total_p99_us = p99(&mut timings.extent_total_us);
            assert!(
                previous_kind_p99_us < 2_000,
                "previous-kind navigation exceeded 2 ms p99 at {record_count} records: {previous_kind_p99_us} us"
            );
            assert!(
                next_role_p99_us < 2_000,
                "next-role navigation exceeded 2 ms p99 at {record_count} records: {next_role_p99_us} us"
            );
            println!(
                "SPARSE_INDEX_BENCH_JSON {}",
                serde_json::json!({
                    "records": record_count,
                    "runs": runs,
                    "fixture_ms": fixture_ms,
                    "previous_kind_p99_us": previous_kind_p99_us,
                    "next_role_p99_us": next_role_p99_us,
                    "block_lookup_p99_us": block_lookup_p99_us,
                    "row_lookup_p99_us": row_lookup_p99_us,
                    "extent_range_p99_us": extent_range_p99_us,
                    "extent_total_p99_us": extent_total_p99_us,
                    "payloads_loaded": payloads_loaded(&snapshot),
                    "database_bytes": std::fs::metadata(temp.path().join("lineage.db"))
                        .map(|metadata| metadata.len())
                        .unwrap_or_default(),
                })
            );
        }
    }
}
