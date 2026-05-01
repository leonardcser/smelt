use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

fn cache_dir() -> PathBuf {
    crate::paths::cache_dir().join("web")
}

fn key_path(key: &str) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = hasher.finish();
    cache_dir().join(format!("{hash:x}"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn get(key: &str) -> Option<String> {
    let path = key_path(key);
    let contents = std::fs::read_to_string(&path).ok()?;
    let (first_line, rest) = contents.split_once('\n')?;
    let expires: u64 = first_line.parse().ok()?;
    if now_secs() > expires {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(rest.to_string())
}

pub(crate) fn put(key: &str, value: &str) {
    put_with_ttl(key, value, DEFAULT_TTL);
}

pub(crate) fn put_with_ttl(key: &str, value: &str, ttl: Duration) {
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = key_path(key);
    let tmp = dir.join(format!("{}.tmp", std::process::id()));
    let expires = now_secs() + ttl.as_secs();
    let data = format!("{expires}\n{value}");
    if std::fs::write(&tmp, &data).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}
