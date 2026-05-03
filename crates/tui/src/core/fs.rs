//! Filesystem capability — sync primitives over `std::fs`. Pure I/O,
//! no policy. Exposed to Lua via `crates/tui/src/lua/api/fs.rs` and
//! composed by tools that need to read, write, or enumerate the
//! filesystem.
//!
//! Async-yielding wrappers (used when a Lua tool's coroutine awaits a
//! long read/write) live in the tool's host binding, not here. This
//! module is the lowest layer.

use std::io;
use std::path::{Path, PathBuf};

/// Read the entire file as a UTF-8 string.
pub(crate) fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    std::fs::read_to_string(path)
}

/// Write `contents` to `path`, replacing existing contents.
pub(crate) fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    std::fs::write(path, contents)
}

/// `true` if the path exists (file, dir, or otherwise).
pub(crate) fn exists(path: impl AsRef<Path>) -> bool {
    path.as_ref().exists()
}

/// `true` if the path resolves to a regular file.
pub(crate) fn is_file(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_file()
}

/// `true` if the path resolves to a directory.
pub(crate) fn is_dir(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_dir()
}

/// Enumerate a directory's immediate entries. Returns absolute (or
/// input-relative) paths in OS order — callers sort if they care.
pub(crate) fn read_dir(path: impl AsRef<Path>) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        out.push(entry?.path());
    }
    Ok(out)
}

/// Create a directory. Errors if the parent does not exist.
pub(crate) fn mkdir(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::create_dir(path)
}

/// Create a directory and any missing parents.
pub(crate) fn mkdir_all(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Remove a regular file.
pub(crate) fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_file(path)
}

/// Remove an empty directory.
pub(crate) fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_dir(path)
}

/// Remove a directory and all its contents.
pub(crate) fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_dir_all(path)
}

/// Rename or move a path.
pub(crate) fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// Copy the file `from` to `to`, returning the number of bytes copied.
pub(crate) fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    std::fs::copy(from, to)
}

/// Modification time as Unix epoch seconds. `None` if the platform
/// does not expose mtime or the value is before the epoch.
pub(crate) fn mtime_secs(path: impl AsRef<Path>) -> io::Result<Option<u64>> {
    let meta = std::fs::metadata(path)?;
    let modified = meta.modified()?;
    Ok(modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs()))
}

/// File size in bytes, or directory link metadata size on platforms
/// that report it.
pub(crate) fn size(path: impl AsRef<Path>) -> io::Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

/// One match emitted by [`glob`]: the file's modification time plus
/// its display path. Callers sort or truncate as they like.
pub(crate) struct GlobMatch {
    pub mtime: std::time::SystemTime,
    pub path: String,
}

/// Walk `search_dir` (or `.` when empty) honouring `.gitignore` and
/// emit files whose path *relative to `search_dir`* matches `pattern`.
/// Stops once `max` matches accumulate. Returns the matches unsorted —
/// the caller decides ordering.
pub(crate) fn glob(pattern: &str, search_dir: &str, max: usize) -> Result<Vec<GlobMatch>, String> {
    let matcher = match globset::Glob::new(pattern) {
        Ok(g) => g.compile_matcher(),
        Err(e) => return Err(format!("invalid glob pattern: {e}")),
    };
    let dir = if search_dir.is_empty() {
        "."
    } else {
        search_dir
    };
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .build();

    let mut out = Vec::new();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(dir).unwrap_or(path);
        if !matcher.is_match(relative) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(mtime) = meta.modified() {
                out.push(GlobMatch {
                    mtime,
                    path: path.display().to_string(),
                });
            }
        }
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_write_round_trip() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("hello.txt");
        write(&p, "hi").unwrap();
        assert_eq!(read_to_string(&p).unwrap(), "hi");
        assert!(exists(&p));
        assert!(is_file(&p));
        assert!(!is_dir(&p));
    }

    #[test]
    fn mkdir_and_read_dir() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a/b/c");
        mkdir_all(&nested).unwrap();
        assert!(is_dir(&nested));

        write(nested.join("x.txt"), "x").unwrap();
        write(nested.join("y.txt"), "y").unwrap();
        let mut entries = read_dir(&nested).unwrap();
        entries.sort();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn remove_and_rename() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a.txt");
        write(&p, "a").unwrap();
        let q = tmp.path().join("b.txt");
        rename(&p, &q).unwrap();
        assert!(!exists(&p));
        assert!(exists(&q));
        remove_file(&q).unwrap();
        assert!(!exists(&q));
    }

    #[test]
    fn mtime_and_size() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("z.txt");
        write(&p, "hello").unwrap();
        assert_eq!(size(&p).unwrap(), 5);
        assert!(mtime_secs(&p).unwrap().is_some());
    }
}

// ── File-state cache (migrated from engine/tools/file_state.rs) ───────────

use std::collections::HashMap;
use std::path::Component;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub content: String,
    pub mtime_ms: u64,
    // `Some` only for read-provenance entries. Writes leave this `None` so a
    // subsequent read_file doesn't dedup against pre-edit content.
    pub read_range: Option<(usize, usize)>,
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
pub fn file_mtime_ms(path: &str) -> std::io::Result<u64> {
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
pub struct FileStateCache(Arc<Mutex<Inner>>);

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
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a copy of the cached state for `path`, if present.
    pub fn get(&self, path: &str) -> Option<FileState> {
        let key = normalize_path(path);
        self.0
            .lock()
            .ok()?
            .entries
            .get(&key)
            .map(|e| e.state.clone())
    }

    pub fn has(&self, path: &str) -> bool {
        let key = normalize_path(path);
        self.0
            .lock()
            .map(|m| m.entries.contains_key(&key))
            .unwrap_or(false)
    }

    /// Cache a just-read file. Looks up mtime itself; entry is dedup-eligible.
    pub fn record_read(&self, path: &str, content: String, range: (usize, usize)) {
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
    pub fn record_write(&self, path: &str, content: String) {
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
    pub fn set(&self, path: &str, state: FileState) {
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
pub fn staleness_error(cache: &FileStateCache, path: &str, noun: &str) -> Option<String> {
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

// ── Advisory file locking (migrated from engine/tools/mod.rs) ─────────────

/// Acquire an exclusive, non-blocking advisory lock on the given file path.
/// Returns `Ok(guard)` on success. Returns `Err(message)` if the file is
/// locked by another process (EWOULDBLOCK) or on any other I/O error.
/// The lock is released when the guard is dropped.
#[cfg(unix)]
pub fn try_flock(path: &str) -> Result<FlockGuard, String> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Err("File is currently being edited by another agent, try again later.".into());
        }
        return Err(format!("flock error: {err}"));
    }
    Ok(FlockGuard { _file: file })
}

#[cfg(not(unix))]
pub fn try_flock(_path: &str) -> Result<FlockGuard, String> {
    Ok(FlockGuard { _file: None })
}

pub struct FlockGuard {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(not(unix))]
    _file: Option<()>,
}

#[cfg(test)]
mod file_state_tests {
    use super::*;
    use std::path::MAIN_SEPARATOR_STR;

    fn sep(s: &str) -> String {
        s.replace('/', MAIN_SEPARATOR_STR)
    }

    fn state(content: &str, mtime: u64, range: Option<(usize, usize)>) -> FileState {
        FileState {
            content: content.to_string(),
            mtime_ms: mtime,
            read_range: range,
        }
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
