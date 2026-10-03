use std::process::Command;

use smelt_test_support::ProcessEnvironmentGuard;

fn smelt(state_home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_smelt"))
        .env("XDG_STATE_HOME", state_home)
        .args(args)
        .output()
        .expect("run smelt")
}

#[test]
fn session_storage_commands_doctor_backup_gc_and_vacuum() {
    let state = tempfile::tempdir().unwrap();
    let guard = ProcessEnvironmentGuard::capture();
    guard.set_var("XDG_STATE_HOME", state.path());
    let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            "persist me",
        )));
    smelt_core::session::save_result(&session).unwrap();
    assert!(smelt_core::session::wait_for_session_catalog(
        std::time::Duration::from_secs(5)
    ));
    let sessions_root = smelt_core::session::sessions_dir();

    let doctor = smelt(state.path(), &["session", "doctor", &session.id, "--json"]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report[0]["session_id"], session.id);
    assert_eq!(report[0]["report"]["healthy"], true);
    assert_eq!(report[0]["report"]["stats"]["history_rows"], 1);

    let backup = state.path().join("portable.db");
    let backup_output = smelt(
        state.path(),
        &["session", "backup", &session.id, backup.to_str().unwrap()],
    );
    assert!(
        backup_output.status.success(),
        "{}",
        String::from_utf8_lossy(&backup_output.stderr)
    );
    let manifest = state.path().join("portable.db.manifest.json");
    assert!(backup.is_file());
    assert!(manifest.is_file());
    let manifest_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(manifest_json["format_version"], 1);
    let lineage_id = manifest_json["lineage_id"].as_str().unwrap();
    assert!(
        smelt_store::verify_lineage_backup(&backup, lineage_id)
            .unwrap()
            .healthy
    );

    let gc = smelt(state.path(), &["session", "gc", &session.id]);
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    let gc = String::from_utf8(gc.stdout).unwrap();
    assert!(gc.contains("deleted_canonical_rows:"), "{gc}");
    assert!(gc.contains("deleted_search_segments: 0"), "{gc}");
    let vacuum = smelt(state.path(), &["session", "vacuum", &session.id]);
    assert!(
        vacuum.status.success(),
        "{}",
        String::from_utf8_lossy(&vacuum.stderr)
    );
    let repeated_backup = smelt(
        state.path(),
        &["session", "backup", &session.id, backup.to_str().unwrap()],
    );
    assert!(!repeated_backup.status.success());

    let mut writer = smelt_store::OwnedLineageWriter::open_existing(&sessions_root, &session.id)
        .expect("open canonical writer");
    let previous = writer.store_head().unwrap();
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            "submitted before restart",
        )));
    let command = smelt_store::SubmitTurn {
        session: smelt_core::session::store_commit_from_session(
            &session,
            previous,
            previous.history_len.get() as usize,
        )
        .unwrap(),
        turn: smelt_store::NewTurn {
            kind: smelt_store::TurnKind::User,
            submitted_history_idx: smelt_store::HistoryIndex::new(previous.history_len.get()),
            continuation_of: None,
            created_at_ms: 42,
        },
    };
    let receipt = writer.submit_turn(&command).unwrap();
    writer.release().unwrap();

    let doctor = smelt(state.path(), &["session", "doctor", &session.id, "--json"]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let recovery = &report[0]["recovery"];
    assert_eq!(report[0]["report"]["healthy"], true);
    assert_eq!(
        recovery["canonical_revision"],
        receipt.session.current.revision.get()
    );
    assert_eq!(recovery["nonterminal_turns"][0]["turn_id"], 1);
    assert_eq!(recovery["nonterminal_turns"][0]["state"], "ready");
    assert!(matches!(
        recovery["catalog"]["state"].as_str(),
        Some("lagging" | "missing")
    ));
    let plain = smelt(state.path(), &["session", "doctor", &session.id]);
    assert!(plain.status.success());
    let plain = String::from_utf8(plain.stdout).unwrap();
    assert!(plain.contains("nonterminal_turn: id=1 state=ready"));
    assert!(plain.contains("catalog: state="));

    let reader =
        smelt_store::LineageSessionReader::open_existing(sessions_root, &session.id).unwrap();
    assert_eq!(
        reader
            .turns()
            .unwrap()
            .into_iter()
            .find(|turn| turn.turn_id == receipt.turn_id)
            .unwrap()
            .state,
        smelt_store::TurnState::Ready,
        "doctor must not mutate nonterminal turns"
    );
}

#[test]
fn session_maintenance_exact_id_does_not_require_catalog_publication() {
    let state = tempfile::tempdir().unwrap();
    let guard = ProcessEnvironmentGuard::capture();
    guard.set_var("XDG_STATE_HOME", state.path());
    let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            "canonical fixture",
        )));
    let sessions_root = smelt_core::session::sessions_dir();
    let layout = smelt_store::SessionStoreLayout::from_sessions_root(&sessions_root);
    let mut writer = smelt_store::OwnedLineageWriter::open(&sessions_root, &session.id).unwrap();
    let command = smelt_core::session::initial_store_commit_from_session(&session).unwrap();
    writer.commit_session(&command).unwrap();
    writer.release().unwrap();
    assert!(
        smelt_store::CatalogReader::open_existing(layout.catalog_path())
            .unwrap()
            .is_none()
    );
    let doctor = smelt(state.path(), &["session", "doctor", &session.id, "--json"]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report[0]["report"]["healthy"], true);
    assert_eq!(report[0]["recovery"]["catalog"]["state"], "missing");
    let gc = smelt(state.path(), &["session", "gc", &session.id]);
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    let backup = state.path().join("canonical.db");
    let output = smelt(
        state.path(),
        &["session", "backup", &session.id, backup.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let vacuum = smelt(state.path(), &["session", "vacuum", &session.id]);
    assert!(
        vacuum.status.success(),
        "{}",
        String::from_utf8_lossy(&vacuum.stderr)
    );
    assert!(
        smelt_store::CatalogReader::open_existing(layout.catalog_path())
            .unwrap()
            .is_none()
    );
}

#[test]
fn session_gc_shares_reachable_legacy_archives_and_recovers_file_space() {
    let state = tempfile::tempdir().unwrap();
    let sessions_root = state.path().join("smelt/sessions");
    let mut session = smelt_core::session::Session::new(1, "/synthetic".into());
    session.created_at_ms = 1;
    session.updated_at_ms = 1;
    session.history = vec![protocol::HistoryItem::user(protocol::Content::text(
        "retained synthetic history",
    ))];
    let mut writer = smelt_store::OwnedLineageWriter::open(&sessions_root, &session.id).unwrap();
    let database = writer.database_path();
    let lineage = writer.lineage_id().to_owned();
    {
        let source = rusqlite::Connection::open_in_memory().unwrap();
        source
            .execute_batch(include_str!("../crates/store/src/lineage_v3.sql"))
            .unwrap();
        source
            .execute_batch(
                "PRAGMA user_version = 3;
             INSERT INTO store_meta (key, value) VALUES ('schema_version', '3');",
            )
            .unwrap();
        source
            .execute("INSERT INTO lineage_identity VALUES (1, ?1, 1)", [&lineage])
            .unwrap();
        let mut conn = rusqlite::Connection::open(&database).unwrap();
        rusqlite::backup::Backup::new(&source, &mut conn)
            .unwrap()
            .run_to_completion(128, std::time::Duration::ZERO, None)
            .unwrap();
    }
    let mut random = 0x123456789abcdef0_u64;
    let summary = (0..512 * 1024)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            char::from(b'!' + (random % 90) as u8)
        })
        .collect::<String>();
    let mut command = smelt_core::session::initial_store_commit_from_session(&session).unwrap();
    command.metadata.checkpoint_events_json = Some(serde_json::json!([{
        "kind": "auto", "summary": summary, "first_live_index": 0,
        "completed_at_history_len": 0, "created_at_ms": 1,
    }]));
    let mut publications = Vec::new();
    for index in 0..8 {
        command.metadata.title = Some(format!("retained title {index}"));
        command.metadata.updated_at = index + 1;
        let receipt = writer.commit_session(&command).unwrap();
        publications.push((command.clone(), receipt.clone()));
        command.expected = receipt.current;
        command.history.start = smelt_store::HistoryIndex::new(1);
        command.history.items.clear();
    }

    writer.release().unwrap();
    let mut writer = smelt_store::OwnedLineageWriter::open_existing_in_lineage(
        &sessions_root,
        &lineage,
        &session.id,
    )
    .unwrap();
    let mut captures = Vec::new();
    for sequence in (1..=8).rev() {
        if sequence != 8 {
            writer.rewind_to_sequence(sequence, 100 + sequence).unwrap();
        }
        let target = format!("{:064x}", sequence + 1000);
        writer.fork_current(&target, 200 + sequence).unwrap();
        let reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
            &sessions_root,
            &lineage,
            &target,
        )
        .unwrap();
        let snapshot = reader.snapshot().unwrap();
        assert_eq!(
            snapshot.metadata.title,
            Some(format!("retained title {}", sequence - 1))
        );
        let mut export = Vec::new();
        reader.export_history_jsonl(&mut export).unwrap();
        captures.push((target, snapshot, export));
    }
    let source_before = writer.snapshot().unwrap();
    writer.release().unwrap();
    let hashes = {
        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        captures
            .iter()
            .map(|(_, snapshot, _)| {
                conn.query_row(
                    "SELECT payload.object_hash, object.raw_size
                     FROM lineage_revisions revision
                     JOIN lineage_payload_object_refs payload
                       ON payload.lineage_id = revision.lineage_id
                      AND payload.payload_id = revision.state_payload_id
                     JOIN objects object ON object.hash = payload.object_hash
                     WHERE revision.lineage_id = ?1 AND revision.revision_id = ?2",
                    (&lineage, &snapshot.revision_id),
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap()
            })
            .collect::<Vec<_>>()
    };
    let reader =
        smelt_store::LineageSessionReader::open_existing(&sessions_root, &session.id).unwrap();
    let before = reader.storage_stats().unwrap();
    drop(reader);

    let gc = smelt(state.path(), &["session", "gc", &session.id]);
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    let reader =
        smelt_store::LineageSessionReader::open_existing(&sessions_root, &session.id).unwrap();
    assert_eq!(reader.snapshot().unwrap(), source_before);
    let after = reader.storage_stats().unwrap();
    drop(reader);
    for (target, snapshot, export) in captures {
        let reader =
            smelt_store::LineageSessionReader::open_existing(&sessions_root, &target).unwrap();
        assert_eq!(reader.snapshot().unwrap(), snapshot);
        let mut retained_export = Vec::new();
        reader.export_history_jsonl(&mut retained_export).unwrap();
        assert_eq!(retained_export, export);
        assert!(reader.doctor_report().unwrap().healthy);
    }
    {
        let conn = rusqlite::Connection::open(&database).unwrap();
        for (hash, size) in hashes {
            assert_eq!(
                conn.query_row(
                    "SELECT raw_size FROM objects WHERE hash = ?1",
                    [&hash],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
                size
            );
        }
    }
    let mut writer =
        smelt_store::OwnedLineageWriter::open_existing(&sessions_root, &session.id).unwrap();
    for (command, receipt) in publications {
        assert_eq!(writer.commit_session(&command).unwrap(), receipt);
        assert_eq!(writer.store_head().unwrap(), source_before.head);
    }
    writer.release().unwrap();
    assert!(
        after.object_stored_bytes < before.object_stored_bytes / 2,
        "reachable legacy repetition survived CLI maintenance: before={} after={}",
        before.object_stored_bytes,
        after.object_stored_bytes
    );
    let before_files = before.database_bytes + before.wal_bytes;
    let after_files = after.database_bytes + after.wal_bytes;
    assert!(
        after_files < before_files / 2,
        "CLI maintenance did not recover actual database/WAL bytes: before={before_files} after={after_files}"
    );
    assert!(String::from_utf8_lossy(&gc.stdout).contains("wal_truncated: true"));
    println!(
        "legacy GC: stored_bytes_before={} stored_bytes_after={} files_before={} files_after={}",
        before.object_stored_bytes, after.object_stored_bytes, before_files, after_files,
    );
    let repeated = smelt(state.path(), &["session", "gc", &session.id]);
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert!(String::from_utf8_lossy(&repeated.stdout).contains("shared_objects: 0"));
    let reader =
        smelt_store::LineageSessionReader::open_existing(&sessions_root, &session.id).unwrap();
    let repeated_stats = reader.storage_stats().unwrap();
    assert_eq!(
        repeated_stats.object_stored_bytes,
        after.object_stored_bytes
    );
    assert!(repeated_stats.database_bytes + repeated_stats.wal_bytes < before_files / 2);
    assert_eq!(reader.snapshot().unwrap(), source_before);
}

#[test]
fn session_gc_prunes_ready_search_and_reports_writer_contention() {
    let state = tempfile::tempdir().unwrap();
    let sessions_root = state.path().join("smelt/sessions");
    let mut session = smelt_core::session::Session::new(1, "/synthetic".into());
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            "retained history",
        )));
    let mut writer = smelt_store::OwnedLineageWriter::open(&sessions_root, &session.id).unwrap();
    let record = |index: u64, text: String| smelt_store::StoredTranscriptBlock {
        block_idx: index,
        history_idx: Some(0),
        kind: "assistant".into(),
        tool_call_id: None,
        tool_name: None,
        content_hash: format!("{index:064x}"),
        estimated_text_bytes: text.len() as u64,
        preview_text: text.clone(),
        block_json: serde_json::json!({"Text": {"content": text.clone()}}).to_string(),
        indexed_text: text,
        origin_json: None,
        tool_state_json: None,
        tool_render_revision: 0,
    };
    let mut command = smelt_core::session::initial_store_commit_from_session(&session).unwrap();
    command.transcript_records = Some(smelt_store::TranscriptRecordSuffix {
        start: smelt_store::TranscriptRecordIndex::ZERO,
        records: (0..1024)
            .map(|index| record(index, format!("retained needle {index}")))
            .collect(),
    });
    let original = command.clone();
    let receipt = writer.commit_session(&command).unwrap();
    let reader =
        smelt_store::LineageSessionReader::open_existing(&sessions_root, &session.id).unwrap();
    let projector = writer.spawn_search_projector().unwrap();
    let wait = || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !projector.is_idle() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(projector.latest_error(), None);
        let status = reader.search_projection_status().unwrap();
        assert_eq!(status.state, smelt_store::SearchProjectionState::Current);
        status.ready_segments
    };
    projector.request();
    assert_eq!(wait(), 1);
    command.expected = receipt.current;
    command.metadata.updated_at += 1;
    command.history.start = smelt_store::HistoryIndex::new(1);
    command.history.items.clear();
    command.side_tables.start = smelt_store::HistoryIndex::new(1);
    command.transcript_records = Some(smelt_store::TranscriptRecordSuffix {
        start: smelt_store::TranscriptRecordIndex::new(1024),
        records: vec![record(1024, "obsolete suffix".into())],
    });
    writer.commit_session(&command).unwrap();
    projector.request();
    assert_eq!(wait(), 2);
    drop(projector);
    writer
        .rewind_to_sequence(1, u64::try_from(command.metadata.updated_at + 1).unwrap())
        .unwrap();
    let snapshot = writer.snapshot().unwrap();
    writer.release().unwrap();
    let candidates = reader
        .search_transcript_candidate_page(
            "needle",
            None,
            smelt_store::TranscriptSearchDirection::Forward,
            3,
        )
        .unwrap();
    let path = reader.search_database_path();
    let search = rusqlite::Connection::open(&path).unwrap();
    search.execute_batch("BEGIN IMMEDIATE").unwrap();
    let busy = smelt(state.path(), &["session", "gc", &session.id]);
    assert!(
        !busy.status.success(),
        "contended CLI maintenance silently succeeded"
    );
    assert!(String::from_utf8_lossy(&busy.stderr).contains("locked"));
    assert!(
        path.exists(),
        "contended CLI maintenance deleted a healthy cache"
    );
    assert_eq!(
        search
            .query_row("SELECT count(*) FROM search_segments", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(reader.snapshot().unwrap(), snapshot);
    assert_eq!(
        reader.search_projection_status().unwrap().state,
        smelt_store::SearchProjectionState::Current
    );
    assert_eq!(
        reader
            .search_transcript_candidate_page(
                "needle",
                None,
                smelt_store::TranscriptSearchDirection::Forward,
                3,
            )
            .unwrap(),
        candidates
    );
    search.execute_batch("ROLLBACK").unwrap();
    drop(search);
    let gc = smelt(state.path(), &["session", "gc", &session.id]);
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    assert!(String::from_utf8_lossy(&gc.stdout).contains("deleted_search_segments: 1"));
    let repeated = smelt(state.path(), &["session", "gc", &session.id]);
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    let output = String::from_utf8_lossy(&repeated.stdout);
    for counter in [
        "deleted_search_segments: 0",
        "deleted_canonical_rows: 0",
        "deleted_objects: 0",
    ] {
        assert!(output.contains(counter), "{output}");
    }
    assert_eq!(
        reader.search_projection_status().unwrap().state,
        smelt_store::SearchProjectionState::Current
    );
    assert_eq!(
        reader
            .search_transcript_candidate_page(
                "needle",
                None,
                smelt_store::TranscriptSearchDirection::Forward,
                3,
            )
            .unwrap(),
        candidates
    );
    assert_eq!(reader.snapshot().unwrap(), snapshot);
    assert!(reader.doctor_report().unwrap().healthy);
    let mut writer =
        smelt_store::OwnedLineageWriter::open_existing(&sessions_root, &session.id).unwrap();
    assert_eq!(writer.commit_session(&original).unwrap(), receipt);
    assert_eq!(writer.snapshot().unwrap(), snapshot);
}

#[test]
fn session_gc_reclaims_abandoned_suffix_and_preserves_shared_fork() {
    let state = tempfile::tempdir().unwrap();
    let guard = ProcessEnvironmentGuard::capture();
    guard.set_var("XDG_STATE_HOME", state.path());
    let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
    session.id = "a".repeat(64);
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            "shared prefix",
        )));
    smelt_core::session::save_result(&session).unwrap();
    let sessions_root = smelt_core::session::sessions_dir();
    let target_id = "b".repeat(64);
    let mut writer =
        smelt_store::OwnedLineageWriter::open_existing(&sessions_root, &session.id).unwrap();
    writer.fork_current(&target_id, 2).unwrap();
    let shared = writer.store_head().unwrap();
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            "abandoned suffix",
        )));
    let command = smelt_core::session::store_commit_from_session(
        &session,
        shared,
        shared.history_len.get() as usize,
    )
    .unwrap();
    let abandoned = writer.commit_session(&command).unwrap();
    assert_eq!(abandoned.current.revision.get(), 2);
    let updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    writer.rewind_to_sequence(1, updated_at).unwrap();
    writer.release().unwrap();
    let writer =
        smelt_store::OwnedLineageWriter::open_existing(&sessions_root, &target_id).unwrap();
    writer.refresh_catalog().unwrap();
    writer.release().unwrap();

    let gc = smelt(state.path(), &["session", "gc", &session.id]);
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    let output = String::from_utf8(gc.stdout).unwrap();
    assert!(output.contains("deleted_canonical_rows:"), "{output}");
    assert!(!output.contains("deleted_canonical_rows: 0"), "{output}");

    for branch in [&session.id, &target_id] {
        let reader =
            smelt_store::LineageSessionReader::open_existing(&sessions_root, branch).unwrap();
        let history = reader.history_range(0, 1).unwrap();
        assert_eq!(
            history,
            vec![protocol::HistoryItem::user(protocol::Content::text(
                "shared prefix"
            ))]
        );
    }

    let repeated = smelt(state.path(), &["session", "gc", &target_id]);
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    let repeated = String::from_utf8(repeated.stdout).unwrap();
    assert!(repeated.contains("deleted_canonical_rows: 0"), "{repeated}");
    assert!(repeated.contains("deleted_objects: 0"), "{repeated}");
    assert!(
        repeated.contains("deleted_search_segments: 0"),
        "{repeated}"
    );
}
