//! Process capability - async spawn-and-wait (`run_async`) and async
//! streaming (`run_streaming`) primitives. `ProcessRegistry` manages
//! long-lived background children (`spawn_bg`, `read_output`, `stop`).

use crate::output_limit::{
    limit_text_tail_with_max_bytes, OutputLimiter, DEFAULT_MAX_BYTES, TRUNCATION_NOTICE,
};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

/// Defaults: 30s timeout, inherit env, no stdin, capture stdout+stderr.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) cwd: PathBuf,
    pub(crate) env: HashMap<String, String>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) stdin: Option<String>,
    pub(crate) max_output_bytes: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct Output {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
    pub(crate) timed_out: bool,
}

const DEFAULT_NONINTERACTIVE_ENV: &[(&str, &str)] = &[
    ("GIT_EDITOR", "true"),
    ("GIT_SEQUENCE_EDITOR", "true"),
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
];

fn apply_default_noninteractive_env(command: &mut std::process::Command) {
    for (key, value) in DEFAULT_NONINTERACTIVE_ENV {
        command.env(key, value);
    }
}

/// Spawn `cmd` with `args` and wait for completion, honoring a
/// `CancellationToken`. The caller can short-circuit a long-running
/// child by cancelling the token - the child starts in its own session, so
/// it has no controlling terminal and its process group can receive SIGTERM
/// (then SIGKILL on the standard escalation). The future resolves with
/// `RunOutcome::Cancelled` once the wait completes.
/// Stdout/stderr are read concurrently so the child can't deadlock on
/// a full pipe.
pub(crate) async fn run_async(
    cmd: &str,
    args: &[String],
    opts: &Options,
    cancel: CancellationToken,
) -> io::Result<RunOutcome> {
    use tokio::io::AsyncWriteExt;

    let mut command = tokio::process::Command::new(cmd);
    command.args(args).current_dir(&opts.cwd);
    apply_default_noninteractive_env(command.as_std_mut());
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
    configure_child_session(&mut command);

    let mut child = command.spawn()?;

    if let (Some(text), Some(mut stdin)) = (opts.stdin.as_ref(), child.stdin.take()) {
        let _ = stdin.write_all(text.as_bytes()).await;
        // `stdin` drops here, closing the pipe so the child sees EOF.
    }

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let max_output_bytes = opts
        .max_output_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .min(MAX_OUTPUT_BYTES);
    let stdout_task =
        tokio::spawn(async move { read_output_tail(&mut stdout, max_output_bytes).await });
    let stderr_task =
        tokio::spawn(async move { read_output_tail(&mut stderr, max_output_bytes).await });

    let timeout = opts.timeout.unwrap_or(Duration::from_secs(30));
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            kill_process_group(&child);
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            Ok(RunOutcome::Cancelled)
        }
        _ = &mut deadline => {
            kill_process_group(&child);
            let _ = child.wait().await;
            let stdout_buf = stdout_task.await.unwrap_or_default();
            let stderr_buf = stderr_task.await.unwrap_or_default();
            let stderr_msg = if stderr_buf.is_empty() {
                format!("process timed out after {}s", timeout.as_secs())
            } else {
                stderr_buf
            };
            Ok(RunOutcome::Done(Output {
                stdout: stdout_buf,
                stderr: stderr_msg,
                exit_code: -1,
                timed_out: true,
            }))
        }
        status = child.wait() => {
            let stdout_buf = stdout_task.await.unwrap_or_default();
            let stderr_buf = stderr_task.await.unwrap_or_default();
            Ok(RunOutcome::Done(Output {
                stdout: stdout_buf,
                stderr: stderr_buf,
                exit_code: status?.code().unwrap_or(-1),
                timed_out: false,
            }))
        }
    }
}

async fn read_output_tail<R>(reader: &mut R, max_bytes: usize) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut retained = VecDeque::new();
    let mut total_bytes = 0usize;
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        total_bytes = total_bytes.saturating_add(n);
        retained.extend(&buf[..n]);
        while retained.len() > max_bytes {
            retained.pop_front();
        }
    }

    let retained_bytes = retained.len();
    let bytes: Vec<u8> = retained.into_iter().collect();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    if total_bytes <= retained_bytes {
        return limit_text_tail_with_max_bytes(&body, max_bytes);
    }

    let body = limit_text_tail_with_max_bytes(&body, max_bytes);
    format!("{TRUNCATION_NOTICE}: last {retained_bytes} of {total_bytes} bytes\n\n{body}")
}

/// Result of [`run_async`]: `Done` for natural completion or timeout,
/// `Cancelled` when the cancellation token fired and the child was
/// killed before producing a status.
pub(crate) enum RunOutcome {
    Done(Output),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct StreamOutput {
    /// stdout + stderr lines interleaved in arrival order, joined by '\n'.
    pub content: String,
    pub is_error: bool,
    pub timed_out: bool,
    pub background_id: Option<String>,
}

#[derive(Clone)]
pub struct StreamDetach {
    pub registry: ProcessRegistry,
    pub command: String,
    pub now: Instant,
}

pub struct StreamConfig {
    pub timeout: Duration,
    pub shell: ShellSpec,
    pub cwd: PathBuf,
    pub cancel: Option<CancellationToken>,
    pub detach: Option<StreamDetach>,
    pub detach_on_timeout: bool,
    pub manual_detach: bool,
}

/// Shell used to run a string-form command (`sh -c <cmd>` by default).
/// Pass `Some(("/bin/zsh", &["-fc"]))` to swap the shell wholesale.
#[derive(Clone, Debug)]
pub struct ShellSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl Default for ShellSpec {
    fn default() -> Self {
        Self {
            program: "sh".into(),
            args: vec!["-c".into()],
        }
    }
}

/// Configure a command so its child has no controlling terminal on Unix.
///
/// The child starts as a new session leader. Interactive programs that try to
/// open `/dev/tty` fail instead of taking over Smelt's terminal while the tool
/// waits on captured pipes. This is a no-op on non-Unix platforms.
pub fn without_controlling_terminal(command: &mut std::process::Command) {
    configure_without_controlling_terminal(command);
}

#[cfg(unix)]
fn configure_without_controlling_terminal(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `pre_exec` runs in the forked child before exec. The closure only
    // calls async-signal-safe `setsid` and builds an `io::Error` on failure, then
    // returns to the standard spawn path.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_without_controlling_terminal(_command: &mut std::process::Command) {}

fn configure_child_session(command: &mut tokio::process::Command) {
    without_controlling_terminal(command.as_std_mut());
}

pub fn spawn_shell_child(command: &str, shell: &ShellSpec, cwd: &Path) -> io::Result<Child> {
    let mut cmd = tokio::process::Command::new(&shell.program);
    for a in &shell.args {
        cmd.arg(a);
    }
    cmd.arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_default_noninteractive_env(cmd.as_std_mut());
    configure_child_session(&mut cmd);
    cmd.spawn()
}

fn detach_streaming_child(
    detach: StreamDetach,
    child: Child,
    stdout_reader: Lines<BufReader<ChildStdout>>,
    stderr_reader: Lines<BufReader<ChildStderr>>,
    output: OutputLimiter,
) -> String {
    detach.registry.adopt_streaming(
        &detach.command,
        child,
        stdout_reader,
        stderr_reader,
        detach.now,
        output,
    )
}

/// Spawn `<shell> <args...> command`, stream lines through `on_line`, return
/// aggregated output once the child exits or the timeout expires. Child runs
/// in its own session so it has no controlling terminal, and its process group
/// can be signalled on cancel/timeout.
/// `shell` is the wrapping program (default `sh -c`); callers swap it to e.g.
/// `("/bin/zsh", &["-fc"])` to run user-shell commands.
pub async fn run_streaming_with_shell(
    command: &str,
    config: StreamConfig,
    mut on_line: impl FnMut(String),
) -> StreamOutput {
    if config.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
        return StreamOutput {
            content: "cancelled".to_string(),
            is_error: true,
            timed_out: false,
            background_id: None,
        };
    }

    let mut child = match spawn_shell_child(command, &config.shell, &config.cwd) {
        Ok(c) => c,
        Err(e) => {
            return StreamOutput {
                content: e.to_string(),
                is_error: true,
                timed_out: false,
                background_id: None,
            };
        }
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();
    let mut output = OutputLimiter::default();
    let mut stdout_done = false;
    let mut stderr_done = false;

    let deadline = tokio::time::sleep(config.timeout);
    tokio::pin!(deadline);

    let mut foreground = if config.manual_detach {
        config
            .detach
            .as_ref()
            .map(|detach| detach.registry.register_foreground_stream())
    } else {
        None
    };

    enum StreamTick {
        Cancel,
        Control(Option<StreamControl>),
        Stdout(io::Result<Option<String>>),
        Stderr(io::Result<Option<String>>),
        Timeout,
        Exit(io::Result<std::process::ExitStatus>),
    }

    loop {
        let tick = tokio::select! {
            biased;
            _ = async {
                if let Some(cancel) = config.cancel.as_ref() {
                    cancel.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => StreamTick::Cancel,
            control = async {
                if let Some(registration) = foreground.as_mut() {
                    registration.recv_control().await
                } else {
                    std::future::pending::<Option<StreamControl>>().await
                }
            } => StreamTick::Control(control),
            line = stdout_reader.next_line(), if !stdout_done => StreamTick::Stdout(line),
            line = stderr_reader.next_line(), if !stderr_done => StreamTick::Stderr(line),
            _ = &mut deadline => StreamTick::Timeout,
            status = child.wait(), if stdout_done && stderr_done => StreamTick::Exit(status),
        };

        match tick {
            StreamTick::Cancel => {
                kill_process_group(&child);
                let content = "cancelled".to_string();
                on_line(content.clone());
                return StreamOutput {
                    content,
                    is_error: true,
                    timed_out: false,
                    background_id: None,
                };
            }
            StreamTick::Control(Some(StreamControl::Detach)) => {
                if let Some(detach) = config.detach.clone() {
                    let id =
                        detach_streaming_child(detach, child, stdout_reader, stderr_reader, output);
                    let content = format!(
                        "moved to background as {id}, you'll be notified when it completes"
                    );
                    on_line(content.clone());
                    return StreamOutput {
                        content,
                        is_error: false,
                        timed_out: false,
                        background_id: Some(id),
                    };
                }
            }
            StreamTick::Control(None) => {
                foreground = None;
            }
            StreamTick::Stdout(line) => match line {
                Ok(Some(line)) => {
                    on_line(line.clone());
                    output.push_line(line);
                }
                _ => stdout_done = true,
            },
            StreamTick::Stderr(line) => match line {
                Ok(Some(line)) => {
                    on_line(line.clone());
                    output.push_line(line);
                }
                _ => stderr_done = true,
            },
            StreamTick::Timeout => {
                if config.detach_on_timeout {
                    if let Some(detach) = config.detach.clone() {
                        let id = detach_streaming_child(
                            detach,
                            child,
                            stdout_reader,
                            stderr_reader,
                            output,
                        );
                        let content = format!(
                            "timed out after {:.0}s; moved to background as {id}, you'll be notified when it completes",
                            config.timeout.as_secs_f64()
                        );
                        on_line(content.clone());
                        return StreamOutput {
                            content,
                            is_error: false,
                            timed_out: true,
                            background_id: Some(id),
                        };
                    }
                }
                kill_process_group(&child);
                let content = format!("timed out after {:.0}s", config.timeout.as_secs_f64());
                on_line(content.clone());
                return StreamOutput {
                    content,
                    is_error: true,
                    timed_out: true,
                    background_id: None,
                };
            }
            StreamTick::Exit(status) => {
                drop(foreground);
                let is_error = status.map(|s| !s.success()).unwrap_or(true);
                return StreamOutput {
                    content: output.format_text(),
                    is_error,
                    timed_out: false,
                    background_id: None,
                };
            }
        }
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

/// Send SIGKILL to the process group whose id is the child's pid.
///
/// Children spawned through [`without_controlling_terminal`] are session leaders,
/// so their pid is also their process group id. No-op on non-Unix platforms.
pub fn kill_child_process_group_sigkill(child: &tokio::process::Child) {
    kill_group_sigkill(child);
}

/// SIGKILL variant used by the process registry stop path (skips SIGTERM grace period).
#[cfg(unix)]
fn kill_group_pid_sigkill(pid: u32) {
    // SAFETY: background children start in their own session, whose session
    // leader pid is also the process group id.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(unix)]
fn kill_group_sigkill(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        kill_group_pid_sigkill(pid);
    }
}

#[cfg(not(unix))]
fn kill_group_pid_sigkill(_pid: u32) {}

#[cfg(not(unix))]
fn kill_group_sigkill(_child: &tokio::process::Child) {}

// ── Background-process registry ──────────────────────────────────────────

static NEXT_PROC_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_FOREGROUND_ID: AtomicU64 = AtomicU64::new(1);

enum StreamControl {
    Detach,
}

struct ForegroundStream {
    id: u64,
    detach_tx: mpsc::UnboundedSender<StreamControl>,
}

struct ForegroundRegistration {
    registry: Arc<ProcessRegistryInner>,
    id: u64,
    detach_rx: mpsc::UnboundedReceiver<StreamControl>,
}

impl Drop for ForegroundRegistration {
    fn drop(&mut self) {
        if let Ok(mut streams) = self.registry.foreground.lock() {
            streams.remove(&self.id);
        }
    }
}

impl ForegroundRegistration {
    async fn recv_control(&mut self) -> Option<StreamControl> {
        self.detach_rx.recv().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachForegroundResult {
    Requested,
    NoForegroundProcess,
}

impl DetachForegroundResult {
    pub fn requested(self) -> bool {
        matches!(self, Self::Requested)
    }
}

struct Process {
    pid: Option<u32>,
    output: OutputLimiter,
    finished: bool,
    exit_code: Option<i32>,
    command: String,
    started_at: Instant,
    finished_at: Option<Instant>,
    kill_tx: Option<mpsc::Sender<()>>,
    finished_rx: watch::Receiver<bool>,
    suppress_notify: bool,
}

#[derive(Debug, Clone)]
pub struct ProcessCompletion {
    pub id: String,
    pub exit_code: Option<i32>,
}

struct ProcessRegistryInner {
    processes: Mutex<HashMap<String, Process>>,
    foreground: Mutex<HashMap<u64, ForegroundStream>>,
    completion_tx: Mutex<Option<mpsc::UnboundedSender<ProcessCompletion>>>,
}

struct AdoptedChild {
    id: String,
    command: String,
    child: Child,
    stdout_reader: Lines<BufReader<ChildStdout>>,
    stderr_reader: Lines<BufReader<ChildStderr>>,
    started_at: Instant,
    initial_output: OutputLimiter,
}

pub struct ProcessInfo {
    pub id: String,
    pub pid: Option<u32>,
    pub command: String,
    pub started_at: Instant,
}

#[derive(Debug)]
pub struct ProcessOutput {
    pub text: String,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub elapsed_secs: u64,
    pub pid: Option<u32>,
}

impl Process {
    fn elapsed_secs(&self) -> u64 {
        self.finished_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.started_at)
            .as_secs()
    }
}

#[derive(Clone)]
pub struct ProcessRegistry(Arc<ProcessRegistryInner>);

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self(Arc::new(ProcessRegistryInner {
            processes: Mutex::new(HashMap::new()),
            foreground: Mutex::new(HashMap::new()),
            completion_tx: Mutex::new(None),
        }))
    }
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_completion_sender(&self, tx: mpsc::UnboundedSender<ProcessCompletion>) {
        *self.0.completion_tx.lock().unwrap() = Some(tx);
    }

    fn register_foreground_stream(&self) -> ForegroundRegistration {
        let id = NEXT_FOREGROUND_ID.fetch_add(1, Ordering::Relaxed);
        let (detach_tx, detach_rx) = mpsc::unbounded_channel();
        let stream = ForegroundStream { id, detach_tx };
        self.0.foreground.lock().unwrap().insert(id, stream);
        ForegroundRegistration {
            registry: self.0.clone(),
            id,
            detach_rx,
        }
    }

    pub fn detach_latest_foreground(&self) -> DetachForegroundResult {
        let stream = {
            let mut streams = self.0.foreground.lock().unwrap();
            loop {
                let Some(id) = streams.values().map(|stream| stream.id).max() else {
                    return DetachForegroundResult::NoForegroundProcess;
                };
                let Some(stream) = streams.get(&id) else {
                    continue;
                };
                if stream.detach_tx.is_closed() {
                    streams.remove(&id);
                    continue;
                }
                break stream.detach_tx.clone();
            }
        };

        if stream.send(StreamControl::Detach).is_ok() {
            DetachForegroundResult::Requested
        } else {
            DetachForegroundResult::NoForegroundProcess
        }
    }

    pub fn child_id(&self, child: &Child) -> String {
        child
            .id()
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| self.next_id())
    }

    pub fn spawn(&self, id: String, command: &str, mut child: Child, now: Instant) {
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdout_reader = BufReader::new(stdout).lines();
        let stderr_reader = BufReader::new(stderr).lines();
        self.adopt_readers(AdoptedChild {
            id,
            command: command.to_string(),
            child,
            stdout_reader,
            stderr_reader,
            started_at: now,
            initial_output: OutputLimiter::default(),
        });
    }

    fn adopt_streaming(
        &self,
        command: &str,
        child: Child,
        stdout_reader: Lines<BufReader<ChildStdout>>,
        stderr_reader: Lines<BufReader<ChildStderr>>,
        now: Instant,
        initial_output: OutputLimiter,
    ) -> String {
        let id = self.child_id(&child);
        self.adopt_readers(AdoptedChild {
            id: id.clone(),
            command: command.to_string(),
            child,
            stdout_reader,
            stderr_reader,
            started_at: now,
            initial_output,
        });
        id
    }

    fn adopt_readers(&self, adopted: AdoptedChild) {
        let AdoptedChild {
            id,
            command,
            mut child,
            mut stdout_reader,
            mut stderr_reader,
            started_at,
            initial_output,
        } = adopted;
        let (kill_tx, mut kill_rx) = mpsc::channel::<()>(1);
        let (finished_tx, finished_rx) = watch::channel(false);
        let pid = child.id();

        {
            let process = Process {
                pid,
                output: initial_output,
                finished: false,
                exit_code: None,
                command,
                started_at,
                finished_at: None,
                kill_tx: Some(kill_tx),
                finished_rx,
                suppress_notify: false,
            };
            let mut map = self.0.processes.lock().unwrap();
            map.insert(id.clone(), process);
        }

        let registry = self.0.clone();
        tokio::spawn(async move {
            let mut stdout_done = false;
            let mut stderr_done = false;
            let mut child_done = false;
            let mut completion_sent = false;

            let mut mark_finished = |code: Option<i32>| {
                if completion_sent {
                    return;
                }
                completion_sent = true;
                let should_notify = {
                    let mut map = registry.processes.lock().unwrap();
                    if let Some(p) = map.get_mut(&id) {
                        p.finished = true;
                        p.finished_at = Some(Instant::now());
                        p.exit_code = code;
                        p.kill_tx = None;
                        !p.suppress_notify
                    } else {
                        false
                    }
                };
                if should_notify {
                    if let Some(tx) = registry.completion_tx.lock().unwrap().clone() {
                        let _ = tx.send(ProcessCompletion {
                            id: id.clone(),
                            exit_code: code,
                        });
                    }
                }
                let _ = finished_tx.send(true);
            };

            loop {
                if stdout_done && stderr_done {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = kill_rx.recv(), if !child_done => {
                        kill_group_sigkill(&child);
                        let code = child.wait().await.ok().and_then(|s| s.code());
                        child_done = true;
                        mark_finished(code);
                    }
                    line = stdout_reader.next_line(), if !stdout_done => {
                        match line {
                            Ok(Some(line)) => {
                                let mut map = registry.processes.lock().unwrap();
                                if let Some(p) = map.get_mut(&id) {
                                    p.output.push_line(line);
                                }
                            }
                            _ => stdout_done = true,
                        }
                    }
                    line = stderr_reader.next_line(), if !stderr_done => {
                        match line {
                            Ok(Some(line)) => {
                                let mut map = registry.processes.lock().unwrap();
                                if let Some(p) = map.get_mut(&id) {
                                    p.output.push_line(line);
                                }
                            }
                            _ => stderr_done = true,
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)), if !child_done => {
                        if let Ok(Some(status)) = child.try_wait() {
                            child_done = true;
                            mark_finished(status.code());
                        }
                    }
                }
            }

            if !child_done {
                let code = child.wait().await.ok().and_then(|s| s.code());
                mark_finished(code);
            }
        });
    }

    /// Drains buffered output and removes finished processes.
    pub fn drain_output(&self, id: &str) -> Result<ProcessOutput, String> {
        let mut map = self.0.processes.lock().unwrap();
        let p = map
            .get_mut(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;
        let output = p.output.drain_text();
        let running = !p.finished;
        let finished = p.finished;
        let exit_code = p.exit_code;
        let elapsed_secs = p.elapsed_secs();
        let pid = p.pid;
        if finished {
            map.remove(id);
        }
        Ok(ProcessOutput {
            pid,
            text: output,
            running,
            exit_code,
            elapsed_secs,
        })
    }

    /// Returns buffered output without draining or removing the process.
    pub fn snapshot_output(&self, id: &str) -> Result<ProcessOutput, String> {
        let map = self.0.processes.lock().unwrap();
        let p = map
            .get(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;
        Ok(ProcessOutput {
            pid: p.pid,
            text: p.output.format_text(),
            running: !p.finished,
            exit_code: p.exit_code,
            elapsed_secs: p.elapsed_secs(),
        })
    }

    pub async fn stop(&self, id: &str) -> Result<String, String> {
        let (kill_tx, mut finished_rx) = {
            let mut map = self.0.processes.lock().unwrap();
            let p = map
                .get_mut(id)
                .ok_or_else(|| format!("no process with id '{id}'"))?;
            p.suppress_notify = true;
            (p.kill_tx.take(), p.finished_rx.clone())
        };
        if let Some(tx) = kill_tx {
            let _ = tx.try_send(());
        }
        if !*finished_rx.borrow() {
            let _ = tokio::time::timeout(Duration::from_secs(2), finished_rx.changed()).await;
        }
        let mut map = self.0.processes.lock().unwrap();
        let p = map
            .get_mut(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;
        p.finished = true;
        p.finished_at = Some(Instant::now());
        p.kill_tx = None;
        Ok(p.output.format_text())
    }

    pub fn next_id(&self) -> String {
        let n = NEXT_PROC_ID.fetch_add(1, Ordering::Relaxed);
        format!("proc_{n}")
    }

    pub fn running_count(&self) -> usize {
        let map = self.0.processes.lock().unwrap();
        map.values().filter(|p| !p.finished).count()
    }

    pub fn list(&self) -> Vec<ProcessInfo> {
        let map = self.0.processes.lock().unwrap();
        let mut procs: Vec<ProcessInfo> = map
            .iter()
            .filter(|(_, p)| !p.finished)
            .map(|(id, p)| ProcessInfo {
                id: id.clone(),
                pid: p.pid,
                command: p.command.clone(),
                started_at: p.started_at,
            })
            .collect();
        procs.sort_by(|a, b| a.id.cmp(&b.id));
        procs
    }

    pub fn clear(&self) {
        self.0.foreground.lock().unwrap().clear();
        let mut map = self.0.processes.lock().unwrap();
        for p in map.values_mut() {
            p.suppress_notify = true;
            if let Some(pid) = p.pid {
                kill_group_pid_sigkill(pid);
            }
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

    fn test_cwd() -> PathBuf {
        std::env::current_dir().expect("test process has a current directory")
    }

    fn test_options() -> Options {
        Options {
            cwd: test_cwd(),
            env: HashMap::new(),
            timeout: None,
            stdin: None,
            max_output_bytes: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn renderer_contract_round_trips_through_a_real_process() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("renderer");
        std::fs::write(
            &path,
            "#!/bin/sh\npayload=$(cat)\nprintf '%s' \"$payload\" >&2\nprintf '%s' '{\"status\":200,\"final_url\":\"https://example.com/final\",\"html\":\"'\nhead -c 200000 /dev/zero | tr '\\000' x\nprintf '%s' '\",\"truncated\":false}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let request =
            r#"{"url":"https://example.com","timeout_ms":30000,"max_response_bytes":5242880}"#;

        let outcome = run_async(
            path.to_str().unwrap(),
            &[],
            &Options {
                stdin: Some(request.into()),
                max_output_bytes: Some(300_000),
                ..test_options()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let RunOutcome::Done(output) = outcome else {
            panic!("renderer was unexpectedly cancelled");
        };
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stderr, request);
        let response: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
        assert_eq!(response["final_url"], "https://example.com/final");
        assert_eq!(response["html"].as_str().unwrap().len(), 200_000);
    }

    fn finished_rx(finished: bool) -> watch::Receiver<bool> {
        watch::channel(finished).1
    }

    fn process_fixture(
        lines: Vec<&str>,
        finished: bool,
        exit_code: Option<i32>,
        started_at: Instant,
        finished_at: Option<Instant>,
    ) -> Process {
        let mut output = OutputLimiter::default();
        for line in lines {
            output.push_line(line.to_string());
        }
        Process {
            pid: None,
            output,
            finished,
            exit_code,
            command: "cmd".into(),
            started_at,
            finished_at,
            kill_tx: None,
            finished_rx: finished_rx(finished),
            suppress_notify: false,
        }
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[cfg(unix)]
    async fn wait_for_process_exit(pid: u32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while process_exists(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "process {pid} was still alive after registry clear"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn run_git(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
        let out = std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .expect("git command spawns");
        assert!(
            out.status.success(),
            "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn shell_quote_path(path: &std::path::Path) -> String {
        let text = path.to_string_lossy();
        format!("'{}'", text.replace('\'', "'\\''"))
    }

    async fn wait_for_snapshot(
        registry: &ProcessRegistry,
        id: &str,
        mut predicate: impl FnMut(&ProcessOutput) -> bool,
    ) -> ProcessOutput {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = registry.snapshot_output(id).unwrap();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for process {id}; last snapshot: {snapshot:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn run(cmd: &str, args: &[&str], opts: &Options) -> Output {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match run_async(cmd, &args, opts, CancellationToken::new())
            .await
            .unwrap()
        {
            RunOutcome::Done(out) => out,
            RunOutcome::Cancelled => panic!("unexpected cancellation"),
        }
    }

    #[tokio::test]
    async fn run_echo_captures_stdout() {
        let out = run("sh", &["-c", "echo hello"], &test_options()).await;
        assert!(out.stdout.contains("hello"));
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn run_propagates_exit_code() {
        let out = run("sh", &["-c", "exit 42"], &test_options()).await;
        assert_eq!(out.exit_code, 42);
    }

    #[tokio::test]
    async fn run_pipes_stdin_to_child() {
        let opts = Options {
            stdin: Some("hello world".into()),
            ..test_options()
        };
        let out = run("cat", &[], &opts).await;
        assert_eq!(out.stdout, "hello world");
    }

    #[tokio::test]
    async fn run_honors_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = Options {
            cwd: tmp.path().to_path_buf(),
            ..test_options()
        };
        let out = run("pwd", &[], &opts).await;
        assert!(out.stdout.contains(tmp.path().to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn run_times_out_long_command() {
        let opts = Options {
            timeout: Some(Duration::from_millis(100)),
            ..test_options()
        };
        let out = run("sh", &["-c", "sleep 5"], &opts).await;
        assert!(out.timed_out);
        assert_eq!(out.exit_code, -1);
    }

    #[tokio::test]
    async fn streaming_lines_reconstruct_the_terminal_result() {
        let mut lines = Vec::new();
        let out = run_streaming_with_shell(
            "printf 'first\\nβeta\\n'",
            StreamConfig {
                timeout: Duration::from_secs(2),
                shell: ShellSpec::default(),
                cwd: test_cwd(),
                cancel: None,
                detach: None,
                detach_on_timeout: false,
                manual_detach: false,
            },
            |line| lines.push(line),
        )
        .await;

        assert!(!out.is_error);
        assert!(!out.timed_out);
        assert_eq!(lines, ["first", "βeta"]);
        assert_eq!(lines.join("\n"), out.content);
    }

    #[tokio::test]
    async fn streaming_timeout_can_detach_to_registry() {
        let registry = ProcessRegistry::new();
        let mut lines = Vec::new();
        let out = run_streaming_with_shell(
            "echo start; sleep 5",
            StreamConfig {
                timeout: Duration::from_millis(100),
                shell: ShellSpec::default(),
                cwd: test_cwd(),
                cancel: None,
                detach: Some(StreamDetach {
                    registry: registry.clone(),
                    command: "echo start; sleep 5".into(),
                    now: Instant::now(),
                }),
                detach_on_timeout: true,
                manual_detach: false,
            },
            |line| lines.push(line),
        )
        .await;

        let id = out.background_id.expect("detached process id");
        assert!(out.timed_out);
        assert!(!out.is_error);
        assert!(out.content.contains("you'll be notified when it completes"));
        assert_eq!(lines.first().map(String::as_str), Some("start"));
        assert_eq!(lines.last().map(String::as_str), Some(out.content.as_str()));
        assert_eq!(registry.running_count(), 1);
        let snapshot = registry.snapshot_output(&id).unwrap();
        assert!(snapshot.running);
        assert!(snapshot.text.contains("start"));
        let _ = registry.stop(&id).await;
    }

    #[tokio::test]
    async fn streaming_timeout_still_applies_after_output_closes() {
        let out = tokio::time::timeout(
            Duration::from_secs(2),
            run_streaming_with_shell(
                "exec >/dev/null 2>/dev/null; sleep 5",
                StreamConfig {
                    timeout: Duration::from_millis(100),
                    shell: ShellSpec::default(),
                    cwd: test_cwd(),
                    cancel: None,
                    detach: None,
                    detach_on_timeout: false,
                    manual_detach: false,
                },
                |_| {},
            ),
        )
        .await
        .expect("streaming timeout should not wait for child exit");

        assert!(out.timed_out);
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn streaming_foreground_can_detach_to_registry() {
        let registry = ProcessRegistry::new();
        let detach = StreamDetach {
            registry: registry.clone(),
            command: "echo start; sleep 5".into(),
            now: Instant::now(),
        };
        let handle = tokio::spawn(async move {
            run_streaming_with_shell(
                "echo start; sleep 5",
                StreamConfig {
                    timeout: Duration::from_secs(5),
                    shell: ShellSpec::default(),
                    cwd: test_cwd(),
                    cancel: None,
                    detach: Some(detach),
                    detach_on_timeout: false,
                    manual_detach: true,
                },
                |_| {},
            )
            .await
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if registry.detach_latest_foreground().requested() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "streaming process never registered as detachable"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let out = handle.await.unwrap();
        let id = out.background_id.expect("detached process id");
        assert!(!out.timed_out);
        assert!(!out.is_error);
        assert_eq!(registry.running_count(), 1);

        let snapshot =
            wait_for_snapshot(&registry, &id, |snapshot| snapshot.text.contains("start")).await;
        assert!(snapshot.running);
        let _ = registry.stop(&id).await;
    }

    #[tokio::test]
    async fn registry_uses_child_pid_as_background_id() {
        let registry = ProcessRegistry::new();
        let child = spawn_shell_child("sleep 5", &ShellSpec::default(), &test_cwd()).unwrap();
        let pid = child.id().expect("spawned child has pid");
        let id = registry.child_id(&child);

        registry.spawn(id.clone(), "sleep 5", child, Instant::now());

        assert_eq!(id, pid.to_string());
        assert_eq!(registry.running_count(), 1);
        let _ = registry.stop(&id).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_child_cannot_open_controlling_terminal() {
        let out = run_streaming_with_shell(
            "cat /dev/tty",
            StreamConfig {
                timeout: Duration::from_secs(1),
                shell: ShellSpec::default(),
                cwd: test_cwd(),
                cancel: None,
                detach: None,
                detach_on_timeout: false,
                manual_detach: false,
            },
            |_| {},
        )
        .await;

        assert!(!out.timed_out, "child blocked on /dev/tty");
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn shell_child_uses_git_editor_noop_by_default() {
        if !git_available() {
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();
        run_git(repo, &["init", "-q", "-b", "main"]);
        run_git(repo, &["config", "user.name", "Smelt Test"]);
        run_git(repo, &["config", "user.email", "smelt@example.invalid"]);
        std::fs::write(repo.join("file.txt"), "base\n").unwrap();
        run_git(repo, &["add", "file.txt"]);
        run_git(repo, &["commit", "-qm", "base"]);

        run_git(repo, &["checkout", "-qb", "feature"]);
        std::fs::write(repo.join("file.txt"), "feature\n").unwrap();
        run_git(repo, &["commit", "-am", "feature"]);

        run_git(repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("file.txt"), "main\n").unwrap();
        run_git(repo, &["commit", "-am", "main"]);

        run_git(repo, &["checkout", "-q", "feature"]);
        let rebase = std::process::Command::new("git")
            .current_dir(repo)
            .args(["rebase", "main"])
            .output()
            .expect("git rebase spawns");
        assert!(!rebase.status.success(), "rebase should stop at a conflict");

        std::fs::write(repo.join("file.txt"), "resolved\n").unwrap();
        run_git(repo, &["add", "file.txt"]);

        let command = format!(
            "EDITOR='sh -c \"echo editor-ran >&2; exit 99\"' VISUAL='sh -c \"echo visual-ran >&2; exit 99\"' git -C {} rebase --continue",
            shell_quote_path(repo)
        );
        let out = run_streaming_with_shell(
            &command,
            StreamConfig {
                timeout: Duration::from_secs(5),
                shell: ShellSpec::default(),
                cwd: test_cwd(),
                cancel: None,
                detach: None,
                detach_on_timeout: false,
                manual_detach: false,
            },
            |_| {},
        )
        .await;

        assert!(
            !out.timed_out,
            "git rebase --continue timed out: {}",
            out.content
        );
        assert!(
            !out.is_error,
            "git rebase --continue failed: {}",
            out.content
        );
        assert!(!out.content.contains("editor-ran"));
        assert!(!out.content.contains("visual-ran"));
        let status = run_git(repo, &["status", "--porcelain"]);
        assert_eq!(String::from_utf8_lossy(&status.stdout), "");
    }

    #[tokio::test]
    async fn registry_reports_natural_completion_and_keeps_snapshot() {
        let registry = ProcessRegistry::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        registry.set_completion_sender(tx);
        let child = spawn_shell_child("echo done", &ShellSpec::default(), &test_cwd()).unwrap();
        let id = registry.child_id(&child);

        registry.spawn(id.clone(), "echo done", child, Instant::now());

        let completion = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completion.id, id);
        assert_eq!(completion.exit_code, Some(0));

        let snapshot = wait_for_snapshot(&registry, &id, |out| !out.running).await;
        assert!(!snapshot.running);
        assert_eq!(snapshot.exit_code, Some(0));
        assert!(snapshot.text.contains("done"));
        assert_eq!(registry.running_count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_detects_external_process_exit() {
        let registry = ProcessRegistry::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        registry.set_completion_sender(tx);
        let child =
            spawn_shell_child("echo ready; sleep 30", &ShellSpec::default(), &test_cwd()).unwrap();
        let id = registry.child_id(&child);

        registry.spawn(id.clone(), "echo ready; sleep 30", child, Instant::now());
        wait_for_snapshot(&registry, &id, |out| out.text.contains("ready")).await;

        // SAFETY: the registry spawned the child in its own process group whose
        // group id matches the pid-derived background id.
        unsafe {
            libc::kill(-(id.parse::<i32>().unwrap()), libc::SIGTERM);
        }

        let completion = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completion.id, id);
        assert_eq!(completion.exit_code, None);

        let snapshot = wait_for_snapshot(&registry, &id, |out| !out.running).await;
        assert!(!snapshot.running);
        assert_eq!(snapshot.exit_code, None);
        assert_eq!(registry.running_count(), 0);
    }

    #[tokio::test]
    async fn registry_stop_kills_process_and_returns_buffered_output() {
        let registry = ProcessRegistry::new();
        let child = spawn_shell_child("echo started; sleep 30", &ShellSpec::default(), &test_cwd())
            .unwrap();
        let id = registry.child_id(&child);

        registry.spawn(id.clone(), "echo started; sleep 30", child, Instant::now());
        wait_for_snapshot(&registry, &id, |out| out.text.contains("started")).await;

        let output = tokio::time::timeout(Duration::from_secs(2), registry.stop(&id))
            .await
            .unwrap()
            .unwrap();
        assert!(output.contains("started"));
        let snapshot = registry.snapshot_output(&id).unwrap();
        assert!(!snapshot.running);
        assert_eq!(snapshot.text, output);
        assert_eq!(registry.running_count(), 0);
    }

    #[test]
    fn process_elapsed_secs_freezes_at_finished_at() {
        let registry = ProcessRegistry::new();
        let started_at = Instant::now() - Duration::from_secs(10);
        {
            let mut map = registry.0.processes.lock().unwrap();
            map.insert(
                "done".into(),
                process_fixture(
                    Vec::new(),
                    true,
                    Some(0),
                    started_at,
                    Some(started_at + Duration::from_secs(3)),
                ),
            );
        }

        let first = registry.snapshot_output("done").unwrap().elapsed_secs;
        std::thread::sleep(Duration::from_millis(20));
        let second = registry.snapshot_output("done").unwrap().elapsed_secs;

        assert_eq!(first, 3);
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn run_passes_custom_env_to_child() {
        let mut env = HashMap::new();
        env.insert("SMELT_TEST_VAR".into(), "from_test".into());
        let opts = Options {
            env,
            ..test_options()
        };
        let out = run("sh", &["-c", "echo $SMELT_TEST_VAR"], &opts).await;
        assert!(out.stdout.contains("from_test"));
    }

    #[tokio::test]
    async fn run_sets_noninteractive_environment_defaults() {
        let out = run(
            "sh",
            &[
                "-c",
                "printf '%s' \"$GIT_EDITOR|$GIT_SEQUENCE_EDITOR|$GIT_PAGER|$PAGER|$EDITOR|$VISUAL\"",
            ],
            &test_options(),
        )
        .await;
        assert_eq!(out.stdout, "true|true|cat|cat|true|true");
    }

    #[tokio::test]
    async fn run_custom_env_overrides_noninteractive_defaults() {
        let mut env = HashMap::new();
        env.insert("GIT_EDITOR".into(), "custom-editor".into());
        let opts = Options {
            env,
            ..test_options()
        };
        let out = run("sh", &["-c", "printf '%s' \"$GIT_EDITOR\""], &opts).await;
        assert_eq!(out.stdout, "custom-editor");
    }

    #[tokio::test]
    async fn run_captures_stderr_separately() {
        let out = run("sh", &["-c", "echo err 1>&2"], &test_options()).await;
        assert_eq!(out.exit_code, 0);
        assert!(out.stderr.contains("err"));
        assert!(!out.stdout.contains("err"));
    }

    #[tokio::test]
    async fn run_returns_io_error_for_nonexistent_binary() {
        let args: Vec<String> = Vec::new();
        let result = run_async(
            "__definitely_no_such_command__",
            &args,
            &test_options(),
            CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
    }

    // ── ProcessRegistry ───────────────────────────────────────────────

    #[test]
    fn registry_new_and_default_yield_empty_registry() {
        let r1 = ProcessRegistry::new();
        let r2 = ProcessRegistry::default();
        assert_eq!(r1.running_count(), 0);
        assert_eq!(r2.running_count(), 0);
        assert!(r1.list().is_empty());
    }

    #[test]
    fn registry_next_id_is_monotonic_and_unique() {
        let r = ProcessRegistry::new();
        let id1 = r.next_id();
        let id2 = r.next_id();
        assert!(id1.starts_with("proc_"));
        assert!(id2.starts_with("proc_"));
        assert_ne!(id1, id2);
    }

    #[test]
    fn registry_read_unknown_id_returns_error() {
        let r = ProcessRegistry::new();
        let err = r.drain_output("no_such_proc").unwrap_err();
        assert!(err.contains("no_such_proc"));
    }

    #[tokio::test]
    async fn registry_stop_unknown_id_returns_error() {
        let r = ProcessRegistry::new();
        let err = r.stop("nope").await.unwrap_err();
        assert!(err.contains("nope"));
    }

    #[test]
    fn registry_clear_empties_running_count_and_list() {
        let r = ProcessRegistry::new();
        r.clear();
        assert_eq!(r.running_count(), 0);
        assert!(r.list().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_clear_kills_running_child_immediately() {
        let registry = ProcessRegistry::new();
        let child = spawn_shell_child("sleep 30", &ShellSpec::default(), &test_cwd()).unwrap();
        let pid = child.id().expect("spawned child has pid");
        let id = registry.child_id(&child);

        registry.spawn(id, "sleep 30", child, Instant::now());
        assert_eq!(registry.running_count(), 1);

        registry.clear();

        assert_eq!(registry.running_count(), 0);
        assert!(registry.list().is_empty());
        wait_for_process_exit(pid).await;
    }

    #[test]
    fn process_info_lists_running_processes_in_id_order() {
        // Insert synthetic entries directly via the lock so we don't spawn real children.
        let r = ProcessRegistry::new();
        {
            let mut map = r.0.processes.lock().unwrap();
            for i in ["proc_b", "proc_a", "proc_c"] {
                map.insert(
                    i.into(),
                    process_fixture(Vec::new(), false, None, Instant::now(), None),
                );
            }
        }
        let infos = r.list();
        let ids: Vec<&str> = infos.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["proc_a", "proc_b", "proc_c"]);
        assert_eq!(r.running_count(), 3);
    }

    #[test]
    fn process_list_filters_out_finished_entries() {
        let r = ProcessRegistry::new();
        {
            let mut map = r.0.processes.lock().unwrap();
            map.insert(
                "live".into(),
                process_fixture(Vec::new(), false, None, Instant::now(), None),
            );
            map.insert(
                "dead".into(),
                process_fixture(Vec::new(), true, Some(0), Instant::now(), None),
            );
        }
        let listing = r.list();
        let ids: Vec<&str> = listing.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["live"]);
        assert_eq!(r.running_count(), 1);
    }

    #[test]
    fn process_read_drains_lines_and_removes_finished_entry() {
        let r = ProcessRegistry::new();
        {
            let mut map = r.0.processes.lock().unwrap();
            map.insert(
                "p1".into(),
                process_fixture(vec!["a", "b"], true, Some(0), Instant::now(), None),
            );
        }
        let out = r.drain_output("p1").unwrap();
        assert_eq!(out.text, "a\nb");
        assert!(!out.running);
        assert_eq!(out.exit_code, Some(0));
        // Finished entry should be removed.
        assert!(r.drain_output("p1").is_err());
    }

    #[test]
    fn process_read_keeps_entry_when_still_running() {
        let r = ProcessRegistry::new();
        {
            let mut map = r.0.processes.lock().unwrap();
            map.insert(
                "p1".into(),
                process_fixture(vec!["a"], false, None, Instant::now(), None),
            );
        }
        let out = r.drain_output("p1").unwrap();
        assert_eq!(out.text, "a");
        assert!(out.running);
        assert_eq!(out.exit_code, None);
        // Entry is still registered with drained lines.
        let map = r.0.processes.lock().unwrap();
        assert!(map.get("p1").is_some());
        assert_eq!(map.get("p1").unwrap().output.retained_lines(), 0);
    }
}
