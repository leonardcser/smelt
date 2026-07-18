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
        Some("prepare") => prepare(rest),
        Some("verify") => verify(rest),
        Some("import-data") => import_data(rest),
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
    eprintln!("  prepare                              initialize shared data for every target");
    eprintln!("  verify                               prepare, build, and replay every target");
    eprintln!("  import-data [fuzz-dir]               merge legacy corpus and artifacts into shared storage");
    eprintln!("  triage <target> <crash-artifact>     minimize and describe a failure artifact");
    eprintln!("  replay-regression                    replay every tracked regression seed");
    eprintln!("  coverage-snapshot [--timeout SECONDS] [target...]  per-target coverage snapshot");
}

fn prepare(args: Vec<String>) {
    if !args.is_empty() {
        die("usage: cargo xtask fuzz prepare");
    }
    let root = repo_root();
    let data = FuzzData::for_repo(&root);
    let targets = all_target_names();
    for target in &targets {
        data.prepare_target(target);
    }
    println!(
        "prepared {} target corpora at {}",
        targets.len(),
        data.root.display()
    );
}

fn verify(args: Vec<String>) {
    if !args.is_empty() {
        die("usage: cargo xtask fuzz verify");
    }
    prepare(Vec::new());
    build(Vec::new());
    replay_regression::run(Vec::new());
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

/// Number of regular files directly under `dir` (non-recursive). `0` if `dir`
/// is missing.
pub(super) fn count_files(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(it) => it
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .count(),
        Err(_) => 0,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum TargetKind {
    Json,
    Bytes,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) struct FuzzTarget {
    pub name: &'static str,
    pub kind: TargetKind,
}

/// Operational source of truth for fuzz targets. Cargo still needs matching
/// `[[bin]]` declarations; `all_targets` verifies the two sets before any fuzz
/// command runs so target-local build, replay, status, and coverage cannot drift.
pub(super) const TARGETS: &[FuzzTarget] = &[
    FuzzTarget {
        name: "smelt_loop",
        kind: TargetKind::Json,
    },
    FuzzTarget {
        name: "lua_loop",
        kind: TargetKind::Json,
    },
    FuzzTarget {
        name: "text_ops",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "attached_ops",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "cache_invariance",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "openai_cache_invariance",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "snapshot_roundtrip",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "grid_invariants",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "ansi_parser",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "edit_ops",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "provider_body",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "transcript_render",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "transcript_scroll_ops",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "provider_stream",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "permissions_rules",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "store_state",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "engine_events",
        kind: TargetKind::Bytes,
    },
];

pub(super) fn all_targets() -> &'static [FuzzTarget] {
    static VALIDATED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    VALIDATED.get_or_init(validate_target_registry);
    TARGETS
}

pub(super) fn all_target_names() -> Vec<String> {
    all_targets()
        .iter()
        .map(|target| target.name.to_string())
        .collect()
}

pub(super) fn target_named(name: &str) -> Option<&'static FuzzTarget> {
    all_targets().iter().find(|target| target.name == name)
}

pub(super) fn targets_of(kind: TargetKind) -> Vec<String> {
    all_targets()
        .iter()
        .filter(|target| target.kind == kind)
        .map(|target| target.name.to_string())
        .collect()
}

fn validate_target_registry() {
    let mut registered: Vec<_> = TARGETS.iter().map(|target| target.name).collect();
    registered.sort_unstable();
    if let Some(duplicate) = registered.windows(2).find(|names| names[0] == names[1]) {
        die(&format!(
            "duplicate fuzz target `{}` in TARGETS",
            duplicate[0]
        ));
    }

    let mut manifest = fuzz_manifest_targets();
    manifest.sort_unstable();
    let manifest: Vec<_> = manifest.iter().map(String::as_str).collect();
    if registered != manifest {
        die(&format!(
            "fuzz target registry does not match fuzz/Cargo.toml\n  registry: {}\n  manifest: {}",
            registered.join(", "),
            manifest.join(", ")
        ));
    }
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

#[derive(Clone, Debug)]
pub(super) struct FuzzData {
    pub root: PathBuf,
}

impl FuzzData {
    pub fn for_repo(repo: &Path) -> Self {
        if let Some(path) = std::env::var_os("SMELT_FUZZ_HOME") {
            return Self {
                root: PathBuf::from(path),
            };
        }

        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .unwrap_or_else(std::env::temp_dir);
        let identity = git_common_dir(repo).unwrap_or_else(|| repo.to_path_buf());
        let repository = if identity.file_name().and_then(|name| name.to_str()) == Some(".git") {
            identity.parent().unwrap_or(&identity)
        } else {
            repo
        };
        let name = repository
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository");
        Self {
            root: cache
                .join("smelt")
                .join("fuzz")
                .join(format!("{name}-{:016x}", stable_path_hash(&identity))),
        }
    }

    pub fn corpus(&self, target: &str) -> PathBuf {
        self.root.join("corpus").join(target)
    }

    pub fn artifacts(&self, target: &str) -> PathBuf {
        self.root.join("artifacts").join(target)
    }

    pub fn coverage_history(&self) -> PathBuf {
        self.root.join("coverage-history")
    }

    pub fn prepare_target(&self, target: &str) {
        for path in [self.corpus(target), self.artifacts(target)] {
            std::fs::create_dir_all(&path)
                .unwrap_or_else(|e| die(&format!("create {}: {e}", path.display())));
        }
        let corpus = self.corpus(target);
        if count_files(&corpus) == 0 {
            let seed = corpus.join("bootstrap-zeroes");
            std::fs::write(&seed, [0; 256])
                .unwrap_or_else(|e| die(&format!("write {}: {e}", seed.display())));
        }
    }

    pub fn artifact_prefix(&self, target: &str) -> String {
        let path = self.artifacts(target);
        format!("{}{sep}", path.display(), sep = std::path::MAIN_SEPARATOR)
    }
}

fn git_common_dir(repo: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let path = if path.is_absolute() {
        path
    } else {
        repo.join(path)
    };
    Some(path.canonicalize().unwrap_or(path))
}

fn stable_path_hash(path: &Path) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn import_data(args: Vec<String>) {
    if args.len() > 1 {
        die("usage: cargo xtask fuzz import-data [fuzz-dir]");
    }
    let repo = repo_root();
    let source = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| legacy_fuzz_dir(&repo));
    let source = if source.join("fuzz/corpus").is_dir() {
        source.join("fuzz")
    } else {
        source
    };
    if !source.is_dir() {
        die(&format!(
            "legacy fuzz directory not found: {}",
            source.display()
        ));
    }

    let data = FuzzData::for_repo(&repo);
    let mut copied = 0usize;
    let mut existing = 0usize;
    for name in ["corpus", "artifacts", "coverage-history"] {
        let from = source.join(name);
        if from.is_dir() {
            merge_tree(&from, &data.root.join(name), &mut copied, &mut existing);
        }
    }
    println!("fuzz data: {}", data.root.display());
    println!("imported: {copied}, already present: {existing}");
}

fn legacy_fuzz_dir(repo: &Path) -> PathBuf {
    git_common_dir(repo)
        .and_then(|git| git.parent().map(|root| root.join("fuzz")))
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| repo.join("fuzz"))
}

fn merge_tree(from: &Path, to: &Path, copied: &mut usize, existing: &mut usize) {
    std::fs::create_dir_all(to).unwrap_or_else(|e| die(&format!("create {}: {e}", to.display())));
    let entries =
        std::fs::read_dir(from).unwrap_or_else(|e| die(&format!("read {}: {e}", from.display())));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| die(&format!("read {} entry: {e}", from.display())));
        let source = entry.path();
        let destination = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .unwrap_or_else(|e| die(&format!("inspect {}: {e}", source.display())));
        if kind.is_dir() {
            merge_tree(&source, &destination, copied, existing);
        } else if kind.is_file() {
            if destination.exists() {
                let source_bytes = std::fs::read(&source)
                    .unwrap_or_else(|e| die(&format!("read {}: {e}", source.display())));
                let destination_bytes = std::fs::read(&destination)
                    .unwrap_or_else(|e| die(&format!("read {}: {e}", destination.display())));
                if source_bytes != destination_bytes {
                    die(&format!(
                        "refusing to overwrite different fuzz data: {}",
                        destination.display()
                    ));
                }
                *existing += 1;
            } else {
                std::fs::copy(&source, &destination).unwrap_or_else(|e| {
                    die(&format!(
                        "copy {} to {}: {e}",
                        source.display(),
                        destination.display()
                    ))
                });
                *copied += 1;
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::FuzzData;

    #[test]
    fn prepare_target_bootstraps_only_an_empty_corpus() {
        let unique = format!(
            "smelt-xtask-fuzz-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let data = FuzzData { root: root.clone() };

        data.prepare_target("example");
        assert_eq!(
            std::fs::read(data.corpus("example").join("bootstrap-zeroes")).unwrap(),
            vec![0; 256]
        );
        assert!(data.artifacts("example").is_dir());

        std::fs::remove_file(data.corpus("example").join("bootstrap-zeroes")).unwrap();
        std::fs::write(data.corpus("example").join("existing"), b"seed").unwrap();
        data.prepare_target("example");
        assert!(!data.corpus("example").join("bootstrap-zeroes").exists());
        assert_eq!(
            std::fs::read(data.corpus("example").join("existing")).unwrap(),
            b"seed"
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
