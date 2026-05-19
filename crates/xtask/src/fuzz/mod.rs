//! `cargo xtask fuzz <subcommand>` — fuzz tooling.

mod coverage;
mod replay_regression;
mod run;
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
    eprintln!("  run <target> [--fork N] [--cmin]   fuzz a target until crash or Ctrl-C");
    eprintln!("  triage <target> <crash-artifact>   crash → JSON → shrink → print");
    eprintln!("  replay-regression                  replay every seed under fuzz/seeds/<target>/regression/");
    eprintln!(
        "  coverage-snapshot [target...]      per-target line/function/region coverage snapshot"
    );
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

/// Every fuzz target this crate knows about. `Json` targets accept the
/// `Scenario` JSON form (replay + triage); `Bytes` targets take raw
/// libFuzzer input. Add a new target here once; every subcommand picks
/// it up automatically.
pub(super) const TARGETS: &[(&str, TargetKind)] = &[
    ("smelt_loop", TargetKind::Json),
    ("lua_loop", TargetKind::Json),
    ("text_ops", TargetKind::Bytes),
    ("attached_ops", TargetKind::Bytes),
    ("cache_invariance", TargetKind::Bytes),
    ("openai_cache_invariance", TargetKind::Bytes),
];

#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) enum TargetKind {
    Json,
    Bytes,
}

pub(super) fn all_target_names() -> impl Iterator<Item = &'static str> {
    TARGETS.iter().map(|(name, _)| *name)
}

pub(super) fn targets_of(kind: TargetKind) -> impl Iterator<Item = &'static str> {
    TARGETS
        .iter()
        .filter(move |(_, k)| *k == kind)
        .map(|(name, _)| *name)
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

/// Howard Hinnant's `civil_from_days` — convert UNIX seconds (UTC) into
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
