use serde::{Deserialize, Serialize};
use smelt_core::config;
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MetricsEntry {
    pub(crate) timestamp_ms: u64,
    pub(crate) prompt_tokens: u32,
    pub(crate) completion_tokens: u32,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) cost_usd: Option<f64>,
    #[serde(default)]
    pub(crate) cache_read_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_write_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) reasoning_tokens: Option<u32>,
}

fn metrics_path() -> PathBuf {
    config::state_dir().join("metrics.jsonl")
}

/// Append one entry to the metrics JSONL file.
pub(crate) fn append(entry: &MetricsEntry) {
    let path = metrics_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
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
}

/// Load all entries from the metrics file.
pub(crate) fn load() -> Vec<MetricsEntry> {
    let path = metrics_path();
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
