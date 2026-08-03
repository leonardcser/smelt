use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RevisionRecord {
    pub(super) id: RevisionId,
    pub(super) parent_id: Option<RevisionId>,
    pub(super) created_by: BranchId,
    pub(super) operation: Option<LineageOperation>,
    pub(super) history_root: SequenceRoot,
    pub(super) transcript_root: SequenceRoot,
    pub(super) state_payload_id: PayloadId,
    pub(super) commit_fingerprint: Option<String>,
    pub(super) created_at: u64,
}

#[cfg(test)]
impl RevisionRecord {
    pub(crate) fn id(&self) -> &RevisionId {
        &self.id
    }

    pub(crate) fn history_root(&self) -> &SequenceRoot {
        &self.history_root
    }

    pub(crate) fn transcript_root(&self) -> &SequenceRoot {
        &self.transcript_root
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn revision_id(
    lineage: &LineageId,
    parent_id: Option<&RevisionId>,
    created_by: &BranchId,
    operation: Option<LineageOperation>,
    history_root: &SequenceRoot,
    transcript_root: &SequenceRoot,
    state_payload_id: &PayloadId,
    created_at: u64,
) -> RevisionId {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-revision-v1\0");
    encoder.str(lineage.as_str());
    encoder.optional_str(parent_id.map(RevisionId::as_str));
    encoder.str(created_by.as_str());
    encoder.optional_str(operation.map(LineageOperation::as_str));
    encoder.str(history_root.id.as_str());
    encoder.u64(history_root.item_count);
    encoder.u64(history_root.byte_count);
    encoder.str(transcript_root.id.as_str());
    encoder.u64(transcript_root.item_count);
    encoder.u64(transcript_root.byte_count);
    encoder.str(state_payload_id.as_str());
    encoder.u64(created_at);
    RevisionId(encoder.hash())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn make_revision(
    lineage: &LineageId,
    parent_id: Option<RevisionId>,
    created_by: BranchId,
    operation: Option<LineageOperation>,
    history_root: SequenceRoot,
    transcript_root: SequenceRoot,
    state_payload_id: PayloadId,
    created_at: u64,
) -> Result<RevisionRecord> {
    if parent_id.is_some() != operation.is_some() {
        return Err(StoreError::Integrity(
            "initial revisions must have no parent or operation".into(),
        ));
    }
    if history_root.kind != SequenceKind::History {
        return Err(StoreError::Integrity(
            "revision history root has the wrong kind".into(),
        ));
    }
    if transcript_root.kind != SequenceKind::Transcript {
        return Err(StoreError::Integrity(
            "revision transcript root has the wrong kind".into(),
        ));
    }
    let id = revision_id(
        lineage,
        parent_id.as_ref(),
        &created_by,
        operation,
        &history_root,
        &transcript_root,
        &state_payload_id,
        created_at,
    );
    Ok(RevisionRecord {
        id,
        parent_id,
        created_by,
        operation,
        history_root,
        transcript_root,
        state_payload_id,
        commit_fingerprint: None,
        created_at,
    })
}

pub(crate) fn insert_revision(
    conn: &Connection,
    lineage: &LineageId,
    revision: &RevisionRecord,
) -> Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO lineage_revisions (
             lineage_id, revision_id, parent_revision_id, created_by_session_id,
             operation_kind, history_root_id, transcript_root_id, state_payload_id,
             history_len, transcript_record_count, transcript_byte_count,
             commit_fingerprint, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        rusqlite::params![
            lineage.as_str(),
            revision.id.as_str(),
            revision.parent_id.as_ref().map(RevisionId::as_str),
            revision.created_by.as_str(),
            revision
                .operation
                .map(LineageOperation::as_str)
                .unwrap_or("initial"),
            revision.history_root.id.as_str(),
            revision.transcript_root.id.as_str(),
            revision.state_payload_id.as_str(),
            checked_i64(revision.history_root.item_count, "revision history_len")?,
            checked_i64(
                revision.transcript_root.item_count,
                "revision transcript_record_count"
            )?,
            checked_i64(
                revision.transcript_root.byte_count,
                "revision transcript_byte_count"
            )?,
            revision.commit_fingerprint,
            checked_i64(revision.created_at, "revision created_at")?
        ],
    )? > 0;
    let stored = load_revision(conn, lineage, &revision.id)?;
    if &stored != revision {
        return Err(StoreError::Integrity(format!(
            "revision {} conflicts with its content address",
            revision.id.as_str()
        )));
    }
    Ok(inserted)
}

pub(crate) fn load_revision(
    conn: &Connection,
    lineage: &LineageId,
    id: &RevisionId,
) -> Result<RevisionRecord> {
    let row = conn
        .query_row(
            "SELECT parent_revision_id, created_by_session_id, operation_kind,
                    history_root_id, transcript_root_id, state_payload_id,
                    history_len, transcript_record_count, transcript_byte_count,
                    commit_fingerprint, created_at
             FROM lineage_revisions
             WHERE lineage_id = ?1 AND revision_id = ?2",
            (lineage.as_str(), id.as_str()),
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::MissingObject {
            reference: format!("lineage revision {}", id.as_str()),
        })?;
    let parent_id = row.0.map(RevisionId::from_db).transpose()?;
    let created_by = BranchId::new(row.1)?;
    let operation = match row.2.as_str() {
        "initial" => None,
        "append" => Some(LineageOperation::Append),
        "split" => Some(LineageOperation::Split),
        "rewind" => Some(LineageOperation::Rewind),
        other => {
            return Err(StoreError::Integrity(format!(
                "unknown lineage revision operation {other:?}"
            )))
        }
    };
    let history_root = load_root(conn, lineage, &RootId::from_db(row.3)?)?;
    let transcript_root = load_root(conn, lineage, &RootId::from_db(row.4)?)?;
    let state_payload_id = PayloadId::from_db(row.5)?;
    let history_len = nonnegative_u64(row.6, "revision history_len")?;
    let transcript_record_count = nonnegative_u64(row.7, "revision transcript_record_count")?;
    let transcript_byte_count = nonnegative_u64(row.8, "revision transcript_byte_count")?;
    let commit_fingerprint = row.9;
    let created_at = nonnegative_u64(row.10, "revision created_at")?;
    if history_root.kind != SequenceKind::History
        || history_root.item_count != history_len
        || transcript_root.kind != SequenceKind::Transcript
        || transcript_root.item_count != transcript_record_count
        || transcript_root.byte_count != transcript_byte_count
    {
        return Err(StoreError::Integrity(format!(
            "revision {} has inconsistent sequence extents",
            id.as_str()
        )));
    }
    let state = load_payload_ref(conn, lineage, &state_payload_id)?;
    if state.kind != PayloadKind::RevisionState {
        return Err(StoreError::Integrity(format!(
            "revision {} has a non-state payload",
            id.as_str()
        )));
    }
    let mut revision = make_revision(
        lineage,
        parent_id,
        created_by,
        operation,
        history_root,
        transcript_root,
        state_payload_id,
        created_at,
    )?;
    if &revision.id != id {
        return Err(StoreError::Integrity(format!(
            "revision {} has an invalid content address",
            id.as_str()
        )));
    }
    if revision.operation.is_some() != commit_fingerprint.is_some() {
        return Err(StoreError::Integrity(format!(
            "revision {} has inconsistent operation and fingerprint fields",
            id.as_str()
        )));
    }
    revision.commit_fingerprint = commit_fingerprint;
    Ok(revision)
}

pub(crate) fn require_revision_ancestor(
    conn: &Connection,
    lineage: &LineageId,
    descendant: &RevisionId,
    target: &RevisionId,
) -> Result<()> {
    let mut current = descendant.clone();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current.clone()) {
            return Err(StoreError::Integrity(format!(
                "revision ancestry from {} contains a cycle",
                descendant.as_str()
            )));
        }
        let revision = load_revision(conn, lineage, &current)?;
        if &revision.id == target {
            return Ok(());
        }
        let Some(parent) = revision.parent_id else {
            return Err(StoreError::Integrity(format!(
                "revision {} is not an ancestor of {}",
                target.as_str(),
                descendant.as_str()
            )));
        };
        current = parent;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LineageOperation {
    Create,
    Append,
    Split,
    Rewind,
    Fork,
}

impl LineageOperation {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Append => "append",
            Self::Split => "split",
            Self::Rewind => "rewind",
            Self::Fork => "fork",
        }
    }

    fn from_receipt_db(value: &str) -> Result<Self> {
        match value {
            "create" => Ok(Self::Create),
            "append" => Ok(Self::Append),
            "split" => Ok(Self::Split),
            "rewind" => Ok(Self::Rewind),
            "fork" => Ok(Self::Fork),
            other => Err(StoreError::Integrity(format!(
                "unknown lineage receipt operation {other:?}"
            ))),
        }
    }
}

pub(crate) fn commit_fingerprint(
    lineage: &LineageId,
    branch: &BranchId,
    operation: LineageOperation,
    prior: Option<&RevisionId>,
    result: &RevisionId,
    source_branch: Option<&BranchId>,
) -> String {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-commit-v1\0");
    encoder.str(lineage.as_str());
    encoder.str(branch.as_str());
    encoder.str(operation.as_str());
    encoder.optional_str(prior.map(RevisionId::as_str));
    encoder.str(result.as_str());
    encoder.optional_str(source_branch.map(BranchId::as_str));
    encoder.hash()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReceiptCoordinates {
    pub(crate) history_start_idx: Option<u64>,
    pub(crate) history_item_count: Option<u64>,
    pub(crate) transcript_start_idx: Option<u64>,
    pub(crate) transcript_record_count: Option<u64>,
}

impl ReceiptCoordinates {
    fn append(prior: &RevisionRecord, result: &RevisionRecord) -> Result<Self> {
        let history_item_count = result
            .history_root
            .item_count
            .checked_sub(prior.history_root.item_count)
            .ok_or_else(|| {
                StoreError::Integrity("append revision truncated history sequence".into())
            })?;
        let transcript_record_count = result
            .transcript_root
            .item_count
            .checked_sub(prior.transcript_root.item_count)
            .ok_or_else(|| {
                StoreError::Integrity("append revision truncated transcript sequence".into())
            })?;
        Ok(Self {
            history_start_idx: Some(prior.history_root.item_count),
            history_item_count: Some(history_item_count),
            transcript_start_idx: Some(prior.transcript_root.item_count),
            transcript_record_count: Some(transcript_record_count),
        })
    }

    fn validate(self, operation: LineageOperation) -> Result<()> {
        let history_paired = self.history_start_idx.is_some() == self.history_item_count.is_some();
        let transcript_paired =
            self.transcript_start_idx.is_some() == self.transcript_record_count.is_some();
        if !history_paired || !transcript_paired {
            return Err(StoreError::Integrity(
                "lineage receipt has incomplete sequence coordinates".into(),
            ));
        }
        let has_coordinates = self.history_start_idx.is_some();
        if has_coordinates != self.transcript_start_idx.is_some() {
            return Err(StoreError::Integrity(
                "lineage receipt coordinates cover only one sequence".into(),
            ));
        }
        if has_coordinates != matches!(operation, LineageOperation::Append) {
            return Err(StoreError::Integrity(format!(
                "lineage {} receipt has invalid sequence coordinates",
                operation.as_str()
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LineageCommitReceipt {
    pub(crate) fingerprint: String,
    pub(crate) operation: LineageOperation,
    pub(crate) prior_revision_id: Option<RevisionId>,
    pub(crate) result_revision_id: RevisionId,
    pub(crate) coordinates: ReceiptCoordinates,
}

impl LineageCommitReceipt {
    fn validate(&self) -> Result<()> {
        let requires_prior = !matches!(
            self.operation,
            LineageOperation::Create | LineageOperation::Fork
        );
        if self.prior_revision_id.is_some() != requires_prior {
            return Err(StoreError::Integrity(format!(
                "lineage {} receipt has invalid prior revision",
                self.operation.as_str()
            )));
        }
        self.coordinates.validate(self.operation)
    }
}

pub(crate) fn validate_receipt_against_canonical(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    receipt: &LineageCommitReceipt,
) -> Result<()> {
    let result = load_revision(conn, lineage, &receipt.result_revision_id)?;
    let prior = receipt
        .prior_revision_id
        .as_ref()
        .map(|id| load_revision(conn, lineage, id))
        .transpose()?;
    let (fork_parent, initial_revision_id) = conn
        .query_row(
            "SELECT fork_parent_session_id, initial_revision_id
             FROM lineage_branches
             WHERE lineage_id = ?1 AND session_id = ?2",
            (lineage.as_str(), branch.as_str()),
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or_else(|| StoreError::Integrity("lineage receipt branch is missing".into()))?;
    let initial_revision_id = RevisionId::from_db(initial_revision_id)?;
    let source_branch = if receipt.operation == LineageOperation::Fork {
        let source = fork_parent.map(BranchId::new).transpose()?.ok_or_else(|| {
            StoreError::Integrity("lineage fork receipt has no source branch".into())
        })?;
        let source_exists = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM lineage_branches
                 WHERE lineage_id = ?1 AND session_id = ?2
             )",
            (lineage.as_str(), source.as_str()),
            |row| row.get::<_, bool>(0),
        )?;
        if !source_exists {
            return Err(StoreError::Integrity(
                "lineage fork receipt source branch is missing".into(),
            ));
        }
        Some(source)
    } else {
        None
    };
    let expected_fingerprint = commit_fingerprint(
        lineage,
        branch,
        receipt.operation,
        receipt.prior_revision_id.as_ref(),
        &receipt.result_revision_id,
        source_branch.as_ref(),
    );
    if receipt.fingerprint != expected_fingerprint {
        return Err(StoreError::Integrity(format!(
            "lineage {} receipt has an invalid fingerprint",
            receipt.operation.as_str()
        )));
    }

    match receipt.operation {
        LineageOperation::Create => {
            if initial_revision_id != result.id
                || result.parent_id.is_some()
                || result.operation.is_some()
                || result.created_by != *branch
            {
                return Err(StoreError::Integrity(
                    "lineage create receipt does not reference its initial revision".into(),
                ));
            }
        }
        LineageOperation::Append => {
            let prior = prior.as_ref().expect("receipt shape was validated");
            if result.parent_id.as_ref() != receipt.prior_revision_id.as_ref()
                || result.operation != Some(receipt.operation)
                || result.created_by != *branch
                || result.commit_fingerprint.as_deref() != Some(receipt.fingerprint.as_str())
                || receipt.coordinates != ReceiptCoordinates::append(prior, &result)?
            {
                return Err(StoreError::Integrity(format!(
                    "lineage {} receipt disagrees with its revisions",
                    receipt.operation.as_str()
                )));
            }
        }
        LineageOperation::Split => {
            if result.parent_id.as_ref() != receipt.prior_revision_id.as_ref()
                || result.operation != Some(LineageOperation::Split)
                || result.created_by != *branch
                || result.commit_fingerprint.as_deref() != Some(receipt.fingerprint.as_str())
            {
                return Err(StoreError::Integrity(
                    "lineage split receipt disagrees with its revision".into(),
                ));
            }
        }
        LineageOperation::Rewind => {
            let prior_id = receipt
                .prior_revision_id
                .as_ref()
                .expect("receipt shape was validated");
            let is_derived_revision = result.parent_id.as_ref() == Some(prior_id)
                && result.operation == Some(LineageOperation::Rewind)
                && result.created_by == *branch
                && result.commit_fingerprint.as_deref() == Some(receipt.fingerprint.as_str());
            if !is_derived_revision {
                require_revision_ancestor(conn, lineage, prior_id, &result.id)?;
            }
        }
        LineageOperation::Fork => {
            if initial_revision_id != result.id {
                return Err(StoreError::Integrity(
                    "lineage fork receipt disagrees with branch creation revision".into(),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn insert_receipt(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    receipt: &LineageCommitReceipt,
    created_at: u64,
) -> Result<()> {
    receipt.validate()?;
    conn.execute(
        "INSERT INTO lineage_commit_receipts (
             lineage_id, session_id, fingerprint, operation_kind,
             prior_revision_id, result_revision_id,
             history_start_idx, history_item_count,
             transcript_start_idx, transcript_record_count, turn_id, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11)",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            receipt.fingerprint,
            receipt.operation.as_str(),
            receipt.prior_revision_id.as_ref().map(RevisionId::as_str),
            receipt.result_revision_id.as_str(),
            receipt
                .coordinates
                .history_start_idx
                .map(|value| checked_i64(value, "receipt history_start_idx"))
                .transpose()?,
            receipt
                .coordinates
                .history_item_count
                .map(|value| checked_i64(value, "receipt history_item_count"))
                .transpose()?,
            receipt
                .coordinates
                .transcript_start_idx
                .map(|value| checked_i64(value, "receipt transcript_start_idx"))
                .transpose()?,
            receipt
                .coordinates
                .transcript_record_count
                .map(|value| checked_i64(value, "receipt transcript_record_count"))
                .transpose()?,
            checked_i64(created_at, "receipt created_at")?
        ],
    )?;
    Ok(())
}

pub(crate) fn load_receipt(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
) -> Result<Option<LineageCommitReceipt>> {
    conn.query_row(
        "SELECT operation_kind, prior_revision_id, result_revision_id,
                history_start_idx, history_item_count,
                transcript_start_idx, transcript_record_count
         FROM lineage_commit_receipts
         WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
        (lineage.as_str(), branch.as_str(), fingerprint),
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        },
    )
    .optional()?
    .map(|row| {
        let operation = LineageOperation::from_receipt_db(&row.0)?;
        let coordinates = ReceiptCoordinates {
            history_start_idx: row
                .3
                .map(|value| nonnegative_u64(value, "receipt history_start_idx"))
                .transpose()?,
            history_item_count: row
                .4
                .map(|value| nonnegative_u64(value, "receipt history_item_count"))
                .transpose()?,
            transcript_start_idx: row
                .5
                .map(|value| nonnegative_u64(value, "receipt transcript_start_idx"))
                .transpose()?,
            transcript_record_count: row
                .6
                .map(|value| nonnegative_u64(value, "receipt transcript_record_count"))
                .transpose()?,
        };
        let receipt = LineageCommitReceipt {
            fingerprint: fingerprint.to_owned(),
            operation,
            prior_revision_id: row.1.map(RevisionId::from_db).transpose()?,
            result_revision_id: RevisionId::from_db(row.2)?,
            coordinates,
        };
        receipt.validate()?;
        validate_receipt_against_canonical(conn, lineage, branch, &receipt)?;
        Ok(receipt)
    })
    .transpose()
}

pub(crate) fn branch_head_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    include_deleted: bool,
) -> Result<RevisionId> {
    let sql = if include_deleted {
        "SELECT head_revision_id FROM lineage_branches
         WHERE lineage_id = ?1 AND session_id = ?2"
    } else {
        "SELECT head_revision_id FROM lineage_branches
         WHERE lineage_id = ?1 AND session_id = ?2 AND deleted_at IS NULL"
    };
    let value = conn
        .query_row(sql, (lineage.as_str(), branch.as_str()), |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .ok_or_else(|| StoreError::Integrity(format!("branch {} is not live", branch.as_str())))?;
    RevisionId::from_db(value)
}

#[cfg(test)]
pub(crate) fn branch_head(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<RevisionRecord> {
    let id = branch_head_in(conn, lineage, branch, false)?;
    load_revision(conn, lineage, &id)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BranchMetadata {
    pub(crate) parent_session_id: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) fast_mode: Option<bool>,
    pub(crate) session_cost_usd: f64,
    pub(crate) input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_tokens: u64,
    pub(crate) accounting_json: String,
}

impl BranchMetadata {
    fn validate(&self) -> Result<()> {
        if !self.session_cost_usd.is_finite() || self.session_cost_usd < 0.0 {
            return Err(StoreError::Integrity(
                "branch session cost must be finite and nonnegative".into(),
            ));
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_initial_branch_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    metadata: &BranchMetadata,
    history_root: SequenceRoot,
    transcript_root: SequenceRoot,
    state_bytes: &[u8],
    branch_created_at: u64,
    revision_created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt)> {
    metadata.validate()?;
    let state_payload = put_payload(
        conn,
        lineage,
        PayloadKind::RevisionState,
        state_bytes,
        ObjectCompression::default(),
        &mut OperationStats::default(),
    )?;
    let revision = make_revision(
        lineage,
        None,
        branch.clone(),
        None,
        history_root,
        transcript_root,
        state_payload.id,
        revision_created_at,
    )?;
    conn.execute(
        "INSERT INTO lineage_branches (
             lineage_id, session_id, fork_parent_session_id, parent_session_id,
             initial_revision_id, head_revision_id, head_sequence, next_turn_id,
             created_at, updated_at, deleted_at,
             cwd, mode, reasoning_effort, model, fast_mode,
             session_cost_usd, input_tokens, cached_input_tokens,
             output_tokens, reasoning_tokens, accounting_json
         ) VALUES (
             ?1, ?2, NULL, ?3, ?4, ?4, 1, 1, ?5, ?6, NULL,
             ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
         )",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            metadata.parent_session_id,
            revision.id.as_str(),
            checked_i64(branch_created_at, "branch created_at")?,
            checked_i64(revision_created_at, "branch updated_at")?,
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
        ],
    )?;
    insert_revision(conn, lineage, &revision)?;
    conn.execute(
        "INSERT INTO lineage_branch_revisions (
             lineage_id, session_id, branch_sequence, revision_id
         ) VALUES (?1, ?2, 1, ?3)",
        (lineage.as_str(), branch.as_str(), revision.id.as_str()),
    )?;
    let receipt = LineageCommitReceipt {
        fingerprint: commit_fingerprint(
            lineage,
            branch,
            LineageOperation::Create,
            None,
            &revision.id,
            None,
        ),
        operation: LineageOperation::Create,
        prior_revision_id: None,
        result_revision_id: revision.id.clone(),
        coordinates: ReceiptCoordinates::default(),
    };
    insert_receipt(conn, lineage, branch, &receipt, revision_created_at)?;
    Ok((revision, receipt))
}

#[cfg(test)]
pub(crate) fn create_initial_branch(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    metadata: &BranchMetadata,
    state_bytes: &[u8],
    created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt)> {
    let tx = conn.transaction()?;
    let history_root = empty_sequence(&tx, lineage, SequenceKind::History)?;
    let transcript_root = empty_sequence(&tx, lineage, SequenceKind::Transcript)?;
    let result = create_initial_branch_in(
        &tx,
        lineage,
        branch,
        metadata,
        history_root,
        transcript_root,
        state_bytes,
        created_at,
        created_at,
    )?;
    tx.commit()?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn commit_revision_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    expected: &RevisionId,
    history_root: &SequenceRoot,
    transcript_root: &SequenceRoot,
    state_bytes: &[u8],
    operation: LineageOperation,
    created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt)> {
    if matches!(operation, LineageOperation::Create | LineageOperation::Fork) {
        return Err(StoreError::Integrity(
            "revision commit uses an invalid operation kind".into(),
        ));
    }
    let prior_revision = load_revision(conn, lineage, expected)?;
    let stored_history = load_matching_root(conn, lineage, history_root)?;
    let stored_transcript = load_matching_root(conn, lineage, transcript_root)?;
    let state_object_hash = sha256_hex(state_bytes);
    let state_payload_id = payload_id(
        lineage,
        PayloadKind::RevisionState,
        &state_object_hash,
        state_bytes.len() as u64,
    );
    let mut revision = make_revision(
        lineage,
        Some(expected.clone()),
        branch.clone(),
        Some(operation),
        stored_history,
        stored_transcript,
        state_payload_id,
        created_at,
    )?;
    let coordinates = match operation {
        LineageOperation::Append => ReceiptCoordinates::append(&prior_revision, &revision)?,
        LineageOperation::Split | LineageOperation::Rewind => ReceiptCoordinates::default(),
        LineageOperation::Create | LineageOperation::Fork => unreachable!("validated above"),
    };
    let receipt = LineageCommitReceipt {
        fingerprint: commit_fingerprint(
            lineage,
            branch,
            operation,
            Some(expected),
            &revision.id,
            None,
        ),
        operation,
        prior_revision_id: Some(expected.clone()),
        result_revision_id: revision.id.clone(),
        coordinates,
    };
    revision.commit_fingerprint = Some(receipt.fingerprint.clone());
    if let Some(stored) = load_receipt(conn, lineage, branch, &receipt.fingerprint)? {
        if stored != receipt {
            return Err(StoreError::Integrity(
                "lineage commit fingerprint collision".into(),
            ));
        }
        return Ok((
            load_revision(conn, lineage, &stored.result_revision_id)?,
            stored,
        ));
    }
    let current = branch_head_in(conn, lineage, branch, false)?;
    if &current != expected {
        return Err(StoreError::Integrity(format!(
            "branch {} moved from expected revision {} to {}",
            branch.as_str(),
            expected.as_str(),
            current.as_str()
        )));
    }
    let state_payload = put_payload(
        conn,
        lineage,
        PayloadKind::RevisionState,
        state_bytes,
        ObjectCompression::default(),
        &mut OperationStats::default(),
    )?;
    if state_payload.id != revision.state_payload_id {
        return Err(StoreError::Integrity(
            "revision state payload changed during publication".into(),
        ));
    }
    insert_revision(conn, lineage, &revision)?;
    let branch_sequence = conn
        .query_row(
            "UPDATE lineage_branches
             SET head_revision_id = ?1, head_sequence = head_sequence + 1, updated_at = ?2
             WHERE lineage_id = ?3 AND session_id = ?4
               AND head_revision_id = ?5 AND deleted_at IS NULL
             RETURNING head_sequence",
            rusqlite::params![
                revision.id.as_str(),
                checked_i64(created_at, "branch updated_at")?,
                lineage.as_str(),
                branch.as_str(),
                expected.as_str()
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Integrity("branch head compare-and-swap failed".into()))?;
    conn.execute(
        "INSERT INTO lineage_branch_revisions (
             lineage_id, session_id, branch_sequence, revision_id
         ) VALUES (?1, ?2, ?3, ?4)",
        (
            lineage.as_str(),
            branch.as_str(),
            branch_sequence,
            revision.id.as_str(),
        ),
    )?;
    insert_receipt(conn, lineage, branch, &receipt, created_at)?;
    Ok((revision, receipt))
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn commit_revision(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    expected: &RevisionId,
    history_root: &SequenceRoot,
    transcript_root: &SequenceRoot,
    state_bytes: &[u8],
    operation: LineageOperation,
    created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt)> {
    let tx = conn.transaction()?;
    let result = commit_revision_in(
        &tx,
        lineage,
        branch,
        expected,
        history_root,
        transcript_root,
        state_bytes,
        operation,
        created_at,
    )?;
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
pub(crate) fn merge_operation_stats(left: &mut OperationStats, right: OperationStats) {
    left.nodes_read += right.nodes_read;
    left.nodes_written += right.nodes_written;
    left.roots_written += right.roots_written;
    left.payloads_read += right.payloads_read;
    left.payloads_written += right.payloads_written;
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn append_revision(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    expected: &RevisionId,
    history_items: &[Vec<u8>],
    transcript_items: &[Vec<u8>],
    state_bytes: &[u8],
    operation: LineageOperation,
    compression: ObjectCompression,
    created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt, OperationStats)> {
    if !matches!(operation, LineageOperation::Append) {
        return Err(StoreError::Integrity(
            "sequence append uses an invalid revision operation".into(),
        ));
    }
    let tx = conn.transaction()?;
    let prior = load_revision(&tx, lineage, expected)?;
    let (history_root, mut stats) = append_sequence_in(
        &tx,
        lineage,
        &prior.history_root,
        history_items,
        compression,
    )?;
    let (transcript_root, transcript_stats) = append_sequence_in(
        &tx,
        lineage,
        &prior.transcript_root,
        transcript_items,
        compression,
    )?;
    merge_operation_stats(&mut stats, transcript_stats);
    let (revision, receipt) = commit_revision_in(
        &tx,
        lineage,
        branch,
        expected,
        &history_root,
        &transcript_root,
        state_bytes,
        operation,
        created_at,
    )?;
    tx.commit()?;
    Ok((revision, receipt, stats))
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn split_revision(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    expected: &RevisionId,
    history_len: u64,
    transcript_record_count: u64,
    state_bytes: &[u8],
    operation: LineageOperation,
    created_at: u64,
) -> Result<(RevisionRecord, LineageCommitReceipt, OperationStats)> {
    if !matches!(
        operation,
        LineageOperation::Split | LineageOperation::Rewind
    ) {
        return Err(StoreError::Integrity(
            "sequence split uses an invalid revision operation".into(),
        ));
    }
    let tx = conn.transaction()?;
    let prior = load_revision(&tx, lineage, expected)?;
    let ((history_root, _), mut stats) =
        split_sequence_in(&tx, lineage, &prior.history_root, history_len)?;
    let ((transcript_root, _), transcript_stats) = split_sequence_in(
        &tx,
        lineage,
        &prior.transcript_root,
        transcript_record_count,
    )?;
    merge_operation_stats(&mut stats, transcript_stats);
    let (revision, receipt) = commit_revision_in(
        &tx,
        lineage,
        branch,
        expected,
        &history_root,
        &transcript_root,
        state_bytes,
        operation,
        created_at,
    )?;
    tx.commit()?;
    Ok((revision, receipt, stats))
}

pub(super) const LINEAGE_REVISION_STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct CanonicalRevisionState {
    pub(super) format_version: u32,
    pub(super) metadata: SessionMetadata,
    pub(super) side_tables: SideTableSuffixes,
}
