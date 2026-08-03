use super::*;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LineageSessionSnapshot {
    pub(crate) identity: SessionIdentity,
    pub(crate) metadata: SessionMetadata,
    pub(crate) head: StoreHead,
    pub(crate) side_tables: SideTableSuffixes,
    pub(crate) revision_id: RevisionId,
    pub(crate) history_root: SequenceRoot,
    pub(crate) transcript_root: SequenceRoot,
}

pub(crate) fn revision_state_bytes(
    metadata: &SessionMetadata,
    side_tables: SideTableSuffixes,
) -> Result<Vec<u8>> {
    let mut metadata = metadata.clone();
    metadata.cwd = None;
    metadata.mode = None;
    metadata.reasoning_effort = None;
    metadata.model = None;
    metadata.fast_mode = None;
    metadata.session_cost_usd = SessionCostUsd::new(0.0)?;
    if let Some(serde_json::Value::Object(accounting)) = metadata.accounting_json.as_mut() {
        accounting.remove("session_usage");
    }
    Ok(serde_json::to_vec(&CanonicalRevisionState {
        format_version: LINEAGE_REVISION_STATE_VERSION,
        metadata,
        side_tables,
    })?)
}

pub(crate) fn load_revision_state(
    conn: &Connection,
    lineage: &LineageId,
    revision: &RevisionRecord,
) -> Result<CanonicalRevisionState> {
    let bytes = hydrate_payload(
        conn,
        lineage,
        &revision.state_payload_id,
        PayloadKind::RevisionState,
        &mut OperationStats::default(),
    )?;
    let state: CanonicalRevisionState = serde_json::from_slice(&bytes)?;
    if state.format_version != LINEAGE_REVISION_STATE_VERSION {
        return Err(StoreError::Integrity(format!(
            "unsupported lineage revision state version {}",
            state.format_version
        )));
    }
    Ok(state)
}

pub(crate) fn branch_metadata_from_session(
    identity: &SessionIdentity,
    metadata: &SessionMetadata,
) -> Result<BranchMetadata> {
    if let Some(parent) = identity.parent_id.as_deref() {
        validate_lower_hex(parent, 64, "parent session id")?;
    }
    let accounting_json = serde_json::to_string(&metadata.accounting_json)?;
    let usage = metadata
        .accounting_json
        .as_ref()
        .and_then(|value| value.get("session_usage"));
    let usage_count = |name: &str| {
        usage
            .and_then(|usage| usage.get(name))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    Ok(BranchMetadata {
        parent_session_id: identity.parent_id.clone(),
        cwd: metadata.cwd.clone(),
        mode: metadata.mode.clone(),
        reasoning_effort: metadata.reasoning_effort.clone(),
        model: metadata.model.clone(),
        fast_mode: metadata.fast_mode,
        session_cost_usd: metadata.session_cost_usd.get(),
        input_tokens: usage_count("input_tokens"),
        cached_input_tokens: usage_count("cached_input_tokens"),
        output_tokens: usage_count("output_tokens"),
        reasoning_tokens: usage_count("reasoning_tokens"),
        accounting_json,
    })
}

pub(crate) fn merge_accounting_json(
    revision: Option<serde_json::Value>,
    branch: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    match (revision, branch) {
        (
            Some(serde_json::Value::Object(mut revision)),
            Some(serde_json::Value::Object(branch)),
        ) => {
            if let Some(usage) = branch.get("session_usage") {
                revision.insert("session_usage".into(), usage.clone());
                Some(serde_json::Value::Object(revision))
            } else {
                Some(serde_json::Value::Object(branch))
            }
        }
        (_, branch @ Some(_)) => branch,
        (revision, None) => revision,
    }
}

pub(crate) fn load_branch_snapshot(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    include_deleted: bool,
) -> Result<LineageSessionSnapshot> {
    let deleted_filter = if include_deleted {
        ""
    } else {
        " AND deleted_at IS NULL"
    };
    let sql = format!(
        "SELECT parent_session_id, created_at, head_sequence, head_revision_id,
                cwd, mode, reasoning_effort, model, fast_mode,
                session_cost_usd, accounting_json
         FROM lineage_branches
         WHERE lineage_id = ?1 AND session_id = ?2{deleted_filter}"
    );
    let row = conn
        .query_row(&sql, (lineage.as_str(), branch.as_str()), |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<bool>>(8)?,
                row.get::<_, f64>(9)?,
                row.get::<_, String>(10)?,
            ))
        })
        .optional()?
        .ok_or_else(|| StoreError::Integrity(format!("branch {} is not live", branch.as_str())))?;
    let revision_id = RevisionId::from_db(row.3)?;
    let revision = load_revision(conn, lineage, &revision_id)?;
    let state = load_revision_state(conn, lineage, &revision)?;
    let mut metadata = state.metadata;
    metadata.cwd = row.4;
    metadata.mode = row.5;
    metadata.reasoning_effort = row.6;
    metadata.model = row.7;
    metadata.fast_mode = row.8;
    metadata.session_cost_usd = SessionCostUsd::new(row.9)?;
    let branch_accounting = serde_json::from_str::<Option<serde_json::Value>>(&row.10)?;
    metadata.accounting_json = merge_accounting_json(metadata.accounting_json, branch_accounting);
    let created_at = row.1;
    if created_at < 0 {
        return Err(StoreError::Integrity(
            "lineage branch has negative creation time".into(),
        ));
    }
    Ok(LineageSessionSnapshot {
        identity: SessionIdentity {
            id: branch.as_str().to_owned(),
            created_at,
            parent_id: row.0,
        },
        metadata,
        head: StoreHead {
            revision: crate::session_commit::Revision::new(nonnegative_u64(
                row.2,
                "branch head sequence",
            )?),
            history_len: crate::session_commit::HistoryLen::new(revision.history_root.item_count),
            transcript_record_count: crate::session_commit::TranscriptRecordCount::new(
                revision.transcript_root.item_count,
            ),
        },
        side_tables: state.side_tables,
        revision_id,
        history_root: revision.history_root,
        transcript_root: revision.transcript_root,
    })
}

pub(crate) fn lineage_session_snapshot(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<LineageSessionSnapshot> {
    load_branch_snapshot(conn, lineage, branch, false)
}

pub(crate) fn deserialize_sequence_range<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    start: u64,
    end: u64,
) -> Result<Vec<T>> {
    sequence_range(conn, lineage, root, start, end)?
        .0
        .into_iter()
        .map(|bytes| serde_json::from_slice(&bytes).map_err(StoreError::from))
        .collect()
}

pub(crate) fn deserialize_history_items(
    conn: &Connection,
    bytes: Vec<Vec<u8>>,
) -> Result<Vec<protocol::HistoryItem>> {
    bytes
        .into_iter()
        .map(|bytes| {
            let mut value = serde_json::from_slice(&bytes)?;
            crate::history::rehydrate_object_refs(conn, &mut value)?;
            serde_json::from_value(value).map_err(StoreError::from)
        })
        .collect()
}

pub(crate) fn lineage_history_range(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    start: u64,
    end: u64,
) -> Result<Vec<protocol::HistoryItem>> {
    let snapshot = lineage_session_snapshot(conn, lineage, branch)?;
    let bytes = sequence_range(conn, lineage, &snapshot.history_root, start, end)?.0;
    deserialize_history_items(conn, bytes)
}

pub(crate) fn lineage_history_tail(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    end: usize,
    max_items: usize,
    max_bytes: Option<usize>,
) -> Result<Vec<protocol::HistoryItem>> {
    if end == 0 || max_items == 0 || max_bytes == Some(0) {
        return Ok(Vec::new());
    }
    let snapshot = lineage_session_snapshot(conn, lineage, branch)?;
    let end = u64::try_from(end)
        .unwrap_or(u64::MAX)
        .min(snapshot.history_root.item_count);
    let start = end.saturating_sub(u64::try_from(max_items).unwrap_or(u64::MAX));
    let bytes = sequence_range(conn, lineage, &snapshot.history_root, start, end)?.0;
    let mut budget = protocol::HistoryTailBudget::new(max_items, max_bytes);
    let mut items = Vec::with_capacity(bytes.len());
    for bytes in bytes.into_iter().rev() {
        let mut value = serde_json::from_slice(&bytes)?;
        if !budget.can_prepend_bytes(crate::history::history_object_bytes(&value)) {
            break;
        }
        crate::history::rehydrate_object_refs(conn, &mut value)?;
        let item = serde_json::from_value(value)?;
        if !budget.try_prepend(&item)? {
            break;
        }
        items.push(item);
    }
    items.reverse();
    Ok(items)
}

pub(crate) fn collect_transcript_search_leaves(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &NodeId,
    expected_level: u32,
    start_index: u64,
    output: &mut Vec<TranscriptSearchLeaf>,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    if cancelled() {
        return Err(StoreError::Cancelled);
    }
    let node = load_node_shallow(conn, lineage, node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != expected_level {
        return Err(StoreError::Integrity(format!(
            "transcript search traversal reached invalid node {}",
            node_id.as_str()
        )));
    }
    if node.level == 0 {
        output.push(TranscriptSearchLeaf {
            node_id: node.id.as_str().to_owned(),
            start_index,
            item_count: node.item_count,
            byte_count: node.byte_count,
        });
        return Ok(());
    }

    let mut child_start = start_index;
    for entry in node.entries {
        let EntryTarget::Child(child_id) = entry.target else {
            return Err(StoreError::Integrity(
                "transcript search internal node contains a payload".into(),
            ));
        };
        collect_transcript_search_leaves(
            conn,
            lineage,
            &child_id,
            expected_level - 1,
            child_start,
            output,
            cancelled,
        )?;
        child_start = child_start
            .checked_add(entry.item_count)
            .ok_or_else(|| StoreError::Integrity("transcript search extent overflow".into()))?;
    }
    if child_start != start_index.saturating_add(node.item_count) {
        return Err(StoreError::Integrity(
            "transcript search leaves reconstructed the wrong extent".into(),
        ));
    }
    Ok(())
}

pub(crate) fn lineage_transcript_search_leaves(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<(String, Vec<TranscriptSearchLeaf>)> {
    lineage_transcript_search_leaves_with_cancellation(conn, lineage, branch, &|| false)
}

pub(crate) fn lineage_transcript_search_leaves_with_cancellation(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    cancelled: &dyn Fn() -> bool,
) -> Result<(String, Vec<TranscriptSearchLeaf>)> {
    if cancelled() {
        return Err(StoreError::Cancelled);
    }
    let snapshot = lineage_session_snapshot(conn, lineage, branch)?;
    let root = load_matching_root(conn, lineage, &snapshot.transcript_root)?;
    let mut leaves = Vec::new();
    if let Some(node_id) = &root.node_id {
        collect_transcript_search_leaves(
            conn,
            lineage,
            node_id,
            root.depth - 1,
            0,
            &mut leaves,
            cancelled,
        )?;
    }
    let mut item_count = 0_u64;
    for leaf in &leaves {
        if cancelled() {
            return Err(StoreError::Cancelled);
        }
        item_count = item_count
            .checked_add(leaf.item_count)
            .ok_or_else(|| StoreError::Integrity("transcript search leaf count overflow".into()))?;
    }
    if item_count != root.item_count {
        return Err(StoreError::Integrity(
            "transcript search leaves do not cover the branch root".into(),
        ));
    }
    Ok((root.id.as_str().to_owned(), leaves))
}

pub(crate) fn lineage_transcript_search_leaf_records(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<StoredTranscriptBlock>> {
    if cancelled() {
        return Err(StoreError::Cancelled);
    }
    let node_id = NodeId::from_db(node_id.to_owned())?;
    let node = load_node_shallow(conn, lineage, &node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != 0 {
        return Err(StoreError::Integrity(format!(
            "search segment {} is not a transcript leaf",
            node.id.as_str()
        )));
    }
    let mut stats = OperationStats::default();
    let mut records = Vec::with_capacity(node.entries.len());
    for entry in node.entries {
        if cancelled() {
            return Err(StoreError::Cancelled);
        }
        let EntryTarget::Item(payload_id) = entry.target else {
            return Err(StoreError::Integrity(
                "transcript search leaf contains a child node".into(),
            ));
        };
        let bytes = hydrate_payload(
            conn,
            lineage,
            &payload_id,
            PayloadKind::Transcript,
            &mut stats,
        )?;
        records.push(serde_json::from_slice(&bytes)?);
    }
    if records.len() as u64 != node.item_count {
        return Err(StoreError::Integrity(
            "transcript search leaf reconstructed the wrong item count".into(),
        ));
    }
    Ok(records)
}

pub(crate) fn lineage_transcript_search_leaf_records_at(
    conn: &Connection,
    lineage: &LineageId,
    node_id: &str,
    ordinals: &[usize],
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<(usize, StoredTranscriptBlock)>> {
    if cancelled() {
        return Err(StoreError::Cancelled);
    }
    let node_id = NodeId::from_db(node_id.to_owned())?;
    let node = load_node_shallow(conn, lineage, &node_id, None)?;
    if node.kind != SequenceKind::Transcript || node.level != 0 {
        return Err(StoreError::Integrity(format!(
            "search segment {} is not a transcript leaf",
            node.id.as_str()
        )));
    }

    let mut stats = OperationStats::default();
    let mut records = Vec::with_capacity(ordinals.len());
    for ordinal in ordinals.iter().copied() {
        if cancelled() {
            return Err(StoreError::Cancelled);
        }
        let entry = node.entries.get(ordinal).ok_or_else(|| {
            StoreError::Integrity(format!(
                "transcript search leaf {} has no record {ordinal}",
                node.id.as_str()
            ))
        })?;
        let EntryTarget::Item(payload_id) = &entry.target else {
            return Err(StoreError::Integrity(
                "transcript search leaf contains a child node".into(),
            ));
        };
        let bytes = hydrate_payload(
            conn,
            lineage,
            payload_id,
            PayloadKind::Transcript,
            &mut stats,
        )?;
        records.push((ordinal, serde_json::from_slice(&bytes)?));
    }
    Ok(records)
}

pub(crate) fn lineage_transcript_object_backed_range(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    start: u64,
    end: u64,
) -> Result<Vec<StoredTranscriptBlock>> {
    let snapshot = lineage_session_snapshot(conn, lineage, branch)?;
    deserialize_sequence_range(conn, lineage, &snapshot.transcript_root, start, end)
}

pub(crate) fn lineage_transcript_range(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    start: u64,
    end: u64,
) -> Result<Vec<StoredTranscriptBlock>> {
    let mut records = lineage_transcript_object_backed_range(conn, lineage, branch, start, end)?;
    hydrate_transcript_records(conn, &mut records)?;
    Ok(records)
}

pub(crate) fn merge_side_rows(
    existing: &[(HistoryIndex, serde_json::Value)],
    suffix: &[(HistoryIndex, serde_json::Value)],
    start: HistoryIndex,
) -> Vec<(HistoryIndex, serde_json::Value)> {
    existing
        .iter()
        .filter(|(index, _)| *index < start)
        .chain(suffix.iter().filter(|(index, _)| *index >= start))
        .map(|(index, value)| (*index, value.clone()))
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect()
}

pub(crate) fn merge_side_tables(
    previous: &SideTableSuffixes,
    suffix: &SideTableSuffixes,
) -> SideTableSuffixes {
    SideTableSuffixes {
        start: HistoryIndex::ZERO,
        turn_metas: merge_side_rows(&previous.turn_metas, &suffix.turn_metas, suffix.start),
        metadata_snapshots: merge_side_rows(
            &previous.metadata_snapshots,
            &suffix.metadata_snapshots,
            suffix.start,
        ),
        context_snapshots: merge_side_rows(
            &previous.context_snapshots,
            &suffix.context_snapshots,
            suffix.start,
        ),
    }
}

pub(crate) fn serialize_history_items(
    conn: &Connection,
    items: &[protocol::HistoryItem],
    compression: ObjectCompression,
) -> Result<Vec<Vec<u8>>> {
    items
        .iter()
        .map(|item| crate::history::serialize_normalized_history_item(conn, item, compression))
        .collect()
}

pub(crate) fn serialize_transcript_items(
    conn: &Connection,
    records: &[StoredTranscriptBlock],
    compression: ObjectCompression,
) -> Result<Vec<Vec<u8>>> {
    records
        .iter()
        .map(|record| {
            let mut record = record.clone();
            let mut block = serde_json::from_str(&record.block_json)?;
            crate::history::normalize_metadata(
                Some(conn),
                &mut block,
                compression,
                &mut Vec::new(),
            )?;
            record.block_json = serde_json::to_string(&block)?;
            if let Some(tool_state_json) = record.tool_state_json.as_mut() {
                let mut tool_state = serde_json::from_str(tool_state_json)?;
                crate::history::normalize_metadata(
                    Some(conn),
                    &mut tool_state,
                    compression,
                    &mut Vec::new(),
                )?;
                *tool_state_json = serde_json::to_string(&tool_state)?;
            }
            serde_json::to_vec(&record).map_err(StoreError::from)
        })
        .collect()
}

pub(crate) fn hydrate_transcript_records(
    conn: &Connection,
    records: &mut [StoredTranscriptBlock],
) -> Result<()> {
    for record in records {
        let mut block = serde_json::from_str(&record.block_json)?;
        crate::history::rehydrate_object_refs(conn, &mut block)?;
        record.block_json = serde_json::to_string(&block)?;
        if let Some(tool_state_json) = record.tool_state_json.as_mut() {
            let mut tool_state = serde_json::from_str(tool_state_json)?;
            crate::history::rehydrate_object_refs(conn, &mut tool_state)?;
            *tool_state_json = serde_json::to_string(&tool_state)?;
        }
    }
    Ok(())
}

pub(crate) fn replace_sequence_suffix_in(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    start: u64,
    items: &[Vec<u8>],
    compression: ObjectCompression,
) -> Result<SequenceRoot> {
    let ((prefix, _), _) = split_sequence_in(conn, lineage, root, start)?;
    let (new_root, _) = append_sequence_in(conn, lineage, &prefix, items, compression)?;
    if root.kind == SequenceKind::Transcript {
        install_transcript_extent_chunks(conn, lineage, root, &new_root, start, items)?;
    }
    Ok(new_root)
}

pub(crate) fn branch_revision_at_sequence(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    sequence: u64,
) -> Result<RevisionId> {
    let sequence = checked_i64(sequence, "branch sequence")?;
    let value = conn
        .query_row(
            "SELECT revision_id FROM lineage_branch_revisions
             WHERE lineage_id = ?1 AND session_id = ?2 AND branch_sequence = ?3",
            (lineage.as_str(), branch.as_str(), sequence),
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Integrity("lineage branch sequence is missing".into()))?;
    RevisionId::from_db(value)
}

pub(crate) fn update_branch_metadata(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    metadata: &BranchMetadata,
) -> Result<()> {
    let updated = conn.execute(
        "UPDATE lineage_branches
         SET cwd = ?1, mode = ?2, reasoning_effort = ?3, model = ?4,
             fast_mode = ?5, session_cost_usd = ?6, input_tokens = ?7,
             cached_input_tokens = ?8, output_tokens = ?9, reasoning_tokens = ?10,
             accounting_json = ?11
         WHERE lineage_id = ?12 AND session_id = ?13 AND deleted_at IS NULL",
        rusqlite::params![
            metadata.cwd,
            metadata.mode,
            metadata.reasoning_effort,
            metadata.model,
            metadata.fast_mode,
            metadata.session_cost_usd,
            checked_i64(metadata.input_tokens, "branch input_tokens")?,
            checked_i64(metadata.cached_input_tokens, "branch cached_input_tokens")?,
            checked_i64(metadata.output_tokens, "branch output_tokens")?,
            checked_i64(metadata.reasoning_tokens, "branch reasoning_tokens")?,
            metadata.accounting_json,
            lineage.as_str(),
            branch.as_str(),
        ],
    )?;
    if updated != 1 {
        return Err(StoreError::Integrity(
            "lineage branch metadata update missed its branch".into(),
        ));
    }
    Ok(())
}

pub(crate) fn store_failure(error: StoreError) -> SessionCommitFailure {
    crate::session_command::commit_failure_from_store_error(error)
}

pub(crate) trait LineageSavepoint {
    fn lineage_savepoint(&mut self) -> rusqlite::Result<Savepoint<'_>>;
}

impl LineageSavepoint for Connection {
    fn lineage_savepoint(&mut self) -> rusqlite::Result<Savepoint<'_>> {
        self.savepoint()
    }
}

impl LineageSavepoint for Transaction<'_> {
    fn lineage_savepoint(&mut self) -> rusqlite::Result<Savepoint<'_>> {
        self.savepoint()
    }
}
