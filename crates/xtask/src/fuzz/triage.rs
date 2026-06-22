//! `cargo xtask fuzz triage <target> <crash-artifact>` - shrink a crash and
//! print the minimal scenario. Replaces the manual "cargo fuzz fmt →
//! cargo fuzz tmin → eyeball → maybe write it up" dance.

use super::{die, repo_root, step, targets_of, TargetKind};
use std::path::Path;
use std::process::Command;

pub fn run(args: Vec<String>) {
    if args.len() != 2 {
        die("usage: cargo xtask fuzz triage <target> <crash-artifact>");
    }
    let target = &args[0];
    let artifact = Path::new(&args[1]);

    if !artifact.is_file() {
        die(&format!("artifact not found: {}", artifact.display()));
    }
    let json_targets = targets_of(TargetKind::Json);
    if !json_targets.contains(target) {
        die(&format!(
            "triage only handles {} (other targets have no scenario form)",
            json_targets.join(" / ")
        ));
    }

    let root = repo_root();
    let fuzz_cargo = root.join("fuzz/Cargo.toml");

    eprintln!(">>> 1/3 building tools");
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
            .current_dir(&root),
    );

    let tmp = tempdir();
    let raw = tmp.join("raw.json");
    let shrunk = tmp.join("shrunk.json");

    let crash_to_scenario = root.join("fuzz/target/debug/crash_to_scenario");
    let shrink_scenario = root.join("fuzz/target/debug/shrink_scenario");

    let raw_file =
        std::fs::File::create(&raw).unwrap_or_else(|e| die(&format!("create raw.json: {e}")));
    step(
        "bytes → JSON scenario",
        Command::new(&crash_to_scenario)
            .args(["--target", target])
            .arg(artifact)
            .stdout(raw_file),
    );

    eprintln!(">>> 2/3 shrinking (predicate: same panic)");
    step(
        "JSON → shrunk JSON",
        Command::new(&shrink_scenario)
            .args(["--target", target])
            .arg(&raw)
            .arg(&shrunk),
    );

    eprintln!(">>> 3/3 shrunk scenario:");
    let body =
        std::fs::read_to_string(&shrunk).unwrap_or_else(|e| die(&format!("read shrunk: {e}")));
    println!("{body}");

    eprintln!();
    eprintln!("────────────────────────────────────────────────────────────────────────");
    eprintln!("to commit as a regression seed:");
    eprintln!(
        "  cp {} fuzz/seeds/{}/regression/<slug>.json",
        shrunk.display(),
        target
    );
    eprintln!("  # edit to add _about and _fix fields before committing");
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
