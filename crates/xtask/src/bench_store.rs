//! `cargo xtask bench-store`: how the session store scales with the number and
//! size of sessions.
//!
//! Builds a store of many sessions and one very long one, shaped like real use:
//! tool outputs that are often read again, incompressible reasoning blocks and
//! occasional images. It then measures what users wait on: a save at the end of a
//! long session compared with one at its start, resume, transcript search, session
//! search, fork and delete, and what the data costs on disk.
//!
//! `--state-dir` measures a copy of a real state directory instead of building
//! one. It only reads, unless `--mutate` also allows forking and deleting.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use protocol::{AssistantStep, Content, HistoryItem, ReasoningBlock, ToolInvocation, ToolOutcome};
use serde_json::{json, Value};
use smelt_store::{
    HistorySuffix, SessionCommit, SessionIdentity, SessionMetadata, Store, StoredTranscriptBlock,
    TranscriptRecordSuffix, TranscriptSearchDirection,
};

const DEFAULT_SESSIONS: usize = 300;
const DEFAULT_TURNS: usize = 60;
const DEFAULT_BIG_TURNS: usize = 4000;
const DEFAULT_RUNS: usize = 20;
/// History items a resume reads as model context.
const RESUME_CONTEXT_ITEMS: u64 = 400;
/// Interactions a user waits on must stay below this at any store size.
const INTERACTION_CEILING: Duration = Duration::from_millis(100);
/// A save at the end of a long session may cost at most this many times one at
/// its start.
const SAVE_GROWTH_CEILING: f64 = 3.0;
const ABSENT: &str = "smelt-bench-absent-sentinel";

struct Options {
    state_dir: Option<PathBuf>,
    out: Option<PathBuf>,
    sessions: usize,
    turns: usize,
    big_turns: usize,
    runs: usize,
    mutate: bool,
    allow_debug: bool,
}

pub fn run(args: Vec<String>) {
    let options = parse_options(&args);
    if let Err(error) = run_benchmark(&options) {
        eprintln!("bench-store: {error}");
        std::process::exit(1);
    }
}

fn parse_options(args: &[String]) -> Options {
    let mut options = Options {
        state_dir: None,
        out: None,
        sessions: DEFAULT_SESSIONS,
        turns: DEFAULT_TURNS,
        big_turns: DEFAULT_BIG_TURNS,
        runs: DEFAULT_RUNS,
        mutate: false,
        allow_debug: false,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut count = |name: &str| -> usize {
            args.next()
                .and_then(|value| value.parse().ok())
                .filter(|value| *value > 0)
                .unwrap_or_else(|| usage_error(&format!("{name} requires a positive integer")))
        };
        match arg.as_str() {
            "--sessions" => options.sessions = count("--sessions"),
            "--turns" => options.turns = count("--turns"),
            "--big-turns" => options.big_turns = count("--big-turns"),
            "--runs" => options.runs = count("--runs"),
            "--state-dir" => {
                options.state_dir = Some(
                    args.next()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| usage_error("--state-dir requires a path")),
                );
            }
            "--out" => {
                options.out = Some(
                    args.next()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| usage_error("--out requires a path")),
                );
            }
            "--mutate" => options.mutate = true,
            "--allow-debug" => options.allow_debug = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => usage_error(&format!("unknown argument {other}")),
        }
    }
    options
}

fn usage_error(message: &str) -> ! {
    eprintln!("bench-store: {message}");
    print_usage();
    std::process::exit(2);
}

fn print_usage() {
    eprintln!(
        "usage: cargo run --release -p xtask -- bench-store [--sessions N] [--turns N] [--big-turns N] [--runs N] [--out DIR]"
    );
    eprintln!(
        "       cargo run --release -p xtask -- bench-store --state-dir PATH [--mutate] [--runs N]"
    );
    eprintln!("The first form builds a synthetic store in --out (default: a temporary directory).");
    eprintln!(
        "The second measures a copy of a real state directory; never point it at a live one."
    );
    eprintln!("Timing ceilings require a release build unless --allow-debug is passed.");
}

fn run_benchmark(options: &Options) -> Result<(), Box<dyn std::error::Error>> {
    if cfg!(debug_assertions) && !options.allow_debug {
        return Err("bench-store must run in release mode; use `cargo run --release -p xtask -- bench-store` or pass --allow-debug".into());
    }
    smelt_perf::perf::enable();
    let temp;
    let (store, synthetic) = match &options.state_dir {
        Some(dir) => {
            if dir.canonicalize().ok() == live_state_dir() {
                return Err("--state-dir is the live state directory; benchmark a copy".into());
            }
            (Store::new(dir), None)
        }
        None => {
            let root = match &options.out {
                Some(out) => {
                    if out.exists() {
                        return Err(format!("{} already exists", out.display()).into());
                    }
                    out.clone()
                }
                None => {
                    temp = tempfile::tempdir()?;
                    temp.path().to_path_buf()
                }
            };
            let store = Store::new(&root);
            let build = build_store(&store, options)?;
            (store, Some(build))
        }
    };
    let mut report = Report::default();
    if let Some(build) = &synthetic {
        report.add("build_seconds", build.elapsed.as_secs_f64());
        report.add("logical_bytes", build.logical_bytes as f64);
        let (early, late) = (&build.early_saves, &build.late_saves);
        report.add("save_early_p50_ms", ms(percentile(early, 50)));
        report.add("save_early_p95_ms", ms(percentile(early, 95)));
        report.add("save_late_p50_ms", ms(percentile(late, 50)));
        report.add("save_late_p95_ms", ms(percentile(late, 95)));
    }
    checkpoint(&store)?;
    measure_size(&store, &mut report)?;
    let big = largest_session(&store)?;
    measure_reads(&store, &big, options.runs, &mut report)?;
    measure_session_search(&store, synthetic.as_ref(), options.runs, &mut report)?;
    if synthetic.is_some() || options.mutate {
        measure_fork_and_delete(&store, &big, &mut report)?;
    }
    let started = Instant::now();
    let issues = store.integrity_issues()?;
    report.add("quick_check_ms", ms(started.elapsed()));
    if !issues.is_empty() {
        return Err(format!("integrity check failed: {issues:?}").into());
    }

    report.print();
    print_store_trace();
    report.check(synthetic.is_some())
}

#[derive(Default)]
struct Report {
    values: BTreeMap<&'static str, f64>,
    tables: BTreeMap<String, u64>,
}

impl Report {
    fn add(&mut self, name: &'static str, value: f64) {
        self.values.insert(name, value);
    }

    fn get(&self, name: &str) -> f64 {
        self.values.get(name).copied().unwrap_or_default()
    }

    fn print(&self) {
        for (name, value) in &self.values {
            eprintln!("{name:<36} {value:>14.3}");
        }
        for (table, bytes) in &self.tables {
            eprintln!("table {table:<30} {:>11.1} MiB", *bytes as f64 / 1048576.0);
        }
        println!(
            "STORE_BENCH_JSON {}",
            json!({ "values": self.values, "table_bytes": self.tables })
        );
    }

    /// Fails when an interaction a user waits on does not stay flat as the store grows.
    fn check(&self, synthetic: bool) -> Result<(), Box<dyn std::error::Error>> {
        let ceiling = ms(INTERACTION_CEILING);
        let mut failures = Vec::new();
        for name in [
            "resume_p95_ms",
            "transcript_search_hit_p95_ms",
            "session_search_rare_p95_ms",
            "session_search_common_p95_ms",
            "session_search_absent_p95_ms",
        ] {
            if self.get(name) > ceiling {
                failures.push(format!(
                    "{name} {:.1} ms exceeds {ceiling:.0} ms",
                    self.get(name)
                ));
            }
        }
        if synthetic {
            let growth = self.get("save_late_p95_ms") / self.get("save_early_p95_ms").max(0.001);
            if growth > SAVE_GROWTH_CEILING {
                failures.push(format!(
                    "a save late in a long session costs {growth:.1}x one early, above {SAVE_GROWTH_CEILING}x"
                ));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; ").into())
        }
    }
}

struct Build {
    elapsed: Duration,
    logical_bytes: u64,
    early_saves: Vec<Duration>,
    late_saves: Vec<Duration>,
    /// The session whose first message holds the rare search token.
    rare_session: String,
    rare_token: String,
}

/// A deterministic xorshift generator, so runs build the same store.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    /// Text from a small vocabulary, which compresses like source and logs do.
    fn words(&mut self, bytes: usize) -> String {
        const WORDS: [&str; 24] = [
            "the", "module", "returns", "error", "value", "let", "fn", "match", "config",
            "session", "store", "while", "index", "query", "render", "buffer", "=>", "{", "}",
            "self", "async", "await", "result", "path",
        ];
        let mut out = String::with_capacity(bytes + 16);
        while out.len() < bytes {
            out.push_str(WORDS[self.below(WORDS.len())]);
            out.push(if self.chance(12) { '\n' } else { ' ' });
        }
        out
    }

    /// Base64-like noise, which is how encrypted reasoning and images look.
    fn noise(&mut self, bytes: usize) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        (0..bytes)
            .map(|_| ALPHABET[self.below(ALPHABET.len())] as char)
            .collect()
    }
}

/// Writes one session turn by turn and returns how long each save took.
struct SessionBuilder<'a> {
    store: &'a Store,
    rng: &'a mut Rng,
    /// Outputs earlier turns produced, which later turns read again.
    outputs: Vec<String>,
    images: Vec<String>,
    logical_bytes: u64,
}

impl SessionBuilder<'_> {
    fn turn(
        &mut self,
        t: usize,
        first_message: &str,
    ) -> (Vec<HistoryItem>, Vec<StoredTranscriptBlock>) {
        let rng = &mut *self.rng;
        let user_text = if t == 0 {
            first_message.to_owned()
        } else {
            format!("turn {t}: {}", rng.words(120))
        };
        let user = if t % 40 == 39 {
            let image = if !self.images.is_empty() && rng.chance(50) {
                self.images[rng.below(self.images.len())].clone()
            } else {
                let image = format!("data:image/png;base64,{}", rng.noise(200_000));
                self.images.push(image.clone());
                image
            };
            Content::with_images(user_text.clone(), vec![("screenshot.png".into(), image)])
        } else {
            Content::text(user_text.clone())
        };
        let mut invocations = Vec::new();
        for call in 0..1 + rng.below(3) {
            let output = if !self.outputs.is_empty() && rng.chance(30) {
                self.outputs[rng.below(self.outputs.len())].clone()
            } else {
                let size = [600, 3_000, 12_000, 40_000][rng.below(4)];
                let output = rng.words(size);
                self.outputs.push(output.clone());
                output
            };
            invocations.push(ToolInvocation {
                call_id: format!("call-{t}-{call}"),
                name: "read_file".into(),
                arguments: json!({ "file_path": format!("/src/file_{}.rs", rng.below(500)) })
                    .to_string(),
                result: ToolOutcome::new(output, false, None),
                elapsed_ms: Some(12),
                called_at_ms: None,
                provider: None,
            });
        }
        let reply = rng.words(400);
        let reasoning_bytes = 2_000 + rng.below(10_000);
        let reasoning = ReasoningBlock {
            provider: ReasoningBlock::OPENAI_RESPONSES.into(),
            data: json!({ "type": "reasoning", "encrypted_content": rng.noise(reasoning_bytes) }),
        };
        let history = vec![
            HistoryItem::user(user),
            HistoryItem::assistant(AssistantStep::with_invocations(
                Some(Content::text(reply.clone())),
                Some(rng.words(200)),
                vec![reasoning],
                invocations.clone(),
            )),
        ];
        let mut records = vec![record(
            "user",
            &user_text,
            json!({ "User": { "text": user_text } }),
            None,
        )];
        for invocation in &invocations {
            let output = &invocation.result.content;
            records.push(record(
                "tool",
                output,
                json!({ "ToolCall": { "call_id": invocation.call_id, "name": invocation.name } }),
                Some(json!({ "status": "ok", "output": output })),
            ));
        }
        records.push(record(
            "assistant",
            &reply,
            json!({ "Text": { "content": reply } }),
            None,
        ));
        self.logical_bytes += history
            .iter()
            .map(|item| serde_json::to_vec(item).map_or(0, |json| json.len() as u64))
            .sum::<u64>();
        self.logical_bytes += records
            .iter()
            .map(|record| {
                (record.indexed_text.len()
                    + record.block_json.len()
                    + record.tool_state_json.as_ref().map_or(0, String::len)) as u64
            })
            .sum::<u64>();
        (history, records)
    }

    fn session(
        &mut self,
        id: &str,
        turns: usize,
        first_message: &str,
    ) -> Result<Vec<Duration>, smelt_store::StoreError> {
        let identity = SessionIdentity {
            id: id.to_owned(),
            created_at: 0,
            parent_id: None,
        };
        let mut writer = self.store.writer(id)?;
        let mut saves = Vec::with_capacity(turns);
        for t in 0..turns {
            let head = writer.head();
            let (items, records) = self.turn(t, first_message);
            let commit = SessionCommit {
                metadata: SessionMetadata {
                    title: Some(format!("bench session {id}")),
                    cwd: Some("/bench".into()),
                    updated_at: t as i64,
                    ..SessionMetadata::default()
                },
                first_user_message: (t == 0).then(|| Some(first_message.to_owned())),
                history: Some(HistorySuffix {
                    start: head.history_len.get(),
                    items,
                }),
                transcript: Some(TranscriptRecordSuffix {
                    start: head.transcript_record_count.get(),
                    records,
                }),
                ..SessionCommit::default()
            };
            let started = Instant::now();
            writer.commit(&identity, &commit)?;
            saves.push(started.elapsed());
        }
        Ok(saves)
    }
}

fn record(
    kind: &str,
    text: &str,
    block: Value,
    tool_state: Option<Value>,
) -> StoredTranscriptBlock {
    StoredTranscriptBlock {
        block_idx: 0,
        history_idx: None,
        kind: kind.to_owned(),
        tool_call_id: None,
        tool_name: None,
        content_hash: String::new(),
        estimated_text_bytes: text.len() as u64,
        summary: text.as_bytes()[..text.len().min(160)].to_vec(),
        indexed_text: text.to_owned(),
        block_json: block.to_string(),
        origin_json: None,
        tool_state_json: tool_state.map(|state| state.to_string()),
        tool_render_revision: 0,
    }
}

fn build_store(store: &Store, options: &Options) -> Result<Build, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut rng = Rng(0x5eed_5eed_5eed_5eed);
    let mut builder = SessionBuilder {
        store,
        rng: &mut rng,
        outputs: Vec::new(),
        images: Vec::new(),
        logical_bytes: 0,
    };
    let rare = options.sessions / 2;
    let rare_token = "quokka-on-rottnest".to_owned();
    for session in 0..options.sessions {
        let first = if session == rare {
            format!("where does the {rare_token} live?")
        } else {
            format!("session {session}: fix the module")
        };
        // Real sessions reread their own files, not each other's.
        builder.outputs.clear();
        builder.session(&format!("{session:064x}"), options.turns, &first)?;
        if session % 50 == 49 {
            eprintln!(
                "bench-store: built {}/{} sessions",
                session + 1,
                options.sessions
            );
        }
    }
    builder.outputs.clear();
    let saves = builder.session(&"b".repeat(64), options.big_turns, "a very long session")?;
    let tenth = (saves.len() / 10).max(1);
    Ok(Build {
        elapsed: started.elapsed(),
        logical_bytes: builder.logical_bytes,
        early_saves: saves[..tenth].to_vec(),
        late_saves: saves[saves.len() - tenth..].to_vec(),
        rare_session: format!("{rare:064x}"),
        rare_token,
    })
}

fn checkpoint(store: &Store) -> rusqlite::Result<()> {
    let conn = rusqlite::Connection::open(store.database_path())?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
}

fn measure_size(store: &Store, report: &mut Report) -> Result<(), Box<dyn std::error::Error>> {
    let path = store.database_path();
    report.add(
        "database_mib",
        std::fs::metadata(&path)?.len() as f64 / 1048576.0,
    );
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT name, sum(pgsize) FROM dbstat GROUP BY name")?;
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (name, bytes) = row?;
        report.tables.insert(name, bytes as u64);
    }
    let count = |sql: &str| -> rusqlite::Result<f64> {
        conn.query_row(sql, [], |row| row.get::<_, i64>(0))
            .map(|n| n as f64)
    };
    report.add("sessions", count("SELECT count(*) FROM sessions")?);
    report.add("history_rows", count("SELECT count(*) FROM history")?);
    report.add("transcript_rows", count("SELECT count(*) FROM transcript")?);
    report.add("blobs", count("SELECT count(*) FROM blobs")?);
    report.add("blob_refs", count("SELECT count(*) FROM blob_refs")?);
    Ok(())
}

fn largest_session(store: &Store) -> Result<String, Box<dyn std::error::Error>> {
    let conn = rusqlite::Connection::open_with_flags(
        store.database_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    Ok(conn.query_row(
        "SELECT id FROM sessions ORDER BY record_count DESC LIMIT 1",
        [],
        |row| row.get(0),
    )?)
}

fn measure_reads(
    store: &Store,
    id: &str,
    runs: usize,
    report: &mut Report,
) -> Result<(), Box<dyn std::error::Error>> {
    let reader = store.reader(id)?;
    let head = reader.head()?;
    report.add(
        "largest_session_records",
        head.transcript_record_count.get() as f64,
    );
    report.add("largest_session_history", head.history_len.get() as f64);
    let len = head.history_len.get();
    let records = head.transcript_record_count.as_usize();
    let (resume, peak) = timed_with_peak(runs, || {
        reader.read_snapshot(|reader| {
            reader.state()?;
            let context = reader.history_range(len.saturating_sub(RESUME_CONTEXT_ITEMS), len)?;
            let tail = reader.transcript_tail_for_rows_with_total(records, 120, 48)?;
            Ok((context.len(), tail.len()))
        })
    })?;
    report.add("resume_p50_ms", ms(percentile(&resume, 50)));
    report.add("resume_p95_ms", ms(percentile(&resume, 95)));
    report.add("resume_peak_mib", peak as f64 / 1048576.0);

    let reader = &reader;
    let search = |query: &'static str| {
        move || {
            reader.search_transcript_candidate_page(
                query,
                None,
                TranscriptSearchDirection::Backward,
                128,
            )
        }
    };
    let (hit, _) = timed_with_peak(runs, search("the"))?;
    report.add("transcript_search_hit_p95_ms", ms(percentile(&hit, 95)));
    let (miss, _) = timed_with_peak(runs.min(5), search(ABSENT))?;
    report.add(
        "transcript_search_full_miss_p50_ms",
        ms(percentile(&miss, 50)),
    );

    let (text, _) = timed_with_peak(runs, || reader.conversation_text())?;
    report.add("conversation_text_p95_ms", ms(percentile(&text, 95)));

    let started = Instant::now();
    let full = reader.history_range(0, len)?;
    report.add("full_history_read_ms", ms(started.elapsed()));
    drop(full);
    Ok(())
}

fn measure_session_search(
    store: &Store,
    build: Option<&Build>,
    runs: usize,
    report: &mut Report,
) -> Result<(), Box<dyn std::error::Error>> {
    let rare = build.map_or("rottnest", |build| build.rare_token.as_str());
    for (name, query) in [
        ("session_search_rare_p95_ms", rare),
        ("session_search_common_p95_ms", "module"),
        ("session_search_absent_p95_ms", ABSENT),
    ] {
        let (times, _) = timed_with_peak(runs, || store.search_sessions(query))?;
        report.add(name, ms(percentile(&times, 95)));
    }
    if let Some(build) = build {
        let found = store.search_sessions(&build.rare_token.to_uppercase())?;
        if found != [build.rare_session.clone()] {
            return Err(format!("rare search found {found:?}").into());
        }
        if !store.search_sessions(ABSENT)?.is_empty() {
            return Err("absent search found sessions".into());
        }
    }
    Ok(())
}

fn measure_fork_and_delete(
    store: &Store,
    id: &str,
    report: &mut Report,
) -> Result<(), Box<dyn std::error::Error>> {
    let fork = "f".repeat(64);
    let size = || std::fs::metadata(store.database_path()).map_or(0, |meta| meta.len());
    let before = size();
    let started = Instant::now();
    store.fork(id, &fork, 0)?;
    report.add("fork_largest_ms", ms(started.elapsed()));
    checkpoint(store)?;
    report.add(
        "fork_growth_mib",
        size().saturating_sub(before) as f64 / 1048576.0,
    );
    let started = Instant::now();
    store.delete(&fork)?;
    report.add("delete_fork_ms", ms(started.elapsed()));
    Ok(())
}

fn timed_with_peak<T, E: Into<Box<dyn std::error::Error>>>(
    runs: usize,
    mut run: impl FnMut() -> Result<T, E>,
) -> Result<(Vec<Duration>, usize), Box<dyn std::error::Error>> {
    run().map_err(Into::into)?;
    let mut times = Vec::with_capacity(runs);
    let mut peak = 0;
    for _ in 0..runs {
        let measurement = smelt_perf::alloc::begin_peak_measurement();
        let baseline = measurement.start_bytes();
        let started = Instant::now();
        let result = run().map_err(Into::into)?;
        times.push(started.elapsed());
        drop(result);
        peak = peak.max(measurement.finish().saturating_sub(baseline));
    }
    Ok((times, peak))
}

fn percentile(times: &[Duration], percent: usize) -> Duration {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    sorted
        .get((sorted.len() * percent).div_ceil(100).saturating_sub(1))
        .copied()
        .unwrap_or_default()
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The store's own spans and counters, recorded while the benchmark ran.
fn print_store_trace() {
    let snapshot = smelt_perf::perf::snapshot();
    for row in snapshot
        .durations
        .iter()
        .filter(|row| row.label.starts_with("store:"))
    {
        eprintln!(
            "trace {:<34} count={:<8} p50={}us p95={}us max={}us total={}ms",
            row.label,
            row.count,
            row.p50_us,
            row.p95_us,
            row.max_us,
            row.total_us / 1000
        );
    }
    for row in snapshot
        .values
        .iter()
        .filter(|row| row.label.starts_with("store:"))
    {
        eprintln!(
            "trace {:<34} count={:<8} p50={} p95={} total={}",
            row.label, row.count, row.p50, row.p95, row.total
        );
    }
}

/// The state directory a running smelt uses, which a benchmark must not touch.
fn live_state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/state")))?;
    base.join("smelt").canonicalize().ok()
}
