//! Process execution and shell job supervision.
//!
//! `run_async` serves small internal subprocesses that need separate stdout,
//! stderr, and stdin. `JobSupervisor` owns every shell job from spawn through
//! completion, including containment, bounded output, following, detaching,
//! cancellation, and completion notification.

use crate::output_limit::{
    limit_text_tail_with_max_bytes, OutputLimiter, DEFAULT_MAX_BYTES, TRUNCATION_NOTICE,
};
use protocol::JobTermination;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const OVERSIZED_GRAPHEME_NOTICE: &str =
    "[process output truncated; oversized grapheme and following text discarded]";

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

    let mut retained = String::new();
    let mut pending_utf8 = Vec::with_capacity(4);
    let mut total_bytes = 0usize;
    let mut retaining = true;
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        total_bytes = total_bytes.saturating_add(n);
        if retaining {
            append_utf8_lossy(&mut retained, &mut pending_utf8, &buf[..n]);
            retaining = trim_output_tail(&mut retained, max_bytes);
            if !retaining {
                pending_utf8.clear();
            }
        }
    }

    if retaining && !pending_utf8.is_empty() {
        retained.push_str(&String::from_utf8_lossy(&pending_utf8));
    }
    let body = limit_text_tail_with_max_bytes(&retained, max_bytes);
    if !retaining {
        if body.is_empty() {
            return format!("{OVERSIZED_GRAPHEME_NOTICE}: read {total_bytes} bytes");
        }
        return format!(
            "{OVERSIZED_GRAPHEME_NOTICE}: retained {} of {total_bytes} bytes\n\n{body}",
            body.len()
        );
    }
    if total_bytes <= max_bytes {
        return body;
    }

    let retained_bytes = body.len();
    format!("{TRUNCATION_NOTICE}: last {retained_bytes} of {total_bytes} bytes\n\n{body}")
}

/// Decode complete UTF-8 scalars while retaining a partial scalar for the next
/// pipe read. Invalid sequences use the same replacement semantics as
/// `String::from_utf8_lossy` without corrupting valid scalars split across reads.
fn append_utf8_lossy(output: &mut String, pending: &mut Vec<u8>, bytes: &[u8]) {
    pending.extend_from_slice(bytes);
    let mut consumed = 0;
    while consumed < pending.len() {
        match std::str::from_utf8(&pending[consumed..]) {
            Ok(valid) => {
                output.push_str(valid);
                consumed = pending.len();
            }
            Err(error) => {
                let valid_end = consumed + error.valid_up_to();
                output.push_str(std::str::from_utf8(&pending[consumed..valid_end]).unwrap());
                consumed = valid_end;
                let Some(invalid_len) = error.error_len() else {
                    break;
                };
                output.push('\u{fffd}');
                consumed += invalid_len;
            }
        }
    }
    pending.drain(..consumed);
}

/// Keep a bounded complete-grapheme suffix.
///
/// Returns `false` when the active trailing grapheme alone exceeds the budget.
/// The caller must stop retaining later input because it cannot discard part of
/// that grapheme while still guaranteeing atomic output.
fn trim_output_tail(output: &mut String, max_bytes: usize) -> bool {
    if output.len() <= max_bytes {
        return true;
    }

    let Some((last_start, last)) = smelt_buffer::cell_width::grapheme_indices(output).next_back()
    else {
        return true;
    };
    let retaining = last.len() <= max_bytes;
    if !retaining {
        smelt_buffer::text::replace_range(output, last_start..output.len(), "");
    }

    let keep = smelt_buffer::text::grapheme_suffix(output, max_bytes).len();
    let remove = output.len() - keep;
    if remove > 0 {
        smelt_buffer::text::replace_range(output, 0..remove, "");
    }
    retaining
}

/// Result of [`run_async`]: `Done` for natural completion or timeout,
/// `Cancelled` when the cancellation token fired and the child was
/// killed before producing a status.
pub(crate) enum RunOutcome {
    Done(Output),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct JobRunOutput {
    /// Bounded stdout and stderr interleaved in arrival order.
    pub content: String,
    pub is_error: bool,
    pub timed_out: bool,
    pub background_id: Option<String>,
    pub exit_code: Option<i32>,
    pub termination: Option<JobTermination>,
}

pub struct JobRunConfig {
    pub timeout: Option<Duration>,
    pub shell: ShellSpec,
    pub cwd: PathBuf,
    pub started_at: Instant,
    pub cancel: Option<CancellationToken>,
    pub background_on_timeout: bool,
    pub detachable: bool,
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
/// open `/dev/tty` fail instead of taking over smelt's terminal while the tool
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

type ManagedChild = Box<dyn process_wrap::tokio::ChildWrapper>;

struct SpawnedJob {
    child: ManagedChild,
    containment: Containment,
}

fn spawn_shell_job(
    command: &str,
    shell: &ShellSpec,
    cwd: &Path,
    unit: &str,
    options: SupervisorOptions,
    systemd_scope: bool,
) -> io::Result<SpawnedJob> {
    let mut cmd = if systemd_scope {
        let mut cmd = tokio::process::Command::new("systemd-run");
        cmd.args([
            "--user",
            "--scope",
            "--quiet",
            "--expand-environment=no",
            "--unit",
            unit,
        ])
        .arg("--property=OOMPolicy=kill");
        if let Some(max_bytes) = options.memory_max_bytes {
            cmd.arg(format!("--property=MemoryMax={max_bytes}"));
        }
        cmd.arg("--")
            .arg(&shell.program)
            .args(&shell.args)
            .arg(command);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new(&shell.program);
        cmd.args(&shell.args).arg(command);
        cmd
    };
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_default_noninteractive_env(cmd.as_std_mut());
    configure_child_session(&mut cmd);

    let mut wrapped = process_wrap::tokio::CommandWrap::from(cmd);
    #[cfg(windows)]
    {
        use process_wrap::tokio::{JobObject, KillOnDrop};
        wrapped.wrap(KillOnDrop).wrap(JobObject);
    }
    let child = wrapped.spawn()?;
    #[cfg(not(windows))]
    let process_group = child.id().and_then(|pid| i32::try_from(pid).ok());
    #[cfg(target_os = "linux")]
    let containment = if systemd_scope {
        Containment::SystemdScope {
            unit: unit.to_string(),
            process_group,
        }
    } else {
        Containment::ProcessGroup { process_group }
    };
    #[cfg(windows)]
    let containment = Containment::JobObject;
    #[cfg(all(not(target_os = "linux"), not(windows)))]
    let containment = Containment::ProcessGroup { process_group };
    Ok(SpawnedJob { child, containment })
}

const LINE_TRUNCATION_NOTICE: &str = "[line truncated; remainder discarded]";

/// Buffered lines decoded with UTF-8 replacement semantics.
///
/// Valid scalars survive arbitrary pipe-read boundaries. Invalid byte sequences
/// become U+FFFD without ending the stream, and a final unterminated line is
/// returned normally. Lines larger than the configured byte limit retain a
/// complete-grapheme prefix and discard input until the next newline.
pub struct LossyLines<R> {
    reader: R,
    buffer: Vec<u8>,
    discarding_line: bool,
    max_line_bytes: usize,
}

impl<R> LossyLines<R>
where
    R: AsyncBufRead + Unpin,
{
    pub fn new(reader: R) -> Self {
        Self::with_max_line_bytes(reader, DEFAULT_MAX_BYTES)
    }

    fn with_max_line_bytes(reader: R, max_line_bytes: usize) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
            discarding_line: false,
            max_line_bytes,
        }
    }

    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                self.discarding_line = false;
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(self.take_line()));
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let content_len = newline.unwrap_or(available.len());
            let consumed = newline.map_or(content_len, |offset| offset + 1);

            if self.discarding_line {
                self.reader.consume(consumed);
                if newline.is_some() {
                    self.discarding_line = false;
                }
                continue;
            }

            let remaining = self.max_line_bytes.saturating_sub(self.buffer.len());
            let copied = content_len.min(remaining);
            self.buffer.extend_from_slice(&available[..copied]);
            let truncated = copied < content_len;
            self.reader.consume(consumed);
            debug_assert!(self.buffer.len() <= self.max_line_bytes);

            if truncated {
                let line = self.take_truncated_line();
                self.discarding_line = newline.is_none();
                return Ok(Some(line));
            }
            if newline.is_some() {
                return Ok(Some(self.take_line()));
            }
        }
    }

    fn take_truncated_line(&mut self) -> String {
        let complete_bytes = complete_utf8_prefix_len(&self.buffer);
        let decoded = String::from_utf8_lossy(&self.buffer[..complete_bytes]);
        let prefix_end = smelt_buffer::cell_width::grapheme_indices(&decoded)
            .next_back()
            .map(|(start, _)| start)
            .unwrap_or(0);
        let mut line = smelt_buffer::text::slice(&decoded, 0..prefix_end).to_string();
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(LINE_TRUNCATION_NOTICE);
        self.buffer.clear();
        line
    }

    fn take_line(&mut self) -> String {
        if self.buffer.last() == Some(&b'\r') {
            self.buffer.pop();
        }
        let line = String::from_utf8_lossy(&self.buffer).into_owned();
        self.buffer.clear();
        line
    }
}

fn complete_utf8_prefix_len(bytes: &[u8]) -> usize {
    let mut consumed = 0;
    while consumed < bytes.len() {
        match std::str::from_utf8(&bytes[consumed..]) {
            Ok(_) => return bytes.len(),
            Err(error) => {
                consumed += error.valid_up_to();
                let Some(invalid_len) = error.error_len() else {
                    return consumed;
                };
                consumed += invalid_len;
            }
        }
    }
    consumed
}

const LIVE_OUTPUT_TRUNCATION_NOTICE: &str =
    "[live process output truncated; final bounded output will replace this preview]";

impl JobSupervisor {
    /// Run a supervised shell job while following its bounded live output.
    /// Dropping the foreground view never transfers the child: the supervisor
    /// owns the same registered job until it exits or is stopped.
    pub async fn run(
        &self,
        command: &str,
        config: JobRunConfig,
        mut on_line: impl FnMut(String),
    ) -> JobRunOutput {
        if config
            .cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return cancelled_job_output();
        }

        let mut follower = match self
            .start_foreground(
                command,
                &config.shell,
                &config.cwd,
                config.started_at,
                config.detachable,
            )
            .await
        {
            Ok(follower) => follower,
            Err(error) => {
                return JobRunOutput {
                    content: error.to_string(),
                    is_error: true,
                    timed_out: false,
                    background_id: None,
                    exit_code: None,
                    termination: None,
                };
            }
        };
        let id = follower.id.clone();
        let deadline = config
            .timeout
            .map(|timeout| (tokio::time::Instant::now() + timeout, timeout));

        loop {
            enum RunTick {
                Cancel,
                Follow(FollowEvent),
                Timeout(Duration),
            }

            let tick = tokio::select! {
                biased;
                _ = async {
                    if let Some(cancel) = config.cancel.as_ref() {
                        cancel.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => RunTick::Cancel,
                event = follower.next_event() => RunTick::Follow(event),
                timeout = async {
                    if let Some((deadline, timeout)) = deadline {
                        tokio::time::sleep_until(deadline).await;
                        timeout
                    } else {
                        std::future::pending::<Duration>().await
                    }
                } => RunTick::Timeout(timeout),
            };

            match tick {
                RunTick::Cancel => match self.stop(&id).await {
                    Ok(_) => {
                        follower.disarm();
                        let output = cancelled_job_output();
                        on_line(output.content.clone());
                        return output;
                    }
                    Err(error) => {
                        follower.background();
                        let content = format!(
                            "cancellation failed: {error}; continuing in background as {id}"
                        );
                        on_line(content.clone());
                        return JobRunOutput {
                            content,
                            is_error: true,
                            timed_out: false,
                            background_id: Some(id),
                            exit_code: None,
                            termination: None,
                        };
                    }
                },
                RunTick::Follow(FollowEvent::Line(line)) => on_line(line),
                RunTick::Follow(FollowEvent::Detached) => {
                    follower.background();
                    let content = format!(
                        "moved to background as {id}, you'll be notified when it completes"
                    );
                    on_line(content.clone());
                    return JobRunOutput {
                        content,
                        is_error: false,
                        timed_out: false,
                        background_id: Some(id),
                        exit_code: None,
                        termination: None,
                    };
                }
                RunTick::Follow(FollowEvent::Finished(output)) => {
                    follower.disarm();
                    self.remove(&id);
                    return completed_job_output(output);
                }
                RunTick::Timeout(timeout) if config.background_on_timeout => {
                    follower.background();
                    let content = format!(
                        "timed out after {:.0}s; moved to background as {id}, you'll be notified when it completes",
                        timeout.as_secs_f64()
                    );
                    on_line(content.clone());
                    return JobRunOutput {
                        content,
                        is_error: false,
                        timed_out: true,
                        background_id: Some(id),
                        exit_code: None,
                        termination: None,
                    };
                }
                RunTick::Timeout(timeout) => match self.stop(&id).await {
                    Ok(_) => {
                        follower.disarm();
                        let content = format!("timed out after {:.0}s", timeout.as_secs_f64());
                        on_line(content.clone());
                        return JobRunOutput {
                            content,
                            is_error: true,
                            timed_out: true,
                            background_id: None,
                            exit_code: None,
                            termination: Some(JobTermination::Stopped),
                        };
                    }
                    Err(error) => {
                        follower.background();
                        let content = format!(
                            "timed out after {:.0}s; termination failed: {error}; continuing in background as {id}",
                            timeout.as_secs_f64()
                        );
                        on_line(content.clone());
                        return JobRunOutput {
                            content,
                            is_error: true,
                            timed_out: true,
                            background_id: Some(id),
                            exit_code: None,
                            termination: None,
                        };
                    }
                },
            }
        }
    }
}

fn cancelled_job_output() -> JobRunOutput {
    JobRunOutput {
        content: "cancelled".to_string(),
        is_error: true,
        timed_out: false,
        background_id: None,
        exit_code: None,
        termination: Some(JobTermination::Stopped),
    }
}

fn completed_job_output(output: JobOutput) -> JobRunOutput {
    let termination = output.termination.unwrap_or(JobTermination::Signaled);
    let mut content = output.text;
    if termination == JobTermination::OutOfMemory {
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str("command was terminated after an out-of-memory event");
    }
    JobRunOutput {
        content,
        is_error: termination != JobTermination::Exited || output.exit_code != Some(0),
        timed_out: false,
        background_id: None,
        exit_code: output.exit_code,
        termination: Some(termination),
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
pub(crate) fn kill_child_process_group(child: &tokio::process::Child) {
    let process_group = child.id().and_then(|pid| i32::try_from(pid).ok());
    kill_optional_process_group(process_group);
}

// ── Supervised shell jobs ─────────────────────────────────────────────────

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SCOPE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_FOLLOWER_ID: AtomicU64 = AtomicU64::new(1);

const MAX_RUNNING_JOBS: usize = 64;
const MAX_COMPLETED_JOBS: usize = 64;
const MAX_COMPLETED_JOB_BYTES: usize = 8 * 1024 * 1024;

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IsolationBackend {
    Auto,
    #[cfg(test)]
    ProcessGroup,
}

#[derive(Clone, Copy, Debug)]
struct SupervisorOptions {
    #[cfg(target_os = "linux")]
    backend: IsolationBackend,
    memory_max_bytes: Option<u64>,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            backend: IsolationBackend::Auto,
            memory_max_bytes: None,
        }
    }
}

#[derive(Clone, Debug)]
enum Containment {
    #[cfg(target_os = "linux")]
    SystemdScope {
        unit: String,
        process_group: Option<i32>,
    },
    #[cfg(unix)]
    ProcessGroup { process_group: Option<i32> },
    #[cfg(windows)]
    JobObject,
    #[cfg(not(any(unix, windows)))]
    ProcessGroup { process_group: Option<i32> },
}

#[cfg(target_os = "linux")]
const SYSTEMD_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(target_os = "linux")]
async fn run_status_with_timeout(
    mut command: tokio::process::Command,
    timeout: Duration,
) -> io::Result<std::process::ExitStatus> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    tokio::time::timeout(timeout, child.wait())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "systemd command timed out"))?
}

#[cfg(target_os = "linux")]
async fn run_systemd_status(
    command: tokio::process::Command,
) -> io::Result<std::process::ExitStatus> {
    run_status_with_timeout(command, SYSTEMD_COMMAND_TIMEOUT).await
}

#[cfg(target_os = "linux")]
async fn run_systemd_output(
    mut command: tokio::process::Command,
) -> io::Result<std::process::Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn()?;
    tokio::time::timeout(SYSTEMD_COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "systemd command timed out"))?
}

#[cfg(target_os = "linux")]
async fn probe_systemd_scope() -> bool {
    let sequence = NEXT_SCOPE_ID.fetch_add(1, Ordering::Relaxed);
    let unit = format!("smelt-job-probe-{}-{sequence}.scope", std::process::id());
    let mut command = tokio::process::Command::new("systemd-run");
    command.args([
        "--user",
        "--scope",
        "--quiet",
        "--expand-environment=no",
        "--unit",
        &unit,
        "--property=OOMPolicy=kill",
        "--",
        "true",
    ]);
    run_systemd_status(command)
        .await
        .is_ok_and(|status| status.success())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachResult {
    Requested,
    NoForegroundJob,
}

impl DetachResult {
    pub fn requested(self) -> bool {
        matches!(self, Self::Requested)
    }
}

struct ForegroundFollower {
    order: u64,
    detach_tx: Option<watch::Sender<bool>>,
}

enum NotificationState {
    Pending,
    Suppressed,
    Sent,
}

enum JobVisibility {
    Foreground {
        follower: Option<ForegroundFollower>,
    },
    Background {
        notification: NotificationState,
    },
    RemoveOnFinish,
}

enum JobLifecycle {
    Running {
        stop_tx: Option<mpsc::Sender<()>>,
    },
    Finished {
        exit_code: Option<i32>,
        termination: JobTermination,
        finished_at: Instant,
    },
}

struct Job {
    pid: Option<u32>,
    output: OutputLimiter,
    lifecycle: JobLifecycle,
    visibility: JobVisibility,
    command: String,
    started_at: Instant,
    finished_rx: watch::Receiver<Option<JobOutput>>,
    containment: Containment,
}

enum StopDisposition {
    AwaitRemoval,
    RemoveOnFinish,
}

struct JobStopRequest {
    stop_tx: Option<mpsc::Sender<()>>,
    finished_rx: watch::Receiver<Option<JobOutput>>,
    containment: Containment,
    restore_notification: bool,
}

impl JobStopRequest {
    fn signal(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.try_send(());
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobCompletion {
    pub id: String,
    pub exit_code: Option<i32>,
    pub termination: JobTermination,
}

struct JobSupervisorInner {
    jobs: Mutex<HashMap<String, Job>>,
    completion_tx: Mutex<Option<mpsc::UnboundedSender<JobCompletion>>>,
    running_slots: Arc<tokio::sync::Semaphore>,
    options: SupervisorOptions,
    #[cfg(target_os = "linux")]
    systemd_scope_available: tokio::sync::OnceCell<bool>,
}

pub struct JobInfo {
    pub id: String,
    pub pid: Option<u32>,
    pub command: String,
    pub elapsed_secs: u64,
}

#[derive(Clone, Debug)]
pub struct JobOutput {
    pub text: String,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub termination: Option<JobTermination>,
    pub elapsed_secs: u64,
    pub pid: Option<u32>,
}

impl Job {
    fn is_finished(&self) -> bool {
        matches!(self.lifecycle, JobLifecycle::Finished { .. })
    }

    fn elapsed_secs(&self) -> u64 {
        let finished_at = match self.lifecycle {
            JobLifecycle::Running { .. } => Instant::now(),
            JobLifecycle::Finished { finished_at, .. } => finished_at,
        };
        finished_at
            .saturating_duration_since(self.started_at)
            .as_secs()
    }

    fn finished_at(&self) -> Option<Instant> {
        match self.lifecycle {
            JobLifecycle::Running { .. } => None,
            JobLifecycle::Finished { finished_at, .. } => Some(finished_at),
        }
    }

    fn retained_memory_bytes(&self) -> usize {
        let final_output_bytes = self
            .finished_rx
            .borrow()
            .as_ref()
            .map_or(0, |output| output.text.capacity());
        self.command
            .capacity()
            .saturating_add(self.output.retained_memory_bytes())
            .saturating_add(final_output_bytes)
    }

    fn exit_code(&self) -> Option<i32> {
        match self.lifecycle {
            JobLifecycle::Running { .. } => None,
            JobLifecycle::Finished { exit_code, .. } => exit_code,
        }
    }

    fn termination(&self) -> Option<JobTermination> {
        match self.lifecycle {
            JobLifecycle::Running { .. } => None,
            JobLifecycle::Finished { termination, .. } => Some(termination),
        }
    }

    fn request_stop(&mut self, disposition: StopDisposition) -> JobStopRequest {
        let restore_notification = match disposition {
            StopDisposition::AwaitRemoval => match &mut self.visibility {
                JobVisibility::Background { notification }
                    if matches!(notification, NotificationState::Pending) =>
                {
                    *notification = NotificationState::Suppressed;
                    true
                }
                _ => false,
            },
            StopDisposition::RemoveOnFinish => {
                self.visibility = JobVisibility::RemoveOnFinish;
                false
            }
        };
        let stop_tx = match &mut self.lifecycle {
            JobLifecycle::Running { stop_tx } => stop_tx.take(),
            JobLifecycle::Finished { .. } => None,
        };
        JobStopRequest {
            stop_tx,
            finished_rx: self.finished_rx.clone(),
            containment: self.containment.clone(),
            restore_notification,
        }
    }

    fn snapshot_output(&self) -> JobOutput {
        JobOutput {
            pid: self.pid,
            text: self.output.format_text(),
            running: !self.is_finished(),
            exit_code: self.exit_code(),
            termination: self.termination(),
            elapsed_secs: self.elapsed_secs(),
        }
    }
}

#[derive(Clone)]
pub struct JobSupervisor(Arc<JobSupervisorInner>);

impl Default for JobSupervisor {
    fn default() -> Self {
        Self::with_options(SupervisorOptions::default())
    }
}

struct ForegroundRegistration {
    supervisor: Arc<JobSupervisorInner>,
    id: String,
    order: u64,
    active: bool,
}

impl ForegroundRegistration {
    fn disarm(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if let Ok(mut jobs) = self.supervisor.jobs.lock() {
            if let Some(job) = jobs.get_mut(&self.id) {
                if let JobVisibility::Foreground { follower } = &mut job.visibility {
                    if follower
                        .as_ref()
                        .is_some_and(|state| state.order == self.order)
                    {
                        *follower = None;
                    }
                }
            }
        }
    }

    fn background(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        mark_job_background(&self.supervisor, &self.id, self.order);
    }
}

impl Drop for ForegroundRegistration {
    fn drop(&mut self) {
        if self.active {
            mark_job_background(&self.supervisor, &self.id, self.order);
        }
    }
}

struct JobFollower {
    id: String,
    lines_rx: mpsc::Receiver<String>,
    finished_rx: watch::Receiver<Option<JobOutput>>,
    detach_rx: Option<watch::Receiver<bool>>,
    registration: ForegroundRegistration,
}

enum FollowEvent {
    Line(String),
    Detached,
    Finished(JobOutput),
}

impl JobFollower {
    async fn next_event(&mut self) -> FollowEvent {
        loop {
            if self
                .detach_rx
                .as_ref()
                .is_some_and(|receiver| *receiver.borrow())
            {
                return FollowEvent::Detached;
            }
            if let Some(output) = self.finished_rx.borrow().clone() {
                if let Ok(line) = self.lines_rx.try_recv() {
                    return FollowEvent::Line(line);
                }
                return FollowEvent::Finished(output);
            }
            tokio::select! {
                biased;
                changed = async {
                    match self.detach_rx.as_mut() {
                        Some(receiver) => receiver.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if changed.is_ok()
                        && self.detach_rx.as_ref().is_some_and(|receiver| *receiver.borrow())
                    {
                        return FollowEvent::Detached;
                    }
                }
                line = self.lines_rx.recv() => {
                    if let Some(line) = line {
                        return FollowEvent::Line(line);
                    }
                }
                changed = self.finished_rx.changed() => {
                    if changed.is_err() || self.finished_rx.borrow().is_some() {
                        continue;
                    }
                }
            }
        }
    }

    fn background(&mut self) {
        self.registration.background();
    }

    fn disarm(&mut self) {
        self.registration.disarm();
    }
}

struct LiveOutput {
    tx: mpsc::Sender<String>,
    lines: usize,
    bytes: usize,
    truncated: bool,
}

impl LiveOutput {
    fn push(&mut self, line: &str) {
        if self.truncated {
            return;
        }
        let separator = usize::from(self.lines > 0);
        let next_bytes = self
            .bytes
            .saturating_add(separator)
            .saturating_add(line.len());
        if self.lines >= crate::output_limit::DEFAULT_MAX_LINES
            || next_bytes > crate::output_limit::DEFAULT_MAX_BYTES
            || self.tx.capacity() <= 1
        {
            self.truncate();
            return;
        }
        match self.tx.try_send(line.to_string()) {
            Ok(()) => {
                self.lines += 1;
                self.bytes = next_bytes;
            }
            Err(mpsc::error::TrySendError::Full(_)) => self.truncate(),
            Err(mpsc::error::TrySendError::Closed(_)) => self.truncated = true,
        }
    }

    fn truncate(&mut self) {
        self.truncated = true;
        let _ = self.tx.try_send(LIVE_OUTPUT_TRUNCATION_NOTICE.to_string());
    }
}

impl JobSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_options(options: SupervisorOptions) -> Self {
        Self(Arc::new(JobSupervisorInner {
            jobs: Mutex::new(HashMap::new()),
            completion_tx: Mutex::new(None),
            running_slots: Arc::new(tokio::sync::Semaphore::new(MAX_RUNNING_JOBS)),
            options,
            #[cfg(target_os = "linux")]
            systemd_scope_available: tokio::sync::OnceCell::new(),
        }))
    }

    #[cfg(test)]
    fn process_group_only() -> Self {
        Self::with_options(SupervisorOptions {
            #[cfg(target_os = "linux")]
            backend: IsolationBackend::ProcessGroup,
            memory_max_bytes: None,
        })
    }

    #[cfg(all(test, target_os = "linux"))]
    fn with_memory_limit(memory_max_bytes: u64) -> Self {
        Self::with_options(SupervisorOptions {
            backend: IsolationBackend::Auto,
            memory_max_bytes: Some(memory_max_bytes),
        })
    }

    pub fn set_completion_sender(&self, tx: mpsc::UnboundedSender<JobCompletion>) {
        *self.0.completion_tx.lock().unwrap() = Some(tx);
    }

    async fn systemd_scope_available(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            if self.0.options.backend != IsolationBackend::Auto {
                return false;
            }
            return *self
                .0
                .systemd_scope_available
                .get_or_init(probe_systemd_scope)
                .await;
        }
        #[cfg(not(target_os = "linux"))]
        false
    }

    pub async fn spawn_background(
        &self,
        command: &str,
        shell: &ShellSpec,
        cwd: &Path,
        started_at: Instant,
    ) -> io::Result<String> {
        self.spawn_job(command, shell, cwd, started_at, true, false)
            .await
            .map(|(id, _)| id)
    }

    async fn start_foreground(
        &self,
        command: &str,
        shell: &ShellSpec,
        cwd: &Path,
        started_at: Instant,
        detachable: bool,
    ) -> io::Result<JobFollower> {
        self.spawn_job(command, shell, cwd, started_at, false, detachable)
            .await
            .map(|(_, follower)| follower.expect("foreground jobs have a follower"))
    }

    async fn spawn_job(
        &self,
        command: &str,
        shell: &ShellSpec,
        cwd: &Path,
        started_at: Instant,
        background: bool,
        detachable: bool,
    ) -> io::Result<(String, Option<JobFollower>)> {
        let running_permit = Arc::clone(&self.0.running_slots)
            .try_acquire_owned()
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("shell job limit reached ({MAX_RUNNING_JOBS})"),
                )
            })?;
        let id = self.next_id();
        let scope_sequence = NEXT_SCOPE_ID.fetch_add(1, Ordering::Relaxed);
        let unit = format!("smelt-job-{}-{scope_sequence}.scope", std::process::id());
        let systemd_scope = self.systemd_scope_available().await;
        let SpawnedJob {
            mut child,
            containment,
        } = spawn_shell_job(command, shell, cwd, &unit, self.0.options, systemd_scope)?;
        let pid = child.id();
        let stdout = child
            .stdout()
            .take()
            .expect("supervised jobs always pipe stdout");
        let stderr = child
            .stderr()
            .take()
            .expect("supervised jobs always pipe stderr");
        let stdout_reader = LossyLines::new(BufReader::new(stdout));
        let stderr_reader = LossyLines::new(BufReader::new(stderr));
        let (stop_tx, stop_rx) = mpsc::channel(1);
        let (finished_tx, finished_rx) = watch::channel(None);

        let (live_output, follower, visibility) = if background {
            (
                None,
                None,
                JobVisibility::Background {
                    notification: NotificationState::Pending,
                },
            )
        } else {
            let (lines_tx, lines_rx) = mpsc::channel(64);
            let (detach_tx, detach_rx) = if detachable {
                let (tx, rx) = watch::channel(false);
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
            let order = NEXT_FOLLOWER_ID.fetch_add(1, Ordering::Relaxed);
            let follower = JobFollower {
                id: id.clone(),
                lines_rx,
                finished_rx: finished_rx.clone(),
                detach_rx,
                registration: ForegroundRegistration {
                    supervisor: Arc::clone(&self.0),
                    id: id.clone(),
                    order,
                    active: true,
                },
            };
            (
                Some(LiveOutput {
                    tx: lines_tx,
                    lines: 0,
                    bytes: 0,
                    truncated: false,
                }),
                Some(follower),
                JobVisibility::Foreground {
                    follower: Some(ForegroundFollower { order, detach_tx }),
                },
            )
        };

        let job = Job {
            pid,
            output: OutputLimiter::default(),
            lifecycle: JobLifecycle::Running {
                stop_tx: Some(stop_tx),
            },
            visibility,
            command: limit_text_tail_with_max_bytes(command, DEFAULT_MAX_BYTES),
            started_at,
            finished_rx,
            containment: containment.clone(),
        };
        self.0.jobs.lock().unwrap().insert(id.clone(), job);

        tokio::spawn(supervise_job(JobTask {
            supervisor: Arc::clone(&self.0),
            id: id.clone(),
            child,
            containment,
            stdout: stdout_reader,
            stderr: stderr_reader,
            stop_rx,
            finished_tx,
            live_output,
            _running_permit: running_permit,
        }));
        Ok((id, follower))
    }

    pub fn detach_latest_foreground(&self) -> DetachResult {
        let sender = {
            let jobs = self.0.jobs.lock().unwrap();
            jobs.values()
                .filter_map(|job| match &job.visibility {
                    JobVisibility::Foreground {
                        follower: Some(follower),
                    } => Some((follower.order, follower.detach_tx.as_ref()?.clone())),
                    _ => None,
                })
                .max_by_key(|(order, _)| *order)
                .map(|(_, sender)| sender)
        };
        let Some(sender) = sender else {
            return DetachResult::NoForegroundJob;
        };
        if sender.send(true).is_ok() {
            DetachResult::Requested
        } else {
            DetachResult::NoForegroundJob
        }
    }

    fn next_id(&self) -> String {
        let sequence = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
        format!("proc_{sequence}")
    }

    /// Drains bounded output and removes finished jobs.
    pub fn drain_output(&self, id: &str) -> Result<JobOutput, String> {
        let mut jobs = self.0.jobs.lock().unwrap();
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;
        let output = JobOutput {
            pid: job.pid,
            text: job.output.drain_text(),
            running: !job.is_finished(),
            exit_code: job.exit_code(),
            termination: job.termination(),
            elapsed_secs: job.elapsed_secs(),
        };
        if job.is_finished() {
            jobs.remove(id);
        }
        Ok(output)
    }

    /// Returns bounded output without draining or removing the job.
    pub fn snapshot_output(&self, id: &str) -> Result<JobOutput, String> {
        let jobs = self.0.jobs.lock().unwrap();
        let job = jobs
            .get(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;
        Ok(job.snapshot_output())
    }

    pub async fn stop(&self, id: &str) -> Result<JobOutput, String> {
        let mut request = {
            let mut jobs = self.0.jobs.lock().unwrap();
            jobs.get_mut(id)
                .ok_or_else(|| format!("no process with id '{id}'"))?
                .request_stop(StopDisposition::AwaitRemoval)
        };
        request.signal();
        if request.finished_rx.borrow().is_none() {
            let stopped =
                tokio::time::timeout(Duration::from_secs(2), request.finished_rx.changed()).await;
            if !matches!(stopped, Ok(Ok(()))) || request.finished_rx.borrow().is_none() {
                request.containment.terminate_detached();
                let _ = tokio::time::timeout(Duration::from_secs(2), request.finished_rx.changed())
                    .await;
            }
        }
        let Some(output) = request.finished_rx.borrow().clone() else {
            let completion = if request.restore_notification {
                let mut jobs = self.0.jobs.lock().unwrap();
                jobs.get_mut(id).and_then(|job| {
                    if let JobVisibility::Background { notification } = &mut job.visibility {
                        if matches!(notification, NotificationState::Suppressed) {
                            *notification = NotificationState::Pending;
                        }
                    }
                    completion_if_ready(id, job)
                })
            } else {
                None
            };
            if let Some(completion) = completion {
                send_job_completion(&self.0, completion);
            }
            return Err(format!(
                "job '{id}' did not stop after containment termination"
            ));
        };
        self.remove(id);
        Ok(output)
    }

    fn remove(&self, id: &str) {
        self.0.jobs.lock().unwrap().remove(id);
    }

    pub fn running_count(&self) -> usize {
        self.0
            .jobs
            .lock()
            .unwrap()
            .values()
            .filter(|job| {
                !job.is_finished() && !matches!(job.visibility, JobVisibility::RemoveOnFinish)
            })
            .count()
    }

    pub fn list(&self) -> Vec<JobInfo> {
        let jobs = self.0.jobs.lock().unwrap();
        let mut infos: Vec<JobInfo> = jobs
            .iter()
            .filter(|(_, job)| {
                !job.is_finished() && !matches!(job.visibility, JobVisibility::RemoveOnFinish)
            })
            .map(|(id, job)| JobInfo {
                id: id.clone(),
                pid: job.pid,
                command: job.command.clone(),
                elapsed_secs: job.elapsed_secs(),
            })
            .collect();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        infos
    }

    pub fn clear(&self) {
        let jobs = {
            let mut registered = self.0.jobs.lock().unwrap();
            let mut jobs = Vec::with_capacity(registered.len());
            registered.retain(|_, job| {
                if job.is_finished() {
                    return false;
                }
                jobs.push(job.request_stop(StopDisposition::RemoveOnFinish));
                true
            });
            jobs
        };
        for mut request in jobs {
            request.signal();
            request.containment.terminate_detached();
        }
    }
}

struct JobTask {
    supervisor: Arc<JobSupervisorInner>,
    id: String,
    child: ManagedChild,
    containment: Containment,
    stdout: LossyLines<BufReader<ChildStdout>>,
    stderr: LossyLines<BufReader<ChildStderr>>,
    stop_rx: mpsc::Receiver<()>,
    finished_tx: watch::Sender<Option<JobOutput>>,
    live_output: Option<LiveOutput>,
    _running_permit: tokio::sync::OwnedSemaphorePermit,
}

async fn supervise_job(task: JobTask) {
    let JobTask {
        supervisor,
        id,
        mut child,
        containment,
        mut stdout,
        mut stderr,
        mut stop_rx,
        finished_tx,
        mut live_output,
        _running_permit,
    } = task;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut child_done = false;
    let mut exit_status = None;
    let mut stopped = false;

    while !stdout_done || !stderr_done || !child_done {
        enum JobTick {
            Stop,
            Stdout(io::Result<Option<String>>),
            Stderr(io::Result<Option<String>>),
            Exit(io::Result<std::process::ExitStatus>),
        }

        let tick = tokio::select! {
            biased;
            _ = stop_rx.recv(), if !stopped => JobTick::Stop,
            line = stdout.next_line(), if !stdout_done => JobTick::Stdout(line),
            line = stderr.next_line(), if !stderr_done => JobTick::Stderr(line),
            status = child.wait(), if !child_done => JobTick::Exit(status),
        };
        match tick {
            JobTick::Stop => {
                stopped = true;
                containment.terminate(child.as_mut()).await;
            }
            JobTick::Stdout(Ok(Some(line))) | JobTick::Stderr(Ok(Some(line))) => {
                if let Some(job) = supervisor.jobs.lock().unwrap().get_mut(&id) {
                    job.output.push_line(line.clone());
                }
                if let Some(output) = live_output.as_mut() {
                    output.push(&line);
                }
            }
            JobTick::Stdout(_) => stdout_done = true,
            JobTick::Stderr(_) => stderr_done = true,
            JobTick::Exit(status) => {
                exit_status = status.ok();
                child_done = true;
            }
        }
    }

    let out_of_memory = containment
        .wait_until_empty(&mut stop_rx, child.as_mut(), &mut stopped)
        .await;
    let exit_code = exit_status.and_then(|status| status.code());
    let termination = if stopped {
        JobTermination::Stopped
    } else if out_of_memory {
        JobTermination::OutOfMemory
    } else if exit_code.is_some() {
        JobTermination::Exited
    } else {
        JobTermination::Signaled
    };
    let (completion, output, remove_on_finish) = {
        let mut jobs = supervisor.jobs.lock().unwrap();
        let Some(job) = jobs.get_mut(&id) else {
            return;
        };
        job.lifecycle = JobLifecycle::Finished {
            exit_code,
            termination,
            finished_at: Instant::now(),
        };
        let completion = completion_if_ready(&id, job);
        (
            completion,
            job.snapshot_output(),
            matches!(job.visibility, JobVisibility::RemoveOnFinish),
        )
    };
    let _ = finished_tx.send(Some(output));
    if let Some(completion) = completion {
        send_job_completion(&supervisor, completion);
    }
    if remove_on_finish {
        supervisor.jobs.lock().unwrap().remove(&id);
    } else {
        prune_completed_jobs(&mut supervisor.jobs.lock().unwrap(), &id);
    }
}

fn prune_completed_jobs(jobs: &mut HashMap<String, Job>, preserve_id: &str) {
    loop {
        let completed = jobs
            .iter()
            .filter(|(_, job)| job.is_finished())
            .map(|(id, job)| (id, job.retained_memory_bytes()))
            .collect::<Vec<_>>();
        let retained_bytes = completed
            .iter()
            .fold(0usize, |total, (_, bytes)| total.saturating_add(*bytes));
        if completed.len() <= MAX_COMPLETED_JOBS && retained_bytes <= MAX_COMPLETED_JOB_BYTES {
            return;
        }
        let oldest = jobs
            .iter()
            .filter(|(id, job)| id.as_str() != preserve_id && job.is_finished())
            .min_by_key(|(_, job)| job.finished_at())
            .map(|(id, _)| id.clone());
        let Some(oldest) = oldest else {
            return;
        };
        jobs.remove(&oldest);
    }
}

fn mark_job_background(supervisor: &Arc<JobSupervisorInner>, id: &str, order: u64) {
    let completion = {
        let mut jobs = supervisor.jobs.lock().unwrap();
        jobs.get_mut(id).and_then(|job| {
            let JobVisibility::Foreground {
                follower: Some(follower),
            } = &job.visibility
            else {
                return None;
            };
            if follower.order != order {
                return None;
            }
            job.visibility = JobVisibility::Background {
                notification: NotificationState::Pending,
            };
            completion_if_ready(id, job)
        })
    };
    if let Some(completion) = completion {
        send_job_completion(supervisor, completion);
    }
}

fn completion_if_ready(id: &str, job: &mut Job) -> Option<JobCompletion> {
    let JobLifecycle::Finished {
        exit_code,
        termination,
        ..
    } = job.lifecycle
    else {
        return None;
    };
    let JobVisibility::Background { notification } = &mut job.visibility else {
        return None;
    };
    if !matches!(notification, NotificationState::Pending) {
        return None;
    }
    *notification = NotificationState::Sent;
    Some(JobCompletion {
        id: id.to_string(),
        exit_code,
        termination,
    })
}

fn send_job_completion(supervisor: &JobSupervisorInner, completion: JobCompletion) {
    if let Some(sender) = supervisor.completion_tx.lock().unwrap().clone() {
        let _ = sender.send(completion);
    }
}

impl Containment {
    async fn terminate(&self, child: &mut dyn process_wrap::tokio::ChildWrapper) {
        match self {
            #[cfg(target_os = "linux")]
            Self::SystemdScope {
                unit,
                process_group,
            } => {
                let mut command = tokio::process::Command::new("systemctl");
                command.args([
                    "--user",
                    "kill",
                    "--kill-whom=all",
                    "--signal=SIGKILL",
                    unit,
                ]);
                let _ = run_systemd_status(command).await;
                kill_optional_process_group(*process_group);
                let _ = child.start_kill();
            }
            #[cfg(unix)]
            Self::ProcessGroup { process_group } => {
                kill_optional_process_group(*process_group);
                let _ = child.start_kill();
            }
            #[cfg(windows)]
            Self::JobObject => {
                let _ = child.start_kill();
            }
            #[cfg(not(any(unix, windows)))]
            Self::ProcessGroup { .. } => {
                let _ = child.start_kill();
            }
        }
    }

    fn terminate_detached(&self) {
        match self {
            #[cfg(target_os = "linux")]
            Self::SystemdScope {
                unit,
                process_group,
            } => {
                kill_optional_process_group(*process_group);
                let unit = unit.clone();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        let mut command = tokio::process::Command::new("systemctl");
                        command.args([
                            "--user",
                            "kill",
                            "--kill-whom=all",
                            "--signal=SIGKILL",
                            &unit,
                        ]);
                        let _ = run_systemd_status(command).await;
                    });
                }
            }
            #[cfg(unix)]
            Self::ProcessGroup { process_group } => {
                kill_optional_process_group(*process_group);
            }
            #[cfg(any(windows, not(any(unix, windows))))]
            _ => {}
        }
    }

    async fn wait_until_empty(
        &self,
        _stop_rx: &mut mpsc::Receiver<()>,
        _child: &mut dyn process_wrap::tokio::ChildWrapper,
        _stopped: &mut bool,
    ) -> bool {
        #[cfg(target_os = "linux")]
        if let Self::SystemdScope { unit, .. } = self {
            let mut out_of_memory = false;
            let mut status_failures = 0;
            loop {
                match systemd_scope_status(unit).await {
                    Ok(status) => {
                        status_failures = 0;
                        out_of_memory |= status.out_of_memory;
                        if !status.active {
                            if out_of_memory {
                                reset_failed_scope(unit).await;
                            }
                            return out_of_memory;
                        }
                    }
                    Err(_) => {
                        status_failures += 1;
                        if status_failures >= 20 {
                            self.terminate(_child).await;
                            return out_of_memory;
                        }
                    }
                }
                if *_stopped {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                tokio::select! {
                    _ = _stop_rx.recv() => {
                        *_stopped = true;
                        self.terminate(_child).await;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        }
        false
    }
}

#[cfg(target_os = "linux")]
struct SystemdScopeStatus {
    active: bool,
    out_of_memory: bool,
}

#[cfg(target_os = "linux")]
async fn systemd_scope_status(unit: &str) -> io::Result<SystemdScopeStatus> {
    let mut command = tokio::process::Command::new("systemctl");
    command.args([
        "--user",
        "show",
        unit,
        "--property=LoadState",
        "--property=ActiveState",
        "--property=Result",
    ]);
    let output = run_systemd_output(command).await?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "systemctl failed with status {}",
            output.status
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let load_state = text
        .lines()
        .find_map(|line| line.strip_prefix("LoadState="))
        .ok_or_else(|| io::Error::other("systemctl omitted LoadState"))?;
    let active_state = text
        .lines()
        .find_map(|line| line.strip_prefix("ActiveState="))
        .ok_or_else(|| io::Error::other("systemctl omitted ActiveState"))?;
    let result = text
        .lines()
        .find_map(|line| line.strip_prefix("Result="))
        .unwrap_or_default();
    Ok(SystemdScopeStatus {
        active: load_state != "not-found"
            && matches!(active_state, "active" | "activating" | "deactivating"),
        out_of_memory: result == "oom-kill",
    })
}

#[cfg(target_os = "linux")]
async fn reset_failed_scope(unit: &str) {
    let mut command = tokio::process::Command::new("systemctl");
    command.args(["--user", "reset-failed", unit]);
    let _ = run_systemd_status(command).await;
}

#[cfg(unix)]
fn kill_optional_process_group(process_group: Option<i32>) {
    if let Some(process_group) = process_group {
        // SAFETY: the supervised wrapper starts as a session leader, so this
        // negative pid targets only its process group.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_optional_process_group(_process_group: Option<i32>) {}

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

    #[tokio::test]
    async fn lossy_lines_retains_partial_bytes_when_read_is_cancelled() {
        use tokio::io::AsyncWriteExt;

        let (reader, mut writer) = tokio::io::duplex(64);
        let mut lines = LossyLines::new(BufReader::new(reader));
        writer.write_all(b"partial").await.unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(20), lines.next_line())
                .await
                .is_err()
        );

        writer.write_all(b" rest\n").await.unwrap();
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("partial rest")
        );
    }

    #[tokio::test]
    async fn lossy_lines_bounds_unterminated_grapheme_and_resumes_after_newline() {
        use tokio::io::AsyncWriteExt;

        let (reader, mut writer) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            let mut output = String::from("a");
            output.extend(core::iter::repeat_n('\u{301}', 10_000));
            output.push_str("\nok\n");
            writer.write_all(output.as_bytes()).await.unwrap();
        });
        let mut lines = LossyLines::with_max_line_bytes(BufReader::new(reader), 32);

        let truncated = lines.next_line().await.unwrap().unwrap();
        assert_eq!(truncated, LINE_TRUNCATION_NOTICE);
        assert!(lines.buffer.len() <= lines.max_line_bytes);
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("ok"));
        assert_eq!(lines.next_line().await.unwrap(), None);
        writer.await.unwrap();
    }

    #[test]
    fn live_output_reserves_channel_space_for_one_truncation_notice() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut output = LiveOutput {
            tx,
            lines: 0,
            bytes: 0,
            truncated: false,
        };

        for index in 0..10 {
            output.push(&format!("line-{index}"));
        }
        drop(output);

        let mut received = Vec::new();
        while let Ok(line) = rx.try_recv() {
            received.push(line);
        }
        assert_eq!(received.len(), 4);
        assert_eq!(
            received
                .iter()
                .filter(|line| line.as_str() == LIVE_OUTPUT_TRUNCATION_NOTICE)
                .count(),
            1
        );
        assert_eq!(
            received.last().map(String::as_str),
            Some(LIVE_OUTPUT_TRUNCATION_NOTICE)
        );
    }

    #[tokio::test]
    async fn output_tail_keeps_grapheme_at_exact_budget_boundary() {
        let text = "abce\u{301}XYZ";
        let mut input = text.as_bytes();

        let output = read_output_tail(&mut input, "e\u{301}XYZ".len()).await;

        assert!(output.ends_with("e\u{301}XYZ"));
    }

    #[tokio::test]
    async fn output_tail_drops_grapheme_that_crosses_budget_boundary() {
        let text = "abce\u{301}XYZ";
        let mut input = text.as_bytes();

        let output = read_output_tail(&mut input, 5).await;

        assert!(output.ends_with("XYZ"));
        assert!(!output.ends_with("\u{301}XYZ"));
    }

    #[tokio::test]
    async fn output_tail_stops_retaining_an_oversized_grapheme() {
        let text = format!("a{}later", "\u{301}".repeat(10_000));
        let mut input = text.as_bytes();

        let output = read_output_tail(&mut input, 32).await;

        assert!(output.contains(OVERSIZED_GRAPHEME_NOTICE));
        assert!(!output.contains("later"));
        assert!(!output.contains('\u{301}'));
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

    fn finished_rx() -> watch::Receiver<Option<JobOutput>> {
        watch::channel(None).1
    }

    fn job_fixture(
        lines: Vec<&str>,
        finished: bool,
        exit_code: Option<i32>,
        started_at: Instant,
        finished_at: Option<Instant>,
    ) -> Job {
        let mut output = OutputLimiter::default();
        for line in lines {
            output.push_line(line.to_string());
        }
        Job {
            pid: None,
            output,
            lifecycle: if finished {
                JobLifecycle::Finished {
                    exit_code,
                    termination: JobTermination::Exited,
                    finished_at: finished_at.unwrap_or_else(Instant::now),
                }
            } else {
                JobLifecycle::Running { stop_tx: None }
            },
            visibility: JobVisibility::Background {
                notification: NotificationState::Pending,
            },
            command: "cmd".into(),
            started_at,
            finished_rx: finished_rx(),
            containment: Containment::ProcessGroup {
                process_group: None,
            },
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
                "process {pid} was still alive after supervisor clear"
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
        supervisor: &JobSupervisor,
        id: &str,
        mut predicate: impl FnMut(&JobOutput) -> bool,
    ) -> JobOutput {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = supervisor.snapshot_output(id).unwrap();
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

    async fn run_test_job(
        command: &str,
        config: JobRunConfig,
        on_line: impl FnMut(String),
    ) -> JobRunOutput {
        JobSupervisor::process_group_only()
            .run(command, config, on_line)
            .await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_process_with_oversized_grapheme_stays_bounded() {
        let mut options = test_options();
        options.max_output_bytes = Some(32);

        let out = run(
            "sh",
            &[
                "-c",
                "printf a; i=0; while [ $i -lt 10000 ]; do printf '\\314\\201'; i=$((i + 1)); done; printf later",
            ],
            &options,
        )
        .await;

        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains(OVERSIZED_GRAPHEME_NOTICE));
        assert!(!out.stdout.contains("later"));
        assert!(!out.stdout.contains('\u{301}'));
    }

    #[tokio::test]
    async fn run_echo_captures_stdout() {
        let out = run("sh", &["-c", "echo hello"], &test_options()).await;
        assert!(out.stdout.contains("hello"));
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn run_preserves_unicode_split_across_pipe_reads() {
        let expected = "besta\u{308}tigt 日本 👩\u{200d}💻";
        let out = run(
            "sh",
            &[
                "-c",
                "printf 'besta'; printf '\\314'; sleep 0.01; printf '\\210tigt 日本 👩‍💻'",
            ],
            &test_options(),
        )
        .await;

        assert_eq!(out.stdout, expected);
    }

    #[tokio::test]
    async fn streaming_preserves_unicode_lines_and_callbacks() {
        let first = "besta\u{308}tigt 日本 👩\u{200d}💻";
        let second = "confirme\u{301}e 🇨🇦";
        let mut streamed = Vec::new();
        let out = run_test_job(
            "printf 'besta'; printf '\\314'; sleep 0.01; printf '\\210tigt 日本 👩‍💻\\n'; printf 'confirmée 🇨🇦\\n' >&2",
            JobRunConfig {
                timeout: Some(Duration::from_secs(2)),
                shell: ShellSpec::default(),
                cwd: test_cwd(),
                started_at: Instant::now(),
                cancel: None,
                background_on_timeout: false,
                detachable: false,
            },
            |line| streamed.push(line),
        )
        .await;

        assert!(!out.is_error);
        assert_eq!(streamed, [first, second]);
        assert_eq!(out.content, format!("{first}\n{second}"));
    }

    #[tokio::test]
    async fn streaming_replaces_invalid_utf8_and_keeps_following_output() {
        let mut streamed = Vec::new();
        let out = run_test_job(
            "printf '\\377bad\\nok\\n'",
            JobRunConfig {
                timeout: Some(Duration::from_secs(2)),
                shell: ShellSpec::default(),
                cwd: test_cwd(),
                started_at: Instant::now(),
                cancel: None,
                background_on_timeout: false,
                detachable: false,
            },
            |line| streamed.push(line),
        )
        .await;

        assert!(!out.is_error);
        assert_eq!(streamed, ["\u{fffd}bad", "ok"]);
        assert_eq!(out.content, "\u{fffd}bad\nok");
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
        let out = run_test_job(
            "printf 'first\\nβeta\\n'",
            JobRunConfig {
                timeout: Some(Duration::from_secs(2)),
                shell: ShellSpec::default(),
                cwd: test_cwd(),
                started_at: Instant::now(),
                cancel: None,
                background_on_timeout: false,
                detachable: false,
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
    async fn high_volume_live_output_is_bounded_and_final_output_is_authoritative() {
        let mut lines = Vec::new();
        let out = run_test_job(
            "i=0; while [ $i -lt 3000 ]; do echo line-$i; i=$((i + 1)); done",
            JobRunConfig {
                timeout: Some(Duration::from_secs(5)),
                shell: ShellSpec::default(),
                cwd: test_cwd(),
                started_at: Instant::now(),
                cancel: None,
                background_on_timeout: false,
                detachable: false,
            },
            |line| lines.push(line),
        )
        .await;

        assert!(!out.is_error);
        assert!(lines.len() <= crate::output_limit::DEFAULT_MAX_LINES + 1);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.as_str() == LIVE_OUTPUT_TRUNCATION_NOTICE)
                .count(),
            1
        );
        assert_eq!(
            lines.last().map(String::as_str),
            Some(LIVE_OUTPUT_TRUNCATION_NOTICE)
        );
        assert!(out.content.contains(TRUNCATION_NOTICE));
        assert!(out.content.ends_with("line-2999"));
        assert!(out.content.len() <= crate::output_limit::DEFAULT_MAX_BYTES + 256);
    }

    #[tokio::test]
    async fn background_job_replaces_invalid_utf8_in_unterminated_stderr() {
        let supervisor = JobSupervisor::process_group_only();
        let id = supervisor
            .spawn_background(
                "printf '\\376bad\\nlast' >&2",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
            )
            .await
            .unwrap();

        let output = wait_for_snapshot(&supervisor, &id, |output| !output.running).await;
        assert_eq!(output.text, "\u{fffd}bad\nlast");
    }

    #[tokio::test]
    async fn streaming_timeout_returns_same_job_as_background() {
        let supervisor = JobSupervisor::process_group_only();
        let mut lines = Vec::new();
        let out = supervisor
            .run(
                "echo start; sleep 5",
                JobRunConfig {
                    timeout: Some(Duration::from_millis(100)),
                    shell: ShellSpec::default(),
                    cwd: test_cwd(),
                    started_at: Instant::now(),
                    cancel: None,
                    background_on_timeout: true,
                    detachable: false,
                },
                |line| lines.push(line),
            )
            .await;

        let id = out.background_id.expect("background job id");
        assert!(out.timed_out);
        assert!(!out.is_error);
        assert!(out.content.contains("you'll be notified when it completes"));
        assert_eq!(lines.first().map(String::as_str), Some("start"));
        assert_eq!(lines.last().map(String::as_str), Some(out.content.as_str()));
        assert_eq!(supervisor.running_count(), 1);
        let snapshot = supervisor.snapshot_output(&id).unwrap();
        assert!(snapshot.running);
        assert!(snapshot.text.contains("start"));
        let _ = supervisor.stop(&id).await;
    }

    #[tokio::test]
    async fn streaming_timeout_still_applies_after_output_closes() {
        let out = tokio::time::timeout(
            Duration::from_secs(2),
            run_test_job(
                "exec >/dev/null 2>/dev/null; sleep 5",
                JobRunConfig {
                    timeout: Some(Duration::from_millis(100)),
                    shell: ShellSpec::default(),
                    cwd: test_cwd(),
                    started_at: Instant::now(),
                    cancel: None,
                    background_on_timeout: false,
                    detachable: false,
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
    async fn cancelling_foreground_job_terminates_and_removes_it() {
        let supervisor = JobSupervisor::process_group_only();
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let supervisor = supervisor.clone();
            let cancel = cancel.clone();
            async move {
                supervisor
                    .run(
                        "sleep 30",
                        JobRunConfig {
                            timeout: None,
                            shell: ShellSpec::default(),
                            cwd: test_cwd(),
                            started_at: Instant::now(),
                            cancel: Some(cancel),
                            background_on_timeout: false,
                            detachable: false,
                        },
                        |_| {},
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while supervisor.running_count() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("foreground job did not start");

        cancel.cancel();
        let out = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cancelled job did not stop")
            .unwrap();

        assert_eq!(out.termination, Some(JobTermination::Stopped));
        assert_eq!(out.content, "cancelled");
        assert!(supervisor.list().is_empty());
    }

    #[tokio::test]
    async fn stopping_foreground_job_preserves_final_output_for_follower() {
        let supervisor = JobSupervisor::process_group_only();
        let handle = tokio::spawn({
            let supervisor = supervisor.clone();
            async move {
                supervisor
                    .run(
                        "echo started; sleep 30",
                        JobRunConfig {
                            timeout: None,
                            shell: ShellSpec::default(),
                            cwd: test_cwd(),
                            started_at: Instant::now(),
                            cancel: None,
                            background_on_timeout: false,
                            detachable: false,
                        },
                        |_| {},
                    )
                    .await
            }
        });
        let id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(job) = supervisor.list().into_iter().next() {
                    break job.id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("foreground job did not start");

        let stopped = supervisor.stop(&id).await.unwrap();
        let followed = handle.await.unwrap();

        assert!(stopped.text.contains("started"));
        assert!(followed.content.contains("started"));
        assert_eq!(followed.termination, Some(JobTermination::Stopped));
        assert!(supervisor.list().is_empty());
    }

    #[tokio::test]
    async fn clearing_foreground_job_finishes_follower_and_hides_job() {
        let supervisor = JobSupervisor::process_group_only();
        let handle = tokio::spawn({
            let supervisor = supervisor.clone();
            async move {
                supervisor
                    .run(
                        "echo started; sleep 30",
                        JobRunConfig {
                            timeout: None,
                            shell: ShellSpec::default(),
                            cwd: test_cwd(),
                            started_at: Instant::now(),
                            cancel: None,
                            background_on_timeout: false,
                            detachable: false,
                        },
                        |_| {},
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while supervisor.list().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("foreground job did not start");

        supervisor.clear();
        assert!(supervisor.list().is_empty());
        assert_eq!(supervisor.running_count(), 0);

        let followed = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cleared foreground job did not finish")
            .unwrap();
        assert!(followed.content.contains("started"));
        assert_eq!(followed.termination, Some(JobTermination::Stopped));
        assert!(supervisor.0.jobs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn streaming_foreground_can_become_background() {
        let supervisor = JobSupervisor::process_group_only();
        let handle = tokio::spawn({
            let supervisor = supervisor.clone();
            async move {
                supervisor
                    .run(
                        "echo start; sleep 5",
                        JobRunConfig {
                            timeout: Some(Duration::from_secs(5)),
                            shell: ShellSpec::default(),
                            cwd: test_cwd(),
                            started_at: Instant::now(),
                            cancel: None,
                            background_on_timeout: false,
                            detachable: true,
                        },
                        |_| {},
                    )
                    .await
            }
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if supervisor.detach_latest_foreground().requested() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "streaming job never registered as detachable"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let out = handle.await.unwrap();
        let id = out.background_id.expect("background job id");
        assert!(!out.timed_out);
        assert!(!out.is_error);
        assert_eq!(supervisor.running_count(), 1);

        let snapshot =
            wait_for_snapshot(&supervisor, &id, |snapshot| snapshot.text.contains("start")).await;
        assert!(snapshot.running);
        let _ = supervisor.stop(&id).await;
    }

    #[tokio::test]
    async fn foreground_completion_does_not_send_background_notification() {
        let supervisor = JobSupervisor::process_group_only();
        let (tx, mut rx) = mpsc::unbounded_channel();
        supervisor.set_completion_sender(tx);

        let out = supervisor
            .run(
                "echo done",
                JobRunConfig {
                    timeout: Some(Duration::from_secs(2)),
                    shell: ShellSpec::default(),
                    cwd: test_cwd(),
                    started_at: Instant::now(),
                    cancel: None,
                    background_on_timeout: false,
                    detachable: false,
                },
                |_| {},
            )
            .await;

        assert_eq!(out.exit_code, Some(0));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn detach_wins_race_with_completion_and_notifies_once() {
        let supervisor = JobSupervisor::process_group_only();
        let (tx, mut rx) = mpsc::unbounded_channel();
        supervisor.set_completion_sender(tx);
        let mut follower = supervisor
            .start_foreground(
                "echo done",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
                true,
            )
            .await
            .unwrap();
        while follower.finished_rx.borrow().is_none() {
            follower.finished_rx.changed().await.unwrap();
        }

        assert_eq!(
            supervisor.detach_latest_foreground(),
            DetachResult::Requested
        );
        assert!(matches!(follower.next_event().await, FollowEvent::Detached));
        follower.background();

        let completion = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completion.id, follower.id);
        assert_eq!(completion.termination, JobTermination::Exited);
        assert!(rx.try_recv().is_err());
        let _ = supervisor.drain_output(&follower.id).unwrap();
    }

    #[tokio::test]
    async fn supervisor_uses_opaque_job_ids() {
        let supervisor = JobSupervisor::process_group_only();
        let id = supervisor
            .spawn_background(
                "sleep 5",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
            )
            .await
            .unwrap();
        let snapshot = supervisor.snapshot_output(&id).unwrap();

        assert!(id.starts_with("proc_"));
        assert_ne!(
            Some(id.as_str()),
            snapshot.pid.map(|pid| pid.to_string()).as_deref()
        );
        assert_eq!(supervisor.running_count(), 1);
        let _ = supervisor.stop(&id).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_child_cannot_open_controlling_terminal() {
        let out = run_test_job(
            "cat /dev/tty",
            JobRunConfig {
                timeout: Some(Duration::from_secs(1)),
                shell: ShellSpec::default(),
                cwd: test_cwd(),
                started_at: Instant::now(),
                cancel: None,
                background_on_timeout: false,
                detachable: false,
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
        let out = run_test_job(
            &command,
            JobRunConfig {
                timeout: Some(Duration::from_secs(5)),
                shell: ShellSpec::default(),
                cwd: test_cwd(),
                started_at: Instant::now(),
                cancel: None,
                background_on_timeout: false,
                detachable: false,
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
    async fn supervisor_reports_natural_completion_and_keeps_snapshot() {
        let supervisor = JobSupervisor::process_group_only();
        let (tx, mut rx) = mpsc::unbounded_channel();
        supervisor.set_completion_sender(tx);
        let id = supervisor
            .spawn_background(
                "echo done",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
            )
            .await
            .unwrap();

        let completion = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completion.id, id);
        assert_eq!(completion.exit_code, Some(0));
        assert_eq!(completion.termination, JobTermination::Exited);

        let snapshot = wait_for_snapshot(&supervisor, &id, |out| !out.running).await;
        assert!(!snapshot.running);
        assert_eq!(snapshot.exit_code, Some(0));
        assert_eq!(snapshot.termination, Some(JobTermination::Exited));
        assert!(snapshot.text.contains("done"));
        assert_eq!(supervisor.running_count(), 0);
    }

    #[tokio::test]
    async fn supervisor_snapshot_preserves_background_unicode_output() {
        let supervisor = JobSupervisor::process_group_only();
        let id = supervisor
            .spawn_background(
                "printf 'Commande 10551 confirme'; printf '\\314'; sleep 0.01; printf '\\201e.pdf 🇨🇦\\n'",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
            )
            .await
            .unwrap();

        let snapshot = wait_for_snapshot(&supervisor, &id, |out| !out.running).await;

        assert_eq!(snapshot.text, "Commande 10551 confirme\u{301}e.pdf 🇨🇦");
        assert_eq!(snapshot.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_detects_external_process_exit() {
        let supervisor = JobSupervisor::process_group_only();
        let (tx, mut rx) = mpsc::unbounded_channel();
        supervisor.set_completion_sender(tx);
        let id = supervisor
            .spawn_background(
                "echo ready; sleep 30",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
            )
            .await
            .unwrap();
        let snapshot = wait_for_snapshot(&supervisor, &id, |out| out.text.contains("ready")).await;
        let pid = snapshot.pid.expect("spawned job has pid");

        // SAFETY: the process-group backend starts the job in a process group
        // whose id matches the shell pid.
        unsafe {
            libc::kill(-(i32::try_from(pid).unwrap()), libc::SIGTERM);
        }

        let completion = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completion.id, id);
        assert_eq!(completion.exit_code, None);
        assert_eq!(completion.termination, JobTermination::Signaled);

        let snapshot = wait_for_snapshot(&supervisor, &id, |out| !out.running).await;
        assert!(!snapshot.running);
        assert_eq!(snapshot.exit_code, None);
        assert_eq!(snapshot.termination, Some(JobTermination::Signaled));
        assert_eq!(supervisor.running_count(), 0);
    }

    #[tokio::test]
    async fn supervisor_stop_kills_job_and_returns_buffered_output() {
        let supervisor = JobSupervisor::process_group_only();
        let id = supervisor
            .spawn_background(
                "echo started; sleep 30",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
            )
            .await
            .unwrap();
        wait_for_snapshot(&supervisor, &id, |out| out.text.contains("started")).await;

        let output = tokio::time::timeout(Duration::from_secs(2), supervisor.stop(&id))
            .await
            .unwrap()
            .unwrap();
        assert!(output.text.contains("started"));
        assert_eq!(output.termination, Some(JobTermination::Stopped));
        assert!(supervisor.snapshot_output(&id).is_err());
        assert_eq!(supervisor.running_count(), 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn systemd_control_timeout_kills_and_reaps_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("systemd-command.pid");
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(format!(
            "echo $$ > {}; sleep 30",
            shell_quote_path(&pid_file)
        ));

        let error = run_status_with_timeout(command, Duration::from_millis(100))
            .await
            .unwrap_err();
        let pid = std::fs::read_to_string(pid_file)
            .expect("timed command wrote pid")
            .trim()
            .parse::<u32>()
            .unwrap();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        wait_for_process_exit(pid).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn systemd_scope_reports_cgroup_out_of_memory() {
        if !probe_systemd_scope().await
            || !std::process::Command::new("python3")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            return;
        }

        let supervisor = JobSupervisor::with_memory_limit(32 * 1024 * 1024);
        let id = supervisor
            .spawn_background(
                "python3 -c 'chunks=[]; exec(\"while True:\\n chunks.append(bytearray(4 * 1024 * 1024))\")'",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
            )
            .await
            .unwrap();
        let snapshot = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = supervisor.snapshot_output(&id).unwrap();
                if !snapshot.running {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("memory-limited scope did not finish");

        assert_eq!(snapshot.termination, Some(JobTermination::OutOfMemory));
        assert_eq!(snapshot.exit_code, None);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn systemd_scope_preserves_shell_dollar_expansion() {
        if !probe_systemd_scope().await {
            return;
        }

        let supervisor = JobSupervisor::new();
        let id = supervisor
            .spawn_background(
                "value=ok; printf '%s:%s' \"$value\" \"$$\"",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
            )
            .await
            .unwrap();
        let snapshot = wait_for_snapshot(&supervisor, &id, |output| !output.running).await;
        let (value, pid) = snapshot.text.split_once(':').unwrap();

        assert_eq!(value, "ok");
        assert!(pid.parse::<u32>().is_ok(), "unexpected shell pid: {pid:?}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn systemd_scope_distinguishes_ordinary_sigkill_from_oom() {
        if !probe_systemd_scope().await {
            return;
        }

        let supervisor = JobSupervisor::new();
        let id = supervisor
            .spawn_background(
                "kill -KILL $$",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
            )
            .await
            .unwrap();
        let snapshot = wait_for_snapshot(&supervisor, &id, |output| !output.running).await;

        assert_ne!(snapshot.termination, Some(JobTermination::OutOfMemory));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn systemd_scope_stop_kills_descendant_that_created_a_new_session() {
        if !probe_systemd_scope().await
            || !std::process::Command::new("setsid")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("escaped.pid");
        let command = format!(
            "setsid sh -c 'echo $$ > \"$1\"; exec sleep 30' sh {} >/dev/null 2>&1 & wait",
            shell_quote_path(&pid_file)
        );
        let supervisor = JobSupervisor::new();
        let id = supervisor
            .spawn_background(&command, &ShellSpec::default(), &test_cwd(), Instant::now())
            .await
            .unwrap();
        let escaped_pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "escaped descendant did not write its pid: {:?}",
                std::fs::read_to_string(&pid_file)
            )
        });
        assert!(process_exists(escaped_pid));

        supervisor.stop(&id).await.unwrap();
        wait_for_process_exit(escaped_pid).await;
    }

    #[test]
    fn job_elapsed_secs_freezes_at_finished_at() {
        let supervisor = JobSupervisor::new();
        let started_at = Instant::now() - Duration::from_secs(10);
        {
            let mut map = supervisor.0.jobs.lock().unwrap();
            map.insert(
                "done".into(),
                job_fixture(
                    Vec::new(),
                    true,
                    Some(0),
                    started_at,
                    Some(started_at + Duration::from_secs(3)),
                ),
            );
        }

        let first = supervisor.snapshot_output("done").unwrap().elapsed_secs;
        std::thread::sleep(Duration::from_millis(20));
        let second = supervisor.snapshot_output("done").unwrap().elapsed_secs;

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

    // ── JobSupervisor ───────────────────────────────────────────────

    #[test]
    fn supervisor_new_and_default_start_empty() {
        let r1 = JobSupervisor::new();
        let r2 = JobSupervisor::default();
        assert_eq!(r1.running_count(), 0);
        assert_eq!(r2.running_count(), 0);
        assert!(r1.list().is_empty());
    }

    #[test]
    fn supervisor_next_id_is_monotonic_and_unique() {
        let r = JobSupervisor::new();
        let id1 = r.next_id();
        let id2 = r.next_id();
        assert!(id1.starts_with("proc_"));
        assert!(id2.starts_with("proc_"));
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn supervisor_rejects_jobs_above_the_running_limit() {
        let supervisor = JobSupervisor::process_group_only();
        let permits = (0..MAX_RUNNING_JOBS)
            .map(|_| {
                Arc::clone(&supervisor.0.running_slots)
                    .try_acquire_owned()
                    .expect("test reserves running slot")
            })
            .collect::<Vec<_>>();

        let error = supervisor
            .spawn_background("true", &ShellSpec::default(), &test_cwd(), Instant::now())
            .await
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("shell job limit reached"));
        drop(permits);
    }

    #[test]
    fn completed_job_cache_is_bounded_by_count_and_memory() {
        let supervisor = JobSupervisor::new();
        let started_at = Instant::now() - Duration::from_secs(1);
        let newest_id = format!("proc_{}", MAX_COMPLETED_JOBS);
        {
            let mut jobs = supervisor.0.jobs.lock().unwrap();
            for index in 0..=MAX_COMPLETED_JOBS {
                let id = format!("proc_{index}");
                let mut job = job_fixture(
                    vec![],
                    true,
                    Some(0),
                    started_at,
                    Some(started_at + Duration::from_millis(index as u64)),
                );
                job.command = "x".repeat(DEFAULT_MAX_BYTES);
                job.output.push_line("y".repeat(DEFAULT_MAX_BYTES));
                jobs.insert(id, job);
            }
            prune_completed_jobs(&mut jobs, &newest_id);

            let retained_bytes = jobs.values().map(Job::retained_memory_bytes).sum::<usize>();
            assert!(jobs.len() <= MAX_COMPLETED_JOBS);
            assert!(retained_bytes <= MAX_COMPLETED_JOB_BYTES);
            assert!(jobs.contains_key(&newest_id));
            assert!(!jobs.contains_key("proc_0"));
        }
    }

    #[test]
    fn supervisor_read_unknown_id_returns_error() {
        let r = JobSupervisor::new();
        let err = r.drain_output("no_such_proc").unwrap_err();
        assert!(err.contains("no_such_proc"));
    }

    #[tokio::test]
    async fn supervisor_stop_unknown_id_returns_error() {
        let r = JobSupervisor::new();
        let err = r.stop("nope").await.unwrap_err();
        assert!(err.contains("nope"));
    }

    #[test]
    fn supervisor_clear_empties_running_count_and_list() {
        let r = JobSupervisor::new();
        r.clear();
        assert_eq!(r.running_count(), 0);
        assert!(r.list().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_clear_kills_running_job_immediately() {
        let supervisor = JobSupervisor::process_group_only();
        let id = supervisor
            .spawn_background(
                "sleep 30",
                &ShellSpec::default(),
                &test_cwd(),
                Instant::now(),
            )
            .await
            .unwrap();
        let pid = supervisor
            .snapshot_output(&id)
            .unwrap()
            .pid
            .expect("spawned job has pid");
        assert_eq!(supervisor.running_count(), 1);

        supervisor.clear();

        assert_eq!(supervisor.running_count(), 0);
        assert!(supervisor.list().is_empty());
        wait_for_process_exit(pid).await;
    }

    #[test]
    fn job_info_lists_running_jobs_in_id_order() {
        // Insert synthetic entries directly via the lock so we don't spawn real children.
        let r = JobSupervisor::new();
        {
            let mut map = r.0.jobs.lock().unwrap();
            for i in ["proc_b", "proc_a", "proc_c"] {
                map.insert(
                    i.into(),
                    job_fixture(Vec::new(), false, None, Instant::now(), None),
                );
            }
        }
        let infos = r.list();
        let ids: Vec<&str> = infos.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["proc_a", "proc_b", "proc_c"]);
        assert_eq!(r.running_count(), 3);
    }

    #[test]
    fn job_list_filters_out_finished_entries() {
        let r = JobSupervisor::new();
        {
            let mut map = r.0.jobs.lock().unwrap();
            map.insert(
                "live".into(),
                job_fixture(Vec::new(), false, None, Instant::now(), None),
            );
            map.insert(
                "dead".into(),
                job_fixture(Vec::new(), true, Some(0), Instant::now(), None),
            );
        }
        let listing = r.list();
        let ids: Vec<&str> = listing.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["live"]);
        assert_eq!(r.running_count(), 1);
    }

    #[test]
    fn job_read_drains_lines_and_removes_finished_entry() {
        let r = JobSupervisor::new();
        {
            let mut map = r.0.jobs.lock().unwrap();
            map.insert(
                "p1".into(),
                job_fixture(vec!["a", "b"], true, Some(0), Instant::now(), None),
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
    fn job_read_keeps_entry_when_still_running() {
        let r = JobSupervisor::new();
        {
            let mut map = r.0.jobs.lock().unwrap();
            map.insert(
                "p1".into(),
                job_fixture(vec!["a"], false, None, Instant::now(), None),
            );
        }
        let out = r.drain_output("p1").unwrap();
        assert_eq!(out.text, "a");
        assert!(out.running);
        assert_eq!(out.exit_code, None);
        // Entry is still registered with drained lines.
        let map = r.0.jobs.lock().unwrap();
        assert!(map.get("p1").is_some());
        assert_eq!(map.get("p1").unwrap().output.retained_lines(), 0);
    }
}
