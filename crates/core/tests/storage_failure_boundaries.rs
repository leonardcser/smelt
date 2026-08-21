use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use smelt_test_support::ProcessEnvironmentGuard;

const PROBE_ROOT: &str = "SMELT_CORE_STORAGE_PROBE_ROOT";
const PROBE_SESSION_ID: &str = "SMELT_CORE_STORAGE_PROBE_SESSION_ID";
const PROBE_READY: &str = "SMELT_CORE_STORAGE_PROBE_READY";
const PROBE_RELEASE: &str = "SMELT_CORE_STORAGE_PROBE_RELEASE";

#[test]
fn storage_owner_probe() {
    let Some(root) = std::env::var_os(PROBE_ROOT).map(PathBuf::from) else {
        return;
    };
    let session_id = std::env::var(PROBE_SESSION_ID).expect("session id");
    let ready = required_path(PROBE_READY);
    let release = required_path(PROBE_RELEASE);
    let _writer = smelt_store::OwnedLineageWriter::open_existing(root, session_id)
        .expect("claim lineage writer");
    touch(&ready);
    wait_for(&release);
}

#[test]
fn delete_refuses_session_owned_by_another_process() {
    let state = tempfile::tempdir().expect("state dir");
    let environment = ProcessEnvironmentGuard::capture();
    environment.set_var("XDG_STATE_HOME", state.path());
    let id = "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let sessions_root = smelt_core::session::sessions_dir();
    let mut session = smelt_core::session::Session::new(1, PathBuf::from("/tmp"));
    session.id = id.into();
    session.history = vec![protocol::HistoryItem::user(protocol::Content::text(
        "owned",
    ))];
    smelt_core::session::save_result(&session).expect("create database");
    let ready = state.path().join("delete-owner.ready");
    let release = state.path().join("delete-owner.release");
    let mut owner = spawn_owner(&sessions_root, id, &ready, &release);
    wait_for(&ready);

    let err = smelt_core::session::delete(id).expect_err("active owner prevents deletion");

    assert!(matches!(
        err,
        smelt_core::session::SessionStoreError::ReadOnlyOwnerConflict { .. }
    ));
    assert!(
        smelt_store::LineageSessionReader::open_existing(&sessions_root, id).is_ok(),
        "canonical lineage branch remains available"
    );
    touch(&release);
    assert!(owner.wait().unwrap().success());
}

fn spawn_owner(root: &Path, session_id: &str, ready: &Path, release: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .arg("--exact")
        .arg("storage_owner_probe")
        .arg("--nocapture")
        .env(PROBE_ROOT, root)
        .env(PROBE_SESSION_ID, session_id)
        .env(PROBE_READY, ready)
        .env(PROBE_RELEASE, release)
        .spawn()
        .expect("spawn owner process")
}

fn required_path(name: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("missing {name}"))
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn touch(path: &Path) {
    std::fs::write(path, b"ready").unwrap_or_else(|err| panic!("write {}: {err}", path.display()));
}
