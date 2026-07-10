use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

const PROBE_DB: &str = "SMELT_CORE_STORAGE_PROBE_DB";
const PROBE_READY: &str = "SMELT_CORE_STORAGE_PROBE_READY";
const PROBE_RELEASE: &str = "SMELT_CORE_STORAGE_PROBE_RELEASE";

#[test]
fn storage_owner_probe() {
    let Some(db_path) = std::env::var_os(PROBE_DB).map(PathBuf::from) else {
        return;
    };
    let ready = required_path(PROBE_READY);
    let release = required_path(PROBE_RELEASE);
    let session_dir = db_path.parent().expect("database parent");
    let session_id = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("session id");
    let _writer = smelt_store::OwnedSessionWriter::open(session_dir, session_id)
        .expect("claim session writer");
    touch(&ready);
    wait_for(&release);
}

#[test]
fn delete_refuses_session_owned_by_another_process() {
    let state = tempfile::tempdir().expect("state dir");
    std::env::set_var("XDG_STATE_HOME", state.path());
    let id = "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let session_dir = smelt_core::session::dir_for_id(id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let db_path = session_dir.join("session.db");
    drop(smelt_store::SessionDb::open(&db_path).expect("create database"));
    let ready = state.path().join("delete-owner.ready");
    let release = state.path().join("delete-owner.release");
    let mut owner = spawn_owner(&db_path, &ready, &release);
    wait_for(&ready);

    let err = smelt_core::session::delete(id).expect_err("active owner prevents deletion");

    assert!(matches!(
        err,
        smelt_core::session::SessionStoreError::ReadOnlyOwnerConflict { .. }
    ));
    assert!(session_dir.exists());
    touch(&release);
    assert!(owner.wait().unwrap().success());
}

#[test]
fn repair_needed_read_does_not_mutate_while_another_process_owns_session() {
    let root = tempfile::tempdir().expect("temp dir");
    let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let session_dir = root.path().join(id);
    std::fs::create_dir(&session_dir).expect("create session dir");
    let db_path = session_dir.join("session.db");
    let mut session = smelt_core::session::Session::new(1, PathBuf::from("/tmp"));
    session.id = id.into();
    session.history = vec![
        protocol::HistoryItem::user(protocol::Content::text("old prompt")),
        protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("recent reply")),
            None,
            Vec::new(),
        )),
    ];
    let db = smelt_store::SessionDb::open(&db_path).expect("create database");
    db.save_session_snapshot_for_import(
        &smelt_core::session::store_snapshot_from_session(&session, 0).expect("build snapshot"),
    )
    .expect("save fixture");
    drop(db);

    let ready = root.path().join("owner.ready");
    let release = root.path().join("owner.release");
    let mut owner = spawn_owner(&db_path, &ready, &release);
    wait_for(&ready);
    let db = smelt_store::SessionDb::open(&db_path).expect("open database for failure injection");
    db.connection()
        .execute(
            "UPDATE session_state SET checkpoint_json = ?1 WHERE singleton = 1",
            [serde_json::json!({
                "kind": "compaction",
                "summary": "retained summary",
                "first_live_index": 177,
                "created_at_ms": 1,
            })
            .to_string()],
        )
        .expect("corrupt checkpoint boundary");
    drop(db);

    let (header, _) =
        smelt_core::session::load_store_header_for_dir(session_dir).expect("resume header loads");
    assert_eq!(
        header
            .meta
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.first_live_index),
        Some(0)
    );
    let persisted = smelt_store::SessionDb::open_read_only(&db_path)
        .expect("open database after read")
        .session_state()
        .expect("read persisted state")
        .expect("session state")
        .checkpoint_json
        .expect("checkpoint");
    assert_eq!(persisted["first_live_index"].as_u64(), Some(177));

    touch(&release);
    let status = owner.wait().expect("wait for owner process");
    assert!(status.success(), "owner process failed with {status}");
}

fn spawn_owner(db_path: &Path, ready: &Path, release: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .arg("--exact")
        .arg("storage_owner_probe")
        .arg("--nocapture")
        .env(PROBE_DB, db_path)
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
