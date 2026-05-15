//! `cargo xtask fuzz [secs]` — full local fuzz cycle.
//!
//! Unpacks `fuzz/seed_corpus.tar.gz`, runs `cargo +nightly fuzz run smelt_loop`
//! for `secs` seconds (default 300), minimizes the resulting corpus, and
//! repacks the tarball. Stops on the first crash and leaves the artifact in
//! `fuzz/artifacts/smelt_loop/`.

use std::path::PathBuf;
use std::process::Command;

pub fn run(args: Vec<String>) {
    let duration: u32 = match args.first().map(|s| s.as_str()) {
        None => 300,
        Some(s) => s.parse().unwrap_or_else(|_| {
            eprintln!("xtask fuzz: bad duration `{s}` (expected seconds)");
            std::process::exit(2);
        }),
    };

    let fuzz_dir = repo_root().join("fuzz");
    if !fuzz_dir.exists() {
        eprintln!("xtask fuzz: missing {}", fuzz_dir.display());
        std::process::exit(2);
    }

    step(
        "unpack seed corpus",
        Command::new("tar")
            .args(["-xzf", "seed_corpus.tar.gz"])
            .current_dir(&fuzz_dir),
    );

    let max_time = format!("-max_total_time={duration}");
    step(
        &format!("fuzz {duration}s"),
        Command::new("cargo")
            .args([
                "+nightly",
                "fuzz",
                "run",
                "smelt_loop",
                "seed_corpus/smelt_loop",
                "--",
                "-max_len=4096",
                &max_time,
            ])
            .current_dir(&fuzz_dir),
    );

    step(
        "minimize corpus",
        Command::new("cargo")
            .args([
                "+nightly",
                "fuzz",
                "cmin",
                "smelt_loop",
                "seed_corpus/smelt_loop",
            ])
            .current_dir(&fuzz_dir),
    );

    step(
        "repack tarball",
        Command::new("tar")
            .args(["-czf", "seed_corpus.tar.gz", "seed_corpus"])
            .current_dir(&fuzz_dir),
    );

    println!("xtask fuzz: done");
}

fn step(label: &str, cmd: &mut Command) {
    println!("xtask fuzz: {label}");
    let status = cmd.status().unwrap_or_else(|e| {
        eprintln!("xtask fuzz: {label}: spawn failed: {e}");
        std::process::exit(2);
    });
    if !status.success() {
        eprintln!("xtask fuzz: {label}: exit {status}");
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("xtask is at crates/xtask/")
        .to_path_buf()
}
