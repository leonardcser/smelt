use std::process::Command;

use smelt_test_support::ProcessEnvironmentGuard;

fn smelt(state_home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_smelt"))
        .env("XDG_STATE_HOME", state_home)
        .args(args)
        .output()
        .expect("run smelt")
}

#[cfg(unix)]
#[tokio::test]
async fn session_complete_targets_owning_turn_after_cwd_changes() {
    use protocol::event::{CONTROL_ENV_KEYS, CONTROL_SESSION_ENV};
    use smelt_core::session_control::SessionControl;
    let control = SessionControl::default();
    control.initialize().unwrap();
    control.begin_turn("session-a", 7);
    let env = control.shell_env();
    let run = |env: std::collections::HashMap<String, String>| async move {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_smelt"));
        for key in CONTROL_ENV_KEYS {
            command.env_remove(key);
        }
        command
            .args(["session", "complete"])
            .current_dir("/")
            .envs(env)
            .output()
            .await
            .unwrap()
    };
    let missing = run(Default::default()).await;
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("no current smelt session"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    let mut wrong = env.clone();
    wrong.insert(CONTROL_SESSION_ENV.into(), "other-session".into());
    assert!(!run(wrong).await.status.success());
    let output = run(env.clone()).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!control.is_completed("session-a"));
    assert!(control.finish_turn("session-a", 7, true).is_none());
    assert!(control.is_completed("session-a"));
    control.begin_turn("session-a", 8);
    assert!(
        !run(env).await.status.success(),
        "stale shell must not complete a later turn"
    );
    let mut shell = smelt_core::process::ShellSpec {
        env: control.shell_env(),
        ..Default::default()
    };
    shell
        .env
        .insert("SMELT_TEST_BIN".into(), env!("CARGO_BIN_EXE_smelt").into());
    let output = smelt_core::process::JobSupervisor::new()
        .run(
            "exec \"$SMELT_TEST_BIN\" session complete",
            smelt_core::process::JobRunConfig {
                timeout: Some(std::time::Duration::from_secs(10)),
                shell,
                cwd: std::path::PathBuf::from("/"),
                started_at: std::time::Instant::now(),
                cancel: None,
                background_on_timeout: false,
                detachable: false,
            },
            |_| {},
        )
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(control.finish_turn("session-a", 8, true).is_none());
    assert!(control.is_completed("session-a"));
}

#[test]
fn session_storage_commands_doctor_and_backup() {
    let state = tempfile::tempdir().unwrap();
    let guard = ProcessEnvironmentGuard::capture();
    guard.set_var("XDG_STATE_HOME", state.path());
    let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            "persist me",
        )));
    smelt_core::session::storage().save(&session).unwrap();

    let doctor = smelt(state.path(), &["session", "doctor", &session.id, "--json"]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(report["session_id"], session.id);
    assert_eq!(report["issues"], serde_json::json!([]));
    assert_eq!(report["stats"]["history_rows"], 1);
    let plain = smelt(state.path(), &["session", "doctor"]);
    assert!(plain.status.success());
    let plain = String::from_utf8(plain.stdout).unwrap();
    assert!(plain.contains("status: healthy"), "{plain}");
    assert!(!plain.contains("history_rows"), "{plain}");

    let backup = state.path().join("portable.db");
    let output = smelt(
        state.path(),
        &["session", "backup", backup.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let copied: String = rusqlite::Connection::open(&backup)
        .unwrap()
        .query_row("SELECT id FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(copied, session.id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&backup).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let repeated = smelt(
        state.path(),
        &["session", "backup", backup.to_str().unwrap()],
    );
    assert!(!repeated.status.success());
    let missing = smelt(state.path(), &["session", "doctor", "ffff"]);
    assert!(!missing.status.success());
}
