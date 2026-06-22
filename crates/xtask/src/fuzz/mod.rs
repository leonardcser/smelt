//! `cargo xtask fuzz <subcommand>` - fuzz tooling.

mod coverage;
mod replay_regression;
mod run;
mod status;
mod triage;

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(args: Vec<String>) {
    let mut it = args.into_iter();
    let sub = it.next();
    let rest: Vec<String> = it.collect();
    match sub.as_deref() {
        None => {
            print_usage();
            std::process::exit(2);
        }
        Some("run") => run::run(rest),
        Some("status") => status::run(rest),
        Some("build") => build(rest),
        Some("triage") => triage::run(rest),
        Some("replay-regression") => replay_regression::run(rest),
        Some("coverage-snapshot") => coverage::run(rest),
        Some(other) => {
            eprintln!("xtask fuzz: unknown subcommand `{other}`");
            print_usage();
            std::process::exit(2);
        }
    }
}

pub fn print_usage() {
    eprintln!("usage: cargo xtask fuzz <subcommand> [args]");
    eprintln!();
    eprintln!("subcommands:");
    eprintln!(
        "  run <target> [--fork N] [--cmin]   fuzz a target until crash/OOM/timeout or Ctrl-C"
    );
    eprintln!(
        "  status                              summarize corpora, seeds, artifacts, coverage"
    );
    eprintln!("  build [target...]                    build real fuzz targets one-by-one");
    eprintln!("  triage <target> <crash-artifact>   crash → JSON → shrink → print");
    eprintln!("  replay-regression                  replay every seed under fuzz/seeds/<target>/regression/");
    eprintln!("  coverage-snapshot [--timeout SECONDS] [target...]  per-target coverage snapshot");
}

fn build(args: Vec<String>) {
    let known = all_target_names();
    let targets: Vec<String> = if args.is_empty() {
        known.clone()
    } else {
        for target in &args {
            if !known.contains(target) {
                die(&format!(
                    "unknown target `{target}`. Known: {}",
                    known.join(", ")
                ));
            }
        }
        args
    };

    let root = repo_root();
    for target in targets {
        step(
            &format!("build {target}"),
            Command::new("cargo")
                .args(["+nightly", "fuzz", "build", "--sanitizer=none", &target])
                .current_dir(&root),
        );
    }
}

pub(super) fn step(label: &str, cmd: &mut Command) {
    println!("xtask fuzz: {label}");
    let status = cmd
        .status()
        .unwrap_or_else(|e| die(&format!("{label}: spawn failed: {e}")));
    if !status.success() {
        die_with_status(&format!("{label}: exit {status}"), status.code());
    }
}

pub(super) fn die(msg: &str) -> ! {
    eprintln!("xtask fuzz: {msg}");
    std::process::exit(2);
}

pub(super) fn die_with_status(msg: &str, code: Option<i32>) -> ! {
    eprintln!("xtask fuzz: {msg}");
    std::process::exit(code.unwrap_or(1));
}

pub(super) fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("xtask is at crates/xtask/")
        .to_path_buf()
}

/// True if `dir` exists and is non-empty (any file or sub-entry).
pub(super) fn dir_has_entries(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut it) => it.next().is_some(),
        Err(_) => false,
    }
}

/// Number of regular files directly under `dir` (non-recursive). `0` if `dir`
/// is missing.
pub(super) fn count_files(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(it) => it.filter_map(|e| e.ok()).count(),
        Err(_) => 0,
    }
}

/// Fuzz targets with JSON scenario support. Byte targets are discovered from
/// `fuzz/Cargo.toml` by selecting bins whose path is under `fuzz_targets/`.
pub(super) const JSON_TARGETS: &[&str] = &["smelt_loop", "lua_loop"];

#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) enum TargetKind {
    Json,
    Bytes,
}

pub(super) fn all_target_names() -> Vec<String> {
    fuzz_manifest_targets()
}

pub(super) fn targets_of(kind: TargetKind) -> Vec<String> {
    all_target_names()
        .into_iter()
        .filter(|name| {
            let is_json = JSON_TARGETS.contains(&name.as_str());
            match kind {
                TargetKind::Json => is_json,
                TargetKind::Bytes => !is_json,
            }
        })
        .collect()
}

fn fuzz_manifest_targets() -> Vec<String> {
    let manifest = repo_root().join("fuzz/Cargo.toml");
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
        .unwrap_or_else(|e| {
            die(&format!(
                "spawn cargo metadata for {}: {e}",
                manifest.display()
            ))
        });
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        die(&format!(
            "cargo metadata failed for {}: {stderr}",
            manifest.display()
        ));
    }

    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| die(&format!("parse cargo metadata: {e}")));
    let packages = metadata
        .get("packages")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| die("cargo metadata missing packages"));
    let package = packages
        .iter()
        .find(|package| package.get("name").and_then(|v| v.as_str()) == Some("smelt-fuzz"))
        .unwrap_or_else(|| die("cargo metadata missing smelt-fuzz package"));
    let targets = package
        .get("targets")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| die("cargo metadata missing targets"));

    let names: Vec<String> = targets
        .iter()
        .filter(|target| metadata_target_is_fuzz_bin(target))
        .filter_map(|target| {
            target
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();

    if names.is_empty() {
        die("no fuzz targets found in cargo metadata");
    }
    names
}

fn metadata_target_is_fuzz_bin(target: &serde_json::Value) -> bool {
    let is_bin = target
        .get("kind")
        .and_then(|v| v.as_array())
        .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")));
    if !is_bin {
        return false;
    }
    target
        .get("src_path")
        .and_then(|v| v.as_str())
        .map(|path| path.replace('\\', "/").contains("/fuzz_targets/"))
        .unwrap_or(false)
}

/// `YYYYMMDD-HHMMSS` UTC stamp without pulling chrono into xtask.
pub(super) fn stamp() -> String {
    let (y, mo, d, h, mi, s) = unix_to_civil(unix_now());
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

pub(super) fn iso_utc() -> String {
    let (y, mo, d, h, mi, s) = unix_to_civil(unix_now());
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Howard Hinnant's `civil_from_days` - convert UNIX seconds (UTC) into
/// `(year, month, day, hour, minute, second)` without a dependency.
fn unix_to_civil(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let day = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let hour = tod / 3600;
    let min = (tod / 60) % 60;
    let sec = tod % 60;
    let z = day + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hour, min, sec)
}
