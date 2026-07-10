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
        backup_reader.session_state().unwrap().unwrap().id,
        session.id
    );

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
}
