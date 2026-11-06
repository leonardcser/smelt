use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DEFAULT_RUNS: usize = 20;
const INTERACTION_CEILING: Duration = Duration::from_millis(100);
const QUERY_PEAK_LIVE_BYTES_CEILING: usize = 8 * 1024 * 1024;

struct Options {
    state_dir: PathBuf,
    session: Option<String>,
    runs: usize,
    allow_debug: bool,
}

struct QueryCase {
    label: &'static str,
    query: &'static str,
    from_middle: bool,
    direction: smelt_store::TranscriptSearchDirection,
    expected_synthetic_match: bool,
}

#[derive(Default)]
struct QueryBenchmark {
    worst_p95: Duration,
    worst_peak_live_bytes_p95: usize,
    durations_ms: BTreeMap<&'static str, f64>,
    peak_live_bytes: BTreeMap<&'static str, usize>,
}

pub fn run(args: Vec<String>) {
    let options = parse_options(&args);
    if let Err(error) = run_benchmark(&options) {
        eprintln!("bench-session-search: {error}");
        std::process::exit(1);
    }
}

fn parse_options(args: &[String]) -> Options {
    let mut state_dir = None;
    let mut session = None;
    let mut runs = DEFAULT_RUNS;
    let mut allow_debug = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--state-dir" => {
                index += 1;
                state_dir = args.get(index).map(PathBuf::from);
            }
            "--session" => {
                index += 1;
                session = args.get(index).cloned();
            }
            "--runs" => {
                index += 1;
                runs = args
                    .get(index)
                    .and_then(|value| value.parse().ok())
                    .filter(|runs| *runs > 0)
                    .unwrap_or_else(|| usage_error("--runs requires a positive integer"));
            }
            "--allow-debug" => allow_debug = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => usage_error(&format!("unknown argument {other}")),
        }
        index += 1;
    }
    Options {
        state_dir: state_dir.unwrap_or_else(|| usage_error("--state-dir is required")),
        session,
        runs,
        allow_debug,
    }
}

fn usage_error(message: &str) -> ! {
    eprintln!("bench-session-search: {message}");
    print_usage();
    std::process::exit(2);
}

fn print_usage() {
    eprintln!(
        "usage: cargo run --release -p xtask -- bench-session-search --state-dir PATH [--session ID] [--runs N] [--allow-debug]"
    );
    eprintln!("PATH is a smelt state directory that contains sessions/sessions.db.");
    eprintln!("Timing thresholds require a release build unless --allow-debug is passed.");
}

fn run_benchmark(options: &Options) -> Result<(), Box<dyn std::error::Error>> {
    if cfg!(debug_assertions) && !options.allow_debug {
        return Err(
            "bench-session-search must run in release mode for timing thresholds; use `cargo run --release -p xtask -- bench-session-search ...` or pass --allow-debug"
                .into(),
        );
    }
    let store = smelt_store::Store::new(&options.state_dir);
    let sessions = match &options.session {
        Some(session) => vec![session.clone()],
        None => store.ids_with_prefix("", u32::MAX)?,
    };
    if sessions.is_empty() {
        return Err("state directory contains no sessions".into());
    }

    let mut worst_p95 = Duration::ZERO;
    let mut worst_peak_live_bytes_p95 = 0_usize;
    let mut reports = Vec::with_capacity(sessions.len());
    for session in &sessions {
        let reader = store.reader(session)?;
        let state = reader.state()?;
        let synthetic = state.metadata.slug.as_deref() == Some("synth");
        let benchmark = benchmark_queries(&reader, options.runs, synthetic)?;
        worst_p95 = worst_p95.max(benchmark.worst_p95);
        worst_peak_live_bytes_p95 =
            worst_peak_live_bytes_p95.max(benchmark.worst_peak_live_bytes_p95);
        reports.push(serde_json::json!({
            "session": session,
            "transcript_records": state.head.transcript_record_count.as_usize(),
            "query_p95_ms": benchmark.worst_p95.as_secs_f64() * 1000.0,
            "query_peak_live_bytes_p95": benchmark.worst_peak_live_bytes_p95,
            "queries": benchmark.durations_ms,
            "query_peak_live_bytes": benchmark.peak_live_bytes,
        }));
    }

    println!(
        "SESSION_SEARCH_BENCH_JSON {}",
        serde_json::to_string(&serde_json::json!({
            "sessions": reports.len(),
            "runs_per_query": options.runs,
            "worst_query_p95_ms": worst_p95.as_secs_f64() * 1000.0,
            "worst_query_peak_live_bytes_p95": worst_peak_live_bytes_p95,
            "results": reports,
        }))?
    );
    if worst_p95 > INTERACTION_CEILING {
        return Err(format!(
            "session search query p95 {:.3} ms exceeds {:.0} ms",
            worst_p95.as_secs_f64() * 1000.0,
            INTERACTION_CEILING.as_secs_f64() * 1000.0
        )
        .into());
    }
    if worst_peak_live_bytes_p95 > QUERY_PEAK_LIVE_BYTES_CEILING {
        return Err(format!(
            "session search query peak live allocation p95 {worst_peak_live_bytes_p95} bytes exceeds {QUERY_PEAK_LIVE_BYTES_CEILING} bytes"
        )
        .into());
    }
    Ok(())
}

fn benchmark_queries(
    reader: &smelt_store::SessionReader,
    runs: usize,
    synthetic: bool,
) -> Result<QueryBenchmark, smelt_store::StoreError> {
    use smelt_store::TranscriptSearchDirection::{Backward, Forward};
    let records = reader.head()?.transcript_record_count.as_usize() as u64;
    let middle_block_idx = reader
        .transcript_range(records / 2, records / 2 + 1)?
        .first()
        .map(|record| record.block_idx);
    let queries = [
        QueryCase {
            label: "one_common_forward",
            query: "e",
            from_middle: false,
            direction: Forward,
            expected_synthetic_match: true,
        },
        QueryCase {
            label: "one_unicode_forward",
            query: "é",
            from_middle: false,
            direction: Forward,
            expected_synthetic_match: true,
        },
        QueryCase {
            label: "three_common_forward",
            query: "the",
            from_middle: false,
            direction: Forward,
            expected_synthetic_match: true,
        },
        QueryCase {
            label: "three_common_backward",
            query: "the",
            from_middle: false,
            direction: Backward,
            expected_synthetic_match: true,
        },
        QueryCase {
            label: "three_common_middle_forward",
            query: "the",
            from_middle: true,
            direction: Forward,
            expected_synthetic_match: true,
        },
        QueryCase {
            label: "absent_full_scan",
            query: "smelt-search-absent-sentinel",
            from_middle: false,
            direction: Forward,
            expected_synthetic_match: false,
        },
    ];
    let mut benchmark = QueryBenchmark::default();
    for case in queries {
        let origin = case.from_middle.then_some(middle_block_idx).flatten();
        let search =
            || reader.search_transcript_candidate_page(case.query, origin, case.direction, 128);
        let warm = search()?;
        if synthetic && warm.is_empty() == case.expected_synthetic_match {
            return Err(smelt_store::StoreError::Integrity(format!(
                "synthetic benchmark query {} had an unexpected result",
                case.label
            )));
        }

        let mut durations = Vec::with_capacity(runs);
        let mut peak_live_bytes = Vec::with_capacity(runs);
        for _ in 0..runs {
            let measurement = smelt_perf::alloc::begin_peak_measurement();
            let baseline = measurement.start_bytes();
            let started = Instant::now();
            let results = search()?;
            durations.push(started.elapsed());
            drop(results);
            peak_live_bytes.push(measurement.finish().saturating_sub(baseline));
        }
        durations.sort_unstable();
        peak_live_bytes.sort_unstable();
        let percentile_index = (runs * 95).div_ceil(100).saturating_sub(1);
        let p95 = durations[percentile_index];
        let peak_live_bytes_p95 = peak_live_bytes[percentile_index];
        benchmark.worst_p95 = benchmark.worst_p95.max(p95);
        benchmark.worst_peak_live_bytes_p95 =
            benchmark.worst_peak_live_bytes_p95.max(peak_live_bytes_p95);
        benchmark
            .durations_ms
            .insert(case.label, p95.as_secs_f64() * 1000.0);
        benchmark
            .peak_live_bytes
            .insert(case.label, peak_live_bytes_p95);
    }
    Ok(benchmark)
}
