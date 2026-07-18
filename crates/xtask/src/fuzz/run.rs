//! `cargo xtask fuzz run <target> [--fork N] [--cmin]` - fuzz a single target.
//!
//! Thin wrapper over `cargo +nightly fuzz run` that wires the flags we
//! actually want:
//!   - preflight corpus replay without `-fork`, so stale corpus crashes fail
//!     before libFuzzer's fork-mode merge can keep going after writing artifacts.
//!   - `-ignore_crashes=0`, `-ignore_ooms=0`, and `-ignore_timeouts=0` so
//!     the first new fork-worker hard failure drops an artifact and exits.
//!   - `-fork=N` for parallel workers (default 1).
//!   - optional `--cmin` runs `cargo fuzz cmin` first to sweep the
//!     accumulated corpus for regressions and shrink it before fuzzing.
//!
//! No time budget - runs until crash or Ctrl-C. To bound a session, pass
//! `-max_total_time=<secs>` after the target (anything trailing the
//! target name lands in the libFuzzer argv).

use super::{all_target_names, die, repo_root, FuzzData};
use std::process::Command;

pub fn run(args: Vec<String>) {
    let mut target: Option<String> = None;
    let mut fork: u32 = 1;
    let mut cmin = false;
    let mut sanitizer = "address".to_string();
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
            "--sanitizer" => {
                sanitizer = it
                    .next()
                    .unwrap_or_else(|| die("--sanitizer needs a value"));
                if !matches!(
                    sanitizer.as_str(),
                    "address" | "leak" | "memory" | "thread" | "none"
                ) {
                    die(&format!("bad --sanitizer value `{sanitizer}`"));
                }
            }
            "-h" | "--help" => {
                print_help();
                return;
            }
            "--" => {
                extra.extend(it);
                break;
            }
            _ if target.is_none() => target = Some(arg),
            _ => extra.push(arg),
        }
    }

    let Some(target) = target else {
        print_help();
        std::process::exit(2);
    };

    let known = all_target_names();
    if !known.contains(&target) {
        die(&format!(
            "unknown target `{target}`. Known: {}",
            known.join(", ")
        ));
    }

    let root = repo_root();
    let data = FuzzData::for_repo(&root);
    data.prepare_target(&target);
    let corpus = data.corpus(&target);
    let artifact_prefix = data.artifact_prefix(&target);

    preflight_corpus(&root, &target, &corpus, &artifact_prefix, &sanitizer);

    if cmin {
        println!(">>> cmin {target}");
        let status = Command::new("cargo")
            .args([
                "+nightly",
                "fuzz",
                "cmin",
                "--sanitizer",
                &sanitizer,
                &target,
            ])
            .arg(&corpus)
            .arg("--")
            .arg(format!("-artifact_prefix={artifact_prefix}"))
            .current_dir(&root)
            .status()
            .unwrap_or_else(|e| die(&format!("spawn cmin: {e}")));
        if !status.success() {
            die(&format!("cmin exited {status}"));
        }
    }

    let fork_flag = format!("-fork={fork}");
    println!(">>> fuzz {target} (fork={fork}, sanitizer={sanitizer})");
    let status = Command::new("cargo")
        .args([
            "+nightly",
            "fuzz",
            "run",
            "--sanitizer",
            &sanitizer,
            &target,
        ])
        .arg(&corpus)
        .arg("--")
        .arg(&fork_flag)
        .args(["-ignore_crashes=0", "-ignore_ooms=0", "-ignore_timeouts=0"])
        .arg(format!("-artifact_prefix={artifact_prefix}"))
        .args(extra)
        .current_dir(&root)
        .status()
        .unwrap_or_else(|e| die(&format!("spawn cargo fuzz run: {e}")));

    if !status.success() {
        eprintln!();
        eprintln!(
            ">>> fuzz exited {status} - inspect {} for the failure artifact",
            data.artifacts(&target).display()
        );
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn preflight_corpus(
    root: &std::path::Path,
    target: &str,
    corpus: &std::path::Path,
    artifact_prefix: &str,
    sanitizer: &str,
) {
    println!("xtask fuzz: preflight corpus {target}");
    let status = Command::new("cargo")
        .args(["+nightly", "fuzz", "run", "--sanitizer", sanitizer, target])
        .arg(corpus)
        .arg("--")
        .args(["-runs=0", "-ignore_crashes=0"])
        .arg(format!("-artifact_prefix={artifact_prefix}"))
        .current_dir(root)
        .status()
        .unwrap_or_else(|e| die(&format!("spawn corpus preflight: {e}")));
    if !status.success() {
        eprintln!();
        eprintln!(">>> corpus preflight exited {status}; inspect {artifact_prefix}");
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn print_help() {
    let names = all_target_names();
    eprintln!("usage: cargo xtask fuzz run <target> [--fork N] [--cmin] [--sanitizer KIND] [libfuzzer-flags...]");
    eprintln!();
    eprintln!("targets: {}", names.join(", "));
    eprintln!();
    eprintln!("flags:");
    eprintln!("  --fork N              parallel workers (default 1)");
    eprintln!("  --cmin                minimize the shared corpus before fuzzing");
    eprintln!("  --sanitizer KIND      address, leak, memory, thread, or none (default address)");
    eprintln!();
    eprintln!("Stops on corpus preflight crash before forking, then on first fork-worker crash, OOM, or timeout. To bound time, append `-max_total_time=<secs>`.");
}
