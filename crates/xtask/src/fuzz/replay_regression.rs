//! `cargo xtask fuzz replay-regression` - replay every seed under
//! `fuzz/seeds/<target>/regression/`. Non-zero exit on the first failure.
//! CI calls this on every PR; devs call it before sending changes.
//!
//! JSON-scenario targets (`smelt_loop`, `lua_loop`) replay via the in-tree
//! `replay_scenario` binary. Byte-form targets replay via
//! `cargo fuzz run --runs=0` which executes every file in the seed dir
//! exactly once and exits.

use super::{die, repo_root, step, targets_of, TargetKind};
use std::process::Command;

pub fn run(_args: Vec<String>) {
    let root = repo_root();
    let seeds = root.join("fuzz/seeds");

    println!(">>> building replay_scenario");
    step(
        "build replay_scenario",
        Command::new("cargo")
            .args([
                "build",
                "--bin",
                "replay_scenario",
                "--manifest-path",
                "fuzz/Cargo.toml",
                "-q",
            ])
            .current_dir(&root),
    );

    let replay = root.join("fuzz/target/debug/replay_scenario");
    let mut fail = false;

    for target in targets_of(TargetKind::Json) {
        let dir = seeds.join(target).join("regression");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        if files.is_empty() {
            continue;
        }
        println!(">>> {target}: {} regression seed(s)", files.len());
        for seed in &files {
            let name = seed.file_name().and_then(|s| s.to_str()).unwrap_or("<?>");
            let status = Command::new(&replay)
                .args(["--target", target])
                .arg(seed)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap_or_else(|e| die(&format!("spawn replay_scenario: {e}")));
            if status.success() {
                println!("  ok   {name}");
            } else {
                println!("  FAIL {name}");
                fail = true;
            }
        }
    }

    for target in targets_of(TargetKind::Bytes) {
        let dir = seeds.join(target).join("regression");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        if files.is_empty() {
            continue;
        }
        println!(">>> {target}: {} byte-form seed(s)", files.len());
        let output = Command::new("cargo")
            .args([
                "+nightly",
                "fuzz",
                "run",
                "--sanitizer=none",
                target,
                dir.to_str().expect("seed dir utf-8"),
                "--",
                "-runs=0",
            ])
            .current_dir(&root)
            .output()
            .unwrap_or_else(|e| die(&format!("spawn cargo fuzz run: {e}")));
        if output.status.success() {
            println!("  ok");
        } else {
            println!("  FAIL");
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                println!("--- stdout ---");
                print!("{stdout}");
            }
            if !stderr.is_empty() {
                println!("--- stderr ---");
                print!("{stderr}");
            }
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
