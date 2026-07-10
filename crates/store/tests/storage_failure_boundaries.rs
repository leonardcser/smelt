use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

const PROBE_ROLE: &str = "SMELT_STORAGE_PROBE_ROLE";
const PROBE_DB: &str = "SMELT_STORAGE_PROBE_DB";
const PROBE_READY: &str = "SMELT_STORAGE_PROBE_READY";
const PROBE_GO: &str = "SMELT_STORAGE_PROBE_GO";
const PROBE_RESULT: &str = "SMELT_STORAGE_PROBE_RESULT";

#[test]
fn storage_subprocess_probe() {
    let Ok(role) = std::env::var(PROBE_ROLE) else {
        return;
    };
    let db_path = required_path(PROBE_DB);
    let ready = required_path(PROBE_READY);
    let go = required_path(PROBE_GO);

    match role.as_str() {
        "claim" => {
            let db = smelt_store::SessionDb::open(&db_path).expect("open claim database");
            let observed = db.writer_lease().expect("read writer lease");
            touch(&ready);
            wait_for(&go);
            let result = if observed.is_none() {
                db.set_writer_lease(&smelt_store::WriterLease {
                    owner_id: format!("probe:{}", std::process::id()),
                    hostname: "probe-host".into(),
                    pid: std::process::id(),
                    app_version: "test".into(),
                    started_at: 1,
                    heartbeat_at: 1,
                })
                .map(|_| "owned".to_string())
                .unwrap_or_else(|err| format!("error:{err}"))
            } else {
                "conflict".into()
            };
            std::fs::write(required_path(PROBE_RESULT), result).expect("write claim result");
        }
        "lock" => {
            let conn = rusqlite::Connection::open(&db_path).expect("open lock database");
            conn.busy_timeout(Duration::from_secs(5))
                .expect("set lock busy timeout");
            conn.execute_batch("BEGIN IMMEDIATE")
                .expect("acquire write transaction");
            touch(&ready);
            wait_for(&go);
            conn.execute_batch("COMMIT")
                .expect("release write transaction");
        }
        "crash-transaction" => {
            let conn = rusqlite::Connection::open(&db_path).expect("open crash database");
            conn.execute_batch(
                "BEGIN IMMEDIATE;
                 INSERT INTO store_meta (key, value) VALUES ('crash_probe', 'uncommitted');",
            )
            .expect("stage uncommitted row");
            touch(&ready);
            std::process::abort();
        }
        other => panic!("unknown storage probe role {other}"),
    }
}

#[test]
fn simultaneous_process_claims_reproduce_non_atomic_writer_lease() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("session.db");
    let db = smelt_store::SessionDb::open(&db_path).expect("create database");
    db.clear_writer_lease().expect("clear writer lease");
    drop(db);

    let first_ready = dir.path().join("first.ready");
    let second_ready = dir.path().join("second.ready");
    let first_go = dir.path().join("first.go");
    let second_go = dir.path().join("second.go");
    let first_result = dir.path().join("first.result");
    let second_result = dir.path().join("second.result");
    let mut first = spawn_probe(
        "claim",
        &db_path,
        &first_ready,
        &first_go,
        Some(&first_result),
    );
    let mut second = spawn_probe(
        "claim",
        &db_path,
        &second_ready,
        &second_go,
        Some(&second_result),
    );
    // This barrier splits the same read-then-write sequence used by the
    // current claim implementation. Both processes observe no owner before
    // either is allowed to publish its lease.
    wait_for(&first_ready);
    wait_for(&second_ready);
    touch(&first_go);
    touch(&second_go);

    assert_success(first.wait().expect("wait for first claim"));
    assert_success(second.wait().expect("wait for second claim"));
    assert_eq!(read(&first_result), "owned");
    assert_eq!(read(&second_result), "owned");
}

#[test]
fn lock_contention_shorter_and_longer_than_busy_timeout_is_distinct() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("session.db");
    let db = smelt_store::SessionDb::open(&db_path).expect("create database");

    let short_ready = dir.path().join("short.ready");
    let short_go = dir.path().join("short.go");
    let mut short_lock = spawn_probe("lock", &db_path, &short_ready, &short_go, None);
    wait_for(&short_ready);
    let short_release = short_go.clone();
    let releaser = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        touch(&short_release);
    });
    let started = Instant::now();
    db.set_meta("short_contention", "committed")
        .expect("short contention should clear");
    let short_elapsed = started.elapsed();
    releaser.join().expect("join short lock releaser");
    assert_success(short_lock.wait().expect("wait for short lock"));
    assert!(
        short_elapsed >= Duration::from_millis(75),
        "write unexpectedly bypassed the lock after {short_elapsed:?}"
    );

    let long_ready = dir.path().join("long.ready");
    let long_go = dir.path().join("long.go");
    let mut long_lock = spawn_probe("lock", &db_path, &long_ready, &long_go, None);
    wait_for(&long_ready);
    let started = Instant::now();
    let err = db
        .set_meta("long_contention", "not committed")
        .expect_err("long contention should exhaust the sqlite timeout");
    let long_elapsed = started.elapsed();
    touch(&long_go);
    assert_success(long_lock.wait().expect("wait for long lock"));

    assert!(
        err.is_database_locked(),
        "unexpected contention error: {err}"
    );
    assert!(
        long_elapsed >= Duration::from_secs(4),
        "busy timeout returned too early after {long_elapsed:?}"
    );
    assert!(
        long_elapsed < Duration::from_secs(7),
        "busy timeout exceeded its intended bound: {long_elapsed:?}"
    );
}

#[test]
fn process_crash_rolls_back_an_open_canonical_transaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("session.db");
    drop(smelt_store::SessionDb::open(&db_path).expect("create database"));

    let ready = dir.path().join("crash.ready");
    let unused_go = dir.path().join("unused.go");
    let mut child = spawn_probe("crash-transaction", &db_path, &ready, &unused_go, None);
    wait_for(&ready);
    let status = child.wait().expect("wait for crashing child");
    assert!(!status.success(), "crash probe unexpectedly exited cleanly");

    let db = smelt_store::SessionDb::open_read_only(&db_path).expect("reopen after crash");
    assert_eq!(db.meta("crash_probe").expect("read crash row"), None);
    db.quick_check()
        .expect("database remains valid after crash");
}

fn spawn_probe(
    role: &str,
    db_path: &Path,
    ready: &Path,
    go: &Path,
    result: Option<&Path>,
) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .arg("--exact")
        .arg("storage_subprocess_probe")
        .arg("--nocapture")
        .env(PROBE_ROLE, role)
        .env(PROBE_DB, db_path)
        .env(PROBE_READY, ready)
        .env(PROBE_GO, go);
    if let Some(result) = result {
        command.env(PROBE_RESULT, result);
    }
    command.spawn().expect("spawn storage subprocess probe")
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

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

fn assert_success(status: ExitStatus) {
    assert!(status.success(), "subprocess failed with {status}");
}
