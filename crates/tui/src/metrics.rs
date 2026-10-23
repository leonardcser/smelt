use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const DAY_MS: u64 = 86_400_000;
/// Ledger size that triggers rolling old entries up into daily totals.
const ROLL_UP_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MetricsEntry {
    pub(crate) timestamp_ms: u64,
    /// Calls summarized by this entry. Entries older than yesterday are rolled
    /// up into one entry per UTC day and model.
    #[serde(default = "one_call", skip_serializing_if = "is_one_call")]
    pub(crate) calls: u64,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) cost_usd: Option<f64>,
    #[serde(default)]
    pub(crate) cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) cache_write_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) reasoning_tokens: Option<u64>,
}

fn one_call() -> u64 {
    1
}

fn is_one_call(calls: &u64) -> bool {
    *calls == 1
}

fn metrics_path(state_root: &Path) -> PathBuf {
    state_root.join("metrics.jsonl")
}

fn lock_path(state_root: &Path) -> PathBuf {
    state_root.join("metrics.lock")
}

fn open_lock(state_root: &Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(state_root))
        .ok()
}

/// Append one entry to the metrics JSONL file, rolling old entries up once the
/// ledger grows past [`ROLL_UP_BYTES`].
pub(crate) fn append(state_root: &Path, entry: &MetricsEntry) {
    let _ = std::fs::create_dir_all(state_root);
    let Some(lock) = open_lock(state_root) else {
        return;
    };
    let path = metrics_path(state_root);
    let size = {
        if lock.lock_shared().is_err() {
            return;
        }
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        else {
            return;
        };
        if let Ok(line) = serde_json::to_string(entry) {
            let _ = writeln!(f, "{line}");
        }
        let size = f.metadata().map_or(0, |meta| meta.len());
        let _ = lock.unlock();
        size
    };
    if size > ROLL_UP_BYTES {
        let now_ms = entry.timestamp_ms;
        let _ = roll_up(state_root, &lock, now_ms);
    }
}

/// Rewrites the ledger with every entry before the start of yesterday folded
/// into one entry per UTC day and model. Totals, calls, daily activity and the
/// last 24 hours stay exact.
fn roll_up(state_root: &Path, lock: &std::fs::File, now_ms: u64) -> std::io::Result<()> {
    lock.lock()?;
    let path = metrics_path(state_root);
    if std::fs::metadata(&path)?.len() <= ROLL_UP_BYTES {
        return lock.unlock();
    }
    let cutoff = (now_ms / DAY_MS).saturating_sub(1) * DAY_MS;
    let mut days: BTreeMap<(u64, String), MetricsEntry> = BTreeMap::new();
    let mut recent = Vec::new();
    for entry in load(state_root) {
        if entry.timestamp_ms >= cutoff {
            recent.push(entry);
            continue;
        }
        let day = entry.timestamp_ms / DAY_MS * DAY_MS;
        let total = days
            .entry((day, entry.model.clone()))
            .or_insert_with(|| MetricsEntry {
                timestamp_ms: day,
                calls: 0,
                prompt_tokens: 0,
                completion_tokens: 0,
                model: entry.model.clone(),
                cost_usd: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            });
        total.calls += entry.calls;
        total.prompt_tokens += entry.prompt_tokens;
        total.completion_tokens += entry.completion_tokens;
        add(&mut total.cost_usd, entry.cost_usd);
        add(&mut total.cache_read_tokens, entry.cache_read_tokens);
        add(&mut total.cache_write_tokens, entry.cache_write_tokens);
        add(&mut total.reasoning_tokens, entry.reasoning_tokens);
    }
    // The exclusive lock also owns the staging path.
    let staged = path.with_extension("jsonl.tmp");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&staged)?);
    for entry in days.values().chain(&recent) {
        serde_json::to_writer(&mut out, entry)?;
        out.write_all(b"\n")?;
    }
    out.into_inner()
        .map_err(|error| error.into_error())?
        .sync_all()?;
    std::fs::rename(&staged, &path)?;
    lock.unlock()
}

fn add<T: std::ops::Add<Output = T> + Copy>(total: &mut Option<T>, value: Option<T>) {
    if let Some(value) = value {
        *total = Some(total.map_or(value, |total| total + value));
    }
}

/// Load all entries from the metrics file.
pub(crate) fn load(state_root: &Path) -> Vec<MetricsEntry> {
    let path = metrics_path(state_root);
    let Ok(f) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    std::io::BufReader::new(f)
        .lines()
        .filter_map(|line| {
            let line = line.ok()?;
            serde_json::from_str(&line).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(timestamp_ms: u64, model: &str, prompt: u64, cost: Option<f64>) -> MetricsEntry {
        MetricsEntry {
            timestamp_ms,
            calls: 1,
            prompt_tokens: prompt,
            completion_tokens: 1,
            model: model.into(),
            cost_usd: cost,
            cache_read_tokens: None,
            cache_write_tokens: Some(2),
            reasoning_tokens: None,
        }
    }

    #[test]
    fn roll_up_keeps_totals_calls_days_and_the_last_day_exact() {
        let dir = tempfile::tempdir().unwrap();
        let now = 100 * DAY_MS + 5;
        let mut line = serde_json::to_string(&entry(0, "pad", 0, None)).unwrap();
        line.push('\n');
        let old = [
            entry(90 * DAY_MS + 1, "a", 10, Some(0.5)),
            entry(90 * DAY_MS + 2, "a", 20, None),
            entry(90 * DAY_MS + 3, "b", 30, Some(1.0)),
            entry(91 * DAY_MS, "a", 40, Some(0.25)),
        ];
        let recent = [
            entry(99 * DAY_MS + 7, "a", 50, None),
            entry(now, "a", 60, None),
        ];
        let mut ledger = String::new();
        for e in old.iter().chain(&recent[..1]) {
            ledger.push_str(&serde_json::to_string(e).unwrap());
            ledger.push('\n');
        }
        let mut pads = 0;
        while (ledger.len() as u64) <= ROLL_UP_BYTES {
            ledger.push_str(&line);
            pads += 1;
        }
        std::fs::write(metrics_path(dir.path()), ledger).unwrap();

        append(dir.path(), &recent[1]);
        let entries = load(dir.path());

        let mut padding = entry(0, "pad", 0, None);
        padding.calls = pads;
        padding.completion_tokens = padding.calls;
        padding.cache_write_tokens = Some(2 * padding.calls);
        let mut day_a = entry(90 * DAY_MS, "a", 30, Some(0.5));
        day_a.calls = 2;
        day_a.completion_tokens = 2;
        day_a.cache_write_tokens = Some(4);
        assert_eq!(
            entries,
            vec![
                padding,
                day_a,
                entry(90 * DAY_MS, "b", 30, Some(1.0)),
                entry(91 * DAY_MS, "a", 40, Some(0.25)),
                recent[0].clone(),
                recent[1].clone(),
            ]
        );
        assert!(std::fs::metadata(metrics_path(dir.path())).unwrap().len() < 4096);
    }

    #[test]
    fn single_calls_serialize_without_a_call_count() {
        let line = serde_json::to_string(&entry(1, "a", 1, None)).unwrap();
        assert!(!line.contains("calls"));
        let parsed: MetricsEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.calls, 1);
    }
}
