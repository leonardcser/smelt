//! `cargo xtask fuzz status` - compact status for local fuzzing state.

use super::{all_target_names, count_files, die, repo_root, FuzzData};
use std::path::Path;
use std::process::Command;

pub fn run(args: Vec<String>) {
    if !args.is_empty() {
        die("usage: cargo xtask fuzz status");
    }
    let root = repo_root();
    let data = FuzzData::for_repo(&root);
    println!("fuzz status");
    println!("repository: {}", root.display());
    println!("data: {}", data.root.display());
    println!();

    print_processes();
    println!();

    println!(
        "{:<28} {:>8} {:>8} {:>12} {:>10}",
        "target", "corpus", "size", "regressions", "artifacts"
    );
    for target in all_target_names() {
        let corpus = data.corpus(&target);
        let seeds = root.join(format!("fuzz/seeds/{target}/regression"));
        let artifacts = data.artifacts(&target);
        let corpus_files = count_files(&corpus);
        let seed_files = count_recursive_files(&seeds);
        let artifact_files = count_recursive_files(&artifacts);
        println!(
            "{:<28} {:>8} {:>8} {:>12} {:>10}",
            target,
            corpus_files,
            dir_size(&corpus),
            seed_files,
            artifact_files
        );
    }

    println!();
    print_latest_coverage(&data);
}

fn print_processes() {
    let out = Command::new("ps")
        .args(["-eo", "pid=,comm=,args="])
        .output();
    let Ok(out) = out else {
        println!("processes: unavailable");
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| {
            line.contains("cargo xtask fuzz")
                || line.contains("cargo-fuzz")
                || line.contains("libfuzzer")
                || line.contains("/fuzz_targets/")
        })
        .filter(|line| !line.contains("xtask fuzz status"))
        .collect();
    if rows.is_empty() {
        println!("processes: none");
    } else {
        println!("processes:");
        for row in rows {
            println!("  {}", row.trim());
        }
    }
}

fn count_recursive_files(path: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut n = 0;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            n += count_recursive_files(&p);
        } else if p.is_file() {
            n += 1;
        }
    }
    n
}

fn dir_size(path: &Path) -> String {
    let out = Command::new("du")
        .args(["-sh", path.to_str().unwrap_or("")])
        .output();
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .to_string(),
        _ => "0".into(),
    }
}

fn print_latest_coverage(data: &FuzzData) {
    let dir = data.coverage_history();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        println!("coverage: no snapshots");
        return;
    };
    let latest = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("txt"))
        .filter(|p| p.file_name().and_then(|s| s.to_str()) != Some("README.md"))
        .max_by_key(|p| p.file_name().map(|s| s.to_owned()));
    let Some(path) = latest else {
        println!("coverage: no snapshots");
        return;
    };
    println!("coverage: {}", path.display());
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines().skip(5).filter(|line| !line.trim().is_empty()) {
            println!("  {line}");
        }
    }
}
