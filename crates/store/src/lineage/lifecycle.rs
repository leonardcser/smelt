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
        return Ok(stored.save);
    }
    let existing = load_branch_snapshot(&tx, lineage, branch, false)
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
        let side_tables = merge_side_tables(&SideTableSuffixes::default(), &command.side_tables);
        let history_text_bytes = history_root.byte_count();
        let state_bytes =
            revision_state_bytes(&command.metadata, side_tables).map_err(store_failure)?;
        create_initial_branch_in(
            &tx,
            lineage,
            branch,
            &branch_metadata,
            history_root,
            transcript_root,
            &state_bytes,
            branch_created_at,
            created_at.max(branch_created_at),
        )
        .map_err(store_failure)?;
        let receipt = SaveReceipt {
            session_id: branch.as_str().to_owned(),
            previous: StoreHead::default(),
            current: StoreHead {
                revision: crate::session_commit::Revision::new(1),
                history_len: command.history.final_len,
                transcript_record_count: crate::session_commit::TranscriptRecordCount::new(
                    command.transcript_records.as_ref().map_or(0, |records| {
                        records.start.get() + records.records.len() as u64
                    }),
                ),
            },
            lineage_id: Some(lineage.as_str().to_owned()),
            history_text_bytes,
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
        return Ok(receipt);
    }

    let current = existing.expect("checked above");
    if command.identity != current.identity {
        return Err(SessionCommitFailure::IdentityMismatch {
            stored: current.identity,
            attempted: command.identity.clone(),
        });
    }
    if command.expected.revision == crate::session_commit::Revision::ZERO {
        let history = sequence_range(
            &tx,
            lineage,
            &current.history_root,
            0,
            current.history_root.item_count,
        )
        .and_then(|(bytes, _)| deserialize_history_items(&tx, bytes))
        .map_err(store_failure)?;
        let mut transcript = deserialize_sequence_range::<StoredTranscriptBlock>(
            &tx,
            lineage,
            &current.transcript_root,
            0,
            current.transcript_root.item_count,
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
            && current.metadata == command.metadata
            && current.side_tables == expected_side
        {
            let receipt = SaveReceipt {
                session_id: branch.as_str().to_owned(),
                previous: StoreHead::default(),
                current: current.head,
                lineage_id: Some(lineage.as_str().to_owned()),
                history_text_bytes: current.history_root.byte_count(),
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
            return Ok(receipt);
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
    let history_items =
        serialize_history_items(&tx, &command.history.items, compression).map_err(store_failure)?;
    let history_root = replace_sequence_suffix_in(
        &tx,
        lineage,
        &prior.history_root,
        command.history.start.get(),
        &history_items,
        compression,
    )
    .map_err(store_failure)?;
    let transcript_root = match &command.transcript_records {
        Some(records) => replace_sequence_suffix_in(
            &tx,
            lineage,
            &prior.transcript_root,
            records.start.get(),
            &serialize_transcript_items(&tx, &records.records, compression)
                .map_err(store_failure)?,
            compression,
        )
        .map_err(store_failure)?,
        None => prior.transcript_root.clone(),
    };
    let prior_state = load_revision_state(&tx, lineage, &prior).map_err(store_failure)?;
    let side_tables = merge_side_tables(&prior_state.side_tables, &command.side_tables);
    let current_was_expected = current.revision_id == expected_revision;
    if current_was_expected
        && history_root == prior.history_root
        && transcript_root == prior.transcript_root
        && command.metadata == current.metadata
        && side_tables == current.side_tables
    {
        let receipt = SaveReceipt {
            session_id: branch.as_str().to_owned(),
            previous: command.expected,
            current: command.expected,
            lineage_id: Some(lineage.as_str().to_owned()),
            history_text_bytes: prior.history_root.byte_count(),
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
        return Ok(receipt);
    }
    let state_bytes =
        revision_state_bytes(&command.metadata, side_tables).map_err(store_failure)?;
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
    let (revision, _) = commit_revision_in(
        &tx,
        lineage,
        branch,
        &expected_revision,
        &history_root,
        &transcript_root,
        &state_bytes,
        operation,
        created_at,
    )
    .map_err(|error| {
        if !current_was_expected {
            SessionCommitFailure::StaleBase {
                expected: command.expected,
                current: current.head,
            }
        } else {
            store_failure(error)
        }
    })?;
    if current_was_expected {
        update_branch_metadata(&tx, lineage, branch, &branch_metadata).map_err(store_failure)?;
    }
    let receipt = SaveReceipt {
        session_id: branch.as_str().to_owned(),
        previous: command.expected,
        current: StoreHead {
            revision: command.expected.revision.checked_add(1).ok_or_else(|| {
                SessionCommitFailure::Integrity {
                    message: "lineage branch sequence overflow".into(),
                }
            })?,
            history_len: crate::session_commit::HistoryLen::new(revision.history_root.item_count),
            transcript_record_count: crate::session_commit::TranscriptRecordCount::new(
                revision.transcript_root.item_count,
            ),
        },
        lineage_id: Some(lineage.as_str().to_owned()),
        history_text_bytes: revision.history_root.byte_count(),
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
    let mut tx = conn
        .transaction()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    let session =
        apply_lineage_session_commit(&mut tx, lineage, branch, &command.session, compression)?;
    let turn_id = tx
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
    let snapshot = load_branch_snapshot(&tx, lineage, branch, false).map_err(store_failure)?;
    let (history_bytes, _) = sequence_item(
        &tx,
        lineage,
        &snapshot.history_root,
        command.turn.submitted_history_idx.get(),
    )
    .map_err(store_failure)?;
    let submitted_item: protocol::HistoryItem = serde_json::from_slice(&history_bytes)
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    let history_hash = crate::history::item_hash(&submitted_item).map_err(store_failure)?;
    let inserted = tx
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
                checked_i64(
                    command.turn.submitted_history_idx.get(),
                    "submitted history index"
                )
                .map_err(store_failure)?,
                history_hash,
                snapshot.revision_id.as_str(),
                checked_i64(snapshot.head.revision.get(), "submitted sequence")
                    .map_err(store_failure)?,
                command.turn.kind.as_str(),
                command
                    .turn
                    .continuation_of
                    .map(TurnId::get)
                    .map(|value| checked_i64(value, "continuation turn id"))
                    .transpose()
                    .map_err(store_failure)?,
                checked_i64(command.turn.created_at_ms, "turn created_at_ms")
                    .map_err(store_failure)?,
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
        &tx,
        lineage,
        branch,
        &fingerprint,
        "submit_turn",
        &session,
        Some(turn_id),
        Some(TurnState::Ready),
        None,
        command.turn.created_at_ms,
    )
    .map_err(store_failure)?;
    {
        let _perf = smelt_perf::perf::begin("store:lineage:transaction_commit");
        tx.commit()
            .map_err(StoreError::from)
            .map_err(store_failure)?;
    }
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
    let mut tx = conn
        .transaction()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    let current = stored_lineage_turn(&tx, lineage, branch, command.turn_id)
        .map_err(store_failure)?
        .ok_or(SessionCommitFailure::TurnNotFound {
            turn_id: command.turn_id,
        })?;
    let allowed = matches!(
        (current.state, command.state),
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
            turn_id: command.turn_id,
            from: current.state,
            to: command.state,
        });
    }
    let minimum_time = current.started_at_ms.unwrap_or(current.created_at_ms);
    if command.at_ms < minimum_time {
        return Err(SessionCommitFailure::InvalidTurn {
            message: format!(
                "turn transition timestamp {} precedes {}",
                command.at_ms, minimum_time
            ),
        });
    }
    let session =
        apply_lineage_session_commit(&mut tx, lineage, branch, &command.session, compression)?;
    let updated = if command.state == TurnState::Running {
        tx.execute(
            "UPDATE lineage_turns
             SET turn_state = 'running', started_at_ms = ?1
             WHERE lineage_id = ?2 AND session_id = ?3 AND turn_id = ?4
               AND turn_state = 'ready'",
            rusqlite::params![
                checked_i64(command.at_ms, "turn transition timestamp").map_err(store_failure)?,
                lineage.as_str(),
                branch.as_str(),
                checked_i64(command.turn_id.get(), "turn id").map_err(store_failure)?,
            ],
        )
    } else {
        tx.execute(
            "UPDATE lineage_turns
             SET turn_state = ?1, finished_at_ms = ?2, terminal_reason = ?3
             WHERE lineage_id = ?4 AND session_id = ?5 AND turn_id = ?6
               AND turn_state IN ('ready', 'running')",
            rusqlite::params![
                command.state.as_str(),
                checked_i64(command.at_ms, "turn transition timestamp").map_err(store_failure)?,
                command.terminal_reason,
                lineage.as_str(),
                branch.as_str(),
                checked_i64(command.turn_id.get(), "turn id").map_err(store_failure)?,
            ],
        )
    }
    .map_err(StoreError::from)
    .map_err(store_failure)?;
    if updated != 1 {
        return Err(SessionCommitFailure::Integrity {
            message: format!("turn {} changed during transition", command.turn_id.get()),
        });
    }
    insert_session_receipt(
        &tx,
        lineage,
        branch,
        &fingerprint,
        "turn_transition",
        &session,
        Some(command.turn_id),
        Some(command.state),
        None,
        command.at_ms,
    )
    .map_err(store_failure)?;
    tx.execute(
        "INSERT INTO lineage_turn_transitions (
             lineage_id, session_id, fingerprint, turn_id, from_state, to_state,
             transitioned_at_ms, terminal_reason
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            fingerprint,
            checked_i64(command.turn_id.get(), "turn id").map_err(store_failure)?,
            current.state.as_str(),
            command.state.as_str(),
            checked_i64(command.at_ms, "turn transition timestamp").map_err(store_failure)?,
            command.terminal_reason,
        ],
    )
    .map_err(StoreError::from)
    .map_err(store_failure)?;
    tx.commit()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    Ok(TurnTransitionReceipt {
        session,
        turn_id: command.turn_id,
        state: command.state,
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
) -> Result<Option<StartupRecoveryReceipt>> {
    let tx = conn.transaction()?;
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
    let previous = load_branch_snapshot(&tx, lineage, branch, false)?;
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
            previous.revision_id.as_str(),
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
            previous.revision_id.as_str(),
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
        history_text_bytes: previous.history_root.byte_count(),
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
    Ok(Some(StartupRecoveryReceipt {
        session: save,
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

pub(crate) fn fork_branch(
    conn: &mut Connection,
    lineage: &LineageId,
    source: &BranchId,
    target: &BranchId,
    captured_revision: Option<&RevisionId>,
    created_at: u64,
) -> Result<(LineageCommitReceipt, ForkStats)> {
    let tx = conn.transaction()?;
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
