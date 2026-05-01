//! Per-engine cache of file observations (content, mtime, read range).
//!
//! Backs read-file dedup and mtime-based staleness for edit/write tools.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileState {
    pub(crate) content: String,
    pub(crate) mtime_ms: u64,
    // `Some` only for read-provenance entries. Writes leave this `None` so a
    // subsequent read_file doesn't dedup against pre-edit content.
    pub(crate) read_range: Option<(usize, usize)>,
}

const MAX_ENTRIES: usize = 100;
const MAX_TOTAL_BYTES: usize = 25 * 1024 * 1024;

/// Collapse `.` and `..` segments without touching the filesystem. Absolute
/// paths stay absolute. Used as the cache key so `./foo` and `foo/../foo`
/// hit the same entry.
fn normalize_path(p: &str) -> String {
    let path = Path::new(p);
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        return p.to_string();
    }
    out.to_string_lossy().into_owned()
}

/// Read `path`'s mtime as milliseconds since the UNIX epoch.
pub(crate) fn file_mtime_ms(path: &str) -> std::io::Result<u64> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta.modified()?;
    let ms = mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok(ms)
}

/// Shared cache of recent file observations. Cheap to clone (Arc-backed).
#[derive(Clone, Default)]
pub(crate) struct FileStateCache(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    seq: u64,
    total_bytes: usize,
}

struct Entry {
    state: FileState,
    seq: u64,
}

impl FileStateCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return a copy of the cached state for `path`, if present.
    pub(crate) fn get(&self, path: &str) -> Option<FileState> {
        let key = normalize_path(path);
        self.0
            .lock()
            .ok()?
            .entries
            .get(&key)
            .map(|e| e.state.clone())
    }

    pub(crate) fn has(&self, path: &str) -> bool {
        let key = normalize_path(path);
        self.0
            .lock()
            .map(|m| m.entries.contains_key(&key))
            .unwrap_or(false)
    }

    /// Cache a just-read file. Looks up mtime itself; entry is dedup-eligible.
    pub(crate) fn record_read(&self, path: &str, content: String, range: (usize, usize)) {
        let mtime_ms = file_mtime_ms(path).unwrap_or(0);
        self.set(
            path,
            FileState {
                content,
                mtime_ms,
                read_range: Some(range),
            },
        );
    }

    /// Cache a just-written file. Not dedup-eligible — a follow-up read_file
    /// must actually re-read rather than hit this entry.
    pub(crate) fn record_write(&self, path: &str, content: String) {
        let mtime_ms = file_mtime_ms(path).unwrap_or(0);
        self.set(
            path,
            FileState {
                content,
                mtime_ms,
                read_range: None,
            },
        );
    }

    /// Insert or replace an entry. Inserts exceeding the total byte cap are
    /// dropped; otherwise oldest entries are evicted until both caps hold.
    pub(crate) fn set(&self, path: &str, state: FileState) {
        let new_bytes = state.content.len();
        if new_bytes > MAX_TOTAL_BYTES {
            return;
        }
        let key = normalize_path(path);
        let Ok(mut inner) = self.0.lock() else {
            return;
        };
        inner.seq += 1;
        let seq = inner.seq;
        if let Some(old) = inner.entries.remove(&key) {
            inner.total_bytes = inner.total_bytes.saturating_sub(old.state.content.len());
        }
        inner.total_bytes += new_bytes;
        inner.entries.insert(key.clone(), Entry { state, seq });
        while (inner.entries.len() > MAX_ENTRIES || inner.total_bytes > MAX_TOTAL_BYTES)
            && inner.entries.len() > 1
        {
            // Evict the oldest entry that is NOT the one we just inserted.
            let oldest_key = inner
                .entries
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, e)| e.seq)
                .map(|(k, _)| k.clone());
            let Some(oldest) = oldest_key else { break };
            if let Some(old) = inner.entries.remove(&oldest) {
                inner.total_bytes = inner.total_bytes.saturating_sub(old.state.content.len());
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.lock().map(|m| m.entries.len()).unwrap_or(0)
    }
}

/// Error string when the cache has no prior observation or the file drifted
/// since the last observation. `None` means safe to proceed. `noun` is
/// `"file"` or `"notebook"`, used to phrase the message for the caller tool.
pub(crate) fn staleness_error(cache: &FileStateCache, path: &str, noun: &str) -> Option<String> {
    let Some(cached) = cache.get(path) else {
        return Some(format!(
            "You must use read_file before editing. Read the {noun} first."
        ));
    };
    match file_mtime_ms(path) {
        Ok(current) if current == cached.mtime_ms => None,
        // Let a missing file / stat error fall through to run()'s real I/O error.
        Err(_) => None,
        Ok(_) => {
            let cap = if noun == "notebook" {
                "Notebook"
            } else {
                "File"
            };
            Some(format!(
                "{cap} has been modified since last read. Use read_file to read the current contents before editing."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::MAIN_SEPARATOR_STR;

    fn sep(s: &str) -> String {
        s.replace('/', MAIN_SEPARATOR_STR)
    }

    #[test]
    fn normalize_strips_curdir() {
        assert_eq!(normalize_path("./foo/bar"), sep("foo/bar"));
        assert_eq!(normalize_path("foo/./bar"), sep("foo/bar"));
    }

    #[test]
    fn normalize_collapses_parent_dir() {
        assert_eq!(normalize_path("/a/b/../c"), sep("/a/c"));
        assert_eq!(normalize_path("/a/b/../../c"), sep("/c"));
    }

    #[test]
    fn normalize_preserves_absolute() {
        assert_eq!(normalize_path("/abs/path"), sep("/abs/path"));
    }

    #[test]
    fn normalize_empty_returns_input() {
        assert_eq!(normalize_path(""), "");
    }

    #[test]
    fn normalize_single_component() {
        assert_eq!(normalize_path("foo"), "foo");
    }

    #[test]
    fn normalize_leading_parent_kept() {
        assert_eq!(normalize_path("../foo"), sep("../foo"));
    }

    fn state(content: &str, mtime: u64, range: Option<(usize, usize)>) -> FileState {
        FileState {
            content: content.to_string(),
            mtime_ms: mtime,
            read_range: range,
        }
    }

    #[test]
    fn set_and_get_roundtrip() {
        let c = FileStateCache::new();
        c.set("/tmp/a.txt", state("hello", 100, Some((1, 2000))));
        let got = c.get("/tmp/a.txt").unwrap();
        assert_eq!(got.content, "hello");
        assert_eq!(got.mtime_ms, 100);
        assert_eq!(got.read_range, Some((1, 2000)));
    }

    #[test]
    fn get_misses_on_unknown_path() {
        let c = FileStateCache::new();
        assert!(c.get("/nope").is_none());
        assert!(!c.has("/nope"));
    }

    #[test]
    fn has_returns_true_after_set() {
        let c = FileStateCache::new();
        c.set("/tmp/x", state("x", 1, None));
        assert!(c.has("/tmp/x"));
    }

    #[test]
    fn set_replaces_existing_entry_and_updates_bytes() {
        let c = FileStateCache::new();
        c.set("/tmp/a", state("aaaa", 1, None));
        c.set("/tmp/a", state("bb", 2, None));
        let got = c.get("/tmp/a").unwrap();
        assert_eq!(got.content, "bb");
        assert_eq!(got.mtime_ms, 2);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn normalized_paths_hit_same_entry() {
        let c = FileStateCache::new();
        c.set("/tmp/./foo", state("x", 1, None));
        assert!(c.has("/tmp/foo"));
        assert!(c.has("/tmp/bar/../foo"));
    }

    #[test]
    fn eviction_removes_oldest_on_entry_overflow() {
        let c = FileStateCache::new();
        for i in 0..(MAX_ENTRIES + 5) {
            c.set(&format!("/tmp/f{i}"), state("x", i as u64, None));
        }
        assert_eq!(c.len(), MAX_ENTRIES);
        // Oldest 5 should be gone.
        for i in 0..5 {
            assert!(
                !c.has(&format!("/tmp/f{i}")),
                "oldest entry f{i} should have been evicted"
            );
        }
        // The most recent should all be present.
        for i in 5..(MAX_ENTRIES + 5) {
            assert!(c.has(&format!("/tmp/f{i}")), "recent f{i} should remain");
        }
    }

    #[test]
    fn eviction_removes_oldest_on_byte_overflow() {
        let c = FileStateCache::new();
        // Half the cap each — third insertion should evict the first.
        let big = "x".repeat(MAX_TOTAL_BYTES / 2 + 1);
        c.set("/tmp/a", state(&big, 1, None));
        c.set("/tmp/b", state(&big, 2, None));
        c.set("/tmp/c", state(&big, 3, None));
        assert!(!c.has("/tmp/a"));
        assert!(c.has("/tmp/c"));
    }

    #[test]
    fn entry_exceeding_total_cap_is_not_cached() {
        let c = FileStateCache::new();
        let too_big = "x".repeat(MAX_TOTAL_BYTES + 1);
        c.set("/tmp/huge", state(&too_big, 1, None));
        assert!(!c.has("/tmp/huge"));
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn file_mtime_ms_returns_monotonic_value() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"hello").unwrap();
        let m = file_mtime_ms(&tmp.path().to_string_lossy()).unwrap();
        assert!(m > 0);
    }

    #[test]
    fn file_mtime_ms_errors_on_missing_file() {
        assert!(file_mtime_ms("/nonexistent/path/xyz").is_err());
    }
}
