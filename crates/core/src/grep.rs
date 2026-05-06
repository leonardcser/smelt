//! Grep capability — thin synchronous wrapper over `rg` (ripgrep).
//! Pure subprocess composition, no policy. Exposed to Lua via
//! `crates/tui/src/lua/api/grep.rs` and composed by tools that need
//! to search a tree.
//!
//! When `rg` is missing or fails to launch, callers see an `io::Error`
//! and decide how to fall back. This module never falls back to grep
//! by itself — the engine's legacy `grep` tool keeps its own fallback
//! until it migrates to Lua in P5.b.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Output mode for `rg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    /// Default — print matching lines.
    #[default]
    Content,
    /// `--files-with-matches` — list files with at least one match.
    FilesWithMatches,
    /// `--count` — line counts per file.
    Count,
}

/// Options accepted by [`run`]. Defaults match `rg`'s defaults.
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

/// Output from a single `rg` invocation. Callers slice / paginate the
/// stdout themselves; this module returns it verbatim.
#[derive(Debug, Clone)]
pub(crate) struct Output {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
    pub(crate) timed_out: bool,
}

/// Run `rg <pattern> <path>` with the given options. `path` defaults
/// to `.` when empty.
pub(crate) fn run(pattern: &str, path: impl AsRef<Path>, opts: &Options) -> io::Result<Output> {
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

    let mut child = Command::new("rg")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

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
                        stderr: format!("rg timed out after {}s", timeout.as_secs()),
                        exit_code: -1,
                        timed_out: true,
                    });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn rg_available() -> bool {
        Command::new("rg")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn content_mode_finds_matches() {
        if !rg_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let opts = Options {
            line_numbers: true,
            ..Default::default()
        };
        let out = run("beta", tmp.path(), &opts).unwrap();
        assert!(out.stdout.contains("beta"));
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
    }

    #[test]
    fn files_with_matches_lists_paths() {
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
        let out = run("alpha", tmp.path(), &opts).unwrap();
        assert!(out.stdout.contains("a.txt"));
        assert!(out.stdout.contains("b.txt"));
        assert!(!out.stdout.contains("c.txt"));
    }

    #[test]
    fn no_match_exit_code_is_one() {
        if !rg_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
        let out = run("zzznomatch", tmp.path(), &Options::default()).unwrap();
        assert_eq!(out.exit_code, 1);
    }
}
