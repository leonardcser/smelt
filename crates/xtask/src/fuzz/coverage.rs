//! `cargo xtask fuzz coverage-snapshot [target...]` - per-target source-code
//! coverage snapshot. Runs each target against its shared corpus and writes a
//! timestamped text and JSON summary under the shared fuzz-data root. With no
//! args it snapshots every target that has a corpus.

use super::{
    all_target_names, count_files, die, die_with_status, iso_utc, nightly_host, repo_root, stamp,
    FuzzData,
};
use std::fs::File;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn run(args: Vec<String>) {
    let root = repo_root();
    let llvm_cov = find_llvm_cov().unwrap_or_else(|| {
        die("nightly llvm-cov not found. Run: rustup component add llvm-tools-preview --toolchain nightly")
    });
    let host = nightly_host().unwrap_or_else(|| die("could not determine the nightly host triple"));

    let (targets, timeout) = parse_args(args);
    let data = FuzzData::for_repo(&root);

    let hist_dir = data.coverage_history();
    std::fs::create_dir_all(&hist_dir).unwrap_or_else(|e| die(&format!("mkdir history dir: {e}")));

    let ts = stamp();
    let date = iso_utc();
    let sha = git_short_sha(&root).unwrap_or_else(|| "unknown".to_string());
    let head_full = git_head(&root).unwrap_or_else(|| "unknown".to_string());
    let branch = git_branch(&root).unwrap_or_else(|| "unknown".to_string());
    let mut records = Vec::new();
    let mut failed = false;

    let summary_path = hist_dir.join(format!("{ts}-{sha}.txt"));
    let mut summary = std::fs::File::create(&summary_path)
        .unwrap_or_else(|e| die(&format!("create summary: {e}")));
    writeln!(summary, "# fuzz coverage snapshot").ok();
    writeln!(summary, "date: {date}").ok();
    writeln!(summary, "commit: {head_full}").ok();
    writeln!(summary, "branch: {branch}").ok();
    writeln!(summary).ok();

    for target in &targets {
        data.prepare_target(target);
        let corpus = data.corpus(target);
        let nfiles = count_files(&corpus);
        let digest = corpus_digest(&corpus);
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
                .arg("--")
                .arg(format!("-artifact_prefix={}", data.artifact_prefix(target)))
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
            records.push(coverage_record(target, nfiles, &digest, "timeout", None));
            failed = true;
            move_root_crashes(&root, &data, target);
            continue;
        };
        if !status.success() {
            let line = format!(
                "{target}: cargo fuzz coverage failed, log={}",
                log_path.display()
            );
            println!("{line}");
            writeln!(summary, "{line}").ok();
            records.push(coverage_record(target, nfiles, &digest, "failed", None));
            failed = true;
            move_root_crashes(&root, &data, target);
            continue;
        }
        move_root_crashes(&root, &data, target);

        let profdata = root.join(format!("fuzz/coverage/{target}/coverage.profdata"));
        let binary = root.join(format!("target/{host}/coverage/{host}/release/{target}"));
        if !profdata.exists() || !binary.exists() {
            let line = format!("{target}: coverage build missing, skipping");
            println!("{line}");
            writeln!(summary, "{line}").ok();
            records.push(coverage_record(
                target,
                nfiles,
                &digest,
                "missing_build",
                None,
            ));
            failed = true;
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
                records.push(coverage_record(
                    target,
                    nfiles,
                    &digest,
                    "llvm_cov_failed",
                    None,
                ));
                failed = true;
                continue;
            }
        };
        let totals = report.lines().last().unwrap_or("").to_string();
        let line = format!("{target} ({nfiles}f): {totals}");
        println!("{line}");
        writeln!(summary, "{line}").ok();
        match llvm_cov_totals(&llvm_cov, &binary, &profdata) {
            Some(totals) => {
                records.push(coverage_record(target, nfiles, &digest, "ok", Some(totals)))
            }
            None => {
                let error = format!("{target}: llvm-cov JSON export failed");
                println!("{error}");
                writeln!(summary, "{error}").ok();
                failed = true;
                records.push(coverage_record(
                    target,
                    nfiles,
                    &digest,
                    "llvm_cov_export_failed",
                    None,
                ));
            }
        }
    }

    let json_path = hist_dir.join(format!("{ts}-{sha}.json"));
    let document = serde_json::json!({
        "schema": 1,
        "date": date,
        "commit": head_full,
        "branch": branch,
        "data_root": data.root,
        "targets": records,
    });
    let body = serde_json::to_vec_pretty(&document)
        .unwrap_or_else(|e| die(&format!("serialize coverage summary: {e}")));
    std::fs::write(&json_path, body)
        .unwrap_or_else(|e| die(&format!("write {}: {e}", json_path.display())));

    eprintln!();
    eprintln!("coverage summary: {}", summary_path.display());
    eprintln!("coverage metadata: {}", json_path.display());
    if failed {
        die_with_status("one or more coverage targets failed", Some(1));
    }
}

fn coverage_record(
    target: &str,
    corpus_files: usize,
    corpus_digest: &str,
    status: &str,
    totals: Option<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "target": target,
        "status": status,
        "corpus_files": corpus_files,
        "corpus_digest": corpus_digest,
        "totals": totals,
    })
}

fn llvm_cov_totals(
    llvm_cov: &std::path::Path,
    binary: &std::path::Path,
    profdata: &std::path::Path,
) -> Option<serde_json::Value> {
    let output = Command::new(llvm_cov)
        .args(["export", "--summary-only"])
        .arg(binary)
        .arg(format!("-instr-profile={}", profdata.display()))
        .arg("-ignore-filename-regex=/.cargo/|/rustc/|/.rustup/|fuzz/")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    value
        .get("data")?
        .as_array()?
        .first()?
        .get("totals")
        .cloned()
}

fn corpus_digest(corpus: &std::path::Path) -> String {
    let Ok(entries) = std::fs::read_dir(corpus) else {
        return "empty".to_string();
    };
    let mut files: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();

    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for path in files {
        if let Some(name) = path.file_name() {
            hash_bytes(&mut hash, name.to_string_lossy().as_bytes());
        }
        match std::fs::read(&path) {
            Ok(bytes) => hash_bytes(&mut hash, &bytes),
            Err(_) => return "unavailable".to_string(),
        }
    }
    format!("fnv1a64:{hash:016x}")
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
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
    let known = all_target_names();
    if targets.is_empty() {
        targets = known;
    } else {
        for target in &targets {
            if !known.contains(target) {
                die(&format!(
                    "unknown target `{target}`. Known: {}",
                    known.join(", ")
                ));
            }
        }
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

fn move_root_crashes(root: &std::path::Path, data: &FuzzData, target: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let dest = data.artifacts(target);
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
    let output = Command::new("rustc")
        .args(["+nightly", "--print", "sysroot"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sysroot = std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    walk_for("llvm-cov", &sysroot)
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
