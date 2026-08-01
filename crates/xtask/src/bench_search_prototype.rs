use fm_index::{MatchWithLocate, RLFMIndexWithLocate, Search, SearchIndex, Text};
use rusqlite::{params, Connection, OpenFlags, Transaction};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_CORPUS_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_RUNS: usize = 20;
const DOC_OVERLAP_BYTES: usize = 1024;
const QUERY_ANCHOR_BYTES: usize = 512;
const QUERY_ANCHOR_GRAMS: usize = 8;
const CHAR_FILTER_BYTES: usize = 32;
const BIGRAM_FILTER_BYTES: usize = 128;
const TRIGRAM_FILTER_BYTES: usize = 352;
const FILTER_BYTES: usize = CHAR_FILTER_BYTES + BIGRAM_FILTER_BYTES + TRIGRAM_FILTER_BYTES;
const FM_LOCATE_SAMPLE_LEVEL: usize = 6;
const FTS_CANDIDATE_SQL: &str =
    "SELECT d.doc_id, d.segment_id, d.core_start, d.core_end, d.record_end
     FROM search_fts f
     JOIN docs d ON d.doc_id = f.rowid
     WHERE search_fts MATCH ?1
     ORDER BY f.rowid";

#[derive(Clone, Debug)]
struct DriverOptions {
    fixture: Option<PathBuf>,
    output_dir: PathBuf,
    max_corpus_bytes: usize,
    runs: usize,
    doc_sizes: Vec<usize>,
    segment_sizes: Vec<usize>,
    representations: Vec<String>,
}

#[derive(Clone, Debug)]
struct WorkerOptions {
    corpus_dir: PathBuf,
    output_path: PathBuf,
    representation: String,
    doc_size: usize,
    segment_size: usize,
    runs: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DocMeta {
    doc_id: u64,
    segment_id: usize,
    core_start: usize,
    core_end: usize,
    record_end: usize,
}

#[derive(Clone, Debug)]
struct QueryCase {
    class: &'static str,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MatchSummary {
    count: u64,
    digest: u64,
    first: Option<(usize, usize)>,
    second: Option<(usize, usize)>,
}

impl MatchSummary {
    fn push(&mut self, segment_id: usize, offset: usize) {
        if self.first.is_none() {
            self.first = Some((segment_id, offset));
        } else if self.second.is_none() {
            self.second = Some((segment_id, offset));
        }
        self.count = self.count.saturating_add(1);
        let position = ((segment_id as u64) << 32) ^ offset as u64;
        self.digest = self
            .digest
            .wrapping_mul(0x9e37_79b1_85eb_ca87)
            .wrapping_add(position ^ 0xa076_1d64_78bd_642f);
    }
}

#[derive(Clone, Debug)]
struct SearchMeasurement {
    summary: MatchSummary,
    first: Option<Duration>,
    next: Option<Duration>,
    total: Duration,
    candidates: u64,
    verified_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct BuildMeasurement {
    elapsed: Duration,
    allocated_bytes: u64,
    start_live_bytes: u64,
    end_live_bytes: u64,
    peak_live_bytes: u64,
    start_rss_bytes: u64,
    end_rss_bytes: u64,
    peak_rss_bytes: u64,
}

#[derive(Debug)]
struct SegmentCatalog {
    paths: Vec<PathBuf>,
    lengths: Vec<usize>,
    record_ends: Vec<Vec<usize>>,
    logical_bytes: u64,
}

#[derive(Debug)]
struct Verifier<'a> {
    catalog: &'a SegmentCatalog,
    files: HashMap<usize, File>,
    verified_bytes: u64,
}

#[derive(Clone, Copy)]
struct FilterSlices<'a> {
    chars: &'a [u8],
    bigrams: &'a [u8],
    trigrams: &'a [u8],
}

#[global_allocator]
static ALLOCATOR: smelt_perf::alloc::Counting = smelt_perf::alloc::Counting;

pub fn run(args: Vec<String>) {
    if args.first().is_some_and(|arg| arg == "--worker") {
        let options = parse_worker_options(&args[1..]);
        match run_worker(&options) {
            Ok(report) => println!("SEARCH_PROTOTYPE_JSON {report}"),
            Err(err) => {
                eprintln!("bench-search-prototype worker: {err}");
                std::process::exit(1);
            }
        }
        return;
    }

    let options = parse_driver_options(&args);
    if let Err(err) = run_driver(&options) {
        eprintln!("bench-search-prototype: {err}");
        std::process::exit(1);
    }
}

fn parse_driver_options(args: &[String]) -> DriverOptions {
    let mut fixture = None;
    let mut output_dir = None;
    let mut max_corpus_bytes = DEFAULT_CORPUS_BYTES;
    let mut runs = DEFAULT_RUNS;
    let mut doc_sizes = vec![4, 8, 16, 32]
        .into_iter()
        .map(|kib| kib * 1024)
        .collect();
    let mut segment_sizes = vec![1, 2, 4, 8]
        .into_iter()
        .map(|mib| mib * 1024 * 1024)
        .collect();
    let mut representations = vec!["fts".into(), "filters".into(), "fm".into()];

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--fixture" => fixture = Some(required_path(&mut iter, "--fixture")),
            "--output-dir" => output_dir = Some(required_path(&mut iter, "--output-dir")),
            "--max-corpus-bytes" => {
                max_corpus_bytes = required_usize(&mut iter, "--max-corpus-bytes")
            }
            "--runs" => runs = required_usize(&mut iter, "--runs"),
            "--doc-kib" => doc_sizes = required_sizes(&mut iter, "--doc-kib", 1024),
            "--segment-mib" => {
                segment_sizes = required_sizes(&mut iter, "--segment-mib", 1024 * 1024)
            }
            "--representations" => {
                let value = required_value(&mut iter, "--representations");
                representations = value.split(',').map(str::to_string).collect();
                if representations
                    .iter()
                    .any(|value| !matches!(value.as_str(), "fts" | "filters" | "fm"))
                {
                    usage_error("--representations accepts fts,filters,fm");
                }
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => usage_error(&format!("unknown argument `{other}`")),
        }
    }

    let output_dir = output_dir.unwrap_or_else(|| {
        usage_error("--output-dir is required");
    });
    DriverOptions {
        fixture,
        output_dir,
        max_corpus_bytes,
        runs,
        doc_sizes,
        segment_sizes,
        representations,
    }
}

fn parse_worker_options(args: &[String]) -> WorkerOptions {
    let mut corpus_dir = None;
    let mut output_path = None;
    let mut representation = None;
    let mut doc_size = None;
    let mut segment_size = None;
    let mut runs = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--corpus-dir" => corpus_dir = Some(required_path(&mut iter, "--corpus-dir")),
            "--output-path" => output_path = Some(required_path(&mut iter, "--output-path")),
            "--representation" => {
                representation = Some(required_value(&mut iter, "--representation"))
            }
            "--doc-size" => doc_size = Some(required_nonnegative_usize(&mut iter, "--doc-size")),
            "--segment-size" => segment_size = Some(required_usize(&mut iter, "--segment-size")),
            "--runs" => runs = Some(required_usize(&mut iter, "--runs")),
            other => usage_error(&format!("unknown worker argument `{other}`")),
        }
    }
    WorkerOptions {
        corpus_dir: corpus_dir.unwrap_or_else(|| usage_error("worker --corpus-dir is required")),
        output_path: output_path.unwrap_or_else(|| usage_error("worker --output-path is required")),
        representation: representation
            .unwrap_or_else(|| usage_error("worker --representation is required")),
        doc_size: doc_size.unwrap_or_else(|| usage_error("worker --doc-size is required")),
        segment_size: segment_size
            .unwrap_or_else(|| usage_error("worker --segment-size is required")),
        runs: runs.unwrap_or_else(|| usage_error("worker --runs is required")),
    }
}

fn required_value<'a>(iter: &mut impl Iterator<Item = &'a String>, name: &str) -> String {
    iter.next()
        .cloned()
        .unwrap_or_else(|| usage_error(&format!("{name} requires a value")))
}

fn required_path<'a>(iter: &mut impl Iterator<Item = &'a String>, name: &str) -> PathBuf {
    PathBuf::from(required_value(iter, name))
}

fn required_usize<'a>(iter: &mut impl Iterator<Item = &'a String>, name: &str) -> usize {
    let value = required_value(iter, name);
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or_else(|| usage_error(&format!("{name} must be a positive integer")))
}

fn required_nonnegative_usize<'a>(
    iter: &mut impl Iterator<Item = &'a String>,
    name: &str,
) -> usize {
    required_value(iter, name)
        .parse::<usize>()
        .unwrap_or_else(|_| usage_error(&format!("{name} must be a nonnegative integer")))
}

fn required_sizes<'a>(
    iter: &mut impl Iterator<Item = &'a String>,
    name: &str,
    multiplier: usize,
) -> Vec<usize> {
    let value = required_value(iter, name);
    let sizes = value
        .split(',')
        .map(|part| {
            part.parse::<usize>()
                .ok()
                .filter(|value| *value > 0)
                .and_then(|value| value.checked_mul(multiplier))
                .unwrap_or_else(|| usage_error(&format!("{name} requires positive CSV integers")))
        })
        .collect::<Vec<_>>();
    if sizes.is_empty() {
        usage_error(&format!("{name} requires at least one size"));
    }
    sizes
}

fn usage_error(message: &str) -> ! {
    eprintln!("bench-search-prototype: {message}");
    print_usage();
    std::process::exit(2);
}

fn print_usage() {
    eprintln!(
        "usage: cargo xtask bench-search-prototype --output-dir PATH [--fixture PATH] [--max-corpus-bytes N] [--runs N] [--doc-kib 4,8,16,32] [--segment-mib 1,2,4,8] [--representations fts,filters,fm]"
    );
    eprintln!("Without --fixture, the benchmark uses a deterministic synthetic corpus.");
    eprintln!(
        "Fixture databases are opened read-only. Generated artifacts go only under --output-dir."
    );
}

fn run_driver(options: &DriverOptions) -> Result<(), String> {
    validate_paths(options)?;
    fs::create_dir_all(&options.output_dir)
        .map_err(|err| format!("create output directory: {err}"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let run_dir = options
        .output_dir
        .join(format!("run-{}-{nonce}", std::process::id()));
    fs::create_dir(&run_dir).map_err(|err| format!("create run directory: {err}"))?;
    let records_path = run_dir.join("corpus.records");
    let extraction = if let Some(fixture) = &options.fixture {
        extract_fixture_records(fixture, &records_path, options.max_corpus_bytes)?
    } else {
        write_synthetic_records(&records_path, options.max_corpus_bytes)?
    };
    if extraction.logical_bytes == 0 {
        return Err("corpus contains no searchable text".into());
    }

    let mut segment_dirs = HashMap::new();
    for size in &options.segment_sizes {
        let segment_dir = run_dir.join(format!("segments-{size}"));
        let packed = pack_segments(&records_path, &segment_dir, *size)?;
        if packed.logical_bytes != extraction.logical_bytes {
            return Err(format!(
                "segment packing changed logical bytes: {} != {}",
                packed.logical_bytes, extraction.logical_bytes
            ));
        }
        segment_dirs.insert(*size, segment_dir);
    }

    let executable = std::env::current_exe().map_err(|err| format!("locate xtask: {err}"))?;
    let mut reports = Vec::new();
    for representation in &options.representations {
        for segment_size in &options.segment_sizes {
            let doc_sizes: Vec<usize> = if representation == "fm" {
                vec![0]
            } else {
                options.doc_sizes.clone()
            };
            for doc_size in doc_sizes {
                let stem = format!("{representation}-s{segment_size}-d{doc_size}");
                let output_path = run_dir.join(format!("{stem}.db"));
                let output = std::process::Command::new(&executable)
                    .args(["bench-search-prototype", "--worker", "--corpus-dir"])
                    .arg(&segment_dirs[segment_size])
                    .arg("--output-path")
                    .arg(&output_path)
                    .args(["--representation", representation, "--doc-size"])
                    .arg(doc_size.to_string())
                    .arg("--segment-size")
                    .arg(segment_size.to_string())
                    .arg("--runs")
                    .arg(options.runs.to_string())
                    .output()
                    .map_err(|err| format!("run {stem} worker: {err}"))?;
                if !output.status.success() {
                    return Err(format!(
                        "{stem} worker failed:\n{}\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let stdout = String::from_utf8(output.stdout)
                    .map_err(|err| format!("decode {stem} output: {err}"))?;
                let report = stdout
                    .lines()
                    .find_map(|line| line.strip_prefix("SEARCH_PROTOTYPE_JSON "))
                    .ok_or_else(|| format!("{stem} worker produced no JSON report"))?;
                let value: Value = serde_json::from_str(report)
                    .map_err(|err| format!("parse {stem} report: {err}"))?;
                println!("SEARCH_PROTOTYPE_JSON {value}");
                reports.push(value);
            }
        }
    }

    let passing = reports
        .iter()
        .filter(|report| report.get("passes").and_then(Value::as_bool) == Some(true))
        .count();
    let summary = json!({
        "type": "search_prototype_summary",
        "source": if options.fixture.is_some() { "fixture" } else { "synthetic" },
        "logical_bytes": extraction.logical_bytes,
        "records": extraction.records,
        "configurations": reports.len(),
        "passing_configurations": passing,
        "run_artifact_name": run_dir.file_name().and_then(|name| name.to_str()),
    });
    let report = json!({
        "summary": &summary,
        "configurations": &reports,
    });
    let report_bytes = serde_json::to_vec_pretty(&report)
        .map_err(|err| format!("encode benchmark report: {err}"))?;
    fs::write(run_dir.join("report.json"), report_bytes)
        .map_err(|err| format!("write benchmark report: {err}"))?;
    println!("SEARCH_PROTOTYPE_SUMMARY_JSON {summary}");
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct CorpusStats {
    records: u64,
    logical_bytes: u64,
}

fn validate_paths(options: &DriverOptions) -> Result<(), String> {
    if let Some(fixture) = &options.fixture {
        let fixture = fixture
            .canonicalize()
            .map_err(|err| format!("resolve fixture path: {err}"))?;
        let output = absolute_path(&options.output_dir)?;
        if output.starts_with(&fixture) || fixture.starts_with(&output) {
            return Err("--output-dir and --fixture must be disjoint".into());
        }
        if let Some(real_sessions) = dirs_sessions_path() {
            if fixture.starts_with(real_sessions) {
                return Err(
                    "refusing to benchmark directly against the live session directory".into(),
                );
            }
        }
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|err| format!("resolve current directory: {err}"))
    }
}

fn dirs_sessions_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("smelt")
            .join("sessions"),
    )
}

fn collect_session_databases(path: &Path) -> Result<Vec<PathBuf>, String> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let direct = path.join("session.db");
    if direct.is_file() {
        return Ok(vec![direct]);
    }
    let mut databases = Vec::new();
    let entries = fs::read_dir(path).map_err(|err| format!("read fixture directory: {err}"))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("read fixture entry: {err}"))?;
        let candidate = entry.path().join("session.db");
        if candidate.is_file() {
            databases.push(candidate);
        }
    }
    databases.sort();
    if databases.is_empty() {
        return Err("fixture contains no session.db files".into());
    }
    Ok(databases)
}

fn extract_fixture_records(
    fixture: &Path,
    destination: &Path,
    max_bytes: usize,
) -> Result<CorpusStats, String> {
    let databases = collect_session_databases(fixture)?;
    let file = File::create(destination).map_err(|err| format!("create corpus: {err}"))?;
    let mut writer = BufWriter::new(file);
    let mut stats = CorpusStats {
        records: 0,
        logical_bytes: 0,
    };
    for database in databases {
        if stats.logical_bytes as usize >= max_bytes {
            break;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(&database, flags)
            .map_err(|err| format!("open fixture database read-only: {err}"))?;
        conn.pragma_update(None, "query_only", true)
            .map_err(|err| format!("make fixture connection query-only: {err}"))?;
        let mut stmt = conn
            .prepare("SELECT indexed_text FROM transcript_search ORDER BY block_idx")
            .map_err(|err| format!("prepare fixture transcript query: {err}"))?;
        let mut rows = stmt
            .query([])
            .map_err(|err| format!("query fixture transcript: {err}"))?;
        while let Some(row) = rows
            .next()
            .map_err(|err| format!("read fixture transcript row: {err}"))?
        {
            let text: String = row
                .get(0)
                .map_err(|err| format!("decode fixture transcript text: {err}"))?;
            let remaining = max_bytes.saturating_sub(stats.logical_bytes as usize);
            if remaining == 0 {
                break;
            }
            let end = floor_char_boundary(&text, remaining.min(text.len()));
            if end == 0 {
                continue;
            }
            write_record(&mut writer, &text.as_bytes()[..end])?;
            stats.records += 1;
            stats.logical_bytes += end as u64;
            if end < text.len() {
                break;
            }
        }
    }
    writer
        .flush()
        .map_err(|err| format!("flush corpus: {err}"))?;
    Ok(stats)
}

fn write_synthetic_records(destination: &Path, max_bytes: usize) -> Result<CorpusStats, String> {
    let file = File::create(destination).map_err(|err| format!("create corpus: {err}"))?;
    let mut writer = BufWriter::new(file);
    let mut stats = CorpusStats {
        records: 0,
        logical_bytes: 0,
    };
    let long_line = "persistent sequence search boundary exact verification ".repeat(96);
    let mut index = 0u64;
    while (stats.logical_bytes as usize) < max_bytes {
        let text = format!(
            "record {index}: the function keeps immutable revisions searchable; café λ::search foo_bar% literal punctuation.\n{long_line}\nrare-marker-{index:016x} end",
        );
        let remaining = max_bytes.saturating_sub(stats.logical_bytes as usize);
        let end = floor_char_boundary(&text, remaining.min(text.len()));
        if end == 0 {
            break;
        }
        write_record(&mut writer, &text.as_bytes()[..end])?;
        stats.records += 1;
        stats.logical_bytes += end as u64;
        index += 1;
    }
    writer
        .flush()
        .map_err(|err| format!("flush corpus: {err}"))?;
    Ok(stats)
}

fn write_record(writer: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    writer
        .write_all(&(bytes.len() as u64).to_le_bytes())
        .and_then(|()| writer.write_all(bytes))
        .map_err(|err| format!("write corpus record: {err}"))
}

fn read_record(reader: &mut impl Read) -> Result<Option<Vec<u8>>, String> {
    let mut length = [0u8; 8];
    let first_byte_count = loop {
        match reader.read(&mut length[..1]) {
            Ok(count) => break count,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(format!("read corpus record length: {err}")),
        }
    };
    if first_byte_count == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut length[1..])
        .map_err(|err| format!("read truncated corpus record length: {err}"))?;
    let length = usize::try_from(u64::from_le_bytes(length))
        .map_err(|_| "corpus record is too large for this platform".to_string())?;
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|err| format!("read corpus record: {err}"))?;
    Ok(Some(bytes))
}

fn pack_segments(
    records_path: &Path,
    output_dir: &Path,
    target: usize,
) -> Result<CorpusStats, String> {
    fs::create_dir(output_dir).map_err(|err| format!("create segment directory: {err}"))?;
    let file = File::open(records_path).map_err(|err| format!("open corpus records: {err}"))?;
    let mut reader = BufReader::new(file);
    let mut segment = Vec::with_capacity(target);
    let mut record_ends = Vec::new();
    let mut segment_index = 0usize;
    let mut stats = CorpusStats {
        records: 0,
        logical_bytes: 0,
    };
    while let Some(record) = read_record(&mut reader)? {
        if record.is_empty() {
            continue;
        }
        if !segment.is_empty() && segment.len().saturating_add(record.len()) > target {
            write_segment(output_dir, segment_index, &segment, &record_ends)?;
            segment_index += 1;
            segment.clear();
            record_ends.clear();
        }
        segment.extend_from_slice(&record);
        record_ends.push(segment.len());
        stats.records += 1;
        stats.logical_bytes += record.len() as u64;
    }
    if !segment.is_empty() {
        write_segment(output_dir, segment_index, &segment, &record_ends)?;
    }
    Ok(stats)
}

fn write_segment(
    directory: &Path,
    index: usize,
    bytes: &[u8],
    record_ends: &[usize],
) -> Result<(), String> {
    let path = directory.join(format!("segment-{index:06}.txt"));
    fs::write(&path, bytes).map_err(|err| format!("write canonical segment: {err}"))?;
    let mut metadata = Vec::with_capacity(record_ends.len().saturating_mul(8));
    for end in record_ends {
        let end =
            u64::try_from(*end).map_err(|_| "canonical record offset exceeds u64".to_string())?;
        metadata.extend_from_slice(&end.to_le_bytes());
    }
    fs::write(path.with_extension("records"), metadata)
        .map_err(|err| format!("write canonical record metadata: {err}"))
}

fn load_segment_catalog(directory: &Path) -> Result<SegmentCatalog, String> {
    let mut paths = fs::read_dir(directory)
        .map_err(|err| format!("read segment directory: {err}"))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|err| format!("read segment entry: {err}"))?;
    paths.retain(|path| path.extension().and_then(|ext| ext.to_str()) == Some("txt"));
    paths.sort();
    let mut lengths = Vec::with_capacity(paths.len());
    let mut record_ends = Vec::with_capacity(paths.len());
    let mut logical_bytes = 0u64;
    for path in &paths {
        let length = usize::try_from(
            fs::metadata(path)
                .map_err(|err| format!("stat canonical segment: {err}"))?
                .len(),
        )
        .map_err(|_| "canonical segment is too large for this platform".to_string())?;
        let segment_record_ends = load_record_ends(path, length)?;
        lengths.push(length);
        record_ends.push(segment_record_ends);
        logical_bytes = logical_bytes.saturating_add(length as u64);
    }
    if paths.is_empty() {
        return Err("canonical segment directory is empty".into());
    }
    Ok(SegmentCatalog {
        paths,
        lengths,
        record_ends,
        logical_bytes,
    })
}

fn load_record_ends(segment_path: &Path, segment_len: usize) -> Result<Vec<usize>, String> {
    let metadata_path = segment_path.with_extension("records");
    let metadata =
        fs::read(&metadata_path).map_err(|err| format!("read canonical record metadata: {err}"))?;
    if metadata.is_empty() || metadata.len() % 8 != 0 {
        return Err(format!(
            "canonical record metadata is invalid: {}",
            metadata_path.display()
        ));
    }
    let mut record_ends = Vec::with_capacity(metadata.len() / 8);
    let mut previous = 0usize;
    for encoded in metadata.chunks_exact(8) {
        let end = usize::try_from(u64::from_le_bytes(encoded.try_into().unwrap()))
            .map_err(|_| "canonical record offset is too large for this platform".to_string())?;
        if end <= previous || end > segment_len {
            return Err(format!(
                "canonical record metadata is out of bounds: {}",
                metadata_path.display()
            ));
        }
        record_ends.push(end);
        previous = end;
    }
    if previous != segment_len {
        return Err(format!(
            "canonical record metadata does not cover its segment: {}",
            metadata_path.display()
        ));
    }
    Ok(record_ends)
}

fn run_worker(options: &WorkerOptions) -> Result<Value, String> {
    if options.representation != "fm" && options.doc_size == 0 {
        return Err("SQLite search documents must have a positive size".into());
    }
    smelt_perf::alloc::enable();
    let catalog = load_segment_catalog(&options.corpus_dir)?;
    match options.representation.as_str() {
        "fts" => run_sqlite_worker(options, &catalog, true),
        "filters" => run_sqlite_worker(options, &catalog, false),
        "fm" => run_fm_worker(options, &catalog),
        other => Err(format!("unknown representation `{other}`")),
    }
}

fn run_sqlite_worker(
    options: &WorkerOptions,
    catalog: &SegmentCatalog,
    fts: bool,
) -> Result<Value, String> {
    if options.output_path.exists() {
        fs::remove_file(&options.output_path)
            .map_err(|err| format!("remove stale projection: {err}"))?;
    }
    let build = measure_build(|| {
        if fts {
            build_fts_projection(&options.output_path, catalog, options.doc_size)
        } else {
            build_filter_projection(&options.output_path, catalog, options.doc_size)
        }
    })?;
    let queries = build_query_cases(catalog, options.doc_size.max(4096))?;
    let physical_bytes = sqlite_physical_bytes(&options.output_path)?;
    let storage_ratio = physical_bytes as f64 / catalog.logical_bytes.max(1) as f64;
    let mut query_reports = Vec::new();
    let mut exact = true;
    let mut latency_pass = true;
    let mut cold_cache_attempts = 0usize;
    let mut cold_cache_successes = 0usize;

    for query in &queries {
        let oracle = direct_scan(catalog, &query.bytes)?;
        let differential =
            run_sqlite_search(&options.output_path, catalog, query, fts, None, false)?;
        if differential.summary != oracle {
            exact = false;
        }

        let mut cold_attempt_success = Vec::with_capacity(options.runs);
        let mut cold_first = Vec::with_capacity(options.runs);
        for _ in 0..options.runs {
            let cache_dropped = drop_sqlite_search_cache(&options.output_path, catalog);
            cold_attempt_success.push(cache_dropped);
            cold_cache_attempts += 1;
            if !cache_dropped {
                continue;
            }
            cold_cache_successes += 1;
            let sample =
                run_sqlite_search(&options.output_path, catalog, query, fts, Some(2), true)?;
            cold_first.push(duration_ms(sample.first.unwrap_or(sample.total)));
        }

        let mut warm_first = Vec::with_capacity(options.runs);
        let mut next = Vec::new();
        let mut bounded_candidates = None;
        let mut bounded_verified_bytes = None;
        for _ in 0..options.runs {
            let sample =
                run_sqlite_search(&options.output_path, catalog, query, fts, Some(2), false)?;
            bounded_candidates.get_or_insert(sample.candidates);
            bounded_verified_bytes.get_or_insert(sample.verified_bytes);
            warm_first.push(duration_ms(sample.first.unwrap_or(sample.total)));
            if let Some(elapsed) = sample.next {
                next.push(duration_ms(elapsed));
            }
        }

        let mut full = Vec::with_capacity(options.runs);
        for _ in 0..options.runs {
            let sample = run_sqlite_search(&options.output_path, catalog, query, fts, None, false)?;
            full.push(duration_ms(sample.total));
        }
        let cold_p95 = (!cold_first.is_empty()).then(|| percentile(&cold_first, 95));
        let warm_p95 = percentile(&warm_first, 95);
        let next_p95 = (!next.is_empty()).then(|| percentile(&next, 95));
        let full_p95 = percentile(&full, 95);
        let absent = oracle.count == 0;
        let cold_gate_pass =
            cold_first.len() == options.runs && cold_p95.is_some_and(|value| value <= 100.0);
        let query_pass = if absent {
            cold_gate_pass && warm_p95 <= 100.0 && full_p95 <= 100.0
        } else {
            cold_gate_pass && warm_p95 <= 50.0 && next_p95.is_none_or(|value| value <= 50.0)
        };
        latency_pass &= query_pass;
        query_reports.push(json!({
            "class": query.class,
            "query_bytes": query.bytes.len(),
            "matches": oracle.count,
            "candidates": differential.candidates,
            "verified_bytes": differential.verified_bytes,
            "bounded_candidates": bounded_candidates,
            "bounded_verified_bytes": bounded_verified_bytes,
            "full_enumeration_candidates": differential.candidates,
            "full_enumeration_verified_bytes": differential.verified_bytes,
            "cold_cache_attempt_success": cold_attempt_success,
            "cold_cache_gate_pass": cold_gate_pass,
            "cold_first_samples": cold_first.len(),
            "cold_first_p95_ms": cold_p95,
            "warm_first_samples": warm_first.len(),
            "warm_first_p95_ms": warm_p95,
            "next_samples": next.len(),
            "next_p95_ms": next_p95,
            "full_enumeration_samples": full.len(),
            "full_enumeration_p95_ms": full_p95,
            "full_enumeration_in_latency_gate": absent,
            "exact": differential.summary == oracle,
            "latency_pass": query_pass,
        }));
    }

    let storage_pass = storage_ratio <= 0.25;
    let passes = exact && storage_pass && latency_pass;
    Ok(json!({
        "type": "search_prototype_configuration",
        "representation": if fts { "compact_fts" } else { "chunk_filters" },
        "doc_bytes": options.doc_size,
        "segment_bytes": options.segment_size,
        "logical_bytes": catalog.logical_bytes,
        "physical_bytes": physical_bytes,
        "storage_ratio": storage_ratio,
        "storage_target_pass": storage_ratio <= 0.15,
        "storage_hard_pass": storage_pass,
        "build_ms": duration_ms(build.elapsed),
        "build_mib_per_s": throughput_mib(catalog.logical_bytes, build.elapsed),
        "build_allocated_bytes": build.allocated_bytes,
        "build_start_live_bytes": build.start_live_bytes,
        "build_end_live_bytes": build.end_live_bytes,
        "build_peak_live_bytes": build.peak_live_bytes,
        "build_peak_live_incremental_bytes": build.peak_live_bytes.saturating_sub(build.start_live_bytes),
        "build_live_bytes_method": "phase_local_global_allocator",
        "build_start_rss_bytes": build.start_rss_bytes,
        "build_end_rss_bytes": build.end_rss_bytes,
        "process_peak_rss_bytes": build.peak_rss_bytes,
        "process_peak_rss_above_build_start_bytes": build.peak_rss_bytes.saturating_sub(build.start_rss_bytes),
        "rss_peak_scope": "process_lifetime_high_water_not_phase_local",
        "cold_cache_method": cold_cache_method(),
        "cold_cache_method_supported": cold_cache_method().is_some(),
        "cold_cache_all_attempts_succeeded": cold_cache_attempts > 0
            && cold_cache_successes == cold_cache_attempts,
        "cold_cache_attempts": cold_cache_attempts,
        "cold_cache_successes": cold_cache_successes,
        "requested_samples_per_query": options.runs,
        "exact": exact,
        "latency_pass": latency_pass,
        "passes": passes,
        "queries": query_reports,
    }))
}

fn measure_build<T>(
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<BuildMeasurement, String> {
    let start_alloc = smelt_perf::alloc::snapshot();
    let start_rss_bytes = linux_memory_bytes("VmRSS:").unwrap_or(0);
    let peak_measurement = smelt_perf::alloc::begin_peak_measurement();
    let start_live_bytes = peak_measurement.start_bytes() as u64;
    let started = Instant::now();
    let result = operation();
    let elapsed = started.elapsed();
    let peak_live_bytes = peak_measurement.finish() as u64;
    let end_alloc = smelt_perf::alloc::snapshot();
    let end_rss_bytes = linux_memory_bytes("VmRSS:").unwrap_or(0);
    let peak_rss_bytes = linux_memory_bytes("VmHWM:").unwrap_or(0);
    let delta = smelt_perf::alloc::delta(start_alloc, end_alloc);
    result?;
    Ok(BuildMeasurement {
        elapsed,
        allocated_bytes: delta.bytes_allocated,
        start_live_bytes,
        end_live_bytes: end_alloc.current_bytes as u64,
        peak_live_bytes,
        start_rss_bytes,
        end_rss_bytes,
        peak_rss_bytes,
    })
}

fn sqlite_storage_path(path: &Path, suffix: &str) -> PathBuf {
    let mut candidate = path.as_os_str().to_os_string();
    candidate.push(suffix);
    PathBuf::from(candidate)
}

fn sqlite_storage_paths(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::with_capacity(4);
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let candidate = sqlite_storage_path(path, suffix);
        match fs::metadata(&candidate) {
            Ok(_) => paths.push(candidate),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && !suffix.is_empty() => {}
            Err(err) => return Err(format!("stat projection storage: {err}")),
        }
    }
    Ok(paths)
}

fn sqlite_physical_bytes(path: &Path) -> Result<u64, String> {
    sqlite_storage_paths(path)?
        .into_iter()
        .try_fold(0u64, |total, path| -> Result<u64, String> {
            let bytes = fs::metadata(path)
                .map_err(|err| format!("stat projection storage: {err}"))?
                .len();
            Ok(total.saturating_add(bytes))
        })
}

fn configure_projection(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "PRAGMA page_size = 4096;
         PRAGMA journal_mode = OFF;
         PRAGMA synchronous = OFF;
         PRAGMA temp_store = MEMORY;
         PRAGMA locking_mode = EXCLUSIVE;",
    )
    .map_err(|err| format!("configure projection database: {err}"))
}

fn create_docs_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE docs (
             doc_id INTEGER PRIMARY KEY,
             segment_id INTEGER NOT NULL,
             core_start INTEGER NOT NULL,
             core_end INTEGER NOT NULL,
             record_end INTEGER NOT NULL
         );",
    )
    .map_err(|err| format!("create projection document schema: {err}"))
}

fn build_fts_projection(
    path: &Path,
    catalog: &SegmentCatalog,
    doc_size: usize,
) -> Result<(), String> {
    let mut conn = Connection::open(path).map_err(|err| format!("create FTS projection: {err}"))?;
    configure_projection(&conn)?;
    create_docs_schema(&conn)?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE search_fts USING fts5(
             text,
             content='',
             detail=none,
             columnsize=0,
             tokenize='trigram'
         );
         CREATE TABLE short_postings (
             segment_id INTEGER NOT NULL,
             kind INTEGER NOT NULL,
             gram_hash INTEGER NOT NULL,
             docs BLOB NOT NULL,
             PRIMARY KEY (kind, gram_hash, segment_id)
         ) WITHOUT ROWID;",
    )
    .map_err(|err| format!("create compact FTS schema: {err}"))?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("begin compact FTS build: {err}"))?;
    let mut next_doc_id = 0u64;
    for (segment_id, path) in catalog.paths.iter().enumerate() {
        let text = fs::read(path).map_err(|err| format!("read canonical segment: {err}"))?;
        let docs = segment_docs(
            segment_id,
            &text,
            &catalog.record_ends[segment_id],
            doc_size,
            &mut next_doc_id,
        )?;
        insert_fts_segment(&tx, segment_id, &text, &docs)?;
    }
    tx.commit()
        .map_err(|err| format!("commit compact FTS build: {err}"))?;
    conn.execute_batch(
        "INSERT INTO search_fts(search_fts) VALUES('optimize');
         VACUUM;",
    )
    .map_err(|err| format!("compact FTS projection: {err}"))?;
    Ok(())
}

fn insert_fts_segment(
    tx: &Transaction<'_>,
    segment_id: usize,
    text: &[u8],
    docs: &[DocMeta],
) -> Result<(), String> {
    let mut postings: HashMap<(u8, u64), Vec<u64>> = HashMap::new();
    let mut insert_doc = tx
        .prepare(
            "INSERT INTO docs(doc_id, segment_id, core_start, core_end, record_end)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(|err| format!("prepare FTS document insert: {err}"))?;
    let mut insert_fts = tx
        .prepare("INSERT INTO search_fts(rowid, text) VALUES (?1, ?2)")
        .map_err(|err| format!("prepare FTS text insert: {err}"))?;
    for doc in docs {
        let doc_id = sql_integer(doc.doc_id, "document ID")?;
        let sql_segment_id = sql_integer(doc.segment_id, "segment ID")?;
        let core_start = sql_integer(doc.core_start, "document start")?;
        let core_end = sql_integer(doc.core_end, "document end")?;
        let record_end = sql_integer(doc.record_end, "record end")?;
        insert_doc
            .execute(params![
                doc_id,
                sql_segment_id,
                core_start,
                core_end,
                record_end
            ])
            .map_err(|err| format!("insert FTS document: {err}"))?;
        let extended_end = doc
            .core_end
            .saturating_add(DOC_OVERLAP_BYTES)
            .min(doc.record_end)
            .min(text.len());
        let extended_end = floor_utf8_boundary(text, extended_end);
        let indexed = std::str::from_utf8(&text[doc.core_start..extended_end])
            .map_err(|err| format!("canonical segment is not UTF-8: {err}"))?;
        insert_fts
            .execute(params![doc_id, indexed])
            .map_err(|err| format!("insert compact FTS text: {err}"))?;
        let mut chars = HashSet::new();
        let mut bigrams = HashSet::new();
        collect_short_hashes(indexed, &mut chars, &mut bigrams);
        for hash in chars {
            postings.entry((1, hash)).or_default().push(doc.doc_id);
        }
        for hash in bigrams {
            postings.entry((2, hash)).or_default().push(doc.doc_id);
        }
    }
    let mut insert_postings = tx
        .prepare(
            "INSERT INTO short_postings(segment_id, kind, gram_hash, docs)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .map_err(|err| format!("prepare short posting insert: {err}"))?;
    let sql_segment_id = sql_integer(segment_id, "segment ID")?;
    for ((kind, hash), doc_ids) in postings {
        let packed = pack_doc_ids(&doc_ids);
        insert_postings
            .execute(params![sql_segment_id, kind, hash as i64, packed])
            .map_err(|err| format!("insert short postings: {err}"))?;
    }
    Ok(())
}

fn build_filter_projection(
    path: &Path,
    catalog: &SegmentCatalog,
    doc_size: usize,
) -> Result<(), String> {
    let mut conn =
        Connection::open(path).map_err(|err| format!("create filter projection: {err}"))?;
    configure_projection(&conn)?;
    conn.execute_batch(
        "CREATE TABLE filter_docs (
             doc_id INTEGER PRIMARY KEY,
             segment_id INTEGER NOT NULL,
             core_start INTEGER NOT NULL,
             core_end INTEGER NOT NULL,
             record_end INTEGER NOT NULL,
             filter BLOB NOT NULL
         );",
    )
    .map_err(|err| format!("create chunk filter schema: {err}"))?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("begin chunk filter build: {err}"))?;
    let mut insert = tx
        .prepare(
            "INSERT INTO filter_docs(
                 doc_id, segment_id, core_start, core_end, record_end, filter
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .map_err(|err| format!("prepare chunk filter insert: {err}"))?;
    let mut next_doc_id = 0u64;
    for (segment_id, path) in catalog.paths.iter().enumerate() {
        let text = fs::read(path).map_err(|err| format!("read canonical segment: {err}"))?;
        let docs = segment_docs(
            segment_id,
            &text,
            &catalog.record_ends[segment_id],
            doc_size,
            &mut next_doc_id,
        )?;
        for doc in docs {
            let extended_end = doc
                .core_end
                .saturating_add(DOC_OVERLAP_BYTES)
                .min(doc.record_end)
                .min(text.len());
            let extended_end = floor_utf8_boundary(&text, extended_end);
            let indexed = std::str::from_utf8(&text[doc.core_start..extended_end])
                .map_err(|err| format!("canonical segment is not UTF-8: {err}"))?;
            let filter = build_filter(indexed);
            let doc_id = sql_integer(doc.doc_id, "document ID")?;
            let sql_segment_id = sql_integer(doc.segment_id, "segment ID")?;
            let core_start = sql_integer(doc.core_start, "document start")?;
            let core_end = sql_integer(doc.core_end, "document end")?;
            let record_end = sql_integer(doc.record_end, "record end")?;
            insert
                .execute(params![
                    doc_id,
                    sql_segment_id,
                    core_start,
                    core_end,
                    record_end,
                    filter
                ])
                .map_err(|err| format!("insert chunk filter: {err}"))?;
        }
    }
    drop(insert);
    tx.commit()
        .map_err(|err| format!("commit chunk filter build: {err}"))?;
    conn.execute_batch("VACUUM")
        .map_err(|err| format!("compact chunk filter projection: {err}"))?;
    Ok(())
}

fn segment_docs(
    segment_id: usize,
    text: &[u8],
    record_ends: &[usize],
    doc_size: usize,
    next_doc_id: &mut u64,
) -> Result<Vec<DocMeta>, String> {
    if doc_size == 0 {
        return Err("search document size must be nonzero".into());
    }
    std::str::from_utf8(text).map_err(|err| format!("canonical segment is not UTF-8: {err}"))?;
    let mut previous_record_end = 0usize;
    for &record_end in record_ends {
        if record_end <= previous_record_end
            || record_end > text.len()
            || floor_utf8_boundary(text, record_end) != record_end
        {
            return Err("canonical record boundaries are invalid".into());
        }
        previous_record_end = record_end;
    }
    if record_ends.is_empty() || previous_record_end != text.len() {
        return Err("canonical record boundaries do not cover the segment".into());
    }

    let mut docs = Vec::new();
    let mut record_start = 0usize;
    for &record_end in record_ends {
        let mut start = record_start;
        while start < record_end {
            let desired = start.saturating_add(doc_size).min(record_end);
            let mut end = floor_utf8_boundary(text, desired);
            if end <= start {
                end = next_utf8_boundary(text, desired).min(record_end);
            }
            if end <= start || end > record_end {
                return Err("search document boundary did not make progress".into());
            }
            docs.push(DocMeta {
                doc_id: *next_doc_id,
                segment_id,
                core_start: start,
                core_end: end,
                record_end,
            });
            *next_doc_id = next_doc_id
                .checked_add(1)
                .ok_or_else(|| "search document ID overflow".to_string())?;
            start = end;
        }
        record_start = record_end;
    }
    Ok(docs)
}

fn collect_short_hashes(text: &str, chars_out: &mut HashSet<u64>, bigrams_out: &mut HashSet<u64>) {
    let mut previous = None;
    for ch in text.chars() {
        chars_out.insert(hash_scalars(1, &[ch as u32]));
        if let Some(left) = previous {
            bigrams_out.insert(hash_scalars(2, &[left, ch as u32]));
        }
        previous = Some(ch as u32);
    }
}

fn pack_doc_ids(doc_ids: &[u64]) -> Vec<u8> {
    let mut packed = Vec::with_capacity(doc_ids.len());
    let mut previous = 0u64;
    for (index, doc_id) in doc_ids.iter().copied().enumerate() {
        let delta = if index == 0 {
            doc_id
        } else {
            doc_id.saturating_sub(previous)
        };
        write_varint(delta, &mut packed);
        previous = doc_id;
    }
    packed
}

fn unpack_doc_ids(packed: &[u8], output: &mut Vec<u64>) -> Result<(), String> {
    let mut offset = 0usize;
    let mut previous: Option<u64> = None;
    while offset < packed.len() {
        let delta = read_varint(packed, &mut offset)?;
        if previous.is_some() && delta == 0 {
            return Err("short posting document IDs are not strictly increasing".into());
        }
        let doc_id = match previous {
            None => delta,
            Some(previous) => previous
                .checked_add(delta)
                .ok_or_else(|| "short posting document delta overflow".to_string())?,
        };
        output.push(doc_id);
        previous = Some(doc_id);
    }
    Ok(())
}

fn write_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn read_varint(bytes: &[u8], offset: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes
            .get(*offset)
            .ok_or_else(|| "truncated short posting varint".to_string())?;
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= 64 {
            return Err("short posting varint overflow".into());
        }
    }
}

fn build_filter(text: &str) -> Vec<u8> {
    let mut filter = vec![0u8; FILTER_BYTES];
    let mut scalars = [0u32; 3];
    let mut count = 0usize;
    for ch in text.chars() {
        scalars.rotate_left(1);
        scalars[2] = ch as u32;
        count += 1;
        set_filter_bit(
            &mut filter[..CHAR_FILTER_BYTES],
            hash_scalars(1, &scalars[2..]),
        );
        if count >= 2 {
            set_filter_bit(
                &mut filter[CHAR_FILTER_BYTES..CHAR_FILTER_BYTES + BIGRAM_FILTER_BYTES],
                hash_scalars(2, &scalars[1..]),
            );
        }
        if count >= 3 {
            set_filter_bit(
                &mut filter[CHAR_FILTER_BYTES + BIGRAM_FILTER_BYTES..],
                hash_scalars(3, &scalars),
            );
        }
    }
    filter
}

fn filter_slices(filter: &[u8]) -> Option<FilterSlices<'_>> {
    (filter.len() == FILTER_BYTES).then(|| FilterSlices {
        chars: &filter[..CHAR_FILTER_BYTES],
        bigrams: &filter[CHAR_FILTER_BYTES..CHAR_FILTER_BYTES + BIGRAM_FILTER_BYTES],
        trigrams: &filter[CHAR_FILTER_BYTES + BIGRAM_FILTER_BYTES..],
    })
}

fn set_filter_bit(filter: &mut [u8], hash: u64) {
    let bit = (hash as usize) % (filter.len() * 8);
    filter[bit / 8] |= 1 << (bit % 8);
}

fn filter_has_bit(filter: &[u8], hash: u64) -> bool {
    let bit = (hash as usize) % (filter.len() * 8);
    filter[bit / 8] & (1 << (bit % 8)) != 0
}

fn filter_matches(filter: &[u8], query: &str) -> Result<bool, String> {
    let filters = filter_slices(filter).ok_or_else(|| "invalid chunk filter length".to_string())?;
    let chars = query.chars().map(|ch| ch as u32).collect::<Vec<_>>();
    Ok(match chars.len() {
        0 => false,
        1 => filter_has_bit(filters.chars, hash_scalars(1, &chars)),
        2 => filter_has_bit(filters.bigrams, hash_scalars(2, &chars)),
        _ => query_anchor_grams(query)
            .into_iter()
            .all(|gram| filter_has_bit(filters.trigrams, hash_scalars(3, &gram))),
    })
}

fn hash_scalars(kind: u8, scalars: &[u32]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ u64::from(kind);
    for scalar in scalars {
        for byte in scalar.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    hash
}

fn run_sqlite_search(
    projection: &Path,
    catalog: &SegmentCatalog,
    query: &QueryCase,
    fts: bool,
    limit_matches: Option<u64>,
    cold_open: bool,
) -> Result<SearchMeasurement, String> {
    let started = Instant::now();
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(projection, flags)
        .map_err(|err| format!("open search projection: {err}"))?;
    conn.pragma_update(None, "query_only", true)
        .and_then(|()| conn.pragma_update(None, "mmap_size", 0))
        .and_then(|()| conn.pragma_update(None, "cache_size", -2048))
        .map_err(|err| format!("configure search projection reader: {err}"))?;
    if cold_open {
        conn.execute_batch("PRAGMA shrink_memory")
            .map_err(|err| format!("shrink cold search cache: {err}"))?;
    }
    let mut verifier = Verifier::new(catalog);
    let mut summary = MatchSummary::default();
    let mut first = None;
    let mut next = None;
    let candidate_count = {
        let mut visit = |doc| {
            for offset in verifier.matches(doc, &query.bytes)? {
                summary.push(doc.segment_id, offset);
                if summary.count == 1 {
                    first = Some(started.elapsed());
                } else if summary.count == 2 {
                    let first_at = first.unwrap_or_default();
                    next = Some(started.elapsed().saturating_sub(first_at));
                }
                if limit_matches.is_some_and(|limit| summary.count >= limit) {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        if fts {
            visit_fts_candidates(&conn, query, &mut visit)?
        } else {
            visit_filter_candidates(&conn, query, &mut visit)?
        }
    };
    Ok(SearchMeasurement {
        summary,
        first,
        next,
        total: started.elapsed(),
        candidates: candidate_count,
        verified_bytes: verifier.verified_bytes,
    })
}

fn visit_fts_candidates<F>(
    conn: &Connection,
    query: &QueryCase,
    visit: &mut F,
) -> Result<u64, String>
where
    F: FnMut(DocMeta) -> Result<bool, String>,
{
    let text = std::str::from_utf8(&query.bytes)
        .map_err(|err| format!("search query is not UTF-8: {err}"))?;
    let char_count = text.chars().count();
    if char_count < 3 {
        return visit_short_posting_candidates(conn, text, visit);
    }
    let expression = fts_anchor_expression(text);
    let mut stmt = conn
        .prepare(FTS_CANDIDATE_SQL)
        .map_err(|err| format!("prepare compact FTS candidate query: {err}"))?;
    let mut rows = stmt
        .query([expression])
        .map_err(|err| format!("query compact FTS candidates: {err}"))?;
    let mut candidate_count = 0_u64;
    while let Some(row) = rows
        .next()
        .map_err(|err| format!("read compact FTS candidate: {err}"))?
    {
        let doc =
            row_doc_meta(row).map_err(|err| format!("decode compact FTS candidate: {err}"))?;
        candidate_count = candidate_count.saturating_add(1);
        if !visit(doc)? {
            break;
        }
    }
    Ok(candidate_count)
}

fn visit_short_posting_candidates<F>(
    conn: &Connection,
    query: &str,
    visit: &mut F,
) -> Result<u64, String>
where
    F: FnMut(DocMeta) -> Result<bool, String>,
{
    let scalars = query.chars().map(|ch| ch as u32).collect::<Vec<_>>();
    let kind = scalars.len() as u8;
    let hash = hash_scalars(kind, &scalars);
    let mut posting_stmt = conn
        .prepare(
            "SELECT segment_id, docs FROM short_postings
             WHERE kind = ?1 AND gram_hash = ?2
             ORDER BY segment_id",
        )
        .map_err(|err| format!("prepare short posting query: {err}"))?;
    let mut posting_rows = posting_stmt
        .query(params![kind, hash as i64])
        .map_err(|err| format!("query short postings: {err}"))?;
    let mut docs_stmt = conn
        .prepare(
            "SELECT doc_id, segment_id, core_start, core_end, record_end
             FROM docs WHERE doc_id BETWEEN ?1 AND ?2 ORDER BY doc_id",
        )
        .map_err(|err| format!("prepare short posting document scan: {err}"))?;
    let mut candidate_count = 0_u64;
    while let Some(posting_row) = posting_rows
        .next()
        .map_err(|err| format!("read short posting row: {err}"))?
    {
        let segment_id = row_usize(posting_row, 0)
            .map_err(|err| format!("decode short posting segment ID: {err}"))?;
        let packed: Vec<u8> = posting_row
            .get(1)
            .map_err(|err| format!("decode short posting row: {err}"))?;
        let mut ids = Vec::new();
        unpack_doc_ids(&packed, &mut ids)?;
        let first_doc_id = ids
            .first()
            .copied()
            .ok_or_else(|| "short posting row has no documents".to_string())?;
        let last_doc_id = ids.last().copied().unwrap_or(first_doc_id);
        let first_doc_id = sql_integer(first_doc_id, "short posting first document ID")?;
        let last_doc_id = sql_integer(last_doc_id, "short posting last document ID")?;
        let mut ids = ids.into_iter().peekable();
        let mut doc_rows = docs_stmt
            .query(params![first_doc_id, last_doc_id])
            .map_err(|err| format!("query short posting documents: {err}"))?;
        while let Some(doc_row) = doc_rows
            .next()
            .map_err(|err| format!("read short posting document: {err}"))?
        {
            let doc = row_doc_meta(doc_row)
                .map_err(|err| format!("decode short posting document: {err}"))?;
            if doc.segment_id != segment_id {
                return Err(format!(
                    "short posting segment {segment_id} references document {} in segment {}",
                    doc.doc_id, doc.segment_id
                ));
            }
            let Some(&candidate_id) = ids.peek() else {
                break;
            };
            if candidate_id < doc.doc_id {
                return Err(format!(
                    "short posting references missing document {candidate_id}"
                ));
            }
            if candidate_id != doc.doc_id {
                continue;
            }
            ids.next();
            candidate_count = candidate_count.saturating_add(1);
            if !visit(doc)? {
                return Ok(candidate_count);
            }
        }
        if let Some(candidate_id) = ids.next() {
            return Err(format!(
                "short posting references missing document {candidate_id}"
            ));
        }
    }
    Ok(candidate_count)
}

fn visit_filter_candidates<F>(
    conn: &Connection,
    query: &QueryCase,
    visit: &mut F,
) -> Result<u64, String>
where
    F: FnMut(DocMeta) -> Result<bool, String>,
{
    let text = std::str::from_utf8(&query.bytes)
        .map_err(|err| format!("search query is not UTF-8: {err}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT doc_id, segment_id, core_start, core_end, record_end, filter
             FROM filter_docs ORDER BY doc_id",
        )
        .map_err(|err| format!("prepare chunk filter scan: {err}"))?;
    let mut rows = stmt
        .query([])
        .map_err(|err| format!("query chunk filters: {err}"))?;
    let mut candidate_count = 0_u64;
    while let Some(row) = rows
        .next()
        .map_err(|err| format!("read chunk filter row: {err}"))?
    {
        let filter: Vec<u8> = row
            .get(5)
            .map_err(|err| format!("decode chunk filter: {err}"))?;
        if !filter_matches(&filter, text)? {
            continue;
        }
        let doc = row_doc_meta(row).map_err(|err| format!("decode chunk document: {err}"))?;
        candidate_count = candidate_count.saturating_add(1);
        if !visit(doc)? {
            break;
        }
    }
    Ok(candidate_count)
}

fn sql_integer<T: TryInto<i64>>(value: T, field: &str) -> Result<i64, String> {
    value
        .try_into()
        .map_err(|_| format!("{field} exceeds SQLite INTEGER"))
}

fn row_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(index)?;
    u64::try_from(value).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(err),
        )
    })
}

fn row_usize(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<usize> {
    let value = row.get::<_, i64>(index)?;
    usize::try_from(value).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(err),
        )
    })
}

fn row_doc_meta(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocMeta> {
    Ok(DocMeta {
        doc_id: row_u64(row, 0)?,
        segment_id: row_usize(row, 1)?,
        core_start: row_usize(row, 2)?,
        core_end: row_usize(row, 3)?,
        record_end: row_usize(row, 4)?,
    })
}

fn fts_anchor_expression(query: &str) -> String {
    query_anchor_grams(query)
        .into_iter()
        .map(|gram| {
            let text = gram
                .into_iter()
                .filter_map(char::from_u32)
                .collect::<String>();
            format!("\"{}\"", text.replace('"', "\"\""))
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn query_anchor_grams(query: &str) -> Vec<[u32; 3]> {
    let anchor_end = floor_char_boundary(query, QUERY_ANCHOR_BYTES.min(query.len()));
    let chars = query[..anchor_end]
        .chars()
        .map(|ch| ch as u32)
        .collect::<Vec<_>>();
    let gram_count = chars.len().saturating_sub(2);
    if gram_count == 0 {
        return Vec::new();
    }
    let selected = QUERY_ANCHOR_GRAMS.min(gram_count);
    let mut grams = Vec::with_capacity(selected);
    for index in 0..selected {
        let start = if selected == 1 {
            0
        } else {
            index * (gram_count - 1) / (selected - 1)
        };
        let gram = [chars[start], chars[start + 1], chars[start + 2]];
        if !grams.contains(&gram) {
            grams.push(gram);
        }
    }
    grams
}

fn record_bounds(record_ends: &[usize], offset: usize) -> Option<(usize, usize)> {
    let record_index = record_ends.partition_point(|end| *end <= offset);
    let record_end = *record_ends.get(record_index)?;
    let record_start = record_index
        .checked_sub(1)
        .map_or(0, |index| record_ends[index]);
    Some((record_start, record_end))
}

impl<'a> Verifier<'a> {
    fn new(catalog: &'a SegmentCatalog) -> Self {
        Self {
            catalog,
            files: HashMap::new(),
            verified_bytes: 0,
        }
    }

    fn matches(&mut self, doc: DocMeta, query: &[u8]) -> Result<Vec<usize>, String> {
        if query.is_empty() {
            return Err("search query must be nonempty".into());
        }
        let segment_len =
            *self.catalog.lengths.get(doc.segment_id).ok_or_else(|| {
                format!("candidate references missing segment {}", doc.segment_id)
            })?;
        if doc.core_start >= doc.core_end
            || doc.core_end > doc.record_end
            || doc.record_end > segment_len
        {
            return Err("candidate document bounds are invalid".into());
        }
        let record_ends = self
            .catalog
            .record_ends
            .get(doc.segment_id)
            .ok_or_else(|| "candidate segment record metadata is missing".to_string())?;
        let (record_start, canonical_record_end) = record_bounds(record_ends, doc.core_start)
            .ok_or_else(|| "candidate document start is outside canonical records".to_string())?;
        if doc.core_start < record_start || doc.record_end != canonical_record_end {
            return Err("candidate document does not match its canonical record".into());
        }
        let verify_end = doc
            .core_end
            .saturating_add(query.len().saturating_sub(1))
            .min(doc.record_end)
            .min(segment_len);
        let length = verify_end - doc.core_start;
        let mut bytes = vec![0u8; length];
        let file = match self.files.entry(doc.segment_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let path = self
                    .catalog
                    .paths
                    .get(doc.segment_id)
                    .ok_or_else(|| "candidate segment path is missing".to_string())?;
                entry.insert(
                    File::open(path).map_err(|err| format!("open canonical segment: {err}"))?,
                )
            }
        };
        let read_offset = u64::try_from(doc.core_start)
            .map_err(|_| "candidate document offset exceeds u64".to_string())?;
        read_file_at(file, read_offset, &mut bytes)?;
        self.verified_bytes = self.verified_bytes.saturating_add(length as u64);
        let owned_len = doc.core_end - doc.core_start;
        Ok(find_occurrences(&bytes, query)
            .into_iter()
            .filter(|offset| *offset < owned_len)
            .map(|offset| doc.core_start + offset)
            .collect())
    }
}

#[cfg(unix)]
fn read_file_at(file: &File, offset: u64, bytes: &mut [u8]) -> Result<(), String> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(bytes, offset)
        .map_err(|err| format!("read canonical verification bytes: {err}"))
}

#[cfg(not(unix))]
fn read_file_at(file: &File, offset: u64, bytes: &mut [u8]) -> Result<(), String> {
    use std::io::{Seek, SeekFrom};
    let mut file = file;
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(bytes))
        .map_err(|err| format!("read canonical verification bytes: {err}"))
}

fn direct_scan(catalog: &SegmentCatalog, query: &[u8]) -> Result<MatchSummary, String> {
    let mut summary = MatchSummary::default();
    for (segment_id, path) in catalog.paths.iter().enumerate() {
        let bytes = fs::read(path).map_err(|err| format!("read direct-scan segment: {err}"))?;
        let record_ends = catalog
            .record_ends
            .get(segment_id)
            .ok_or_else(|| "direct-scan record metadata is missing".to_string())?;
        let mut record_start = 0usize;
        for &record_end in record_ends {
            let record = bytes
                .get(record_start..record_end)
                .ok_or_else(|| "direct-scan record metadata is invalid".to_string())?;
            visit_occurrences(record, query, |offset| {
                summary.push(segment_id, record_start + offset);
            });
            record_start = record_end;
        }
        if record_start != bytes.len() {
            return Err("direct-scan record metadata does not cover its segment".into());
        }
    }
    Ok(summary)
}

fn visit_occurrences(haystack: &[u8], needle: &[u8], mut visit: impl FnMut(usize)) {
    if needle.is_empty() || needle.len() > haystack.len() {
        return;
    }
    let mut offset = 0usize;
    while offset + needle.len() <= haystack.len() {
        let Some(relative) = haystack[offset..=haystack.len() - needle.len()]
            .iter()
            .position(|byte| *byte == needle[0])
        else {
            break;
        };
        offset += relative;
        if haystack[offset..].starts_with(needle) {
            visit(offset);
        }
        offset += 1;
    }
}

fn find_occurrences(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut matches = Vec::new();
    visit_occurrences(haystack, needle, |offset| matches.push(offset));
    matches
}

fn build_query_cases(catalog: &SegmentCatalog, doc_size: usize) -> Result<Vec<QueryCase>, String> {
    let mut samples = Vec::new();
    for path in catalog.paths.iter().take(8) {
        samples.push(fs::read(path).map_err(|err| format!("read query sample segment: {err}"))?);
    }
    let sample = samples.concat();
    let text = std::str::from_utf8(&sample)
        .map_err(|err| format!("canonical corpus is not UTF-8: {err}"))?;
    let one = if text.contains('e') {
        "e".to_string()
    } else {
        text.chars()
            .find(|ch| *ch != '\n')
            .map(|ch| ch.to_string())
            .ok_or_else(|| "corpus has no searchable scalar".to_string())?
    };
    let two = if text.contains("th") {
        "th".to_string()
    } else {
        first_query_chars(text, 2).ok_or_else(|| "corpus has no searchable bigram".to_string())?
    };
    let common = ["the function", "function", "error", "return", "the"]
        .into_iter()
        .find(|query| text.contains(query))
        .unwrap_or(&two)
        .to_string();
    let punctuation = ["::", "foo_bar%", "://", "`", "\""]
        .into_iter()
        .find(|query| text.contains(query))
        .unwrap_or(&two)
        .to_string();
    let unicode = text
        .chars()
        .find(|ch| !ch.is_ascii() && *ch != '\n')
        .map(|ch| ch.to_string())
        .unwrap_or_else(|| one.clone());
    let rare = rare_query(text).unwrap_or_else(|| common.clone());
    let long = long_query(text).unwrap_or_else(|| common.clone());
    let boundary = boundary_query(&sample, doc_size).unwrap_or_else(|| common.clone());
    let absent_two = absent_scalar_query(text, 2);
    let absent_common_grams = absent_common_gram_query(catalog, &samples, 48)?
        .ok_or_else(|| "corpus has no reusable trigram path for an absent query".to_string())?;
    let absent_long = absent_scalar_query(text, 24);
    let mut cases = vec![
        QueryCase {
            class: "one_common",
            bytes: one.into_bytes(),
        },
        QueryCase {
            class: "two_common",
            bytes: two.into_bytes(),
        },
        QueryCase {
            class: "common_phrase",
            bytes: common.into_bytes(),
        },
        QueryCase {
            class: "rare",
            bytes: rare.into_bytes(),
        },
        QueryCase {
            class: "punctuation",
            bytes: punctuation.into_bytes(),
        },
        QueryCase {
            class: "unicode",
            bytes: unicode.into_bytes(),
        },
        QueryCase {
            class: "long_anchor",
            bytes: long.into_bytes(),
        },
        QueryCase {
            class: "document_boundary",
            bytes: boundary.into_bytes(),
        },
        QueryCase {
            class: "absent_two",
            bytes: absent_two.into_bytes(),
        },
        QueryCase {
            class: "absent_common_grams",
            bytes: absent_common_grams.into_bytes(),
        },
        QueryCase {
            class: "absent_long",
            bytes: absent_long.into_bytes(),
        },
    ];
    cases.retain(|query| !query.bytes.is_empty() && !query.bytes.contains(&b'\n'));
    let mut seen = HashSet::new();
    cases.retain(|query| seen.insert((query.class, query.bytes.clone())));
    Ok(cases)
}

fn first_query_chars(text: &str, count: usize) -> Option<String> {
    let chars = text
        .chars()
        .filter(|ch| *ch != '\n')
        .take(count)
        .collect::<String>();
    (chars.chars().count() == count).then_some(chars)
}

fn rare_query(text: &str) -> Option<String> {
    text.split(|ch: char| ch.is_whitespace())
        .rev()
        .find(|word| word.chars().count() >= 8)
        .map(|word| word.chars().take(32).collect())
}

fn long_query(text: &str) -> Option<String> {
    text.lines()
        .find(|line| line.len() >= DOC_OVERLAP_BYTES + 256)
        .map(|line| {
            let end = floor_char_boundary(line, (DOC_OVERLAP_BYTES + 256).min(line.len()));
            line[..end].to_string()
        })
}

fn boundary_query(bytes: &[u8], doc_size: usize) -> Option<String> {
    let mut boundary = doc_size;
    while boundary + 24 < bytes.len() {
        let start = floor_utf8_boundary(bytes, boundary.saturating_sub(12));
        let end = floor_utf8_boundary(bytes, (boundary + 24).min(bytes.len()));
        let candidate = std::str::from_utf8(&bytes[start..end]).ok()?;
        if !candidate.contains('\n') && candidate.chars().count() >= 3 {
            return Some(candidate.to_string());
        }
        boundary += doc_size;
    }
    None
}

fn absent_scalar_query(text: &str, chars: usize) -> String {
    let seeds = ['\u{10ffff}', '\u{10fffe}', '\u{10ffd}', '\u{10ffc}'];
    for rotation in 0..seeds.len() {
        let candidate = (0..chars)
            .map(|index| seeds[(index + rotation) % seeds.len()])
            .collect::<String>();
        if !text.contains(&candidate) {
            return candidate;
        }
    }
    "smelt-search-prototype-proven-absent-value".repeat(chars.max(1))
}

fn absent_common_gram_query(
    catalog: &SegmentCatalog,
    samples: &[Vec<u8>],
    target_bytes: usize,
) -> Result<Option<String>, String> {
    const MAX_CANDIDATES: usize = 64;

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (segment_id, bytes) in samples.iter().enumerate() {
        let expected_len = catalog
            .lengths
            .get(segment_id)
            .ok_or_else(|| "query sample has no canonical segment".to_string())?;
        if bytes.len() != *expected_len {
            return Err("query sample length differs from its canonical segment".into());
        }
        let record_ends = catalog
            .record_ends
            .get(segment_id)
            .ok_or_else(|| "query sample has no canonical record metadata".to_string())?;
        if record_ends.last().copied() != Some(bytes.len()) {
            return Err("query sample record metadata does not cover its segment".into());
        }
        let mut record_start = 0usize;
        for &record_end in record_ends {
            let record = bytes
                .get(record_start..record_end)
                .ok_or_else(|| "query sample record metadata is invalid".to_string())?;
            let record = std::str::from_utf8(record)
                .map_err(|err| format!("canonical corpus is not UTF-8: {err}"))?;
            for line in record.split('\n') {
                collect_common_gram_splices(
                    line,
                    target_bytes,
                    MAX_CANDIDATES,
                    &mut seen,
                    &mut candidates,
                );
                if candidates.len() == MAX_CANDIDATES {
                    break;
                }
            }
            record_start = record_end;
            if candidates.len() == MAX_CANDIDATES {
                break;
            }
        }
        if candidates.len() == MAX_CANDIDATES {
            break;
        }
    }
    if candidates.is_empty() {
        return Ok(None);
    }

    let mut present = vec![false; candidates.len()];
    for (segment_id, path) in catalog.paths.iter().enumerate() {
        let bytes = fs::read(path).map_err(|err| format!("read absent-query segment: {err}"))?;
        let record_ends = catalog
            .record_ends
            .get(segment_id)
            .ok_or_else(|| "absent-query record metadata is missing".to_string())?;
        let mut record_start = 0usize;
        for &record_end in record_ends {
            let record = bytes
                .get(record_start..record_end)
                .ok_or_else(|| "absent-query record metadata is invalid".to_string())?;
            for (index, candidate) in candidates.iter().enumerate() {
                if !present[index]
                    && candidate.len() <= record.len()
                    && record
                        .windows(candidate.len())
                        .any(|window| window == candidate.as_bytes())
                {
                    present[index] = true;
                }
            }
            record_start = record_end;
        }
        if record_start != bytes.len() {
            return Err("absent-query record metadata does not cover its segment".into());
        }
    }

    Ok(candidates
        .into_iter()
        .zip(present)
        .find_map(|(candidate, present)| (!present).then_some(candidate)))
}

fn collect_common_gram_splices(
    line: &str,
    target_bytes: usize,
    limit: usize,
    seen: &mut HashSet<String>,
    candidates: &mut Vec<String>,
) {
    let mut region_start = 0usize;
    while region_start < line.len() && candidates.len() < limit {
        let region_end = floor_char_boundary(
            line,
            region_start
                .saturating_add(QUERY_ANCHOR_BYTES)
                .min(line.len()),
        );
        if region_end <= region_start {
            break;
        }
        collect_region_splices(
            &line[region_start..region_end],
            target_bytes.max(3),
            limit,
            seen,
            candidates,
        );
        if region_end == line.len() {
            break;
        }
        let next = floor_char_boundary(
            line,
            region_start
                .saturating_add(QUERY_ANCHOR_BYTES / 2)
                .min(line.len()),
        );
        if next <= region_start {
            break;
        }
        region_start = next;
    }
}

fn collect_region_splices(
    region: &str,
    target_bytes: usize,
    limit: usize,
    seen: &mut HashSet<String>,
    candidates: &mut Vec<String>,
) {
    let mut starts = Vec::new();
    let mut scalars = Vec::new();
    for (start, ch) in region.char_indices() {
        starts.push(start);
        scalars.push(ch as u32);
    }
    starts.push(region.len());
    if scalars.len() < 4 {
        return;
    }

    let prefix_targets = [
        target_bytes / 3,
        target_bytes / 2,
        target_bytes.saturating_mul(2) / 3,
    ];
    let mut occurrences: HashMap<[u32; 2], Vec<usize>> = HashMap::new();
    for right in 0..scalars.len().saturating_sub(2) {
        let key = [scalars[right], scalars[right + 1]];
        if let Some(lefts) = occurrences.get(&key) {
            for &left in lefts.iter().rev().take(4) {
                for (prefix_occurrence, suffix_occurrence) in [(left, right), (right, left)] {
                    for desired_prefix_bytes in prefix_targets {
                        let prefix_end = starts[prefix_occurrence + 2];
                        let desired_prefix_start =
                            prefix_end.saturating_sub(desired_prefix_bytes.max(2));
                        let prefix_start_index =
                            starts.partition_point(|start| *start < desired_prefix_start);
                        if prefix_start_index > prefix_occurrence {
                            continue;
                        }

                        let suffix_start = starts[suffix_occurrence + 2];
                        let desired_suffix_bytes =
                            target_bytes.saturating_sub(prefix_end - starts[prefix_start_index]);
                        let desired_suffix_end =
                            suffix_start.saturating_add(desired_suffix_bytes.max(1));
                        let mut suffix_end_index = starts
                            .partition_point(|start| *start <= desired_suffix_end)
                            .saturating_sub(1);
                        suffix_end_index = suffix_end_index.max(suffix_occurrence + 3);
                        if suffix_end_index > scalars.len() {
                            continue;
                        }

                        let prefix = &region[starts[prefix_start_index]..prefix_end];
                        let suffix = &region[suffix_start..starts[suffix_end_index]];
                        let candidate = format!("{prefix}{suffix}");
                        if candidate.chars().count() >= 3
                            && !candidate.contains('\n')
                            && seen.insert(candidate.clone())
                        {
                            candidates.push(candidate);
                            if candidates.len() == limit {
                                return;
                            }
                        }
                    }
                }
            }
        }
        let positions = occurrences.entry(key).or_default();
        if positions.len() < 8 {
            positions.push(right);
        }
    }
}

fn run_fm_worker(options: &WorkerOptions, catalog: &SegmentCatalog) -> Result<Value, String> {
    let mut indexes = Vec::with_capacity(catalog.paths.len());
    let build = measure_build(|| {
        for path in &catalog.paths {
            let mut bytes =
                fs::read(path).map_err(|err| format!("read FM-index segment: {err}"))?;
            if bytes.contains(&0) {
                return Err("FM-index experiment does not accept NUL corpus bytes".into());
            }
            bytes.push(0);
            let index = RLFMIndexWithLocate::new(&Text::new(&bytes), FM_LOCATE_SAMPLE_LEVEL)
                .map_err(|err| format!("build run-length FM-index: {err}"))?;
            indexes.push(index);
        }
        Ok(())
    })?;
    let queries = build_query_cases(catalog, options.doc_size.max(4096))?;
    let physical_bytes = indexes.iter().map(SearchIndex::heap_size).sum::<usize>() as u64;
    let storage_ratio = physical_bytes as f64 / catalog.logical_bytes.max(1) as f64;
    let mut exact = true;
    let mut warm_latency_pass = true;
    let mut reports = Vec::new();
    for query in &queries {
        let oracle = direct_scan(catalog, &query.bytes)?;
        let differential = run_fm_search(&indexes, catalog, query, None)?;
        exact &= differential.summary == oracle;
        let mut first = Vec::with_capacity(options.runs);
        let mut next = Vec::new();
        for _ in 0..options.runs {
            let sample = run_fm_search(&indexes, catalog, query, Some(2))?;
            first.push(duration_ms(sample.first.unwrap_or(sample.total)));
            if let Some(elapsed) = sample.next {
                next.push(duration_ms(elapsed));
            }
        }
        let mut full = Vec::with_capacity(options.runs);
        for _ in 0..options.runs {
            full.push(duration_ms(
                run_fm_search(&indexes, catalog, query, None)?.total,
            ));
        }
        let warm_p95 = percentile(&first, 95);
        let next_p95 = (!next.is_empty()).then(|| percentile(&next, 95));
        let full_p95 = percentile(&full, 95);
        let absent = oracle.count == 0;
        let query_warm_pass = if absent {
            warm_p95 <= 100.0 && full_p95 <= 100.0
        } else {
            warm_p95 <= 50.0 && next_p95.is_none_or(|value| value <= 50.0)
        };
        warm_latency_pass &= query_warm_pass;
        reports.push(json!({
            "class": query.class,
            "query_bytes": query.bytes.len(),
            "matches": oracle.count,
            "candidates": differential.candidates,
            "verified_bytes": differential.verified_bytes,
            "cold_cache_attempt_success": Vec::<bool>::new(),
            "cold_cache_gate_pass": false,
            "cold_first_samples": 0,
            "cold_first_p95_ms": Value::Null,
            "warm_first_samples": first.len(),
            "warm_first_p95_ms": warm_p95,
            "next_samples": next.len(),
            "next_p95_ms": next_p95,
            "full_enumeration_samples": full.len(),
            "full_enumeration_p95_ms": full_p95,
            "full_enumeration_in_latency_gate": absent,
            "exact": differential.summary == oracle,
            "warm_latency_pass": query_warm_pass,
            "latency_pass": false,
        }));
    }
    let storage_pass = storage_ratio <= 0.25;
    Ok(json!({
        "type": "search_prototype_configuration",
        "representation": "run_length_fm_index",
        "doc_bytes": Value::Null,
        "segment_bytes": options.segment_size,
        "logical_bytes": catalog.logical_bytes,
        "physical_bytes": physical_bytes,
        "physical_bytes_method": "retained_heap",
        "storage_ratio": storage_ratio,
        "storage_target_pass": storage_ratio <= 0.15,
        "storage_hard_pass": storage_pass,
        "build_ms": duration_ms(build.elapsed),
        "build_mib_per_s": throughput_mib(catalog.logical_bytes, build.elapsed),
        "build_allocated_bytes": build.allocated_bytes,
        "build_start_live_bytes": build.start_live_bytes,
        "build_end_live_bytes": build.end_live_bytes,
        "build_peak_live_bytes": build.peak_live_bytes,
        "build_peak_live_incremental_bytes": build.peak_live_bytes.saturating_sub(build.start_live_bytes),
        "build_live_bytes_method": "phase_local_global_allocator",
        "build_start_rss_bytes": build.start_rss_bytes,
        "build_end_rss_bytes": build.end_rss_bytes,
        "process_peak_rss_bytes": build.peak_rss_bytes,
        "process_peak_rss_above_build_start_bytes": build.peak_rss_bytes.saturating_sub(build.start_rss_bytes),
        "rss_peak_scope": "process_lifetime_high_water_not_phase_local",
        "cold_cache_method": Value::Null,
        "cold_cache_method_supported": false,
        "cold_cache_all_attempts_succeeded": false,
        "cold_cache_attempts": 0,
        "cold_cache_successes": 0,
        "requested_samples_per_query": options.runs,
        "exact": exact,
        "warm_latency_pass": warm_latency_pass,
        "latency_pass": false,
        "passes": false,
        "passes_without_cold_gate": exact && storage_pass && warm_latency_pass,
        "queries": reports,
    }))
}

fn query_within_record(record_ends: &[usize], offset: usize, query_len: usize) -> bool {
    query_len > 0
        && record_bounds(record_ends, offset).is_some_and(|(_, record_end)| {
            offset
                .checked_add(query_len)
                .is_some_and(|query_end| query_end <= record_end)
        })
}

fn run_fm_search(
    indexes: &[RLFMIndexWithLocate<u8>],
    catalog: &SegmentCatalog,
    query: &QueryCase,
    limit_matches: Option<u64>,
) -> Result<SearchMeasurement, String> {
    if query.bytes.is_empty() {
        return Err("search query must be nonempty".into());
    }
    let started = Instant::now();
    let mut summary = MatchSummary::default();
    let mut first = None;
    let mut next = None;
    let mut candidates = 0u64;
    let mut verified_bytes = 0u64;
    for (segment_id, index) in indexes.iter().enumerate() {
        let record_ends = catalog
            .record_ends
            .get(segment_id)
            .ok_or_else(|| "FM-index record metadata is missing".to_string())?;
        let search = index.search(&query.bytes);
        candidates = candidates.saturating_add(search.count() as u64);
        let mut positions = search
            .iter_matches()
            .map(|matched| matched.locate())
            .filter(|offset| query_within_record(record_ends, *offset, query.bytes.len()))
            .collect::<Vec<_>>();
        positions.sort_unstable();
        let mut file = None;
        for offset in positions {
            let file = match &mut file {
                Some(file) => file,
                None => {
                    let path = catalog
                        .paths
                        .get(segment_id)
                        .ok_or_else(|| "FM-index segment path is missing".to_string())?;
                    file.insert(
                        File::open(path)
                            .map_err(|err| format!("open FM verification segment: {err}"))?,
                    )
                }
            };
            let mut bytes = vec![0u8; query.bytes.len()];
            let read_offset = u64::try_from(offset)
                .map_err(|_| "FM-index match offset exceeds u64".to_string())?;
            read_file_at(file, read_offset, &mut bytes)?;
            verified_bytes = verified_bytes.saturating_add(bytes.len() as u64);
            if bytes != query.bytes {
                continue;
            }
            summary.push(segment_id, offset);
            if summary.count == 1 {
                first = Some(started.elapsed());
            } else if summary.count == 2 {
                next = Some(started.elapsed().saturating_sub(first.unwrap_or_default()));
            }
            if limit_matches.is_some_and(|limit| summary.count >= limit) {
                return Ok(SearchMeasurement {
                    summary,
                    first,
                    next,
                    total: started.elapsed(),
                    candidates,
                    verified_bytes,
                });
            }
        }
    }
    Ok(SearchMeasurement {
        summary,
        first,
        next,
        total: started.elapsed(),
        candidates,
        verified_bytes,
    })
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn floor_utf8_boundary(bytes: &[u8], mut index: usize) -> usize {
    index = index.min(bytes.len());
    while index > 0 && index < bytes.len() && bytes[index] & 0b1100_0000 == 0b1000_0000 {
        index -= 1;
    }
    index
}

fn next_utf8_boundary(bytes: &[u8], mut index: usize) -> usize {
    index = index.min(bytes.len());
    while index < bytes.len() && bytes[index] & 0b1100_0000 == 0b1000_0000 {
        index += 1;
    }
    index
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn throughput_mib(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64().max(f64::MIN_POSITIVE)
}

fn percentile(values: &[f64], percentile: usize) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = sorted.len().saturating_mul(percentile).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn linux_memory_bytes(field: &str) -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix(field))?
        .split_ascii_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(kib.saturating_mul(1024))
}

fn drop_sqlite_search_cache(projection: &Path, catalog: &SegmentCatalog) -> bool {
    let Ok(projection_paths) = sqlite_storage_paths(projection) else {
        return false;
    };
    let canonical_paths = catalog
        .paths
        .iter()
        .flat_map(|path| [path.clone(), path.with_extension("records")]);
    let mut dropped = true;
    for path in projection_paths.into_iter().chain(canonical_paths) {
        dropped = drop_file_cache(&path) && dropped;
    }
    dropped
}

#[cfg(unix)]
fn cold_cache_method() -> Option<&'static str> {
    Some("posix_fadvise_dontneed")
}

#[cfg(not(unix))]
fn cold_cache_method() -> Option<&'static str> {
    None
}

#[cfg(unix)]
fn drop_file_cache(path: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = File::open(path) else {
        return false;
    };
    unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) == 0 }
}

#[cfg(not(unix))]
fn drop_file_cache(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "smelt-search-prototype-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn test_catalog(text: &str) -> (PathBuf, SegmentCatalog) {
        test_catalog_records(&[text])
    }

    fn test_catalog_records(records: &[&str]) -> (PathBuf, SegmentCatalog) {
        test_catalog_segments(&[records])
    }

    fn test_catalog_segments(segments: &[&[&str]]) -> (PathBuf, SegmentCatalog) {
        let directory = temp_path("catalog");
        fs::create_dir(&directory).unwrap();
        for (segment_id, records) in segments.iter().enumerate() {
            let mut bytes = Vec::new();
            let mut record_ends = Vec::with_capacity(records.len());
            for record in *records {
                assert!(!record.is_empty());
                bytes.extend_from_slice(record.as_bytes());
                record_ends.push(bytes.len());
            }
            write_segment(&directory, segment_id, &bytes, &record_ends).unwrap();
        }
        let catalog = load_segment_catalog(&directory).unwrap();
        (directory, catalog)
    }

    fn query_case(text: &str) -> QueryCase {
        QueryCase {
            class: "test",
            bytes: text.as_bytes().to_vec(),
        }
    }

    fn common_gram_fixture_text() -> &'static str {
        concat!(
            "αβγδεζηθικλμνξοπρστυφχψωAB一二三四五六七八九十甲乙丙丁戊己庚辛 ",
            "zyxwvutsrqponmlkjihgfedcbaAB9876543210[]{}<>!?~+=-_/,.;:"
        )
    }

    fn common_gram_candidates(text: &str) -> Vec<String> {
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        collect_common_gram_splices(text, 48, 64, &mut seen, &mut candidates);
        candidates
    }

    fn encode_record_ends(ends: &[u64]) -> Vec<u8> {
        ends.iter().flat_map(|end| end.to_le_bytes()).collect()
    }

    fn assert_representations_match_oracle(
        directory: &Path,
        catalog: &SegmentCatalog,
        doc_size: usize,
        queries: &[QueryCase],
    ) {
        for fts in [true, false] {
            let projection = directory.join(if fts { "fts.db" } else { "filters.db" });
            if fts {
                build_fts_projection(&projection, catalog, doc_size).unwrap();
            } else {
                build_filter_projection(&projection, catalog, doc_size).unwrap();
            }
            for query in queries {
                let oracle = direct_scan(catalog, &query.bytes).unwrap();
                let actual =
                    run_sqlite_search(&projection, catalog, query, fts, None, false).unwrap();
                assert_eq!(actual.summary, oracle, "{}", query.class);
            }
        }

        let indexes = catalog
            .paths
            .iter()
            .map(|path| {
                let mut bytes = fs::read(path).unwrap();
                assert!(!bytes.contains(&0));
                bytes.push(0);
                RLFMIndexWithLocate::new(&Text::new(&bytes), FM_LOCATE_SAMPLE_LEVEL).unwrap()
            })
            .collect::<Vec<_>>();
        for query in queries {
            let oracle = direct_scan(catalog, &query.bytes).unwrap();
            let actual = run_fm_search(&indexes, catalog, query, None).unwrap();
            assert_eq!(actual.summary, oracle, "FM-index: {}", query.class);
        }
    }

    fn assert_search_stops_after_two_matches(query: &str, fts: bool) {
        let records = (0..64)
            .map(|index| format!("{query}:{index:04}"))
            .collect::<Vec<_>>();
        let record_refs = records.iter().map(String::as_str).collect::<Vec<_>>();
        let (directory, catalog) = test_catalog_records(&record_refs);
        let projection = directory.join(if fts { "fts.db" } else { "filters.db" });
        if fts {
            build_fts_projection(&projection, &catalog, 64).unwrap();
        } else {
            build_filter_projection(&projection, &catalog, 64).unwrap();
        }
        let query = query_case(query);
        let oracle = direct_scan(&catalog, &query.bytes).unwrap();
        let bounded =
            run_sqlite_search(&projection, &catalog, &query, fts, Some(2), false).unwrap();
        let full = run_sqlite_search(&projection, &catalog, &query, fts, None, false).unwrap();

        assert_eq!(oracle.count, 64);
        assert_eq!(full.summary, oracle);
        assert_eq!(bounded.summary.count, 2);
        assert_eq!(bounded.summary.first, oracle.first);
        assert_eq!(bounded.summary.second, oracle.second);
        assert_eq!(bounded.candidates, 2);
        assert!(bounded.candidates < full.candidates);

        fs::remove_dir_all(directory).unwrap();
    }

    fn short_posting_fixture() -> (PathBuf, SegmentCatalog, PathBuf) {
        let (directory, catalog) = test_catalog_records(&["a0", "a1", "a2"]);
        let projection = directory.join("fts.db");
        build_fts_projection(&projection, &catalog, 2).unwrap();
        (directory, catalog, projection)
    }

    fn replace_short_posting(projection: &Path, packed: &[u8]) {
        let conn = Connection::open(projection).unwrap();
        let hash = hash_scalars(1, &['a' as u32]);
        let updated = conn
            .execute(
                "UPDATE short_postings SET docs = ?1
                 WHERE segment_id = 0 AND kind = 1 AND gram_hash = ?2",
                params![packed, hash as i64],
            )
            .unwrap();
        assert_eq!(updated, 1);
    }

    #[test]
    fn short_posting_search_stops_after_two_verified_matches() {
        assert_search_stops_after_two_matches("a", true);
    }

    #[test]
    fn chunk_filter_search_stops_after_two_verified_matches() {
        assert_search_stops_after_two_matches("a", false);
    }

    #[test]
    fn compact_fts_search_stops_after_two_verified_matches() {
        assert_search_stops_after_two_matches("needle", true);
    }

    #[test]
    fn common_gram_absent_query_is_deterministic_and_uses_observed_trigrams() {
        let text = common_gram_fixture_text();
        let (directory, catalog) = test_catalog(text);
        let samples = vec![text.as_bytes().to_vec()];

        let first = absent_common_gram_query(&catalog, &samples, 48)
            .unwrap()
            .expect("fixture should produce an absent splice");
        let second = absent_common_gram_query(&catalog, &samples, 48)
            .unwrap()
            .expect("fixture should produce an absent splice");

        assert_eq!(first, second);
        assert!(
            (32..=64).contains(&first.len()),
            "query length: {}",
            first.len()
        );
        assert!(!first.contains('\n'));
        assert_eq!(direct_scan(&catalog, first.as_bytes()).unwrap().count, 0);

        let corpus_chars = text.chars().collect::<Vec<_>>();
        let corpus_grams = corpus_chars
            .windows(3)
            .map(|gram| [gram[0] as u32, gram[1] as u32, gram[2] as u32])
            .collect::<HashSet<_>>();
        let query_chars = first.chars().collect::<Vec<_>>();
        assert!(query_chars.windows(3).all(|gram| corpus_grams.contains(&[
            gram[0] as u32,
            gram[1] as u32,
            gram[2] as u32,
        ])));
        assert!(query_anchor_grams(&first)
            .iter()
            .all(|gram| corpus_grams.contains(gram)));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn common_gram_absence_checks_unsampled_segments() {
        let text = common_gram_fixture_text();
        let candidates = common_gram_candidates(text);
        assert!(!candidates.is_empty());
        let candidate_records = candidates.iter().map(String::as_str).collect::<Vec<_>>();

        let mut segments = vec![vec![text]];
        for _ in 0..8 {
            segments.push(vec!["cdefghijklmnopqrstuvwxyz"]);
        }
        segments.push(candidate_records);
        let segment_refs = segments.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let (directory, catalog) = test_catalog_segments(&segment_refs);
        let samples = catalog
            .paths
            .iter()
            .take(8)
            .map(|path| fs::read(path).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            absent_common_gram_query(&catalog, &samples, 48).unwrap(),
            None
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn common_gram_absence_preserves_record_boundaries() {
        let text = common_gram_fixture_text();
        let (source_directory, source_catalog) = test_catalog(text);
        let source_samples = vec![text.as_bytes().to_vec()];
        let query = absent_common_gram_query(&source_catalog, &source_samples, 48)
            .unwrap()
            .expect("fixture should produce an absent splice");
        fs::remove_dir_all(source_directory).unwrap();

        let midpoint = query.len() / 2;
        let split = query
            .char_indices()
            .map(|(offset, _)| offset)
            .find(|offset| *offset >= midpoint)
            .unwrap();
        let (directory, catalog) = test_catalog_records(&[text, &query[..split], &query[split..]]);
        let samples = vec![fs::read(&catalog.paths[0]).unwrap()];
        let generated = absent_common_gram_query(&catalog, &samples, 48)
            .unwrap()
            .expect("a match spanning records must remain absent");

        assert_eq!(generated, query);
        assert_eq!(direct_scan(&catalog, query.as_bytes()).unwrap().count, 0);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn common_gram_candidate_limit_does_not_invalidate_complete_metadata() {
        let mut text = String::new();
        for index in 0..128 {
            text.push_str(&format!(
                "left{index:03}abcdefghijklmnopABqrstuvwxyz{index:03}|"
            ));
        }
        assert_eq!(common_gram_candidates(&text).len(), 64);
        let (directory, catalog) = test_catalog(&text);
        let samples = vec![text.into_bytes()];

        assert!(absent_common_gram_query(&catalog, &samples, 48).is_ok());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn common_gram_query_case_has_false_positive_candidates() {
        let text = common_gram_fixture_text();
        let (directory, catalog) = test_catalog(text);
        let cases = build_query_cases(&catalog, 64).unwrap();
        let matches = cases
            .iter()
            .filter(|query| query.class == "absent_common_grams")
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1);
        let query = matches[0];
        assert!(!query.bytes.is_empty());
        assert!(!query.bytes.contains(&b'\n'));
        let oracle = direct_scan(&catalog, &query.bytes).unwrap();
        assert_eq!(oracle.count, 0);

        for fts in [true, false] {
            let projection = directory.join(if fts {
                "fts-absent.db"
            } else {
                "filters-absent.db"
            });
            if fts {
                build_fts_projection(&projection, &catalog, 4096).unwrap();
            } else {
                build_filter_projection(&projection, &catalog, 4096).unwrap();
            }
            let actual = run_sqlite_search(&projection, &catalog, query, fts, None, false).unwrap();
            assert_eq!(actual.summary, oracle);
            assert!(
                actual.candidates > 0,
                "representation did not exercise verification"
            );
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn compact_fts_candidate_plan_does_not_sort_before_streaming() {
        let (directory, catalog) = test_catalog("needle one needle two");
        let projection = directory.join("fts.db");
        build_fts_projection(&projection, &catalog, 8).unwrap();
        let conn =
            Connection::open_with_flags(&projection, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let sql = format!("EXPLAIN QUERY PLAN {FTS_CANDIDATE_SQL}");
        let mut stmt = conn.prepare(&sql).unwrap();
        let expression = fts_anchor_expression("needle");
        let details = stmt
            .query_map([expression], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert!(!details.is_empty());
        assert!(
            details.iter().all(|detail| !detail.contains("TEMP B-TREE")),
            "candidate plan must stream without a temporary sort: {details:?}"
        );

        drop(stmt);
        drop(conn);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn short_postings_reject_duplicate_document_ids() {
        let (directory, catalog, projection) = short_posting_fixture();
        replace_short_posting(&projection, &[0, 0]);

        let error = run_sqlite_search(&projection, &catalog, &query_case("a"), true, None, false)
            .unwrap_err();
        assert!(error.contains("not strictly increasing"), "{error}");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn short_postings_reject_documents_from_another_segment() {
        let (directory, catalog, projection) = short_posting_fixture();
        let conn = Connection::open(&projection).unwrap();
        conn.execute("UPDATE docs SET segment_id = 1 WHERE doc_id = 0", [])
            .unwrap();
        drop(conn);

        let error = run_sqlite_search(&projection, &catalog, &query_case("a"), true, None, false)
            .unwrap_err();
        assert!(error.contains("in segment 1"), "{error}");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn short_postings_reject_missing_documents() {
        let (directory, catalog, projection) = short_posting_fixture();
        replace_short_posting(&projection, &pack_doc_ids(&[999]));

        let error = run_sqlite_search(&projection, &catalog, &query_case("a"), true, None, false)
            .unwrap_err();
        assert!(error.contains("missing document 999"), "{error}");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn short_postings_reject_empty_rows() {
        let (directory, catalog, projection) = short_posting_fixture();
        replace_short_posting(&projection, &[]);

        let error = run_sqlite_search(&projection, &catalog, &query_case("a"), true, None, false)
            .unwrap_err();
        assert!(error.contains("has no documents"), "{error}");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn compact_fts_and_filters_have_no_false_negatives() {
        let text = format!(
            "start café λ::needle foo_bar% quote \"the function\" aaabaaab {} boundary target end",
            "long exact anchor payload ".repeat(100)
        );
        let (directory, catalog) = test_catalog(&text);
        let long_query = "long exact anchor payload ".repeat(60);
        let query_texts = [
            "a",
            "fé",
            "λ::needle",
            "foo_bar%",
            "\"the function\"",
            "aaabaaab",
            "boundary target",
            long_query.as_str(),
            "\u{10ffff}\u{10fffe}",
        ];
        let queries = query_texts
            .into_iter()
            .enumerate()
            .map(|(index, text)| QueryCase {
                class: match index {
                    0 => "one",
                    1 => "two_unicode",
                    2 => "unicode_punctuation",
                    3 => "punctuation",
                    4 => "quoted_phrase",
                    5 => "repeated_trigrams",
                    6 => "boundary",
                    7 => "long",
                    _ => "absent",
                },
                bytes: text.as_bytes().to_vec(),
            })
            .collect::<Vec<_>>();
        assert_representations_match_oracle(&directory, &catalog, 64, &queries);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn representations_do_not_match_across_canonical_records() {
        let (directory, catalog) = test_catalog_records(&["abc", "def", "x"]);
        let queries = ["abc", "def", "x", "cde"].map(query_case).to_vec();
        assert_eq!(direct_scan(&catalog, b"cde").unwrap().count, 0);
        assert_representations_match_oracle(&directory, &catalog, 2, &queries);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn long_unicode_queries_verify_beyond_document_overlap() {
        let query = format!("λ{}終", "éx".repeat(600));
        assert!(query.len() > DOC_OVERLAP_BYTES);
        let split = floor_char_boundary(&query, query.len() / 2);
        let first_crossing_record = format!("{}{}", "p".repeat(14), &query[..split]);
        let second_crossing_record = format!("{} tail", &query[split..]);
        let matching_record = format!("{}{} tail", "p".repeat(14), query);
        let (directory, catalog) = test_catalog_records(&[
            &first_crossing_record,
            &second_crossing_record,
            &matching_record,
        ]);
        let query = query_case(&query);
        let oracle = direct_scan(&catalog, &query.bytes).unwrap();
        assert_eq!(oracle.count, 1);
        assert_representations_match_oracle(&directory, &catalog, 16, &[query]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn canonical_record_sidecars_are_strictly_validated() {
        let directory = temp_path("record-sidecar");
        fs::create_dir(&directory).unwrap();
        let segment = directory.join("segment-000000.txt");
        fs::write(&segment, b"abcdef").unwrap();
        let sidecar = segment.with_extension("records");

        assert!(load_record_ends(&segment, 6).is_err());
        let invalid_ends = [
            vec![],
            vec![1, 2, 3],
            encode_record_ends(&[3, 3]),
            encode_record_ends(&[7]),
            encode_record_ends(&[3, 5]),
        ];
        for metadata in invalid_ends {
            fs::write(&sidecar, metadata).unwrap();
            assert!(load_record_ends(&segment, 6).is_err());
        }
        fs::write(&sidecar, encode_record_ends(&[3, 6])).unwrap();
        assert_eq!(load_record_ends(&segment, 6).unwrap(), vec![3, 6]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn verifier_rejects_malformed_derived_record_bounds() {
        let (directory, catalog) = test_catalog_records(&["abc", "def"]);
        let mut verifier = Verifier::new(&catalog);
        let valid = DocMeta {
            doc_id: 0,
            segment_id: 0,
            core_start: 0,
            core_end: 3,
            record_end: 3,
        };
        assert_eq!(verifier.matches(valid, b"abc").unwrap(), vec![0]);
        assert!(verifier.matches(valid, b"").is_err());
        assert!(verifier
            .matches(
                DocMeta {
                    record_end: 6,
                    ..valid
                },
                b"abc"
            )
            .is_err());
        assert!(verifier
            .matches(
                DocMeta {
                    core_end: 2,
                    record_end: 2,
                    ..valid
                },
                b"ab"
            )
            .is_err());
        assert!(verifier
            .matches(
                DocMeta {
                    core_end: 0,
                    ..valid
                },
                b"a"
            )
            .is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sqlite_physical_size_includes_existing_sidecars() {
        let path = temp_path("projection-size");
        fs::write(&path, [0u8; 3]).unwrap();
        fs::write(sqlite_storage_path(&path, "-wal"), [0u8; 5]).unwrap();
        fs::write(sqlite_storage_path(&path, "-journal"), [0u8; 7]).unwrap();
        assert_eq!(sqlite_physical_bytes(&path).unwrap(), 15);
        fs::remove_file(&path).unwrap();
        fs::remove_file(sqlite_storage_path(&path, "-wal")).unwrap();
        fs::remove_file(sqlite_storage_path(&path, "-journal")).unwrap();
        assert!(sqlite_physical_bytes(&path).is_err());
    }

    #[test]
    fn sqlite_storage_paths_skip_missing_optional_sidecars() {
        let path = temp_path("projection-paths");
        let wal = sqlite_storage_path(&path, "-wal");
        fs::write(&path, [0u8; 1]).unwrap();
        fs::write(&wal, [0u8; 1]).unwrap();

        assert_eq!(
            sqlite_storage_paths(&path).unwrap(),
            vec![path.clone(), wal.clone()]
        );

        fs::remove_file(path).unwrap();
        fs::remove_file(wal).unwrap();
    }

    #[test]
    fn dropping_cache_fails_for_a_missing_required_file() {
        assert!(!drop_file_cache(&temp_path("missing-cache-file")));
    }

    #[cfg(unix)]
    #[test]
    fn dropping_cache_reports_successful_posix_advice() {
        let path = temp_path("cache-file");
        fs::write(&path, [0u8; 4096]).unwrap();

        assert!(drop_file_cache(&path));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn sqlite_cache_drop_requires_canonical_record_sidecars() {
        let (directory, catalog) = test_catalog("searchable text");
        let projection = directory.join("projection.db");
        fs::write(&projection, [0u8; 4096]).unwrap();
        fs::remove_file(catalog.paths[0].with_extension("records")).unwrap();

        assert!(!drop_sqlite_search_cache(&projection, &catalog));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn posting_varints_roundtrip_across_segment_absolute_ids() {
        let groups = [vec![0, 1, 2, 128, 65_000], vec![73, 80, 1_000_000]];
        let mut actual = Vec::new();
        for ids in &groups {
            unpack_doc_ids(&pack_doc_ids(ids), &mut actual).unwrap();
        }
        assert_eq!(actual, groups.concat());
    }

    #[test]
    fn record_reader_rejects_truncated_frames() {
        assert_eq!(read_record(&mut &[][..]).unwrap(), None);
        assert!(read_record(&mut &[1, 2, 3][..]).is_err());
        let mut truncated_payload = Vec::from(4u64.to_le_bytes());
        truncated_payload.extend_from_slice(b"abc");
        assert!(read_record(&mut truncated_payload.as_slice()).is_err());
    }

    #[test]
    fn anchor_grams_cover_the_start_and_end_of_a_bounded_prefix() {
        let query = "abcdefghijklmnopqrstuvwxyz".repeat(40);
        let grams = query_anchor_grams(&query);
        assert!(!grams.is_empty());
        assert!(grams.len() <= QUERY_ANCHOR_GRAMS);
        assert_eq!(grams[0], ['a' as u32, 'b' as u32, 'c' as u32]);
        let prefix_end = floor_char_boundary(&query, QUERY_ANCHOR_BYTES);
        let prefix = &query[..prefix_end];
        let last = prefix.chars().rev().take(3).collect::<Vec<_>>();
        assert!(grams.contains(&[last[2] as u32, last[1] as u32, last[0] as u32]));
    }

    #[test]
    fn filters_never_reject_inserted_unicode_grams() {
        let text = "aéλ foo_bar%";
        let filter = build_filter(text);
        for query in ["é", "éλ", "λ f", "foo_bar%"] {
            assert!(filter_matches(&filter, query).unwrap(), "{query}");
        }
    }
}
