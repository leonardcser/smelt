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

#[derive(Debug)]
pub(crate) enum StoredRevisionState {
    Shared(SharedRevisionState),
    Legacy(CanonicalRevisionState),
}

impl StoredRevisionState {
    pub(crate) fn metadata(&self) -> &SessionMetadata {
        match self {
            Self::Shared(state) => &state.metadata,
            Self::Legacy(state) => &state.metadata,
        }
    }

    pub(crate) fn catalog_message_id(&self) -> Option<&str> {
        match self {
            Self::Shared(state) => state.first_user_message_root.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn catalog_message(
        &self,
        conn: &Connection,
        lineage: &LineageId,
        stats: &mut OperationStats,
    ) -> Result<Option<std::sync::Arc<str>>> {
        match self {
            Self::Shared(state) => {
                Ok(read_first_user_message(conn, lineage, state, stats)?.map(std::sync::Arc::from))
            }
            _ => Ok(self
                .metadata()
                .first_user_message
                .as_deref()
                .map(std::sync::Arc::from)),
        }
    }
}

pub(crate) struct PreparedRevisionState {
    pub(crate) bytes: Vec<u8>,
    pub(crate) metadata: SessionMetadata,
    pub(crate) archives_unchanged: bool,
}

pub(crate) fn normalize_revision_metadata(
    mut metadata: SessionMetadata,
) -> Result<SessionMetadata> {
    metadata.cwd = None;
    metadata.mode = None;
    metadata.reasoning_effort = None;
    metadata.model = None;
    metadata.fast_mode = None;
    metadata.session_cost_usd = SessionCostUsd::new(0.0)?;
    if let Some(serde_json::Value::Object(accounting)) = metadata.accounting_json.as_mut() {
        accounting.remove("session_usage");
    }
    Ok(metadata)
}

pub(crate) fn prepare_revision_state(
    conn: &Connection,
    lineage: &LineageId,
    metadata: &SessionMetadata,
    side_tables: &SideTableSuffixes,
    previous: Option<&StoredRevisionState>,
    compression: ObjectCompression,
) -> Result<PreparedRevisionState> {
    let metadata = normalize_revision_metadata(metadata.clone())?;
    if crate::schema::user_version(conn)? == crate::schema::LINEAGE_SCHEMA_VERSION {
        let projected;
        let previous = match previous {
            Some(StoredRevisionState::Shared(state)) => Some(state),
            Some(StoredRevisionState::Legacy(state)) => {
                projected = shared_revision_state(
                    conn,
                    lineage,
                    state.metadata.clone(),
                    &state.side_tables,
                    None,
                    compression,
                )?;
                Some(&projected)
            }
            None => None,
        };
        let state = shared_revision_state(
            conn,
            lineage,
            metadata,
            side_tables,
            previous.map(|state| &state.archives),
            compression,
        )?;
        return Ok(PreparedRevisionState {
            archives_unchanged: previous.is_some_and(|previous| {
                previous.archives == state.archives
                    && previous.first_user_message_root == state.first_user_message_root
            }),
            bytes: serde_json::to_vec(&state)?,
            metadata: state.metadata,
        });
    }
    // COMPAT(revision-state-v1): legacy migration fixtures retain their format.
    let previous = match previous {
        Some(StoredRevisionState::Legacy(state)) => Some(state),
        Some(StoredRevisionState::Shared(_)) => {
            return Err(StoreError::Integrity(
                "shared revision state requires the current schema".into(),
            ))
        }
        None => None,
    };
    let state = CanonicalRevisionState {
        format_version: LINEAGE_REVISION_STATE_VERSION,
        metadata,
        side_tables: merge_side_tables(
            previous.map_or(&SideTableSuffixes::default(), |state| &state.side_tables),
            side_tables,
        ),
    };
    let archives_unchanged = previous.is_some_and(|previous| {
        previous.side_tables == state.side_tables
            && previous.metadata.first_user_message == state.metadata.first_user_message
            && previous.metadata.checkpoint_json == state.metadata.checkpoint_json
            && previous.metadata.checkpoint_events_json == state.metadata.checkpoint_events_json
    });
    let bytes = serde_json::to_vec(&state)?;
    let mut metadata = state.metadata;
    metadata.checkpoint_json = None;
    metadata.checkpoint_events_json = None;
    Ok(PreparedRevisionState {
        archives_unchanged,
        bytes,
        metadata,
    })
}

pub(crate) fn load_revision_envelope(
    conn: &Connection,
    lineage: &LineageId,
    revision: &RevisionRecord,
    stats: &mut OperationStats,
) -> Result<StoredRevisionState> {
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()?;
    let projected = revision_projection(conn, lineage, &revision.state_payload_id)?;
    let payload = projected.as_ref().unwrap_or(&revision.state_payload_id);
    let state = load_revision_payload(conn, lineage, payload, stats)?;
    if projected.is_some() && !matches!(state, StoredRevisionState::Shared(_)) {
        return Err(StoreError::Integrity(
            "revision projection is not a shared envelope".into(),
        ));
    }
    Ok(state)
}

fn revision_projection(
    conn: &Connection,
    lineage: &LineageId,
    original: &PayloadId,
) -> Result<Option<PayloadId>> {
    if crate::schema::user_version(conn)? == 3 {
        return Ok(None);
    }
    let row = conn
        .query_row(
            "SELECT projected_payload_id, original_format_version, projection_id
         FROM lineage_revision_state_projections
         WHERE lineage_id = ?1 AND original_payload_id = ?2",
            (lineage.as_str(), original.as_str()),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((id, format, proof)) = row else {
        return Ok(None);
    };
    let projected = PayloadId::from_db(id)?;
    if format != LINEAGE_REVISION_STATE_VERSION
        || proof != revision_projection_id(lineage, original, &projected, format)
    {
        return Err(StoreError::Integrity(
            "revision projection has an invalid content address".into(),
        ));
    }
    Ok(Some(projected))
}

fn revision_projection_id(
    lineage: &LineageId,
    original: &PayloadId,
    projected: &PayloadId,
    original_format: u32,
) -> String {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-revision-projection-v1\0");
    encoder.str(lineage.as_str());
    encoder.str(original.as_str());
    encoder.str(projected.as_str());
    encoder.u64(u64::from(original_format));
    encoder.u64(u64::from(SHARED_REVISION_STATE_VERSION));
    encoder.hash()
}

pub(crate) fn load_revision_for_save(
    conn: &Connection,
    lineage: &LineageId,
    revision: &RevisionRecord,
    compression: ObjectCompression,
) -> Result<StoredRevisionState> {
    let previous = load_revision_envelope(conn, lineage, revision, &mut OperationStats::default())?;
    if crate::schema::user_version(conn)? == 3 || matches!(previous, StoredRevisionState::Shared(_))
    {
        return Ok(previous);
    }
    if conn.is_autocommit() {
        return Err(StoreError::Integrity(
            "revision projection requires a write transaction".into(),
        ));
    }
    let (format, projected) = match &previous {
        StoredRevisionState::Legacy(state) => (
            LINEAGE_REVISION_STATE_VERSION,
            shared_revision_state(
                conn,
                lineage,
                state.metadata.clone(),
                &state.side_tables,
                None,
                compression,
            )?,
        ),
        StoredRevisionState::Shared(_) => unreachable!(),
    };
    let bytes = serde_json::to_vec(&projected)?;
    let payload = put_payload(
        conn,
        lineage,
        PayloadKind::RevisionState,
        &bytes,
        compression,
        &mut OperationStats::default(),
    )?;
    verify_revision_projection(conn, lineage, &previous, &payload.id, &projected)?;
    conn.execute(
        "INSERT INTO lineage_revision_state_projections
         (lineage_id, original_payload_id, projected_payload_id, original_format_version, projection_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (lineage.as_str(), revision.state_payload_id.as_str(), payload.id.as_str(), format,
         revision_projection_id(lineage, &revision.state_payload_id, &payload.id, format)),
    )?;
    Ok(StoredRevisionState::Shared(projected))
}

pub(crate) fn verify_revision_projections(conn: &Connection, lineage: &LineageId) -> Result<()> {
    if crate::schema::user_version(conn)? == 3 {
        return Ok(());
    }
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()?;
    let originals = conn
        .prepare(
            "SELECT original_payload_id, original_format_version
         FROM lineage_revision_state_projections WHERE lineage_id = ?1",
        )?
        .query_map([lineage.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, format) in originals {
        let original = PayloadId::from_db(id)?;
        let projected = revision_projection(conn, lineage, &original)?.ok_or_else(|| {
            StoreError::Integrity("revision projection disappeared from read snapshot".into())
        })?;
        let original =
            load_revision_payload(conn, lineage, &original, &mut OperationStats::default())?;
        let original_format = match &original {
            StoredRevisionState::Legacy(state) => state.format_version,
            StoredRevisionState::Shared(state) => state.format_version,
        };
        if format != original_format {
            return Err(StoreError::Integrity(
                "revision projection records a different original format".into(),
            ));
        }
        let StoredRevisionState::Shared(state) =
            load_revision_payload(conn, lineage, &projected, &mut OperationStats::default())?
        else {
            return Err(StoreError::Integrity(
                "revision projection is not a shared envelope".into(),
            ));
        };
        verify_revision_projection(conn, lineage, &original, &projected, &state)?;
    }
    Ok(())
}

fn load_revision_payload(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadId,
    stats: &mut OperationStats,
) -> Result<StoredRevisionState> {
    let bytes = hydrate_payload(conn, lineage, payload, PayloadKind::RevisionState, stats)?;
    let format: RevisionStateFormat = serde_json::from_slice(&bytes)?;
    if format.format_version == SHARED_REVISION_STATE_VERSION {
        let state = serde_json::from_slice(&bytes)?;
        validate_shared_revision_archives(conn, lineage, payload, &state)?;
        return Ok(StoredRevisionState::Shared(state));
    }
    // COMPAT(revision-state-v1): retained historical states remain byte-exact.
    let state: CanonicalRevisionState = serde_json::from_slice(&bytes)?;
    if state.format_version != LINEAGE_REVISION_STATE_VERSION {
        return Err(StoreError::Integrity(format!(
            "unsupported lineage revision state version {}",
            state.format_version
        )));
    }
    Ok(StoredRevisionState::Legacy(state))
}

pub(crate) fn load_revision_state(
    conn: &Connection,
    lineage: &LineageId,
    revision: &RevisionRecord,
) -> Result<CanonicalRevisionState> {
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()?;
    match load_revision_payload(
        conn,
        lineage,
        &revision.state_payload_id,
        &mut OperationStats::default(),
    )? {
        StoredRevisionState::Shared(state) => {
            hydrate_shared_revision_state(conn, lineage, &revision.state_payload_id, state)
        }
        StoredRevisionState::Legacy(state) => Ok(state),
    }
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

pub(crate) struct LineageBranchRecord {
    pub(crate) identity: SessionIdentity,
    pub(crate) metadata: BranchMetadata,
    pub(crate) head: StoreHead,
    pub(crate) revision: RevisionRecord,
}

pub(crate) fn load_branch_record(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    include_deleted: bool,
) -> Result<LineageBranchRecord> {
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()?;
    let deleted_filter = if include_deleted {
        ""
    } else {
        " AND deleted_at IS NULL"
    };
    let sql = format!(
        "SELECT parent_session_id, created_at, head_sequence, head_revision_id,
                cwd, mode, reasoning_effort, model, fast_mode,
                session_cost_usd, accounting_json,
                input_tokens, cached_input_tokens, output_tokens, reasoning_tokens
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
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, i64>(14)?,
            ))
        })
        .optional()?
        .ok_or_else(|| StoreError::Integrity(format!("branch {} is not live", branch.as_str())))?;
    let revision_id = RevisionId::from_db(row.3)?;
    let revision = load_revision(conn, lineage, &revision_id)?;
    let metadata = BranchMetadata {
        parent_session_id: row.0.clone(),
        cwd: row.4,
        mode: row.5,
        reasoning_effort: row.6,
        model: row.7,
        fast_mode: row.8,
        session_cost_usd: SessionCostUsd::new(row.9)?.get(),
        accounting_json: row.10,
        input_tokens: nonnegative_u64(row.11, "branch input tokens")?,
        cached_input_tokens: nonnegative_u64(row.12, "branch cached input tokens")?,
        output_tokens: nonnegative_u64(row.13, "branch output tokens")?,
        reasoning_tokens: nonnegative_u64(row.14, "branch reasoning tokens")?,
    };
    let created_at = row.1;
    if created_at < 0 {
        return Err(StoreError::Integrity(
            "lineage branch has negative creation time".into(),
        ));
    }
    Ok(LineageBranchRecord {
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
        revision,
    })
}

pub(crate) fn metadata_for_branch(
    mut metadata: SessionMetadata,
    branch: &BranchMetadata,
) -> Result<SessionMetadata> {
    metadata.cwd = branch.cwd.clone();
    metadata.mode = branch.mode.clone();
    metadata.reasoning_effort = branch.reasoning_effort.clone();
    metadata.model = branch.model.clone();
    metadata.fast_mode = branch.fast_mode;
    metadata.session_cost_usd = SessionCostUsd::new(branch.session_cost_usd)?;
    let accounting = serde_json::from_str(&branch.accounting_json)?;
    metadata.accounting_json = merge_accounting_json(metadata.accounting_json, accounting);
    Ok(metadata)
}

pub(crate) fn effective_revision_metadata(
    state: &StoredRevisionState,
    branch: &BranchMetadata,
) -> Result<SessionMetadata> {
    let mut metadata = state.metadata().clone();
    metadata.checkpoint_json = None;
    metadata.checkpoint_events_json = None;
    metadata_for_branch(metadata, branch)
}

pub(crate) fn revision_metadata_matches(
    prior: &StoredRevisionState,
    prior_branch: &BranchMetadata,
    mut metadata: SessionMetadata,
    branch: &BranchMetadata,
) -> Result<bool> {
    let mut previous = effective_revision_metadata(prior, prior_branch)?;
    // Message equality is established independently by archive roots or legacy values.
    previous.first_user_message = None;
    metadata.first_user_message = None;
    Ok(previous == metadata_for_branch(metadata, branch)?)
}

pub(crate) fn load_branch_snapshot(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    include_deleted: bool,
) -> Result<LineageSessionSnapshot> {
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()?;
    let record = load_branch_record(conn, lineage, branch, include_deleted)?;
    let state = load_revision_state(conn, lineage, &record.revision)?;
    let metadata = metadata_for_branch(state.metadata, &record.metadata)?;
    Ok(LineageSessionSnapshot {
        identity: record.identity,
        metadata,
        head: record.head,
        side_tables: state.side_tables,
        revision_id: record.revision.id,
        history_root: record.revision.history_root,
        transcript_root: record.revision.transcript_root,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LineageSessionHead {
    pub(crate) head: StoreHead,
    pub(crate) revision_id: RevisionId,
    pub(crate) history_root: SequenceRoot,
    pub(crate) transcript_root: SequenceRoot,
}

pub(crate) fn lineage_session_head(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<LineageSessionHead> {
    let (sequence, revision_id) = conn
        .query_row(
            "SELECT head_sequence, head_revision_id FROM lineage_branches
             WHERE lineage_id = ?1 AND session_id = ?2 AND deleted_at IS NULL",
            (lineage.as_str(), branch.as_str()),
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or_else(|| StoreError::Integrity(format!("branch {} is not live", branch.as_str())))?;
    let revision_id = RevisionId::from_db(revision_id)?;
    let revision = load_revision(conn, lineage, &revision_id)?;
    Ok(LineageSessionHead {
        head: StoreHead {
            revision: crate::session_commit::Revision::new(nonnegative_u64(
                sequence,
                "branch head sequence",
            )?),
            history_len: crate::session_commit::HistoryLen::new(revision.history_root.item_count),
            transcript_record_count: crate::session_commit::TranscriptRecordCount::new(
                revision.transcript_root.item_count,
            ),
        },
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
    let snapshot = lineage_session_head(conn, lineage, branch)?;
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
    let snapshot = lineage_session_head(conn, lineage, branch)?;
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

pub(crate) fn lineage_transcript_root_identity(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<(String, u64)> {
    let snapshot = lineage_session_head(conn, lineage, branch)?;
    Ok((
        snapshot.transcript_root.id.as_str().to_owned(),
        snapshot.transcript_root.item_count,
    ))
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
    let snapshot = lineage_session_head(conn, lineage, branch)?;
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
    let snapshot = lineage_session_head(conn, lineage, branch)?;
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
    append_sequence_in(conn, lineage, &prefix, items, compression).map(|(new_root, _)| new_root)
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
