//! `cargo xtask fuzz coverage-snapshot [target...]` - per-target source-code
//! coverage snapshot. Runs each target against its on-disk corpus and writes
//! a timestamped summary under `fuzz/coverage-history/`. With no args it
//! snapshots every target that has a corpus.

use super::{all_target_names, count_files, die, dir_has_entries, iso_utc, repo_root, stamp};
use std::fs::File;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn run(args: Vec<String>) {
    let root = repo_root();
    let llvm_cov = find_llvm_cov().unwrap_or_else(|| {
        die("llvm-cov not found in ~/.rustup/toolchains. Run: rustup component add llvm-tools-preview --toolchain nightly")
    });

    let (targets, timeout) = parse_args(args);

    let hist_dir = root.join("fuzz/coverage-history");
    std::fs::create_dir_all(&hist_dir).unwrap_or_else(|e| die(&format!("mkdir history dir: {e}")));

    let ts = stamp();
    let sha = git_short_sha(&root).unwrap_or_else(|| "unknown".to_string());
    let head_full = git_head(&root).unwrap_or_else(|| "unknown".to_string());
    let branch = git_branch(&root).unwrap_or_else(|| "unknown".to_string());

    let summary_path = hist_dir.join(format!("{ts}-{sha}.txt"));
    let mut summary = std::fs::File::create(&summary_path)
        .unwrap_or_else(|e| die(&format!("create summary: {e}")));
    writeln!(summary, "# fuzz coverage snapshot").ok();
    writeln!(summary, "date: {}", iso_utc()).ok();
    writeln!(summary, "commit: {head_full}").ok();
    writeln!(summary, "branch: {branch}").ok();
    writeln!(summary).ok();

    for target in &targets {
        let corpus = root.join(format!("fuzz/corpus/{target}"));
        if !dir_has_entries(&corpus) {
            let line = format!("{target}: no corpus, skipping");
            println!("{line}");
            writeln!(summary, "{line}").ok();
            continue;
        }
        let nfiles = count_files(&corpus);
        eprintln!(">>> {target}: {nfiles} corpus files");

        let log_path = hist_dir.join(format!("{ts}-{sha}-{target}.log"));
        let status = run_with_timeout(
            Command::new("cargo")
                .args([
                    "+nightly",
                    "fuzz",
                    "coverage",
                    "--sanitizer=none",
                    target,
                    corpus.to_str().expect("corpus path utf-8"),
                ])
                .current_dir(&root),
            timeout,
            &log_path,
        );
        let Ok(status) = status else {
            let line = format!(
                "{target}: cargo fuzz coverage timed out after {}s, log={}",
                timeout.as_secs(),
                log_path.display()
            );
            println!("{line}");
            writeln!(summary, "{line}").ok();
            move_root_crashes(&root, target);
            continue;
        };
        if !status.success() {
            let line = format!(
                "{target}: cargo fuzz coverage failed, log={}",
                log_path.display()
            );
            println!("{line}");
            writeln!(summary, "{line}").ok();
            move_root_crashes(&root, target);
            continue;
        }
        move_root_crashes(&root, target);

        let profdata = root.join(format!("fuzz/coverage/{target}/coverage.profdata"));
        let binary = root.join(format!(
            "target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/{target}"
        ));
        if !profdata.exists() || !binary.exists() {
            let line = format!("{target}: coverage build missing, skipping");
            println!("{line}");
            writeln!(summary, "{line}").ok();
            continue;
        }

        let out = Command::new(&llvm_cov)
            .args(["report"])
            .arg(&binary)
            .arg(format!("-instr-profile={}", profdata.display()))
            .arg("-ignore-filename-regex=/.cargo/|/rustc/|/.rustup/|fuzz/")
            .output();
        let report = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => {
                let line = format!("{target}: llvm-cov report failed, skipping");
                println!("{line}");
                writeln!(summary, "{line}").ok();
                continue;
            }
        };
        let totals = report.lines().last().unwrap_or("").to_string();
        let line = format!("{target} ({nfiles}f): {totals}");
        println!("{line}");
        writeln!(summary, "{line}").ok();
    }

    eprintln!();
    eprintln!("snapshot saved to {}", summary_path.display());
}

fn parse_args(args: Vec<String>) -> (Vec<String>, Duration) {
    let mut timeout = Duration::from_secs(300);
    let mut targets = Vec::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--timeout" => {
                let v = it.next().unwrap_or_else(|| die("--timeout needs seconds"));
                let secs: u64 = v
                    .parse()
                    .unwrap_or_else(|_| die(&format!("bad --timeout `{v}`")));
                timeout = Duration::from_secs(secs.max(1));
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: cargo xtask fuzz coverage-snapshot [--timeout SECONDS] [target...]"
                );
                std::process::exit(0);
            }
            _ => targets.push(arg),
        }
    }
    if targets.is_empty() {
        targets = all_target_names().map(String::from).collect();
    }
    (targets, timeout)
}

fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    log_path: &std::path::Path,
) -> Result<std::process::ExitStatus, ()> {
    let log = File::create(log_path).unwrap_or_else(|e| die(&format!("create coverage log: {e}")));
    let stdout = log
        .try_clone()
        .unwrap_or_else(|e| die(&format!("clone coverage log: {e}")));
    let stderr = log;
    let mut child = cmd
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap_or_else(|e| die(&format!("spawn coverage: {e}")));
    let start = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .unwrap_or_else(|e| die(&format!("wait coverage: {e}")))
        {
            return Ok(status);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn move_root_crashes(root: &std::path::Path, target: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let dest = root.join(format!("fuzz/artifacts/{target}"));
    if let Err(e) = std::fs::create_dir_all(&dest) {
        eprintln!(
            ">>> {target}: failed to create artifact dir {}: {e}",
            dest.display()
        );
        return;
    }
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("crash-") || !path.is_file() {
            continue;
        }
        let mut to = dest.join(name);
        if to.exists() {
            to = dest.join(format!("coverage-{name}"));
        }
        match std::fs::rename(&path, &to) {
            Ok(()) => eprintln!(
                ">>> {target}: moved root crash {} -> {}",
                path.display(),
                to.display()
            ),
            Err(e) => eprintln!(
                ">>> {target}: failed to move root crash {} -> {}: {e}",
                path.display(),
                to.display()
            ),
        }
    }
}

fn find_llvm_cov() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let root = std::path::PathBuf::from(home).join(".rustup/toolchains");
    walk_for("llvm-cov", &root)
}

fn walk_for(name: &str, dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(hit) = walk_for(name, &path) {
                return Some(hit);
            }
        } else if path.file_name().and_then(|s| s.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

fn git_short_sha(root: &std::path::Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn git_head(root: &std::path::Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn git_branch(root: &std::path::Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}
