#![cfg(unix)]

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ChildProcessGroup {
    child: Child,
    id: i32,
}

impl Drop for ChildProcessGroup {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            unsafe {
                libc::kill(-self.id, libc::SIGKILL);
            }
        }
        let _ = self.child.wait();
    }
}

fn open_pty() -> (File, File) {
    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(result, 0, "openpty: {}", std::io::Error::last_os_error());

    let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
    assert_ne!(flags, -1, "fcntl(F_GETFL)");
    assert_ne!(
        unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        -1,
        "fcntl(F_SETFL)"
    );

    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

fn spawn_in_pty(mut command: Command) -> (File, ChildProcessGroup) {
    let (master, slave) = open_pty();
    let stdin = slave.try_clone().expect("clone PTY slave for stdin");
    let stdout = slave.try_clone().expect("clone PTY slave for stdout");
    command
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn().expect("launch smelt in a PTY");
    let process = ChildProcessGroup {
        id: child.id() as i32,
        child,
    };
    (master, process)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn drain_provider_requests(listener: &TcpListener) -> bool {
    let mut model_request_seen = false;
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => panic!("accept provider request: {error}"),
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set provider read timeout");
        let mut request = [0_u8; 16 * 1024];
        let read = stream.read(&mut request).unwrap_or(0);
        model_request_seen |= request[..read].starts_with(b"POST ");
        let body = r#"{"data":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }
    model_request_seen
}

#[test]
fn interactive_startup_renders_and_queues_prompt_while_mcp_discovery_is_stalled() {
    let home = tempfile::tempdir().expect("temporary home");
    let provider = TcpListener::bind("127.0.0.1:0").expect("bind test provider");
    provider
        .set_nonblocking(true)
        .expect("make test provider nonblocking");
    let config = home.path().join("init.lua");
    std::fs::write(
        &config,
        format!(
            r#"
smelt.settings.autoupgrade = "off"

smelt.provider.register("local", {{
  type = "openai-compatible",
  api_base = "http://{}/v1",
  models = {{ "test-model" }},
}})

smelt.mcp.register("stalled", {{
  type = "local",
  command = {{ "sh", "-c", "sleep 30" }},
  timeout = 30000,
}})
"#,
            provider.local_addr().unwrap()
        ),
    )
    .expect("write init.lua");

    for name in ["state", "cache", "data"] {
        std::fs::create_dir(home.path().join(name)).expect("create XDG directory");
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args([
            "--config",
            config.to_str().unwrap(),
            "--ephemeral",
            "wait for MCP tools",
        ])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();
    let mut rendered_at = None;

    while Instant::now() < deadline {
        drain_pty(&mut master, &mut captured);

        assert!(
            !drain_provider_requests(&provider),
            "initial prompt reached the model before MCP discovery settled:\n{}",
            String::from_utf8_lossy(&captured)
        );
        let alternate_screen = contains(&captured, b"\x1b[?1049h");
        let rendered_model = contains(&captured, b"local/test-model");
        if alternate_screen && rendered_model {
            let first_render = rendered_at.get_or_insert_with(Instant::now);
            if first_render.elapsed() >= Duration::from_secs(1) {
                return;
            }
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before rendering ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    panic!(
        "smelt did not render before stalled MCP discovery timed out:\n{}",
        String::from_utf8_lossy(&captured)
    );
}

#[test]
fn interactive_startup_applies_dynamic_lua_flags_before_the_first_frame() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let config_dir = root.path().join("config/smelt");
    let ready_marker = root.path().join("ready-kind");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    for name in ["home", "state", "cache", "data"] {
        std::fs::create_dir(root.path().join(name)).expect("create XDG directory");
    }
    std::fs::write(
        config_dir.join("early.lua"),
        r#"
smelt.cli.register_flag({
  name = "startup-model",
  kind = "string",
})
"#,
    )
    .expect("write early.lua");
    let ready_path = serde_json::to_string(ready_marker.to_str().unwrap()).unwrap();
    std::fs::write(
        config_dir.join("init.lua"),
        format!(
            r#"
smelt.settings.autoupgrade = "off"
local model = assert(smelt.cli.get("startup-model"))
smelt.provider.register("dynamic", {{
  type = "openai-compatible",
  api_base = "http://127.0.0.1:9/v1",
  models = {{ model }},
}})
smelt.lifecycle.on_ready(function(ctx)
  local ok, err = smelt.fs.write({ready_path}, ctx.kind)
  assert(ok, err)
end)
"#
        ),
    )
    .expect("write init.lua");

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["--startup-model", "selected-model", "--ephemeral"])
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();

    loop {
        drain_pty(&mut master, &mut captured);
        let selected_model_rendered = contains(&captured, b"dynamic/selected-model");
        let ready_kind = std::fs::read_to_string(&ready_marker).ok();
        if selected_model_rendered && ready_kind.as_deref() == Some("launch") {
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before dynamic startup completed ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "dynamic Lua flag was not applied before the first frame:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn resumed_session_loads_project_lua_in_generation_zero() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let home = root.path().join("home");
    let config_home = root.path().join("config");
    let state_home = root.path().join("state");
    let cache_home = root.path().join("cache");
    let data_home = root.path().join("data");
    let initial_cwd = root.path().join("initial");
    let restored_cwd = root.path().join("restored");
    let ready_marker = root.path().join("project-ready-kind");
    for path in [
        &home,
        &config_home,
        &state_home,
        &cache_home,
        &data_home,
        &initial_cwd,
        &restored_cwd,
        &config_home.join("smelt"),
        &restored_cwd.join(".smelt"),
    ] {
        std::fs::create_dir_all(path).expect("create startup runtime directory");
    }

    std::fs::write(
        config_home.join("smelt/init.lua"),
        r#"
smelt.settings.autoupgrade = "off"
smelt.provider.register("local", {
  type = "openai-compatible",
  api_base = "http://127.0.0.1:9/v1",
  models = { "test-model" },
})
"#,
    )
    .expect("write global config");
    let ready_path = serde_json::to_string(ready_marker.to_str().unwrap()).unwrap();
    std::fs::write(
        restored_cwd.join(".smelt/init.lua"),
        format!(
            r#"
smelt.lifecycle.on_ready(function(ctx)
  local previous = smelt.fs.read({ready_path}) or ""
  local ok, err = smelt.fs.write({ready_path}, previous .. ctx.kind .. "\n")
  assert(ok, err)
end)
"#
        ),
    )
    .expect("write restored project config");
    smelt_core::trust::TrustStore::new(state_home.join("smelt"))
        .mark_trusted(&restored_cwd)
        .expect("trust restored project");

    let session_id = "a100000000000000000000000000000000000000000000000000000000000001";
    let transcript_marker = "canonical resume startup marker";
    let mut session = smelt_core::session::Session::new(1, restored_cwd.clone());
    session.id = session_id.to_string();
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            transcript_marker,
        )));
    let sessions_root = state_home.join("smelt/sessions");
    let mut writer = smelt_store::OwnedSessionWriter::open(&sessions_root, session_id)
        .expect("create previous-format fixture");
    writer
        .commit_session(
            &smelt_core::session::initial_store_commit_from_session(&session)
                .expect("build previous-format fixture"),
        )
        .expect("commit previous-format fixture");
    writer.publish().expect("publish previous-format fixture");
    writer.release().expect("release previous-format fixture");

    let migration = Command::new(env!("CARGO_BIN_EXE_smelt"))
        .args(["session", "migrate", session_id, "--json"])
        .current_dir(&initial_cwd)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_DATA_HOME", &data_home)
        .output()
        .expect("migrate startup fixture");
    assert!(
        migration.status.success(),
        "fixture migration failed: {}",
        String::from_utf8_lossy(&migration.stderr)
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["--resume", session_id])
        .current_dir(&initial_cwd)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();

    loop {
        drain_pty(&mut master, &mut captured);
        let session_rendered = contains(&captured, transcript_marker.as_bytes());
        let ready_kind = std::fs::read_to_string(&ready_marker).ok();
        if session_rendered && ready_kind.is_some() {
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before canonical resume completed ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "canonical resume did not finish:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        std::fs::read_to_string(&ready_marker).expect("project ready marker"),
        "launch\n",
        "restored project Lua must load once in generation zero"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/cwd", process.child.id())).expect("process cwd"),
        restored_cwd
    );
}

#[test]
fn interactive_config_error_is_evaluated_once() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let config_dir = root.path().join("config/smelt");
    let evaluation_marker = root.path().join("config-evaluations");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    for name in ["home", "state", "cache", "data"] {
        std::fs::create_dir(root.path().join(name)).expect("create XDG directory");
    }
    let marker_path = serde_json::to_string(evaluation_marker.to_str().unwrap()).unwrap();
    std::fs::write(
        config_dir.join("init.lua"),
        format!(
            r#"
smelt.settings.autoupgrade = "off"
local previous = smelt.fs.read({marker_path}) or ""
local ok, err = smelt.fs.write({marker_path}, previous .. "loaded\n")
assert(ok, err)
error("single-generation-config-error")
"#
        ),
    )
    .expect("write init.lua");

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .arg("--ephemeral")
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();

    loop {
        drain_pty(&mut master, &mut captured);
        if contains(&captured, b"~/.config/smelt/init.lua: runtime error") {
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before rendering the config error ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "smelt did not render the config error:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        std::fs::read_to_string(evaluation_marker).expect("config evaluation marker"),
        "loaded\n",
        "normal config must execute exactly once during interactive launch"
    );
}

#[test]
fn graceful_interactive_exit_runs_shutdown_hook_once() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let config_dir = root.path().join("config/smelt");
    let shutdown_marker = root.path().join("shutdown-calls");
    std::fs::create_dir_all(&config_dir).expect("create config directory");
    for name in ["home", "state", "cache", "data"] {
        std::fs::create_dir(root.path().join(name)).expect("create XDG directory");
    }
    let marker_path = serde_json::to_string(shutdown_marker.to_str().unwrap()).unwrap();
    std::fs::write(
        config_dir.join("init.lua"),
        format!(
            r#"
smelt.settings.autoupgrade = "off"
smelt.lifecycle.on_shutdown(function(ctx)
  local previous = smelt.fs.read({marker_path}) or ""
  local call = tostring(ctx.ephemeral) .. ":" .. tostring(ctx.has_messages) .. "\n"
  local ok, err = smelt.fs.write({marker_path}, previous .. call)
  assert(ok, err)
end)
"#
        ),
    )
    .expect("write init.lua");

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .arg("--ephemeral")
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();

    loop {
        drain_pty(&mut master, &mut captured);
        if contains(&captured, b"\x1b[?1049h") && contains(&captured, b"f1 help") {
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before its first frame ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "smelt did not render before graceful-exit test timeout:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    master.write_all(b"\x03\x03").expect("send Ctrl-C to smelt");
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        drain_pty(&mut master, &mut captured);
        if let Some(status) = process.child.try_wait().expect("inspect smelt shutdown") {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "smelt did not exit gracefully:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(status.success(), "smelt exited with {status}");
    assert!(
        contains(&captured, b"\x1b[?1049l"),
        "smelt did not restore the terminal:\n{}",
        String::from_utf8_lossy(&captured)
    );
    assert_eq!(
        std::fs::read_to_string(shutdown_marker).expect("shutdown marker"),
        "true:false\n",
        "shutdown hook must run exactly once with the final session context"
    );
}

#[derive(Clone, Copy)]
struct LifecycleSample {
    migration: Option<Duration>,
    first_frame: Option<Duration>,
    ready: Duration,
    shutdown: Duration,
    peak_rss_kib: u64,
}

fn drain_pty(master: &mut File, captured: &mut Vec<u8>) {
    const BACKGROUND_QUERY: &[u8] = b"\x1b]11;?\x07\x1b[5n";
    const DARK_BACKGROUND_RESPONSE: &[u8] = b"\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[0n";

    let mut buffer = [0_u8; 64 * 1024];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                let output = &buffer[..read];
                let previous_len = captured.len();
                captured.extend_from_slice(output);
                let query_start =
                    previous_len.saturating_sub(BACKGROUND_QUERY.len().saturating_sub(1));
                // A real terminal answers this probe immediately. Search across
                // read boundaries so the harness cannot accidentally trigger
                // background detection's 100 ms fallback.
                if contains(&captured[query_start..], BACKGROUND_QUERY) {
                    master
                        .write_all(DARK_BACKGROUND_RESPONSE)
                        .expect("answer terminal background query");
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return,
            Err(error) => panic!("read smelt PTY: {error}"),
        }
    }
}

#[cfg(target_os = "linux")]
fn process_rss_kib(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_ascii_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn process_rss_kib(_pid: u32) -> u64 {
    0
}

fn nearest_rank(sorted: &[f64], percentile: usize) -> f64 {
    let rank = sorted.len().saturating_mul(percentile).div_ceil(100).max(1);
    sorted[rank.saturating_sub(1).min(sorted.len().saturating_sub(1))]
}

fn print_sample_summary(name: &str, unit: &str, mut values: Vec<f64>) {
    values.sort_by(f64::total_cmp);
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / values.len() as f64;
    println!(
        "LIFECYCLE_BENCH_SUMMARY metric={name} runs={} mean_{unit}={mean:.3} stddev_{unit}={:.3} p50_{unit}={:.3} p95_{unit}={:.3} p99_{unit}={:.3} max_{unit}={:.3}",
        values.len(),
        variance.sqrt(),
        nearest_rank(&values, 50),
        nearest_rank(&values, 95),
        nearest_rank(&values, 99),
        values.last().copied().unwrap_or_default(),
    );
}

fn print_duration_summary(name: &str, samples: impl Iterator<Item = Duration>) {
    print_sample_summary(
        name,
        "ms",
        samples
            .map(|duration| duration.as_secs_f64() * 1_000.0)
            .collect(),
    );
}

struct LifecycleBenchmark<'a> {
    binary: &'a Path,
    fixture: Option<&'a Path>,
    session_id: Option<&'a str>,
    config: &'a Path,
    first_frame_marker: &'a [u8],
    ready_marker: &'a [u8],
    timeout: Duration,
}

impl LifecycleBenchmark<'_> {
    fn run_sample(&self, root: &Path) -> LifecycleSample {
        let state_home = root.join("state");
        for name in ["home", "config", "cache", "data"] {
            std::fs::create_dir_all(root.join(name)).expect("create benchmark runtime directory");
        }
        let migration = if let (Some(fixture), Some(session_id)) = (self.fixture, self.session_id) {
            let session_dir = state_home.join("smelt/sessions").join(session_id);
            std::fs::create_dir_all(&session_dir).expect("create benchmark session directory");
            smelt_store::backup_session_database(
                fixture.join("session.db"),
                session_dir.join("session.db"),
            )
            .expect("copy benchmark fixture with SQLite backup");
            let started = Instant::now();
            let output = Command::new(self.binary)
                .args(["session", "migrate", session_id, "--json"])
                .current_dir(root)
                .env("HOME", root.join("home"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_STATE_HOME", &state_home)
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env("XDG_DATA_HOME", root.join("data"))
                .output()
                .expect("run explicit benchmark migration");
            assert!(
                output.status.success(),
                "explicit benchmark migration failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Some(started.elapsed())
        } else {
            None
        };

        let mut command = Command::new(self.binary);
        command.args([
            "--config",
            self.config.to_str().expect("UTF-8 benchmark config path"),
            "--bench",
        ]);
        if let Some(session_id) = self.session_id {
            command.args(["--resume", session_id]);
        }
        command
            .current_dir(root)
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("TERM", "xterm-256color")
            .env("NO_COLOR", "1");
        let started = Instant::now();
        let (mut master, mut process) = spawn_in_pty(command);
        let mut captured = Vec::new();
        let mut first_frame = None;
        let mut peak_rss_kib = 0;
        let ready = loop {
            drain_pty(&mut master, &mut captured);
            if first_frame.is_none() && contains(&captured, self.first_frame_marker) {
                first_frame = Some(started.elapsed());
            }
            if contains(&captured, self.ready_marker) {
                break started.elapsed();
            }
            peak_rss_kib = peak_rss_kib.max(process_rss_kib(process.child.id()));
            if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
                panic!(
                    "smelt exited before lifecycle ready marker ({status}):\n{}",
                    String::from_utf8_lossy(&captured)
                );
            }
            assert!(
                started.elapsed() < self.timeout,
                "smelt lifecycle ready marker was not rendered within {:?}:\n{}",
                self.timeout,
                String::from_utf8_lossy(&captured)
            );
            std::thread::sleep(Duration::from_millis(1));
        };

        let settle_until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < settle_until {
            drain_pty(&mut master, &mut captured);
            peak_rss_kib = peak_rss_kib.max(process_rss_kib(process.child.id()));
            assert!(
                process
                    .child
                    .try_wait()
                    .expect("inspect smelt process")
                    .is_none(),
                "smelt exited while lifecycle benchmark was settling"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        let quit_started = Instant::now();
        master.write_all(b"\x03\x03").expect("send Ctrl-C to smelt");
        let status = loop {
            drain_pty(&mut master, &mut captured);
            peak_rss_kib = peak_rss_kib.max(process_rss_kib(process.child.id()));
            if let Some(status) = process.child.try_wait().expect("inspect smelt shutdown") {
                break status;
            }
            assert!(
                quit_started.elapsed() < self.timeout,
                "smelt did not shut down within {:?}",
                self.timeout
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(
            status.success(),
            "smelt lifecycle benchmark exited {status}"
        );
        drain_pty(&mut master, &mut captured);
        if let Some(capture_dir) = std::env::var_os("SMELT_LIFECYCLE_BENCH_CAPTURE_DIR") {
            let capture_dir = std::path::PathBuf::from(capture_dir);
            std::fs::create_dir_all(&capture_dir).expect("create lifecycle capture directory");
            let run = root
                .file_name()
                .and_then(|name| name.to_str())
                .expect("lifecycle run directory requires a UTF-8 name");
            std::fs::write(capture_dir.join(format!("{run}.terminal.bin")), &captured)
                .expect("write lifecycle terminal capture");
        }

        LifecycleSample {
            migration,
            first_frame,
            ready,
            shutdown: quit_started.elapsed(),
            peak_rss_kib,
        }
    }
}

#[test]
fn interactive_lifecycle_benchmark_suite() {
    let Some(target) = std::env::var_os("SMELT_LIFECYCLE_BENCH_TARGET") else {
        return;
    };
    let target = std::path::PathBuf::from(target);
    assert!(target.is_file(), "benchmark target must be a smelt binary");
    let fixture = std::env::var_os("SMELT_LIFECYCLE_BENCH_FIXTURE").map(std::path::PathBuf::from);
    let fixture_database_before = fixture
        .as_ref()
        .map(|fixture| std::fs::read(fixture.join("session.db")).expect("read benchmark fixture"));
    let session_id = fixture.as_ref().map(|fixture| {
        fixture
            .file_name()
            .and_then(|name| name.to_str())
            .expect("fixture directory must have a UTF-8 session id")
    });
    let ready_marker = std::env::var("SMELT_LIFECYCLE_BENCH_READY_TEXT")
        .expect("SMELT_LIFECYCLE_BENCH_READY_TEXT must identify loaded transcript content");
    assert!(
        !ready_marker.is_empty(),
        "SMELT_LIFECYCLE_BENCH_READY_TEXT must not be empty"
    );
    let first_frame_marker = std::env::var("SMELT_LIFECYCLE_BENCH_FIRST_FRAME_TEXT")
        .unwrap_or_else(|_| "local/test-model".to_string());
    assert!(
        !first_frame_marker.is_empty(),
        "SMELT_LIFECYCLE_BENCH_FIRST_FRAME_TEXT must not be empty"
    );
    let runs = std::env::var("SMELT_LIFECYCLE_BENCH_RUNS")
        .ok()
        .and_then(|runs| runs.parse::<usize>().ok())
        .unwrap_or(10)
        .max(1);
    let timeout = Duration::from_secs(
        std::env::var("SMELT_LIFECYCLE_BENCH_TIMEOUT_SECS")
            .ok()
            .and_then(|seconds| seconds.parse::<u64>().ok())
            .unwrap_or(30),
    );

    let root = tempfile::tempdir().expect("create lifecycle benchmark root");
    let config = root.path().join("init.lua");
    std::fs::write(
        &config,
        r#"smelt.settings.autoupgrade = "off"
smelt.provider.register("local", {
  type = "openai-compatible",
  api_base = "http://127.0.0.1:9/v1",
  models = { "test-model" },
})
"#,
    )
    .expect("write lifecycle benchmark config");
    let benchmark = LifecycleBenchmark {
        binary: &target,
        fixture: fixture.as_deref(),
        session_id,
        config: &config,
        first_frame_marker: first_frame_marker.as_bytes(),
        ready_marker: ready_marker.as_bytes(),
        timeout,
    };

    let mut samples = Vec::with_capacity(runs);
    for run in 1..=runs {
        let run_root = root.path().join(format!("run-{run:03}"));
        let sample = benchmark.run_sample(&run_root);
        std::fs::remove_dir_all(run_root).expect("remove lifecycle benchmark run directory");
        let first_frame_ms = sample
            .first_frame
            .map(|duration| format!("{:.3}", duration.as_secs_f64() * 1_000.0))
            .unwrap_or_else(|| "na".to_string());
        let migration_ms = sample
            .migration
            .map(|duration| format!("{:.3}", duration.as_secs_f64() * 1_000.0))
            .unwrap_or_else(|| "na".to_string());
        println!(
            "LIFECYCLE_BENCH_RUN run={run} migration_ms={migration_ms} first_frame_ms={first_frame_ms} ready_ms={:.3} shutdown_ms={:.3} peak_rss_kib={}",
            sample.ready.as_secs_f64() * 1_000.0,
            sample.shutdown.as_secs_f64() * 1_000.0,
            sample.peak_rss_kib,
        );
        samples.push(sample);
    }

    let migrations = samples.iter().filter_map(|sample| sample.migration);
    if migrations.clone().next().is_some() {
        print_duration_summary("migration", migrations);
    }
    let first_frames = samples.iter().filter_map(|sample| sample.first_frame);
    if first_frames.clone().next().is_some() {
        print_duration_summary("first_frame", first_frames);
    }
    print_duration_summary("ready", samples.iter().map(|sample| sample.ready));
    print_duration_summary("shutdown", samples.iter().map(|sample| sample.shutdown));
    print_sample_summary(
        "peak_rss",
        "kib",
        samples
            .iter()
            .map(|sample| sample.peak_rss_kib as f64)
            .collect(),
    );
    if let (Some(fixture), Some(before)) = (&fixture, fixture_database_before) {
        assert_eq!(
            std::fs::read(fixture.join("session.db")).expect("reread benchmark fixture"),
            before,
            "lifecycle benchmark must not mutate its source fixture"
        );
    }
}
