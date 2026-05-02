//! Process capability — synchronous spawn-and-wait primitive over
//! `std::process::Command`. Pure subprocess composition, no policy.
//! Exposed to Lua via `crates/tui/src/lua/api/process.rs::run` and
//! composed by tools that need to run a short-lived shell command.
//!
//! Long-lived background processes go through the engine
//! `ProcessRegistry` (`smelt.process.{list,kill,read_output,spawn_bg}`);
//! that surface stays put. The future `tui::subprocess` module will
//! cover bidirectional event-channel children (MCP, etc.).
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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;
use tokio::sync::mpsc;

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
    cancel: Option<CancellationToken>,
) -> StreamOutput {
    if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
        return StreamOutput {
            content: "cancelled".to_string(),
            is_error: true,
            timed_out: false,
        };
    }

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
            biased;
            _ = cancel.as_ref().unwrap().cancelled(), if cancel.as_ref().is_some_and(|c| !c.is_cancelled()) => {
                kill_process_group(&child);
                return StreamOutput {
                    content: "cancelled".to_string(),
                    is_error: true,
                    timed_out: false,
                };
            }
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

/// SIGKILL variant of [`kill_process_group`] used by the background-
/// process registry's stop path. The streaming `run_streaming` timeout
/// path uses SIGTERM (above) for graceful shutdown; explicit
/// `stop_process` invocations skip the grace period.
#[cfg(unix)]
fn kill_group_sigkill(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: pid is a valid process group ID (we set process_group(0) at spawn).
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group_sigkill(_child: &tokio::process::Child) {}

// ── Background-process registry ───────────────────────────────────────
// Long-lived `proc_<n>` children spawned via the `bash` tool's
// `run_in_background` parameter. Output buffered on the registry; the Lua
// `read_process_output` / `stop_process` tools read or terminate
// running entries. Read by the statusline indicator and cleared on
// session reset.

static NEXT_PROC_ID: AtomicU32 = AtomicU32::new(1);

/// Maximum number of output lines retained per background process.
/// Older lines are dropped once this limit is reached.
const MAX_LINES: usize = 10_000;

struct Process {
    lines: Vec<String>,
    read_cursor: usize,
    finished: bool,
    exit_code: Option<i32>,
    command: String,
    started_at: Instant,
    /// Sends SIGKILL to the child process.
    kill_tx: Option<mpsc::Sender<()>>,
}

/// Info about a running background process, returned by
/// [`ProcessRegistry::list`].
pub struct ProcessInfo {
    pub id: String,
    pub command: String,
    pub started_at: Instant,
}

impl Process {
    fn push_line(&mut self, line: String) {
        self.lines.push(line);
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            self.lines.drain(..drop);
            self.read_cursor = self.read_cursor.saturating_sub(drop);
        }
    }
}

/// Shared registry of background processes.
#[derive(Clone)]
pub struct ProcessRegistry(Arc<Mutex<HashMap<String, Process>>>);

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn a background process. Output is accumulated internally; a
    /// background tokio task reads stdout/stderr and marks the process
    /// finished when it exits.
    pub fn spawn(
        &self,
        id: String,
        command: &str,
        mut child: tokio::process::Child,
        done_tx: mpsc::UnboundedSender<(String, Option<i32>)>,
    ) {
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let (kill_tx, mut kill_rx) = mpsc::channel::<()>(1);

        {
            let mut map = self.0.lock().unwrap();
            map.insert(
                id.clone(),
                Process {
                    lines: Vec::new(),
                    read_cursor: 0,
                    finished: false,
                    exit_code: None,
                    command: command.to_string(),
                    started_at: Instant::now(),
                    kill_tx: Some(kill_tx),
                },
            );
        }

        let registry = self.0.clone();
        let id2 = id.clone();
        tokio::spawn(async move {
            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut stderr_reader = BufReader::new(stderr).lines();
            let mut stdout_done = false;
            let mut stderr_done = false;

            loop {
                if stdout_done && stderr_done {
                    break;
                }
                tokio::select! {
                    line = stdout_reader.next_line(), if !stdout_done => {
                        match line {
                            Ok(Some(line)) => {
                                let mut map = registry.lock().unwrap();
                                if let Some(p) = map.get_mut(&id2) {
                                    p.push_line(line);
                                }
                            }
                            _ => stdout_done = true,
                        }
                    }
                    line = stderr_reader.next_line(), if !stderr_done => {
                        match line {
                            Ok(Some(line)) => {
                                let mut map = registry.lock().unwrap();
                                if let Some(p) = map.get_mut(&id2) {
                                    p.push_line(line);
                                }
                            }
                            _ => stderr_done = true,
                        }
                    }
                    _ = kill_rx.recv() => {
                        kill_group_sigkill(&child);
                        break;
                    }
                }
            }

            let status = child.wait().await;
            let code = status.ok().and_then(|s| s.code());
            {
                let mut map = registry.lock().unwrap();
                if let Some(p) = map.get_mut(&id2) {
                    p.finished = true;
                    p.exit_code = code;
                    p.kill_tx = None;
                }
            }
            let _ = done_tx.send((id2, code));
        });
    }

    /// Read new output since the last read. Returns (new_lines, running, exit_code).
    pub fn read(&self, id: &str) -> Result<(String, bool, Option<i32>), String> {
        let mut map = self.0.lock().unwrap();
        let p = map
            .get_mut(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;
        let output = std::mem::take(&mut p.lines).join("\n");
        p.read_cursor = 0;
        let running = !p.finished;
        let exit_code = p.exit_code;
        if p.finished {
            map.remove(id);
        }
        Ok((output, running, exit_code))
    }

    /// Stop a background process. Returns its final accumulated output.
    pub async fn stop(&self, id: &str) -> Result<String, String> {
        let kill_tx = {
            let mut map = self.0.lock().unwrap();
            let p = map
                .get_mut(id)
                .ok_or_else(|| format!("no process with id '{id}'"))?;
            p.kill_tx.take()
        };
        if let Some(tx) = kill_tx {
            let _ = tx.try_send(());
        }
        // Poll until the background task marks the process finished,
        // rather than a blind sleep that may be too short or too long.
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let map = self.0.lock().unwrap();
            if map.get(id).is_some_and(|p| p.finished) {
                break;
            }
        }
        let mut map = self.0.lock().unwrap();
        let p = map
            .remove(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;
        Ok(p.lines.join("\n"))
    }

    pub fn next_id(&self) -> String {
        let n = NEXT_PROC_ID.fetch_add(1, Ordering::Relaxed);
        format!("proc_{n}")
    }

    /// Number of currently running processes.
    pub fn running_count(&self) -> usize {
        let map = self.0.lock().unwrap();
        map.values().filter(|p| !p.finished).count()
    }

    /// List running background processes.
    pub fn list(&self) -> Vec<ProcessInfo> {
        let map = self.0.lock().unwrap();
        let mut procs: Vec<ProcessInfo> = map
            .iter()
            .filter(|(_, p)| !p.finished)
            .map(|(id, p)| ProcessInfo {
                id: id.clone(),
                command: p.command.clone(),
                started_at: p.started_at,
            })
            .collect();
        procs.sort_by(|a, b| a.id.cmp(&b.id));
        procs
    }

    /// Kill all running processes and remove all entries.
    pub fn clear(&self) {
        let mut map = self.0.lock().unwrap();
        for p in map.values_mut() {
            if let Some(tx) = p.kill_tx.take() {
                let _ = tx.try_send(());
            }
        }
        map.clear();
    }
}

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
