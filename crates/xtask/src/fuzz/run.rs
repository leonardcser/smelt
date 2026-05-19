//! `cargo xtask fuzz run <target> [--fork N] [--cmin]` — fuzz a single target.
//!
//! Thin wrapper over `cargo +nightly fuzz run` that wires the flags we
//! actually want:
//!   - `-ignore_crashes=0` so the first crash drops an artifact and exits.
//!   - `-fork=N` for parallel workers (default 1).
//!   - optional `--cmin` runs `cargo fuzz cmin` first to sweep the
//!     accumulated corpus for regressions and shrink it before fuzzing.
//!
//! No time budget — runs until crash or Ctrl-C. To bound a session, pass
//! `-max_total_time=<secs>` after the target (anything trailing the
//! target name lands in the libFuzzer argv).

use super::{all_target_names, die, repo_root};
use std::process::Command;

pub fn run(args: Vec<String>) {
    let mut target: Option<String> = None;
    let mut fork: u32 = 1;
    let mut cmin = false;
    let mut extra: Vec<String> = Vec::new();

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--fork" => {
                let v = it.next().unwrap_or_else(|| die("--fork needs a value"));
                fork = v
                    .parse()
                    .unwrap_or_else(|_| die(&format!("bad --fork value `{v}`")));
            }
            "--cmin" => cmin = true,
            "-h" | "--help" => {
                print_help();
                return;
            }
            _ if target.is_none() => target = Some(arg),
            _ => extra.push(arg),
        }
    }

    let Some(target) = target else {
        print_help();
        std::process::exit(2);
    };

    let known: Vec<&str> = all_target_names().collect();
    if !known.contains(&target.as_str()) {
        die(&format!(
            "unknown target `{target}`. Known: {}",
            known.join(", ")
        ));
    }

    let root = repo_root();

    if cmin {
        println!(">>> cmin {target}");
        let status = Command::new("cargo")
            .args(["+nightly", "fuzz", "cmin", "--sanitizer=none", &target])
            .current_dir(&root)
            .status()
            .unwrap_or_else(|e| die(&format!("spawn cmin: {e}")));
        if !status.success() {
            die(&format!("cmin exited {status}"));
        }
    }

    let fork_flag = format!("-fork={fork}");
    let mut cargo_args: Vec<String> = vec![
        "+nightly".into(),
        "fuzz".into(),
        "run".into(),
        "--sanitizer=none".into(),
        target.clone(),
        "--".into(),
        fork_flag,
        "-ignore_crashes=0".into(),
    ];
    cargo_args.extend(extra);

    println!(">>> fuzz {target} (fork={fork})");
    // Exec semantics: cargo fuzz prints its own banner and the libFuzzer
    // process inherits stdio. On crash, cargo-fuzz exits non-zero and
    // leaves the artifact under `fuzz/artifacts/<target>/`.
    let status = Command::new("cargo")
        .args(&cargo_args)
        .current_dir(&root)
        .status()
        .unwrap_or_else(|e| die(&format!("spawn cargo fuzz run: {e}")));

    if !status.success() {
        eprintln!();
        eprintln!(
            ">>> fuzz exited {status} — check fuzz/artifacts/{target}/ for the crash artifact"
        );
        eprintln!(">>> next: cargo xtask fuzz triage {target} fuzz/artifacts/{target}/crash-<hex>");
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn print_help() {
    let names: Vec<&str> = all_target_names().collect();
    eprintln!("usage: cargo xtask fuzz run <target> [--fork N] [--cmin] [-- libfuzzer-flags...]");
    eprintln!();
    eprintln!("targets: {}", names.join(", "));
    eprintln!();
    eprintln!("flags:");
    eprintln!("  --fork N    parallel workers (default 1)");
    eprintln!("  --cmin      run `cargo fuzz cmin <target>` first");
    eprintln!();
    eprintln!("Stops on first crash. To bound time, append `-max_total_time=<secs>`.");
}
