use std::process::Command;

pub fn run(args: Vec<String>) {
    let mut runs = String::from("10");
    let mut entries = String::from("500000");
    let mut queries: Option<String> = None;
    let mut include_dirs = true;
    let mut release = true;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--runs" => {
                runs = value(&mut iter, "--runs");
                positive_usize("--runs", &runs);
            }
            "--entries" => {
                entries = value(&mut iter, "--entries");
                positive_usize("--entries", &entries);
            }
            "--queries" => {
                queries = Some(value(&mut iter, "--queries"));
            }
            "--files-only" => include_dirs = false,
            "--debug" => release = false,
            "-h" | "--help" => {
                print_usage();
                return;
            }
            other => {
                eprintln!("bench-file-search: unknown argument `{other}`");
                print_usage();
                std::process::exit(2);
            }
        }
    }

    let mut cmd = Command::new("cargo");
    cmd.args(["test", "-p", "smelt-core"]);
    if release {
        cmd.arg("--release");
    }
    cmd.args([
        "workspace_file_search_benchmark_suite",
        "--",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    cmd.env("SMELT_FILE_SEARCH_BENCH_RUNS", &runs);
    cmd.env("SMELT_FILE_SEARCH_BENCH_ENTRIES", &entries);
    cmd.env(
        "SMELT_FILE_SEARCH_BENCH_INCLUDE_DIRS",
        if include_dirs { "1" } else { "0" },
    );
    if let Some(queries) = queries {
        cmd.env("SMELT_FILE_SEARCH_BENCH_QUERIES", queries);
    }

    eprintln!(
        "running file search benchmark: profile={} runs={} entries={} include_dirs={}",
        if release { "release" } else { "test/debug" },
        runs,
        entries,
        include_dirs
    );
    let status = cmd.status().unwrap_or_else(|e| {
        eprintln!("bench-file-search: failed to run cargo test: {e}");
        std::process::exit(1);
    });
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn value(iter: &mut impl Iterator<Item = String>, flag: &str) -> String {
    iter.next().unwrap_or_else(|| {
        eprintln!("bench-file-search: {flag} requires a value");
        std::process::exit(2);
    })
}

fn positive_usize(flag: &str, value: &str) {
    if value.parse::<usize>().ok().filter(|n| *n > 0).is_none() {
        eprintln!("bench-file-search: {flag} must be a positive integer");
        std::process::exit(2);
    }
}

fn print_usage() {
    eprintln!("usage: cargo xtask bench-file-search [--runs N] [--entries N] [--queries CSV] [--files-only] [--debug]");
    eprintln!();
    eprintln!("Runs the ignored in-memory workspace file fuzzy-search benchmark.");
    eprintln!("Default profile is --release, default runs is 10, default entries is 500000.");
    eprintln!(
        "No filesystem tree is created; the benchmark generates synthetic indexed paths in memory."
    );
    eprintln!("Default queries: main, widget, config, controller, bench, zzz_nomatch.");
}
