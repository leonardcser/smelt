use super::*;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct PersistedLineageSessionReceipt {
    save: SaveReceipt,
    turn_id: Option<TurnId>,
    turn_state: Option<TurnState>,
    turn_payload: Option<serde_json::Value>,
}

pub(crate) fn load_session_receipt(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
    command_kind: &str,
) -> Result<Option<PersistedLineageSessionReceipt>> {
    let row = conn
        .query_row(
            "SELECT command_kind, save_receipt_json, turn_id, turn_state, turn_payload_json
             FROM lineage_session_receipts
             WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
            (lineage.as_str(), branch.as_str(), fingerprint),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_kind, save_json, turn_id, turn_state, turn_payload)) = row else {
        return Ok(None);
    };
    if stored_kind != command_kind {
        return Err(StoreError::Integrity(
            "lineage session receipt fingerprint changed command kind".into(),
        ));
    }
    let turn_id = turn_id
        .map(|value| nonnegative_u64(value, "session receipt turn id"))
        .transpose()?
        .map(TurnId::new);
    let turn_state = turn_state
        .map(|value| {
            TurnState::from_db(&value).ok_or_else(|| {
                StoreError::Integrity(format!("invalid session receipt turn state {value:?}"))
            })
        })
        .transpose()?;
    Ok(Some(PersistedLineageSessionReceipt {
        save: serde_json::from_str(&save_json)?,
        turn_id,
        turn_state,
        turn_payload: turn_payload
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
    }))
}

pub(crate) fn recover_lineage_session_commit(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &SessionCommit,
) -> std::result::Result<Option<SaveReceipt>, SessionCommitFailure> {
    let fingerprint = crate::session_commit_fingerprint(command)?;
    Ok(
        load_session_receipt(conn, lineage, branch, &fingerprint, "save")
            .map_err(store_failure)?
            .map(|receipt| receipt.save),
    )
}

pub(crate) fn recover_compact_session(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &crate::CompactSessionCommit,
) -> std::result::Result<Option<SessionCommitResult>, SessionCommitFailure> {
    let fingerprint = crate::compact_session_commit_fingerprint(command)?;
    Ok(
        recover_compact_receipt_in(conn, lineage, branch, &fingerprint, "save")?
            .map(|(_, result)| result),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_session_receipt(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
    command_kind: &str,
    receipt: &SaveReceipt,
    turn_id: Option<TurnId>,
    turn_state: Option<TurnState>,
    turn_payload: Option<&serde_json::Value>,
    created_at: u64,
) -> Result<()> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO lineage_session_receipts (
             lineage_id, session_id, fingerprint, command_kind, save_receipt_json,
             turn_id, turn_state, turn_payload_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            fingerprint,
            command_kind,
            serde_json::to_string(receipt)?,
            turn_id
                .map(TurnId::get)
                .map(|value| checked_i64(value, "session receipt turn id"))
                .transpose()?,
            turn_state.map(TurnState::as_str),
            turn_payload.map(serde_json::to_string).transpose()?,
            checked_i64(created_at, "session receipt created_at")?,
        ],
    )?;
    if inserted == 0 {
        let stored = load_session_receipt(conn, lineage, branch, fingerprint, command_kind)?
            .ok_or_else(|| StoreError::Integrity("session receipt disappeared".into()))?;
        let expected = PersistedLineageSessionReceipt {
            save: receipt.clone(),
            turn_id,
            turn_state,
            turn_payload: turn_payload.cloned(),
        };
        if serde_json::to_value(stored)? != serde_json::to_value(expected)? {
            return Err(StoreError::Integrity(
                "lineage session receipt fingerprint collision".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn apply_lineage_session_commit<C: LineageSavepoint>(
    conn: &mut C,
    lineage: &LineageId,
    branch: &BranchId,
    command: &SessionCommit,
    compression: ObjectCompression,
) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
    apply_lineage_session_commit_with_fingerprint(conn, lineage, branch, command, compression)
        .map(|(_, receipt)| receipt)
}

pub(crate) fn apply_lineage_session_commit_with_fingerprint<C: LineageSavepoint>(
    conn: &mut C,
    lineage: &LineageId,
    branch: &BranchId,
    command: &SessionCommit,
    compression: ObjectCompression,
) -> std::result::Result<(String, SaveReceipt), SessionCommitFailure> {
    let _perf = smelt_perf::perf::begin("store:lineage:apply_session_commit");
    crate::session_command::validate_session_commit(command)?;
    if command.session_id != branch.as_str() {
        return Err(SessionCommitFailure::SessionMismatch {
            expected: branch.as_str().to_owned(),
            actual: Some(command.session_id.clone()),
        });
    }
    let created_at = u64::try_from(command.metadata.updated_at).map_err(|_| {
        SessionCommitFailure::InvalidCommand {
            message: "lineage revision timestamp is negative".into(),
        }
    })?;
    let branch_created_at = u64::try_from(command.identity.created_at).map_err(|_| {
        SessionCommitFailure::InvalidCommand {
            message: "lineage branch creation timestamp is negative".into(),
        }
    })?;
    let branch_metadata = branch_metadata_from_session(&command.identity, &command.metadata)
        .map_err(store_failure)?;
    let command_fingerprint = crate::session_command::session_commit_fingerprint(command)?;
    let tx = conn
        .lineage_savepoint()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    if let Some(stored) = load_session_receipt(&tx, lineage, branch, &command_fingerprint, "save")
        .map_err(store_failure)?
    {
        tx.commit()
            .map_err(StoreError::from)
            .map_err(store_failure)?;
        return Ok((command_fingerprint, stored.save));
    }
    let existing = load_branch_record(&tx, lineage, branch, false)
        .optional_store()
        .map_err(store_failure)?;

    if existing.is_none() {
        if command.expected != StoreHead::default() {
            return Err(SessionCommitFailure::StaleBase {
                expected: command.expected,
                current: StoreHead::default(),
            });
        }
        if command.history.start != HistoryIndex::ZERO {
            return Err(SessionCommitFailure::InvalidHistorySuffixStart {
                start: command.history.start,
                current_len: crate::session_commit::HistoryLen::ZERO,
            });
        }
        if let Some(records) = &command.transcript_records {
            if records.start != crate::session_commit::TranscriptRecordIndex::ZERO {
                return Err(SessionCommitFailure::InvalidTranscriptRecordSuffix {
                    start: records.start,
                    current_len: crate::session_commit::TranscriptRecordCount::ZERO,
                });
            }
        }
        if command.side_tables.start != HistoryIndex::ZERO {
            return Err(SessionCommitFailure::InvalidSideTableSuffix {
                start: command.side_tables.start,
                final_len: command.history.final_len,
            });
        }
        let history_root = empty_sequence(&tx, lineage, SequenceKind::History)
            .and_then(|empty| {
                append_sequence_in(
                    &tx,
                    lineage,
                    &empty,
                    &serialize_history_items(&tx, &command.history.items, compression)?,
                    compression,
                )
                .map(|(root, _)| root)
            })
            .map_err(store_failure)?;
        let empty_transcript =
            empty_sequence(&tx, lineage, SequenceKind::Transcript).map_err(store_failure)?;
        let transcript_root = match &command.transcript_records {
            Some(records) => {
                let items = serialize_transcript_items(&tx, &records.records, compression)
                    .map_err(store_failure)?;
                append_sequence_in(&tx, lineage, &empty_transcript, &items, compression)
                    .map(|(root, _)| root)
                    .map_err(store_failure)?
            }
            None => empty_transcript,
        };
        let state = prepare_revision_state(
            &tx,
            lineage,
            &command.metadata,
            &command.side_tables,
            None,
            compression,
        )
        .map_err(store_failure)?;
        let receipt = publish_session_save_in(
            &tx,
            lineage,
            branch,
            &command_fingerprint,
            PreparedSessionSave {
                expected: command.expected,
                branch_metadata: &branch_metadata,
                history_root,
                transcript_root,
                state,
                created_at,
                base: SessionSaveBase::Initial { branch_created_at },
            },
        )?;
        tx.commit()
            .map_err(StoreError::from)
            .map_err(store_failure)?;
        return Ok((command_fingerprint, receipt));
    }

    let current = existing.expect("checked above");
    if command.identity != current.identity {
        return Err(SessionCommitFailure::IdentityMismatch {
            stored: current.identity,
            attempted: command.identity.clone(),
        });
    }
    if command.expected.revision == crate::session_commit::Revision::ZERO {
        // Bootstrap equivalence is an explicit full comparison, not a normal commit read.
        let snapshot = load_branch_snapshot(&tx, lineage, branch, false).map_err(store_failure)?;
        let history = sequence_range(
            &tx,
            lineage,
            &current.revision.history_root,
            0,
            current.revision.history_root.item_count,
        )
        .and_then(|(bytes, _)| deserialize_history_items(&tx, bytes))
        .map_err(store_failure)?;
        let mut transcript = deserialize_sequence_range::<StoredTranscriptBlock>(
            &tx,
            lineage,
            &current.revision.transcript_root,
            0,
            current.revision.transcript_root.item_count,
        )
        .map_err(store_failure)?;
        hydrate_transcript_records(&tx, &mut transcript).map_err(store_failure)?;
        let expected_transcript = command
            .transcript_records
            .as_ref()
            .map_or_else(Vec::new, |records| records.records.clone());
        let expected_side = merge_side_tables(&SideTableSuffixes::default(), &command.side_tables);
        if current.head.revision == crate::session_commit::Revision::new(1)
            && history == command.history.items
            && transcript == expected_transcript
            && snapshot.metadata == command.metadata
            && snapshot.side_tables == expected_side
        {
            let receipt = SaveReceipt {
                session_id: branch.as_str().to_owned(),
                previous: StoreHead::default(),
                current: current.head,
                lineage_id: Some(lineage.as_str().to_owned()),
                history_text_bytes: current.revision.history_root.byte_count(),
            };
            insert_session_receipt(
                &tx,
                lineage,
                branch,
                &command_fingerprint,
                "save",
                &receipt,
                None,
                None,
                None,
                created_at,
            )
            .map_err(store_failure)?;
            tx.commit()
                .map_err(StoreError::from)
                .map_err(store_failure)?;
            return Ok((command_fingerprint, receipt));
        }
        return Err(SessionCommitFailure::StaleBase {
            expected: command.expected,
            current: current.head,
        });
    }

    let expected_revision =
        branch_revision_at_sequence(&tx, lineage, branch, command.expected.revision.get())
            .map_err(store_failure)?;
    let prior = load_revision(&tx, lineage, &expected_revision).map_err(store_failure)?;
    if prior.history_root.item_count != command.expected.history_len.get()
        || prior.transcript_root.item_count != command.expected.transcript_record_count.get()
    {
        return Err(SessionCommitFailure::StaleBase {
            expected: command.expected,
            current: current.head,
        });
    }
    let (history_root, transcript_root) = prepare_session_sequences_in(
        &tx,
        lineage,
        (&prior.history_root, &prior.transcript_root),
        &command.history,
        command.transcript_records.as_ref(),
        compression,
    )
    .map_err(store_failure)?;
    let prior_state =
        load_revision_for_save(&tx, lineage, &prior, compression).map_err(store_failure)?;
    let state = prepare_revision_state(
        &tx,
        lineage,
        &command.metadata,
        &command.side_tables,
        Some(&prior_state),
        compression,
    )
    .map_err(store_failure)?;
    let is_append = command.history.start.get() == prior.history_root.item_count
        && command
            .transcript_records
            .as_ref()
            .is_none_or(|records| records.start.get() == prior.transcript_root.item_count);
    let operation = if is_append {
        LineageOperation::Append
    } else {
        LineageOperation::Split
    };
    let receipt = publish_session_save_in(
        &tx,
        lineage,
        branch,
        &command_fingerprint,
        PreparedSessionSave {
            expected: command.expected,
            branch_metadata: &branch_metadata,
            history_root,
            transcript_root,
            state,
            created_at,
            base: SessionSaveBase::Existing {
                current: &current,
                prior: &prior,
                prior_state: &prior_state,
                operation,
            },
        },
    )?;
    tx.commit()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    Ok((command_fingerprint, receipt))
}

fn prepare_session_sequences_in(
    conn: &Connection,
    lineage: &LineageId,
    previous: (&SequenceRoot, &SequenceRoot),
    history: &crate::HistorySuffix,
    records: Option<&crate::TranscriptRecordSuffix>,
    compression: ObjectCompression,
) -> Result<(SequenceRoot, SequenceRoot)> {
    let history_items = serialize_history_items(conn, &history.items, compression)?;
    let history_root = replace_sequence_suffix_in(
        conn,
        lineage,
        previous.0,
        history.start.get(),
        &history_items,
        compression,
    )?;
    let transcript_root = match records {
        Some(records) => replace_sequence_suffix_in(
            conn,
            lineage,
            previous.1,
            records.start.get(),
            &serialize_transcript_items(conn, &records.records, compression)?,
            compression,
        )?,
        None => previous.1.clone(),
    };
    Ok((history_root, transcript_root))
}

pub(crate) fn apply_compact_session_commit<C: LineageSavepoint>(
    conn: &mut C,
    lineage: &LineageId,
    branch: &BranchId,
    command: &crate::CompactSessionCommit,
    compression: ObjectCompression,
) -> std::result::Result<SessionCommitResult, SessionCommitFailure> {
    let _perf = smelt_perf::perf::begin("store:lineage:apply_compact_session_commit");
    let fingerprint = crate::compact_session_commit_fingerprint(command)?;
    if command.session_id != branch.as_str() {
        return Err(SessionCommitFailure::SessionMismatch {
            expected: branch.as_str().to_owned(),
            actual: Some(command.session_id.clone()),
        });
    }
    let tx = conn
        .lineage_savepoint()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    let version = crate::schema::user_version(&tx).map_err(store_failure)?;
    if version != crate::schema::LINEAGE_SCHEMA_VERSION {
        return Err(SessionCommitFailure::UnsupportedSchema {
            found: version,
            expected: crate::schema::LINEAGE_SCHEMA_VERSION,
        });
    }
    if let Some((_, result)) =
        recover_compact_receipt_in(&tx, lineage, branch, &fingerprint, "save")?
    {
        tx.commit()
            .map_err(StoreError::from)
            .map_err(store_failure)?;
        return Ok(result);
    }
    let current = load_branch_record(&tx, lineage, branch, false)
        .optional_store()
        .map_err(store_failure)?;
    let current_head = current
        .as_ref()
        .map_or(StoreHead::default(), |current| current.head);
    if command.expected != current_head {
        return Err(SessionCommitFailure::StaleBase {
            expected: command.expected,
            current: current_head,
        });
    }
    if let Some(current) = &current {
        if command.identity != current.identity {
            return Err(SessionCommitFailure::IdentityMismatch {
                stored: current.identity.clone(),
                attempted: command.identity.clone(),
            });
        }
    }
    let prior_state = current
        .as_ref()
        .map(|current| load_revision_for_save(&tx, lineage, &current.revision, compression))
        .transpose()
        .map_err(store_failure)?;
    let projected_base;
    let archive_base = match (&command.archive_base, &current) {
        (None, None) => None,
        (Some(base), Some(current)) => {
            if base.lineage_id != lineage.as_str() {
                return Err(SessionCommitFailure::InvalidCommand {
                    message: "archive base belongs to another lineage".into(),
                });
            }
            let associated =
                branch_revision_at_sequence(&tx, lineage, branch, base.branch_sequence.get())
                    .map_err(store_failure)?;
            if base.branch_sequence > command.expected.revision
                || associated.as_str() != base.revision_id
            {
                return Err(SessionCommitFailure::InvalidCommand {
                    message: "archive base is not an exact revision owned by this branch".into(),
                });
            }
            let state = if base.revision_id == current.revision.id.as_str() {
                prior_state.as_ref().expect("existing revision state")
            } else {
                let revision = load_revision(
                    &tx,
                    lineage,
                    &RevisionId::from_db(base.revision_id.clone()).map_err(store_failure)?,
                )
                .map_err(store_failure)?;
                projected_base = load_revision_for_save(&tx, lineage, &revision, compression)
                    .map_err(store_failure)?;
                &projected_base
            };
            match state {
                StoredRevisionState::Shared(state) => Some(state),
                _ => {
                    return Err(SessionCommitFailure::Integrity {
                        message: "compact archive base has no verified shared state".into(),
                    })
                }
            }
        }
        _ => {
            return Err(SessionCommitFailure::InvalidCommand {
                message:
                    "existing compact saves require an exact archive base; initial saves have none"
                        .into(),
            })
        }
    };
    let retained_accounting = match (&command.scalars.accounting, &prior_state, &current) {
        (crate::ValueEdit::Retain, Some(state), Some(current)) => {
            effective_revision_metadata(state, &current.metadata)
                .map_err(store_failure)?
                .accounting_json
        }
        _ => None,
    };
    let metadata = command
        .scalars
        .metadata(retained_accounting)
        .map_err(store_failure)?;
    let branch_metadata =
        branch_metadata_from_session(&command.identity, &metadata).map_err(store_failure)?;
    let state = prepare_compact_archives(
        &tx,
        lineage,
        normalize_revision_metadata(metadata).map_err(store_failure)?,
        &command.archives,
        archive_base,
        command.history.final_len.get(),
        compression,
    )
    .map_err(store_failure)?;
    let archives_unchanged = prior_state.as_ref().is_some_and(|prior| match prior {
        StoredRevisionState::Shared(prior) => {
            prior.archives == state.archives
                && prior.first_user_message_root == state.first_user_message_root
        }
        _ => false,
    });
    let prepared = PreparedRevisionState {
        bytes: serde_json::to_vec(&state)
            .map_err(StoreError::from)
            .map_err(store_failure)?,
        metadata: state.metadata,
        archives_unchanged,
    };
    let empty_history;
    let empty_transcript;
    let previous = match &current {
        Some(current) => (
            &current.revision.history_root,
            &current.revision.transcript_root,
        ),
        None => {
            empty_history =
                empty_sequence(&tx, lineage, SequenceKind::History).map_err(store_failure)?;
            empty_transcript =
                empty_sequence(&tx, lineage, SequenceKind::Transcript).map_err(store_failure)?;
            (&empty_history, &empty_transcript)
        }
    };
    let (history_root, transcript_root) = prepare_session_sequences_in(
        &tx,
        lineage,
        previous,
        &command.history,
        command.transcript_records.as_ref(),
        compression,
    )
    .map_err(store_failure)?;
    let base = match &current {
        Some(current) => SessionSaveBase::Existing {
            current,
            prior: &current.revision,
            prior_state: prior_state.as_ref().expect("existing revision state"),
            operation: if command.history.start.get() == current.revision.history_root.item_count
                && command.transcript_records.as_ref().is_none_or(|records| {
                    records.start.get() == current.revision.transcript_root.item_count
                }) {
                LineageOperation::Append
            } else {
                LineageOperation::Split
            },
        },
        None => SessionSaveBase::Initial {
            branch_created_at: command.identity.created_at as u64,
        },
    };
    let receipt = publish_session_save_in(
        &tx,
        lineage,
        branch,
        &fingerprint,
        PreparedSessionSave {
            expected: command.expected,
            branch_metadata: &branch_metadata,
            history_root,
            transcript_root,
            state: prepared,
            created_at: command.scalars.updated_at as u64,
            base,
        },
    )?;
    let result =
        retain_session_receipt_result(&tx, lineage, branch, &fingerprint, receipt, compression)
            .map_err(store_failure)?;
    tx.commit()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    Ok(result)
}

enum SessionSaveBase<'a> {
    Initial {
        branch_created_at: u64,
    },
    Existing {
        current: &'a LineageBranchRecord,
        prior: &'a RevisionRecord,
        prior_state: &'a StoredRevisionState,
        operation: LineageOperation,
    },
}

struct PreparedSessionSave<'a> {
    expected: StoreHead,
    branch_metadata: &'a BranchMetadata,
    history_root: SequenceRoot,
    transcript_root: SequenceRoot,
    state: PreparedRevisionState,
    created_at: u64,
    base: SessionSaveBase<'a>,
}

fn publish_session_save_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
    save: PreparedSessionSave<'_>,
) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
    let (head, history_text_bytes) = match save.base {
        SessionSaveBase::Initial { branch_created_at } => {
            let head = StoreHead {
                revision: crate::session_commit::Revision::new(1),
                history_len: crate::session_commit::HistoryLen::new(save.history_root.item_count),
                transcript_record_count: crate::session_commit::TranscriptRecordCount::new(
                    save.transcript_root.item_count,
                ),
            };
            let history_text_bytes = save.history_root.byte_count();
            create_initial_branch_in(
                conn,
                lineage,
                branch,
                save.branch_metadata,
                save.history_root,
                save.transcript_root,
                &save.state.bytes,
                branch_created_at,
                save.created_at.max(branch_created_at),
            )
            .map_err(store_failure)?;
            (head, history_text_bytes)
        }
        SessionSaveBase::Existing {
            current,
            prior,
            prior_state,
            operation,
        } => {
            let current_was_expected = current.revision.id == prior.id;
            if current_was_expected
                && save.history_root == prior.history_root
                && save.transcript_root == prior.transcript_root
                && save.state.archives_unchanged
                && revision_metadata_matches(
                    prior_state,
                    &current.metadata,
                    save.state.metadata,
                    save.branch_metadata,
                )
                .map_err(store_failure)?
            {
                (save.expected, prior.history_root.byte_count())
            } else {
                let (revision, _) = commit_revision_in(
                    conn,
                    lineage,
                    branch,
                    &prior.id,
                    &save.history_root,
                    &save.transcript_root,
                    &save.state.bytes,
                    operation,
                    save.created_at,
                )
                .map_err(|error| {
                    if !current_was_expected {
                        SessionCommitFailure::StaleBase {
                            expected: save.expected,
                            current: current.head,
                        }
                    } else {
                        store_failure(error)
                    }
                })?;
                if current_was_expected {
                    update_branch_metadata(conn, lineage, branch, save.branch_metadata)
                        .map_err(store_failure)?;
                }
                let head = StoreHead {
                    revision: save.expected.revision.checked_add(1).ok_or_else(|| {
                        SessionCommitFailure::Integrity {
                            message: "lineage branch sequence overflow".into(),
                        }
                    })?,
                    history_len: crate::session_commit::HistoryLen::new(
                        revision.history_root.item_count,
                    ),
                    transcript_record_count: crate::session_commit::TranscriptRecordCount::new(
                        revision.transcript_root.item_count,
                    ),
                };
                (head, revision.history_root.byte_count())
            }
        }
    };
    let receipt = SaveReceipt {
        session_id: branch.as_str().to_owned(),
        previous: save.expected,
        current: head,
        lineage_id: Some(lineage.as_str().to_owned()),
        history_text_bytes,
    };
    insert_session_receipt(
        conn,
        lineage,
        branch,
        fingerprint,
        "save",
        &receipt,
        None,
        None,
        None,
        save.created_at,
    )
    .map_err(store_failure)?;
    Ok(receipt)
}

pub(crate) fn turn_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTurn> {
    let kind = row.get::<_, String>(4)?;
    let state = row.get::<_, String>(5)?;
    Ok(StoredTurn {
        turn_id: TurnId::new(row.get::<_, i64>(0)? as u64),
        submitted_history_idx: HistoryIndex::new(row.get::<_, i64>(1)? as u64),
        submitted_history_hash: row.get(2)?,
        submitted_revision: crate::session_commit::Revision::new(row.get::<_, i64>(3)? as u64),
        kind: TurnKind::from_db(&kind).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                format!("invalid lineage turn kind {kind:?}").into(),
            )
        })?,
        state: TurnState::from_db(&state).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                format!("invalid lineage turn state {state:?}").into(),
            )
        })?,
        continuation_of: row
            .get::<_, Option<i64>>(6)?
            .map(|value| TurnId::new(value as u64)),
        created_at_ms: row.get::<_, i64>(7)? as u64,
        started_at_ms: row.get::<_, Option<i64>>(8)?.map(|value| value as u64),
        finished_at_ms: row.get::<_, Option<i64>>(9)?.map(|value| value as u64),
        terminal_reason: row.get(10)?,
    })
}

pub(crate) fn stored_lineage_turn(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    turn_id: TurnId,
) -> Result<Option<StoredTurn>> {
    conn.query_row(
        "SELECT turn_id, submitted_history_idx, submitted_history_hash,
                submitted_sequence, turn_kind, turn_state, continuation_of,
                created_at_ms, started_at_ms, finished_at_ms, terminal_reason
         FROM lineage_turns
         WHERE lineage_id = ?1 AND session_id = ?2 AND turn_id = ?3",
        (
            lineage.as_str(),
            branch.as_str(),
            checked_i64(turn_id.get(), "turn id")?,
        ),
        turn_from_row,
    )
    .optional()
    .map_err(StoreError::from)
}

pub(crate) fn lineage_latest_terminal_turn_id(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<Option<TurnId>> {
    let value = conn.query_row(
        "SELECT MAX(turn_id) FROM lineage_turns
         WHERE lineage_id = ?1 AND session_id = ?2
           AND turn_state IN ('completed', 'interrupted', 'failed', 'cancelled')",
        (lineage.as_str(), branch.as_str()),
        |row| row.get::<_, Option<i64>>(0),
    )?;
    value
        .map(|value| nonnegative_u64(value, "latest terminal turn id").map(TurnId::new))
        .transpose()
}

pub(crate) fn lineage_last_session_receipt(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<Option<(String, SaveReceipt)>> {
    let row = conn
        .query_row(
            "SELECT fingerprint, save_receipt_json
             FROM lineage_session_receipts
             WHERE lineage_id = ?1 AND session_id = ?2
             ORDER BY created_at DESC, rowid DESC
             LIMIT 1",
            (lineage.as_str(), branch.as_str()),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(fingerprint, receipt)| Ok((fingerprint, serde_json::from_str(&receipt)?)))
        .transpose()
}

pub(crate) fn recover_lineage_submit_turn(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &SubmitTurn,
) -> std::result::Result<Option<SubmitTurnReceipt>, SessionCommitFailure> {
    let fingerprint = crate::session_command::submit_turn_fingerprint(command)?;
    let stored = load_session_receipt(conn, lineage, branch, &fingerprint, "submit_turn")
        .map_err(store_failure)?;
    stored
        .map(|stored| {
            let turn_id = stored
                .turn_id
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "submit-turn receipt has no turn ID".into(),
                })?;
            Ok(SubmitTurnReceipt {
                session: stored.save,
                turn_id,
            })
        })
        .transpose()
}

pub(crate) fn apply_lineage_submit_turn(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &SubmitTurn,
    compression: ObjectCompression,
) -> std::result::Result<SubmitTurnReceipt, SessionCommitFailure> {
    crate::session_command::validate_new_turn(&command.turn, command.session.history.final_len)?;
    let fingerprint = crate::session_command::submit_turn_fingerprint(command)?;
    if let Some(receipt) = recover_lineage_submit_turn(conn, lineage, branch, command)? {
        return Ok(receipt);
    }
    let mut tx =
        crate::write_transaction::begin_write(conn, "commit turn").map_err(store_failure)?;
    let session =
        apply_lineage_session_commit(&mut tx, lineage, branch, &command.session, compression)?;
    let receipt =
        publish_turn_submission_in(&tx, lineage, branch, &fingerprint, session, &command.turn)?;
    {
        let _perf = smelt_perf::perf::begin("store:lineage:transaction_commit");
        tx.commit()
            .map_err(StoreError::from)
            .map_err(store_failure)?;
    }
    Ok(receipt)
}

fn publish_turn_submission_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
    session: SaveReceipt,
    turn: &crate::NewTurn,
) -> std::result::Result<SubmitTurnReceipt, SessionCommitFailure> {
    let id = branch_revision_at_sequence(conn, lineage, branch, session.current.revision.get())
        .map_err(store_failure)?;
    let revision = load_revision(conn, lineage, &id).map_err(store_failure)?;
    let turn_id = conn
        .query_row(
            "UPDATE lineage_branches
             SET next_turn_id = next_turn_id + 1
             WHERE lineage_id = ?1 AND session_id = ?2 AND deleted_at IS NULL
             RETURNING next_turn_id - 1",
            (lineage.as_str(), branch.as_str()),
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(StoreError::from)
        .map_err(store_failure)?
        .ok_or_else(|| SessionCommitFailure::Integrity {
            message: "turn ID allocation missed its lineage branch".into(),
        })?;
    let turn_id =
        TurnId::new(nonnegative_u64(turn_id, "allocated turn id").map_err(store_failure)?);
    let (history_bytes, _) = sequence_item(
        conn,
        lineage,
        &revision.history_root,
        turn.submitted_history_idx.get(),
    )
    .map_err(store_failure)?;
    let submitted_item: protocol::HistoryItem = serde_json::from_slice(&history_bytes)
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    let history_hash = crate::history::item_hash(&submitted_item).map_err(store_failure)?;
    let inserted = conn
        .execute(
            "INSERT INTO lineage_turns (
                 lineage_id, session_id, turn_id, submitted_history_idx,
                 submitted_history_hash, submitted_revision_id, submitted_sequence,
                 turn_kind, turn_state, continuation_of, created_at_ms,
                 started_at_ms, finished_at_ms, terminal_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'ready', ?9, ?10, NULL, NULL, NULL)",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                checked_i64(turn_id.get(), "turn id").map_err(store_failure)?,
                checked_i64(turn.submitted_history_idx.get(), "submitted history index")
                    .map_err(store_failure)?,
                history_hash,
                revision.id.as_str(),
                checked_i64(session.current.revision.get(), "submitted sequence")
                    .map_err(store_failure)?,
                turn.kind.as_str(),
                turn.continuation_of
                    .map(TurnId::get)
                    .map(|value| checked_i64(value, "continuation turn id"))
                    .transpose()
                    .map_err(store_failure)?,
                checked_i64(turn.created_at_ms, "turn created_at_ms").map_err(store_failure)?,
            ],
        )
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    if inserted != 1 {
        return Err(SessionCommitFailure::Integrity {
            message: "turn insertion did not write one lineage row".into(),
        });
    }
    insert_session_receipt(
        conn,
        lineage,
        branch,
        fingerprint,
        "submit_turn",
        &session,
        Some(turn_id),
        Some(TurnState::Ready),
        None,
        turn.created_at_ms,
    )
    .map_err(store_failure)?;
    Ok(SubmitTurnReceipt { session, turn_id })
}

pub(crate) fn recover_lineage_turn_transition(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &TurnTransition,
) -> std::result::Result<Option<TurnTransitionReceipt>, SessionCommitFailure> {
    let fingerprint = crate::session_command::turn_transition_fingerprint(command)?;
    let stored = load_session_receipt(conn, lineage, branch, &fingerprint, "turn_transition")
        .map_err(store_failure)?;
    stored
        .map(|stored| {
            let turn_id = stored
                .turn_id
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "turn-transition receipt has no turn ID".into(),
                })?;
            let state = stored
                .turn_state
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "turn-transition receipt has no turn state".into(),
                })?;
            Ok(TurnTransitionReceipt {
                session: stored.save,
                turn_id,
                state,
            })
        })
        .transpose()
}

pub(crate) fn apply_lineage_turn_transition(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &TurnTransition,
    compression: ObjectCompression,
) -> std::result::Result<TurnTransitionReceipt, SessionCommitFailure> {
    crate::session_command::validate_turn_transition(command)?;
    let fingerprint = crate::session_command::turn_transition_fingerprint(command)?;
    if let Some(receipt) = recover_lineage_turn_transition(conn, lineage, branch, command)? {
        return Ok(receipt);
    }
    let mut tx =
        crate::write_transaction::begin_write(conn, "commit turn").map_err(store_failure)?;
    let transition = prepare_turn_transition_in(
        &tx,
        lineage,
        branch,
        command.turn_id,
        command.state,
        command.at_ms,
        command.terminal_reason.as_deref(),
    )?;
    let session =
        apply_lineage_session_commit(&mut tx, lineage, branch, &command.session, compression)?;
    let receipt =
        publish_turn_transition_in(&tx, lineage, branch, &fingerprint, session, transition)?;
    tx.commit()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    Ok(receipt)
}

struct PreparedTurnTransition<'a> {
    current: StoredTurn,
    state: TurnState,
    at_ms: u64,
    terminal_reason: Option<&'a str>,
}

fn prepare_turn_transition_in<'a>(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    turn_id: TurnId,
    state: TurnState,
    at_ms: u64,
    terminal_reason: Option<&'a str>,
) -> std::result::Result<PreparedTurnTransition<'a>, SessionCommitFailure> {
    let current = stored_lineage_turn(conn, lineage, branch, turn_id)
        .map_err(store_failure)?
        .ok_or(SessionCommitFailure::TurnNotFound { turn_id })?;
    let allowed = matches!(
        (current.state, state),
        (TurnState::Ready, TurnState::Running)
            | (TurnState::Ready, TurnState::Failed)
            | (TurnState::Ready, TurnState::Cancelled)
            | (TurnState::Ready, TurnState::Interrupted)
            | (TurnState::Running, TurnState::Completed)
            | (TurnState::Running, TurnState::Failed)
            | (TurnState::Running, TurnState::Cancelled)
            | (TurnState::Running, TurnState::Interrupted)
    );
    if !allowed {
        return Err(SessionCommitFailure::InvalidTurnTransition {
            turn_id,
            from: current.state,
            to: state,
        });
    }
    let minimum_time = current.started_at_ms.unwrap_or(current.created_at_ms);
    if at_ms < minimum_time {
        return Err(SessionCommitFailure::InvalidTurn {
            message: format!("turn transition timestamp {at_ms} precedes {minimum_time}"),
        });
    }
    Ok(PreparedTurnTransition {
        current,
        state,
        at_ms,
        terminal_reason,
    })
}

fn publish_turn_transition_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
    session: SaveReceipt,
    transition: PreparedTurnTransition<'_>,
) -> std::result::Result<TurnTransitionReceipt, SessionCommitFailure> {
    let PreparedTurnTransition {
        current,
        state,
        at_ms,
        terminal_reason,
    } = transition;
    let turn_id = current.turn_id;
    let updated = if state == TurnState::Running {
        conn.execute(
            "UPDATE lineage_turns
             SET turn_state = 'running', started_at_ms = ?1
             WHERE lineage_id = ?2 AND session_id = ?3 AND turn_id = ?4
               AND turn_state = 'ready'",
            rusqlite::params![
                checked_i64(at_ms, "turn transition timestamp").map_err(store_failure)?,
                lineage.as_str(),
                branch.as_str(),
                checked_i64(turn_id.get(), "turn id").map_err(store_failure)?,
            ],
        )
    } else {
        conn.execute(
            "UPDATE lineage_turns
             SET turn_state = ?1, finished_at_ms = ?2, terminal_reason = ?3
             WHERE lineage_id = ?4 AND session_id = ?5 AND turn_id = ?6
               AND turn_state IN ('ready', 'running')",
            rusqlite::params![
                state.as_str(),
                checked_i64(at_ms, "turn transition timestamp").map_err(store_failure)?,
                terminal_reason,
                lineage.as_str(),
                branch.as_str(),
                checked_i64(turn_id.get(), "turn id").map_err(store_failure)?,
            ],
        )
    }
    .map_err(StoreError::from)
    .map_err(store_failure)?;
    if updated != 1 {
        return Err(SessionCommitFailure::Integrity {
            message: format!("turn {} changed during transition", turn_id.get()),
        });
    }
    insert_session_receipt(
        conn,
        lineage,
        branch,
        fingerprint,
        "turn_transition",
        &session,
        Some(turn_id),
        Some(state),
        None,
        at_ms,
    )
    .map_err(store_failure)?;
    conn.execute(
        "INSERT INTO lineage_turn_transitions (
             lineage_id, session_id, fingerprint, turn_id, from_state, to_state,
             transitioned_at_ms, terminal_reason
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            fingerprint,
            checked_i64(turn_id.get(), "turn id").map_err(store_failure)?,
            current.state.as_str(),
            state.as_str(),
            checked_i64(at_ms, "turn transition timestamp").map_err(store_failure)?,
            terminal_reason,
        ],
    )
    .map_err(StoreError::from)
    .map_err(store_failure)?;
    Ok(TurnTransitionReceipt {
        session,
        turn_id,
        state,
    })
}

fn recover_compact_receipt_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
    kind: &str,
) -> std::result::Result<
    Option<(PersistedLineageSessionReceipt, SessionCommitResult)>,
    SessionCommitFailure,
> {
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    let Some(receipt) =
        load_session_receipt(conn, lineage, branch, fingerprint, kind).map_err(store_failure)?
    else {
        return Ok(None);
    };
    let result = load_session_receipt_result(conn, lineage, branch, fingerprint)
        .map_err(store_failure)?
        .ok_or_else(|| SessionCommitFailure::Integrity {
            message: "compact receipt has no retained exact result".into(),
        })?;
    Ok(Some((receipt, result)))
}

pub(crate) fn recover_compact_submit_turn(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &crate::CompactSubmitTurn,
) -> std::result::Result<Option<crate::CompactSubmitTurnResult>, SessionCommitFailure> {
    let fingerprint = crate::compact_submit_turn_fingerprint(command)?;
    recover_compact_submit_turn_in(conn, lineage, branch, &fingerprint)
}

fn recover_compact_submit_turn_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
) -> std::result::Result<Option<crate::CompactSubmitTurnResult>, SessionCommitFailure> {
    recover_compact_receipt_in(conn, lineage, branch, fingerprint, "submit_turn")?
        .map(|(receipt, session)| {
            let turn_id = receipt
                .turn_id
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "compact submission receipt has no turn ID".into(),
                })?;
            Ok(crate::CompactSubmitTurnResult { session, turn_id })
        })
        .transpose()
}

pub(crate) fn apply_compact_submit_turn(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &crate::CompactSubmitTurn,
    compression: ObjectCompression,
) -> std::result::Result<crate::CompactSubmitTurnResult, SessionCommitFailure> {
    let fingerprint = crate::compact_submit_turn_fingerprint(command)?;
    let mut tx = crate::write_transaction::begin_write(conn, "commit compact turn")
        .map_err(store_failure)?;
    if let Some(result) = recover_compact_submit_turn_in(&tx, lineage, branch, &fingerprint)? {
        tx.commit()
            .map_err(StoreError::from)
            .map_err(store_failure)?;
        return Ok(result);
    }
    let session =
        apply_compact_session_commit(&mut tx, lineage, branch, &command.session, compression)?;
    let receipt = publish_turn_submission_in(
        &tx,
        lineage,
        branch,
        &fingerprint,
        session.receipt,
        &command.turn,
    )?;
    let session = retain_session_receipt_result(
        &tx,
        lineage,
        branch,
        &fingerprint,
        receipt.session,
        compression,
    )
    .map_err(store_failure)?;
    tx.commit()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    Ok(crate::CompactSubmitTurnResult {
        session,
        turn_id: receipt.turn_id,
    })
}

pub(crate) fn recover_compact_turn_transition(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &crate::CompactTurnTransition,
) -> std::result::Result<Option<crate::CompactTurnTransitionResult>, SessionCommitFailure> {
    let fingerprint = crate::compact_turn_transition_fingerprint(command)?;
    recover_compact_turn_transition_in(conn, lineage, branch, &fingerprint)
}

fn recover_compact_turn_transition_in(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    fingerprint: &str,
) -> std::result::Result<Option<crate::CompactTurnTransitionResult>, SessionCommitFailure> {
    recover_compact_receipt_in(conn, lineage, branch, fingerprint, "turn_transition")?
        .map(|(receipt, session)| {
            let turn_id = receipt
                .turn_id
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "compact transition receipt has no turn ID".into(),
                })?;
            let state = receipt
                .turn_state
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "compact transition receipt has no turn state".into(),
                })?;
            Ok(crate::CompactTurnTransitionResult {
                session,
                turn_id,
                state,
            })
        })
        .transpose()
}

pub(crate) fn apply_compact_turn_transition(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &crate::CompactTurnTransition,
    compression: ObjectCompression,
) -> std::result::Result<crate::CompactTurnTransitionResult, SessionCommitFailure> {
    let fingerprint = crate::compact_turn_transition_fingerprint(command)?;
    let mut tx = crate::write_transaction::begin_write(conn, "commit compact turn")
        .map_err(store_failure)?;
    if let Some(result) = recover_compact_turn_transition_in(&tx, lineage, branch, &fingerprint)? {
        tx.commit()
            .map_err(StoreError::from)
            .map_err(store_failure)?;
        return Ok(result);
    }
    let transition = prepare_turn_transition_in(
        &tx,
        lineage,
        branch,
        command.turn_id,
        command.state,
        command.at_ms,
        command.terminal_reason.as_deref(),
    )?;
    let session =
        apply_compact_session_commit(&mut tx, lineage, branch, &command.session, compression)?;
    let receipt = publish_turn_transition_in(
        &tx,
        lineage,
        branch,
        &fingerprint,
        session.receipt,
        transition,
    )?;
    let session = retain_session_receipt_result(
        &tx,
        lineage,
        branch,
        &fingerprint,
        receipt.session,
        compression,
    )
    .map_err(store_failure)?;
    tx.commit()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    Ok(crate::CompactTurnTransitionResult {
        session,
        turn_id: receipt.turn_id,
        state: receipt.state,
    })
}

pub(crate) fn lineage_has_nonterminal_turns(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM lineage_turns
             WHERE lineage_id = ?1 AND session_id = ?2
               AND turn_state IN ('ready', 'running')
         )",
        (lineage.as_str(), branch.as_str()),
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

pub(crate) fn recover_lineage_nonterminal_turns(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    at_ms: u64,
) -> Result<Option<StartupRecoveryResult>> {
    let tx = crate::write_transaction::begin_write(conn, "recover interrupted turns")?;
    let mut statement = tx.prepare(
        "SELECT turn_id, turn_state, created_at_ms, started_at_ms
         FROM lineage_turns
         WHERE lineage_id = ?1 AND session_id = ?2
           AND turn_state IN ('ready', 'running')
         ORDER BY turn_id",
    )?;
    let rows = statement.query_map((lineage.as_str(), branch.as_str()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<i64>>(3)?,
        ))
    })?;
    let pending = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    if pending.is_empty() {
        tx.commit()?;
        return Ok(None);
    }
    let previous = load_branch_record(&tx, lineage, branch, false)?;
    let at_ms_sql = checked_i64(at_ms, "startup recovery timestamp")?;
    let updated = tx.execute(
        "UPDATE lineage_turns
         SET turn_state = 'interrupted',
             finished_at_ms = MAX(?1, created_at_ms, COALESCE(started_at_ms, created_at_ms)),
             terminal_reason = 'process_restart'
         WHERE lineage_id = ?2 AND session_id = ?3
           AND turn_state IN ('ready', 'running')",
        (at_ms_sql, lineage.as_str(), branch.as_str()),
    )?;
    if updated != pending.len() {
        return Err(StoreError::Integrity(
            "nonterminal lineage turn count changed during recovery".into(),
        ));
    }
    let next_sequence = previous
        .head
        .revision
        .checked_add(1)
        .ok_or_else(|| StoreError::Integrity("branch sequence overflow".into()))?;
    tx.execute(
        "UPDATE lineage_branches
         SET head_sequence = ?1, updated_at = MAX(updated_at, ?2)
         WHERE lineage_id = ?3 AND session_id = ?4 AND head_revision_id = ?5
           AND deleted_at IS NULL",
        rusqlite::params![
            checked_i64(next_sequence.get(), "recovery branch sequence")?,
            at_ms_sql,
            lineage.as_str(),
            branch.as_str(),
            previous.revision.id.as_str(),
        ],
    )?;
    tx.execute(
        "INSERT INTO lineage_branch_revisions (
             lineage_id, session_id, branch_sequence, revision_id
         ) VALUES (?1, ?2, ?3, ?4)",
        (
            lineage.as_str(),
            branch.as_str(),
            checked_i64(next_sequence.get(), "recovery branch sequence")?,
            previous.revision.id.as_str(),
        ),
    )?;
    let current = StoreHead {
        revision: next_sequence,
        ..previous.head
    };
    let save = SaveReceipt {
        session_id: branch.as_str().to_owned(),
        previous: previous.head,
        current,
        lineage_id: Some(lineage.as_str().to_owned()),
        history_text_bytes: previous.revision.history_root.byte_count(),
    };
    let mut interrupted_turns = Vec::with_capacity(pending.len());
    for (turn_id, from_state, _, _) in pending {
        let turn_id = TurnId::new(nonnegative_u64(turn_id, "recovered turn id")?);
        let from_state = TurnState::from_db(&from_state).ok_or_else(|| {
            StoreError::Integrity(format!("invalid nonterminal turn state {from_state:?}"))
        })?;
        interrupted_turns.push(turn_id);
        let fingerprint = sha256_hex(
            format!(
                "smelt-lineage-startup-recovery-v1\0{}\0{}\0{}\0{}",
                branch.as_str(),
                previous.head.revision.get(),
                turn_id.get(),
                at_ms
            )
            .as_bytes(),
        );
        insert_session_receipt(
            &tx,
            lineage,
            branch,
            &fingerprint,
            "startup_recovery",
            &save,
            Some(turn_id),
            Some(TurnState::Interrupted),
            None,
            at_ms,
        )?;
        retain_session_receipt_result(
            &tx,
            lineage,
            branch,
            &fingerprint,
            save.clone(),
            ObjectCompression::default(),
        )?;
        tx.execute(
            "INSERT INTO lineage_turn_transitions (
                 lineage_id, session_id, fingerprint, turn_id, from_state, to_state,
                 transitioned_at_ms, terminal_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'interrupted', ?6, 'process_restart')",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                fingerprint,
                checked_i64(turn_id.get(), "recovered turn id")?,
                from_state.as_str(),
                at_ms_sql,
            ],
        )?;
    }
    tx.commit()?;
    Ok(Some(StartupRecoveryResult {
        session: SessionCommitResult {
            receipt: save,
            revision_id: previous.revision.id.as_str().to_owned(),
        },
        interrupted_turns,
    }))
}

pub(crate) trait OptionalStore<T> {
    fn optional_store(self) -> Result<Option<T>>;
}

impl<T> OptionalStore<T> for Result<T> {
    fn optional_store(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(StoreError::Integrity(message)) if message.contains("is not live") => Ok(None),
            Err(error) => Err(error),
        }
    }
}

pub(crate) fn rewind_branch(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    expected: &RevisionId,
    target: &RevisionId,
    updated_at: u64,
) -> Result<LineageCommitReceipt> {
    let tx = conn.transaction()?;
    let receipt = LineageCommitReceipt {
        fingerprint: commit_fingerprint(
            lineage,
            branch,
            LineageOperation::Rewind,
            Some(expected),
            target,
            None,
        ),
        operation: LineageOperation::Rewind,
        prior_revision_id: Some(expected.clone()),
        result_revision_id: target.clone(),
        coordinates: ReceiptCoordinates::default(),
    };
    if let Some(stored) = load_receipt(&tx, lineage, branch, &receipt.fingerprint)? {
        if stored == receipt {
            return Ok(stored);
        }
        return Err(StoreError::Integrity(
            "lineage rewind fingerprint collision".into(),
        ));
    }
    let current = branch_head_in(&tx, lineage, branch, false)?;
    if &current != expected {
        return Err(StoreError::Integrity("branch moved before rewind".into()));
    }
    require_revision_ancestor(&tx, lineage, expected, target)?;
    let branch_sequence = tx
        .query_row(
            "UPDATE lineage_branches
             SET head_revision_id = ?1, head_sequence = head_sequence + 1, updated_at = ?2
             WHERE lineage_id = ?3 AND session_id = ?4
               AND head_revision_id = ?5 AND deleted_at IS NULL
             RETURNING head_sequence",
            rusqlite::params![
                target.as_str(),
                checked_i64(updated_at, "branch updated_at")?,
                lineage.as_str(),
                branch.as_str(),
                expected.as_str()
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Integrity("branch rewind compare-and-swap failed".into()))?;
    tx.execute(
        "INSERT INTO lineage_branch_revisions (
             lineage_id, session_id, branch_sequence, revision_id
         ) VALUES (?1, ?2, ?3, ?4)",
        (
            lineage.as_str(),
            branch.as_str(),
            branch_sequence,
            target.as_str(),
        ),
    )?;
    insert_receipt(&tx, lineage, branch, &receipt, updated_at)?;
    tx.commit()?;
    Ok(receipt)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ForkStats {
    pub(crate) branch_rows_written: u64,
    pub(crate) receipt_rows_written: u64,
    pub(crate) sequence_rows_written: u64,
}

pub(crate) fn fork_branch<C: LineageSavepoint>(
    conn: &mut C,
    lineage: &LineageId,
    source: &BranchId,
    target: &BranchId,
    captured_revision: Option<&RevisionId>,
    created_at: u64,
) -> Result<(LineageCommitReceipt, ForkStats)> {
    let tx = conn.lineage_savepoint()?;
    let existing_creation = tx
        .query_row(
            "SELECT fork_parent_session_id, initial_revision_id
             FROM lineage_branches
             WHERE lineage_id = ?1 AND session_id = ?2",
            (lineage.as_str(), target.as_str()),
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((stored_source, stored_initial)) = existing_creation {
        let stored_source = stored_source
            .map(BranchId::new)
            .transpose()?
            .ok_or_else(|| StoreError::Integrity("fork target is not a fork branch".into()))?;
        let stored_initial = RevisionId::from_db(stored_initial)?;
        let captured = captured_revision.unwrap_or(&stored_initial);
        if stored_source != *source || captured != &stored_initial {
            return Err(StoreError::Integrity(
                "fork target has different creation metadata".into(),
            ));
        }
        let fingerprint = commit_fingerprint(
            lineage,
            target,
            LineageOperation::Fork,
            None,
            captured,
            Some(source),
        );
        let stored = load_receipt(&tx, lineage, target, &fingerprint)?.ok_or_else(|| {
            StoreError::Integrity("fork target has no canonical creation receipt".into())
        })?;
        return Ok((stored, ForkStats::default()));
    }

    let source_head = branch_head_in(&tx, lineage, source, false)?;
    let captured = captured_revision.unwrap_or(&source_head);
    require_revision_ancestor(&tx, lineage, &source_head, captured)?;
    let receipt = LineageCommitReceipt {
        fingerprint: commit_fingerprint(
            lineage,
            target,
            LineageOperation::Fork,
            None,
            captured,
            Some(source),
        ),
        operation: LineageOperation::Fork,
        prior_revision_id: None,
        result_revision_id: captured.clone(),
        coordinates: ReceiptCoordinates::default(),
    };
    let inserted = tx.execute(
        "INSERT INTO lineage_branches (
             lineage_id, session_id, fork_parent_session_id, parent_session_id,
             initial_revision_id, head_revision_id, head_sequence, next_turn_id,
             created_at, updated_at, deleted_at,
             cwd, mode, reasoning_effort, model, fast_mode,
             session_cost_usd, input_tokens, cached_input_tokens,
             output_tokens, reasoning_tokens, accounting_json
         )
         SELECT lineage_id, ?1, session_id, session_id, ?2, ?2, 1, next_turn_id, ?3, ?3, NULL,
                cwd, mode, reasoning_effort, model, fast_mode,
                session_cost_usd, input_tokens, cached_input_tokens,
                output_tokens, reasoning_tokens, accounting_json
         FROM lineage_branches
         WHERE lineage_id = ?4 AND session_id = ?5 AND deleted_at IS NULL",
        rusqlite::params![
            target.as_str(),
            captured.as_str(),
            checked_i64(created_at, "fork created_at")?,
            lineage.as_str(),
            source.as_str()
        ],
    )?;
    if inserted != 1 {
        return Err(StoreError::Integrity(format!(
            "cannot fork missing or deleted branch {}",
            source.as_str()
        )));
    }
    tx.execute(
        "INSERT INTO lineage_branch_revisions (
             lineage_id, session_id, branch_sequence, revision_id
         ) VALUES (?1, ?2, 1, ?3)",
        (lineage.as_str(), target.as_str(), captured.as_str()),
    )?;
    insert_receipt(&tx, lineage, target, &receipt, created_at)?;
    tx.commit()?;
    Ok((
        receipt,
        ForkStats {
            branch_rows_written: 1,
            receipt_rows_written: 1,
            sequence_rows_written: 0,
        },
    ))
}

pub(crate) fn delete_branch(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    deleted_at: u64,
) -> Result<()> {
    let updated = conn.execute(
        "UPDATE lineage_branches
         SET head_revision_id = NULL, deleted_at = ?1, updated_at = ?1
         WHERE lineage_id = ?2 AND session_id = ?3 AND deleted_at IS NULL",
        rusqlite::params![
            checked_i64(deleted_at, "branch deleted_at")?,
            lineage.as_str(),
            branch.as_str()
        ],
    )?;
    if updated != 1 {
        return Err(StoreError::Integrity(format!(
            "branch {} is not live",
            branch.as_str()
        )));
    }
    Ok(())
}
