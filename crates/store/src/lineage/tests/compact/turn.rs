use super::*;
use crate::{CompactSubmitTurn, CompactTurnTransition};

fn submission(branch: &BranchId, events: usize) -> CompactSubmitTurn {
    CompactSubmitTurn {
        session: archived(branch, events),
        turn: crate::NewTurn {
            kind: TurnKind::Command,
            submitted_history_idx: HistoryIndex::ZERO,
            continuation_of: None,
            created_at_ms: 2,
        },
    }
}

fn transition(
    command: &CompactSubmitTurn,
    lineage: &LineageId,
    result: &crate::CompactSubmitTurnResult,
) -> CompactTurnTransition {
    CompactTurnTransition {
        session: next(&command.session, lineage, &result.session),
        turn_id: result.turn_id,
        state: TurnState::Running,
        at_ms: 3,
        terminal_reason: None,
    }
}

fn turn_rows(conn: &Connection, lineage: &LineageId, branch: &BranchId) -> Vec<StoredTurn> {
    let ids = conn.prepare("SELECT turn_id FROM lineage_turns WHERE lineage_id = ?1 AND session_id = ?2 ORDER BY turn_id")
        .unwrap().query_map((lineage.as_str(), branch.as_str()), |row| row.get::<_, i64>(0))
        .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    ids.into_iter()
        .map(|id| {
            stored_lineage_turn(conn, lineage, branch, TurnId::new(id.try_into().unwrap()))
                .unwrap()
                .unwrap()
        })
        .collect()
}

fn lifecycle_counts(conn: &Connection) -> Vec<i64> {
    let mut counts = archive_publication_counts(conn);
    for table in ["lineage_turns", "lineage_turn_transitions"] {
        counts.push(
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap(),
        );
    }
    counts.push(
        conn.query_row(
            "SELECT coalesce(sum(next_turn_id), 0) FROM lineage_branches",
            [],
            |row| row.get(0),
        )
        .unwrap(),
    );
    counts
}

#[test]
fn turn_submission_rejects_reclaimed_saved_result_without_allocating() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let initial = initial_session_commit(&branch);
    let first = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &initial,
        ObjectCompression::none(),
    )
    .unwrap();
    let first_id =
        branch_revision_at_sequence(&conn, &lineage, &branch, first.current.revision.get())
            .unwrap();
    let mut session = initial.clone();
    session.expected = first.current;
    session.metadata.updated_at = 2;
    session.history.items = vec![protocol::HistoryItem::system("two")];
    let saved = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &session,
        ObjectCompression::none(),
    )
    .unwrap();
    let saved_id =
        branch_revision_at_sequence(&conn, &lineage, &branch, saved.current.revision.get())
            .unwrap();
    rewind_branch(&mut conn, &lineage, &branch, &saved_id, &first_id, 3).unwrap();
    reclaim_fixture(&mut conn, &lineage);
    assert!(load_revision(&conn, &lineage, &saved_id).is_err());
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &session,
            ObjectCompression::none(),
        )
        .unwrap(),
        saved
    );
    let before = lifecycle_counts(&conn);
    let head = load_branch_record(&conn, &lineage, &branch, false)
        .unwrap()
        .head;
    let command = SubmitTurn {
        session,
        turn: crate::NewTurn {
            kind: TurnKind::Command,
            submitted_history_idx: HistoryIndex::ZERO,
            continuation_of: None,
            created_at_ms: 4,
        },
    };
    assert!(apply_lineage_submit_turn(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .is_err());
    assert_eq!(lifecycle_counts(&conn), before);
    assert_eq!(
        load_branch_record(&conn, &lineage, &branch, false)
            .unwrap()
            .head,
        head
    );
    assert!(
        recover_lineage_submit_turn(&conn, &lineage, &branch, &command)
            .unwrap()
            .is_none()
    );
}

#[test]
fn native_compact_turn_public_writer_lifecycle_reopens_and_replays_exact_results() {
    let root = tempfile::tempdir().unwrap();
    let branch = branch_id('a');
    let mut writer = crate::OwnedLineageWriter::open(root.path(), branch.as_str()).unwrap();
    let lineage = LineageId::from_hex(writer.lineage_id().to_owned()).unwrap();
    let command = submission(&branch, 3);
    assert!(writer
        .recover_compact_submit_turn(&command)
        .unwrap()
        .is_none());
    let submitted = writer.submit_compact_turn(&command).unwrap();
    assert_eq!(submitted.turn_id.get(), 1);
    assert_eq!(writer.submit_compact_turn(&command).unwrap(), submitted);
    assert_eq!(
        writer.recover_compact_submit_turn(&command).unwrap(),
        Some(submitted.clone())
    );
    let running = transition(&command, &lineage, &submitted);
    assert!(writer
        .recover_compact_turn_transition(&running)
        .unwrap()
        .is_none());
    let started = writer.transition_compact_turn(&running).unwrap();
    assert_eq!(started.session.revision_id, submitted.session.revision_id);
    assert_eq!(
        started.session.receipt.current,
        submitted.session.receipt.current
    );
    let mut completed = running.clone();
    completed.session = next(&command.session, &lineage, &started.session);
    completed.session.scalars.title = Some("completed".into());
    completed.session.scalars.updated_at = 4;
    completed.state = TurnState::Completed;
    completed.at_ms = 4;
    completed.terminal_reason = Some("complete α\0".into());
    let finished = writer.transition_compact_turn(&completed).unwrap();
    assert_ne!(finished.session.revision_id, submitted.session.revision_id);
    let snapshot = writer.snapshot().unwrap();
    writer.release().unwrap();
    let mut writer = crate::OwnedLineageWriter::open_existing_in_lineage(
        root.path(),
        lineage.as_str(),
        branch.as_str(),
    )
    .unwrap();
    assert_eq!(writer.submit_compact_turn(&command).unwrap(), submitted);
    assert_eq!(writer.transition_compact_turn(&running).unwrap(), started);
    assert_eq!(
        writer.transition_compact_turn(&completed).unwrap(),
        finished
    );
    assert_eq!(
        writer.recover_compact_turn_transition(&completed).unwrap(),
        Some(finished)
    );
    assert_eq!(writer.snapshot().unwrap(), snapshot);
    let reader = crate::LineageSessionReader::open_existing(root.path(), branch.as_str()).unwrap();
    let turns = reader.turns().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].state, TurnState::Completed);
    assert_eq!(
        turns[0].submitted_revision,
        submitted.session.receipt.current.revision
    );
}

#[test]
fn native_compact_turn_submission_uses_exact_saved_result_after_head_advances() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let command = submission(&branch, 3);
    let saved = apply(&mut conn, &lineage, &branch, &command.session);
    let mut changed = next(&command.session, &lineage, &saved);
    changed.scalars.title = Some("later".into());
    changed.history.start = HistoryIndex::ZERO;
    changed.history.items = vec![protocol::HistoryItem::system("two")];
    let latest = apply(&mut conn, &lineage, &branch, &changed);
    let result = apply_compact_submit_turn(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    assert_eq!(result.session, saved);
    let turn = stored_lineage_turn(&conn, &lineage, &branch, result.turn_id)
        .unwrap()
        .unwrap();
    assert_eq!(turn.submitted_revision, saved.receipt.current.revision);
    assert_eq!(
        turn.submitted_history_hash,
        crate::history::item_hash(&command.session.history.items[0]).unwrap()
    );
    assert_eq!(
        load_branch_record(&conn, &lineage, &branch, false)
            .unwrap()
            .head,
        latest.receipt.current
    );
}

#[test]
fn native_compact_turn_rewind_gc_replay_and_deleted_owner_release_are_exact() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let first = apply(&mut conn, &lineage, &branch, &initial(&branch));
    let mut command = submission(&branch, 3);
    command.session.expected = first.receipt.current;
    command.session.archive_base = Some(SessionArchiveBase {
        lineage_id: lineage.as_str().to_owned(),
        revision_id: first.revision_id.clone(),
        branch_sequence: first.receipt.current.revision,
    });
    command.session.scalars.updated_at = 2;
    let submitted = apply_compact_submit_turn(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    let mut running = transition(&command, &lineage, &submitted);
    running.session.scalars.title = Some("running result".into());
    running.session.scalars.updated_at = 3;
    running.session.archives.checkpoint = crate::CheckpointEdit::ReplaceRecord {
        record: checkpoint_record(crate::CheckpointSummary::BaseCheckpoint, 3),
    };
    running.session.archives.checkpoint_events = CheckpointEventsEdit::ReplaceRecordsSuffix {
        retain_records: 3,
        records: vec![checkpoint_record(crate::CheckpointSummary::Checkpoint, 3)],
    };
    let started = apply_compact_turn_transition(
        &mut conn,
        &lineage,
        &branch,
        &running,
        ObjectCompression::none(),
    )
    .unwrap();
    rewind_branch(
        &mut conn,
        &lineage,
        &branch,
        &RevisionId::from_db(started.session.revision_id.clone()).unwrap(),
        &RevisionId::from_db(first.revision_id).unwrap(),
        4,
    )
    .unwrap();
    reclaim_fixture(&mut conn, &lineage);
    let head = load_branch_record(&conn, &lineage, &branch, false)
        .unwrap()
        .head;
    assert_eq!(
        recover_compact_submit_turn(&conn, &lineage, &branch, &command).unwrap(),
        Some(submitted.clone())
    );
    assert_eq!(
        recover_compact_turn_transition(&conn, &lineage, &branch, &running).unwrap(),
        Some(started.clone())
    );
    assert_eq!(
        apply_compact_turn_transition(
            &mut conn,
            &lineage,
            &branch,
            &running,
            ObjectCompression::none()
        )
        .unwrap(),
        started
    );
    assert_eq!(
        load_branch_record(&conn, &lineage, &branch, false)
            .unwrap()
            .head,
        head
    );
    verify_session_receipt_results(&conn, &lineage).unwrap();
    verify_archive_coordinates(&conn, &lineage).unwrap();
    delete_branch(&conn, &lineage, &branch, 5).unwrap();
    reclaim_fixture(&mut conn, &lineage);
    for table in [
        "lineage_session_receipt_results",
        "lineage_checkpoint_summary_presence",
        "lineage_turns",
        "lineage_turn_transitions",
    ] {
        assert_eq!(
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn native_compact_turn_missing_results_fail_closed_without_allocating_or_transitioning() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let command = submission(&branch, 0);
    let submitted = apply_compact_submit_turn(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    let mut missing = command.clone();
    missing.turn.created_at_ms = 9;
    let fingerprint = crate::compact_submit_turn_fingerprint(&missing).unwrap();
    insert_session_receipt(
        &conn,
        &lineage,
        &branch,
        &fingerprint,
        "submit_turn",
        &submitted.session.receipt,
        Some(submitted.turn_id),
        Some(TurnState::Ready),
        None,
        9,
    )
    .unwrap();
    let running = transition(&command, &lineage, &submitted);
    let fingerprint = crate::compact_turn_transition_fingerprint(&running).unwrap();
    insert_session_receipt(
        &conn,
        &lineage,
        &branch,
        &fingerprint,
        "turn_transition",
        &submitted.session.receipt,
        Some(submitted.turn_id),
        Some(TurnState::Running),
        None,
        3,
    )
    .unwrap();
    let before = lifecycle_counts(&conn);
    assert!(matches!(
        recover_compact_submit_turn(&conn, &lineage, &branch, &missing),
        Err(SessionCommitFailure::Integrity { .. })
    ));
    assert!(matches!(
        apply_compact_submit_turn(
            &mut conn,
            &lineage,
            &branch,
            &missing,
            ObjectCompression::none()
        ),
        Err(SessionCommitFailure::Integrity { .. })
    ));
    assert!(matches!(
        recover_compact_turn_transition(&conn, &lineage, &branch, &running),
        Err(SessionCommitFailure::Integrity { .. })
    ));
    assert!(matches!(
        apply_compact_turn_transition(
            &mut conn,
            &lineage,
            &branch,
            &running,
            ObjectCompression::none()
        ),
        Err(SessionCommitFailure::Integrity { .. })
    ));
    assert_eq!(lifecycle_counts(&conn), before);
    assert_eq!(
        stored_lineage_turn(&conn, &lineage, &branch, submitted.turn_id)
            .unwrap()
            .unwrap()
            .state,
        TurnState::Ready
    );
}

#[test]
fn native_compact_turn_invalid_commands_and_transition_order_are_atomic() {
    for terminal in [
        TurnState::Completed,
        TurnState::Interrupted,
        TurnState::Failed,
        TurnState::Cancelled,
    ] {
        let (mut conn, lineage) = setup();
        let branch = branch_id('a');
        let command = submission(&branch, 3);
        let submitted = apply_compact_submit_turn(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .unwrap();
        let running = transition(&command, &lineage, &submitted);
        let before = lifecycle_counts(&conn);
        let mut cases = Vec::new();
        for (id, state, at, reason) in [
            (0, TurnState::Running, 3, None),
            (2, TurnState::Running, 3, None),
            (1, TurnState::Ready, 3, None),
            (1, TurnState::Running, 1, None),
            (1, TurnState::Running, 3, Some("reason".into())),
            (1, TurnState::Completed, 3, None),
            (1, TurnState::Failed, 3, Some("α".repeat(513))),
        ] {
            let mut invalid = running.clone();
            invalid.turn_id = TurnId::new(id);
            invalid.state = state;
            invalid.at_ms = at;
            invalid.terminal_reason = reason;
            cases.push(invalid);
        }
        for invalid in cases {
            assert!(apply_compact_turn_transition(
                &mut conn,
                &lineage,
                &branch,
                &invalid,
                ObjectCompression::none()
            )
            .is_err());
            assert_eq!(lifecycle_counts(&conn), before);
        }
        let started = apply_compact_turn_transition(
            &mut conn,
            &lineage,
            &branch,
            &running,
            ObjectCompression::none(),
        )
        .unwrap();
        let mut finished = running;
        finished.session = next(&command.session, &lineage, &started.session);
        finished.state = terminal;
        finished.at_ms = 4;
        let result = apply_compact_turn_transition(
            &mut conn,
            &lineage,
            &branch,
            &finished,
            ObjectCompression::none(),
        )
        .unwrap();
        assert_eq!(result.state, terminal);
        assert_eq!(
            apply_compact_turn_transition(
                &mut conn,
                &lineage,
                &branch,
                &finished,
                ObjectCompression::none()
            )
            .unwrap(),
            result
        );
    }
}

fn boundary(role: &str, mode: &str) -> String {
    let kind = if mode == "submit" {
        "submit_turn"
    } else {
        "turn_transition"
    };
    match role {
        "allocation" => "AFTER UPDATE ON lineage_branches WHEN NEW.next_turn_id != OLD.next_turn_id".into(),
        "turn" if mode == "submit" => "AFTER INSERT ON lineage_turns".into(),
        "turn" => "AFTER UPDATE ON lineage_turns".into(),
        "transition" => "AFTER INSERT ON lineage_turn_transitions".into(),
        "receipt" => format!("AFTER INSERT ON lineage_session_receipts WHEN NEW.command_kind = '{kind}'"),
        "result" => format!("AFTER INSERT ON lineage_session_receipt_results WHEN EXISTS (SELECT 1 FROM lineage_session_receipts WHERE lineage_id = NEW.lineage_id AND session_id = NEW.session_id AND fingerprint = NEW.fingerprint AND command_kind = '{kind}')"),
        _ => panic!("unknown lifecycle boundary"),
    }
}

fn seed(conn: &mut Connection, lineage: &LineageId, branch: &BranchId, mode: &str) {
    if mode == "submit" {
        return;
    }
    let command = submission(branch, 3);
    let submitted =
        apply_compact_submit_turn(conn, lineage, branch, &command, ObjectCompression::none())
            .unwrap();
    if mode == "complete" {
        apply_compact_turn_transition(
            conn,
            lineage,
            branch,
            &transition(&command, lineage, &submitted),
            ObjectCompression::none(),
        )
        .unwrap();
    }
}

fn run_lifecycle(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    mode: &str,
) -> std::result::Result<SessionCommitResult, SessionCommitFailure> {
    let command = submission(branch, 3);
    if mode == "submit" {
        return apply_compact_submit_turn(
            conn,
            lineage,
            branch,
            &command,
            ObjectCompression::none(),
        )
        .map(|result| result.session);
    }
    let submitted = recover_compact_submit_turn(conn, lineage, branch, &command)?.unwrap();
    let mut update = transition(&command, lineage, &submitted);
    if mode == "complete" {
        let started = recover_compact_turn_transition(conn, lineage, branch, &update)?.unwrap();
        update.session = next(&command.session, lineage, &started.session);
        update.session.scalars.title = Some("complete".into());
        update.session.scalars.updated_at = 4;
        update.state = TurnState::Completed;
        update.at_ms = 4;
    }
    apply_compact_turn_transition(conn, lineage, branch, &update, ObjectCompression::none())
        .map(|result| result.session)
}

#[test]
fn native_compact_turn_publication_errors_roll_back_session_turn_and_result() {
    for mode in ["submit", "running", "complete"] {
        let roles: &[&str] = if mode == "submit" {
            &["allocation", "turn", "receipt", "result"]
        } else {
            &["turn", "receipt", "transition", "result"]
        };
        for role in roles {
            let (mut conn, lineage) = setup();
            let branch = branch_id('a');
            seed(&mut conn, &lineage, &branch, mode);
            let before = lifecycle_counts(&conn);
            let snapshot = load_branch_snapshot(&conn, &lineage, &branch, false)
                .optional_store()
                .unwrap();
            let turns = turn_rows(&conn, &lineage, &branch);
            conn.execute_batch(&format!("CREATE TEMP TRIGGER reject_compact_turn {} BEGIN SELECT RAISE(ABORT, 'injected compact turn failure'); END;", boundary(role, mode))).unwrap();
            assert!(
                run_lifecycle(&mut conn, &lineage, &branch, mode).is_err(),
                "{mode}/{role}"
            );
            assert_eq!(lifecycle_counts(&conn), before, "{mode}/{role}");
            assert_eq!(
                load_branch_snapshot(&conn, &lineage, &branch, false)
                    .optional_store()
                    .unwrap(),
                snapshot
            );
            assert_eq!(turn_rows(&conn, &lineage, &branch), turns);
            conn.execute_batch("DROP TRIGGER reject_compact_turn")
                .unwrap();
            let result = run_lifecycle(&mut conn, &lineage, &branch, mode).unwrap();
            assert_eq!(
                run_lifecycle(&mut conn, &lineage, &branch, mode).unwrap(),
                result
            );
        }
    }
}

#[test]
fn native_compact_turn_publication_is_crash_atomic() {
    const ROLE: &str = "SMELT_COMPACT_TURN_CRASH_ROLE";
    const MODE: &str = "SMELT_COMPACT_TURN_CRASH_MODE";
    const DB: &str = "SMELT_COMPACT_TURN_CRASH_DB";
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
        if role == "commit" {
            conn.commit_hook(Some(|| -> bool { std::process::abort() }))
                .unwrap();
        } else {
            conn.create_scalar_function(
                "smelt_test_compact_turn_crash",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                |_| -> rusqlite::Result<i64> { std::process::abort() },
            )
            .unwrap();
            conn.execute_batch(&format!("CREATE TEMP TRIGGER crash_compact_turn {} BEGIN SELECT smelt_test_compact_turn_crash(); END;", boundary(&role, &mode))).unwrap();
        }
        let result = run_lifecycle(&mut conn, &lineage, &branch, &mode);
        panic!(
            "compact turn crash boundary was not reached: {}",
            result.is_ok()
        );
    }
    let dir = tempfile::tempdir().unwrap();
    for mode in ["submit", "running", "complete"] {
        let roles: &[&str] = if mode == "submit" {
            &["allocation", "turn", "receipt", "result", "commit"]
        } else {
            &["turn", "receipt", "transition", "result", "commit"]
        };
        for role in roles {
            let path = dir.path().join(format!("{mode}-{role}.db"));
            let (before, snapshot, turns) = {
                let mut conn = Connection::open(&path).unwrap();
                conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;").unwrap();
                crate::schema::initialize_lineage_schema(&mut conn).unwrap();
                create_lineage(&conn, &lineage, 1).unwrap();
                seed(&mut conn, &lineage, &branch, mode);
                (
                    lifecycle_counts(&conn),
                    load_branch_snapshot(&conn, &lineage, &branch, false)
                        .optional_store()
                        .unwrap(),
                    turn_rows(&conn, &lineage, &branch),
                )
            };
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "lineage::tests::compact::turn::native_compact_turn_publication_is_crash_atomic", "--nocapture"])
                .env(ROLE, role).env(MODE, mode).env(DB, &path)
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
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
            assert_eq!(lifecycle_counts(&conn), before, "{mode}/{role}");
            assert_eq!(
                load_branch_snapshot(&conn, &lineage, &branch, false)
                    .optional_store()
                    .unwrap(),
                snapshot
            );
            assert_eq!(turn_rows(&conn, &lineage, &branch), turns);
            let result = run_lifecycle(&mut conn, &lineage, &branch, mode).unwrap();
            assert_eq!(
                run_lifecycle(&mut conn, &lineage, &branch, mode).unwrap(),
                result
            );
        }
    }
}
