//! `cargo xtask fuzz replay-regression` - replay every seed under
//! `fuzz/seeds/<target>/regression/`. Exits non-zero if any replay fails.
//! CI calls this on every PR; devs call it before sending changes.
//!
//! JSON-scenario targets (`smelt_loop`, `lua_loop`) replay via the in-tree
//! `replay_scenario` binary. Byte-form targets execute the prebuilt libFuzzer
//! binaries directly, once per seed.

use super::{build, die, nightly_host, repo_root, step, targets_of, FuzzData, TargetKind};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(args: Vec<String>) {
    if !args.is_empty() {
        die("usage: cargo xtask fuzz replay-regression");
    }
    build(Vec::new());
    run_prebuilt();
}

pub(super) fn run_prebuilt() {
    let root = repo_root();
    let seeds = root.join("fuzz/seeds");

    println!(">>> building replay_scenario");
    step(
        "build replay_scenario",
        Command::new("cargo")
            .args([
                "build",
                "--features",
                "scenario-tools",
                "--bin",
                "replay_scenario",
                "--manifest-path",
                "fuzz/Cargo.toml",
                "-q",
            ])
            .current_dir(&root),
    );

    let replay = cargo_target_dir(&root)
        .join("debug")
        .join(format!("replay_scenario{}", std::env::consts::EXE_SUFFIX));
    let mut fail = false;

    for target in targets_of(TargetKind::Json) {
        let dir = seeds.join(&target).join("regression");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        files.sort();
        if files.is_empty() {
            continue;
        }
        println!(">>> {target}: {} regression seed(s)", files.len());
        for seed in &files {
            let name = seed.file_name().and_then(|s| s.to_str()).unwrap_or("<?>");
            let output = Command::new(&replay)
                .args(["--target", &target])
                .arg(seed)
                .output()
                .unwrap_or_else(|e| die(&format!("spawn replay_scenario: {e}")));
            if output.status.success() {
                println!("  ok   {name}");
            } else {
                println!("  FAIL {name}");
                print_failure_output(&output);
                fail = true;
            }
        }
    }

    let host = nightly_host().unwrap_or_else(|| die("could not determine the nightly host triple"));
    let data = FuzzData::for_repo(&root);
    for target in targets_of(TargetKind::Bytes) {
        let dir = seeds.join(&target).join("regression");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        if files.is_empty() {
            continue;
        }
        println!(">>> {target}: {} byte-form seed(s)", files.len());
        data.prepare_target(&target);
        let binary = fuzz_binary(&root, &host, &target);
        if !binary.is_file() {
            die(&format!(
                "prebuilt fuzz target not found: {}",
                binary.display()
            ));
        }
        let output = Command::new(&binary)
            .arg("-runs=0")
            .arg(format!(
                "-artifact_prefix={}",
                data.artifact_prefix(&target)
            ))
            .args(&files)
            .current_dir(&root)
            .output()
            .unwrap_or_else(|e| die(&format!("spawn {}: {e}", binary.display())));
        if output.status.success() {
            println!("  ok");
        } else {
            println!("  FAIL");
            print_failure_output(&output);
            fail = true;
        }
    }

    if fail {
        println!();
        println!("regression replay FAILED");
        std::process::exit(1);
    }
    println!();
    println!("all regression seeds passed");
}

fn cargo_target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
        .unwrap_or_else(|| root.join("fuzz/target"))
}

fn fuzz_binary(root: &Path, host: &str, target: &str) -> PathBuf {
    cargo_target_dir(root)
        .join(host)
        .join("release")
        .join(format!("{target}{}", std::env::consts::EXE_SUFFIX))
}

fn print_failure_output(output: &std::process::Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.is_empty() {
        println!("--- stdout ---");
        print!("{stdout}");
    }
    if !stderr.is_empty() {
        println!("--- stderr ---");
        print!("{stderr}");
    }
}
