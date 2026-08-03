use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

const DEFAULT_RUNS: usize = 20;
const PROJECTION_TIMEOUT: Duration = Duration::from_secs(600);
const INTERACTION_CEILING: Duration = Duration::from_millis(100);
const STORAGE_RATIO_CEILING: f64 = 0.25;
const TRANSCRIPT_READ_CHUNK: u64 = 256;

struct Options {
    state_dir: PathBuf,
    session: Option<String>,
    runs: usize,
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
    }
}

fn usage_error(message: &str) -> ! {
    eprintln!("bench-lineage-search: {message}");
    print_usage();
    std::process::exit(2);
}

fn print_usage() {
    eprintln!("usage: cargo xtask bench-lineage-search --state-dir PATH [--session ID] [--runs N]");
    eprintln!("The state directory must contain canonical lineage sessions.");
}

fn run_benchmark(options: &Options) -> Result<(), Box<dyn std::error::Error>> {
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
    let mut reports = Vec::with_capacity(sessions.len());
    for session in &sessions {
        let writer = smelt_store::OwnedLineageWriter::open_existing(&options.state_dir, session)?;
        let projector = writer.spawn_search_projector()?;
        projector.request();
        let reader = smelt_store::LineageSessionReader::open_existing(&options.state_dir, session)?;
        let status = wait_for_projection(&reader, &projector)?;
        projector.stop();

        let text_bytes = canonical_text_bytes(&reader)?;
        let search_path = reader
            .database_path()
            .parent()
            .ok_or("lineage database has no parent directory")?
            .join("search.db");
        let physical = sqlite_physical_bytes(&search_path);
        let ratio = if text_bytes == 0 {
            0.0
        } else {
            physical as f64 / text_bytes as f64
        };
        let (query_p95, query_reports) = benchmark_queries(&reader, options.runs)?;
        worst_query_p95 = worst_query_p95.max(query_p95);
        total_text_bytes = total_text_bytes.saturating_add(text_bytes);
        total_search_bytes = total_search_bytes.saturating_add(physical);
        reports.push(serde_json::json!({
            "transcript_records": reader.snapshot()?.transcript_len,
            "canonical_searchable_bytes": text_bytes,
            "search_physical_bytes": physical,
            "search_storage_ratio": ratio,
            "ready_segments": status.ready_segments,
            "query_p95_ms": query_p95.as_secs_f64() * 1000.0,
            "queries": query_reports,
            "components": sqlite_components(&search_path)?,
        }));
        writer.release()?;
    }

    let aggregate_ratio = if total_text_bytes == 0 {
        0.0
    } else {
        total_search_bytes as f64 / total_text_bytes as f64
    };
    println!(
        "LINEAGE_SEARCH_BENCH_JSON {}",
        serde_json::to_string(&serde_json::json!({
            "sessions": reports.len(),
            "runs_per_query": options.runs,
            "canonical_searchable_bytes": total_text_bytes,
            "search_physical_bytes": total_search_bytes,
            "search_storage_ratio": aggregate_ratio,
            "worst_query_p95_ms": worst_query_p95.as_secs_f64() * 1000.0,
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
) -> Result<(Duration, BTreeMap<&'static str, f64>), smelt_store::StoreError> {
    let queries = [
        ("one_common", "e"),
        ("one_unicode", "é"),
        ("two_common", "th"),
        ("two_punctuation", "::"),
        ("three_common", "the"),
        ("absent", "smelt-search-absent-sentinel"),
    ];
    let mut worst = Duration::ZERO;
    let mut report = BTreeMap::new();
    for (label, query) in queries {
        let _ = reader.search_transcript_candidate_page(
            query,
            None,
            smelt_store::TranscriptSearchDirection::Forward,
            128,
        )?;
        let mut durations = Vec::with_capacity(runs);
        for _ in 0..runs {
            let started = Instant::now();
            let _ = reader.search_transcript_candidate_page(
                query,
                None,
                smelt_store::TranscriptSearchDirection::Forward,
                128,
            )?;
            durations.push(started.elapsed());
        }
        durations.sort_unstable();
        let p95 = durations[(durations.len() * 95).div_ceil(100).saturating_sub(1)];
        worst = worst.max(p95);
        report.insert(label, p95.as_secs_f64() * 1000.0);
    }
    Ok((worst, report))
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
