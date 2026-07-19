#![cfg(unix)]

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ChildProcessGroup {
    child: Child,
    id: i32,
}

impl Drop for ChildProcessGroup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-self.id, libc::SIGKILL);
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

    let (mut master, slave) = open_pty();
    let stdin = slave.try_clone().expect("clone PTY slave for stdin");
    let stdout = slave.try_clone().expect("clone PTY slave for stdout");
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
        .env("NO_COLOR", "1")
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
    let mut process = ChildProcessGroup {
        id: child.id() as i32,
        child,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut rendered_at = None;

    while Instant::now() < deadline {
        loop {
            match master.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => captured.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("read smelt PTY: {error}"),
            }
        }

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
