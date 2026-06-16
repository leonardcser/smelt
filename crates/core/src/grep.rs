//! Grep capability - thin async wrapper over `rg`. Pure subprocess
//! composition, no policy. Missing/failed `rg` surfaces as `io::Error`;
//! fallback is the caller's concern.

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
    pub(crate) include_ignored: bool,
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

fn append_hard_excludes(args: &mut Vec<String>, path: &Path) {
    let search_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    for dir in [".git", ".jj", ".hg", ".svn", ".sl", ".worktrees"] {
        if !path_contains_component(&search_path, dir) {
            args.push(format!("--glob=!**/{dir}/**"));
        }
    }
}

fn path_contains_component(path: &Path, needle: &str) -> bool {
    let needle = std::ffi::OsStr::new(needle);
    path.components()
        .any(|component| component.as_os_str() == needle)
}

fn is_noop_glob(glob: &str) -> bool {
    matches!(glob.trim(), "" | "*" | "**" | "**/*" | "./*" | "./**/*")
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
    args.push("--max-columns=500".into());

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
    if opts.include_ignored {
        args.push("--no-ignore".into());
    }
    if let Some(g) = &opts.glob {
        if !is_noop_glob(g) {
            args.push(format!("--glob={g}"));
        }
    }
    if let Some(t) = &opts.file_type {
        args.push(format!("--type={t}"));
    }

    append_hard_excludes(&mut args, &path);

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
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            Ok(RunOutcome::Cancelled)
        }
        _ = &mut deadline => {
            let _ = child.start_kill();
            let _ = child.wait().await;
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
    async fn glob_filters_do_not_search_ignored_dirs_by_default() {
        if !rg_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".ignore"), "target/\n").unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::create_dir(tmp.path().join("target")).unwrap();
        std::fs::write(tmp.path().join("src").join("a.txt"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("src").join("a.rs"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("target").join("generated.txt"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("target").join("generated.rs"), "needle\n").unwrap();

        let broad = Options {
            mode: Mode::FilesWithMatches,
            glob: Some("*".into()),
            ..Default::default()
        };
        let rust = Options {
            mode: Mode::FilesWithMatches,
            glob: Some("*.rs".into()),
            ..Default::default()
        };

        let (first, second) = tokio::join!(
            run("needle", tmp.path(), &broad),
            run("needle", tmp.path(), &broad)
        );
        for out in [first, second] {
            assert!(out.stdout.contains("src/a.txt"), "stdout: {}", out.stdout);
            assert!(
                !out.stdout.contains("target/generated.txt"),
                "glob=* must not re-include ignored build output: {}",
                out.stdout
            );
            assert_eq!(out.exit_code, 0);
            assert!(!out.timed_out);
        }

        let out = run("needle", tmp.path(), &rust).await;
        assert!(out.stdout.contains("src/a.rs"), "stdout: {}", out.stdout);
        assert!(
            !out.stdout.contains("target/generated.rs"),
            "glob=*.rs must not search ignored build output: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn include_ignored_searches_ignored_dirs() {
        if !rg_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".ignore"), "target/\n").unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::create_dir(tmp.path().join("target")).unwrap();
        std::fs::write(tmp.path().join("src").join("a.txt"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("target").join("generated.txt"), "needle\n").unwrap();
        let opts = Options {
            mode: Mode::FilesWithMatches,
            include_ignored: true,
            ..Default::default()
        };

        let out = run("needle", tmp.path(), &opts).await;
        assert!(out.stdout.contains("src/a.txt"), "stdout: {}", out.stdout);
        assert!(
            out.stdout.contains("target/generated.txt"),
            "include_ignored=true should search ignored output: {}",
            out.stdout
        );
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn explicit_path_inside_hard_excluded_dir_is_searchable() {
        if !rg_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join(".worktrees/session/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "needle\n").unwrap();

        let out = run("needle", &src, &Options::default()).await;
        assert!(out.stdout.contains("needle"), "stdout: {}", out.stdout);
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
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
