use super::*;

fn receipt_result_id(
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
    command_kind: &str,
    save_json: &str,
    revision: &RevisionId,
) -> String {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-session-receipt-result-v1\0");
    encoder.str(lineage.as_str());
    encoder.str(branch.as_str());
    encoder.str(fingerprint);
    encoder.str(command_kind);
    encoder.str(save_json);
    encoder.str(revision.as_str());
    encoder.hash()
}

fn validate_receipt_result(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    receipt: &SaveReceipt,
    revision: &RevisionRecord,
) -> Result<()> {
    if receipt.session_id != branch.as_str()
        || receipt.lineage_id.as_deref() != Some(lineage.as_str())
        || receipt.current.history_len.get() != revision.history_root.item_count
        || receipt.current.transcript_record_count.get() != revision.transcript_root.item_count
        || receipt.history_text_bytes != revision.history_root.byte_count()
        || branch_revision_at_sequence(conn, lineage, branch, receipt.current.revision.get())?
            != revision.id
    {
        return Err(StoreError::Integrity(
            "session receipt result does not match its exact revision".into(),
        ));
    }
    Ok(())
}

pub(crate) fn load_session_receipt_result(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
) -> Result<Option<SessionCommitResult>> {
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()?;
    if crate::schema::user_version(conn)? == 3 {
        return Ok(None);
    }
    let row = conn
        .query_row(
            "SELECT receipt.command_kind, receipt.save_receipt_json,
                result.result_revision_id, result.result_id
         FROM lineage_session_receipt_results result
         JOIN lineage_session_receipts receipt ON receipt.lineage_id = result.lineage_id
           AND receipt.session_id = result.session_id AND receipt.fingerprint = result.fingerprint
         WHERE result.lineage_id = ?1 AND result.session_id = ?2 AND result.fingerprint = ?3",
            (lineage.as_str(), branch.as_str(), fingerprint),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, save_json, id, proof)) = row else {
        return Ok(None);
    };
    let id = RevisionId::from_db(id)?;
    if proof != receipt_result_id(lineage, branch, fingerprint, &kind, &save_json, &id) {
        return Err(StoreError::Integrity(
            "session receipt result has an invalid content address".into(),
        ));
    }
    let receipt: SaveReceipt = serde_json::from_str(&save_json)?;
    let revision = load_revision(conn, lineage, &id)?;
    validate_receipt_result(conn, lineage, branch, &receipt, &revision)?;
    if !matches!(
        load_revision_envelope(conn, lineage, &revision, &mut OperationStats::default())?,
        StoredRevisionState::Shared(_)
    ) {
        return Err(StoreError::Integrity(
            "session receipt result has no verified shared archive state".into(),
        ));
    }
    Ok(Some(SessionCommitResult {
        receipt,
        revision_id: id.as_str().to_owned(),
    }))
}

pub(crate) fn retain_session_receipt_result(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
    receipt: SaveReceipt,
    compression: ObjectCompression,
) -> Result<SessionCommitResult> {
    if conn.is_autocommit() {
        return Err(StoreError::Integrity(
            "session receipt result publication requires a write transaction".into(),
        ));
    }
    let version = crate::schema::user_version(conn)?;
    if version != crate::schema::LINEAGE_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: version,
            expected: crate::schema::LINEAGE_SCHEMA_VERSION,
        });
    }
    if let Some(stored) = load_session_receipt_result(conn, lineage, branch, fingerprint)? {
        if stored.receipt != receipt {
            return Err(StoreError::Integrity(
                "session receipt result changed its receipt".into(),
            ));
        }
        return Ok(stored);
    }
    let (kind, save_json) = conn.query_row(
        "SELECT command_kind, save_receipt_json FROM lineage_session_receipts
         WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
        (lineage.as_str(), branch.as_str(), fingerprint),
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    if serde_json::from_str::<SaveReceipt>(&save_json)? != receipt {
        return Err(StoreError::Integrity(
            "session receipt result changed its receipt".into(),
        ));
    }
    let id = branch_revision_at_sequence(conn, lineage, branch, receipt.current.revision.get())?;
    let revision = load_revision(conn, lineage, &id)?;
    validate_receipt_result(conn, lineage, branch, &receipt, &revision)?;
    if !matches!(
        load_revision_for_save(conn, lineage, &revision, compression)?,
        StoredRevisionState::Shared(_)
    ) {
        return Err(StoreError::Integrity(
            "session receipt result has no verified shared archive state".into(),
        ));
    }
    conn.execute(
        "INSERT INTO lineage_session_receipt_results
         (lineage_id, session_id, fingerprint, result_revision_id, result_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            lineage.as_str(),
            branch.as_str(),
            fingerprint,
            id.as_str(),
            receipt_result_id(lineage, branch, fingerprint, &kind, &save_json, &id),
        ),
    )?;
    Ok(SessionCommitResult {
        receipt,
        revision_id: id.as_str().to_owned(),
    })
}

pub(crate) fn verify_session_receipt_results(conn: &Connection, lineage: &LineageId) -> Result<()> {
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()?;
    if crate::schema::user_version(conn)? == 3 {
        return Ok(());
    }
    let owners = conn.prepare(
        "SELECT session_id, fingerprint FROM lineage_session_receipt_results WHERE lineage_id = ?1",
    )?.query_map([lineage.as_str()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut verified = HashSet::new();
    for (branch, fingerprint) in owners {
        let branch = BranchId::new(branch)?;
        let result = load_session_receipt_result(conn, lineage, &branch, &fingerprint)?
            .ok_or_else(|| {
                StoreError::Integrity(
                    "session receipt result disappeared from read snapshot".into(),
                )
            })?;
        if !verified.insert(result.revision_id.clone()) {
            continue;
        }
        let id = RevisionId::from_db(result.revision_id)?;
        let revision = load_revision(conn, lineage, &id)?;
        load_revision_state(conn, lineage, &revision)?;
        validate_sequence(conn, lineage, &revision.history_root)?;
        validate_sequence(conn, lineage, &revision.transcript_root)?;
        validate_transcript_indexes(conn, lineage, &revision.transcript_root)?;
    }
    Ok(())
}
