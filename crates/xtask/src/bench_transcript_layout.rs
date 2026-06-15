use std::process::Command;

pub fn run(args: Vec<String>) {
    let mut runs = String::from("5");
    let mut workloads: Option<String> = None;
    let mut release = true;
    let mut skip_nav = false;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--runs" => {
                runs = iter.next().unwrap_or_else(|| {
                    eprintln!("bench-transcript-layout: --runs requires a value");
                    std::process::exit(2);
                });
                if runs.parse::<usize>().ok().filter(|n| *n > 0).is_none() {
                    eprintln!("bench-transcript-layout: --runs must be a positive integer");
                    std::process::exit(2);
                }
            }
            "--workloads" => {
                workloads = Some(iter.next().unwrap_or_else(|| {
                    eprintln!(
                        "bench-transcript-layout: --workloads requires a comma-separated value"
                    );
                    std::process::exit(2);
                }));
            }
            "--skip-nav" => skip_nav = true,
            "--debug" => release = false,
            "-h" | "--help" => {
                print_usage();
                return;
            }
            other => {
                eprintln!("bench-transcript-layout: unknown argument `{other}`");
                print_usage();
                std::process::exit(2);
            }
        }
    }

    let mut cmd = Command::new("cargo");
    cmd.args(["test", "-p", "smelt-tui"]);
    if release {
        cmd.arg("--release");
    }
    cmd.args([
        "transcript_layout_",
        "--",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    cmd.env("SMELT_TRANSCRIPT_BENCH_RUNS", &runs);
    if let Some(workloads) = workloads {
        cmd.env("SMELT_TRANSCRIPT_BENCH_WORKLOADS", workloads);
    }
    if skip_nav {
        cmd.env("SMELT_TRANSCRIPT_BENCH_SKIP_NAV", "1");
    }

    eprintln!(
        "running transcript layout benchmark: profile={} runs={}",
        if release { "release" } else { "test/debug" },
        runs
    );
    let status = cmd.status().unwrap_or_else(|e| {
        eprintln!("bench-transcript-layout: failed to run cargo test: {e}");
        std::process::exit(1);
    });
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn print_usage() {
    eprintln!("usage: cargo xtask bench-transcript-layout [--runs N] [--workloads CSV] [--skip-nav] [--debug]");
    eprintln!();
    eprintln!("Runs the ignored transcript layout benchmark suite and prints mean±stddev tables.");
    eprintln!("Default profile is --release and default runs is 5.");
    eprintln!();
    eprintln!("workloads: mixed_10mib, mixed_50mib, markdown_4mib, tool_output_4mib, tiny_blocks_1mib, huge_blocks_4mib");
    eprintln!("--skip-nav omits the app-level navigation/search suite for projection-only runs.");
}
