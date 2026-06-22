use std::process::Command;

fn take_required_arg(iter: &mut impl Iterator<Item = String>, name: &str) -> String {
    iter.next().unwrap_or_else(|| {
        eprintln!("bench-transcript-layout: {name} requires a value");
        std::process::exit(2);
    })
}

fn take_positive_usize_arg(iter: &mut impl Iterator<Item = String>, name: &str) -> String {
    let value = take_required_arg(iter, name);
    if value.parse::<usize>().ok().filter(|n| *n > 0).is_none() {
        eprintln!("bench-transcript-layout: {name} must be a positive integer");
        std::process::exit(2);
    }
    value
}

fn cargo_tui_test(release: bool) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.args(["test", "-p", "smelt-tui", "--features", "harness"]);
    if release {
        cmd.arg("--release");
    }
    cmd
}

pub fn run(args: Vec<String>) {
    let mut runs = String::from("5");
    let mut workloads: Option<String> = None;
    let mut release = true;
    let mut skip_nav = false;
    let mut search = false;
    let mut search_bytes: Option<String> = None;
    let mut resume = false;
    let mut resume_bytes: Option<String> = None;
    let mut resumed_wheel = false;
    let mut resumed_wheel_frames: Option<String> = None;
    let mut resumed_wheel_ticks: Option<String> = None;
    let mut hot_path = false;
    let mut hot_path_history: Option<String> = None;
    let mut no_warmup = false;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--runs" => {
                runs = take_positive_usize_arg(&mut iter, "--runs");
            }
            "--workloads" => {
                workloads = Some(take_required_arg(&mut iter, "--workloads"));
            }
            "--skip-nav" => skip_nav = true,
            "--search" => search = true,
            "--search-bytes" => {
                search_bytes = Some(take_required_arg(&mut iter, "--search-bytes"));
            }
            "--resume" => resume = true,
            "--resume-bytes" => {
                resume_bytes = Some(take_required_arg(&mut iter, "--resume-bytes"));
            }
            "--resumed-wheel" => resumed_wheel = true,
            "--resumed-wheel-frames" => {
                resumed_wheel = true;
                resumed_wheel_frames =
                    Some(take_positive_usize_arg(&mut iter, "--resumed-wheel-frames"));
            }
            "--resumed-wheel-ticks" => {
                resumed_wheel = true;
                resumed_wheel_ticks =
                    Some(take_positive_usize_arg(&mut iter, "--resumed-wheel-ticks"));
            }
            "--save-request" => hot_path = true,
            "--scale-500mb" => {
                resumed_wheel = true;
                search = true;
                resume = true;
                no_warmup = true;
                search_bytes = Some((500usize * 1024 * 1024).to_string());
                resume_bytes = Some((500usize * 1024 * 1024).to_string());
            }
            "--no-warmup" => no_warmup = true,
            "--save-request-history" => {
                let value = iter.next().unwrap_or_else(|| {
                    eprintln!("bench-transcript-layout: --save-request-history requires a value");
                    std::process::exit(2);
                });
                if value.parse::<usize>().ok().filter(|n| *n >= 4).is_none() {
                    eprintln!(
                        "bench-transcript-layout: --save-request-history must be an integer >= 4"
                    );
                    std::process::exit(2);
                }
                hot_path = true;
                hot_path_history = Some(value);
            }
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

    let mut cmd = cargo_tui_test(release);
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
    if search {
        cmd.env("SMELT_TRANSCRIPT_BENCH_SEARCH", "1");
    }
    if let Some(bytes) = &search_bytes {
        cmd.env("SMELT_TRANSCRIPT_BENCH_SEARCH_BYTES", bytes);
    }
    if no_warmup {
        cmd.env("SMELT_TRANSCRIPT_BENCH_NO_WARMUP", "1");
    }
    if hot_path {
        cmd.env("SMELT_TRANSCRIPT_HOT_PATH", "1");
    }
    if let Some(history_len) = hot_path_history {
        cmd.env("SMELT_TRANSCRIPT_HOT_PATH_HISTORY", history_len);
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

    if resume {
        let mut cmd = cargo_tui_test(release);
        cmd.args([
            "transcript_true_resume_benchmark_suite",
            "--",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]);
        if let Some(bytes) = &resume_bytes {
            cmd.env("SMELT_TRANSCRIPT_RESUME_BENCH_BYTES", bytes);
        }
        eprintln!(
            "running transcript resume benchmark: profile={}",
            if release { "release" } else { "test/debug" },
        );
        let status = cmd.status().unwrap_or_else(|e| {
            eprintln!("bench-transcript-layout: failed to run resume cargo test: {e}");
            std::process::exit(1);
        });
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
    }
    if resumed_wheel {
        let mut cmd = cargo_tui_test(release);
        cmd.args([
            "transcript_resumed_wheel_scroll_benchmark_suite",
            "--",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]);
        let wheel_bytes = resume_bytes.clone().or(search_bytes.clone());
        if let Some(bytes) = wheel_bytes {
            cmd.env("SMELT_TRANSCRIPT_RESUMED_WHEEL_BYTES", bytes);
        }
        if let Some(frames) = resumed_wheel_frames {
            cmd.env("SMELT_TRANSCRIPT_RESUMED_WHEEL_FRAMES", frames);
        }
        if let Some(ticks) = resumed_wheel_ticks {
            cmd.env("SMELT_TRANSCRIPT_RESUMED_WHEEL_TICKS", ticks);
        }
        eprintln!(
            "running transcript resumed wheel benchmark: profile={}",
            if release { "release" } else { "test/debug" },
        );
        let status = cmd.status().unwrap_or_else(|e| {
            eprintln!("bench-transcript-layout: failed to run resumed wheel cargo test: {e}");
            std::process::exit(1);
        });
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
    }
}

fn print_usage() {
    eprintln!("usage: cargo xtask bench-transcript-layout [--runs N] [--workloads CSV] [--skip-nav] [--search] [--search-bytes N] [--resume] [--resume-bytes N] [--resumed-wheel] [--resumed-wheel-frames N] [--resumed-wheel-ticks N] [--save-request] [--save-request-history N] [--scale-500mb] [--no-warmup] [--debug]");
    eprintln!();
    eprintln!("Runs the ignored transcript layout benchmark suite and prints mean±stddev tables.");
    eprintln!("Default profile is --release and default runs is 5.");
    eprintln!();
    eprintln!("workloads: mixed_10mib, mixed_50mib, markdown_4mib, tool_output_4mib, tiny_blocks_1mib, huge_blocks_4mib");
    eprintln!("--skip-nav omits the app-level navigation/search suite for projection-only runs.");
    eprintln!("--search enables the large app-level transcript search benchmark.");
    eprintln!("--search-bytes N sets its generated transcript size; default is 50 MiB.");
    eprintln!("--resume runs the true session resume benchmark after layout/search.");
    eprintln!("--resume-bytes N sets its generated resume session size; default is 10 MiB.");
    eprintln!("--resumed-wheel runs a sparse resumed-session wheel-scroll benchmark.");
    eprintln!("--resumed-wheel-frames N sets rendered wheel frames; default is 240.");
    eprintln!(
        "--resumed-wheel-ticks N sets wheel events queued per rendered frame; default is 24."
    );
    eprintln!("--save-request enables no-op save, append, HistoryUpdated, rewind, and provider-history wall-time samples.");
    eprintln!(
        "--save-request-history N sets its generated hot-path history length; default is 1024."
    );
    eprintln!("--scale-500mb enables 500 MiB search/resume benchmark targets and disables warmup.");
    eprintln!("--no-warmup skips benchmark warmup samples for large-session runs.");
}
