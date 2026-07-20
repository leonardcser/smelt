use crate::bench_support::{run_cargo_test_benchmark, CargoTestBenchmark};

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

fn transcript_bench_env(env: Vec<(&'static str, String)>) -> Vec<(String, String)> {
    env.into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn run_tui_bench(
    bench_name: &'static str,
    test_filter: &'static str,
    release: bool,
    env: Vec<(&'static str, String)>,
) {
    run_cargo_test_benchmark(CargoTestBenchmark {
        package: "smelt-tui",
        test_filter,
        release,
        features: &["harness"],
        env: transcript_bench_env(env),
        bench_name,
    });
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
    let mut hot_path_item_bytes: Option<String> = None;
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
            "--save-request-item-bytes" => {
                hot_path = true;
                hot_path_item_bytes = Some(take_positive_usize_arg(
                    &mut iter,
                    "--save-request-item-bytes",
                ));
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

    let mut env = vec![("SMELT_TRANSCRIPT_BENCH_RUNS", runs.clone())];
    if let Some(workloads) = workloads {
        env.push(("SMELT_TRANSCRIPT_BENCH_WORKLOADS", workloads));
    }
    if skip_nav {
        env.push(("SMELT_TRANSCRIPT_BENCH_SKIP_NAV", "1".to_string()));
    }
    if search {
        env.push(("SMELT_TRANSCRIPT_BENCH_SEARCH", "1".to_string()));
    }
    if let Some(bytes) = &search_bytes {
        env.push(("SMELT_TRANSCRIPT_BENCH_SEARCH_BYTES", bytes.clone()));
    }
    if no_warmup {
        env.push(("SMELT_TRANSCRIPT_BENCH_NO_WARMUP", "1".to_string()));
    }
    if hot_path {
        env.push(("SMELT_TRANSCRIPT_HOT_PATH", "1".to_string()));
    }
    if let Some(history_len) = hot_path_history {
        env.push(("SMELT_TRANSCRIPT_HOT_PATH_HISTORY", history_len));
    }
    if let Some(item_bytes) = hot_path_item_bytes {
        env.push(("SMELT_TRANSCRIPT_HOT_PATH_ITEM_BYTES", item_bytes));
    }

    eprintln!(
        "running transcript layout benchmark: profile={} runs={}",
        if release { "release" } else { "test/debug" },
        runs
    );
    run_tui_bench(
        "bench-transcript-layout",
        "transcript_layout_",
        release,
        env,
    );

    if resume {
        let mut env = Vec::new();
        if let Some(bytes) = &resume_bytes {
            env.push(("SMELT_TRANSCRIPT_RESUME_BENCH_BYTES", bytes.clone()));
        }
        eprintln!(
            "running transcript resume benchmark: profile={}",
            if release { "release" } else { "test/debug" },
        );
        run_tui_bench(
            "bench-transcript-resume",
            "transcript_true_resume_benchmark_suite",
            release,
            env,
        );
    }
    if resumed_wheel {
        let mut env = Vec::new();
        let wheel_bytes = resume_bytes.clone().or(search_bytes.clone());
        if let Some(bytes) = wheel_bytes {
            env.push(("SMELT_TRANSCRIPT_RESUMED_WHEEL_BYTES", bytes));
        }
        if let Some(frames) = resumed_wheel_frames {
            env.push(("SMELT_TRANSCRIPT_RESUMED_WHEEL_FRAMES", frames));
        }
        if let Some(ticks) = resumed_wheel_ticks {
            env.push(("SMELT_TRANSCRIPT_RESUMED_WHEEL_TICKS", ticks));
        }
        eprintln!(
            "running transcript resumed wheel benchmark: profile={}",
            if release { "release" } else { "test/debug" },
        );
        run_tui_bench(
            "bench-transcript-resumed-wheel",
            "transcript_resumed_wheel_scroll_benchmark_suite",
            release,
            env,
        );
    }
}

fn print_usage() {
    eprintln!("usage: cargo xtask bench-transcript-layout [--runs N] [--workloads CSV] [--skip-nav] [--search] [--search-bytes N] [--resume] [--resume-bytes N] [--resumed-wheel] [--resumed-wheel-frames N] [--resumed-wheel-ticks N] [--save-request] [--save-request-history N] [--save-request-item-bytes N] [--scale-500mb] [--no-warmup] [--debug]");
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
    eprintln!("--save-request enables end-to-end Enter dispatch/redraw, save/append, rewind, and checkpointed/uncheckpointed provider-history samples.");
    eprintln!(
        "--save-request-history N sets its generated hot-path history length; default is 1024."
    );
    eprintln!("--save-request-item-bytes N pads each generated history item to at least N bytes for byte-heavy latency and memory tests.");
    eprintln!("--scale-500mb enables 500 MiB search/resume benchmark targets and disables warmup.");
    eprintln!("--no-warmup skips benchmark warmup samples for large-session runs.");
}
