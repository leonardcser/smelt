//! `cargo xtask fuzz coverage-snapshot [target...]` — per-target source-code
//! coverage snapshot. Runs each target against its on-disk corpus and writes
//! a timestamped summary under `fuzz/coverage-history/`. With no args it
//! snapshots every target that has a corpus.

use super::{all_target_names, count_files, die, dir_has_entries, iso_utc, repo_root, stamp};
use std::io::Write;
use std::process::Command;

pub fn run(args: Vec<String>) {
    let root = repo_root();
    let llvm_cov = find_llvm_cov().unwrap_or_else(|| {
        die("llvm-cov not found in ~/.rustup/toolchains. Run: rustup component add llvm-tools-preview --toolchain nightly")
    });

    let targets: Vec<String> = if args.is_empty() {
        all_target_names().map(String::from).collect()
    } else {
        args.clone()
    };

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

        let status = Command::new("cargo")
            .args([
                "+nightly",
                "fuzz",
                "coverage",
                "--sanitizer=none",
                target,
                corpus.to_str().expect("corpus path utf-8"),
            ])
            .current_dir(&root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if status.map(|s| !s.success()).unwrap_or(true) {
            let line = format!("{target}: cargo fuzz coverage failed, skipping");
            println!("{line}");
            writeln!(summary, "{line}").ok();
            continue;
        }

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
