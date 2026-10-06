#![cfg(unix)]

use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
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

fn consume_http_request(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set request read timeout");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut content_length = None;
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).expect("read request header"), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse::<u64>().expect("request content length"));
            }
        }
    }
    let content_length = content_length.expect("POST request has content length");
    let consumed = std::io::copy(&mut reader.take(content_length), &mut std::io::sink())
        .expect("consume request body");
    assert_eq!(consumed, content_length, "truncated request body");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_reload_refreshes_server_context_window() {
    interactive_reload_context_window("openai-compatible").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_reload_refreshes_compatible_server_context_window() {
    interactive_reload_context_window("anthropic-compatible").await;
}

async fn interactive_reload_context_window(provider_type: &str) {
    use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let home = tempfile::tempdir().expect("temporary home");
    let provider = MockServer::start().await;
    let context_window = Arc::new(AtomicU32::new(32_768));
    let response_status = Arc::new(AtomicU16::new(200));
    let primary_status = Arc::new(AtomicU16::new(404));
    let server_primary_status = Arc::clone(&primary_status);
    Mock::given(method("GET"))
        .and(path("/v1/models/test-model"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(server_primary_status.load(Ordering::SeqCst))
        })
        .mount(&provider)
        .await;
    let server_context_window = Arc::clone(&context_window);
    let server_response_status = Arc::clone(&response_status);
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(server_response_status.load(Ordering::SeqCst)).set_body_json(
                serde_json::json!({
                    "data": [{
                        "id": "test-model",
                        "max_model_len": server_context_window.load(Ordering::SeqCst),
                    }],
                }),
            )
        })
        .mount(&provider)
        .await;

    let config = home.path().join("init.lua");
    std::fs::write(
        &config,
        format!(
            r#"
smelt.settings.autoupgrade = "off"
smelt.settings.auto_reload = false
smelt.settings.show_prediction = false
smelt.provider.register("local", {{
  type = "{provider_type}",
  api_base = "{}/v1",
  models = {{ "test-model" }},
}})
smelt.cmd.register("context-probe", function()
  local file = assert(io.open("context-window", "w"))
  file:write(smelt.json.encode({{
    window = smelt.session.context_window(),
    session_id = smelt.session.id(),
    controller = smelt.config.runtime_status().controllers.context_window,
  }}))
  file:close()
end)
"#,
            provider.uri()
        ),
    )
    .expect("write init.lua");
    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["--config", config.to_str().unwrap(), "--ephemeral"])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let mut captured = Vec::new();
    let mut session_id = None;
    let mut observed_revision = 0;

    for (index, (primary, status, expected)) in [
        (404, 200, 32_768),
        (404, 200, 131_072),
        (404, 503, 131_072),
        (503, 200, 16_384),
    ]
    .into_iter()
    .enumerate()
    {
        primary_status.store(primary, Ordering::SeqCst);
        response_status.store(status, Ordering::SeqCst);
        context_window.store(expected, Ordering::SeqCst);
        if index > 0 {
            master.write_all(b"/reload\r").expect("reload in place");
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last_probe = None;
        loop {
            drain_pty(&mut master, &mut captured);
            assert!(process.child.try_wait().unwrap().is_none(), "smelt exited");
            let probe = std::fs::read(home.path().join("context-window"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            if let Some(probe) = probe {
                let controller = &probe["controller"];
                if let Some(revision) = controller["observed_revision"].as_u64() {
                    if probe["window"] == expected
                        && revision > observed_revision
                        && controller["desired_revision"] == revision
                    {
                        let id = probe["session_id"].as_str().expect("session id");
                        assert_eq!(session_id.get_or_insert_with(|| id.to_owned()), id);
                        if status == 200 {
                            assert_eq!(controller["status"], "ready");
                            assert!(controller["error"].is_null());
                        } else {
                            assert_eq!(controller["status"], "degraded");
                            assert!(controller["error"].as_str().unwrap().contains("503"));
                        }
                        if index > 0 {
                            assert_eq!(revision, observed_revision + 1);
                        }
                        observed_revision = revision;
                        break;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "context window did not become {expected} after {index} reloads; probe: {:?}; requests: {}; terminal:\n{}",
                std::fs::read_to_string(home.path().join("context-window")),
                provider.received_requests().await.unwrap().len(),
                String::from_utf8_lossy(&captured[captured.len().saturating_sub(4096)..])
            );
            if contains(&captured, b"local/test-model")
                && last_probe
                    .is_none_or(|last: Instant| last.elapsed() >= Duration::from_millis(100))
            {
                master
                    .write_all(b"/context-probe\r")
                    .expect("read live context window");
                last_probe = Some(Instant::now());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let requests = provider.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/v1/models")
                .count(),
            index + 1,
            "each reload should fetch the context window exactly once"
        );
        let requests_per_refresh = if provider_type == "anthropic-compatible" {
            2
        } else {
            1
        };
        assert_eq!(requests.len(), (index + 1) * requests_per_refresh);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_first_message_rewind_can_resubmit() {
    interactive_first_message_recovery(FirstMessageRecovery::Rewind).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_first_message_rewind_before_dispatch_can_resubmit() {
    interactive_first_message_recovery(FirstMessageRecovery::RewindBeforeDispatch).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_first_response_cancel_can_resubmit() {
    interactive_first_message_recovery(FirstMessageRecovery::CancelStreaming).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_completed_response_can_submit_follow_up() {
    interactive_first_message_recovery(FirstMessageRecovery::Complete).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_rewind_preserves_paused_queue_until_submission() {
    interactive_first_message_recovery(FirstMessageRecovery::RewindPausedQueue).await;
}

#[derive(Clone, Copy, Debug)]
enum FirstMessageRecovery {
    Rewind,
    RewindBeforeDispatch,
    CancelStreaming,
    Complete,
    RewindPausedQueue,
}

async fn interactive_first_message_recovery(recovery: FirstMessageRecovery) {
    for vim in [false, true] {
        interactive_first_message_recovery_in_mode(recovery, vim).await;
    }
}

async fn interactive_first_message_recovery_in_mode(recovery: FirstMessageRecovery, vim: bool) {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let paused_queue = matches!(recovery, FirstMessageRecovery::RewindPausedQueue);
    let before_dispatch = matches!(recovery, FirstMessageRecovery::RewindBeforeDispatch);
    let complete = matches!(recovery, FirstMessageRecovery::Complete);
    let streaming = matches!(recovery, FirstMessageRecovery::CancelStreaming) || complete;
    let home = tempfile::tempdir().expect("temporary home");
    let provider = MockServer::start().await;
    let stream_provider = TcpListener::bind("127.0.0.1:0").unwrap();
    stream_provider.set_nonblocking(true).unwrap();
    let mut streams = Vec::new();
    let response = if complete {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Starting\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
    } else if streaming {
        ResponseTemplate::new(307).insert_header(
            "location",
            format!("http://{}/", stream_provider.local_addr().unwrap()),
        )
    } else if paused_queue {
        ResponseTemplate::new(429)
            .insert_header("retry-after", "3600")
            .set_body_json(serde_json::json!({"error": {"code": "insufficient_quota"}}))
            .set_delay(Duration::from_secs(1))
    } else {
        ResponseTemplate::new(200).set_delay(Duration::from_secs(30))
    };
    Mock::given(method("POST"))
        .respond_with(response)
        .mount(&provider)
        .await;
    let config = home.path().join("init.lua");
    std::fs::write(
        &config,
        format!(
            r#"
smelt.settings.autoupgrade = "off"
smelt.settings.auto_continue = "off"
smelt.settings.auto_reload = false
smelt.settings.show_prediction = false
smelt.settings.vim = {vim}
local submitted, cleared = false, false
smelt.events.on("input_submit", function() submitted = true end)
smelt.prompt.win():on("text_changed", function()
  if submitted and smelt.prompt.text() == "" then cleared = true end
  if cleared and not smelt.engine.is_running() and smelt.prompt.text() == "first request" then
    local file = assert(io.open("prompt-restored", "w"))
    file:write("ready")
    file:close()
  end
end)
smelt.events.on("stream_delta", function()
  local file = assert(io.open("output-started", "w"))
  file:write("ready")
  file:close()
end)
smelt.events.on("turn_end", function(ev)
  if not ev.error_kind then
    local file = assert(io.open("turn-ended", "w"))
    file:write("ready")
    file:close()
  end
  if ev.error_kind == "quota" then
    local file = assert(io.open("quota-paused", "w"))
    file:write("ready")
    file:close()
  end
end)
smelt.cmd.register("rewind-first", function()
  smelt.session.rewind_to(smelt.session.turns()[1].history_idx)
end)
smelt.provider.register("local", {{
  type = "openai-compatible",
  api_base = "{}",
  models = {{ "test-model" }},
}})
"#,
            provider.uri()
        ),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["--config", config.to_str().unwrap()])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut captured = Vec::new();
    let mut submitted = false;
    let mut queued = false;
    let mut rewind_sent = false;
    let mut restored_at = None;
    let mut resubmitted = false;
    loop {
        drain_pty(&mut master, &mut captured);
        assert!(process.child.try_wait().unwrap().is_none(), "smelt exited");
        while let Ok((mut stream, _)) = stream_provider.accept() {
            consume_http_request(&mut stream);
            let chunk =
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Starting\"}}]}\n\n";
            write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n", chunk.len()).unwrap();
            // Leave the response open so Escape interrupts a live stream.
            streams.push(stream);
        }
        if !submitted && contains(&captured, b"local/test-model") {
            if before_dispatch {
                master.write_all(b"first request\r\x1b\x1b").unwrap();
                rewind_sent = true;
            } else {
                master.write_all(b"first request\r").unwrap();
            }
            submitted = true;
        }
        let requests = provider.received_requests().await.unwrap();
        let bodies: Vec<serde_json::Value> = requests
            .iter()
            .filter_map(|request| request.body_json().ok())
            .filter(|body: &serde_json::Value| {
                body["tools"]
                    .as_array()
                    .is_some_and(|tools| !tools.is_empty())
            })
            .collect();
        assert!(
            resubmitted || bodies.len() <= 1,
            "unexpected request before resubmission (rewind_sent={rewind_sent}, output_started={}, interrupted={}, requests={}): {}",
            home.path().join("output-started").exists(),
            home.path().join("turn-ended").exists(),
            bodies.len(),
            String::from_utf8_lossy(&captured[captured.len().saturating_sub(5000)..])
        );
        if bodies.len() == 1 && !rewind_sent {
            if complete {
                rewind_sent = home.path().join("turn-ended").exists();
            } else if !paused_queue {
                if !streaming || home.path().join("output-started").exists() {
                    master.write_all(b"\x1b\x1b").unwrap();
                    rewind_sent = true;
                }
            } else if !queued {
                master.write_all(b"queued follow-up\r").unwrap();
                queued = true;
            } else if home.path().join("quota-paused").exists() {
                master.write_all(b"/rewind-first\r").unwrap();
                rewind_sent = true;
            }
        }
        if !resubmitted
            && rewind_sent
            && home
                .path()
                .join(if streaming {
                    "turn-ended"
                } else {
                    "prompt-restored"
                })
                .exists()
        {
            let restored = restored_at.get_or_insert_with(Instant::now);
            // Observe an idle interval to catch unsolicited dispatch before explicitly submitting.
            if !paused_queue || restored.elapsed() >= Duration::from_secs(1) {
                if streaming {
                    master.write_all(b"first request").unwrap();
                }
                master.write_all(b" edited\r").unwrap();
                resubmitted = true;
            }
        }
        if resubmitted
            && bodies.last().is_some_and(|body| {
                body["messages"]
                    .to_string()
                    .contains("first request edited")
            })
        {
            assert!(bodies.len() <= 2);
            if !before_dispatch {
                assert_eq!(bodies.len(), 2);
            }
            let messages = &bodies.last().unwrap()["messages"];
            assert!(!messages.to_string().contains("queued follow-up"));
            assert_eq!(
                messages.to_string().matches("first request").count(),
                if streaming { 2 } else { 1 }
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "message recovery failed ({recovery:?}, vim={vim}, requests: {}, rewind_sent={rewind_sent}, resubmitted={resubmitted}, last_has_edited={}, last_has_queued={}, output_started={}, turn_ended={}): {}",
            bodies.len(),
            bodies.last().is_some_and(|body| body["messages"].to_string().contains("first request edited")),
            bodies.last().is_some_and(|body| body["messages"].to_string().contains("queued follow-up")),
            home.path().join("output-started").exists(),
            home.path().join("turn-ended").exists(),
            String::from_utf8_lossy(&captured[captured.len().saturating_sub(5000)..])
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_error_does_not_dispatch_queued_input() {
    interactive_quota_pause(false, QuotaRecovery::Wait).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_background_completion_does_not_bypass_quota_pause() {
    interactive_quota_pause(true, QuotaRecovery::Wait).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_reset_resumes_original_work_before_queued_input() {
    interactive_quota_pause(true, QuotaRecovery::Resume).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_retry_detects_early_reset_without_consuming_queued_input() {
    interactive_quota_pause(true, QuotaRecovery::EarlyResume).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_double_escape_cancels_quota_retry_without_losing_queued_input() {
    interactive_quota_pause(true, QuotaRecovery::Cancel).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_recovery_preserves_command_overrides() {
    interactive_quota_pause(true, QuotaRecovery::CommandResume).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_recovery_cancels_foreground_work() {
    interactive_quota_pause(true, QuotaRecovery::CancelBusy).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_recovery_retains_deadline_without_new_metadata() {
    interactive_quota_pause(true, QuotaRecovery::MissingReset).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_quota_recovery_can_be_cancelled_from_turn_end_hook() {
    interactive_quota_pause(true, QuotaRecovery::CancelHook).await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuotaRecovery {
    Wait,
    Resume,
    EarlyResume,
    CommandResume,
    MissingReset,
    Cancel,
    CancelBusy,
    CancelHook,
}

async fn interactive_quota_pause(background: bool, recovery: QuotaRecovery) {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn main_request(request: &Request) -> Option<serde_json::Value> {
        request
            .body_json::<serde_json::Value>()
            .ok()
            .filter(|body| {
                body["tools"]
                    .as_array()
                    .is_some_and(|tools| !tools.is_empty())
            })
    }

    let home = tempfile::tempdir().expect("temporary home");
    let provider = MockServer::start().await;
    let attempts = AtomicUsize::new(0);
    let resume = matches!(
        recovery,
        QuotaRecovery::Resume
            | QuotaRecovery::EarlyResume
            | QuotaRecovery::CommandResume
            | QuotaRecovery::MissingReset
    );
    let short_wait = matches!(
        recovery,
        QuotaRecovery::Resume
            | QuotaRecovery::CommandResume
            | QuotaRecovery::Cancel
            | QuotaRecovery::CancelBusy
    );
    let command_resume = recovery == QuotaRecovery::CommandResume;
    let cancel_hook = recovery == QuotaRecovery::CancelHook;
    let quota_attempts = if recovery == QuotaRecovery::MissingReset {
        2
    } else {
        1
    };
    let retry_after = if recovery == QuotaRecovery::MissingReset {
        "90"
    } else if short_wait {
        "2"
    } else {
        "3600"
    };
    let resumed_at = Arc::new(AtomicU64::new(0));
    let resumed_at_response = Arc::clone(&resumed_at);
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let main = main_request(request).is_some();
            let attempt = if main {
                attempts.fetch_add(1, Ordering::SeqCst)
            } else {
                0
            };
            if !resume || (main && attempt < quota_attempts) {
                let response = ResponseTemplate::new(429)
                    .set_body_json(serde_json::json!({"error": {"code": "insufficient_quota"}}))
                    .set_delay(Duration::from_secs(1));
                return if recovery == QuotaRecovery::MissingReset && main && attempt == 1 {
                    response
                } else {
                    response.insert_header("retry-after", retry_after)
                };
            }
            if main && attempt == quota_attempts {
                resumed_at_response.store(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64,
                    Ordering::SeqCst,
                );
            }
            let content = if main {
                "completed requested work"
            } else {
                "test session"
            };
            let chunk = serde_json::json!({
                "id": "chatcmpl-quota", "object": "chat.completion.chunk",
                "choices": [{ "index": 0, "delta": { "role": "assistant", "content": content } }],
            });
            let finish = serde_json::json!({
                "id": "chatcmpl-quota", "object": "chat.completion.chunk",
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            });
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!(
                    "data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"
                ))
                .set_delay(Duration::from_secs(1))
        })
        .mount(&provider)
        .await;
    let config = home.path().join("init.lua");
    std::fs::write(
        &config,
        format!(
            r#"
smelt.settings.autoupgrade = "off"
-- Runtime edits must not reset the quota fixture's in-flight callback state.
smelt.settings.auto_reload = false
smelt.settings.auto_continue = "always"
local function record_busy_state()
  local file = assert(io.open("foreground-busy", "w"))
  file:write(tostring(smelt.work.is_busy()))
  file:close()
end
local started_background = false
smelt.events.on("turn_end", function(ev)
  if {resume} and not ev.cancelled then smelt.settings.auto_continue = "off" end
  if ev.error_kind == "quota" and ev.retry_at_ms then
    local file = assert(io.open("quota-deadline", "w"))
    file:write(tostring(ev.retry_at_ms))
    file:close()
  end
  if {background} and ev.error_kind == "quota" and not started_background then
    started_background = true
    smelt.spawn(function() smelt.process.spawn_bg("sleep 0.1") end)
  end
  if {cancel_hook} and ev.error_kind == "quota" then
    _G.quota_busy = smelt.work.busy("foreground quota check")
    smelt.engine.cancel()
    record_busy_state()
    local file = assert(io.open("hook-pause-kind", "w"))
    file:write(tostring(smelt.engine.continuation_state().error_kind))
    file:close()
  end
end)
smelt.provider.register("local", {{
  type = "openai-compatible",
  api_base = "{}",
  models = {{ "test-model", "command-model" }},
}})
smelt.lifecycle.on_ready(function() smelt.model.set("local/test-model") end)
smelt.cmd.register("quota-command", function()
  smelt.engine.submit_command("quota-command", "original request", {{
    model = "local/command-model", temperature = 0.3, tools = {{ deny = {{ "bash" }} }},
  }})
end)
smelt.cmd.register("quota-busy", function()
  _G.quota_busy = smelt.work.busy("foreground quota check")
end)
smelt.signal.subscribe("work_busy", record_busy_state)
"#,
            provider.uri()
        ),
    )
    .expect("write init.lua");
    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["--config", config.to_str().unwrap(), "--ephemeral"])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    if !command_resume {
        command.arg("original request");
    }
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now()
        + Duration::from_secs(match recovery {
            QuotaRecovery::EarlyResume => 90,
            QuotaRecovery::MissingReset => 120,
            _ => 30,
        });
    let mut captured = Vec::new();
    let mut command_submitted = false;
    let mut busy_submitted = false;
    let mut queued_at = None;
    let mut cancelled_at = None;
    let mut follow_up_at = None;
    loop {
        drain_pty(&mut master, &mut captured);
        assert!(process.child.try_wait().unwrap().is_none(), "smelt exited");
        let requests = provider.received_requests().await.unwrap();
        let bodies: Vec<_> = requests.iter().filter_map(main_request).collect();
        let posts = bodies.len();
        if command_resume && !command_submitted && contains(&captured, b"local/test-model") {
            master
                .write_all(b"/quota-command\r")
                .expect("submit scoped command");
            command_submitted = true;
        }
        assert!(
            posts <= if resume { quota_attempts + 2 } else { 1 },
            "unexpected automatic turn after quota error"
        );
        if posts == 1 && queued_at.is_none() {
            master
                .write_all(b"queued follow-up\r")
                .expect("queue a user message");
            queued_at = Some(Instant::now());
        }
        if recovery == QuotaRecovery::CancelBusy
            && !busy_submitted
            && contains(&captured, b"resuming at")
        {
            master
                .write_all(b"/quota-busy\r")
                .expect("start foreground work while paused");
            busy_submitted = true;
        }
        if matches!(recovery, QuotaRecovery::Cancel | QuotaRecovery::CancelBusy)
            && cancelled_at.is_none()
            && contains(&captured, b"resuming at")
            && (recovery != QuotaRecovery::CancelBusy
                || std::fs::read_to_string(home.path().join("foreground-busy"))
                    .is_ok_and(|value| value == "true"))
        {
            master
                .write_all(b"\x1b\x1b")
                .expect("cancel scheduled retry");
            cancelled_at = Some(Instant::now());
        }
        if cancel_hook && cancelled_at.is_none() {
            if let Ok(kind) = std::fs::read_to_string(home.path().join("hook-pause-kind")) {
                assert_eq!(
                    kind, "quota",
                    "cancelling from turn_end finished the turn twice"
                );
                cancelled_at = Some(Instant::now());
            }
        }
        if command_resume {
            for body in bodies.iter().take(quota_attempts + 1) {
                assert_eq!(
                    body["model"], "command-model",
                    "retry lost command model override"
                );
                assert_eq!(
                    body["temperature"], 0.3,
                    "retry lost command sampling override"
                );
            }
        }
        if resume && posts > quota_attempts {
            let reset: u64 = std::fs::read_to_string(home.path().join("quota-deadline"))
                .expect("quota error published a reset deadline")
                .parse()
                .unwrap();
            let resumed_at = resumed_at.load(Ordering::SeqCst);
            if recovery == QuotaRecovery::EarlyResume {
                assert!(resumed_at < reset, "missed the early reset");
                assert!(
                    resumed_at >= reset.saturating_sub(3_540_000),
                    "retried before the one-minute backoff"
                );
            } else {
                assert!(resumed_at >= reset, "retried before the provider reset");
            }
            let messages = bodies[quota_attempts]["messages"].to_string();
            assert_eq!(messages.matches("original request").count(), 1);
            assert_eq!(
                messages.matches("finished successfully").count(),
                1,
                "background result must reach the resumed request exactly once"
            );
            assert!(
                !messages.contains("queued follow-up"),
                "retry consumed the next turn"
            );
        }
        if resume && posts == quota_attempts + 2 {
            let messages = bodies[quota_attempts + 1]["messages"].to_string();
            assert_eq!(messages.matches("queued follow-up").count(), 1);
            assert!(
                messages.contains("completed requested work"),
                "queue ran before resumed work finished"
            );
            follow_up_at.get_or_insert_with(Instant::now);
        }
        let settled = match recovery {
            QuotaRecovery::Resume
            | QuotaRecovery::EarlyResume
            | QuotaRecovery::CommandResume
            | QuotaRecovery::MissingReset => {
                follow_up_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(3))
            }
            QuotaRecovery::Cancel | QuotaRecovery::CancelBusy | QuotaRecovery::CancelHook => {
                cancelled_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(4))
            }
            QuotaRecovery::Wait => {
                queued_at.is_some_and(|at| at.elapsed() >= Duration::from_secs(3))
            }
        };
        if settled && (!background || contains(&captured, b"finished successfully")) {
            assert!(
                contains(&captured, b"queued follow-up"),
                "queued input was not rendered"
            );
            assert!(
                contains(&captured, b"quota exceeded")
                    && contains(
                        &captured,
                        if cancel_hook {
                            b"paused"
                        } else {
                            b"resuming at"
                        }
                    ),
                "quiet quota status was not rendered"
            );
            if matches!(
                recovery,
                QuotaRecovery::CancelBusy | QuotaRecovery::CancelHook
            ) {
                assert_eq!(
                    std::fs::read_to_string(home.path().join("foreground-busy")).unwrap(),
                    "false",
                    "cancel left foreground work busy"
                );
            }
            if matches!(recovery, QuotaRecovery::Cancel | QuotaRecovery::CancelBusy) {
                assert!(
                    contains(&captured, b"paused"),
                    "cancelled wait did not render as paused: {}",
                    String::from_utf8_lossy(&captured[captured.len().saturating_sub(3500)..])
                );
            }
            return;
        }
        assert!(Instant::now() < deadline,
            "quota pause did not settle (main requests: {posts}, background: {background}, resume: {resume}): {}",
            String::from_utf8_lossy(&captured[captured.len().saturating_sub(2000)..]));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn interactive_startup_reuses_completed_session_catalog() {
    let root = tempfile::tempdir().expect("temporary runtime root");
    let config_dir = root.path().join("config/smelt");
    let state_home = root.path().join("state");
    for path in [
        root.path().join("home"),
        state_home.clone(),
        root.path().join("cache"),
        root.path().join("data"),
        config_dir.clone(),
    ] {
        std::fs::create_dir_all(path).expect("create startup runtime directory");
    }
    let config = config_dir.join("init.lua");
    std::fs::write(
        &config,
        r#"
smelt.settings.autoupgrade = "off"
smelt.provider.register("local", {
  type = "openai-compatible",
  api_base = "http://127.0.0.1:9/v1",
  models = { "test-model" },
})
"#,
    )
    .expect("write init.lua");

    let catalog_path = state_home.join("smelt/sessions/catalog.db");
    let mut catalog = smelt_store::Catalog::open(&catalog_path).expect("create session catalog");
    let scan_id = catalog.allocate_scan().expect("allocate catalog scan");
    catalog
        .complete_scan(scan_id, 1_700_000_000_000)
        .expect("complete catalog scan");
    drop(catalog);

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["--config", config.to_str().unwrap(), "--ephemeral"])
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();

    loop {
        drain_pty(&mut master, &mut captured);
        if contains(&captured, b"\x1b[?1049h") && contains(&captured, b"local/test-model") {
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect smelt process") {
            panic!(
                "smelt exited before rendering ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "smelt did not render before catalog startup timeout:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let stable_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < stable_deadline {
        drain_pty(&mut master, &mut captured);
        assert!(
            process
                .child
                .try_wait()
                .expect("inspect smelt process")
                .is_none(),
            "smelt exited while observing catalog stability:\n{}",
            String::from_utf8_lossy(&captured)
        );
        let metadata = smelt_store::CatalogReader::open_existing(&catalog_path)
            .expect("open session catalog during startup")
            .expect("session catalog exists during startup")
            .metadata()
            .expect("read session catalog metadata during startup");
        assert_eq!(
            metadata.completed_scan_id, scan_id,
            "interactive startup unexpectedly completed a catalog scan"
        );
        assert_eq!(
            metadata.next_scan_id,
            scan_id + 1,
            "interactive startup unexpectedly allocated a catalog scan"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    master.write_all(b"\x03\x03").expect("send Ctrl-C to smelt");
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        drain_pty(&mut master, &mut captured);
        if let Some(status) = process.child.try_wait().expect("inspect smelt shutdown") {
            assert!(status.success(), "smelt exited with {status}");
            break;
        }
        assert!(
            Instant::now() < exit_deadline,
            "smelt did not exit after catalog startup test:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let metadata = smelt_store::CatalogReader::open_existing(catalog_path)
        .expect("open session catalog")
        .expect("session catalog exists")
        .metadata()
        .expect("read session catalog metadata");
    assert_eq!(metadata.completed_scan_id, scan_id);
    assert_eq!(metadata.next_scan_id, scan_id + 1);
}

#[test]
fn inspect_startup_explicitly_reconciles_the_session_catalog() {
    let root = tempfile::tempdir().expect("temporary inspect root");
    let state_home = root.path().join("state");
    for path in [
        root.path().join("home"),
        root.path().join("config"),
        state_home.clone(),
        root.path().join("cache"),
        root.path().join("data"),
    ] {
        std::fs::create_dir_all(path).expect("create inspect runtime directory");
    }
    let catalog_path = state_home.join("smelt/sessions/catalog.db");
    let mut catalog = smelt_store::Catalog::open(&catalog_path).expect("create session catalog");
    let scan_id = catalog.allocate_scan().expect("allocate catalog scan");
    catalog
        .complete_scan(scan_id, 1_700_000_000_000)
        .expect("complete catalog scan");
    drop(catalog);

    let mut command = Command::new(env!("CARGO_BIN_EXE_smelt"));
    command
        .args(["inspect", "--no-open"])
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1");
    let reader = smelt_store::CatalogReader::open_existing(&catalog_path)
        .expect("open inspector catalog")
        .expect("inspector catalog exists");
    let (mut master, mut process) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();
    loop {
        drain_pty(&mut master, &mut captured);
        let metadata = reader.metadata().expect("read inspector catalog metadata");
        if metadata.completed_scan_id != scan_id {
            assert_eq!(metadata.completed_scan_id, scan_id + 1);
            assert_eq!(metadata.next_scan_id, scan_id + 2);
            break;
        }
        if let Some(status) = process.child.try_wait().expect("inspect inspector process") {
            panic!(
                "inspector exited before catalog reconciliation ({status}):\n{}",
                String::from_utf8_lossy(&captured)
            );
        }
        assert!(
            Instant::now() < deadline,
            "inspector did not reconcile the catalog:\n{}",
            String::from_utf8_lossy(&captured)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
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
    let mut writer = smelt_store::OwnedLineageWriter::open(&sessions_root, session_id)
        .expect("create lineage fixture");
    let mut command = smelt_core::session::initial_store_commit_from_session(&session)
        .expect("build lineage fixture");
    let mut transcript = smelt_core::content::transcript::Transcript::new();
    transcript.push(smelt_core::Block::Text {
        content: transcript_marker.into(),
    });
    let records = transcript
        .history
        .block_records()
        .into_iter()
        .enumerate()
        .map(|(index, record)| {
            smelt_core::transcript_model::transcript_block_row_with_block_idx(
                index,
                index as u64,
                &record,
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .expect("build transcript fixture");
    command.transcript_records = Some(smelt_store::TranscriptRecordSuffix {
        start: smelt_store::TranscriptRecordIndex::ZERO,
        records,
    });
    writer
        .commit_session(&command)
        .expect("commit lineage fixture");
    writer
        .refresh_catalog()
        .expect("publish lineage fixture catalog row");
    writer.release().expect("release lineage fixture");

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

fn copy_fixture_directory(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create benchmark fixture destination");
    for entry in std::fs::read_dir(source).expect("read benchmark fixture directory") {
        let entry = entry.expect("read benchmark fixture entry");
        let file_type = entry.file_type().expect("read benchmark fixture file type");
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_fixture_directory(&entry.path(), &target);
        } else {
            assert!(
                file_type.is_file(),
                "benchmark fixtures cannot contain symlinks"
            );
            std::fs::copy(entry.path(), target).expect("copy benchmark fixture file");
        }
    }
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
        if let Some(fixture) = self.fixture {
            copy_fixture_directory(fixture, &state_home.join("smelt/sessions"));
        }

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
    let session_id = fixture.as_ref().map(|_| {
        std::env::var("SMELT_LIFECYCLE_BENCH_SESSION_ID")
            .expect("SMELT_LIFECYCLE_BENCH_SESSION_ID must identify a lineage fixture branch")
    });
    let fixture_database =
        fixture
            .as_ref()
            .zip(session_id.as_deref())
            .map(|(fixture, session_id)| {
                smelt_store::LineageSessionReader::open_existing(fixture, session_id)
                    .expect("open benchmark lineage fixture")
                    .database_path()
                    .to_path_buf()
            });
    let fixture_database_before = fixture_database
        .as_ref()
        .map(|database| std::fs::read(database).expect("read benchmark fixture"));
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
        session_id: session_id.as_deref(),
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
        println!(
            "LIFECYCLE_BENCH_RUN run={run} first_frame_ms={first_frame_ms} ready_ms={:.3} shutdown_ms={:.3} peak_rss_kib={}",
            sample.ready.as_secs_f64() * 1_000.0,
            sample.shutdown.as_secs_f64() * 1_000.0,
            sample.peak_rss_kib,
        );
        samples.push(sample);
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
    if let (Some(database), Some(before)) = (&fixture_database, fixture_database_before) {
        assert_eq!(
            std::fs::read(database).expect("reread benchmark fixture"),
            before,
            "lifecycle benchmark must not mutate its source fixture"
        );
    }
}

fn run_headless_config(source: &str) -> std::process::Output {
    let home = tempfile::tempdir().expect("temporary headless home");
    let config = home.path().join("init.lua");
    std::fs::write(
        &config,
        format!(
            r#"smelt.provider.register("test", {{
  type = "openai-compatible",
  api_base = "http://127.0.0.1:9/v1",
  api_key_env = "SMELT_HEADLESS_TEST_KEY",
  models = {{ "test-model" }},
}})
{source}
"#
        ),
    )
    .expect("write headless config");
    Command::new(env!("CARGO_BIN_EXE_smelt"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("SMELT_HEADLESS_TEST_KEY", "test-only")
        .current_dir(home.path())
        .args([
            "--headless",
            "--color",
            "never",
            "--model",
            "test/test-model",
            "--config",
        ])
        .arg(config)
        .arg("!printf headless-config-ok")
        .stdin(Stdio::null())
        .output()
        .expect("run headless CLI")
}

#[derive(Clone, Copy)]
enum StreamEnding {
    Eof,
    NetworkFailure,
    Cancel,
}

struct StreamingTrial {
    status: std::process::ExitStatus,
    events: Vec<serde_json::Value>,
    requests: usize,
    request_bodies: Vec<serde_json::Value>,
    tool_effect: Option<String>,
    stdout: String,
    stderr: String,
}

fn run_headless_stream(bodies: &[String], ending: StreamEnding) -> StreamingTrial {
    run_headless_stream_with_model(bodies, ending, "\"test-model\"")
}

fn run_headless_stream_with_model(
    bodies: &[String],
    ending: StreamEnding,
    model: &str,
) -> StreamingTrial {
    run_headless_stream_with_options(bodies, ending, model, "json", "openai-compatible")
}

fn run_headless_stream_with_options(
    bodies: &[String],
    ending: StreamEnding,
    model: &str,
    format: &str,
    provider_type: &str,
) -> StreamingTrial {
    let home = tempfile::tempdir().unwrap();
    let provider = TcpListener::bind("127.0.0.1:0").unwrap();
    provider.set_nonblocking(true).unwrap();
    let config = home.path().join("init.lua");
    std::fs::write(
        &config,
        format!(
            r#"
smelt.settings.autoupgrade = "off"
smelt.settings.auto_continue = "off"
smelt.provider.register("test", {{
  type = "{provider_type}",
  api_base = "http://{}/v1",
  api_key_env = "SMELT_STREAM_TEST_KEY",
  models = {{ {model} }},
}})
smelt.tools.register({{
  name = "stream_probe",
  description = "Record a test side effect",
  permission_defaults = {{ normal = "allow" }},
  parameters = {{ type = "object", properties = {{ value = {{ type = "string" }} }} }},
  execute = function(args)
    local file = assert(io.open("tool-executed", "a"))
    file:write(args.value or "missing")
    file:close()
    return {{ content = "recorded" }}
  end,
}})
"#,
            provider.local_addr().unwrap()
        ),
    )
    .unwrap();
    let stdout = home.path().join("events.jsonl");
    let stderr = home.path().join("stderr");
    let child = Command::new(env!("CARGO_BIN_EXE_smelt"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("SMELT_STREAM_TEST_KEY", "test-only")
        .current_dir(home.path())
        .args([
            "--headless",
            "--format",
            format,
            "--color",
            "never",
            "--model",
            "test/test-model",
            "--config",
        ])
        .arg(config)
        .arg("exercise the stream")
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap())
        .process_group(0)
        .spawn()
        .unwrap();
    let mut process = ChildProcessGroup {
        id: child.id() as i32,
        child,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut requests = 0;
    let mut request_bodies = Vec::new();
    let mut held_streams = Vec::new();
    let mut cancelled = false;
    let status = loop {
        if let Some(status) = process.child.try_wait().unwrap() {
            break status;
        }
        while let Ok((mut stream, _)) = provider.accept() {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let post = line.starts_with("POST ");
            let mut length = 0;
            loop {
                line.clear();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<u64>().unwrap();
                    }
                }
            }
            let mut request_body = Vec::new();
            reader.take(length).read_to_end(&mut request_body).unwrap();
            if !post {
                let body = r#"{"data":[{"id":"test-model","context_window":100000}]}"#;
                write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
                continue;
            }
            request_bodies.push(serde_json::from_slice(&request_body).unwrap());
            let body = &bodies[requests.min(bodies.len() - 1)];
            requests += 1;
            assert!(requests <= bodies.len() + 3, "unexpected retries");
            if requests < bodies.len() || matches!(ending, StreamEnding::Eof) {
                write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).unwrap();
                // Small writes exercise arbitrary transport boundaries; decoder tests
                // separately check every split, including mid-codepoint UTF-8.
                for byte in body.as_bytes() {
                    if stream.write_all(&[*byte]).is_err() {
                        break;
                    }
                }
            } else {
                write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{body}\r\n", body.len()).unwrap();
                if matches!(ending, StreamEnding::Cancel) {
                    held_streams.push(stream);
                }
                // Dropping a chunked response without its zero chunk is a transport error.
            }
        }
        if matches!(ending, StreamEnding::Cancel) && !cancelled && {
            let events = std::fs::read_to_string(&stdout).unwrap();
            events.contains("TextDelta") || events.contains("EngineAskDelta")
        } {
            assert_eq!(unsafe { libc::kill(process.id, libc::SIGINT) }, 0);
            cancelled = true;
        }
        assert!(
            Instant::now() < deadline,
            "headless stream timed out: {}",
            std::fs::read_to_string(&stderr).unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = std::fs::read_to_string(stdout).unwrap();
    let events = if format == "json" {
        stdout
            .lines()
            .map(|line| serde_json::from_str(line).expect("structured CLI event"))
            .collect()
    } else {
        Vec::new()
    };
    StreamingTrial {
        status,
        events,
        requests,
        request_bodies,
        tool_effect: std::fs::read_to_string(home.path().join("tool-executed")).ok(),
        stdout,
        stderr: std::fs::read_to_string(stderr).unwrap(),
    }
}

fn stream_event(value: serde_json::Value) -> String {
    format!("data: {value}\n\n")
}

fn stream_content() -> String {
    stream_event(
        serde_json::json!({"choices": [{"delta": {"content": "partial 界", "reasoning_content": "thinking"}}]}),
    )
}

fn stream_finish() -> String {
    stream_finish_reason("stop")
}

fn stream_finish_reason(reason: &str) -> String {
    stream_event(serde_json::json!({"choices": [{"delta": {}, "finish_reason": reason}]}))
}

fn stream_tool(arguments: &str, start: bool) -> String {
    let mut tool = serde_json::json!({"index": 0, "function": {"arguments": arguments}});
    if start {
        tool["id"] = serde_json::json!("probe-1");
        tool["function"]["name"] = serde_json::json!("stream_probe");
    }
    stream_event(serde_json::json!({"choices": [{"delta": {"tool_calls": [tool]}}]}))
}

fn assert_stream_failure(trial: &StreamingTrial, cause: &str) {
    assert_stream_failure_attempts(trial, cause, 1);
}

fn assert_stream_failure_attempts(trial: &StreamingTrial, cause: &str, attempts: usize) {
    assert_eq!(trial.status.code(), Some(3), "{}", trial.stderr);
    assert_eq!(trial.requests, attempts, "unexpected retry count");
    assert!(trial.tool_effect.is_none());
    let error = trial
        .events
        .iter()
        .find_map(|ev| ev.get("TurnError"))
        .expect("TurnError");
    assert!(
        error["message"].as_str().unwrap().contains(cause),
        "{error}"
    );
    assert!(!error.to_string().contains("sensitive-fixture"));
    assert!(!error.to_string().contains("partial 界"));
    assert!(!trial.stderr.contains("sensitive-fixture"));
    assert!(!trial
        .events
        .iter()
        .any(|ev| ev.get("TurnComplete").is_some() || ev.get("ToolStarted").is_some()));
    assert!(!trial
        .events
        .iter()
        .filter_map(|ev| ev.get("HistoryAppended"))
        .any(|ev| ev.to_string().contains("\"kind\":\"assistant\"")));
}

fn assert_output_limit(trial: &StreamingTrial, requests: usize) {
    assert_eq!(trial.status.code(), Some(3), "{:?}", trial.events);
    assert_eq!(
        trial.requests, requests,
        "output-limited response was continued"
    );
    let error = trial
        .events
        .iter()
        .find_map(|ev| ev.get("TurnError"))
        .expect("TurnError");
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("output token limit"), "{error}");
    assert!(message.contains("incomplete"), "{error}");
}

fn last_stream_assistant(trial: &StreamingTrial) -> &serde_json::Value {
    trial
        .events
        .iter()
        .rev()
        .filter_map(|ev| ev.get("HistoryAppended"))
        .filter_map(|ev| ev["delta"]["items"].as_array())
        .flat_map(|items| items.iter().rev())
        .find(|item| item["kind"] == "assistant")
        .expect("committed assistant history")
}

#[test]
fn headless_stream_malformed_then_valid_retries_unchanged_request() {
    let invalid = format!(
        "{}{}{}{}",
        stream_content(),
        stream_tool("{\"sensitive-fixture\":", true),
        stream_finish_reason("tool_calls"),
        stream_event(
            serde_json::json!({"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 4}})
        )
    );
    let valid = format!(
        "{}{}{}",
        stream_event(serde_json::json!({"choices": [{"delta": {"content": "recovered"}}]})),
        stream_finish(),
        stream_event(
            serde_json::json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2}})
        )
    );
    let trial = run_headless_stream(&[invalid, valid], StreamEnding::Eof);
    assert_eq!(trial.status.code(), Some(0), "{:?}", trial.events);
    assert_eq!(trial.requests, 2);
    assert_eq!(trial.request_bodies[0], trial.request_bodies[1]);
    assert!(trial.tool_effect.is_none());
    assert_eq!(last_stream_assistant(&trial)["content"], "recovered");
    let usage: Vec<_> = trial
        .events
        .iter()
        .filter_map(|event| event.get("TokenUsage"))
        .collect();
    assert_eq!(usage.len(), 2);
    assert_eq!(
        usage
            .iter()
            .map(|event| event["usage"]["prompt_tokens"].as_u64().unwrap())
            .sum::<u64>(),
        10
    );
    assert_eq!(
        usage
            .iter()
            .map(|event| event["usage"]["completion_tokens"].as_u64().unwrap())
            .sum::<u64>(),
        6
    );
    assert!(trial
        .events
        .iter()
        .any(|event| event.as_str() == Some("ResponseDraftRejected")));
}

#[test]
fn headless_stream_malformed_retry_exhaustion_counts_usage_once() {
    let body = format!(
        "{}{}{}{}{}",
        stream_content(),
        stream_tool("{\"sensitive-fixture\":", true),
        stream_finish_reason("length"),
        stream_event(
            serde_json::json!({"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 4}})
        ),
        stream_event(
            serde_json::json!({"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 4}})
        )
    );
    let trial = run_headless_stream(&[body], StreamEnding::Eof);
    assert_eq!(trial.status.code(), Some(3));
    assert_eq!(trial.requests, 3);
    assert!(trial
        .request_bodies
        .windows(2)
        .all(|pair| pair[0] == pair[1]));
    assert!(trial.tool_effect.is_none());
    let usage: Vec<_> = trial
        .events
        .iter()
        .filter_map(|event| event.get("TokenUsage"))
        .collect();
    assert_eq!(usage.len(), 3);
    assert_eq!(
        usage
            .iter()
            .map(|event| event["usage"]["prompt_tokens"].as_u64().unwrap())
            .sum::<u64>(),
        21
    );
    assert_eq!(
        usage
            .iter()
            .map(|event| event["usage"]["completion_tokens"].as_u64().unwrap())
            .sum::<u64>(),
        12
    );
    assert_eq!(
        trial
            .events
            .iter()
            .filter(|event| event.as_str() == Some("ResponseDraftRejected"))
            .count(),
        3
    );
    assert!(!trial
        .events
        .iter()
        .any(|event| event.get("ToolStarted").is_some()));
    let error = trial
        .events
        .iter()
        .find_map(|event| event.get("TurnError"))
        .unwrap();
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("malformed response"));
    assert!(!error.to_string().contains("sensitive-fixture"));
    assert!(!trial.stderr.contains("sensitive-fixture"));
}

#[test]
fn headless_stream_mixed_batch_executes_only_after_valid_recovery() {
    let invalid_batch = stream_event(serde_json::json!({"choices": [{"delta": {"tool_calls": [
        {"index": 0, "id": "valid-call", "function": {"name": "stream_probe", "arguments": "{\"value\":\"must-not-run\"}"}},
        {"index": 1, "id": "invalid-call", "function": {"name": "stream_probe", "arguments": "[1,2]"}}
    ]}}]}));
    let valid = format!(
        "{}{}",
        stream_tool("{\"value\":\"recovered\"}", true),
        stream_finish_reason("tool_calls")
    );
    let final_answer = format!(
        "{}{}",
        stream_event(serde_json::json!({"choices": [{"delta": {"content": "done"}}]})),
        stream_finish()
    );
    let trial = run_headless_stream(
        &[
            format!(
                "{}{}{}",
                stream_content(),
                invalid_batch,
                stream_finish_reason("tool_calls")
            ),
            valid,
            final_answer,
        ],
        StreamEnding::Eof,
    );
    assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
    assert_eq!(trial.requests, 3);
    assert_eq!(trial.request_bodies[0], trial.request_bodies[1]);
    assert_eq!(trial.tool_effect.as_deref(), Some("recovered"));
    assert!(!trial.request_bodies[2]["messages"]
        .to_string()
        .contains("must-not-run"));
    let assistants: Vec<_> = trial
        .events
        .iter()
        .filter_map(|event| event.get("HistoryAppended"))
        .flat_map(|event| event["delta"]["items"].as_array().unwrap())
        .filter(|item| item["kind"] == "assistant")
        .collect();
    assert!(assistants
        .iter()
        .all(|item| item.get("reasoning").is_none()));
    assert!(!assistants
        .iter()
        .any(|item| item["content"] == "partial 界"));
}

#[test]
fn headless_compacts_before_next_request_using_reported_context() {
    let tool = format!(
        "{}{}{}",
        stream_tool("{\"value\":\"recorded\"}", true),
        stream_finish_reason("tool_calls"),
        stream_event(
            serde_json::json!({"choices": [], "usage": {"prompt_tokens": 90000, "completion_tokens": 2}})
        )
    );
    let answer = |content: &str| {
        format!(
            "{}{}",
            stream_event(serde_json::json!({"choices": [{"delta": {"content": content}}]})),
            stream_finish()
        )
    };
    let bodies = [tool, answer("# Goal\nContinue the test"), answer("done")];
    for model in [
        r#"{ name = "test-model", context_window = 100000 }"#,
        r#""test-model""#,
    ] {
        let trial = run_headless_stream_with_model(&bodies, StreamEnding::Eof, model);
        assert_eq!(trial.status.code(), Some(0), "{:?}", trial.events);
        assert_eq!(
            trial.requests,
            3,
            "compaction was not invoked: usage={:?}, stderr={}",
            trial
                .events
                .iter()
                .filter_map(|event| event.get("TokenUsage"))
                .collect::<Vec<_>>(),
            trial.stderr
        );
        let summarizer = &trial.request_bodies[1];
        assert!(
            summarizer["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("CONTEXT CHECKPOINT COMPACTION")
        );
        assert_eq!(summarizer["tools"], trial.request_bodies[0]["tools"]);
        let resumed = &trial.request_bodies[2]["messages"];
        assert!(resumed.to_string().contains("# Goal\\nContinue the test"));
        assert!(resumed.to_string().contains("probe-1"));
        assert_eq!(trial.tool_effect.as_deref(), Some("recorded"));
        assert_eq!(last_stream_assistant(&trial)["content"], "done");
    }
}

#[test]
fn headless_compaction_retry_rejects_only_the_auxiliary_draft() {
    let tool = format!(
        "{}{}{}",
        stream_tool("{\"value\":\"recorded\"}", true),
        stream_finish_reason("tool_calls"),
        stream_event(
            serde_json::json!({"choices": [], "usage": {"prompt_tokens": 90000, "completion_tokens": 2}})
        ),
    );
    let malformed = format!(
        "{}{}{}",
        stream_event(serde_json::json!({"choices": [{"delta": {"content": "rejected summary"}}]})),
        stream_tool("[1,2]", true),
        stream_finish_reason("tool_calls"),
    );
    let answer = |content: &str| {
        format!(
            "{}{}",
            stream_event(serde_json::json!({"choices": [{"delta": {"content": content}}]})),
            stream_finish(),
        )
    };
    let trial = run_headless_stream_with_model(
        &[
            tool,
            malformed,
            answer("# Goal\nContinue safely"),
            answer("done"),
        ],
        StreamEnding::Eof,
        r#"{ name = "test-model", context_window = 100000 }"#,
    );
    assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
    assert_eq!(trial.requests, 4);
    assert_eq!(trial.request_bodies[1], trial.request_bodies[2]);
    assert_eq!(trial.tool_effect.as_deref(), Some("recorded"));
    let rejection = trial
        .events
        .iter()
        .find_map(|event| event.get("EngineAskDraftRejected"))
        .expect("auxiliary retry must identify its rejected draft");
    let id = &rejection["id"];
    assert!(trial
        .events
        .iter()
        .filter_map(|event| event.get("EngineAskDelta"))
        .any(|event| &event["id"] == id && event["delta"] == "rejected summary"));
    assert!(!trial
        .events
        .iter()
        .any(|event| event.as_str() == Some("ResponseDraftRejected")));
    assert!(!trial.request_bodies[3]
        .to_string()
        .contains("rejected summary"));
    assert_eq!(last_stream_assistant(&trial)["content"], "done");
}

#[test]
fn headless_cancellation_during_compaction_stops_requests() {
    let tool = format!(
        "{}{}{}",
        stream_tool("{\"value\":\"recorded\"}", true),
        stream_finish_reason("tool_calls"),
        stream_event(
            serde_json::json!({"choices": [], "usage": {"prompt_tokens": 90000, "completion_tokens": 2}})
        )
    );
    let summary =
        stream_event(serde_json::json!({"choices": [{"delta": {"content": "draft summary"}}]}));
    let trial = run_headless_stream_with_model(
        &[tool, summary],
        StreamEnding::Cancel,
        r#"{ name = "test-model", context_window = 100000 }"#,
    );
    assert_eq!(trial.status.code(), Some(130), "{}", trial.stderr);
    assert_eq!(trial.requests, 2);
    assert_eq!(trial.tool_effect.as_deref(), Some("recorded"));
    assert!(trial
        .events
        .iter()
        .any(|event| event.get("EngineAskDelta").is_some()));
}

#[test]
fn headless_text_output_resets_only_rejected_attempt() {
    let tool = format!(
        "{}{}{}",
        stream_event(serde_json::json!({"choices": [{"delta": {"content": "kept 界\n"}}]})),
        stream_tool("{\"value\":\"recorded\"}", true),
        stream_finish_reason("tool_calls"),
    );
    let malformed = format!(
        "{}{}{}",
        stream_content(),
        stream_tool("{\"sensitive-fixture\":", true),
        stream_finish_reason("length")
    );
    let recovered = format!(
        "{}{}",
        stream_event(serde_json::json!({"choices": [{"delta": {"content": "recovered"}}]})),
        stream_finish(),
    );
    let trial = run_headless_stream_with_options(
        &[tool, malformed, recovered],
        StreamEnding::Eof,
        "\"test-model\"",
        "text",
        "openai-compatible",
    );
    assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
    assert_eq!(trial.requests, 3);
    assert_eq!(trial.stdout, "kept 界\nrecovered\n");
    assert_eq!(trial.request_bodies[1], trial.request_bodies[2]);
    assert_eq!(trial.tool_effect.as_deref(), Some("recorded"));
    assert!(!trial.stderr.contains("sensitive-fixture"));
}

#[test]
fn headless_stream_responses_terminal_arguments_reject_entire_draft_batch() {
    let completed = |input_tokens, output_tokens| {
        stream_event(
            serde_json::json!({"type": "response.completed", "response": {
                "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
            }}),
        )
    };
    let call = |id, draft, terminal| {
        [
            serde_json::json!({"type": "response.output_item.added", "item": {
                "type": "function_call", "id": id, "call_id": id, "name": "stream_probe"
            }}),
            serde_json::json!({"type": "response.function_call_arguments.done", "item_id": id, "arguments": draft}),
            serde_json::json!({"type": "response.output_item.done", "item": {
                "type": "function_call", "id": id, "call_id": id, "name": "stream_probe", "arguments": terminal
            }}),
        ].into_iter().map(stream_event).collect::<String>()
    };
    let malformed = format!(
        "{}{}{}{}",
        stream_event(
            serde_json::json!({"type": "response.output_text.delta", "delta": "rejected draft"})
        ),
        call(
            "valid-call",
            r#"{"value":"must-not-run"}"#,
            r#"{"value":"must-not-run"}"#
        ),
        call("invalid-call", r#"{"value":"must-not-run"}"#, "[1,2]"),
        completed(7, 4),
    );
    let recovered = format!(
        "{}{}",
        call("recovered-call", r#"{"value":"#, r#"{"value":"recovered"}"#),
        completed(3, 2),
    );
    let answer = format!(
        "{}{}",
        stream_event(serde_json::json!({"type": "response.output_text.delta", "delta": "done"})),
        completed(2, 1),
    );
    let trial = run_headless_stream_with_options(
        &[malformed, recovered, answer],
        StreamEnding::Eof,
        "\"test-model\"",
        "json",
        "openai",
    );
    assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
    assert_eq!(trial.tool_effect.as_deref(), Some("recovered"));
    assert_eq!(trial.requests, 3);
    assert_eq!(trial.request_bodies[0], trial.request_bodies[1]);
    assert_eq!(last_stream_assistant(&trial)["content"], "done");
    assert_eq!(
        trial
            .events
            .iter()
            .filter(|event| event.as_str() == Some("ResponseDraftRejected"))
            .count(),
        1
    );
    let usage: Vec<_> = trial
        .events
        .iter()
        .filter_map(|event| event.get("TokenUsage"))
        .collect();
    assert_eq!(usage.len(), 3);
    assert_eq!(
        usage
            .iter()
            .map(|event| event["usage"]["prompt_tokens"].as_u64().unwrap())
            .sum::<u64>(),
        12
    );
    assert_eq!(
        usage
            .iter()
            .map(|event| event["usage"]["completion_tokens"].as_u64().unwrap())
            .sum::<u64>(),
        7
    );
}

#[test]
fn headless_stream_anthropic_empty_tool_input_is_accepted() {
    let tool = [
        serde_json::json!({"type": "message_start", "message": {"usage": {"input_tokens": 7}}}),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "probe-1", "name": "stream_probe", "input": {}}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 2}}),
        serde_json::json!({"type": "message_stop"}),
    ].into_iter().map(stream_event).collect::<String>();
    let answer = [
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "done"}}),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
        serde_json::json!({"type": "message_stop"}),
    ].into_iter().map(stream_event).collect::<String>();
    let trial = run_headless_stream_with_options(
        &[tool, answer],
        StreamEnding::Eof,
        "\"test-model\"",
        "json",
        "anthropic-compatible",
    );
    assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
    assert_eq!(trial.requests, 2);
    assert_eq!(trial.tool_effect.as_deref(), Some("missing"));
    assert_eq!(last_stream_assistant(&trial)["content"], "done");
    assert!(!trial
        .events
        .iter()
        .any(|event| event == "ResponseDraftRejected"));
}

#[test]
fn headless_recovers_from_context_limit_with_compaction() {
    let tool = format!(
        "{}{}",
        stream_tool("{\"value\":\"recorded\"}", true),
        stream_finish_reason("tool_calls")
    );
    let context_error = stream_event(
        serde_json::json!({"error": {"code": "context_length_exceeded", "message": "private-provider-detail"}}),
    );
    let answer = |content: &str| {
        format!(
            "{}{}",
            stream_event(serde_json::json!({"choices": [{"delta": {"content": content}}]})),
            stream_finish()
        )
    };
    let trial = run_headless_stream_with_model(
        &[
            tool,
            context_error,
            answer("# Goal\nResume safely"),
            answer("done"),
        ],
        StreamEnding::Eof,
        r#"{ name = "test-model", context_window = 100000 }"#,
    );
    assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
    assert_eq!(trial.requests, 4);
    assert!(trial.request_bodies[2]["messages"]
        .to_string()
        .contains("CONTEXT CHECKPOINT COMPACTION"));
    assert!(trial.request_bodies[3]["messages"]
        .to_string()
        .contains("# Goal\\nResume safely"));
    assert_eq!(trial.tool_effect.as_deref(), Some("recorded"));
    assert_eq!(last_stream_assistant(&trial)["content"], "done");
    assert!(!trial
        .events
        .iter()
        .any(|event| event.get("TurnError").is_some()));
}

#[test]
fn headless_compaction_quota_failure_aborts_without_looping() {
    let tool = format!(
        "{}{}{}",
        stream_tool("{\"value\":\"recorded\"}", true),
        stream_finish_reason("tool_calls"),
        stream_event(
            serde_json::json!({"choices": [], "usage": {"prompt_tokens": 90000, "completion_tokens": 2}})
        )
    );
    let quota = stream_event(serde_json::json!({"error": {"code": "insufficient_quota"}}));
    let trial = run_headless_stream_with_model(
        &[tool, quota],
        StreamEnding::Eof,
        r#"{ name = "test-model", context_window = 100000 }"#,
    );
    assert_eq!(trial.status.code(), Some(3), "{}", trial.stderr);
    assert_eq!(trial.requests, 2);
    let error = trial
        .events
        .iter()
        .find_map(|event| event.get("TurnError"))
        .expect("terminal error");
    assert!(error["message"].as_str().unwrap().contains("quota"));
}

#[test]
fn headless_stream_valid_content_and_usage() {
    let usage = stream_event(
        serde_json::json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 2}}),
    );
    let body = format!(
        ": keepalive\n\n{}{}{}data: [DONE]\n\n",
        stream_content(),
        stream_finish(),
        usage
    )
    .replace("data: ", "data:")
    .replace('\n', "\r\n");
    let trial = run_headless_stream(&[body], StreamEnding::Eof);
    assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
    assert_eq!(trial.requests, 1);
    assert!(trial.events.iter().any(|ev| ev
        .get("TextDelta")
        .is_some_and(|v| v.to_string().contains("界"))));
    assert!(trial
        .events
        .iter()
        .any(|ev| ev.get("TurnComplete").is_some()));
    assert!(trial.events.iter().any(|ev| ev.get("TokenUsage").is_some()));
}

#[test]
fn headless_stream_reasoning_only_stop_is_complete() {
    let body = format!(
        "{}{}data: [DONE]\n\n",
        stream_event(
            serde_json::json!({"choices": [{"delta": {"reasoning_content": "thinking"}}]})
        ),
        stream_finish(),
    );
    let trial = run_headless_stream(&[body], StreamEnding::Eof);
    assert_eq!(trial.status.code(), Some(0), "{:?}", trial.events);
    assert_eq!(trial.requests, 1);
    assert!(!trial.events.iter().any(|ev| ev.get("TurnError").is_some()));
    assert!(trial
        .events
        .iter()
        .any(|ev| ev.get("TurnComplete").is_some()));
}

#[test]
fn headless_stream_output_limit_is_incomplete() {
    for delta in [
        serde_json::json!({"reasoning_content": "thinking"}),
        serde_json::json!({"content": "partial answer"}),
        serde_json::json!({}),
    ] {
        let body = format!(
            "{}{}data: [DONE]\n\n",
            stream_event(serde_json::json!({"choices": [{"delta": delta}]})),
            stream_finish_reason("length"),
        );
        let trial = run_headless_stream(&[body], StreamEnding::Eof);
        assert_output_limit(&trial, 1);
        assert!(trial.tool_effect.is_none());
        let assistant = last_stream_assistant(&trial);
        assert_eq!(assistant["content"], delta["content"]);
        assert_eq!(assistant["reasoning"], delta["reasoning_content"]);
        assert!(assistant.get("invocations").is_none());
    }
}

#[test]
fn headless_stream_output_limited_tools_execute_then_stop() {
    let body = format!(
        "{}{}{}data: [DONE]\n\n",
        stream_content(),
        stream_tool("{\"value\":\"recorded\"}", true),
        stream_finish_reason("length"),
    );
    let trial = run_headless_stream(&[body], StreamEnding::Eof);
    assert_eq!(
        trial.tool_effect.as_deref(),
        Some("recorded"),
        "{:?}",
        trial.events
    );
    assert_output_limit(&trial, 1);
    let assistant = last_stream_assistant(&trial);
    assert_eq!(assistant["content"], "partial 界");
    assert_eq!(assistant["reasoning"], "thinking");
    let invocations = assistant["invocations"]
        .as_array()
        .expect("committed tool results");
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0]["call_id"], "probe-1");
    assert_eq!(invocations[0]["name"], "stream_probe");
    assert_eq!(invocations[0]["result"]["content"], "recorded");
    assert_eq!(invocations[0]["result"]["is_error"], false);
    let tool_finished = trial
        .events
        .iter()
        .position(|ev| ev.get("ToolFinished").is_some())
        .expect("ToolFinished");
    let error_index = trial
        .events
        .iter()
        .position(|ev| ev.get("TurnError").is_some())
        .unwrap();
    assert!(tool_finished < error_index);
}

#[test]
fn headless_stream_empty_output_limit_after_tool_is_not_retried() {
    let tool = format!(
        "{}{}",
        stream_tool("{\"value\":\"recorded\"}", true),
        stream_finish()
    );
    let trial = run_headless_stream(&[tool, stream_finish_reason("length")], StreamEnding::Eof);
    assert_eq!(trial.tool_effect.as_deref(), Some("recorded"));
    assert_output_limit(&trial, 2);
    let assistant = last_stream_assistant(&trial);
    assert!(assistant.get("content").is_none());
    assert!(assistant.get("reasoning").is_none());
    assert!(assistant.get("invocations").is_none());
}

#[test]
fn headless_stream_thinking_disabled_replays_only_received_reasoning_fields() {
    for empty_reasoning in [false, true] {
        let reasoning = if empty_reasoning {
            stream_event(serde_json::json!({"choices": [{"delta": {"reasoning_content": ""}}]}))
        } else {
            String::new()
        };
        let tool = format!(
            "{reasoning}{}{}data: [DONE]\n\n",
            stream_tool("{\"value\":\"recorded\"}", true),
            stream_finish_reason("tool_calls")
        );
        let final_response = format!(
            "{}{}data: [DONE]\n\n",
            stream_event(serde_json::json!({"choices": [{"delta": {"content": "done"}}]})),
            stream_finish()
        );
        let trial = run_headless_stream_with_model(
            &[tool, final_response],
            StreamEnding::Eof,
            r#"{ name = "test-model", supports_reasoning = true, thinking_token_budget = 8192, max_tokens = 32768, chat_template_kwargs = { enable_thinking = false } }"#,
        );
        assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
        assert_eq!(trial.requests, 2);
        assert_eq!(trial.tool_effect.as_deref(), Some("recorded"));
        for body in &trial.request_bodies {
            assert_eq!(body["reasoning_effort"], "none");
            assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
            assert_eq!(body["thinking_token_budget"], 8192);
            assert_eq!(body["max_tokens"], 32768);
        }
        let assistant = trial.request_bodies[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        if empty_reasoning {
            assert_eq!(assistant["reasoning_content"], "");
        } else {
            assert!(assistant.get("reasoning_content").is_none());
        }
    }
}

#[test]
fn headless_stream_replays_reasoning_across_multiple_tool_continuations() {
    for field in [
        "reasoning_content",
        "reasoning",
        "reasoning_text",
        "unsupported_reasoning",
    ] {
        let reasoning = |text: &str| {
            let mut delta = serde_json::json!({});
            delta[field] = serde_json::json!(text);
            stream_event(serde_json::json!({"choices": [{"delta": delta}]}))
        };
        let first = format!(
            "{}{}{}data: [DONE]\n\n",
            reasoning("first thought"),
            stream_tool("{\"value\":\"first\"}", true),
            stream_finish_reason("tool_calls")
        );
        let second = format!(
            "{}{}{}data: [DONE]\n\n",
            reasoning("second thought"),
            stream_tool("{\"value\":\"second\"}", true).replace("probe-1", "probe-2"),
            stream_finish_reason("tool_calls")
        );
        let final_response = format!(
            "{}{}data: [DONE]\n\n",
            stream_event(serde_json::json!({"choices": [{"delta": {"content": "done"}}]})),
            stream_finish()
        );
        let trial = run_headless_stream(&[first, second, final_response], StreamEnding::Eof);
        assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
        assert_eq!(trial.requests, 3);
        assert_eq!(trial.tool_effect.as_deref(), Some("firstsecond"));
        for (index, body) in trial.request_bodies.iter().enumerate() {
            let messages = body["messages"].as_array().unwrap();
            let assistants: Vec<_> = messages
                .iter()
                .filter(|message| message["role"] == "assistant")
                .collect();
            assert_eq!(assistants.len(), index);
            for (step, assistant) in assistants.iter().enumerate() {
                let expected = if step == 0 {
                    "first thought"
                } else {
                    "second thought"
                };
                if field == "unsupported_reasoning" {
                    assert!(assistant.get(field).is_none());
                    assert_eq!(assistant.as_object().unwrap().len(), 2);
                } else {
                    assert_eq!(assistant[field], expected);
                    assert_eq!(assistant.as_object().unwrap().len(), 3);
                }
                assert_eq!(
                    assistant["tool_calls"][0]["id"],
                    format!("probe-{}", step + 1)
                );
                assert!(messages.iter().any(|message| message["role"] == "tool"
                    && message["tool_call_id"] == assistant["tool_calls"][0]["id"]));
            }
            assert!(!body.to_string().contains("reasoning_details"));
            assert!(!body.to_string().contains("tool_metadata"));
            assert!(!body.to_string().contains("endpoint_sha256"));
        }
    }
}

#[test]
fn headless_stream_valid_fragmented_tool_call() {
    let body = format!(
        "{}{}{}data: [DONE]\n\n",
        stream_tool("{\"value\":", true),
        stream_tool("\"recorded\"}", false),
        stream_finish()
    );
    let trial = run_headless_stream(
        &[body, format!("{}{}", stream_content(), stream_finish())],
        StreamEnding::Eof,
    );
    assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
    assert_eq!(trial.requests, 2);
    assert_eq!(
        trial.tool_effect.as_deref(),
        Some("recorded"),
        "{:?}",
        trial.events
    );
    assert!(trial
        .events
        .iter()
        .any(|ev| ev.get("ToolStarted").is_some()));
    assert!(trial
        .events
        .iter()
        .any(|ev| ev.get("ToolFinished").is_some()));
}

#[test]
fn headless_stream_upstream_error() {
    for (error, cause) in [
        (
            stream_event(
                serde_json::json!({"error": {"message": "sensitive-fixture", "code": "server_error"}}),
            ),
            "upstream error event",
        ),
        (
            "event: error\ndata: sensitive-fixture\n\n".into(),
            "upstream error event",
        ),
        ("event: error\ndata:\n\n".into(), "upstream error event"),
        (
            stream_event(
                serde_json::json!({"error": {"message": "sensitive-fixture", "code": "insufficient_quota"}}),
            ),
            "API quota exceeded",
        ),
        (
            stream_event(
                serde_json::json!({"error": {"message": "sensitive-fixture", "code": "rate_limit_exceeded", "resets_at": 0}}),
            ),
            "rate limited",
        ),
    ] {
        assert_stream_failure(
            &run_headless_stream(
                &[format!("{}{}{error}", stream_content(), stream_finish())],
                StreamEnding::Eof,
            ),
            cause,
        );
    }
}

#[test]
fn headless_stream_malformed_json() {
    assert_stream_failure(
        &run_headless_stream(
            &[format!(
                "{}{}data: {{sensitive-fixture}}\n\n",
                stream_content(),
                stream_finish()
            )],
            StreamEnding::Eof,
        ),
        "malformed JSON",
    );
}

#[test]
fn headless_stream_eof_during_event() {
    for tail in ["data: {\"sensitive-fixture\":", "data: {\"ok\":true}\n"] {
        assert_stream_failure(
            &run_headless_stream(
                &[format!("{}{}{tail}", stream_content(), stream_finish())],
                StreamEnding::Eof,
            ),
            "incomplete SSE event",
        );
    }
}

#[test]
fn headless_stream_eof_without_finish_reason() {
    assert_stream_failure(
        &run_headless_stream(&[stream_content()], StreamEnding::Eof),
        "without finish_reason",
    );
}

#[test]
fn headless_stream_done_without_finish_reason() {
    let trial = run_headless_stream(
        &[format!("{}data: [DONE]\n\n", stream_content())],
        StreamEnding::Eof,
    );
    assert_stream_failure(&trial, "without finish_reason");
    let error = trial
        .events
        .iter()
        .find_map(|ev| ev.get("TurnError"))
        .unwrap();
    assert!(error["message"].as_str().unwrap().contains("done=true"));
}

#[test]
fn headless_stream_network_interruption() {
    assert_stream_failure(
        &run_headless_stream(&[stream_content()], StreamEnding::NetworkFailure),
        "network error",
    );
}

#[test]
fn headless_stream_cancellation() {
    let trial = run_headless_stream(
        &[format!("{}data: {{", stream_content())],
        StreamEnding::Cancel,
    );
    assert_eq!(trial.status.code(), Some(130), "{}", trial.stderr);
    assert_eq!(trial.requests, 1);
    assert!(trial.tool_effect.is_none());
    assert!(!trial.events.iter().any(|ev| ev.get("TurnError").is_some()));
}

#[test]
fn headless_stream_incomplete_tool_is_not_executed() {
    for finish in [false, true] {
        let body = format!(
            "{}{}data: [DONE]\n\n",
            stream_tool("{\"value\":", true),
            if finish {
                stream_finish()
            } else {
                String::new()
            }
        );
        assert_stream_failure_attempts(
            &run_headless_stream(&[body], StreamEnding::Eof),
            if finish {
                "tool-call arguments"
            } else {
                "without finish_reason"
            },
            if finish { 3 } else { 1 },
        );
    }
}

#[test]
fn headless_stream_tool_finish_does_not_execute_before_stream_validation() {
    let tool = format!(
        "{}{}",
        stream_tool("{\"value\":\"recorded\"}", true),
        stream_finish()
    );
    for tail in [
        "data: {\"error\":{\"message\":\"sensitive-fixture\"}}\n\n",
        "data: {sensitive-fixture}\n\n",
        "data: {",
    ] {
        let trial = run_headless_stream(&[format!("{tool}{tail}")], StreamEnding::Eof);
        assert_stream_failure(
            &trial,
            if tail.contains("error") {
                "upstream error event"
            } else if tail.ends_with("\n\n") {
                "malformed JSON"
            } else {
                "incomplete SSE event"
            },
        );
        assert!(trial
            .events
            .iter()
            .any(|ev| ev.get("ToolCallDraftFinished").is_some()));
        assert!(!trial
            .events
            .iter()
            .any(|ev| ev.get("ToolDispatch").is_some()));
    }
}

#[test]
fn headless_stream_failure_after_tool_effect_does_not_replay() {
    let tool = format!(
        "{}{}",
        stream_tool("{\"value\":\"recorded\"}", true),
        stream_finish()
    );
    let trial = run_headless_stream(&[tool, stream_content()], StreamEnding::NetworkFailure);
    assert_eq!(trial.status.code(), Some(3), "{}", trial.stderr);
    assert_eq!(trial.requests, 2, "failed response was retried");
    assert_eq!(trial.tool_effect.as_deref(), Some("recorded"));
    let error = trial
        .events
        .iter()
        .find_map(|ev| ev.get("TurnError"))
        .unwrap();
    assert!(error["message"].as_str().unwrap().contains("network error"));
    assert!(!trial
        .events
        .iter()
        .any(|ev| ev.get("TurnComplete").is_some()));
}

#[test]
fn headless_stream_multiline_data() {
    let body = format!("data: {{\"choices\": [\n: keepalive\ndata: {{\"delta\": {{\"content\": \"multiline\"}}}}]}}\n\n{}", stream_finish());
    let trial = run_headless_stream(&[body], StreamEnding::Eof);
    assert_eq!(trial.status.code(), Some(0), "{}", trial.stderr);
    assert!(trial.events.iter().any(|ev| ev
        .get("TextDelta")
        .is_some_and(|v| v.to_string().contains("multiline"))));
}

#[test]
fn headless_settings_load_before_shell_dispatch() {
    let output = run_headless_config(
        r#"
assert(smelt.settings.restrict_to_workspace == true)
smelt.settings.autoupgrade = "off"
smelt.settings.fast_mode = true
smelt.settings.restrict_to_workspace = false
assert(smelt.settings.autoupgrade == "off")
assert(smelt.settings.fast_mode == true)
assert(smelt.settings.restrict_to_workspace == false)
"#,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"headless-config-ok");
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn headless_config_errors_prevent_shell_dispatch() {
    for source in [
        "error('invalid headless config')",
        "smelt.settings.unknown_setting = true",
        "smelt.settings.fast_mode = 'true'",
    ] {
        let output = run_headless_config(source);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("error: lua init:"));
    }
}
