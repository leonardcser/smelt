//! Grep capability — thin async wrapper over `rg`. Pure subprocess
//! composition, no policy. Missing/failed `rg` surfaces as `io::Error`;
//! fallback is the caller's concern.

use crate::process::wait_child;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    #[default]
    Content,
    FilesWithMatches,
    Count,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    pub(crate) mode: Mode,
    pub(crate) case_insensitive: bool,
    pub(crate) multiline: bool,
    pub(crate) line_numbers: bool,
    pub(crate) before_context: u32,
    pub(crate) after_context: u32,
    pub(crate) context: u32,
    pub(crate) glob: Option<String>,
    pub(crate) file_type: Option<String>,
    pub(crate) timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub(crate) struct Output {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
    pub(crate) timed_out: bool,
}

/// Result of [`run_async`]: `Done` for natural completion or timeout,
/// `Cancelled` when the cancellation token fired and the child was
/// killed before producing a status.
pub(crate) enum RunOutcome {
    Done(Output),
    Cancelled,
}

/// Run `rg <pattern> <path>` with the given options, honoring a
/// `CancellationToken`. `path` defaults to `.` when empty. The child is
/// killed (SIGKILL via `start_kill`) on cancel or timeout, and the
/// future resolves once `wait()` completes.
pub(crate) async fn run_async(
    pattern: &str,
    path: impl AsRef<Path>,
    opts: &Options,
    cancel: CancellationToken,
) -> io::Result<RunOutcome> {
    let path: PathBuf = {
        let p = path.as_ref();
        if p.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            p.to_path_buf()
        }
    };

    let mut args: Vec<String> = Vec::new();

    match opts.mode {
        Mode::Content => {
            if opts.line_numbers {
                args.push("--line-number".into());
            }
            if opts.before_context > 0 {
                args.push(format!("--before-context={}", opts.before_context));
            }
            if opts.after_context > 0 {
                args.push(format!("--after-context={}", opts.after_context));
            }
            if opts.context > 0 {
                args.push(format!("--context={}", opts.context));
            }
        }
        Mode::FilesWithMatches => args.push("--files-with-matches".into()),
        Mode::Count => args.push("--count".into()),
    }

    if opts.case_insensitive {
        args.push("--ignore-case".into());
    }
    if opts.multiline {
        args.push("--multiline".into());
        args.push("--multiline-dotall".into());
    }
    if let Some(g) = &opts.glob {
        args.push(format!("--glob={g}"));
    }
    if let Some(t) = &opts.file_type {
        args.push(format!("--type={t}"));
    }

    args.push("--".into());
    args.push(pattern.to_string());
    args.push(path.to_string_lossy().into_owned());

    let mut child = tokio::process::Command::new("rg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        buf
    });

    let timeout = opts.timeout.unwrap_or(Duration::from_secs(30));
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = wait_child(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            Ok(RunOutcome::Cancelled)
        }
        _ = &mut deadline => {
            let _ = child.start_kill();
            let _ = wait_child(&mut child).await;
            let stdout_buf = stdout_task.await.unwrap_or_default();
            let stderr_buf = stderr_task.await.unwrap_or_default();
            let stderr_msg = if stderr_buf.is_empty() {
                format!("rg timed out after {}s", timeout.as_secs())
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
        status = wait_child(&mut child) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn rg_available() -> bool {
        std::process::Command::new("rg")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn run(pattern: &str, path: &Path, opts: &Options) -> Output {
        match run_async(pattern, path, opts, CancellationToken::new())
            .await
            .unwrap()
        {
            RunOutcome::Done(out) => out,
            RunOutcome::Cancelled => panic!("unexpected cancellation"),
        }
    }

    #[tokio::test]
    async fn content_mode_finds_matches() {
        if !rg_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let opts = Options {
            line_numbers: true,
            ..Default::default()
        };
        let out = run("beta", tmp.path(), &opts).await;
        assert!(out.stdout.contains("beta"));
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn files_with_matches_lists_paths() {
        if !rg_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "alpha\n").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "beta\n").unwrap();
        let opts = Options {
            mode: Mode::FilesWithMatches,
            ..Default::default()
        };
        let out = run("alpha", tmp.path(), &opts).await;
        assert!(out.stdout.contains("a.txt"));
        assert!(out.stdout.contains("b.txt"));
        assert!(!out.stdout.contains("c.txt"));
    }

    #[tokio::test]
    async fn no_match_exit_code_is_one() {
        if !rg_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
        let out = run("zzznomatch", tmp.path(), &Options::default()).await;
        assert_eq!(out.exit_code, 1);
    }

    #[tokio::test]
    async fn run_async_returns_cancelled_when_token_fired() {
        if !rg_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = run_async("alpha", tmp.path(), &Options::default(), cancel)
            .await
            .unwrap();
        assert!(matches!(outcome, RunOutcome::Cancelled));
    }
}
