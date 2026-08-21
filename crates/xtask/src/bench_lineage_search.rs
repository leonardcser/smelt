use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

const DEFAULT_RUNS: usize = 20;
const PROJECTION_TIMEOUT: Duration = Duration::from_secs(600);
const INTERACTION_CEILING: Duration = Duration::from_millis(100);
const STORAGE_RATIO_CEILING: f64 = 0.25;
const QUERY_PEAK_LIVE_BYTES_CEILING: usize = 8 * 1024 * 1024;
const TRANSCRIPT_READ_CHUNK: u64 = 256;
const VERIFIED_RECORDS_METRIC: &str = "store:lineage:derived_search_records_verified";

struct Options {
    state_dir: PathBuf,
    session: Option<String>,
    runs: usize,
    allow_debug: bool,
}

struct QueryCase {
    label: &'static str,
    query: &'static str,
    origin_block_idx: Option<u64>,
    direction: smelt_store::TranscriptSearchDirection,
    expected_synthetic_match: bool,
    require_synthetic_verification: bool,
}

struct QueryBenchmark {
    worst_p95: Duration,
    worst_peak_live_bytes_p95: usize,
    durations_ms: BTreeMap<&'static str, f64>,
    peak_live_bytes: BTreeMap<&'static str, usize>,
    verified_records: BTreeMap<&'static str, u64>,
}

pub fn run(args: Vec<String>) {
    let options = parse_options(&args);
    if let Err(error) = run_benchmark(&options) {
        eprintln!("bench-lineage-search: {error}");
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
    eprintln!("bench-lineage-search: {message}");
    print_usage();
    std::process::exit(2);
}

fn print_usage() {
    eprintln!(
        "usage: cargo run --release -p xtask -- bench-lineage-search --state-dir PATH [--session ID] [--runs N] [--allow-debug]"
    );
    eprintln!("The state directory must contain canonical lineage sessions.");
    eprintln!(
        "Use a representative large corpus; storage accounting includes per-record overhead."
    );
    eprintln!("Timing thresholds require a release build unless --allow-debug is passed.");
}

fn run_benchmark(options: &Options) -> Result<(), Box<dyn std::error::Error>> {
    smelt_perf::perf::enable();
    if cfg!(debug_assertions) && !options.allow_debug {
        return Err(
            "bench-lineage-search must run in release mode for timing thresholds; use `cargo run --release -p xtask -- bench-lineage-search ...` or pass --allow-debug"
                .into(),
        );
    }

    let sessions = match &options.session {
        Some(session) => vec![session.clone()],
        None => smelt_store::lineage_session_ids(&options.state_dir)?,
    };
    if sessions.is_empty() {
        return Err("state directory contains no lineage sessions".into());
    }

    let mut total_text_bytes = 0_u64;
    let mut total_search_bytes = 0_u64;
    let mut worst_query_p95 = Duration::ZERO;
    let mut worst_query_peak_live_bytes_p95 = 0_usize;
    let mut reports = Vec::with_capacity(sessions.len());
    for session in &sessions {
        let reader = smelt_store::LineageSessionReader::open_existing(&options.state_dir, session)?;
        let snapshot = reader.snapshot()?;
        let synthetic = snapshot.metadata.slug.as_deref() == Some("synth");
        let projector = reader.spawn_search_projector()?;
        projector.request();
        let status = wait_for_projection(&reader, &projector)?;
        projector.stop();

        let text_bytes = canonical_text_bytes(&reader)?;
        if text_bytes == 0 {
            return Err(format!("session {session} has no searchable transcript bytes").into());
        }
        let search_path = reader.search_database_path();
        let physical = sqlite_physical_bytes(&search_path);
        let ratio = physical as f64 / text_bytes as f64;
        let query_benchmark = benchmark_queries(&reader, options.runs, synthetic)?;
        worst_query_p95 = worst_query_p95.max(query_benchmark.worst_p95);
        worst_query_peak_live_bytes_p95 =
            worst_query_peak_live_bytes_p95.max(query_benchmark.worst_peak_live_bytes_p95);
        total_text_bytes = total_text_bytes.saturating_add(text_bytes);
        total_search_bytes = total_search_bytes.saturating_add(physical);
        reports.push(serde_json::json!({
            "transcript_records": snapshot.transcript_len,
            "canonical_searchable_bytes": text_bytes,
            "search_physical_bytes": physical,
            "search_storage_ratio": ratio,
            "ready_segments": status.ready_segments,
            "query_p95_ms": query_benchmark.worst_p95.as_secs_f64() * 1000.0,
            "query_peak_live_bytes_p95": query_benchmark.worst_peak_live_bytes_p95,
            "queries": query_benchmark.durations_ms,
            "query_peak_live_bytes": query_benchmark.peak_live_bytes,
            "query_verified_records": query_benchmark.verified_records,
            "components": sqlite_components(&search_path)?,
        }));
    }

    let aggregate_ratio = total_search_bytes as f64 / total_text_bytes as f64;
    println!(
        "LINEAGE_SEARCH_BENCH_JSON {}",
        serde_json::to_string(&serde_json::json!({
            "sessions": reports.len(),
            "runs_per_query": options.runs,
            "canonical_searchable_bytes": total_text_bytes,
            "search_physical_bytes": total_search_bytes,
            "search_storage_ratio": aggregate_ratio,
            "worst_query_p95_ms": worst_query_p95.as_secs_f64() * 1000.0,
            "worst_query_peak_live_bytes_p95": worst_query_peak_live_bytes_p95,
            "results": reports,
        }))?
    );
    if aggregate_ratio > STORAGE_RATIO_CEILING {
        return Err(format!(
            "derived search storage ratio {:.2}% exceeds {:.0}%",
            aggregate_ratio * 100.0,
            STORAGE_RATIO_CEILING * 100.0
        )
        .into());
    }
    if worst_query_p95 > INTERACTION_CEILING {
        return Err(format!(
            "derived search query p95 {:.3} ms exceeds {:.0} ms",
            worst_query_p95.as_secs_f64() * 1000.0,
            INTERACTION_CEILING.as_secs_f64() * 1000.0
        )
        .into());
    }
    if worst_query_peak_live_bytes_p95 > QUERY_PEAK_LIVE_BYTES_CEILING {
        return Err(format!(
            "derived search query peak live allocation p95 {} bytes exceeds {} bytes",
            worst_query_peak_live_bytes_p95, QUERY_PEAK_LIVE_BYTES_CEILING
        )
        .into());
    }
    Ok(())
}

fn wait_for_projection(
    reader: &smelt_store::LineageSessionReader,
    projector: &smelt_store::LineageSearchProjector,
) -> Result<smelt_store::SearchProjectionStatus, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + PROJECTION_TIMEOUT;
    loop {
        if projector.is_idle() {
            if let Some(error) = projector.latest_error() {
                return Err(format!("projection worker failed: {error}").into());
            }
            let status = reader.search_projection_status()?;
            if status.state == smelt_store::SearchProjectionState::Current {
                return Ok(status);
            }
            return Err(format!("projection stopped before becoming current: {status:?}").into());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "projection did not finish; worker error: {:?}",
                projector.latest_error()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn canonical_text_bytes(
    reader: &smelt_store::LineageSessionReader,
) -> Result<u64, smelt_store::StoreError> {
    let total = reader.snapshot()?.transcript_len;
    let mut start = 0_u64;
    let mut bytes = 0_u64;
    while start < total {
        let end = start.saturating_add(TRANSCRIPT_READ_CHUNK).min(total);
        for record in reader.transcript_range(start, end)? {
            bytes = bytes.saturating_add(record.indexed_text.len() as u64);
        }
        start = end;
    }
    Ok(bytes)
}

fn benchmark_queries(
    reader: &smelt_store::LineageSessionReader,
    runs: usize,
    synthetic: bool,
) -> Result<QueryBenchmark, smelt_store::StoreError> {
    let snapshot = reader.snapshot()?;
    let origin_block_idx = if snapshot.transcript_len == 0 {
        None
    } else {
        reader
            .transcript_range(snapshot.transcript_len / 2, snapshot.transcript_len / 2 + 1)?
            .first()
            .map(|record| record.block_idx)
    };
    let queries = [
        QueryCase {
            label: "one_common_forward",
            query: "e",
            origin_block_idx: None,
            direction: smelt_store::TranscriptSearchDirection::Forward,
            expected_synthetic_match: true,
            require_synthetic_verification: false,
        },
        QueryCase {
            label: "one_unicode_forward",
            query: "é",
            origin_block_idx: None,
            direction: smelt_store::TranscriptSearchDirection::Forward,
            expected_synthetic_match: true,
            require_synthetic_verification: false,
        },
        QueryCase {
            label: "two_common_forward",
            query: "th",
            origin_block_idx: None,
            direction: smelt_store::TranscriptSearchDirection::Forward,
            expected_synthetic_match: true,
            require_synthetic_verification: false,
        },
        QueryCase {
            label: "two_punctuation_forward",
            query: "::",
            origin_block_idx: None,
            direction: smelt_store::TranscriptSearchDirection::Forward,
            expected_synthetic_match: true,
            require_synthetic_verification: false,
        },
        QueryCase {
            label: "three_common_forward",
            query: "the",
            origin_block_idx: None,
            direction: smelt_store::TranscriptSearchDirection::Forward,
            expected_synthetic_match: true,
            require_synthetic_verification: true,
        },
        QueryCase {
            label: "three_common_backward",
            query: "the",
            origin_block_idx: None,
            direction: smelt_store::TranscriptSearchDirection::Backward,
            expected_synthetic_match: true,
            require_synthetic_verification: true,
        },
        QueryCase {
            label: "three_common_origin_forward",
            query: "the",
            origin_block_idx,
            direction: smelt_store::TranscriptSearchDirection::Forward,
            expected_synthetic_match: true,
            require_synthetic_verification: true,
        },
        QueryCase {
            label: "forced_false_positive_absent",
            query: "unicode sample café path::segment",
            origin_block_idx: None,
            direction: smelt_store::TranscriptSearchDirection::Forward,
            expected_synthetic_match: false,
            require_synthetic_verification: true,
        },
        QueryCase {
            label: "index_miss_absent",
            query: "smelt-search-absent-sentinel",
            origin_block_idx: None,
            direction: smelt_store::TranscriptSearchDirection::Forward,
            expected_synthetic_match: false,
            require_synthetic_verification: false,
        },
    ];
    let mut benchmark = QueryBenchmark {
        worst_p95: Duration::ZERO,
        worst_peak_live_bytes_p95: 0,
        durations_ms: BTreeMap::new(),
        peak_live_bytes: BTreeMap::new(),
        verified_records: BTreeMap::new(),
    };
    for query in queries {
        smelt_perf::perf::set_enabled(true);
        smelt_perf::perf::clear();
        let warm = reader.search_transcript_candidate_page(
            query.query,
            query.origin_block_idx,
            query.direction,
            128,
        )?;
        let verified_records = metric_total(VERIFIED_RECORDS_METRIC);
        if synthetic {
            let matched = !warm.is_empty();
            if matched != query.expected_synthetic_match {
                return Err(smelt_store::StoreError::Integrity(format!(
                    "synthetic benchmark query {} had an unexpected result",
                    query.label
                )));
            }
            if query.require_synthetic_verification && verified_records == 0 {
                return Err(smelt_store::StoreError::Integrity(format!(
                    "synthetic benchmark query {} did not verify canonical candidates",
                    query.label
                )));
            }
        }
        benchmark
            .verified_records
            .insert(query.label, verified_records);
        smelt_perf::perf::set_enabled(false);

        let mut durations = Vec::with_capacity(runs);
        let mut peak_live_bytes = Vec::with_capacity(runs);
        for _ in 0..runs {
            let measurement = smelt_perf::alloc::begin_peak_measurement();
            let baseline = measurement.start_bytes();
            let started = Instant::now();
            let results = reader.search_transcript_candidate_page(
                query.query,
                query.origin_block_idx,
                query.direction,
                128,
            )?;
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
            .insert(query.label, p95.as_secs_f64() * 1000.0);
        benchmark
            .peak_live_bytes
            .insert(query.label, peak_live_bytes_p95);
    }
    Ok(benchmark)
}

fn metric_total(label: &str) -> u64 {
    smelt_perf::perf::snapshot()
        .values
        .into_iter()
        .find(|row| row.label == label)
        .map_or(0, |row| row.total)
}

fn sqlite_physical_bytes(path: &Path) -> u64 {
    [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ]
    .into_iter()
    .filter_map(|path| std::fs::metadata(path).ok())
    .map(|metadata| metadata.len())
    .sum()
}

fn sqlite_components(path: &Path) -> Result<BTreeMap<String, u64>, rusqlite::Error> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut components = BTreeMap::new();
    let mut statement = conn.prepare(
        "SELECT name, SUM(pgsize) FROM dbstat
         WHERE name LIKE 'search_%' GROUP BY name ORDER BY name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (name, bytes) = row?;
        let bytes = bytes.max(0) as u64;
        let component = if name.starts_with("search_fts") {
            "fts"
        } else if name.starts_with("search_short_postings") {
            "short_postings"
        } else if name.starts_with("search_docs") {
            "document_mapping"
        } else {
            "segments_and_metadata"
        };
        *components.entry(component.to_string()).or_default() += bytes;
    }
    let page_size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let free_pages: i64 = conn.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    components.insert(
        "free_pages".into(),
        (page_size.max(0) as u64).saturating_mul(free_pages.max(0) as u64),
    );
    Ok(components)
}
