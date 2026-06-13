//! `cargo xtask fuzz status` - compact status for local fuzzing state.

use super::{all_target_names, count_files, repo_root};
use std::path::Path;
use std::process::Command;

pub fn run(_args: Vec<String>) {
    let root = repo_root();
    println!("fuzz status");
    println!("root: {}", root.display());
    println!();

    print_processes();
    println!();

    println!(
        "{:<28} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "target", "corpus", "size", "tracked", "seeds", "artifacts"
    );
    for target in all_target_names() {
        let corpus = root.join(format!("fuzz/corpus/{target}"));
        let seeds = root.join(format!("fuzz/seeds/{target}/regression"));
        let artifacts = root.join(format!("fuzz/artifacts/{target}"));
        let corpus_files = count_files(&corpus);
        let seed_files = count_recursive_files(&seeds);
        let artifact_files = count_recursive_files(&artifacts);
        let tracked = tracked_count(&root, &format!("fuzz/corpus/{target}"));
        println!(
            "{:<28} {:>8} {:>8} {:>8} {:>8} {:>8}",
            target,
            corpus_files,
            dir_size(&corpus),
            tracked,
            seed_files,
            artifact_files
        );
    }

    println!();
    print_latest_coverage(&root);
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

fn tracked_count(root: &Path, path: &str) -> usize {
    let out = Command::new("git")
        .args(["ls-files", path])
        .current_dir(root)
        .output();
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        _ => 0,
    }
}

fn print_latest_coverage(root: &Path) {
    let dir = root.join("fuzz/coverage-history");
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
