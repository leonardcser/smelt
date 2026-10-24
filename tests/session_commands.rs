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
