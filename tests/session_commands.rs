use std::process::Command;

use smelt_test_support::ProcessEnvironmentGuard;

fn smelt(state_home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_smelt"))
        .env("XDG_STATE_HOME", state_home)
        .args(args)
        .output()
        .expect("run smelt")
}

fn saved_session(number: u128) -> smelt_core::session::Session {
    let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
    session.id = format!("{number:064x}");
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            "migration fixture",
        )));
    smelt_core::session::save_result(&session).unwrap();
    session
}

fn set_schema_version(session: &smelt_core::session::Session, version: i32) {
    let db = smelt_store::SessionDb::open(smelt_core::session::dir_for(session).join("session.db"))
        .unwrap();
    db.connection()
        .pragma_update(None, "user_version", version)
        .unwrap();
}

fn schema_status(session: &smelt_core::session::Session) -> smelt_store::SessionSchemaStatus {
    let session_dir = smelt_core::session::dir_for(session);
    smelt_store::session_schema_status(session_dir.parent().unwrap(), &session.id).unwrap()
}

fn make_request_audit_only_orphan(session: &smelt_core::session::Session) {
    let db = smelt_store::SessionDb::open(smelt_core::session::dir_for(session).join("session.db"))
        .unwrap();
    db.connection()
        .execute_batch(
            "DELETE FROM transcript_search_chars;
             DELETE FROM transcript_search;
             DELETE FROM transcript_extent_chunks;
             DELETE FROM transcript_blocks;
             DELETE FROM turns;
             DELETE FROM history_object_refs;
             DELETE FROM turn_metas;
             DELETE FROM metadata_snapshots;
             DELETE FROM accounting_snapshots;
             DELETE FROM history_items;
             DELETE FROM session_state;
             INSERT INTO request_attempts (started_at) VALUES (1);
             PRAGMA user_version = 9;",
        )
        .unwrap();
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
    let session_dir = smelt_core::session::dir_for(&session);

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
    let backup_reader = smelt_store::SessionReader::open_database(&backup).unwrap();
    assert_eq!(
        backup_reader.stored_session().unwrap().unwrap().identity.id,
        session.id
    );

    for args in [
        vec!["session", "gc", session.id.as_str()],
        vec!["session", "vacuum", session.id.as_str()],
    ] {
        let output = smelt(state.path(), &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let repeated_backup = smelt(
        state.path(),
        &["session", "backup", &session.id, backup.to_str().unwrap()],
    );
    assert!(!repeated_backup.status.success());

    let sessions_root = session_dir.parent().expect("sessions root");
    let mut writer = smelt_store::OwnedSessionWriter::open_existing(sessions_root, &session.id)
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

    let reader = smelt_store::SessionReader::open_existing(&session_dir).unwrap();
    assert_eq!(
        reader.turn(receipt.turn_id).unwrap().unwrap().state,
        smelt_store::TurnState::Ready,
        "doctor must not mutate nonterminal turns"
    );
}

#[test]
fn session_migrate_all_succeeds_for_empty_storage() {
    let state = tempfile::tempdir().unwrap();
    let output = smelt(
        state.path(),
        &["session", "migrate", "--all", "--dry-run", "--json"],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["sessions"], serde_json::json!([]));
    assert_eq!(report["summary"]["total"], 0);
}

#[test]
fn session_migrate_single_dry_run_and_prefix_upgrade() {
    let state = tempfile::tempdir().unwrap();
    let guard = ProcessEnvironmentGuard::capture();
    guard.set_var("XDG_STATE_HOME", state.path());
    let session = saved_session(101);
    set_schema_version(&session, 9);
    let prefix = &session.id[..12];

    let dry_run = smelt(
        state.path(),
        &["session", "migrate", prefix, "--dry-run", "--json"],
    );
    assert!(
        dry_run.status.success(),
        "{}",
        String::from_utf8_lossy(&dry_run.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["sessions"][0]["session_id"], session.id);
    assert_eq!(report["sessions"][0]["status"], "would_migrate");
    assert_eq!(report["sessions"][0]["from_version"], 9);
    assert_eq!(
        report["sessions"][0]["to_version"],
        smelt_store::SCHEMA_VERSION
    );
    assert_eq!(report["summary"]["would_migrate"], 1);
    assert_eq!(
        schema_status(&session),
        smelt_store::SessionSchemaStatus::Upgradeable {
            found: 9,
            target: smelt_store::SCHEMA_VERSION,
        }
    );

    let migrated = smelt(state.path(), &["session", "migrate", prefix, "--json"]);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&migrated.stdout).unwrap();
    assert_eq!(report["sessions"][0]["status"], "migrated");
    assert_eq!(report["summary"]["migrated"], 1);
    assert_eq!(
        schema_status(&session),
        smelt_store::SessionSchemaStatus::Current {
            version: smelt_store::SCHEMA_VERSION,
        }
    );
}

#[test]
fn session_migrate_all_reports_every_schema_and_continues_after_failures() {
    let state = tempfile::tempdir().unwrap();
    let guard = ProcessEnvironmentGuard::capture();
    guard.set_var("XDG_STATE_HOME", state.path());
    let current = saved_session(201);
    let future = saved_session(202);
    let unrecognized = saved_session(203);
    let missing_identity = saved_session(204);
    let old = saved_session(205);
    let orphan = saved_session(206);
    make_request_audit_only_orphan(&orphan);
    set_schema_version(&old, 9);
    set_schema_version(&future, smelt_store::SCHEMA_VERSION + 1);
    set_schema_version(&unrecognized, 0);
    let db = smelt_store::SessionDb::open(
        smelt_core::session::dir_for(&missing_identity).join("session.db"),
    )
    .unwrap();
    db.connection()
        .execute_batch("DELETE FROM session_state; PRAGMA user_version = 9")
        .unwrap();
    drop(db);

    let dry_run = smelt(
        state.path(),
        &["session", "migrate", "--all", "--dry-run", "--json"],
    );
    assert!(!dry_run.status.success());
    let report: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["summary"]["total"], 6);
    assert_eq!(report["summary"]["current"], 1);
    assert_eq!(report["summary"]["would_migrate"], 1);
    assert_eq!(report["summary"]["future"], 1);
    assert_eq!(report["summary"]["unrecognized"], 1);
    assert_eq!(report["summary"]["orphaned"], 1);
    assert_eq!(report["summary"]["busy"], 0);
    assert_eq!(report["summary"]["failed"], 1);
    let future_result = report["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["session_id"] == future.id)
        .unwrap();
    assert_eq!(future_result["to_version"], serde_json::Value::Null);
    assert_eq!(
        future_result["supported_version"],
        smelt_store::SCHEMA_VERSION
    );
    let corrupt_result = report["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["session_id"] == missing_identity.id)
        .unwrap();
    assert_eq!(corrupt_result["status"], "failed");
    assert_eq!(corrupt_result["error_kind"], "corrupt");
    let orphan_result = report["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["session_id"] == orphan.id)
        .unwrap();
    assert_eq!(orphan_result["status"], "orphaned");
    assert_eq!(orphan_result["error_kind"], "orphaned");
    assert_eq!(orphan_result["from_version"], 9);
    assert_eq!(orphan_result["to_version"], serde_json::Value::Null);
    assert_eq!(
        schema_status(&old),
        smelt_store::SessionSchemaStatus::Upgradeable {
            found: 9,
            target: smelt_store::SCHEMA_VERSION,
        },
        "dry-run must not rewrite upgradeable sessions"
    );

    let migrated = smelt(state.path(), &["session", "migrate", "--all", "--json"]);
    assert!(!migrated.status.success());
    let report: serde_json::Value = serde_json::from_slice(&migrated.stdout).unwrap();
    assert_eq!(report["summary"]["total"], 6);
    assert_eq!(report["summary"]["current"], 1);
    assert_eq!(report["summary"]["migrated"], 1);
    assert_eq!(report["summary"]["future"], 1);
    assert_eq!(report["summary"]["unrecognized"], 1);
    assert_eq!(report["summary"]["orphaned"], 1);
    assert_eq!(report["summary"]["busy"], 0);
    assert_eq!(report["summary"]["failed"], 1);
    assert_eq!(
        schema_status(&old),
        smelt_store::SessionSchemaStatus::Current {
            version: smelt_store::SCHEMA_VERSION,
        },
        "a failed bulk run must still migrate later upgradeable sessions"
    );
    assert_eq!(
        schema_status(&future),
        smelt_store::SessionSchemaStatus::Future {
            found: smelt_store::SCHEMA_VERSION + 1,
            supported: smelt_store::SCHEMA_VERSION,
        }
    );
    assert_eq!(
        schema_status(&unrecognized),
        smelt_store::SessionSchemaStatus::Unrecognized {
            found: 0,
            supported: smelt_store::SCHEMA_VERSION,
        }
    );
    assert!(matches!(
        smelt_store::session_schema_status(
            smelt_core::session::dir_for(&missing_identity)
                .parent()
                .unwrap(),
            &missing_identity.id,
        )
        .unwrap(),
        smelt_store::SessionSchemaStatus::Corrupt { found: 9, reason }
            if reason.contains("missing canonical identity")
    ));
    assert_eq!(
        smelt_store::session_schema_status(
            smelt_core::session::dir_for(&orphan).parent().unwrap(),
            &orphan.id,
        )
        .unwrap(),
        smelt_store::SessionSchemaStatus::Orphaned { found: 9 }
    );
    assert_eq!(
        schema_status(&current),
        smelt_store::SessionSchemaStatus::Current {
            version: smelt_store::SCHEMA_VERSION,
        }
    );
}

#[test]
fn session_migrate_reports_active_writer_as_busy() {
    let state = tempfile::tempdir().unwrap();
    let guard = ProcessEnvironmentGuard::capture();
    guard.set_var("XDG_STATE_HOME", state.path());
    let session = saved_session(301);
    let session_dir = smelt_core::session::dir_for(&session);
    let writer =
        smelt_store::OwnedSessionWriter::open_existing(session_dir.parent().unwrap(), &session.id)
            .unwrap();
    set_schema_version(&session, 9);

    let output = smelt(state.path(), &["session", "migrate", &session.id, "--json"]);

    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["sessions"][0]["status"], "busy");
    assert_eq!(report["sessions"][0]["error_kind"], "busy");
    assert_eq!(report["summary"]["busy"], 1);
    assert_eq!(report["summary"]["failed"], 0);
    assert!(report["sessions"][0]["error"]
        .as_str()
        .unwrap()
        .contains("close it and retry"));
    assert_eq!(
        schema_status(&session),
        smelt_store::SessionSchemaStatus::Upgradeable {
            found: 9,
            target: smelt_store::SCHEMA_VERSION,
        },
        "a busy migration must leave the schema unchanged"
    );
    writer.release().unwrap();
}

#[test]
fn session_quarantine_orphans_dry_run_then_moves_only_orphans() {
    let state = tempfile::tempdir().unwrap();
    let guard = ProcessEnvironmentGuard::capture();
    guard.set_var("XDG_STATE_HOME", state.path());
    let current = saved_session(401);
    let orphan = saved_session(402);
    make_request_audit_only_orphan(&orphan);
    let orphan_dir = smelt_core::session::dir_for(&orphan);

    let dry_run = smelt(
        state.path(),
        &[
            "session",
            "quarantine-orphans",
            "--all",
            "--dry-run",
            "--json",
        ],
    );
    assert!(
        dry_run.status.success(),
        "{}",
        String::from_utf8_lossy(&dry_run.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["summary"]["total"], 2);
    assert_eq!(report["summary"]["not_orphaned"], 1);
    assert_eq!(report["summary"]["would_quarantine"], 1);
    assert_eq!(report["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(report["sessions"][0]["session_id"], orphan.id);
    assert_eq!(report["sessions"][0]["status"], "would_quarantine");
    assert!(orphan_dir.is_dir(), "dry-run must not move the orphan");

    let quarantined = smelt(
        state.path(),
        &["session", "quarantine-orphans", "--all", "--json"],
    );
    assert!(
        quarantined.status.success(),
        "{}",
        String::from_utf8_lossy(&quarantined.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&quarantined.stdout).unwrap();
    assert_eq!(report["summary"]["quarantined"], 1);
    assert_eq!(report["summary"]["not_orphaned"], 1);
    assert_eq!(report["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(report["sessions"][0]["status"], "quarantined");
    let quarantine_path =
        std::path::PathBuf::from(report["sessions"][0]["quarantine_path"].as_str().unwrap());
    assert!(!orphan_dir.exists());
    assert!(quarantine_path.join("session.db").is_file());
    assert!(quarantine_path.starts_with(orphan_dir.parent().unwrap().join(".quarantine")));
    assert_eq!(
        schema_status(&current),
        smelt_store::SessionSchemaStatus::Current {
            version: smelt_store::SCHEMA_VERSION,
        }
    );
}

#[test]
fn session_quarantine_orphans_reports_active_writer_as_busy_and_retries() {
    let state = tempfile::tempdir().unwrap();
    let guard = ProcessEnvironmentGuard::capture();
    guard.set_var("XDG_STATE_HOME", state.path());
    let orphan = saved_session(451);
    let orphan_dir = smelt_core::session::dir_for(&orphan);
    let writer =
        smelt_store::OwnedSessionWriter::open_existing(orphan_dir.parent().unwrap(), &orphan.id)
            .unwrap();
    make_request_audit_only_orphan(&orphan);

    let busy = smelt(
        state.path(),
        &["session", "quarantine-orphans", &orphan.id, "--json"],
    );
    assert!(!busy.status.success());
    let report: serde_json::Value = serde_json::from_slice(&busy.stdout).unwrap();
    assert_eq!(report["sessions"][0]["status"], "busy");
    assert_eq!(report["sessions"][0]["error_kind"], "busy");
    assert_eq!(report["summary"]["busy"], 1);
    assert!(orphan_dir.is_dir(), "a busy orphan must not be moved");

    writer.release().unwrap();
    let retry = smelt(
        state.path(),
        &["session", "quarantine-orphans", &orphan.id, "--json"],
    );
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(report["sessions"][0]["status"], "quarantined");
    assert!(!orphan_dir.exists());
}

#[test]
fn session_quarantine_orphans_preserves_identity_less_canonical_data() {
    let state = tempfile::tempdir().unwrap();
    let guard = ProcessEnvironmentGuard::capture();
    guard.set_var("XDG_STATE_HOME", state.path());
    let corrupt = saved_session(501);
    let corrupt_dir = smelt_core::session::dir_for(&corrupt);
    let db = smelt_store::SessionDb::open(corrupt_dir.join("session.db")).unwrap();
    db.connection()
        .execute_batch("DELETE FROM session_state; PRAGMA user_version = 9")
        .unwrap();
    drop(db);

    let output = smelt(
        state.path(),
        &["session", "quarantine-orphans", &corrupt.id, "--json"],
    );

    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["sessions"][0]["status"], "failed");
    assert_eq!(report["sessions"][0]["error_kind"], "corrupt");
    assert_eq!(report["summary"]["failed"], 1);
    assert!(corrupt_dir.is_dir());
}
