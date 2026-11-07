use crate::bench_support::{run_cargo_test_benchmark, CargoTestBenchmark};

use std::collections::BTreeMap;

// One mapping for argument parsing, environment forwarding, and help output.
const VALUES: &[(&str, &str)] = &[
    ("runs", "SMELT_TRANSCRIPT_BENCH_RUNS"),
    ("workloads", "SMELT_TRANSCRIPT_BENCH_WORKLOADS"),
    ("search-bytes", "SMELT_TRANSCRIPT_BENCH_SEARCH_BYTES"),
    ("resume-bytes", "SMELT_TRANSCRIPT_RESUME_BENCH_BYTES"),
    (
        "resumed-wheel-frames",
        "SMELT_TRANSCRIPT_RESUMED_WHEEL_FRAMES",
    ),
    (
        "resumed-wheel-ticks",
        "SMELT_TRANSCRIPT_RESUMED_WHEEL_TICKS",
    ),
    (
        "active-memory-bytes",
        "SMELT_TRANSCRIPT_ACTIVE_MEMORY_BYTES",
    ),
    ("save-request-history", "SMELT_TRANSCRIPT_HOT_PATH_HISTORY"),
    (
        "save-request-item-bytes",
        "SMELT_TRANSCRIPT_HOT_PATH_ITEM_BYTES",
    ),
    (
        "save-request-operations",
        "SMELT_TRANSCRIPT_HOT_PATH_OPERATIONS",
    ),
    ("save-request-fixture", "SMELT_TRANSCRIPT_HOT_PATH_FIXTURE"),
    (
        "save-request-session-id",
        "SMELT_TRANSCRIPT_HOT_PATH_SESSION_ID",
    ),
    ("stream-history", "SMELT_TRANSCRIPT_STREAM_HISTORY"),
    (
        "stream-resumed-bytes",
        "SMELT_TRANSCRIPT_STREAM_RESUMED_BYTES",
    ),
    (
        "stream-resumed-position",
        "SMELT_TRANSCRIPT_STREAM_RESUMED_POSITION",
    ),
    (
        "stream-boundary-record-bytes",
        "SMELT_TRANSCRIPT_STREAM_BOUNDARY_RECORD_BYTES",
    ),
    ("stream-chunks", "SMELT_TRANSCRIPT_STREAM_CHUNKS"),
    ("stream-bytes", "SMELT_TRANSCRIPT_STREAM_BYTES"),
    ("stream-width", "SMELT_TRANSCRIPT_STREAM_WIDTH"),
    ("stream-height", "SMELT_TRANSCRIPT_STREAM_HEIGHT"),
    (
        "stream-parallel-tools",
        "SMELT_TRANSCRIPT_STREAM_PARALLEL_TOOLS",
    ),
    ("stream-workload", "SMELT_TRANSCRIPT_STREAM_WORKLOAD"),
    ("stream-idle-frames", "SMELT_TRANSCRIPT_STREAM_IDLE_FRAMES"),
    ("tall-write-lines", "SMELT_TRANSCRIPT_TALL_WRITE_LINES"),
    ("tall-diff-lines", "SMELT_TRANSCRIPT_TALL_DIFF_LINES"),
    ("scaling-blocks", "SMELT_TRANSCRIPT_SCALING_BLOCKS"),
    ("user-jump-count", "SMELT_TRANSCRIPT_USER_JUMP_COUNT"),
    ("user-jump-fixture", "SMELT_TRANSCRIPT_USER_JUMP_FIXTURE"),
    (
        "user-jump-session-id",
        "SMELT_TRANSCRIPT_USER_JUMP_SESSION_ID",
    ),
];
const SWITCHES: &[(&str, &str)] = &[
    (
        "save-request-heterogeneous",
        "SMELT_TRANSCRIPT_HOT_PATH_HETEROGENEOUS",
    ),
    ("stream-scheduled", "SMELT_TRANSCRIPT_STREAM_SCHEDULED"),
    ("stream-scroll", "SMELT_TRANSCRIPT_STREAM_SCROLL"),
    ("no-warmup", "SMELT_TRANSCRIPT_BENCH_NO_WARMUP"),
    ("scaling-compacted", "SMELT_TRANSCRIPT_SCALING_COMPACTED"),
];
const SELECTORS: &[&str] = &[
    "skip-nav",
    "search",
    "save-request",
    "stream",
    "scaling",
    "resume",
    "resumed-wheel",
    "active-memory",
    "save-request-only",
    "stream-only",
    "tall-write-only",
    "tall-diff-only",
    "scale-500mb",
    "keys",
    "debug",
];

const SUITES: &[(&str, &str)] = &[
    ("layout", "transcript_layout_projection_benchmark_suite"),
    ("navigation", "transcript_layout_navigation_benchmark_suite"),
    (
        "tall-write",
        "transcript_layout_tall_write_file_benchmark_suite",
    ),
    (
        "tall-diff",
        "transcript_layout_tall_retained_diff_benchmark_suite",
    ),
    ("search", "transcript_layout_search_benchmark_suite"),
    ("save-request", "transcript_layout_hot_path_benchmark_suite"),
    ("stream", "transcript_stream_benchmark_suite"),
    ("scaling", "transcript_scaling_benchmark_suite"),
    ("resume", "transcript_true_resume_benchmark_suite"),
    ("wheel", "transcript_resumed_wheel_scroll_benchmark_suite"),
    ("user-jumps", "transcript_resumed_user_jump_benchmark_suite"),
    ("memory", "transcript_active_memory_benchmark_suite"),
    ("keys", "transcript_keys_benchmark_suite"),
    ("ui", "default_ui_frame_benchmark_suite"),
    ("resume-dialog", "resume_dialog_open_benchmark_suite"),
    ("sparse-watch", "transcript_sparse_watcher_benchmark_suite"),
    (
        "sparse-idle",
        "transcript_sparse_autonomous_frame_benchmark_suite",
    ),
];

#[derive(Debug)]
struct Plan {
    options: BTreeMap<&'static str, String>,
    suites: Vec<&'static str>,
}

impl Plan {
    fn parse(args: Vec<String>) -> Result<Self, String> {
        let mut options = BTreeMap::from([("runs", "5".to_string())]);
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            let name = arg
                .strip_prefix("--")
                .ok_or_else(|| format!("unknown argument `{arg}`"))?;
            if name == "suite" {
                let value = iter.next().ok_or("--suite requires a name")?;
                if !SUITES.iter().any(|(key, _)| *key == value) {
                    return Err(format!("unknown suite `{value}`"));
                }
                options.insert("suite", value);
            } else if let Some(&(key, _)) = VALUES.iter().find(|(key, _)| *key == name) {
                let value = iter
                    .next()
                    .ok_or_else(|| format!("--{key} requires a value"))?;
                match key {
                    "stream-resumed-position" => {
                        if !["top", "middle", "tail"].contains(&value.as_str()) {
                            return Err(format!("--{key} must be top, middle, or tail"));
                        }
                    }
                    "stream-workload" => {
                        if ![
                            "text",
                            "bash",
                            "mixed",
                            "exec",
                            "write-draft",
                            "explore-group",
                        ]
                        .contains(&value.as_str())
                        {
                            return Err(format!("unknown --{key}: {value}"));
                        }
                    }
                    "scaling-blocks" => {
                        tui::benchmark_config::scaling_block_counts(&value)
                            .map_err(|error| format!("--{key}: {error}"))?;
                    }
                    "workloads" | "save-request-operations" => {
                        use tui::benchmark_config::{
                            named_list, HOT_PATH_OPERATIONS, LAYOUT_WORKLOADS,
                        };
                        let allowed = if key == "workloads" {
                            LAYOUT_WORKLOADS
                                .iter()
                                .map(|entry| entry.0)
                                .collect::<Vec<_>>()
                        } else {
                            HOT_PATH_OPERATIONS.to_vec()
                        };
                        named_list(&value, allowed).map_err(|error| format!("--{key}: {error}"))?;
                    }
                    "save-request-fixture"
                    | "save-request-session-id"
                    | "user-jump-fixture"
                    | "user-jump-session-id" => {
                        if value.is_empty() || value.starts_with("--") {
                            return Err(format!("--{key} requires a value"));
                        }
                    }
                    _ => {
                        let min = if key == "save-request-history" { 4 } else { 1 };
                        if value.parse::<usize>().ok().filter(|n| *n >= min).is_none() {
                            return Err(format!("--{key} must be an integer >= {min}"));
                        }
                    }
                }
                options.insert(key, value);
            } else if let Some(key) = SWITCHES
                .iter()
                .map(|(key, _)| *key)
                .chain(SELECTORS.iter().copied())
                .find(|key| *key == name)
            {
                options.insert(key, "1".into());
            } else {
                return Err(format!("unknown argument `{arg}`"));
            }
        }
        if options.contains_key("save-request-fixture")
            != options.contains_key("save-request-session-id")
        {
            return Err(
                "--save-request-fixture and --save-request-session-id must be provided together"
                    .into(),
            );
        }
        if options.contains_key("user-jump-fixture") != options.contains_key("user-jump-session-id")
        {
            return Err(
                "--user-jump-fixture and --user-jump-session-id must be provided together".into(),
            );
        }
        if options.contains_key("scale-500mb") {
            for key in [
                "search",
                "resume",
                "resumed-wheel",
                "active-memory",
                "no-warmup",
            ] {
                options.insert(key, "1".into());
            }
            for key in ["search-bytes", "resume-bytes", "active-memory-bytes"] {
                options.insert(key, (500usize * 1024 * 1024).to_string());
            }
        }
        for (prefix, enable) in [
            ("stream-", "stream"),
            ("save-request-", "save-request"),
            ("scaling-", "scaling"),
            ("resumed-wheel-", "resumed-wheel"),
            ("active-memory-", "active-memory"),
            ("tall-diff-", "tall-diff-only"),
        ] {
            if options.keys().any(|key| key.starts_with(prefix)) {
                options.insert(enable, "1".into());
            }
        }
        if options.contains_key("stream-idle-frames") {
            options.insert("stream-scheduled", "1".into());
        }
        if options.contains_key("save-request-fixture") {
            options.insert("save-request-only", "1".into());
        }
        let exclusive = [
            "keys",
            "save-request-only",
            "stream-only",
            "tall-write-only",
            "tall-diff-only",
        ];
        if exclusive
            .iter()
            .filter(|key| options.contains_key(**key))
            .count()
            > 1
        {
            return Err(format!(
                "choose only one of {}",
                exclusive.map(|key| format!("--{key}")).join(", ")
            ));
        }
        let mut names = if let Some(name) = options.get("suite") {
            vec![name.as_str()]
        } else if let Some(key) = exclusive.iter().find(|key| options.contains_key(**key)) {
            vec![key.strip_suffix("-only").unwrap_or(key)]
        } else {
            let mut names = vec!["layout", "tall-write"];
            if !options.contains_key("skip-nav") {
                names.push("navigation");
            }
            for key in ["search", "save-request", "stream"] {
                if options.contains_key(key) {
                    names.push(key);
                }
            }
            names
        };
        if !options.contains_key("suite") && !options.contains_key("keys") {
            for (key, name) in [
                ("scaling", "scaling"),
                ("resume", "resume"),
                ("resumed-wheel", "wheel"),
                ("active-memory", "memory"),
            ] {
                if options.contains_key(key) {
                    names.push(name);
                }
            }
        }
        let suites = names
            .into_iter()
            .map(|name| SUITES.iter().find(|(key, _)| *key == name).unwrap().1)
            .collect();
        Ok(Self { options, suites })
    }

    fn env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        for (key, var) in VALUES.iter().chain(SWITCHES) {
            if let Some(value) = self.options.get(key) {
                env.push(((*var).into(), value.clone()));
            }
        }
        if let Some(bytes) = self
            .options
            .get("resume-bytes")
            .or_else(|| self.options.get("search-bytes"))
        {
            env.push(("SMELT_TRANSCRIPT_RESUMED_WHEEL_BYTES".into(), bytes.clone()));
        }
        env
    }
}

pub fn run(args: Vec<String>) {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        return;
    }
    let plan = Plan::parse(args).unwrap_or_else(|error| {
        eprintln!("bench-transcript-layout: {error}");
        std::process::exit(2);
    });

    for test_filter in &plan.suites {
        run_cargo_test_benchmark(CargoTestBenchmark {
            package: "smelt-tui",
            test_filter,
            release: !plan.options.contains_key("debug"),
            features: &["harness", "transcript-bench"],
            env: plan.env(),
            bench_name: test_filter,
        });
    }
}

fn print_usage() {
    eprintln!("usage: cargo xtask bench-transcript-layout [options]");
    eprintln!("Default: release profile, 5 runs, layout, navigation and tall-write suites.");
    eprintln!("Workloads, defaults, metrics and examples: docs/transcript-layout-benchmarks.md");
    eprintln!(
        "--suite NAME runs only that suite: {}",
        SUITES
            .iter()
            .map(|(key, _)| *key)
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!("Value options:");
    for (key, _) in VALUES {
        eprintln!("  --{key} VALUE");
    }
    eprintln!("Switches:");
    for key in SWITCHES
        .iter()
        .map(|(key, _)| *key)
        .chain(SELECTORS.iter().copied())
    {
        eprintln!("  --{key}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Plan, String> {
        Plan::parse(args.iter().map(|arg| (*arg).into()).collect())
    }

    #[test]
    fn streaming_options_enable_the_selected_workload_and_scheduler() {
        let plan = parse(&[
            "--stream-only",
            "--stream-workload",
            "bash",
            "--stream-idle-frames",
            "12",
            "--runs",
            "3",
        ])
        .unwrap();
        assert_eq!(plan.suites, ["transcript_stream_benchmark_suite"]);
        let env: BTreeMap<_, _> = plan.env().into_iter().collect();
        for (key, value) in [
            ("SMELT_TRANSCRIPT_STREAM_WORKLOAD", "bash"),
            ("SMELT_TRANSCRIPT_STREAM_SCHEDULED", "1"),
            ("SMELT_TRANSCRIPT_STREAM_IDLE_FRAMES", "12"),
            ("SMELT_TRANSCRIPT_BENCH_RUNS", "3"),
        ] {
            assert_eq!(env.get(key).map(String::as_str), Some(value));
        }
    }

    #[test]
    fn large_session_preset_shares_sizes_and_run_count_across_suites() {
        let plan = parse(&["--scale-500mb", "--runs", "2"]).unwrap();
        assert_eq!(plan.suites.len(), 7);
        let env: BTreeMap<_, _> = plan.env().into_iter().collect();
        for key in [
            "SMELT_TRANSCRIPT_BENCH_SEARCH_BYTES",
            "SMELT_TRANSCRIPT_RESUME_BENCH_BYTES",
            "SMELT_TRANSCRIPT_RESUMED_WHEEL_BYTES",
            "SMELT_TRANSCRIPT_ACTIVE_MEMORY_BYTES",
        ] {
            assert_eq!(env[key], (500usize * 1024 * 1024).to_string());
        }
        assert_eq!(env["SMELT_TRANSCRIPT_BENCH_RUNS"], "2");
        assert_eq!(env["SMELT_TRANSCRIPT_BENCH_NO_WARMUP"], "1");
    }

    #[test]
    fn named_suite_excludes_default_and_inferred_suites() {
        let plan = parse(&["--suite", "stream", "--stream-history", "4", "--resume"]).unwrap();
        assert_eq!(plan.suites, ["transcript_stream_benchmark_suite"]);
        let combined = parse(&["--stream"]).unwrap();
        assert!(combined
            .suites
            .contains(&"transcript_stream_benchmark_suite"));
    }

    #[test]
    fn list_options_preserve_selected_workloads_and_sizes() {
        let plan = parse(&[
            "--suite",
            "scaling",
            "--scaling-blocks",
            "64, 128",
            "--workloads",
            "mixed_10mib, markdown_4mib",
            "--save-request-operations",
            "submit_enter, noop_save",
        ])
        .unwrap();
        let env: BTreeMap<_, _> = plan.env().into_iter().collect();
        assert_eq!(env["SMELT_TRANSCRIPT_SCALING_BLOCKS"], "64, 128");
        assert_eq!(
            env["SMELT_TRANSCRIPT_BENCH_WORKLOADS"],
            "mixed_10mib, markdown_4mib"
        );
        assert_eq!(
            env["SMELT_TRANSCRIPT_HOT_PATH_OPERATIONS"],
            "submit_enter, noop_save"
        );
        assert_eq!(
            tui::benchmark_config::scaling_block_counts("64, 128").unwrap(),
            [64, 128]
        );
    }

    #[test]
    fn invalid_arguments_fail_before_building_fixtures() {
        for args in [
            vec!["--runs"],
            vec!["--runs", "0"],
            vec!["runs", "2"],
            vec!["--search-bytes", "invalid"],
            vec!["--scaling-blocks", "invalid"],
            vec!["--scaling-blocks", "64,invalid"],
            vec!["--scaling-blocks", "63,128"],
            vec!["--scaling-blocks", "64,"],
            vec!["--workloads", "mixed_10mib,unknown"],
            vec!["--workloads", ""],
            vec!["--workloads", "mixed_10mib,"],
            vec!["--save-request-operations", "submit_enter,unknown"],
            vec!["--save-request-operations", "submit_enter,,noop_save"],
            vec!["--stream-workload", "unknown"],
            vec!["--save-request-fixture", "/tmp/fixture"],
            vec!["--stream-only", "--tall-diff-lines", "10"],
            vec![
                "--stream-only",
                "--save-request-fixture",
                "/tmp/fixture",
                "--save-request-session-id",
                "id",
            ],
        ] {
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
    }
}
