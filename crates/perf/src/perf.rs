//! Labelled scope guards plus a pretty-printer.
//!
//! Wrap a scope with [`begin`] (RAII guard); on drop it appends a sample to a per-label ring
//! (capacity [`RING_CAPACITY`]). When [`crate::alloc::enabled`] is also true, allocs that happened
//! on the same thread between begin and drop are recorded alongside the duration. Use
//! [`record_value`] for non-duration metrics and [`begin_value_ms`] for elapsed-millisecond value
//! samples. [`snapshot`] returns sortable rows; [`print_summary`] emits a colored ANSI table.

use crate::alloc;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Max retained samples per label. Oldest sample is dropped when the buffer fills.
pub const RING_CAPACITY: usize = 1024;

#[derive(Clone, Debug)]
struct SampleRing<T> {
    values: VecDeque<T>,
}

impl<T> Default for SampleRing<T> {
    fn default() -> Self {
        Self {
            values: VecDeque::with_capacity(RING_CAPACITY),
        }
    }
}

impl<T> SampleRing<T> {
    fn push(&mut self, value: T) {
        if self.values.len() == RING_CAPACITY {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.values.iter()
    }

    fn last(&self) -> Option<&T> {
        self.values.back()
    }
}

fn samples() -> &'static Mutex<HashMap<&'static str, SampleRing<Duration>>> {
    static S: OnceLock<Mutex<HashMap<&'static str, SampleRing<Duration>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn value_samples() -> &'static Mutex<HashMap<&'static str, SampleRing<u64>>> {
    static V: OnceLock<Mutex<HashMap<&'static str, SampleRing<u64>>>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(HashMap::new()))
}

type AllocSamples = Mutex<HashMap<&'static str, SampleRing<(u64, u64)>>>;

fn alloc_samples() -> &'static AllocSamples {
    static A: OnceLock<AllocSamples> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Process-local monotonic timestamp for ordering instrumentation events.
pub fn timestamp_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Record a raw numeric sample (byte count, cache size, etc.) under `label`.
pub fn record_value(label: &'static str, value: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut m) = value_samples().lock() {
        m.entry(label).or_default().push(value);
    }
}

/// Start a scope guard that records elapsed milliseconds as a value sample.
pub fn begin_value_ms(label: &'static str) -> Option<ValueTimer> {
    enabled().then(|| ValueTimer {
        label,
        start: Instant::now(),
    })
}

pub struct ValueTimer {
    label: &'static str,
    start: Instant,
}

impl Drop for ValueTimer {
    fn drop(&mut self) {
        record_value(
            self.label,
            self.start
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        );
    }
}

/// Drop all retained samples.
pub fn clear() {
    if let Ok(mut s) = samples().lock() {
        s.clear();
    }
    if let Ok(mut a) = alloc_samples().lock() {
        a.clear();
    }
    if let Ok(mut v) = value_samples().lock() {
        v.clear();
    }
}

#[derive(Debug, Clone)]
pub struct DurationRow {
    pub label: &'static str,
    pub count: usize,
    pub last_us: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub total_us: u64,
}

#[derive(Debug, Clone)]
pub struct ValueRow {
    pub label: &'static str,
    pub count: usize,
    pub last: u64,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
    pub total: u64,
}

#[derive(Debug, Clone)]
pub struct AllocRow {
    pub label: &'static str,
    pub count: usize,
    pub allocs_last: u64,
    pub allocs_total: u64,
    pub bytes_last: u64,
    pub bytes_total: u64,
    pub bytes_p95: u64,
    pub bytes_max: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub durations: Vec<DurationRow>,
    pub values: Vec<ValueRow>,
    pub allocs: Vec<AllocRow>,
}

#[derive(Debug, Clone, Copy)]
struct DurationSeed {
    label: &'static str,
    count: usize,
    last_us: u64,
    total_us: u64,
}

fn pct_idx(count: usize, p: usize) -> usize {
    ((count * p) / 100).min(count.saturating_sub(1))
}

fn duration_seed(label: &'static str, durs: &SampleRing<Duration>) -> Option<DurationSeed> {
    if durs.is_empty() {
        return None;
    }
    Some(DurationSeed {
        label,
        count: durs.len(),
        last_us: durs.last().map(|d| d.as_micros() as u64).unwrap_or(0),
        total_us: durs.iter().map(|d| d.as_micros() as u64).sum(),
    })
}

fn duration_row(seed: DurationSeed, durs: &SampleRing<Duration>) -> Option<DurationRow> {
    if durs.is_empty() {
        return None;
    }
    let mut sorted: Vec<u64> = durs.iter().map(|d| d.as_micros() as u64).collect();
    sorted.sort_unstable();
    Some(DurationRow {
        label: seed.label,
        count: seed.count,
        last_us: seed.last_us,
        p50_us: sorted[pct_idx(sorted.len(), 50)],
        p95_us: sorted[pct_idx(sorted.len(), 95)],
        p99_us: sorted[pct_idx(sorted.len(), 99)],
        max_us: *sorted.last().unwrap(),
        total_us: seed.total_us,
    })
}

fn value_row(label: &'static str, vs: &SampleRing<u64>) -> Option<ValueRow> {
    if vs.is_empty() {
        return None;
    }
    let mut sorted = vs.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    Some(ValueRow {
        label,
        count: sorted.len(),
        last: vs.last().copied().unwrap_or(0),
        p50: sorted[pct_idx(sorted.len(), 50)],
        p95: sorted[pct_idx(sorted.len(), 95)],
        p99: sorted[pct_idx(sorted.len(), 99)],
        max: *sorted.last().unwrap(),
        total: sorted.iter().sum(),
    })
}

fn alloc_row(label: &'static str, samples: &SampleRing<(u64, u64)>) -> Option<AllocRow> {
    if samples.is_empty() {
        return None;
    }
    let mut bytes = samples.iter().map(|(_, bytes)| *bytes).collect::<Vec<_>>();
    bytes.sort_unstable();
    Some(AllocRow {
        label,
        count: samples.len(),
        allocs_last: samples.last().map(|(allocs, _)| *allocs).unwrap_or(0),
        allocs_total: samples.iter().map(|(allocs, _)| *allocs).sum(),
        bytes_last: samples.last().map(|(_, bytes)| *bytes).unwrap_or(0),
        bytes_total: samples.iter().map(|(_, bytes)| *bytes).sum(),
        bytes_p95: bytes[pct_idx(bytes.len(), 95)],
        bytes_max: *bytes.last().unwrap(),
    })
}

/// Snapshot current sample buffers. Durations sorted by total descending; values and allocs by label.
pub fn snapshot() -> Snapshot {
    let _perf = begin("perf:snapshot");
    let mut out = Snapshot::default();
    if let Ok(map) = samples().lock() {
        for (label, durs) in map.iter() {
            let Some(seed) = duration_seed(label, durs) else {
                continue;
            };
            if let Some(row) = duration_row(seed, durs) {
                out.durations.push(row);
            }
        }
    }
    out.durations.sort_by_key(|r| std::cmp::Reverse(r.total_us));

    if let Ok(map) = value_samples().lock() {
        for (label, vs) in map.iter() {
            if let Some(row) = value_row(label, vs) {
                out.values.push(row);
            }
        }
    }
    out.values.sort_by(|a, b| a.label.cmp(b.label));

    if let Ok(map) = alloc_samples().lock() {
        for (label, samples) in map.iter() {
            if let Some(row) = alloc_row(label, samples) {
                out.allocs.push(row);
            }
        }
    }
    out.allocs.sort_by(|a, b| a.label.cmp(b.label));
    out
}

/// Cheap live-panel snapshot of the top duration rows. Omits value rows and only
/// computes percentiles for labels that will be displayed.
pub fn snapshot_top(limit: usize) -> Snapshot {
    let _perf = begin("perf:snapshot_top");
    let mut out = Snapshot::default();
    if limit == 0 {
        return out;
    }
    if let Ok(map) = samples().lock() {
        let mut seeds = map
            .iter()
            .filter_map(|(label, durs)| duration_seed(label, durs))
            .collect::<Vec<_>>();
        seeds.sort_by_key(|seed| std::cmp::Reverse(seed.total_us));
        seeds.truncate(limit);
        for seed in seeds {
            let Some(durs) = map.get(seed.label) else {
                continue;
            };
            if let Some(row) = duration_row(seed, durs) {
                out.durations.push(row);
            }
        }
    }
    out
}

/// RAII guard that records a self-time (and allocation delta when enabled) for `label` on drop.
pub fn begin(label: &'static str) -> Option<Guard> {
    if !enabled() {
        return None;
    }
    Some(Guard {
        label,
        start: Instant::now(),
        allocs_start: alloc::thread_snapshot(),
    })
}

pub struct Guard {
    label: &'static str,
    start: Instant,
    allocs_start: (u64, u64),
}

impl Drop for Guard {
    fn drop(&mut self) {
        let dur = self.start.elapsed();
        if let Ok(mut s) = samples().lock() {
            s.entry(self.label).or_default().push(dur);
        }
        if alloc::enabled() {
            let (c1, b1) = alloc::thread_snapshot();
            let (c0, b0) = self.allocs_start;
            if let Ok(mut m) = alloc_samples().lock() {
                m.entry(self.label)
                    .or_default()
                    .push((c1.saturating_sub(c0), b1.saturating_sub(b0)));
            }
        }
    }
}

const TABLE_WIDTH: usize = 115;

/// Print a summary table of all recorded timings to stdout.
pub fn print_summary() {
    if !enabled() {
        return;
    }
    let map = samples().lock().unwrap();
    if map.is_empty() {
        return;
    }
    let mut groups: Vec<(&'static str, Vec<Duration>)> = map
        .iter()
        .map(|(label, values)| (*label, values.iter().copied().collect()))
        .collect();
    drop(map);
    groups.sort_by(|a, b| {
        let ta: Duration = a.1.iter().sum();
        let tb: Duration = b.1.iter().sum();
        tb.cmp(&ta)
    });
    let max_total: Duration = groups
        .iter()
        .map(|(_, ds)| ds.iter().sum::<Duration>())
        .max()
        .unwrap_or_default();

    let bar = "─".repeat(TABLE_WIDTH);
    let title = "── bench ";
    let title_bar = format!(
        "{}{}",
        title,
        "─".repeat(TABLE_WIDTH - title.chars().count())
    );
    println!("\n{}", title_bar);
    print_header("function", &bar);
    for (label, mut durs) in groups {
        durs.sort();
        let total: Duration = durs.iter().sum();
        let avg = total / durs.len() as u32;
        let row = format_row(label, &durs, total, avg, fmt_dur);
        println!("{}", colorize_row(&row, total, max_total));
    }
    println!("{}", bar);

    let alloc_map = alloc_samples().lock().unwrap();
    if !alloc_map.is_empty() {
        let mut agroups: Vec<(&'static str, Vec<(u64, u64)>)> = alloc_map
            .iter()
            .map(|(label, values)| (*label, values.iter().copied().collect()))
            .collect();
        drop(alloc_map);
        agroups.sort_by(|a, b| {
            let ta: u64 = a.1.iter().map(|(_, b)| *b).sum();
            let tb: u64 = b.1.iter().map(|(_, b)| *b).sum();
            tb.cmp(&ta)
        });
        print_header("allocs", &bar);
        for (label, samples) in agroups {
            let mut counts: Vec<u64> = samples.iter().map(|(c, _)| *c).collect();
            let mut bytes: Vec<u64> = samples.iter().map(|(_, b)| *b).collect();
            counts.sort();
            bytes.sort();
            let total_bytes: u64 = bytes.iter().sum();
            let avg_bytes = total_bytes / bytes.len() as u64;
            let total_count: u64 = counts.iter().sum();
            let avg_count = total_count / counts.len() as u64;
            println!(
                "{}",
                format_row(
                    &format!("{label}  (n)"),
                    &counts,
                    total_count,
                    avg_count,
                    |v| v.to_string(),
                )
            );
            println!(
                "{}",
                format_row(
                    &format!("{label}  (bytes)"),
                    &bytes,
                    total_bytes,
                    avg_bytes,
                    fmt_bytes,
                )
            );
        }
        println!("{}", bar);
    } else {
        drop(alloc_map);
    }

    let value_map = value_samples().lock().unwrap();
    if !value_map.is_empty() {
        let mut vgroups: Vec<(&'static str, Vec<u64>)> = value_map
            .iter()
            .map(|(label, values)| (*label, values.iter().copied().collect()))
            .collect();
        drop(value_map);
        vgroups.sort_by_key(|(k, _)| *k);
        print_header("value", &bar);
        for (label, mut vs) in vgroups {
            vs.sort();
            let total: u64 = vs.iter().sum();
            let avg = total / vs.len() as u64;
            println!("{}", format_row(label, &vs, total, avg, fmt_bytes));
        }
        println!("{}", bar);
    }
}

fn print_header(first: &str, bar: &str) {
    println!(
        "{:<40} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        first, "count", "total", "avg", "p50", "p95", "p99", "max"
    );
    println!("{}", bar);
}

/// Format one row. `samples` must be sorted ascending.
fn format_row<T, F>(label: &str, samples: &[T], total: T, avg: T, fmt: F) -> String
where
    T: Copy,
    F: Fn(T) -> String,
{
    let count = samples.len();
    let pct = |p: usize| -> T {
        let idx = ((count * p) / 100).min(count - 1);
        samples[idx]
    };
    let max = *samples.last().unwrap();
    format!(
        "{:<40} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        label,
        count,
        fmt(total),
        fmt(avg),
        fmt(pct(50)),
        fmt(pct(95)),
        fmt(pct(99)),
        fmt(max),
    )
}

fn colorize_row(row: &str, total: Duration, max_total: Duration) -> String {
    let code = severity_color(total, max_total);
    format!("\x1b[{}m{}\x1b[0m", code, row)
}

/// Map a `(total, max_total)` pair to an ANSI SGR color code using a log scale.
fn severity_color(total: Duration, max_total: Duration) -> &'static str {
    let t = total.as_secs_f64();
    let m = max_total.as_secs_f64().max(1e-9);
    // log(1 + x*1000) avoids -inf at zero and compresses the range.
    let ratio = (1.0 + t * 1000.0).ln() / (1.0 + m * 1000.0).ln();
    let ratio = ratio.clamp(0.0, 1.0);

    match ratio {
        r if r >= 0.85 => "1;91", // bold bright red
        r if r >= 0.65 => "91",   // bright red
        r if r >= 0.45 => "33",   // yellow
        r if r >= 0.25 => "36",   // cyan
        r if r >= 0.10 => "37",   // white
        _ => "2;37",              // dim
    }
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1_000 {
        format!("{}µs", us)
    } else if us < 1_000_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

fn fmt_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{}B", n)
    } else if n < 1024 * 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{:.2}MB", n as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    #[test]
    fn sample_ring_evicts_oldest_in_constant_time_order() {
        let mut ring = SampleRing::default();
        for value in 0..RING_CAPACITY + 3 {
            ring.push(value);
        }

        assert_eq!(ring.len(), RING_CAPACITY);
        assert_eq!(ring.iter().copied().next(), Some(3));
        assert_eq!(ring.last(), Some(&(RING_CAPACITY + 2)));
    }

    #[test]
    fn value_timer_records_when_its_scope_exits() {
        let _lock = test_lock();
        set_enabled(true);
        clear();
        {
            let _timer = begin_value_ms("test:value_timer_ms");
        }
        let row = snapshot()
            .values
            .into_iter()
            .find(|row| row.label == "test:value_timer_ms")
            .expect("elapsed value sample");
        assert_eq!(row.count, 1);
    }

    #[test]
    fn concurrent_recording_stays_bounded() {
        let _lock = test_lock();
        set_enabled(true);
        clear();
        let threads = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for value in 0..RING_CAPACITY {
                        record_value("test:concurrent", value as u64);
                    }
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("recorder thread");
        }

        let row = snapshot()
            .values
            .into_iter()
            .find(|row| row.label == "test:concurrent")
            .expect("concurrent samples");
        assert_eq!(row.count, RING_CAPACITY);
    }
}
