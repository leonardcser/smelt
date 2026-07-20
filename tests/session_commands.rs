use std::process::Command;

struct StateHomeGuard(Option<std::ffi::OsString>);

impl StateHomeGuard {
    fn install(path: &std::path::Path) -> Self {
        let previous = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", path);
        Self(previous)
    }
}

impl Drop for StateHomeGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }
}

fn smelt(state_home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_smelt"))
        .env("XDG_STATE_HOME", state_home)
        .args(args)
        .output()
        .expect("run smelt")
}

#[test]
fn session_storage_commands_doctor_backup_rebuild_gc_and_vacuum() {
    let state = tempfile::tempdir().unwrap();
    let _guard = StateHomeGuard::install(state.path());
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

    // COMPAT(session-derived-sidecar-exports): seed and exercise explicit alpha exports.
    smelt_core::session::rebuild_compatibility_exports(&session_dir).unwrap();
    std::fs::remove_file(session_dir.join("meta.json")).unwrap();
    std::fs::remove_file(session_dir.join("content.txt")).unwrap();
    for args in [
        vec!["session", "rebuild-derived", session.id.as_str()],
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
    assert!(session_dir.join("meta.json").is_file());
    assert!(session_dir.join("content.txt").is_file());

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
    assert_eq!(recovery["compatibility_metadata"]["state"], "lagging");
    assert_eq!(recovery["compatibility_content"]["state"], "lagging");

    let plain = smelt(state.path(), &["session", "doctor", &session.id]);
    assert!(plain.status.success());
    let plain = String::from_utf8(plain.stdout).unwrap();
    assert!(plain.contains("nonterminal_turn: id=1 state=ready"));
    assert!(plain.contains("catalog: state="));
    assert!(plain.contains("compatibility_metadata: state=lagging"));
    assert!(plain.contains("compatibility_content: state=lagging"));

    std::fs::write(
        session_dir.join("content.txt"),
        "# smelt-revision:999999\nahead compatibility export\n",
    )
    .unwrap();
    let ahead = smelt(state.path(), &["session", "doctor", &session.id, "--json"]);
    assert!(ahead.status.success());
    let ahead: serde_json::Value = serde_json::from_slice(&ahead.stdout).unwrap();
    let content = &ahead[0]["recovery"]["compatibility_content"];
    assert_eq!(content["state"], "ahead");
    assert_eq!(content["revision_lag"], serde_json::Value::Null);

    let reader = smelt_store::SessionReader::open_existing(&session_dir).unwrap();
    assert_eq!(
        reader.turn(receipt.turn_id).unwrap().unwrap().state,
        smelt_store::TurnState::Ready,
        "doctor must not mutate nonterminal turns"
    );
}
