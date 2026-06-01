//! Process capability - async spawn-and-wait (`run_async`) and async
//! streaming (`run_streaming`) primitives. `ProcessRegistry` manages
//! long-lived background children (`spawn_bg`, `read_output`, `stop`).

use crate::output_limit::{limit_text_tail, OutputLimiter, DEFAULT_MAX_BYTES, TRUNCATION_NOTICE};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Defaults: 30s timeout, inherit env, no stdin, capture stdout+stderr.
#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    pub(crate) cwd: Option<String>,
    pub(crate) env: HashMap<String, String>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) stdin: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Output {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
    pub(crate) timed_out: bool,
}

/// Spawn `cmd` with `args` and wait for completion, honoring a
/// `CancellationToken`. The caller can short-circuit a long-running
/// child by cancelling the token - the child's process group receives
/// SIGTERM (then SIGKILL on the standard escalation) and the future
/// resolves with `RunOutcome::Cancelled` once the wait completes.
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
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn()?;

    if let (Some(text), Some(mut stdin)) = (opts.stdin.as_ref(), child.stdin.take()) {
        let _ = stdin.write_all(text.as_bytes()).await;
        // `stdin` drops here, closing the pipe so the child sees EOF.
    }

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let stdout_task = tokio::spawn(async move { read_output_tail(&mut stdout).await });
    let stderr_task = tokio::spawn(async move { read_output_tail(&mut stderr).await });

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

async fn read_output_tail<R>(reader: &mut R) -> String
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
        while retained.len() > DEFAULT_MAX_BYTES {
            retained.pop_front();
        }
    }

    let retained_bytes = retained.len();
    let bytes: Vec<u8> = retained.into_iter().collect();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    if total_bytes <= retained_bytes {
        return limit_text_tail(&body);
    }

    let body = limit_text_tail(&body);
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

pub struct StreamDetach {
    pub registry: ProcessRegistry,
    pub command: String,
    pub now: Instant,
}

pub struct StreamConfig {
    pub timeout: Duration,
    pub shell: ShellSpec,
    pub cancel: Option<CancellationToken>,
    pub detach_on_timeout: Option<StreamDetach>,
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

pub fn spawn_shell_child(command: &str, shell: &ShellSpec) -> io::Result<Child> {
    let mut cmd = tokio::process::Command::new(&shell.program);
    for a in &shell.args {
        cmd.arg(a);
    }
    cmd.arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.spawn()
}

/// Spawn `<shell> <args...> command`, stream lines through `on_line`, return
/// aggregated output once the child exits or the timeout expires. Child runs
/// in its own process group so the whole group can be signalled on cancel/timeout.
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

    let mut child = match spawn_shell_child(command, &config.shell) {
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

    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            biased;
            _ = async {
                if let Some(cancel) = config.cancel.as_ref() {
                    cancel.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                kill_process_group(&child);
                return StreamOutput {
                    content: "cancelled".to_string(),
                    is_error: true,
                    timed_out: false,
                    background_id: None,
                };
            }
            line = stdout_reader.next_line(), if !stdout_done => {
                match line {
                    Ok(Some(line)) => {
                        on_line(line.clone());
                        output.push_line(line);
                    }
                    _ => stdout_done = true,
                }
            }
            line = stderr_reader.next_line(), if !stderr_done => {
                match line {
                    Ok(Some(line)) => {
                        on_line(line.clone());
                        output.push_line(line);
                    }
                    _ => stderr_done = true,
                }
            }
            _ = &mut deadline => {
                if let Some(detach) = config.detach_on_timeout {
                    let id = detach.registry.adopt_streaming(
                        &detach.command,
                        child,
                        stdout_reader,
                        stderr_reader,
                        detach.now,
                        output,
                    );
                    return StreamOutput {
                        content: format!("timed out after {:.0}s; moved to background as {id}", config.timeout.as_secs_f64()),
                        is_error: false,
                        timed_out: true,
                        background_id: Some(id),
                    };
                }
                kill_process_group(&child);
                return StreamOutput {
                    content: format!("timed out after {:.0}s", config.timeout.as_secs_f64()),
                    is_error: true,
                    timed_out: true,
                    background_id: None,
                };
            }
        }
    }

    let status = child.wait().await;
    let is_error = status.map(|s| !s.success()).unwrap_or(true);
    StreamOutput {
        content: output.format_text(),
        is_error,
        timed_out: false,
        background_id: None,
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

/// SIGKILL variant used by the process registry stop path (skips SIGTERM grace period).
#[cfg(unix)]
fn kill_group_pid_sigkill(pid: u32) {
    // SAFETY: background children are spawned with process_group(0), so the
    // child's pid is also the process group id.
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
        let out = run("sh", &["-c", "echo hello"], &Options::default()).await;
        assert!(out.stdout.contains("hello"));
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn run_propagates_exit_code() {
        let out = run("sh", &["-c", "exit 42"], &Options::default()).await;
        assert_eq!(out.exit_code, 42);
    }

    #[tokio::test]
    async fn run_pipes_stdin_to_child() {
        let opts = Options {
            stdin: Some("hello world".into()),
            ..Default::default()
        };
        let out = run("cat", &[], &opts).await;
        assert_eq!(out.stdout, "hello world");
    }

    #[tokio::test]
    async fn run_honors_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let opts = Options {
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let out = run("pwd", &[], &opts).await;
        assert!(out.stdout.contains(tmp.path().to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn run_times_out_long_command() {
        let opts = Options {
            timeout: Some(Duration::from_millis(100)),
            ..Default::default()
        };
        let out = run("sh", &["-c", "sleep 5"], &opts).await;
        assert!(out.timed_out);
        assert_eq!(out.exit_code, -1);
    }

    #[tokio::test]
    async fn streaming_timeout_can_detach_to_registry() {
        let registry = ProcessRegistry::new();
        let out = run_streaming_with_shell(
            "echo start; sleep 5",
            StreamConfig {
                timeout: Duration::from_millis(100),
                shell: ShellSpec::default(),
                cancel: None,
                detach_on_timeout: Some(StreamDetach {
                    registry: registry.clone(),
                    command: "echo start; sleep 5".into(),
                    now: Instant::now(),
                }),
            },
            |_| {},
        )
        .await;

        let id = out.background_id.expect("detached process id");
        assert!(out.timed_out);
        assert!(!out.is_error);
        assert_eq!(registry.running_count(), 1);
        let snapshot = registry.snapshot_output(&id).unwrap();
        assert!(snapshot.running);
        assert!(snapshot.text.contains("start"));
        let _ = registry.stop(&id).await;
    }

    #[tokio::test]
    async fn registry_uses_child_pid_as_background_id() {
        let registry = ProcessRegistry::new();
        let child = spawn_shell_child("sleep 5", &ShellSpec::default()).unwrap();
        let pid = child.id().expect("spawned child has pid");
        let id = registry.child_id(&child);

        registry.spawn(id.clone(), "sleep 5", child, Instant::now());

        assert_eq!(id, pid.to_string());
        assert_eq!(registry.running_count(), 1);
        let _ = registry.stop(&id).await;
    }

    #[tokio::test]
    async fn registry_reports_natural_completion_and_keeps_snapshot() {
        let registry = ProcessRegistry::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        registry.set_completion_sender(tx);
        let child = spawn_shell_child("echo done", &ShellSpec::default()).unwrap();
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
        let child = spawn_shell_child("echo ready; sleep 30", &ShellSpec::default()).unwrap();
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
        let child = spawn_shell_child("echo started; sleep 30", &ShellSpec::default()).unwrap();
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
            ..Default::default()
        };
        let out = run("sh", &["-c", "echo $SMELT_TEST_VAR"], &opts).await;
        assert!(out.stdout.contains("from_test"));
    }

    #[tokio::test]
    async fn run_captures_stderr_separately() {
        let out = run("sh", &["-c", "echo err 1>&2"], &Options::default()).await;
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
            &Options::default(),
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
        let child = spawn_shell_child("sleep 30", &ShellSpec::default()).unwrap();
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
