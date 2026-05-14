use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl Level {
    pub fn enabled(self) -> bool {
        self as u8 >= LOG_LEVEL.load(Ordering::Relaxed)
    }
}

pub fn set_level(level: Level) {
    LOG_LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn parse_level(s: &str) -> Option<Level> {
    match s.trim().to_lowercase().as_str() {
        "debug" => Some(Level::Debug),
        "info" => Some(Level::Info),
        "warn" | "warning" => Some(Level::Warn),
        "error" => Some(Level::Error),
        _ => None,
    }
}

const MAX_LOG_FILES: usize = 20;

fn log_path() -> &'static PathBuf {
    LOG_PATH.get_or_init(|| {
        let dir = dirs();
        let _ = fs::create_dir_all(&dir);
        rotate_logs(&dir);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        dir.join(format!("{ts}-{}.jsonl", std::process::id()))
    })
}

fn rotate_logs(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<_> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            Some((name, e.path()))
        })
        .collect();
    if logs.len() <= MAX_LOG_FILES {
        return;
    }
    logs.sort_by(|a, b| a.0.cmp(&b.0));
    let to_remove = logs.len() - MAX_LOG_FILES;
    for (_, path) in &logs[..to_remove] {
        let _ = fs::remove_file(path);
    }
}

fn dirs() -> PathBuf {
    crate::paths::state_dir().join("logs")
}

pub fn logs_dir() -> PathBuf {
    let dir = dirs();
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn entry(level: Level, event: &str, data: &impl Serialize) {
    if !level.enabled() {
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let payload = serde_json::json!({
        "ts": ts,
        "level": format!("{:?}", level).to_lowercase(),
        "event": event,
        "data": data,
    });

    let Ok(line) = serde_json::to_string(&payload) else {
        return;
    };

    let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    else {
        return;
    };

    let _ = writeln!(f, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_level ----

    #[test]
    fn parse_level_matches_each_keyword_case_insensitively() {
        assert_eq!(parse_level("debug"), Some(Level::Debug));
        assert_eq!(parse_level("INFO"), Some(Level::Info));
        assert_eq!(parse_level("Warn"), Some(Level::Warn));
        assert_eq!(parse_level("warning"), Some(Level::Warn));
        assert_eq!(parse_level("error"), Some(Level::Error));
    }

    #[test]
    fn parse_level_trims_whitespace() {
        assert_eq!(parse_level("  info  "), Some(Level::Info));
    }

    #[test]
    fn parse_level_returns_none_for_unknown_values() {
        assert_eq!(parse_level(""), None);
        assert_eq!(parse_level("verbose"), None);
        assert_eq!(parse_level("???"), None);
    }

    // ---- Level ordering / enabled ----

    #[test]
    fn level_enabled_obeys_atomic_threshold() {
        let prev = LOG_LEVEL.load(Ordering::Relaxed);
        set_level(Level::Warn);
        assert!(!Level::Debug.enabled());
        assert!(!Level::Info.enabled());
        assert!(Level::Warn.enabled());
        assert!(Level::Error.enabled());
        LOG_LEVEL.store(prev, Ordering::Relaxed);
    }

    #[test]
    fn set_level_round_trips_through_load() {
        let prev = LOG_LEVEL.load(Ordering::Relaxed);
        set_level(Level::Debug);
        assert_eq!(LOG_LEVEL.load(Ordering::Relaxed), Level::Debug as u8);
        set_level(Level::Error);
        assert_eq!(LOG_LEVEL.load(Ordering::Relaxed), Level::Error as u8);
        LOG_LEVEL.store(prev, Ordering::Relaxed);
    }
}
