//! Real CLI processes against isolated installations and a local HTTPS proxy.
//! No test contacts GitHub or replaces an installed executable.
#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;

struct Installation {
    _dir: tempfile::TempDir,
    exe: PathBuf,
}

impl Installation {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("smelt");
        fs::copy(env!("CARGO_BIN_EXE_smelt"), &exe).unwrap();
        Self { _dir: dir, exe }
    }

    async fn cli(&self, args: &[&str]) -> Value {
        cli(&self.exe, args, None).await
    }
}

async fn cli(exe: &Path, args: &[&str], proxy: Option<&str>) -> Value {
    let mut command = Command::new(exe);
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    if let Some(proxy) = proxy {
        command
            .env("HTTPS_PROXY", proxy)
            .env("https_proxy", proxy)
            .env("NO_PROXY", "")
            .env("no_proxy", "");
    }
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("CLI deadline")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON report")
}

struct Worker(u32);

impl Worker {
    fn kill(&mut self) {
        if self.0 != 0 {
            // This PID comes from the worker this test started in its private installation.
            assert_eq!(unsafe { libc::kill(self.0 as i32, libc::SIGKILL) }, 0);
            self.0 = 0;
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                libc::kill(self.0 as i32, libc::SIGKILL);
            }
        }
    }
}

async fn start(install: &Installation) -> (Worker, Value, TcpStream) {
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", proxy.local_addr().unwrap());
    let report = cli(
        &install.exe,
        &["upgrade", "--background", "--json"],
        Some(&url),
    )
    .await;
    let worker = Worker(report["status"]["worker_pid"].as_u64().unwrap() as u32);
    let (stream, _) = tokio::time::timeout(Duration::from_secs(10), proxy.accept())
        .await
        .expect("worker connects to local proxy")
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    loop {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .expect("proxy request deadline")
            .unwrap();
        assert!(!line.is_empty(), "worker closed proxy connection");
        if line == "\r\n" {
            break;
        }
        request.push_str(&line);
    }
    assert!(request.starts_with("CONNECT github.com:443"), "{request}");
    // Keep the response pending so the worker remains in a deterministic stage.
    (worker, report, reader.into_inner())
}

async fn wait_for_unlock(exe: &Path) {
    let lock_path = exe.with_file_name(".smelt-upgrade.lock");
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock_path)
                .unwrap();
            file.lock().unwrap();
        }),
    )
    .await
    .expect("killed worker releases OS lock")
    .unwrap();
}

#[tokio::test]
async fn standalone_check_does_not_publish_status_or_block_installer() {
    let install = Installation::new();
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", proxy.local_addr().unwrap());
    let check = Command::new(&install.exe)
        .args(["upgrade", "check", "--json"])
        .env("HTTPS_PROXY", &url)
        .env("https_proxy", &url)
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (connection, _) = tokio::time::timeout(Duration::from_secs(10), proxy.accept())
        .await
        .expect("check reaches local proxy")
        .unwrap();
    let mut reader = BufReader::new(connection);
    loop {
        let mut line = String::new();
        let bytes = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes > 0);
        if line == "\r\n" {
            break;
        }
    }
    assert!(!install.exe.with_file_name(".smelt-upgrade.lock").exists());
    assert!(!install
        .exe
        .with_file_name(".smelt-upgrade.status.json")
        .exists());
    assert!(install.cli(&["upgrade", "status", "--json"]).await["status"].is_null());

    // The check is still pending, but another session can start an installer.
    let (mut worker, started, _worker_connection) = start(&install).await;
    let status_path = install.exe.with_file_name(".smelt-upgrade.status.json");
    let before = fs::read(&status_path).unwrap();
    reader
        .get_mut()
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(10), check.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !output.status.success(),
        "local proxy rejects the standalone check"
    );
    assert_eq!(
        fs::read(&status_path).unwrap(),
        before,
        "check failure must not overwrite installer status"
    );
    let status = install.cli(&["upgrade", "status", "--json"]).await;
    assert_eq!(
        status["status"]["operation_id"],
        started["status"]["operation_id"]
    );
    assert_eq!(status["status"]["running"], true);
    worker.kill();
    wait_for_unlock(&install.exe).await;
}

#[tokio::test]
async fn killing_initiating_process_group_does_not_kill_worker() {
    let install = Installation::new();
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", proxy.local_addr().unwrap());
    let mut parent = Command::new("sh");
    parent
        .args([
            "-c",
            "\"$1\" upgrade --background --json; read hold",
            "upgrade-parent",
        ])
        .arg(&install.exe)
        .env("HTTPS_PROXY", &url)
        .env("https_proxy", &url)
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut parent = parent.spawn().unwrap();
    let group = parent.id().unwrap() as i32;
    let mut output = BufReader::new(parent.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(15), output.read_line(&mut line))
        .await
        .expect("parent receives worker acknowledgement")
        .unwrap();
    let report: Value = serde_json::from_str(&line).unwrap();
    let mut worker = Worker(report["status"]["worker_pid"].as_u64().unwrap() as u32);
    let (_connection, _) = tokio::time::timeout(Duration::from_secs(10), proxy.accept())
        .await
        .expect("worker reaches proxy")
        .unwrap();
    // Kill exactly the fresh process group this test created, like UI task cancellation.
    assert_eq!(unsafe { libc::kill(-group, libc::SIGKILL) }, 0);
    parent.wait().await.unwrap();
    let status = install.cli(&["upgrade", "status", "--json"]).await;
    assert_eq!(status["status"]["running"], true);
    assert_eq!(status["status"]["worker_pid"], worker.0);
    worker.kill();
    wait_for_unlock(&install.exe).await;
}

#[tokio::test]
async fn sessions_follow_detached_worker_without_network_or_duplicate_installs() {
    let install = Installation::new();
    assert!(install.cli(&["upgrade", "status", "--json"]).await["status"].is_null());
    let (mut worker, started, _connection) = start(&install).await;
    assert_eq!(
        started.as_object().unwrap().len(),
        2,
        "startup returns only current identity and status"
    );
    assert!(
        started["status"]["target"].is_null(),
        "checking has no target yet"
    );

    // The initiating CLI has already exited. The detached installer still owns the lock.
    let status = install.cli(&["upgrade", "status", "--json"]).await;
    assert_eq!(status["status"]["running"], true);
    assert_eq!(
        status["status"]["operation_id"],
        started["status"]["operation_id"]
    );

    for args in [
        vec!["upgrade", "--json"],
        vec!["upgrade", "--background", "--json", "--channel", "unstable"],
    ] {
        let attached = install.cli(&args).await;
        assert_eq!(
            attached["status"]["operation_id"],
            started["status"]["operation_id"]
        );
        assert_eq!(
            attached["status"]["channel"], "stable",
            "report actual active channel"
        );
        assert_eq!(attached["status"]["worker_pid"], worker.0);
    }
    worker.kill();
    wait_for_unlock(&install.exe).await;
    let interrupted = install.cli(&["upgrade", "status", "--json"]).await;
    assert_eq!(interrupted["status"]["running"], false);
    assert_eq!(interrupted["status"]["stage"], "interrupted");

    // Both files still exist. Neither prevents a new operation taking ownership.
    let (mut replacement, restarted, _connection) = start(&install).await;
    assert_ne!(
        restarted["status"]["operation_id"],
        started["status"]["operation_id"]
    );
    assert_eq!(restarted["status"]["stage"], "checking");
    replacement.kill();
    wait_for_unlock(&install.exe).await;
}
