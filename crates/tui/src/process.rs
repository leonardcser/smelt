//! Process capability — synchronous spawn-and-wait primitive over
//! `std::process::Command`. Pure subprocess composition, no policy.
//! Exposed to Lua via `crates/tui/src/lua/api/process.rs::run` and
//! composed by tools that need to run a short-lived shell command.
//!
//! Long-lived background processes go through the engine
//! `ProcessRegistry` (`smelt.process.{list,kill,read_output,spawn_bg}`);
//! that surface stays put. The future `tui::subprocess` module will
//! cover bidirectional event-channel children (sub-agents, MCP).
//!
//! `run_streaming` is the async counterpart used by the bash tool —
//! drives the child via `tokio::process`, fires a per-line callback
//! as stdout/stderr arrive, returns aggregated output. The Lua-side
//! `bash` tool calls this through `smelt.process.run_streaming` and
//! parks its coroutine on `smelt.task.wait`.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Options accepted by [`run`]. Defaults: 30s timeout, inherit env,
/// no stdin, capture stdout+stderr.
#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    pub(crate) cwd: Option<String>,
    pub(crate) env: HashMap<String, String>,
    pub(crate) timeout: Option<Duration>,
    /// Optional stdin text; written to the child's stdin then closed.
    pub(crate) stdin: Option<String>,
}

/// Result of a single short-lived process invocation.
#[derive(Debug, Clone)]
pub(crate) struct Output {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
    pub(crate) timed_out: bool,
}

/// Run `cmd` with `args` and the given options, awaiting exit (or the
/// configured timeout). Stdout/stderr are captured as UTF-8 (lossy).
pub(crate) fn run<I, S>(cmd: &str, args: I, opts: &Options) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(cmd);
    command.args(args);

    if let Some(cwd) = &opts.cwd {
        command.current_dir(cwd);
    }
    for (k, v) in &opts.env {
        command.env(k, v);
    }

    let stdin_kind = if opts.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    command
        .stdin(stdin_kind)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;

    if let (Some(text), Some(stdin)) = (&opts.stdin, child.stdin.as_mut()) {
        use std::io::Write;
        stdin.write_all(text.as_bytes())?;
    }
    child.stdin.take(); // close

    let timeout = opts.timeout.unwrap_or(Duration::from_secs(30));
    let deadline = Instant::now() + timeout;

    loop {
        match child.try_wait()? {
            Some(status) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read;
                    let _ = out.read_to_string(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read;
                    let _ = err.read_to_string(&mut stderr);
                }
                return Ok(Output {
                    stdout,
                    stderr,
                    exit_code: status.code().unwrap_or(-1),
                    timed_out: false,
                });
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(Output {
                        stdout: String::new(),
                        stderr: format!("process timed out after {}s", timeout.as_secs()),
                        exit_code: -1,
                        timed_out: true,
                    });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Result of [`run_streaming`]. Mirrors [`Output`] without the
/// stdout/stderr split — streaming aggregates both into one buffer
/// since per-line callbacks already saw them in arrival order.
#[derive(Debug, Clone)]
pub(crate) struct StreamOutput {
    /// stdout + stderr lines joined by '\n' in arrival order.
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) timed_out: bool,
}

/// Spawn `sh -c command` and stream stdout+stderr lines through
/// `on_line` as they arrive. Returns aggregated output once the child
/// exits or the timeout expires. The child is in its own process
/// group so `kill_process_group` semantics are available to a future
/// cancel path.
pub(crate) async fn run_streaming(
    command: &str,
    timeout: Duration,
    mut on_line: impl FnMut(String),
) -> StreamOutput {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return StreamOutput {
                content: e.to_string(),
                is_error: true,
                timed_out: false,
            };
        }
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();
    let mut output = String::new();
    let mut stdout_done = false;
    let mut stderr_done = false;

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            line = stdout_reader.next_line(), if !stdout_done => {
                match line {
                    Ok(Some(line)) => {
                        on_line(line.clone());
                        if !output.is_empty() { output.push('\n'); }
                        output.push_str(&line);
                    }
                    _ => stdout_done = true,
                }
            }
            line = stderr_reader.next_line(), if !stderr_done => {
                match line {
                    Ok(Some(line)) => {
                        on_line(line.clone());
                        if !output.is_empty() { output.push('\n'); }
                        output.push_str(&line);
                    }
                    _ => stderr_done = true,
                }
            }
            _ = &mut deadline => {
                kill_process_group(&child);
                return StreamOutput {
                    content: format!("timed out after {:.0}s", timeout.as_secs_f64()),
                    is_error: true,
                    timed_out: true,
                };
            }
        }
    }

    let status = child.wait().await;
    let is_error = status.map(|s| !s.success()).unwrap_or(true);
    StreamOutput {
        content: output,
        is_error,
        timed_out: false,
    }
}

#[cfg(unix)]
fn kill_process_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        unsafe {
            // Negative pid → process group; SIGTERM.
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_child: &tokio::process::Child) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_echo_captures_stdout() {
        let out = run("sh", ["-c", "echo hello"], &Options::default()).unwrap();
        assert!(out.stdout.contains("hello"));
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
    }

    #[test]
    fn run_propagates_exit_code() {
        let out = run("sh", ["-c", "exit 42"], &Options::default()).unwrap();
        assert_eq!(out.exit_code, 42);
    }

    #[test]
    fn run_pipes_stdin_to_child() {
        let opts = Options {
            stdin: Some("hello world".into()),
            ..Default::default()
        };
        let out = run("cat", Vec::<&str>::new(), &opts).unwrap();
        assert_eq!(out.stdout, "hello world");
    }

    #[test]
    fn run_honors_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = Options {
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let out = run("pwd", Vec::<&str>::new(), &opts).unwrap();
        assert!(out.stdout.contains(tmp.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn run_times_out_long_command() {
        let opts = Options {
            timeout: Some(Duration::from_millis(100)),
            ..Default::default()
        };
        let out = run("sh", ["-c", "sleep 5"], &opts).unwrap();
        assert!(out.timed_out);
        assert_eq!(out.exit_code, -1);
    }
}
