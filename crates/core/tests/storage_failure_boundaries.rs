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
    let db = smelt_store::SessionDb::open(db_path).expect("open owner database");
    db.acquire_current_process_writer_lease()
        .expect("claim writer lease");
    touch(&ready);
    wait_for(&release);
}

#[test]
fn repair_needed_resume_currently_mutates_while_another_process_owns_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("session.db");
    let mut session = smelt_core::session::Session::new(1, PathBuf::from("/tmp"));
    session.id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into();
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

    let ready = dir.path().join("owner.ready");
    let release = dir.path().join("owner.release");
    let mut owner = spawn_owner(&db_path, &ready, &release);
    wait_for(&ready);

    let (header, _) = smelt_core::session::load_store_header_for_dir(dir.path().to_path_buf())
        .expect("resume header loads");
    assert_eq!(
        header
            .meta
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.first_live_index),
        Some(0)
    );
    let repaired = smelt_store::SessionDb::open_read_only(&db_path)
        .expect("open repaired database")
        .session_state()
        .expect("read repaired state")
        .expect("session state")
        .checkpoint_json
        .expect("checkpoint");
    assert_eq!(repaired["first_live_index"].as_u64(), Some(0));

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
