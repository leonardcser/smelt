//! Disk-backed TTL cache for HTTP responses.
//! Files: `<cache_dir>/web/<hash>`, format: `<expires_unix_secs>\n<body>`.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

fn cache_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("web")
}

fn key_path(cache_root: &Path, key: &str) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = hasher.finish();
    cache_dir(cache_root).join(format!("{hash:x}"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Return the cached body for `key`, or `None` if missing or expired (expired entries deleted).
pub(crate) fn get(cache_root: &Path, key: &str) -> Option<String> {
    let path = key_path(cache_root, key);
    let contents = std::fs::read_to_string(&path).ok()?;
    let (first_line, rest) = contents.split_once('\n')?;
    let expires: u64 = first_line.parse().ok()?;
    if now_secs() > expires {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(rest.to_string())
}

/// Cache `value` under `key` with the default TTL (15 minutes).
pub(crate) fn put(cache_root: &Path, key: &str, value: &str) {
    put_with_ttl(cache_root, key, value, DEFAULT_TTL);
}

/// Cache `value` under `key` with an explicit TTL.
pub(crate) fn put_with_ttl(cache_root: &Path, key: &str, value: &str, ttl: Duration) {
    let path = key_path(cache_root, key);
    let expires = now_secs() + ttl.as_secs();
    let data = format!("{expires}\n{value}");
    let _ = crate::fs::write_atomic(&path, data.as_bytes());
}
