use super::*;
use crate::{
    ArchiveEdit, ArchiveRow, CheckpointEventsEdit, CompactSessionArchives, CompactSessionCommit,
    MetadataArchiveRow, MetadataMessage, SessionArchiveBase, SessionScalars, ValueEdit,
};

mod turn;

fn scalars(metadata: &SessionMetadata) -> SessionScalars {
    SessionScalars {
        title: metadata.title.clone(),
        slug: metadata.slug.clone(),
        cwd: metadata.cwd.clone(),
        mode: metadata.mode.clone(),
        reasoning_effort: metadata.reasoning_effort.clone(),
        model: metadata.model.clone(),
        fast_mode: metadata.fast_mode,
        accounting: ValueEdit::Retain,
        context_tokens: metadata.context_tokens,
        context_tokens_history_len: metadata.context_tokens_history_len,
        display_context_tokens: metadata.display_context_tokens,
        session_cost_usd: metadata.session_cost_usd,
        updated_at: metadata.updated_at,
    }
}

fn initial(branch: &BranchId) -> CompactSessionCommit {
    let eager = initial_session_commit(branch);
    CompactSessionCommit {
        session_id: eager.session_id,
        expected: eager.expected,
        identity: eager.identity,
        scalars: scalars(&eager.metadata),
        archive_base: None,
        archives: CompactSessionArchives::default(),
        history: eager.history,
        transcript_records: eager.transcript_records,
    }
}

fn event(summary: &str, completion: u64, created: u64) -> serde_json::Value {
    serde_json::json!({"kind":"compact", "summary":summary, "first_live_index":0, "completed_at_history_len":completion, "created_at_ms":created})
}

fn checkpoint_record(summary: crate::CheckpointSummary, created: u64) -> crate::CheckpointRecord {
    crate::CheckpointRecord {
        fields: serde_json::json!({"kind":"compact", "first_live_index":0, "completed_at_history_len":1, "created_at_ms":created}),
        summary,
    }
}

fn archived(branch: &BranchId, events: usize) -> CompactSessionCommit {
    let mut command = initial(branch);
    command.archives.first_user_message = ValueEdit::Replace {
        value: Some("first α\0".repeat(128).into()),
    };
    command.archives.checkpoint = crate::CheckpointEdit::Replace {
        value: Some(event("active", 1, 1)),
    };
    command.archives.checkpoint_events = CheckpointEventsEdit::ReplaceSuffix {
        retain_records: 0,
        records: (0..events)
            .map(|index| event(&format!("event {index}"), 1, index as u64))
            .collect(),
    };
    command.archives.metadata_snapshots = ArchiveEdit::ReplaceSuffix {
        retain_records: 0,
        records: vec![MetadataArchiveRow {
            index: HistoryIndex::ZERO,
            fields: serde_json::json!({"title":"initial", "unknown":[null, false, {"nested":"preserved"}]}),
            message: MetadataMessage::Active,
        }],
    };
    command.archives.turn_metas = ArchiveEdit::ReplaceSuffix {
        retain_records: 0,
        records: vec![ArchiveRow {
            index: HistoryIndex::ZERO,
            value: serde_json::json!({"turn":"initial"}),
        }],
    };
    command.archives.context_snapshots = ArchiveEdit::ReplaceSuffix {
        retain_records: 0,
        records: vec![ArchiveRow {
            index: HistoryIndex::ZERO,
            value: serde_json::json!({"context":"initial"}),
        }],
    };
    command
}

fn next(
    command: &CompactSessionCommit,
    lineage: &LineageId,
    result: &SessionCommitResult,
) -> CompactSessionCommit {
    let mut next = command.clone();
    next.expected = result.receipt.current;
    next.archive_base = Some(SessionArchiveBase {
        lineage_id: lineage.as_str().to_owned(),
        revision_id: result.revision_id.clone(),
        branch_sequence: result.receipt.current.revision,
    });
    next.history.start = HistoryIndex::new(next.history.final_len.get());
    next.history.items.clear();
    next.archives = CompactSessionArchives::default();
    next.transcript_records = None;
    next
}

fn apply(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &CompactSessionCommit,
) -> SessionCommitResult {
    apply_compact_session_commit(conn, lineage, branch, command, ObjectCompression::none()).unwrap()
}

#[test]
fn turn_submission_uses_saved_receipt_revision_after_head_advances() {
    let root = tempfile::tempdir().unwrap();
    let branch = branch_id('a');
    let mut writer = crate::OwnedLineageWriter::open(root.path(), branch.as_str()).unwrap();
    let command = initial_session_commit(&branch);
    let first = writer.commit_session(&command).unwrap();
    let mut changed = command.clone();
    changed.expected = first.current;
    changed.metadata.updated_at = 2;
    changed.history.items = vec![protocol::HistoryItem::system("two")];
    writer.commit_session(&changed).unwrap();
    let receipt = writer
        .submit_turn(&SubmitTurn {
            session: command.clone(),
            turn: crate::NewTurn {
                kind: TurnKind::Command,
                submitted_history_idx: HistoryIndex::ZERO,
                continuation_of: None,
                created_at_ms: 3,
            },
        })
        .unwrap();
    let reader = crate::LineageSessionReader::open_existing(root.path(), branch.as_str()).unwrap();
    let turns = reader.turns().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0].submitted_revision,
        receipt.session.current.revision
    );
    assert_eq!(
        turns[0].submitted_history_hash,
        crate::history::item_hash(&command.history.items[0]).unwrap()
    );
}

#[test]
fn native_compact_public_writer_creates_noops_and_replays_exact_results() {
    let root = tempfile::tempdir().unwrap();
    let branch = branch_id('a');
    let mut writer =
        crate::OwnedLineageWriter::open(root.path(), branch.as_str().to_owned()).unwrap();
    let lineage = LineageId::from_hex(writer.lineage_id().to_owned()).unwrap();
    let command = archived(&branch, 3);
    let result = writer.commit_compact_session(&command).unwrap();
    assert_eq!(result.receipt.current.revision.get(), 1);
    assert_eq!(writer.commit_compact_session(&command).unwrap(), result);
    let snapshot = writer.snapshot().unwrap();
    assert_eq!(snapshot.revision_id, result.revision_id);
    assert_eq!(
        snapshot.metadata.first_user_message.as_deref(),
        Some("first α\0".repeat(128).as_str())
    );
    assert_eq!(
        snapshot
            .metadata
            .checkpoint_events_json
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        snapshot.side_tables.metadata_snapshots[0].1["first_user_message"],
        snapshot.metadata.first_user_message.clone().unwrap()
    );
    let noop = next(&command, &lineage, &result);
    let unchanged = writer.commit_compact_session(&noop).unwrap();
    assert_eq!(unchanged.revision_id, result.revision_id);
    assert_eq!(unchanged.receipt.current, result.receipt.current);
    assert_eq!(writer.snapshot().unwrap(), snapshot);
    writer.release().unwrap();
    let mut writer = crate::OwnedLineageWriter::open_existing_in_lineage(
        root.path(),
        lineage.as_str(),
        branch.as_str(),
    )
    .unwrap();
    assert_eq!(writer.commit_compact_session(&noop).unwrap(), unchanged);
}

#[test]
fn native_checkpoint_header_updates_have_bounded_commands() {
    for summary_bytes in [0, 32 * 1024, 1024 * 1024] {
        let root = tempfile::tempdir().unwrap();
        let branch = branch_id('a');
        let mut writer = crate::OwnedLineageWriter::open(root.path(), branch.as_str()).unwrap();
        let lineage = LineageId::from_hex(writer.lineage_id().to_owned()).unwrap();
        let summary = "s".repeat(summary_bytes);
        let mut command = archived(&branch, 1);
        command.archives.checkpoint = crate::CheckpointEdit::Replace {
            value: Some(event(&summary, 1, 1)),
        };
        command.archives.checkpoint_events = CheckpointEventsEdit::ReplaceSuffix {
            retain_records: 0,
            records: vec![event(&summary, 1, 1)],
        };
        let saved = writer.commit_compact_session(&command).unwrap();
        let mut update = next(&command, &lineage, &saved);
        let fields = serde_json::json!({"kind":"compact", "first_live_index":0, "completed_at_history_len":1, "created_at_ms":2});
        update.archives.checkpoint = crate::CheckpointEdit::ReplaceRecord {
            record: crate::CheckpointRecord {
                fields: fields.clone(),
                summary: crate::CheckpointSummary::BaseCheckpoint,
            },
        };
        update.archives.checkpoint_events = CheckpointEventsEdit::ReplaceRecordsSuffix {
            retain_records: 0,
            records: vec![crate::CheckpointRecord {
                fields,
                summary: crate::CheckpointSummary::BaseEvent { record: 0 },
            }],
        };
        let bytes = serde_json::to_vec(&update).unwrap().len();
        eprintln!("CHECKPOINT_HEADER_UPDATE summary_bytes={summary_bytes} command_bytes={bytes}");
        let fingerprint = crate::compact_session_commit_fingerprint(&update).unwrap();
        let updated = writer.commit_compact_session(&update).unwrap();
        assert_eq!(fingerprint.len(), 64);
        let snapshot = writer.snapshot().unwrap();
        assert!(
            snapshot.metadata.checkpoint_json.as_ref().unwrap()["summary"].as_str()
                == Some(summary.as_str())
        );
        assert!(
            snapshot.metadata.checkpoint_events_json.as_ref().unwrap()[0]["summary"].as_str()
                == Some(summary.as_str())
        );
        assert_eq!(
            snapshot.metadata.checkpoint_json.as_ref().unwrap()["created_at_ms"],
            2
        );
        assert_eq!(writer.commit_compact_session(&update).unwrap(), updated);
        assert!(
            bytes < 4096,
            "unchanged summary inflates header command to {bytes} bytes"
        );
    }
}

#[test]
fn native_checkpoint_references_do_not_hydrate_old_headers_or_summaries() {
    for compression in [ObjectCompression::none(), ObjectCompression::zstd(3, 1, 0)] {
        for events in [0, 32, 128] {
            let (mut conn, lineage) = setup();
            let branch = branch_id('a');
            let active = format!("{}α\0", "active".repeat(32768));
            let summary = format!("{}β\0", "event".repeat(8192));
            let mut command = archived(&branch, events);
            let mut checkpoint = event(&active, 1, 1);
            checkpoint["unknown"] = serde_json::json!("x".repeat(1024 * 1024));
            command.archives.checkpoint = crate::CheckpointEdit::Replace {
                value: Some(checkpoint),
            };
            command.archives.checkpoint_events = CheckpointEventsEdit::ReplaceSuffix {
                retain_records: 0,
                records: (0..events)
                    .map(|index| event(&summary, 1, index as u64))
                    .collect(),
            };
            let saved =
                apply_compact_session_commit(&mut conn, &lineage, &branch, &command, compression)
                    .unwrap();
            let revision = load_revision(
                &conn,
                &lineage,
                &RevisionId::from_db(saved.revision_id.clone()).unwrap(),
            )
            .unwrap();
            let envelope =
                load_revision_envelope(&conn, &lineage, &revision, &mut OperationStats::default())
                    .unwrap();
            let StoredRevisionState::Shared(envelope) = envelope else {
                panic!("shared fixture")
            };
            let wire = serde_json::to_value(envelope).unwrap();
            let mut hashes = BTreeSet::new();
            for (role, ordinal) in [
                ("checkpoint", 0),
                ("checkpoint_events", events.saturating_sub(1) * 2),
            ] {
                let Some(id) = wire["archives"][role].as_str() else {
                    continue;
                };
                let root =
                    load_root(&conn, &lineage, &RootId::from_db(id.into()).unwrap()).unwrap();
                if root.item_count == 0 {
                    continue;
                }
                let (payloads, _) = sequence_payload_refs_from_root(
                    &conn,
                    &lineage,
                    &root,
                    ordinal as u64,
                    ordinal as u64 + 2,
                )
                .unwrap();
                hashes.extend(payloads.into_iter().map(|payload| payload.object_hash));
            }
            let physical: Vec<_> = hashes
                .into_iter()
                .map(|hash| {
                    let bytes: Vec<u8> = conn
                        .query_row(
                            "SELECT bytes FROM objects WHERE hash = ?1",
                            [&hash],
                            |row| row.get(0),
                        )
                        .unwrap();
                    let mut corrupted = bytes.clone();
                    corrupted[0] ^= 0xff;
                    conn.execute(
                        "UPDATE objects SET bytes = ?2 WHERE hash = ?1",
                        rusqlite::params![&hash, corrupted],
                    )
                    .unwrap();
                    (hash, bytes)
                })
                .collect();
            assert!(load_branch_snapshot(&conn, &lineage, &branch, false).is_err());
            assert!(verify_archive_coordinates(&conn, &lineage).is_err());
            let mut update = next(&command, &lineage, &saved);
            update.archives.checkpoint = crate::CheckpointEdit::ReplaceRecord {
                record: checkpoint_record(crate::CheckpointSummary::BaseCheckpoint, 129),
            };
            update.archives.checkpoint_events = CheckpointEventsEdit::ReplaceRecordsSuffix {
                retain_records: events.saturating_sub(1) as u64,
                records: vec![checkpoint_record(
                    if events == 0 {
                        crate::CheckpointSummary::Checkpoint
                    } else {
                        crate::CheckpointSummary::BaseEvent {
                            record: events as u64 - 1,
                        }
                    },
                    129,
                )],
            };
            assert!(serde_json::to_vec(&update).unwrap().len() < 4096);
            let updated =
                apply_compact_session_commit(&mut conn, &lineage, &branch, &update, compression)
                    .unwrap();
            assert_eq!(
                apply_compact_session_commit(&mut conn, &lineage, &branch, &update, compression)
                    .unwrap(),
                updated
            );
            assert!(load_branch_snapshot(&conn, &lineage, &branch, false).is_err());
            for (hash, bytes) in physical {
                conn.execute(
                    "UPDATE objects SET bytes = ?2 WHERE hash = ?1",
                    rusqlite::params![hash, bytes],
                )
                .unwrap();
            }
            verify_archive_coordinates(&conn, &lineage).unwrap();
            let snapshot = load_branch_snapshot(&conn, &lineage, &branch, false).unwrap();
            assert!(
                snapshot.metadata.checkpoint_json.as_ref().unwrap()["summary"].as_str()
                    == Some(active.as_str())
            );
            let timeline = snapshot
                .metadata
                .checkpoint_events_json
                .as_ref()
                .unwrap()
                .as_array()
                .unwrap();
            assert_eq!(timeline.len(), events.max(1));
            assert!(
                timeline.last().unwrap()["summary"].as_str()
                    == Some(if events == 0 {
                        active.as_str()
                    } else {
                        summary.as_str()
                    })
            );
            assert!(
                load_revision_state(&conn, &lineage, &revision)
                    .unwrap()
                    .metadata
                    .checkpoint_json
                    .as_ref()
                    .unwrap()["unknown"]
                    .as_str()
                    == Some("x".repeat(1024 * 1024).as_str())
            );
        }
    }
}

#[test]
fn native_checkpoint_records_bind_exact_base_and_share_prepared_summary() {
    let root = tempfile::tempdir().unwrap();
    let branch = branch_id('a');
    let mut writer = crate::OwnedLineageWriter::open(root.path(), branch.as_str()).unwrap();
    let lineage = LineageId::from_hex(writer.lineage_id().to_owned()).unwrap();
    let initial = archived(&branch, 1);
    let first = writer.commit_compact_session(&initial).unwrap();
    let mut changed = next(&initial, &lineage, &first);
    changed.archives.checkpoint = crate::CheckpointEdit::ReplaceRecord {
        record: checkpoint_record(
            crate::CheckpointSummary::New {
                text: "new α\0".repeat(8192).into(),
            },
            2,
        ),
    };
    changed.archives.checkpoint_events = CheckpointEventsEdit::ReplaceRecordsSuffix {
        retain_records: 1,
        records: vec![checkpoint_record(crate::CheckpointSummary::Checkpoint, 2)],
    };
    assert!(serde_json::to_vec(&changed).unwrap().len() < 2 * "new α\0".repeat(8192).len() + 4096);
    let second = writer.commit_compact_session(&changed).unwrap();
    let changed_snapshot = writer.snapshot().unwrap();
    assert!(
        changed_snapshot.metadata.checkpoint_json.as_ref().unwrap()["summary"]
            == changed_snapshot
                .metadata
                .checkpoint_events_json
                .as_ref()
                .unwrap()[1]["summary"]
    );
    let mut update = next(&initial, &lineage, &first);
    update.expected = second.receipt.current;
    update.archives.checkpoint = crate::CheckpointEdit::ReplaceRecord {
        record: checkpoint_record(crate::CheckpointSummary::BaseEvent { record: 0 }, 3),
    };
    update.archives.checkpoint_events = CheckpointEventsEdit::ReplaceRecordsSuffix {
        retain_records: 0,
        records: vec![
            checkpoint_record(crate::CheckpointSummary::BaseCheckpoint, 3),
            checkpoint_record(crate::CheckpointSummary::Checkpoint, 4),
            checkpoint_record(
                crate::CheckpointSummary::New {
                    text: "independent".into(),
                },
                5,
            ),
        ],
    };
    let third = writer.commit_compact_session(&update).unwrap();
    let snapshot = writer.snapshot().unwrap();
    assert_eq!(
        snapshot.metadata.checkpoint_json.as_ref().unwrap()["summary"],
        "event 0"
    );
    let events = snapshot
        .metadata
        .checkpoint_events_json
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(events[0]["summary"], "active");
    assert_eq!(events[1]["summary"], "event 0");
    assert_eq!(events[2]["summary"], "independent");
    let mut advance = next(&update, &lineage, &third);
    advance.scalars.title = Some("advanced".into());
    advance.scalars.updated_at = 4;
    let advanced = writer.commit_compact_session(&advance).unwrap();
    assert!(advanced.receipt.current.revision > third.receipt.current.revision);
    writer.release().unwrap();
    let mut writer = crate::OwnedLineageWriter::open_existing_in_lineage(
        root.path(),
        lineage.as_str(),
        branch.as_str(),
    )
    .unwrap();
    assert_eq!(writer.commit_compact_session(&update).unwrap(), third);
    assert_eq!(writer.commit_compact_session(&changed).unwrap(), second);
    assert_eq!(writer.store_head().unwrap(), advanced.receipt.current);
}

#[test]
fn native_checkpoint_references_reject_missing_nonstring_and_ambiguous_sources() {
    for value in [
        None,
        Some(serde_json::json!({})),
        Some(serde_json::json!({"summary":null})),
        Some(serde_json::json!({"summary":42})),
    ] {
        let (mut conn, lineage) = setup();
        let branch = branch_id('a');
        let mut initial = archived(&branch, 1);
        initial.archives.checkpoint = crate::CheckpointEdit::Replace { value };
        let saved = apply(&mut conn, &lineage, &branch, &initial);
        let base = next(&initial, &lineage, &saved);
        let mut invalid = Vec::new();
        for summary in [
            crate::CheckpointSummary::BaseCheckpoint,
            crate::CheckpointSummary::BaseEvent { record: 1 },
            crate::CheckpointSummary::BaseEvent { record: u64::MAX },
            crate::CheckpointSummary::Checkpoint,
        ] {
            let mut command = base.clone();
            command.archives.checkpoint = crate::CheckpointEdit::ReplaceRecord {
                record: checkpoint_record(summary, 2),
            };
            invalid.push(command);
        }
        for fields in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({"summary":null}),
            serde_json::json!({"summary":"inline"}),
        ] {
            let mut command = base.clone();
            command.archives.checkpoint = crate::CheckpointEdit::ReplaceRecord {
                record: crate::CheckpointRecord {
                    fields,
                    summary: crate::CheckpointSummary::New {
                        text: "body".into(),
                    },
                },
            };
            invalid.push(command);
        }
        let mut cleared = base.clone();
        cleared.archives.checkpoint = crate::CheckpointEdit::Replace { value: None };
        cleared.archives.checkpoint_events = CheckpointEventsEdit::ReplaceRecordsSuffix {
            retain_records: 1,
            records: vec![checkpoint_record(crate::CheckpointSummary::Checkpoint, 2)],
        };
        invalid.push(cleared);
        let before = archive_publication_counts(&conn);
        for command in invalid {
            assert!(apply_compact_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &command,
                ObjectCompression::none()
            )
            .is_err());
            assert_eq!(archive_publication_counts(&conn), before);
            assert_eq!(
                load_branch_record(&conn, &lineage, &branch, false)
                    .unwrap()
                    .head,
                saved.receipt.current
            );
        }
    }
}

#[test]
fn native_compact_public_fork_noop_uses_destination_sequence_and_effective_metadata() {
    let root = tempfile::tempdir().unwrap();
    let source = branch_id('a');
    let target = branch_id('b');
    let mut writer =
        crate::OwnedLineageWriter::open(root.path(), source.as_str().to_owned()).unwrap();
    let lineage = LineageId::from_hex(writer.lineage_id().to_owned()).unwrap();
    let mut command = archived(&source, 3);
    command.scalars.accounting = ValueEdit::Replace {
        value: Some(crate::SessionAccounting {
            session_usage: crate::SessionTokenUsage {
                context_tokens: Some(42),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let first = writer.commit_compact_session(&command).unwrap();
    let mut changed = next(&command, &lineage, &first);
    changed.scalars.title = Some("source title".into());
    changed.scalars.updated_at = 2;
    let latest = writer.commit_compact_session(&changed).unwrap();
    let (mut destination, forked) = crate::OwnedLineageWriter::fork_from(
        root.path(),
        source.as_str(),
        target.as_str(),
        3,
        Some(latest.receipt.current),
        &|| false,
    )
    .unwrap();
    let snapshot = destination.snapshot().unwrap();
    assert_eq!(snapshot.revision_id, latest.revision_id);
    assert_eq!(forked.source_session_id, source.as_str());
    assert_eq!(forked.source_head, latest.receipt.current);
    assert_eq!(forked.session.revision_id, latest.revision_id);
    assert_eq!(forked.session.receipt.current.revision.get(), 1);
    assert_eq!(latest.receipt.current.revision.get(), 2);
    let mut noop = next(&command, &lineage, &forked.session);
    noop.session_id = target.as_str().to_owned();
    noop.identity = snapshot.identity.clone();
    noop.scalars = scalars(&snapshot.metadata);
    let mut invalid = noop.clone();
    invalid.archive_base.as_mut().unwrap().branch_sequence = latest.receipt.current.revision;
    assert!(destination.commit_compact_session(&invalid).is_err());
    assert_eq!(destination.snapshot().unwrap(), snapshot);
    let result = destination.commit_compact_session(&noop).unwrap();
    assert_eq!(result.receipt.current, snapshot.head);
    assert_eq!(result.revision_id, snapshot.revision_id);
    assert_eq!(destination.snapshot().unwrap(), snapshot);
    assert_eq!(destination.commit_compact_session(&noop).unwrap(), result);
    assert_eq!(writer.snapshot().unwrap().head, latest.receipt.current);
}

#[test]
fn native_compact_retained_bodies_and_headers_are_not_read_or_fingerprinted() {
    for compression in [ObjectCompression::none(), ObjectCompression::zstd(3, 1, 1)] {
        let mut wire_sizes = Vec::new();
        let mut lifecycle_sizes = Vec::new();
        for checkpoints in [0, 32, 128] {
            let (mut conn, lineage) = setup();
            let branch = branch_id('a');
            let mut command = archived(&branch, checkpoints);
            command.archives.first_user_message = ValueEdit::Replace {
                value: Some("message α\0".repeat(32768).into()),
            };
            if let ArchiveEdit::ReplaceSuffix { records, .. } =
                &mut command.archives.metadata_snapshots
            {
                records[0].fields["unknown"] =
                    serde_json::Value::String("retained header".repeat(65536));
            }
            command.archives.checkpoint_events = CheckpointEventsEdit::ReplaceSuffix {
                retain_records: 0,
                records: (0..checkpoints)
                    .map(|index| event(&"archived summary".repeat(2048), 1, index as u64))
                    .collect(),
            };
            let first =
                apply_compact_session_commit(&mut conn, &lineage, &branch, &command, compression)
                    .unwrap();
            let before = load_branch_snapshot(&conn, &lineage, &branch, false).unwrap();
            let revision = load_revision(
                &conn,
                &lineage,
                &RevisionId::from_db(first.revision_id.clone()).unwrap(),
            )
            .unwrap();
            let envelope =
                load_revision_envelope(&conn, &lineage, &revision, &mut OperationStats::default())
                    .unwrap();
            let StoredRevisionState::Shared(envelope) = envelope else {
                panic!("shared fixture")
            };
            let wire = serde_json::to_value(envelope).unwrap();
            let mut hashes = BTreeSet::new();
            for (role, id, ordinal) in [
                (
                    "first",
                    wire["first_user_message_root"].as_str().unwrap(),
                    0,
                ),
                (
                    "header",
                    wire["archives"]["metadata_snapshots"].as_str().unwrap(),
                    0,
                ),
            ] {
                let root =
                    load_root(&conn, &lineage, &RootId::from_db(id.to_owned()).unwrap()).unwrap();
                let (payloads, _) =
                    sequence_payload_refs_from_root(&conn, &lineage, &root, ordinal, ordinal + 1)
                        .unwrap();
                assert!(
                    payloads[0].byte_count > 128 * 1024,
                    "{role} fixture is large"
                );
                hashes.insert(payloads[0].object_hash.clone());
            }
            let physical: Vec<_> = hashes
                .into_iter()
                .map(|hash| {
                    let bytes: Vec<u8> = conn
                        .query_row(
                            "SELECT bytes FROM objects WHERE hash = ?1",
                            [&hash],
                            |row| row.get(0),
                        )
                        .unwrap();
                    let mut corrupted = bytes.clone();
                    corrupted[0] ^= 0xff;
                    conn.execute(
                        "UPDATE objects SET bytes = ?2 WHERE hash = ?1",
                        rusqlite::params![&hash, corrupted],
                    )
                    .unwrap();
                    (hash, bytes)
                })
                .collect();
            assert!(load_branch_snapshot(&conn, &lineage, &branch, false).is_err());
            let mut title = next(&command, &lineage, &first);
            title.scalars.title = Some("native title".into());
            title.scalars.updated_at = 2;
            title.archives.metadata_snapshots = ArchiveEdit::ReplaceSuffix {
                retain_records: 1,
                records: vec![MetadataArchiveRow {
                    index: HistoryIndex::new(1),
                    fields: serde_json::json!({"title":"native title"}),
                    message: MetadataMessage::Active,
                }],
            };
            wire_sizes.push(serde_json::to_vec(&title).unwrap().len());
            let result =
                apply_compact_session_commit(&mut conn, &lineage, &branch, &title, compression)
                    .unwrap();
            assert_eq!(result.receipt.current.revision.get(), 2);
            assert_eq!(
                apply_compact_session_commit(&mut conn, &lineage, &branch, &title, compression)
                    .unwrap(),
                result
            );
            let submit = crate::CompactSubmitTurn {
                session: next(&title, &lineage, &result),
                turn: crate::NewTurn {
                    kind: TurnKind::Command,
                    submitted_history_idx: HistoryIndex::ZERO,
                    continuation_of: None,
                    created_at_ms: 3,
                },
            };
            let submitted =
                apply_compact_submit_turn(&mut conn, &lineage, &branch, &submit, compression)
                    .unwrap();
            assert_eq!(
                recover_compact_submit_turn(&conn, &lineage, &branch, &submit).unwrap(),
                Some(submitted.clone())
            );
            let running = crate::CompactTurnTransition {
                session: next(&title, &lineage, &submitted.session),
                turn_id: submitted.turn_id,
                state: TurnState::Running,
                at_ms: 4,
                terminal_reason: None,
            };
            let started =
                apply_compact_turn_transition(&mut conn, &lineage, &branch, &running, compression)
                    .unwrap();
            assert_eq!(
                recover_compact_turn_transition(&conn, &lineage, &branch, &running).unwrap(),
                Some(started)
            );
            lifecycle_sizes.push((
                serde_json::to_vec(&submit).unwrap().len(),
                serde_json::to_vec(&running).unwrap().len(),
            ));
            assert!(load_branch_snapshot(&conn, &lineage, &branch, false).is_err());
            for (hash, bytes) in physical {
                conn.execute(
                    "UPDATE objects SET bytes = ?2 WHERE hash = ?1",
                    rusqlite::params![hash, bytes],
                )
                .unwrap();
            }
            let after = load_branch_snapshot(&conn, &lineage, &branch, false).unwrap();
            assert!(after.metadata.first_user_message == before.metadata.first_user_message);
            assert!(
                after.metadata.checkpoint_events_json == before.metadata.checkpoint_events_json
            );
            assert!(
                after.side_tables.metadata_snapshots[0] == before.side_tables.metadata_snapshots[0]
            );
            assert_eq!(
                after.side_tables.metadata_snapshots[1].1["title"],
                "native title"
            );
            assert!(
                after.side_tables.metadata_snapshots[1].1["first_user_message"].as_str()
                    == before.metadata.first_user_message.as_deref()
            );
            assert_eq!(after.history_root, before.history_root);
            assert_eq!(after.transcript_root, before.transcript_root);
        }
        assert!(wire_sizes.iter().all(|size| *size == wire_sizes[0]));
        assert!(lifecycle_sizes
            .iter()
            .all(|size| *size == lifecycle_sizes[0]));
        assert!(lifecycle_sizes[0].0 < 4096 && lifecycle_sizes[0].1 < 4096);
        eprintln!(
            "NATIVE_COMPACT_TURN checkpoints=0/32/128 submit_bytes={} transition_bytes={}",
            lifecycle_sizes[0].0, lifecycle_sizes[0].1
        );
        assert!(wire_sizes[0] < 4096);
        eprintln!(
            "NATIVE_COMPACT_TITLE checkpoints=0/32/128 command_bytes={}",
            wire_sizes[0]
        );
    }
}

#[test]
fn native_compact_suffixes_use_independent_exact_archive_bases() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let command = archived(&branch, 3);
    let first = apply(&mut conn, &lineage, &branch, &command);
    let original = load_branch_snapshot(&conn, &lineage, &branch, false).unwrap();
    let mut a = next(&command, &lineage, &first);
    a.scalars.title = Some("a".into());
    a.archives.metadata_snapshots = ArchiveEdit::ReplaceSuffix {
        retain_records: 0,
        records: vec![MetadataArchiveRow {
            index: HistoryIndex::new(1),
            fields: serde_json::json!({"title":"a"}),
            message: MetadataMessage::Active,
        }],
    };
    a.archives.checkpoint_events = CheckpointEventsEdit::ReplaceSuffix {
        retain_records: 2,
        records: vec![event("changed suffix", 1, 4)],
    };
    a.archives.context_snapshots = ArchiveEdit::ReplaceSuffix {
        retain_records: 1,
        records: vec![ArchiveRow {
            index: HistoryIndex::new(1),
            value: serde_json::json!({"context":"a"}),
        }],
    };
    let second = apply(&mut conn, &lineage, &branch, &a);
    let mut b = next(&command, &lineage, &first);
    b.expected = second.receipt.current;
    b.scalars.title = Some("b".into());
    b.archives.metadata_snapshots = ArchiveEdit::ReplaceSuffix {
        retain_records: 1,
        records: vec![MetadataArchiveRow {
            index: HistoryIndex::new(1),
            fields: serde_json::json!({"title":"b"}),
            message: MetadataMessage::Active,
        }],
    };
    b.archives.checkpoint_events = a.archives.checkpoint_events.clone();
    b.archives.context_snapshots = a.archives.context_snapshots.clone();
    let third = apply(&mut conn, &lineage, &branch, &b);
    let snapshot = load_branch_snapshot(&conn, &lineage, &branch, false).unwrap();
    assert_eq!(snapshot.side_tables.metadata_snapshots.len(), 2);
    assert_eq!(
        snapshot.side_tables.metadata_snapshots[0],
        original.side_tables.metadata_snapshots[0]
    );
    assert_eq!(snapshot.side_tables.metadata_snapshots[1].1["title"], "b");
    assert_eq!(
        snapshot.metadata.checkpoint_events_json.as_ref().unwrap()[2]["summary"],
        "changed suffix"
    );
    assert_eq!(snapshot.side_tables.context_snapshots.len(), 2);
    assert_eq!(
        snapshot.side_tables.turn_metas,
        original.side_tables.turn_metas
    );
    assert_eq!(third.receipt.current.revision.get(), 3);
    assert_eq!(apply(&mut conn, &lineage, &branch, &a), second);
    assert_eq!(
        load_branch_snapshot(&conn, &lineage, &branch, false).unwrap(),
        snapshot
    );
    let historical = load_revision_state(
        &conn,
        &lineage,
        &load_revision(
            &conn,
            &lineage,
            &RevisionId::from_db(second.revision_id).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(historical.side_tables.metadata_snapshots.len(), 1);
    assert_eq!(historical.side_tables.metadata_snapshots[0].1["title"], "a");
}

#[test]
fn native_compact_message_and_event_presence_are_exact() {
    for message in [None, Some(String::new()), Some("α\0body".into())] {
        let (mut conn, lineage) = setup();
        let branch = branch_id('a');
        let mut command = initial(&branch);
        command.archives.first_user_message = ValueEdit::Replace {
            value: message.clone().map(Into::into),
        };
        command.archives.metadata_snapshots = ArchiveEdit::ReplaceSuffix {
            retain_records: 0,
            records: vec![
                MetadataArchiveRow {
                    index: HistoryIndex::ZERO,
                    fields: serde_json::json!({"first_user_message":null}),
                    message: MetadataMessage::None,
                },
                MetadataArchiveRow {
                    index: HistoryIndex::new(1),
                    fields: serde_json::json!({"unknown":true}),
                    message: MetadataMessage::Active,
                },
            ],
        };
        command.archives.checkpoint_events = CheckpointEventsEdit::ReplaceSuffix {
            retain_records: 0,
            records: Vec::new(),
        };
        let first = apply(&mut conn, &lineage, &branch, &command);
        let snapshot = load_branch_snapshot(&conn, &lineage, &branch, false).unwrap();
        assert_eq!(snapshot.metadata.first_user_message, message);
        assert_eq!(
            snapshot.metadata.checkpoint_events_json,
            Some(serde_json::json!([]))
        );
        assert_eq!(
            snapshot.side_tables.metadata_snapshots[0].1["first_user_message"],
            serde_json::Value::Null
        );
        match &message {
            Some(message) => assert_eq!(
                snapshot.side_tables.metadata_snapshots[1].1["first_user_message"].as_str(),
                Some(message.as_str())
            ),
            None => assert!(snapshot.side_tables.metadata_snapshots[1]
                .1
                .get("first_user_message")
                .is_none()),
        }
        let mut clear = next(&command, &lineage, &first);
        clear.archives.checkpoint_events = CheckpointEventsEdit::Clear;
        let cleared = apply(&mut conn, &lineage, &branch, &clear);
        assert_ne!(cleared.revision_id, first.revision_id);
        assert_eq!(
            load_branch_snapshot(&conn, &lineage, &branch, false)
                .unwrap()
                .metadata
                .checkpoint_events_json,
            None
        );
        assert_eq!(apply(&mut conn, &lineage, &branch, &command), first);
    }
}

#[test]
fn native_compact_explicit_snapshot_messages_preserve_presence_independently_of_active() {
    for text in ["", "α\0body", "active"] {
        let (mut conn, lineage) = setup();
        let branch = branch_id('a');
        let mut command = initial(&branch);
        command.archives.first_user_message = ValueEdit::Replace {
            value: Some("active".into()),
        };
        command.archives.metadata_snapshots = ArchiveEdit::ReplaceSuffix {
            retain_records: 0,
            records: vec![MetadataArchiveRow {
                index: HistoryIndex::ZERO,
                fields: serde_json::json!({"title":"snapshot", "unknown":[null, false]}),
                message: MetadataMessage::New { text: text.into() },
            }],
        };
        let result = apply(&mut conn, &lineage, &branch, &command);
        let snapshot = load_branch_snapshot(&conn, &lineage, &branch, false).unwrap();
        assert_eq!(
            snapshot.metadata.first_user_message.as_deref(),
            Some("active")
        );
        assert_eq!(
            snapshot.side_tables.metadata_snapshots[0].1["first_user_message"].as_str(),
            Some(text)
        );
        assert_eq!(
            snapshot.side_tables.metadata_snapshots[0].1["unknown"],
            serde_json::json!([null, false])
        );
        let noop = next(&command, &lineage, &result);
        assert_eq!(
            apply(&mut conn, &lineage, &branch, &noop).revision_id,
            result.revision_id
        );
        assert_eq!(
            load_branch_snapshot(&conn, &lineage, &branch, false).unwrap(),
            snapshot
        );
    }
}

#[test]
fn native_compact_fingerprint_domains_and_closed_wire_are_frozen() {
    struct NoDefault;
    assert!(matches!(
        ValueEdit::<NoDefault>::default(),
        ValueEdit::Retain
    ));
    assert!(matches!(
        ArchiveEdit::<NoDefault>::default(),
        ArchiveEdit::Retain
    ));
    const WIRE: &str = r#"{"archive_base":null,"archives":{"checkpoint":{"kind":"retain"},"checkpoint_events":{"kind":"retain"},"context_snapshots":{"kind":"retain"},"first_user_message":{"kind":"retain"},"metadata_snapshots":{"kind":"retain"},"turn_metas":{"kind":"retain"}},"expected":{"history_len":0,"revision":0,"transcript_record_count":0},"history":{"final_len":0,"items":[],"start":0},"identity":{"created_at":0,"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","parent_id":null},"scalars":{"accounting":{"kind":"retain"},"context_tokens":null,"context_tokens_history_len":null,"cwd":null,"display_context_tokens":null,"fast_mode":null,"mode":null,"model":null,"reasoning_effort":null,"session_cost_usd":0.0,"slug":null,"title":null,"updated_at":0},"session_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","transcript_records":null}"#;
    let command: CompactSessionCommit = serde_json::from_str(WIRE).unwrap();
    let transition = crate::CompactTurnTransition {
        session: command.clone(),
        turn_id: TurnId::new(1),
        state: TurnState::Running,
        at_ms: 1,
        terminal_reason: None,
    };
    assert_eq!(
        crate::compact_turn_transition_fingerprint(&transition).unwrap(),
        "b9841a8cb609ec4cfa2b47d521ba6117919f81b0d4965ea67b2e3e1792789d51"
    );
    let mut submit = crate::CompactSubmitTurn {
        session: command.clone(),
        turn: crate::NewTurn {
            kind: TurnKind::Command,
            submitted_history_idx: HistoryIndex::ZERO,
            continuation_of: None,
            created_at_ms: 1,
        },
    };
    submit.session.history.final_len = crate::HistoryLen::new(1);
    submit.session.history.items = vec![protocol::HistoryItem::system("golden")];
    assert_eq!(
        crate::compact_submit_turn_fingerprint(&submit).unwrap(),
        "2126dfa46ee5c72efad43e5ffae9346e00b01263e4271f5511c5b4f694cbff6e"
    );
    let mut wire = serde_json::to_value(&submit).unwrap();
    wire["archive_body"] = serde_json::json!([]);
    assert!(serde_json::from_value::<crate::CompactSubmitTurn>(wire).is_err());
    let mut wire = serde_json::to_value(&transition).unwrap();
    wire["archive_body"] = serde_json::json!([]);
    assert!(serde_json::from_value::<crate::CompactTurnTransition>(wire).is_err());
    assert_eq!(
        crate::compact_session_commit_fingerprint(&command).unwrap(),
        "10594fe9712a02f9be9609923d786867e94f4c85b4f3909a38545f6f3a4c9abc"
    );
    let legacy = SessionCommit {
        session_id: command.session_id.clone(),
        expected: command.expected,
        identity: command.identity.clone(),
        metadata: command.scalars.metadata(None).unwrap(),
        history: command.history.clone(),
        side_tables: SideTableSuffixes::default(),
        transcript_records: None,
    };
    assert_eq!(
        crate::session_commit_fingerprint(&legacy).unwrap(),
        "76b5fdaa4eed7d98ccc668821b37f9f5b71f68db6299c717dc5d8c9b67778cb3"
    );
    let value = serde_json::json!({"z":{},"a":[null,false,true,1,1.25,"α\u{0}\n"]});
    let mut bytes = Vec::new();
    crate::session_command::write_canonical_json(&value, &mut bytes).unwrap();
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        r#"{"a":[null,false,true,1,1.25,"α\u0000\n"],"z":{}}"#
    );
    let mut value: serde_json::Value = serde_json::from_str(WIRE).unwrap();
    value["scalars"]["checkpoint_events_json"] = serde_json::json!([]);
    assert!(serde_json::from_value::<CompactSessionCommit>(value).is_err());
    let mut value: serde_json::Value = serde_json::from_str(WIRE).unwrap();
    value["scalars"]["accounting"] = serde_json::json!({"kind":"replace","value":{"session_usage":{"archive_body":"rejected"},"context_token_identity":null,"display_context_token_identity":null}});
    assert!(serde_json::from_value::<CompactSessionCommit>(value).is_err());
}

#[test]
fn native_compact_legacy_noop_preserves_original_bytes_and_revision() {
    let (mut conn, lineage, branch, legacy, original) = legacy_projection_fixture(2);
    let before = load_branch_snapshot(&conn, &lineage, &branch, false).unwrap();
    let payload = load_payload_ref(&conn, &lineage, &original.state_payload_id).unwrap();
    let original_bytes = object(&conn, &payload.object_hash).unwrap().unwrap().bytes;
    let command = CompactSessionCommit {
        session_id: legacy.session_id.clone(),
        expected: legacy.expected,
        identity: legacy.identity.clone(),
        scalars: scalars(&legacy.metadata),
        archive_base: Some(SessionArchiveBase {
            lineage_id: lineage.as_str().to_owned(),
            revision_id: original.id.as_str().to_owned(),
            branch_sequence: legacy.expected.revision,
        }),
        archives: CompactSessionArchives::default(),
        history: legacy.history.clone(),
        transcript_records: None,
    };
    let result = apply(&mut conn, &lineage, &branch, &command);
    assert_eq!(result.revision_id, original.id.as_str());
    assert_eq!(result.receipt.current, legacy.expected);
    assert!(load_branch_snapshot(&conn, &lineage, &branch, false).unwrap() == before);
    assert!(object(&conn, &payload.object_hash).unwrap().unwrap().bytes == original_bytes);
    assert_eq!(apply(&mut conn, &lineage, &branch, &command), result);
    verify_revision_projections(&conn, &lineage).unwrap();
    verify_archive_coordinates(&conn, &lineage).unwrap();
}

#[test]
fn native_compact_exact_result_survives_rewind_and_reclamation_and_missing_results_fail_closed() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let command = archived(&branch, 3);
    let first = apply(&mut conn, &lineage, &branch, &command);
    let mut changed = next(&command, &lineage, &first);
    changed.scalars.title = Some("retained result".into());
    let result = apply(&mut conn, &lineage, &branch, &changed);
    rewind_branch(
        &mut conn,
        &lineage,
        &branch,
        &RevisionId::from_db(result.revision_id.clone()).unwrap(),
        &RevisionId::from_db(first.revision_id.clone()).unwrap(),
        3,
    )
    .unwrap();
    reclaim_fixture(&mut conn, &lineage);
    let head = load_branch_record(&conn, &lineage, &branch, false)
        .unwrap()
        .head;
    assert_eq!(apply(&mut conn, &lineage, &branch, &changed), result);
    assert_eq!(
        load_branch_record(&conn, &lineage, &branch, false)
            .unwrap()
            .head,
        head
    );
    let historical = load_revision_state(
        &conn,
        &lineage,
        &load_revision(
            &conn,
            &lineage,
            &RevisionId::from_db(result.revision_id).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        historical.metadata.title.as_deref(),
        Some("retained result")
    );
    let mut missing = next(&command, &lineage, &first);
    missing.expected = head;
    missing.scalars.title = Some("missing result".into());
    let fingerprint = crate::compact_session_commit_fingerprint(&missing).unwrap();
    let receipt = SaveReceipt {
        session_id: branch.as_str().to_owned(),
        previous: head,
        current: head,
        lineage_id: Some(lineage.as_str().to_owned()),
        history_text_bytes: first.receipt.history_text_bytes,
    };
    insert_session_receipt(
        &conn,
        &lineage,
        &branch,
        &fingerprint,
        "save",
        &receipt,
        None,
        None,
        None,
        4,
    )
    .unwrap();
    let before = archive_publication_counts(&conn);
    assert!(matches!(
        apply_compact_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &missing,
            ObjectCompression::none()
        ),
        Err(SessionCommitFailure::Integrity { .. })
    ));
    assert_eq!(archive_publication_counts(&conn), before);
    assert_eq!(
        load_branch_record(&conn, &lineage, &branch, false)
            .unwrap()
            .head,
        head
    );
}

fn publication_command(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    mode: &str,
) -> CompactSessionCommit {
    let command = archived(branch, 3);
    if mode == "initial" {
        return command;
    }
    let current = load_branch_record(conn, lineage, branch, false).unwrap();
    let result = SessionCommitResult {
        receipt: SaveReceipt {
            session_id: branch.as_str().to_owned(),
            previous: StoreHead::default(),
            current: current.head,
            lineage_id: Some(lineage.as_str().to_owned()),
            history_text_bytes: current.revision.history_root.byte_count(),
        },
        revision_id: current.revision.id.as_str().to_owned(),
    };
    let mut command = next(&command, lineage, &result);
    if mode == "changed" {
        command.scalars.title = Some("changed publication".into());
        command.scalars.updated_at = 2;
        command.archives.checkpoint = crate::CheckpointEdit::Replace {
            value: Some(event("changed active", 1, 99)),
        };
    }
    if mode == "header" {
        command.archives.checkpoint = crate::CheckpointEdit::ReplaceRecord {
            record: checkpoint_record(crate::CheckpointSummary::BaseCheckpoint, 99),
        };
        command.archives.checkpoint_events = CheckpointEventsEdit::ReplaceRecordsSuffix {
            retain_records: 2,
            records: vec![checkpoint_record(
                crate::CheckpointSummary::BaseEvent { record: 2 },
                99,
            )],
        };
    }
    command
}

fn publication_event(role: &str) -> &'static str {
    if role == "result" {
        "AFTER INSERT ON lineage_session_receipt_results"
    } else if role == "summary_presence" {
        "AFTER INSERT ON lineage_checkpoint_summary_presence"
    } else {
        archive_publication_event(role)
    }
}

#[test]
fn native_compact_publication_errors_roll_back_all_owners_and_results() {
    for mode in ["initial", "changed", "header", "noop"] {
        let roles: &[&str] = if mode == "noop" {
            &["receipt", "result"]
        } else {
            &[
                "value",
                "coordinates",
                "summary_presence",
                "root",
                "owner",
                "revision",
                "receipt",
                "result",
            ]
        };
        for role in roles {
            let (mut conn, lineage) = setup();
            let branch = branch_id('a');
            if mode != "initial" {
                apply(&mut conn, &lineage, &branch, &archived(&branch, 3));
            }
            let command = publication_command(&conn, &lineage, &branch, mode);
            let before = archive_publication_counts(&conn);
            let snapshot = load_branch_snapshot(&conn, &lineage, &branch, false)
                .optional_store()
                .unwrap();
            conn.execute_batch(&format!("CREATE TEMP TRIGGER reject_native_publication {} BEGIN SELECT RAISE(ABORT, 'injected native publication failure'); END;", publication_event(role))).unwrap();
            assert!(
                apply_compact_session_commit(
                    &mut conn,
                    &lineage,
                    &branch,
                    &command,
                    ObjectCompression::none()
                )
                .is_err(),
                "{mode}/{role}"
            );
            assert_eq!(archive_publication_counts(&conn), before, "{mode}/{role}");
            assert!(
                load_branch_snapshot(&conn, &lineage, &branch, false)
                    .optional_store()
                    .unwrap()
                    == snapshot
            );
            conn.execute_batch("DROP TRIGGER reject_native_publication")
                .unwrap();
            let result = apply(&mut conn, &lineage, &branch, &command);
            assert_eq!(apply(&mut conn, &lineage, &branch, &command), result);
        }
    }
}

#[test]
fn native_compact_publication_is_process_crash_atomic() {
    const ROLE: &str = "SMELT_NATIVE_COMPACT_CRASH_ROLE";
    const MODE: &str = "SMELT_NATIVE_COMPACT_CRASH_MODE";
    const DB: &str = "SMELT_NATIVE_COMPACT_CRASH_DB";
    let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
    let branch = branch_id('a');
    if let (Ok(role), Ok(mode), Ok(path)) =
        (std::env::var(ROLE), std::env::var(MODE), std::env::var(DB))
    {
        let mut conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        let command = publication_command(&conn, &lineage, &branch, &mode);
        if role == "commit" {
            conn.commit_hook(Some(|| -> bool { std::process::abort() }))
                .unwrap();
        } else {
            conn.create_scalar_function(
                "smelt_test_native_crash",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                |_| -> rusqlite::Result<i64> { std::process::abort() },
            )
            .unwrap();
            conn.execute_batch(&format!("CREATE TEMP TRIGGER crash_native_publication {} BEGIN SELECT smelt_test_native_crash(); END;", publication_event(&role))).unwrap();
        }
        let result = apply_compact_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        );
        panic!("native crash boundary was not reached: {}", result.is_ok());
    }
    let dir = tempfile::tempdir().unwrap();
    for mode in ["initial", "changed", "header", "noop"] {
        let roles: &[&str] = if mode == "noop" {
            &["receipt", "result", "commit"]
        } else {
            &[
                "value",
                "coordinates",
                "summary_presence",
                "root",
                "owner",
                "revision",
                "receipt",
                "result",
                "commit",
            ]
        };
        for role in roles {
            let path = dir.path().join(format!("native-{mode}-{role}.db"));
            let (before, snapshot) = {
                let mut conn = Connection::open(&path).unwrap();
                conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;").unwrap();
                crate::schema::initialize_lineage_schema(&mut conn).unwrap();
                create_lineage(&conn, &lineage, 1).unwrap();
                if mode != "initial" {
                    apply(&mut conn, &lineage, &branch, &archived(&branch, 3));
                }
                (
                    archive_publication_counts(&conn),
                    load_branch_snapshot(&conn, &lineage, &branch, false)
                        .optional_store()
                        .unwrap(),
                )
            };
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("lineage::tests::compact::native_compact_publication_is_process_crash_atomic")
                .arg("--nocapture")
                .env(ROLE, role)
                .env(MODE, mode)
                .env(DB, &path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(status.signal(), Some(libc::SIGABRT), "{mode}/{role}");
            }
            assert!(!status.success());
            let mut conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            assert_eq!(
                conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                    .unwrap(),
                "ok"
            );
            assert!(conn
                .prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query([])
                .unwrap()
                .next()
                .unwrap()
                .is_none());
            crate::schema::validate_lineage_schema(&conn).unwrap();
            assert_eq!(archive_publication_counts(&conn), before, "{mode}/{role}");
            assert!(
                load_branch_snapshot(&conn, &lineage, &branch, false)
                    .optional_store()
                    .unwrap()
                    == snapshot
            );
            let command = publication_command(&conn, &lineage, &branch, mode);
            let result = apply(&mut conn, &lineage, &branch, &command);
            assert_eq!(apply(&mut conn, &lineage, &branch, &command), result);
        }
    }
}

#[test]
fn native_compact_archive_base_lookup_is_independent_of_revision_count() {
    let mut counts = Vec::new();
    for saves in [1, 256] {
        let (mut conn, lineage) = setup();
        let branch = branch_id('a');
        let command = initial(&branch);
        let first = apply(&mut conn, &lineage, &branch, &command);
        let mut latest = first.clone();
        for index in 0..saves {
            let mut command = next(&command, &lineage, &first);
            command.expected = latest.receipt.current;
            command.scalars.title = Some(format!("title {index}"));
            latest = apply(&mut conn, &lineage, &branch, &command);
        }
        let mut command = next(&command, &lineage, &latest);
        command.scalars.title = Some("final native title".into());
        let steps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = steps.clone();
        conn.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
        apply(&mut conn, &lineage, &branch, &command);
        conn.progress_handler(0, None::<fn() -> bool>).unwrap();
        counts.push(steps.load(std::sync::atomic::Ordering::Relaxed));
    }
    eprintln!(
        "NATIVE_COMPACT_BASE_LOOKUP revisions=1/256 vm_steps={}/{}",
        counts[0], counts[1]
    );
    assert!(
        counts[1] <= counts[0] + 512,
        "base lookup must use its exact branch sequence, not scan revision associations"
    );
}

#[test]
fn native_compact_invalid_bases_suffixes_and_coordinates_are_atomic() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let command = archived(&branch, 3);
    let first = apply(&mut conn, &lineage, &branch, &command);
    let valid = next(&command, &lineage, &first);
    let snapshot = load_branch_snapshot(&conn, &lineage, &branch, false).unwrap();
    let before = archive_publication_counts(&conn);
    let mut invalid = Vec::new();
    let mut case = valid.clone();
    case.archive_base = None;
    invalid.push(case);
    let mut case = valid.clone();
    case.archive_base.as_mut().unwrap().lineage_id = "2".repeat(32);
    invalid.push(case);
    let mut case = valid.clone();
    case.archive_base.as_mut().unwrap().revision_id = "2".repeat(64);
    invalid.push(case);
    let mut case = valid.clone();
    case.archive_base.as_mut().unwrap().branch_sequence = crate::Revision::ZERO;
    invalid.push(case);
    let mut case = valid.clone();
    case.expected.history_len = crate::HistoryLen::new(2);
    invalid.push(case);
    let mut case = valid.clone();
    case.history.final_len = crate::HistoryLen::ZERO;
    case.history.start = HistoryIndex::ZERO;
    invalid.push(case);
    let mut case = valid.clone();
    case.archives.metadata_snapshots = ArchiveEdit::ReplaceSuffix {
        retain_records: 2,
        records: Vec::new(),
    };
    invalid.push(case);
    let mut case = valid.clone();
    case.archives.metadata_snapshots = ArchiveEdit::ReplaceSuffix {
        retain_records: 1,
        records: vec![MetadataArchiveRow {
            index: HistoryIndex::ZERO,
            fields: serde_json::json!({}),
            message: MetadataMessage::Active,
        }],
    };
    invalid.push(case);
    let mut case = valid.clone();
    case.archives.context_snapshots = ArchiveEdit::ReplaceSuffix {
        retain_records: 0,
        records: vec![
            ArchiveRow {
                index: HistoryIndex::new(1),
                value: serde_json::Value::Null,
            },
            ArchiveRow {
                index: HistoryIndex::ZERO,
                value: serde_json::Value::Null,
            },
        ],
    };
    invalid.push(case);
    let mut case = valid.clone();
    case.archives.checkpoint_events = CheckpointEventsEdit::ReplaceSuffix {
        retain_records: 3,
        records: vec![event("earlier", 0, 4)],
    };
    invalid.push(case);
    let mut case = valid.clone();
    case.archives.metadata_snapshots = ArchiveEdit::ReplaceSuffix {
        retain_records: 0,
        records: vec![MetadataArchiveRow {
            index: HistoryIndex::new(1),
            fields: serde_json::json!({"first_user_message":"inline"}),
            message: MetadataMessage::None,
        }],
    };
    invalid.push(case);
    for command in invalid {
        assert!(apply_compact_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none()
        )
        .is_err());
        assert_eq!(archive_publication_counts(&conn), before);
        assert_eq!(
            load_branch_snapshot(&conn, &lineage, &branch, false).unwrap(),
            snapshot
        );
    }
    assert_eq!(
        apply(&mut conn, &lineage, &branch, &valid).receipt.current,
        first.receipt.current
    );
}
