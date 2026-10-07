use super::*;
use serde_json::Value;

pub(super) const SHARED_REVISION_STATE_VERSION: u32 = 2;

#[derive(serde::Deserialize)]
pub(super) struct RevisionStateFormat {
    pub(super) format_version: u32,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SharedRevisionState {
    pub(super) format_version: u32,
    pub(super) metadata: SessionMetadata,
    pub(super) archives: RevisionArchives,
    pub(super) first_user_message_root: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RevisionArchives {
    checkpoint: Option<String>,
    checkpoint_events: Option<String>,
    turn_metas: String,
    metadata_snapshots: String,
    context_snapshots: String,
}

impl RevisionArchives {
    fn roles(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("checkpoint", self.checkpoint.as_deref()),
            ("checkpoint_events", self.checkpoint_events.as_deref()),
            ("turn_metas", Some(self.turn_metas.as_str())),
            ("metadata_snapshots", Some(self.metadata_snapshots.as_str())),
            ("context_snapshots", Some(self.context_snapshots.as_str())),
        ]
        .into_iter()
        .filter_map(|(role, id)| id.map(|id| (role, id)))
    }
}

// A summary is a separate leaf so active checkpoints and timeline events share
// its bytes, and boundary searches need not read the summary.
#[derive(serde::Deserialize, serde::Serialize)]
struct CheckpointHeader {
    fields: Value,
    has_summary: bool,
}

fn checkpoint_items(value: &Value) -> Result<[Vec<u8>; 2]> {
    let summary = value.get("summary").and_then(Value::as_str);
    let fields = match (value, summary) {
        (Value::Object(fields), Some(_)) => Value::Object(
            fields
                .iter()
                .filter(|(key, _)| key.as_str() != "summary")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        _ => value.clone(),
    };
    Ok([
        serde_json::to_vec(&CheckpointHeader {
            fields,
            has_summary: summary.is_some(),
        })?,
        summary.unwrap_or_default().as_bytes().to_vec(),
    ])
}

pub(super) fn derive_checkpoint_summary_presence(bytes: &[u8]) -> Result<bool> {
    let header: CheckpointHeader = serde_json::from_slice(bytes)?;
    if header.has_summary && (!header.fields.is_object() || header.fields.get("summary").is_some())
    {
        return Err(StoreError::Integrity(
            "invalid checkpoint summary header".into(),
        ));
    }
    Ok(header.has_summary)
}

fn checkpoint_value(header: &[u8], summary: Vec<u8>) -> Result<Value> {
    let header: CheckpointHeader = serde_json::from_slice(header)?;
    let mut fields = header.fields;
    if header.has_summary {
        let object = fields.as_object_mut().ok_or_else(|| {
            StoreError::Integrity("checkpoint header with summary must be an object".into())
        })?;
        if object.contains_key("summary") {
            return Err(StoreError::Integrity(
                "checkpoint header contains an inline summary".into(),
            ));
        }
        let summary = String::from_utf8(summary)
            .map_err(|_| StoreError::Integrity("checkpoint summary is not UTF-8".into()))?;
        object.insert("summary".into(), Value::String(summary));
    } else if !summary.is_empty() {
        return Err(StoreError::Integrity(
            "checkpoint without summary has nonempty summary payload".into(),
        ));
    }
    Ok(fields)
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct MetadataHeader {
    index: HistoryIndex,
    fields: Value,
    has_first_user_message: bool,
}

pub(super) fn derive_archive_coordinates(
    kind: ArchiveHeaderKind,
    bytes: &[u8],
) -> Result<ArchiveCoordinates> {
    match kind {
        ArchiveHeaderKind::Metadata => Ok(ArchiveCoordinates::Metadata {
            index: serde_json::from_slice::<MetadataHeader>(bytes)?.index,
        }),
        ArchiveHeaderKind::Checkpoint => {
            let header: CheckpointHeader = serde_json::from_slice(bytes)?;
            Ok(ArchiveCoordinates::Checkpoint {
                first_live_index: ArchiveCoordinate::field(&header.fields, "first_live_index"),
                completed_at_history_len: ArchiveCoordinate::field(
                    &header.fields,
                    "completed_at_history_len",
                ),
                created_at_ms: ArchiveCoordinate::field(&header.fields, "created_at_ms"),
            })
        }
    }
}

#[derive(Clone, Copy)]
enum SideRowFormat {
    Json,
    Metadata,
}

impl SideRowFormat {
    fn index(self, bytes: &[u8]) -> Result<HistoryIndex> {
        match self {
            Self::Json => Ok(serde_json::from_slice(bytes)?),
            Self::Metadata => Ok(serde_json::from_slice::<MetadataHeader>(bytes)?.index),
        }
    }

    fn items(self, index: HistoryIndex, value: &Value) -> Result<[Vec<u8>; 2]> {
        if matches!(self, Self::Json) {
            return Ok([serde_json::to_vec(&index)?, serde_json::to_vec(value)?]);
        }
        let message = value.get("first_user_message").and_then(Value::as_str);
        let fields = match (value, message) {
            (Value::Object(fields), Some(_)) => Value::Object(
                fields
                    .iter()
                    .filter(|(key, _)| key.as_str() != "first_user_message")
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ),
            _ => value.clone(),
        };
        Ok([
            serde_json::to_vec(&MetadataHeader {
                index,
                fields,
                has_first_user_message: message.is_some(),
            })?,
            message.unwrap_or_default().as_bytes().to_vec(),
        ])
    }

    fn row(self, header: &[u8], body: Vec<u8>) -> Result<(HistoryIndex, Value)> {
        if matches!(self, Self::Json) {
            return Ok((
                serde_json::from_slice(header)?,
                serde_json::from_slice(&body)?,
            ));
        }
        let header: MetadataHeader = serde_json::from_slice(header)?;
        let mut fields = header.fields;
        if header.has_first_user_message {
            let object = fields.as_object_mut().ok_or_else(|| {
                StoreError::Integrity(
                    "metadata header with a first message must be an object".into(),
                )
            })?;
            if object.contains_key("first_user_message") {
                return Err(StoreError::Integrity(
                    "metadata header contains an inline first message".into(),
                ));
            }
            let message = String::from_utf8(body)
                .map_err(|_| StoreError::Integrity("metadata first message is not UTF-8".into()))?;
            object.insert("first_user_message".into(), Value::String(message));
        } else if !body.is_empty()
            || fields
                .get("first_user_message")
                .is_some_and(Value::is_string)
        {
            return Err(StoreError::Integrity(
                "metadata header without a first message has message content".into(),
            ));
        }
        Ok((header.index, fields))
    }
}

fn store_records(
    conn: &Connection,
    lineage: &LineageId,
    items: &[Vec<u8>],
    compression: ObjectCompression,
) -> Result<String> {
    let mut stats = OperationStats::default();
    let root = build_sequence_from_empty(
        conn,
        lineage,
        SequenceKind::Data,
        items,
        compression,
        &mut stats,
    )?;
    insert_root(conn, lineage, &root, &mut stats)?;
    Ok(root.id.0)
}

fn append_archive_records(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    items: &[Vec<u8>],
    compression: ObjectCompression,
    kind: ArchiveHeaderKind,
) -> Result<(SequenceRoot, OperationStats)> {
    let mut stats = OperationStats::default();
    let payloads = items.iter().enumerate().map(|(ordinal, bytes)| {
        let payload = put_payload(
            conn,
            lineage,
            PayloadKind::Data,
            bytes,
            compression,
            &mut stats,
        )?;
        if ordinal.is_multiple_of(2) {
            publish_archive_coordinates(conn, lineage, &payload, kind, bytes)?;
            if kind == ArchiveHeaderKind::Checkpoint {
                publish_checkpoint_summary_presence(conn, lineage, &payload, bytes)?;
            }
        }
        Ok(payload)
    });
    let (root, delta) = append_sequence_payloads_in(conn, lineage, root, payloads)?;
    merge_operation_stats(&mut stats, delta);
    Ok((root, stats))
}

fn store_checkpoint_records(
    conn: &Connection,
    lineage: &LineageId,
    items: &[Vec<u8>],
    compression: ObjectCompression,
) -> Result<String> {
    let empty = empty_sequence(conn, lineage, SequenceKind::Data)?;
    append_archive_records(
        conn,
        lineage,
        &empty,
        items,
        compression,
        ArchiveHeaderKind::Checkpoint,
    )
    .map(|(root, _)| root.id.0)
}

fn side_row_index(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    ordinal: u64,
    format: SideRowFormat,
    stats: &mut OperationStats,
) -> Result<HistoryIndex> {
    if matches!(format, SideRowFormat::Json) {
        let (bytes, delta) = sequence_item(conn, lineage, root, ordinal)?;
        merge_operation_stats(stats, delta);
        return format.index(&bytes);
    }
    let (payloads, delta) =
        sequence_payload_refs_from_root(conn, lineage, root, ordinal, ordinal + 1)?;
    merge_operation_stats(stats, delta);
    let payload = payloads
        .first()
        .ok_or_else(|| StoreError::Integrity("missing metadata header".into()))?;
    match archive_header_coordinates(conn, lineage, payload, ArchiveHeaderKind::Metadata, stats)? {
        ArchiveCoordinates::Metadata { index } => Ok(index),
        _ => Err(StoreError::Integrity(
            "invalid metadata header coordinates".into(),
        )),
    }
}

fn side_row_boundary(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    start: HistoryIndex,
    format: SideRowFormat,
    stats: &mut OperationStats,
) -> Result<u64> {
    let mut low = 0;
    let mut high = root.item_count / 2;
    if start == HistoryIndex::ZERO || high == 0 {
        return Ok(0);
    }
    let last_ordinal = high - 1;
    let last_index = side_row_index(conn, lineage, root, last_ordinal * 2, format, stats)?;
    if start >= last_index {
        return Ok(if start == last_index {
            last_ordinal * 2
        } else {
            root.item_count
        });
    }
    high = last_ordinal;
    while low < high {
        let mid = low + (high - low) / 2;
        let index = side_row_index(conn, lineage, root, mid * 2, format, stats)?;
        if index < start {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    Ok(low * 2)
}

fn replace_side_rows(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    start: HistoryIndex,
    rows: &[(HistoryIndex, Value)],
    compression: ObjectCompression,
    format: SideRowFormat,
) -> Result<(SequenceRoot, OperationStats)> {
    if root.kind != SequenceKind::Data || !root.item_count.is_multiple_of(2) {
        return Err(StoreError::Integrity(
            "invalid side-table archive root".into(),
        ));
    }
    let mut stats = OperationStats::default();
    let boundary = side_row_boundary(conn, lineage, root, start, format, &mut stats)?;
    let ((prefix, _), delta) = split_sequence_in(conn, lineage, root, boundary)?;
    merge_operation_stats(&mut stats, delta);
    let rows: BTreeMap<_, _> = rows
        .iter()
        .filter(|(index, _)| *index >= start)
        .map(|(index, value)| (*index, value))
        .collect();
    let mut items = Vec::with_capacity(rows.len().saturating_mul(2));
    for (index, value) in rows {
        items.extend(format.items(index, value)?);
    }
    let (root, delta) = match format {
        SideRowFormat::Json => append_sequence_in(conn, lineage, &prefix, &items, compression)?,
        SideRowFormat::Metadata => append_archive_records(
            conn,
            lineage,
            &prefix,
            &items,
            compression,
            ArchiveHeaderKind::Metadata,
        )?,
    };
    merge_operation_stats(&mut stats, delta);
    Ok((root, stats))
}

fn store_side_rows(
    conn: &Connection,
    lineage: &LineageId,
    previous: Option<&str>,
    start: HistoryIndex,
    rows: &[(HistoryIndex, Value)],
    compression: ObjectCompression,
    format: SideRowFormat,
) -> Result<String> {
    let root = match previous {
        Some(id) => archive_root(conn, lineage, "side_table", id)?,
        None => empty_sequence(conn, lineage, SequenceKind::Data)?,
    };
    replace_side_rows(conn, lineage, &root, start, rows, compression, format)
        .map(|(root, _)| root.id.0)
}

pub(super) fn shared_revision_state(
    conn: &Connection,
    lineage: &LineageId,
    mut metadata: SessionMetadata,
    side_tables: &SideTableSuffixes,
    previous: Option<&RevisionArchives>,
    compression: ObjectCompression,
) -> Result<SharedRevisionState> {
    let first_user_message_root = metadata
        .first_user_message
        .take()
        .map(|message| store_records(conn, lineage, &[message.into_bytes()], compression))
        .transpose()?;
    let archives = store_revision_archives(
        conn,
        lineage,
        &mut metadata,
        side_tables,
        previous,
        compression,
        SideRowFormat::Metadata,
    )?;
    Ok(SharedRevisionState {
        format_version: SHARED_REVISION_STATE_VERSION,
        metadata,
        archives,
        first_user_message_root,
    })
}

pub(super) fn prepare_compact_archives(
    conn: &Connection,
    lineage: &LineageId,
    metadata: SessionMetadata,
    edits: &crate::CompactSessionArchives,
    previous: Option<&SharedRevisionState>,
    history_len: u64,
    compression: ObjectCompression,
) -> Result<SharedRevisionState> {
    use crate::{CheckpointEdit, CheckpointEventsEdit, ValueEdit};
    let first_user_message_root = match &edits.first_user_message {
        ValueEdit::Retain => previous.and_then(|state| state.first_user_message_root.clone()),
        ValueEdit::Replace { value } => value
            .as_ref()
            .map(|message| {
                store_records(conn, lineage, &[message.as_bytes().to_vec()], compression)
            })
            .transpose()?,
    };
    let checkpoint = match &edits.checkpoint {
        CheckpointEdit::Retain => previous.and_then(|state| state.archives.checkpoint.clone()),
        CheckpointEdit::Replace { value } => value
            .as_ref()
            .map(|value| {
                store_checkpoint_records(conn, lineage, &checkpoint_items(value)?, compression)
            })
            .transpose()?,
        CheckpointEdit::ReplaceRecord { record } => {
            let empty = empty_sequence(conn, lineage, SequenceKind::Data)?;
            Some(append_checkpoint_records(
                conn,
                lineage,
                &empty,
                std::slice::from_ref(record),
                previous,
                None,
                compression,
            )?)
        }
    };
    if let Some(id) = &checkpoint {
        let root = archive_root(conn, lineage, "checkpoint", id)?;
        if let ArchiveCoordinates::Checkpoint {
            first_live_index: ArchiveCoordinate::Unsigned(index),
            ..
        } = checkpoint_coordinates(conn, lineage, &root, 0)?
        {
            if index > history_len {
                return Err(StoreError::Integrity(
                    "retained checkpoint exceeds final history length".into(),
                ));
            }
        }
    }
    let prior_events = previous.and_then(|state| state.archives.checkpoint_events.as_deref());
    let checkpoint_events = match &edits.checkpoint_events {
        CheckpointEventsEdit::Retain => {
            if let Some(id) = prior_events {
                checkpoint_archive_completion(
                    conn,
                    lineage,
                    &archive_root(conn, lineage, "checkpoint_events", id)?,
                    history_len,
                )?;
            }
            prior_events.map(str::to_owned)
        }
        CheckpointEventsEdit::Clear => None,
        CheckpointEventsEdit::ReplaceSuffix {
            retain_records,
            records,
        } => {
            let prefix = checkpoint_suffix_prefix(
                conn,
                lineage,
                prior_events,
                *retain_records,
                records.first(),
                history_len,
            )?;
            let mut items = Vec::with_capacity(records.len().saturating_mul(2));
            for event in records {
                items.extend(checkpoint_items(event)?);
            }
            let (root, _) = append_archive_records(
                conn,
                lineage,
                &prefix,
                &items,
                compression,
                ArchiveHeaderKind::Checkpoint,
            )?;
            Some(root.id.0)
        }
        CheckpointEventsEdit::ReplaceRecordsSuffix {
            retain_records,
            records,
        } => {
            let prefix = checkpoint_suffix_prefix(
                conn,
                lineage,
                prior_events,
                *retain_records,
                records.first().map(|record| &record.fields),
                history_len,
            )?;
            Some(append_checkpoint_records(
                conn,
                lineage,
                &prefix,
                records,
                previous,
                checkpoint.as_deref(),
                compression,
            )?)
        }
    };
    let archives = RevisionArchives {
        checkpoint,
        checkpoint_events,
        turn_metas: compact_json_archive(
            conn,
            lineage,
            previous.map(|state| state.archives.turn_metas.as_str()),
            &edits.turn_metas,
            "turn_metas",
            history_len,
            compression,
        )?,
        context_snapshots: compact_json_archive(
            conn,
            lineage,
            previous.map(|state| state.archives.context_snapshots.as_str()),
            &edits.context_snapshots,
            "context_snapshots",
            history_len,
            compression,
        )?,
        metadata_snapshots: compact_metadata_archive(
            conn,
            lineage,
            previous.map(|state| state.archives.metadata_snapshots.as_str()),
            &edits.metadata_snapshots,
            first_user_message_root.as_deref(),
            history_len,
            compression,
        )?,
    };
    Ok(SharedRevisionState {
        format_version: SHARED_REVISION_STATE_VERSION,
        metadata,
        archives,
        first_user_message_root,
    })
}

fn checkpoint_suffix_prefix(
    conn: &Connection,
    lineage: &LineageId,
    previous: Option<&str>,
    retain_records: u64,
    first: Option<&Value>,
    history_len: u64,
) -> Result<SequenceRoot> {
    let prefix =
        compact_archive_prefix(conn, lineage, previous, "checkpoint_events", retain_records)?;
    let completion = checkpoint_archive_completion(conn, lineage, &prefix, history_len)?;
    if completion
        .zip(
            first
                .and_then(|fields| fields.get("completed_at_history_len"))
                .and_then(Value::as_u64),
        )
        .is_some_and(|(last, first)| last > first)
    {
        return Err(StoreError::Integrity(
            "checkpoint suffix precedes retained completion boundary".into(),
        ));
    }
    Ok(prefix)
}

fn checkpoint_summary_payload(
    conn: &Connection,
    lineage: &LineageId,
    id: Option<&str>,
    role: &str,
    record: u64,
    stats: &mut OperationStats,
) -> Result<PayloadRef> {
    let id =
        id.ok_or_else(|| StoreError::Integrity("checkpoint summary source is absent".into()))?;
    let root = archive_root(conn, lineage, role, id)?;
    let start = record.checked_mul(2).ok_or_else(|| {
        StoreError::Integrity("checkpoint summary source ordinal overflow".into())
    })?;
    let end = start
        .checked_add(2)
        .filter(|end| *end <= root.item_count)
        .ok_or_else(|| {
            StoreError::Integrity("checkpoint summary source exceeds its archive".into())
        })?;
    let (payloads, delta) = sequence_payload_refs_from_root(conn, lineage, &root, start, end)?;
    merge_operation_stats(stats, delta);
    if payloads.len() != 2 || !checkpoint_summary_presence(conn, lineage, &payloads[0], stats)? {
        return Err(StoreError::Integrity(
            "checkpoint summary source has no string summary".into(),
        ));
    }
    Ok(payloads[1].clone())
}

fn append_checkpoint_records(
    conn: &Connection,
    lineage: &LineageId,
    prefix: &SequenceRoot,
    records: &[crate::CheckpointRecord],
    previous: Option<&SharedRevisionState>,
    checkpoint: Option<&str>,
    compression: ObjectCompression,
) -> Result<String> {
    use crate::CheckpointSummary;
    let mut stats = OperationStats::default();
    let mut payloads = Vec::with_capacity(records.len().saturating_mul(2));
    for record in records {
        let body = match &record.summary {
            CheckpointSummary::New { text } => put_payload(
                conn,
                lineage,
                PayloadKind::Data,
                text.as_bytes(),
                compression,
                &mut stats,
            )?,
            CheckpointSummary::BaseCheckpoint => checkpoint_summary_payload(
                conn,
                lineage,
                previous.and_then(|state| state.archives.checkpoint.as_deref()),
                "checkpoint",
                0,
                &mut stats,
            )?,
            CheckpointSummary::BaseEvent { record } => checkpoint_summary_payload(
                conn,
                lineage,
                previous.and_then(|state| state.archives.checkpoint_events.as_deref()),
                "checkpoint_events",
                *record,
                &mut stats,
            )?,
            CheckpointSummary::Checkpoint => {
                checkpoint_summary_payload(conn, lineage, checkpoint, "checkpoint", 0, &mut stats)?
            }
        };
        let header = serde_json::to_vec(&CheckpointHeader {
            fields: record.fields.clone(),
            has_summary: true,
        })?;
        let payload = put_payload(
            conn,
            lineage,
            PayloadKind::Data,
            &header,
            compression,
            &mut stats,
        )?;
        publish_archive_coordinates(
            conn,
            lineage,
            &payload,
            ArchiveHeaderKind::Checkpoint,
            &header,
        )?;
        publish_checkpoint_summary_presence(conn, lineage, &payload, &header)?;
        payloads.push(payload);
        payloads.push(body);
    }
    append_sequence_payloads_in(conn, lineage, prefix, payloads.into_iter().map(Ok))
        .map(|(root, _)| root.id.0)
}

fn compact_archive_prefix(
    conn: &Connection,
    lineage: &LineageId,
    previous: Option<&str>,
    role: &str,
    retain_records: u64,
) -> Result<SequenceRoot> {
    let root = match previous {
        Some(id) => archive_root(conn, lineage, role, id)?,
        None => empty_sequence(conn, lineage, SequenceKind::Data)?,
    };
    let boundary = retain_records
        .checked_mul(2)
        .ok_or_else(|| StoreError::Integrity("archive retained extent overflow".into()))?;
    if boundary > root.item_count {
        return Err(StoreError::Integrity(
            "archive suffix retains rows beyond its exact base".into(),
        ));
    }
    if boundary == root.item_count {
        return Ok(root);
    }
    split_sequence_in(conn, lineage, &root, boundary).map(|((prefix, _), _)| prefix)
}

fn compact_side_boundary(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    format: SideRowFormat,
    history_len: u64,
    first: Option<HistoryIndex>,
) -> Result<()> {
    if root.item_count == 0 {
        return Ok(());
    }
    let last = side_row_index(
        conn,
        lineage,
        root,
        root.item_count - 2,
        format,
        &mut OperationStats::default(),
    )?;
    if last.get() > history_len || first.is_some_and(|first| first <= last) {
        return Err(StoreError::Integrity(
            "archive suffix conflicts with retained history boundary".into(),
        ));
    }
    Ok(())
}

fn compact_json_archive(
    conn: &Connection,
    lineage: &LineageId,
    previous: Option<&str>,
    edit: &crate::ArchiveEdit<crate::ArchiveRow>,
    role: &str,
    history_len: u64,
    compression: ObjectCompression,
) -> Result<String> {
    use crate::ArchiveEdit;
    match edit {
        ArchiveEdit::Retain => {
            let root = match previous {
                Some(id) => archive_root(conn, lineage, role, id)?,
                None => empty_sequence(conn, lineage, SequenceKind::Data)?,
            };
            compact_side_boundary(conn, lineage, &root, SideRowFormat::Json, history_len, None)?;
            Ok(root.id.0)
        }
        ArchiveEdit::ReplaceSuffix {
            retain_records,
            records,
        } => {
            let prefix = compact_archive_prefix(conn, lineage, previous, role, *retain_records)?;
            compact_side_boundary(
                conn,
                lineage,
                &prefix,
                SideRowFormat::Json,
                history_len,
                records.first().map(|row| row.index),
            )?;
            let mut items = Vec::with_capacity(records.len().saturating_mul(2));
            for row in records {
                items.extend(SideRowFormat::Json.items(row.index, &row.value)?);
            }
            append_sequence_in(conn, lineage, &prefix, &items, compression)
                .map(|(root, _)| root.id.0)
        }
    }
}

fn checkpoint_coordinates(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    ordinal: u64,
) -> Result<ArchiveCoordinates> {
    let (payloads, _) = sequence_payload_refs_from_root(conn, lineage, root, ordinal, ordinal + 1)?;
    let payload = payloads
        .first()
        .ok_or_else(|| StoreError::Integrity("missing checkpoint header".into()))?;
    archive_header_coordinates(
        conn,
        lineage,
        payload,
        ArchiveHeaderKind::Checkpoint,
        &mut OperationStats::default(),
    )
}

fn checkpoint_archive_completion(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    history_len: u64,
) -> Result<Option<u64>> {
    if root.item_count == 0 {
        return Ok(None);
    }
    match checkpoint_coordinates(conn, lineage, root, root.item_count - 2)? {
        ArchiveCoordinates::Checkpoint {
            first_live_index: ArchiveCoordinate::Unsigned(first),
            completed_at_history_len: ArchiveCoordinate::Unsigned(completed),
            created_at_ms: ArchiveCoordinate::Unsigned(_),
        } if first <= completed && completed <= history_len => Ok(Some(completed)),
        _ => Err(StoreError::Integrity(
            "retained checkpoint event does not fit final history length".into(),
        )),
    }
}

fn compact_metadata_archive(
    conn: &Connection,
    lineage: &LineageId,
    previous: Option<&str>,
    edit: &crate::ArchiveEdit<crate::MetadataArchiveRow>,
    active_message: Option<&str>,
    history_len: u64,
    compression: ObjectCompression,
) -> Result<String> {
    use crate::{ArchiveEdit, MetadataMessage};
    let (retain_records, records) = match edit {
        ArchiveEdit::Retain => {
            let root = match previous {
                Some(id) => archive_root(conn, lineage, "metadata_snapshots", id)?,
                None => empty_sequence(conn, lineage, SequenceKind::Data)?,
            };
            compact_side_boundary(
                conn,
                lineage,
                &root,
                SideRowFormat::Metadata,
                history_len,
                None,
            )?;
            return Ok(root.id.0);
        }
        ArchiveEdit::ReplaceSuffix {
            retain_records,
            records,
        } => (*retain_records, records),
    };
    let prefix = compact_archive_prefix(
        conn,
        lineage,
        previous,
        "metadata_snapshots",
        retain_records,
    )?;
    compact_side_boundary(
        conn,
        lineage,
        &prefix,
        SideRowFormat::Metadata,
        history_len,
        records.first().map(|row| row.index),
    )?;
    let active_payload = if records
        .iter()
        .any(|row| matches!(row.message, MetadataMessage::Active))
    {
        active_message
            .map(|id| {
                let root = archive_root(conn, lineage, "first_user_message", id)?;
                let (mut payloads, _) =
                    sequence_payload_refs_from_root(conn, lineage, &root, 0, 1)?;
                payloads
                    .pop()
                    .ok_or_else(|| StoreError::Integrity("missing active message payload".into()))
            })
            .transpose()?
    } else {
        None
    };
    let mut stats = OperationStats::default();
    let mut payloads = Vec::with_capacity(records.len().saturating_mul(2));
    for row in records {
        let body = match &row.message {
            MetadataMessage::None => None,
            MetadataMessage::Active => active_payload.clone(),
            MetadataMessage::New { text } => Some(put_payload(
                conn,
                lineage,
                PayloadKind::Data,
                text.as_bytes(),
                compression,
                &mut stats,
            )?),
        };
        let header = serde_json::to_vec(&MetadataHeader {
            index: row.index,
            fields: row.fields.clone(),
            has_first_user_message: body.is_some(),
        })?;
        let payload = put_payload(
            conn,
            lineage,
            PayloadKind::Data,
            &header,
            compression,
            &mut stats,
        )?;
        publish_archive_coordinates(
            conn,
            lineage,
            &payload,
            ArchiveHeaderKind::Metadata,
            &header,
        )?;
        payloads.push(payload);
        payloads.push(match body {
            Some(body) => body,
            None => put_payload(
                conn,
                lineage,
                PayloadKind::Data,
                b"",
                compression,
                &mut stats,
            )?,
        });
    }
    append_sequence_payloads_in(conn, lineage, &prefix, payloads.into_iter().map(Ok))
        .map(|(root, _)| root.id.0)
}

pub(super) fn verify_revision_projection(
    conn: &Connection,
    lineage: &LineageId,
    original: &StoredRevisionState,
    payload: &PayloadId,
    projected: &SharedRevisionState,
) -> Result<()> {
    validate_shared_revision_archives(conn, lineage, payload, projected)?;
    let matches = match original {
        StoredRevisionState::Legacy(state) => {
            hydrate_shared_revision_state(conn, lineage, payload, projected.clone())? == *state
        }
        StoredRevisionState::Shared(_) => false,
    };
    if !matches {
        return Err(StoreError::Integrity(
            "revision projection differs from its original state".into(),
        ));
    }
    Ok(())
}

fn store_revision_archives(
    conn: &Connection,
    lineage: &LineageId,
    metadata: &mut SessionMetadata,
    side_tables: &SideTableSuffixes,
    previous: Option<&RevisionArchives>,
    compression: ObjectCompression,
    metadata_format: SideRowFormat,
) -> Result<RevisionArchives> {
    let checkpoint = metadata
        .checkpoint_json
        .take()
        .map(|value| {
            store_checkpoint_records(conn, lineage, &checkpoint_items(&value)?, compression)
        })
        .transpose()?;
    let checkpoint_events = metadata
        .checkpoint_events_json
        .take()
        .map(|events| {
            let events = events.as_array().ok_or_else(|| {
                StoreError::Integrity("checkpoint events must be an array".into())
            })?;
            let mut items = Vec::with_capacity(events.len().saturating_mul(2));
            for event in events {
                items.extend(checkpoint_items(event)?);
            }
            store_checkpoint_records(conn, lineage, &items, compression)
        })
        .transpose()?;
    Ok(RevisionArchives {
        checkpoint,
        checkpoint_events,
        turn_metas: store_side_rows(
            conn,
            lineage,
            previous.map(|archives| archives.turn_metas.as_str()),
            side_tables.start,
            &side_tables.turn_metas,
            compression,
            SideRowFormat::Json,
        )?,
        metadata_snapshots: store_side_rows(
            conn,
            lineage,
            previous.map(|archives| archives.metadata_snapshots.as_str()),
            side_tables.start,
            &side_tables.metadata_snapshots,
            compression,
            metadata_format,
        )?,
        context_snapshots: store_side_rows(
            conn,
            lineage,
            previous.map(|archives| archives.context_snapshots.as_str()),
            side_tables.start,
            &side_tables.context_snapshots,
            compression,
            SideRowFormat::Json,
        )?,
    })
}

fn archive_root(
    conn: &Connection,
    lineage: &LineageId,
    role: &str,
    id: &str,
) -> Result<SequenceRoot> {
    let root = load_root(conn, lineage, &RootId::from_db(id.to_owned())?)?;
    let valid_extent = match role {
        "first_user_message" => root.item_count == 1,
        "checkpoint" => root.item_count == 2,
        _ => root.item_count.is_multiple_of(2),
    };
    if root.kind != SequenceKind::Data || !valid_extent {
        return Err(StoreError::Integrity(format!(
            "invalid {role} archive root"
        )));
    }
    Ok(root)
}

pub(super) fn register_revision_archive_roots(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadId,
    bytes: &[u8],
) -> Result<()> {
    // Opaque lineage fixtures and legacy states have no archive-root envelope.
    let Ok(format) = serde_json::from_slice::<RevisionStateFormat>(bytes) else {
        return Ok(());
    };
    let (archives, message) = match format.format_version {
        SHARED_REVISION_STATE_VERSION => {
            let state: SharedRevisionState = serde_json::from_slice(bytes)?;
            validate_shared_metadata(conn, &state)?;
            (state.archives, state.first_user_message_root)
        }
        _ => return Ok(()),
    };
    for (role, id) in archives
        .roles()
        .chain(message.as_deref().map(|id| ("first_user_message", id)))
    {
        archive_root(conn, lineage, role, id)?;
        conn.execute(
            "INSERT OR IGNORE INTO lineage_revision_state_roots
             (lineage_id, state_payload_id, role, root_id) VALUES (?1, ?2, ?3, ?4)",
            (lineage.as_str(), payload.as_str(), role, id),
        )?;
    }
    verify_archive_ownership(conn, lineage, payload, &archives, message.as_deref())
}

fn validate_shared_metadata(conn: &Connection, state: &SharedRevisionState) -> Result<()> {
    if !crate::schema::has_shared_storage(conn)?
        || state.format_version != SHARED_REVISION_STATE_VERSION
        || state.metadata.first_user_message.is_some()
        || state.metadata.checkpoint_json.is_some()
        || state.metadata.checkpoint_events_json.is_some()
    {
        return Err(StoreError::Integrity(
            "invalid shared revision envelope".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_shared_revision_archives(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadId,
    state: &SharedRevisionState,
) -> Result<()> {
    validate_shared_metadata(conn, state)?;
    verify_archive_ownership(
        conn,
        lineage,
        payload,
        &state.archives,
        state.first_user_message_root.as_deref(),
    )?;
    for (role, id) in state.archives.roles().chain(
        state
            .first_user_message_root
            .as_deref()
            .map(|id| ("first_user_message", id)),
    ) {
        archive_root(conn, lineage, role, id)?;
    }
    Ok(())
}

pub(super) fn read_first_user_message(
    conn: &Connection,
    lineage: &LineageId,
    state: &SharedRevisionState,
    stats: &mut OperationStats,
) -> Result<Option<String>> {
    state
        .first_user_message_root
        .as_deref()
        .map(|id| {
            let root = archive_root(conn, lineage, "first_user_message", id)?;
            let (bytes, delta) = sequence_item(conn, lineage, &root, 0)?;
            merge_operation_stats(stats, delta);
            String::from_utf8(bytes)
                .map_err(|_| StoreError::Integrity("first user message is not UTF-8".into()))
        })
        .transpose()
}

fn verify_archive_ownership(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadId,
    archives: &RevisionArchives,
    message: Option<&str>,
) -> Result<()> {
    let rows = conn
        .prepare(
            "SELECT role, root_id FROM lineage_revision_state_roots
         WHERE lineage_id = ?1 AND state_payload_id = ?2",
        )?
        .query_map((lineage.as_str(), payload.as_str()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    let expected: BTreeMap<_, _> = archives
        .roles()
        .chain(message.map(|id| ("first_user_message", id)))
        .map(|(role, id)| (role.to_owned(), id.to_owned()))
        .collect();
    if rows != expected {
        return Err(StoreError::Integrity(
            "revision archive ownership differs from envelope".into(),
        ));
    }
    Ok(())
}

fn read_records(
    conn: &Connection,
    lineage: &LineageId,
    role: &str,
    id: &str,
) -> Result<Vec<Vec<u8>>> {
    let root = archive_root(conn, lineage, role, id)?;
    sequence_range(conn, lineage, &root, 0, root.item_count).map(|(items, _)| items)
}

fn read_checkpoints(
    conn: &Connection,
    lineage: &LineageId,
    role: &str,
    id: &str,
) -> Result<Vec<Value>> {
    let mut items = read_records(conn, lineage, role, id)?.into_iter();
    let mut values = Vec::new();
    while let Some(header) = items.next() {
        let summary = items
            .next()
            .ok_or_else(|| StoreError::Integrity("incomplete checkpoint pair".into()))?;
        values.push(checkpoint_value(&header, summary)?);
    }
    Ok(values)
}

fn read_side_rows(
    conn: &Connection,
    lineage: &LineageId,
    role: &str,
    id: &str,
    format: SideRowFormat,
) -> Result<Vec<(HistoryIndex, Value)>> {
    let mut items = read_records(conn, lineage, role, id)?.into_iter();
    let mut rows = Vec::new();
    while let Some(header) = items.next() {
        let body = items
            .next()
            .ok_or_else(|| StoreError::Integrity("incomplete side row pair".into()))?;
        let (index, value) = format.row(&header, body)?;
        if rows.last().is_some_and(|(previous, _)| *previous >= index) {
            return Err(StoreError::Integrity(
                "archive side rows are not strictly ordered".into(),
            ));
        }
        rows.push((index, value));
    }
    Ok(rows)
}

pub(super) fn hydrate_shared_revision_state(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadId,
    mut state: SharedRevisionState,
) -> Result<CanonicalRevisionState> {
    validate_shared_revision_archives(conn, lineage, payload, &state)?;
    state.metadata.first_user_message =
        read_first_user_message(conn, lineage, &state, &mut OperationStats::default())?;
    hydrate_archive_values(
        conn,
        lineage,
        state.metadata,
        state.archives,
        SideRowFormat::Metadata,
    )
}

fn hydrate_archive_values(
    conn: &Connection,
    lineage: &LineageId,
    mut metadata: SessionMetadata,
    archives: RevisionArchives,
    metadata_format: SideRowFormat,
) -> Result<CanonicalRevisionState> {
    metadata.checkpoint_json = archives
        .checkpoint
        .as_deref()
        .map(|id| {
            let mut values = read_checkpoints(conn, lineage, "checkpoint", id)?;
            values
                .pop()
                .ok_or_else(|| StoreError::Integrity("active checkpoint is missing".into()))
        })
        .transpose()?;
    metadata.checkpoint_events_json = archives
        .checkpoint_events
        .as_deref()
        .map(|id| read_checkpoints(conn, lineage, "checkpoint_events", id).map(Value::Array))
        .transpose()?;
    Ok(CanonicalRevisionState {
        format_version: LINEAGE_REVISION_STATE_VERSION,
        metadata,
        side_tables: SideTableSuffixes {
            start: HistoryIndex::ZERO,
            turn_metas: read_side_rows(
                conn,
                lineage,
                "turn_metas",
                &archives.turn_metas,
                SideRowFormat::Json,
            )?,
            metadata_snapshots: read_side_rows(
                conn,
                lineage,
                "metadata_snapshots",
                &archives.metadata_snapshots,
                metadata_format,
            )?,
            context_snapshots: read_side_rows(
                conn,
                lineage,
                "context_snapshots",
                &archives.context_snapshots,
                SideRowFormat::Json,
            )?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Connection, LineageId) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        create_lineage(&conn, &lineage, 1).unwrap();
        (conn, lineage)
    }

    fn metadata() -> SessionMetadata {
        serde_json::from_value(serde_json::json!({
            "session_cost_usd": 0.0, "updated_at": 1
        }))
        .unwrap()
    }

    fn install(
        conn: &mut Connection,
        lineage: &LineageId,
        metadata: &SessionMetadata,
        side: &SideTableSuffixes,
    ) -> (PayloadRef, Vec<u8>) {
        let tx = conn.transaction().unwrap();
        let bytes = prepare_revision_state(
            &tx,
            lineage,
            metadata,
            side,
            None,
            ObjectCompression::none(),
        )
        .unwrap()
        .bytes;
        let payload = put_payload(
            &tx,
            lineage,
            PayloadKind::RevisionState,
            &bytes,
            ObjectCompression::none(),
            &mut OperationStats::default(),
        )
        .unwrap();
        tx.commit().unwrap();
        (payload, bytes)
    }

    fn stored_bytes(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COALESCE(SUM(stored_size), 0) FROM objects",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn metadata_coordinate_boundaries_preserve_bulk_roots_and_replace_suffixes_without_header_reads(
    ) {
        for count in [1_u64, 32, 1025] {
            for size in [128, 262144, 1048576] {
                let (mut conn, lineage) = fixture();
                crate::schema::initialize_lineage_schema(&mut conn).unwrap();
                let rows: Vec<_> = (0..count).map(|index| {
                    (HistoryIndex::new(index * 2), serde_json::json!({
                        "unknown": if index == count / 2 || index == count - 1 { "x".repeat(size) } else { String::new() },
                    }))
                }).collect();
                let mut items = Vec::new();
                for (index, fields) in &rows {
                    items.extend(SideRowFormat::Metadata.items(*index, fields).unwrap());
                }
                let tx = conn.transaction().unwrap();
                let bulk = build_sequence_from_empty(
                    &tx,
                    &lineage,
                    SequenceKind::Data,
                    &items,
                    ObjectCompression::none(),
                    &mut OperationStats::default(),
                )
                .unwrap();
                assert_eq!(
                    tx.query_row(
                        "SELECT count(*) FROM lineage_archive_coordinates",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                    0,
                    "Data remains opaque, even for bytes that happen to match a header"
                );
                let id = store_side_rows(
                    &tx,
                    &lineage,
                    None,
                    HistoryIndex::ZERO,
                    &rows,
                    ObjectCompression::none(),
                    SideRowFormat::Metadata,
                )
                .unwrap();
                let root = archive_root(&tx, &lineage, "metadata_snapshots", &id).unwrap();
                assert_eq!(
                    root, bulk,
                    "coordinate projections must not change canonical roots"
                );
                tx.commit().unwrap();
                let ordinal = if count == 1 { 0 } else { count / 2 };
                let (headers, _) = sequence_payload_refs_from_root(
                    &conn,
                    &lineage,
                    &root,
                    ordinal * 2,
                    ordinal * 2 + 1,
                )
                .unwrap();
                let header = &headers[0];
                let stored: Vec<u8> = conn
                    .query_row(
                        "SELECT bytes FROM objects WHERE hash = ?1",
                        [&header.object_hash],
                        |row| row.get(0),
                    )
                    .unwrap();
                conn.execute(
                    "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
                    [&header.object_hash],
                )
                .unwrap();
                assert!(sequence_item(&conn, &lineage, &root, ordinal * 2).is_err());
                let tx = conn.transaction().unwrap();
                let mut stats = OperationStats::default();
                for (start, expected) in [
                    (HistoryIndex::ZERO, 0),
                    (HistoryIndex::new(ordinal * 2), ordinal * 2),
                    (HistoryIndex::new(ordinal * 2 + 1), (ordinal + 1) * 2),
                    (HistoryIndex::new(count * 2), count * 2),
                ] {
                    assert_eq!(
                        side_row_boundary(
                            &tx,
                            &lineage,
                            &root,
                            start,
                            SideRowFormat::Metadata,
                            &mut stats
                        )
                        .unwrap(),
                        expected
                    );
                }
                assert_eq!(
                    stats.payloads_read, 0,
                    "boundary search must not hydrate any header fields"
                );
                let changed_rows = vec![(
                    HistoryIndex::new(ordinal * 2),
                    serde_json::json!({"title": "changed"}),
                )];
                let (changed, stats) = replace_side_rows(
                    &tx,
                    &lineage,
                    &root,
                    HistoryIndex::new(ordinal * 2),
                    &changed_rows,
                    ObjectCompression::none(),
                    SideRowFormat::Metadata,
                )
                .unwrap();
                assert_eq!(stats.payloads_read, 0);
                let (old_prefix, _) =
                    sequence_payload_refs_from_root(&tx, &lineage, &root, 0, ordinal * 2).unwrap();
                let (new_prefix, _) =
                    sequence_payload_refs_from_root(&tx, &lineage, &changed, 0, ordinal * 2)
                        .unwrap();
                assert_eq!(old_prefix, new_prefix);
                let mut expected_items = items[..ordinal as usize * 2].to_vec();
                expected_items.extend(
                    SideRowFormat::Metadata
                        .items(changed_rows[0].0, &changed_rows[0].1)
                        .unwrap(),
                );
                let expected = build_sequence_from_empty(
                    &tx,
                    &lineage,
                    SequenceKind::Data,
                    &expected_items,
                    ObjectCompression::none(),
                    &mut OperationStats::default(),
                )
                .unwrap();
                assert_eq!(changed, expected);
                tx.commit().unwrap();
                conn.execute(
                    "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
                    (&stored, &header.object_hash),
                )
                .unwrap();
                verify_archive_coordinates(&conn, &lineage).unwrap();
                assert_eq!(
                    read_side_rows(
                        &conn,
                        &lineage,
                        "metadata_snapshots",
                        &root.id.0,
                        SideRowFormat::Metadata
                    )
                    .unwrap(),
                    rows
                );
            }
        }
    }

    #[test]
    fn metadata_message_records_preserve_shapes_and_reject_malformed_bodies() {
        let index = HistoryIndex::new(42);
        for value in [
            Value::Null,
            Value::from("legacy"),
            serde_json::json!({}),
            serde_json::json!({"first_user_message": null}),
            serde_json::json!({"first_user_message": false}),
            serde_json::json!({"first_user_message": ""}),
            serde_json::json!({"first_user_message": "quote\"\n\0 α 日本語", "unknown": [null, true, {"value": 1.25}]}),
        ] {
            let [header, body] = SideRowFormat::Metadata.items(index, &value).unwrap();
            assert_eq!(SideRowFormat::Metadata.index(&header).unwrap(), index);
            assert_eq!(
                SideRowFormat::Metadata.row(&header, body).unwrap(),
                (index, value)
            );
        }
        for (fields, has_message, body) in [
            (Value::Null, true, vec![]),
            (
                serde_json::json!({"first_user_message": "inline"}),
                true,
                vec![],
            ),
            (
                serde_json::json!({"first_user_message": "inline"}),
                false,
                vec![],
            ),
            (serde_json::json!({}), false, vec![b'x']),
            (serde_json::json!({}), true, vec![0xff]),
        ] {
            let header = serde_json::to_vec(&MetadataHeader {
                index,
                fields,
                has_first_user_message: has_message,
            })
            .unwrap();
            assert!(SideRowFormat::Metadata.row(&header, body).is_err());
        }
        let header = serde_json::json!({"index": 0, "fields": {}, "has_first_user_message": false, "unknown": true});
        assert!(SideRowFormat::Metadata
            .row(&serde_json::to_vec(&header).unwrap(), vec![])
            .is_err());
    }

    #[test]
    fn shared_message_bodies_roundtrip_and_do_not_repeat_on_title_saves() {
        for message in [
            None,
            Some(String::new()),
            Some("synthetic α 日本語\n\0".repeat(65536)),
        ] {
            let (mut conn, lineage) = fixture();
            crate::schema::initialize_lineage_schema(&mut conn).unwrap();
            let mut metadata = metadata();
            metadata.first_user_message = message.clone();
            let mut side = SideTableSuffixes {
                metadata_snapshots: vec![(
                    HistoryIndex::new(1),
                    serde_json::json!({"first_user_message": message, "unknown": [null, true, "α"]}),
                )],
                ..SideTableSuffixes::default()
            };
            let (payload, bytes) = install(&mut conn, &lineage, &metadata, &side);
            let state: SharedRevisionState = serde_json::from_slice(&bytes).unwrap();
            assert!(bytes.len() < 2048);
            assert!(state.metadata.first_user_message.is_none());
            assert_eq!(state.first_user_message_root.is_some(), message.is_some());
            if let Some(id) = &state.first_user_message_root {
                let active = archive_root(&conn, &lineage, "first_user_message", id).unwrap();
                let snapshots = archive_root(
                    &conn,
                    &lineage,
                    "metadata_snapshots",
                    &state.archives.metadata_snapshots,
                )
                .unwrap();
                let (active_refs, _) =
                    sequence_payload_refs_from_root(&conn, &lineage, &active, 0, 1).unwrap();
                let (snapshot_refs, _) =
                    sequence_payload_refs_from_root(&conn, &lineage, &snapshots, 1, 2).unwrap();
                assert_eq!(
                    active_refs, snapshot_refs,
                    "active and snapshot bodies must share identity"
                );
            }
            let hydrated =
                hydrate_shared_revision_state(&conn, &lineage, &payload.id, state).unwrap();
            assert_eq!(hydrated.metadata, metadata);
            assert_eq!(hydrated.side_tables, side);
            let before = stored_bytes(&conn);
            for title in 0..20 {
                metadata.title = Some(format!("title-{title}"));
                metadata.updated_at = title + 2;
                side.metadata_snapshots[0].1["title"] = Value::String(format!("title-{title}"));
                let (payload, bytes) = install(&mut conn, &lineage, &metadata, &side);
                assert!(bytes.len() < 2048);
                let state: SharedRevisionState = serde_json::from_slice(&bytes).unwrap();
                let hydrated =
                    hydrate_shared_revision_state(&conn, &lineage, &payload.id, state).unwrap();
                assert_eq!(hydrated.metadata, metadata);
                assert_eq!(hydrated.side_tables, side);
            }
            assert!(
                stored_bytes(&conn) - before < 20 * 4096,
                "unchanged first messages must not repeat physically"
            );
        }
    }

    #[test]
    fn shared_envelopes_reject_inline_messages_unknown_fields_and_invalid_utf8() {
        let (mut conn, lineage) = fixture();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let (_, bytes) = install(
            &mut conn,
            &lineage,
            &metadata(),
            &SideTableSuffixes::default(),
        );
        for (key, value) in [
            ("unknown", Value::Bool(true)),
            ("first_user_message_root", Value::from("invalid")),
        ] {
            let mut state: Value = serde_json::from_slice(&bytes).unwrap();
            state[key] = value;
            let tx = conn.transaction().unwrap();
            assert!(put_payload(
                &tx,
                &lineage,
                PayloadKind::RevisionState,
                &serde_json::to_vec(&state).unwrap(),
                ObjectCompression::none(),
                &mut OperationStats::default()
            )
            .is_err());
            tx.rollback().unwrap();
        }
        let tx = conn.transaction().unwrap();
        let mut state: SharedRevisionState = serde_json::from_slice(&bytes).unwrap();
        state.metadata.first_user_message = Some("inline".into());
        assert!(put_payload(
            &tx,
            &lineage,
            PayloadKind::RevisionState,
            &serde_json::to_vec(&state).unwrap(),
            ObjectCompression::none(),
            &mut OperationStats::default()
        )
        .is_err());
        tx.rollback().unwrap();
        let tx = conn.transaction().unwrap();
        let mut state: SharedRevisionState = serde_json::from_slice(&bytes).unwrap();
        state.first_user_message_root =
            Some(store_records(&tx, &lineage, &[vec![0xff]], ObjectCompression::none()).unwrap());
        let payload = put_payload(
            &tx,
            &lineage,
            PayloadKind::RevisionState,
            &serde_json::to_vec(&state).unwrap(),
            ObjectCompression::none(),
            &mut OperationStats::default(),
        )
        .unwrap();
        assert!(hydrate_shared_revision_state(&tx, &lineage, &payload.id, state).is_err());
        tx.rollback().unwrap();
    }

    #[test]
    fn compact_revision_archives_roundtrip_unknown_fields_and_repeated_boundaries() {
        let (mut conn, lineage) = fixture();
        let mut metadata = metadata();
        let event = serde_json::json!({
            "kind": "future-kind", "summary": "quote\" newline\n nul\u{0} 日本語 α",
            "first_live_index": 1, "completed_at_history_len": 2,
            "created_at_ms": 3, "future": {"nested": [null, true, 1.25]}
        });
        let mut repeated = event.clone();
        repeated["created_at_ms"] = Value::from(4);
        metadata.checkpoint_json = Some(repeated.clone());
        metadata.checkpoint_events_json = Some(serde_json::json!([event, repeated]));
        let side = SideTableSuffixes {
            start: HistoryIndex::ZERO,
            turn_metas: vec![
                (HistoryIndex::new(2), Value::from("old")),
                (HistoryIndex::ZERO, Value::Null),
                (HistoryIndex::new(2), serde_json::json!({"last": true})),
            ],
            metadata_snapshots: vec![(
                HistoryIndex::new(1),
                serde_json::json!({"unknown": [1,2,3]}),
            )],
            context_snapshots: vec![(HistoryIndex::ZERO, Value::from("context"))],
        };
        let (payload, bytes) = install(&mut conn, &lineage, &metadata, &side);
        assert!(
            bytes.len() < 2048,
            "archive bodies must not be in the envelope"
        );
        let state: SharedRevisionState = serde_json::from_slice(&bytes).unwrap();
        let active = archive_root(
            &conn,
            &lineage,
            "checkpoint",
            state.archives.checkpoint.as_deref().unwrap(),
        )
        .unwrap();
        let timeline = archive_root(
            &conn,
            &lineage,
            "checkpoint_events",
            state.archives.checkpoint_events.as_deref().unwrap(),
        )
        .unwrap();
        let (active_summary, _) =
            sequence_payload_refs_from_root(&conn, &lineage, &active, 1, 2).unwrap();
        let (first_summary, _) =
            sequence_payload_refs_from_root(&conn, &lineage, &timeline, 1, 2).unwrap();
        assert_eq!(
            active_summary, first_summary,
            "summary leaves must share identity"
        );
        let hydrated = hydrate_shared_revision_state(&conn, &lineage, &payload.id, state).unwrap();
        assert_eq!(hydrated.metadata, metadata);
        assert_eq!(
            hydrated.side_tables,
            merge_side_tables(&SideTableSuffixes::default(), &side)
        );
        crate::schema::validate_lineage_schema(&conn).unwrap();
    }

    #[test]
    fn compact_revision_archives_preserve_optional_and_legacy_checkpoint_shapes() {
        let (mut conn, lineage) = fixture();
        for checkpoint in [
            None,
            Some(Value::Null),
            Some(Value::from("legacy")),
            Some(serde_json::json!({"summary": null, "unknown": 42})),
            Some(serde_json::json!({"summary": ""})),
        ] {
            for events in [None, Some(serde_json::json!([]))] {
                let mut metadata = metadata();
                metadata.checkpoint_json = checkpoint.clone();
                metadata.checkpoint_events_json = events;
                let (payload, bytes) = install(
                    &mut conn,
                    &lineage,
                    &metadata,
                    &SideTableSuffixes::default(),
                );
                let hydrated = hydrate_shared_revision_state(
                    &conn,
                    &lineage,
                    &payload.id,
                    serde_json::from_slice(&bytes).unwrap(),
                )
                .unwrap();
                assert_eq!(hydrated.metadata, metadata);
            }
        }
    }

    #[test]
    fn unchanged_compact_revision_archives_have_bounded_title_storage_growth() {
        for count in [0, 32, 128] {
            let (mut conn, lineage) = fixture();
            let mut metadata = metadata();
            metadata.checkpoint_events_json = Some(Value::Array((0..count).map(|index| {
                serde_json::json!({"kind": "auto", "summary": format!("{index}:{}", "s".repeat(32 * 1024)),
                    "first_live_index": 0, "completed_at_history_len": 0, "created_at_ms": index})
            }).collect()));
            let side = SideTableSuffixes::default();
            let (_, initial) = install(&mut conn, &lineage, &metadata, &side);
            let initial: SharedRevisionState = serde_json::from_slice(&initial).unwrap();
            let before = stored_bytes(&conn);
            for index in 0..20 {
                metadata.updated_at = index + 2;
                metadata.title = Some(format!("title-{index}"));
                let (payload, bytes) = install(&mut conn, &lineage, &metadata, &side);
                assert!(bytes.len() < 2048);
                let state: SharedRevisionState = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    state.archives.checkpoint_events,
                    initial.archives.checkpoint_events
                );
                let hydrated =
                    hydrate_shared_revision_state(&conn, &lineage, &payload.id, state).unwrap();
                assert_eq!(hydrated.metadata, metadata);
            }
            assert!(
                stored_bytes(&conn) - before < 20 * 2048,
                "unchanged archives must not be physically repeated for {count} checkpoints"
            );
        }
    }

    #[test]
    fn compact_revision_archive_ownership_is_immutable_and_validates_root_shape() {
        let (mut conn, lineage) = fixture();
        let mut metadata = metadata();
        metadata.checkpoint_json = Some(serde_json::json!({"summary": "active"}));
        let (payload, bytes) = install(
            &mut conn,
            &lineage,
            &metadata,
            &SideTableSuffixes::default(),
        );
        assert!(conn
            .execute(
                "DELETE FROM lineage_revision_state_roots WHERE state_payload_id = ?1",
                [payload.id.as_str()]
            )
            .is_err());
        assert!(conn.execute("UPDATE lineage_revision_state_roots SET role = 'checkpoint_events' WHERE role = 'checkpoint'", []).is_err());
        let empty = empty_sequence(&conn, &lineage, SequenceKind::Data).unwrap();
        assert!(conn.execute(
            "INSERT INTO lineage_revision_state_roots (lineage_id, state_payload_id, role, root_id) VALUES (?1, ?2, 'checkpoint', ?3)",
            (lineage.as_str(), payload.id.as_str(), empty.id.as_str())).is_err());
        let tx = conn.transaction().unwrap();
        let mut state: SharedRevisionState = serde_json::from_slice(&bytes).unwrap();
        let (odd, _) = append_sequence_in(
            &tx,
            &lineage,
            &empty,
            &[b"{}".to_vec()],
            ObjectCompression::none(),
        )
        .unwrap();
        state.archives.checkpoint_events = Some(odd.id.0);
        let bytes = serde_json::to_vec(&state).unwrap();
        assert!(put_payload(
            &tx,
            &lineage,
            PayloadKind::RevisionState,
            &bytes,
            ObjectCompression::none(),
            &mut OperationStats::default()
        )
        .is_err());
        tx.rollback().unwrap();
    }

    #[test]
    fn message_root_ownership_guards_shape_and_uses_existing_reclamation() {
        let (mut conn, lineage) = fixture();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let branch = BranchId::new("a".repeat(64)).unwrap();
        let branch_metadata = BranchMetadata {
            parent_session_id: None,
            cwd: None,
            mode: None,
            reasoning_effort: None,
            model: None,
            fast_mode: None,
            session_cost_usd: 0.0,
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            accounting_json: "null".into(),
        };
        let (initial, _) = create_initial_branch(
            &mut conn,
            &lineage,
            &branch,
            &branch_metadata,
            b"initial fixture state",
            1,
        )
        .unwrap();
        let (head, _) = commit_revision(
            &mut conn,
            &lineage,
            &branch,
            &initial.id,
            &initial.history_root,
            &initial.transcript_root,
            b"message fixture state",
            LineageOperation::Append,
            2,
        )
        .unwrap();
        let owner = head.state_payload_id.as_str();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::Data).unwrap();
        let body = "shared body α 日本語\n\0".as_bytes().to_vec();
        let (root, _) = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            std::slice::from_ref(&body),
            ObjectCompression::none(),
        )
        .unwrap();
        let insert = "INSERT INTO lineage_revision_state_roots (lineage_id, state_payload_id, role, root_id) VALUES (?1, ?2, 'first_user_message', ?3)";
        for count in [0, 2, 3] {
            let (invalid, _) = append_sequence(
                &mut conn,
                &lineage,
                &empty,
                &vec![body.clone(); count],
                ObjectCompression::none(),
            )
            .unwrap();
            assert!(
                conn.execute(insert, (lineage.as_str(), owner, invalid.id.as_str()))
                    .is_err(),
                "message extent {count} must fail"
            );
        }
        let history = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let (history, _) = append_sequence(
            &mut conn,
            &lineage,
            &history,
            std::slice::from_ref(&body),
            ObjectCompression::none(),
        )
        .unwrap();
        assert!(conn
            .execute(insert, (lineage.as_str(), owner, history.id.as_str()))
            .is_err());
        conn.execute(insert, (lineage.as_str(), owner, root.id.as_str()))
            .unwrap();
        assert!(conn.execute("UPDATE lineage_revision_state_roots SET root_id = ?1 WHERE role = 'first_user_message'", [empty.id.as_str()]).is_err());
        assert!(conn
            .execute(
                "DELETE FROM lineage_revision_state_roots WHERE role = 'first_user_message'",
                []
            )
            .is_err());
        let mut completed = false;
        for _ in 0..1000 {
            let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
            assert!(step.work_rows() <= 1);
            assert_eq!(
                sequence_range(&conn, &lineage, &root, 0, 1)
                    .unwrap()
                    .0
                    .as_slice(),
                std::slice::from_ref(&body)
            );
            if step.complete {
                completed = true;
                break;
            }
        }
        assert!(
            completed,
            "reachable message roots must not block reclamation"
        );
        assert!(conn
            .execute(
                "DELETE FROM lineage_sequence_roots WHERE root_id = ?1",
                [root.id.as_str()]
            )
            .is_err());
        delete_branch(&conn, &lineage, &branch, 3).unwrap();
        let mut completed = false;
        for _ in 0..1000 {
            let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
            assert!(step.work_rows() <= 1);
            if step.complete {
                completed = true;
                break;
            }
        }
        assert!(
            completed,
            "message ownership must release with its state payload"
        );
        assert!(load_root(&conn, &lineage, &root.id).is_err());
        assert_eq!(conn.query_row("SELECT count(*) FROM lineage_revision_state_roots WHERE role = 'first_user_message'", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        crate::schema::validate_lineage_schema(&conn).unwrap();
    }

    #[test]
    fn side_table_suffix_updates_read_only_index_headers_and_reuse_prefixes() {
        for format in [SideRowFormat::Json, SideRowFormat::Metadata] {
            for count in [1_u64, 32, 4096] {
                let (mut conn, lineage) = fixture();
                let conn = conn.transaction().unwrap();
                let rows: Vec<_> = (0..count)
                .map(|index| {
                    (
                        HistoryIndex::new(index * 2),
                        match format {
                            SideRowFormat::Json => Value::String("retained α".repeat(256)),
                            SideRowFormat::Metadata => serde_json::json!({"first_user_message": "retained α".repeat(256), "unknown": 42}),
                        },
                    )
                })
                .collect();
                let id = store_side_rows(
                    &conn,
                    &lineage,
                    None,
                    HistoryIndex::ZERO,
                    &rows,
                    ObjectCompression::none(),
                    format,
                )
                .unwrap();
                let root = archive_root(&conn, &lineage, "side_table", &id).unwrap();
                let (unchanged, stats) = replace_side_rows(
                    &conn,
                    &lineage,
                    &root,
                    HistoryIndex::new(count * 2),
                    &[],
                    ObjectCompression::none(),
                    format,
                )
                .unwrap();
                assert_eq!(unchanged, root);
                assert_eq!(
                    stats.payloads_read,
                    u64::from(matches!(format, SideRowFormat::Json)),
                    "metadata coordinates avoid header hydration; JSON rows read one index header"
                );
                assert_eq!(stats.payloads_written, 0);
                assert_eq!(stats.nodes_written, 0);
                assert!(stats.nodes_read <= u64::from(root.depth) + 2);
                let start = HistoryIndex::new((count / 2) * 2 + 1);
                let suffix = vec![
                    (HistoryIndex::ZERO, Value::from("ignored")),
                    (start, Value::from("old")),
                    (start, serde_json::json!({"last": true})),
                    (HistoryIndex::new(count * 2 + 4), Value::Null),
                ];
                let (changed, stats) = replace_side_rows(
                    &conn,
                    &lineage,
                    &root,
                    start,
                    &suffix,
                    ObjectCompression::none(),
                    format,
                )
                .unwrap();
                assert!(
                    stats.payloads_read <= u64::from(count.ilog2()) + 2,
                    "suffix lookup must not hydrate values: {stats:?}"
                );
                assert!(stats.payloads_written <= 4);
                let prefix_end = rows.iter().filter(|(index, _)| *index < start).count() as u64 * 2;
                let (before, stats) =
                    sequence_payload_refs_from_root(&conn, &lineage, &root, 0, prefix_end).unwrap();
                assert_eq!(stats.payloads_read, 0);
                let (after, stats) =
                    sequence_payload_refs_from_root(&conn, &lineage, &changed, 0, prefix_end)
                        .unwrap();
                assert_eq!(stats.payloads_read, 0);
                assert_eq!(before, after);
                assert_eq!(
                    read_side_rows(&conn, &lineage, "side_table", changed.id.as_str(), format)
                        .unwrap(),
                    merge_side_rows(&rows, &suffix, start)
                );
                validate_sequence(&conn, &lineage, &changed).unwrap();
            }
        }
    }

    #[test]
    fn compact_revision_envelopes_reject_unrecognized_archive_owners() {
        let (mut conn, lineage) = fixture();
        let (_, bytes) = install(
            &mut conn,
            &lineage,
            &metadata(),
            &SideTableSuffixes::default(),
        );
        let mut state: Value = serde_json::from_slice(&bytes).unwrap();
        state["archives"]["unrecognized_root"] = Value::from("a".repeat(64));
        let tx = conn.transaction().unwrap();
        assert!(put_payload(
            &tx,
            &lineage,
            PayloadKind::RevisionState,
            &serde_json::to_vec(&state).unwrap(),
            ObjectCompression::none(),
            &mut OperationStats::default()
        )
        .is_err());
        tx.rollback().unwrap();
    }

    #[test]
    fn unreferenced_compact_revision_archives_are_reclaimed_with_budget_one() {
        let (mut conn, lineage) = fixture();
        let mut metadata = metadata();
        metadata.checkpoint_events_json = Some(serde_json::json!([{"summary": "orphan"}]));
        let (_, bytes) = install(
            &mut conn,
            &lineage,
            &metadata,
            &SideTableSuffixes::default(),
        );
        let state: SharedRevisionState = serde_json::from_slice(&bytes).unwrap();
        let roots: Vec<_> = state
            .archives
            .roles()
            .map(|(_, id)| id.to_owned())
            .collect();
        let mut completed = false;
        for _ in 0..1000 {
            let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
            assert!(step.work_rows() <= 1);
            if step.complete {
                completed = true;
                break;
            }
        }
        assert!(
            completed,
            "archive ownership must not prevent bounded GC progress"
        );
        for id in roots {
            assert!(load_root(&conn, &lineage, &RootId::from_db(id).unwrap()).is_err());
        }
        assert_eq!(stored_bytes(&conn), 0);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM lineage_revision_state_roots",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        crate::schema::validate_lineage_schema(&conn).unwrap();
    }
}
