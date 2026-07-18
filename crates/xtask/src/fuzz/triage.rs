//! `cargo xtask fuzz triage <target> <crash-artifact>` - minimize a crash while
//! preserving its failure identity. Structured targets produce JSON scenarios;
//! byte targets retain exact libFuzzer inputs.

use super::{die, iso_utc, repo_root, step, target_named, TargetKind};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(args: Vec<String>) {
    if args.len() != 2 {
        die("usage: cargo xtask fuzz triage <target> <crash-artifact>");
    }
    let target = &args[0];
    let artifact = PathBuf::from(&args[1]);
    if !artifact.is_file() {
        die(&format!("artifact not found: {}", artifact.display()));
    }
    let Some(target_meta) = target_named(target) else {
        die(&format!("unknown fuzz target `{target}`"));
    };

    let root = repo_root();
    let minimized = match target_meta.kind {
        TargetKind::Json => triage_json(&root, target, &artifact),
        TargetKind::Bytes => triage_bytes(&root, target, &artifact),
    };
    write_metadata(&root, target, &artifact, &minimized, target_meta.kind);

    eprintln!();
    eprintln!("minimized artifact: {}", minimized.display());
    eprintln!("to commit as a regression seed:");
    let extension = if target_meta.kind == TargetKind::Json {
        ".json"
    } else {
        ""
    };
    eprintln!(
        "  cp {} fuzz/seeds/{}/regression/<slug>{extension}",
        minimized.display(),
        target
    );
}

fn triage_json(root: &Path, target: &str, artifact: &Path) -> PathBuf {
    let fuzz_cargo = root.join("fuzz/Cargo.toml");
    eprintln!(">>> 1/3 building structured triage tools");
    step(
        "build crash_to_scenario, shrink_scenario, replay_scenario",
        Command::new("cargo")
            .args([
                "build",
                "--manifest-path",
                fuzz_cargo.to_str().expect("fuzz manifest path utf-8"),
                "--bin",
                "crash_to_scenario",
                "--bin",
                "shrink_scenario",
                "--bin",
                "replay_scenario",
                "-q",
            ])
            .current_dir(root),
    );

    let tmp = tempdir();
    let raw = tmp.join("raw.json");
    let minimized = sibling_path(artifact, ".min.json");
    let crash_to_scenario = root.join("fuzz/target/debug/crash_to_scenario");
    let shrink_scenario = root.join("fuzz/target/debug/shrink_scenario");

    let raw_file =
        std::fs::File::create(&raw).unwrap_or_else(|e| die(&format!("create raw.json: {e}")));
    step(
        "decode bytes to JSON scenario",
        Command::new(&crash_to_scenario)
            .args(["--target", target])
            .arg(artifact)
            .stdout(raw_file),
    );

    eprintln!(">>> 2/3 shrinking with panic identity");
    step(
        "shrink JSON scenario",
        Command::new(&shrink_scenario)
            .args(["--target", target])
            .arg(&raw)
            .arg(&minimized),
    );

    eprintln!(">>> 3/3 minimized scenario:");
    let body = std::fs::read_to_string(&minimized)
        .unwrap_or_else(|e| die(&format!("read {}: {e}", minimized.display())));
    if let Err(error) = std::fs::remove_dir_all(&tmp) {
        eprintln!("xtask fuzz: remove temporary {}: {error}", tmp.display());
    }
    println!("{body}");
    minimized
}

fn triage_bytes(root: &Path, target: &str, artifact: &Path) -> PathBuf {
    let minimized = sibling_path(artifact, ".min");
    let original_fingerprint = replay_fingerprint(root, target, artifact);
    eprintln!(">>> minimizing byte artifact");
    step(
        "cargo fuzz tmin",
        Command::new("cargo")
            .args(["+nightly", "fuzz", "tmin", "--sanitizer=address", target])
            .arg(artifact)
            .arg("--")
            .arg(format!("-exact_artifact_path={}", minimized.display()))
            .current_dir(root),
    );
    if !minimized.is_file() {
        die(&format!(
            "cargo fuzz tmin did not write {}",
            minimized.display()
        ));
    }
    let minimized_fingerprint = replay_fingerprint(root, target, &minimized);
    if original_fingerprint != minimized_fingerprint {
        die(&format!(
            "minimized artifact changed failure identity\noriginal: {original_fingerprint}\nminimized: {minimized_fingerprint}"
        ));
    }
    eprintln!("failure fingerprint: {original_fingerprint}");
    minimized
}

fn replay_fingerprint(root: &Path, target: &str, artifact: &Path) -> String {
    let output = Command::new("cargo")
        .args(["+nightly", "fuzz", "run", "--sanitizer=address", target])
        .arg(artifact)
        .args(["--", "-runs=1"])
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| die(&format!("replay {}: {e}", artifact.display())));
    if output.status.success() {
        die(&format!(
            "artifact does not fail when replayed: {}",
            artifact.display()
        ));
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    failure_fingerprint(&text)
}

fn failure_fingerprint(output: &str) -> String {
    let lines: Vec<_> = output.lines().collect();
    let mut panic_identity = Vec::new();
    let mut sanitizer_summaries = Vec::new();
    let mut runtime_errors = Vec::new();
    let mut fallback_errors = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.contains("panicked at") {
            // Source line and column are part of the identity. Normalizing them
            // would collapse unrelated assertions in the same target file.
            panic_identity.push(line.trim().to_string());
            if let Some(message) = lines[index + 1..]
                .iter()
                .map(|line| line.trim())
                .find(|line| !line.is_empty())
                .filter(|line| !line.starts_with("stack backtrace:") && !line.starts_with("note:"))
            {
                panic_identity.push(normalize_numbers(message));
            }
        } else if line.contains("SUMMARY:") {
            sanitizer_summaries.push(normalize_numbers(line.trim()));
        } else if line.contains("runtime error:") {
            runtime_errors.push(normalize_numbers(line.trim()));
        } else if line.contains("ERROR:") {
            fallback_errors.push(normalize_numbers(line.trim()));
        }
    }
    let relevant = if !panic_identity.is_empty() {
        panic_identity
    } else if !sanitizer_summaries.is_empty() {
        sanitizer_summaries
    } else if !runtime_errors.is_empty() {
        runtime_errors
    } else {
        fallback_errors
    };
    if relevant.is_empty() {
        die("failing fuzz replay did not contain a recognizable crash fingerprint");
    }
    relevant.join(" | ")
}

fn normalize_numbers(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len());
    let mut in_digits = false;
    for ch in line.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                normalized.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            normalized.push(ch);
        }
    }
    normalized
}

fn sibling_path(artifact: &Path, suffix: &str) -> PathBuf {
    let name = artifact
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    artifact.with_file_name(format!("{name}{suffix}"))
}

fn write_metadata(root: &Path, target: &str, artifact: &Path, minimized: &Path, kind: TargetKind) {
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let metadata = serde_json::json!({
        "schema": 1,
        "target": target,
        "input_kind": match kind { TargetKind::Json => "json", TargetKind::Bytes => "bytes" },
        "sanitizer": match kind { TargetKind::Json => "structured-replay", TargetKind::Bytes => "address" },
        "commit": commit,
        "triaged_at": iso_utc(),
        "original": artifact,
        "minimized": minimized,
        "original_bytes": file_size(artifact),
        "minimized_bytes": file_size(minimized),
        "failure_identity_preserved": true,
    });
    let path = sibling_path(artifact, ".triage.json");
    let body = serde_json::to_vec_pretty(&metadata)
        .unwrap_or_else(|e| die(&format!("serialize triage metadata: {e}")));
    std::fs::write(&path, body).unwrap_or_else(|e| die(&format!("write {}: {e}", path.display())));
    eprintln!("triage metadata: {}", path.display());
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn tempdir() -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("xtask-fuzz-triage-{pid}-{stamp}"));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| die(&format!("mkdir {}: {e}", dir.display())));
    dir
}

#[cfg(test)]
mod tests {
    use super::failure_fingerprint;

    #[test]
    fn panic_fingerprint_preserves_location_and_normalizes_message_numbers() {
        let first = failure_fingerprint(
            "thread '<unnamed>' panicked at fuzz_targets/example.rs:42:7:\nvalue 123 failed",
        );
        let minimized = failure_fingerprint(
            "thread '<unnamed>' panicked at fuzz_targets/example.rs:42:7:\nvalue 9 failed",
        );
        let other_location = failure_fingerprint(
            "thread '<unnamed>' panicked at fuzz_targets/example.rs:43:7:\nvalue 9 failed",
        );

        assert_eq!(first, minimized);
        assert_ne!(first, other_location);
        assert!(first.contains("example.rs:42:7"));
        assert!(first.contains("value # failed"));
    }

    #[test]
    fn panic_fingerprint_distinguishes_messages_at_the_same_location() {
        let first = failure_fingerprint(
            "thread '<unnamed>' panicked at fuzz_targets/example.rs:42:7:\nfirst invariant",
        );
        let second = failure_fingerprint(
            "thread '<unnamed>' panicked at fuzz_targets/example.rs:42:7:\nsecond invariant",
        );

        assert_ne!(first, second);
    }
}
