//! Filesystem capability - sync primitives over `std::fs`. Pure I/O, no policy.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const TEXT_WINDOW_LINE_LIMIT: usize = 2000;

pub fn render_text_window(content: &str, offset: usize, limit: usize) -> Option<String> {
    let mut lines: Vec<&str> = content.lines().collect();
    if content.ends_with('\n') {
        lines.push("");
    }
    if lines.is_empty() {
        return (offset <= 1).then(String::new);
    }
    let start = offset.max(1) - 1;
    if start >= lines.len() {
        return None;
    }
    let end = start.saturating_add(limit).min(lines.len());
    Some(
        lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let line = smelt_buffer::text::slice(line, 0..TEXT_WINDOW_LINE_LIMIT);
                format!("{:4}\t{}", start + i + 1, line)
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

pub(crate) fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    std::fs::read_to_string(path)
}

pub(crate) struct LimitedRead {
    pub content: String,
    pub truncated: bool,
}

pub(crate) fn read_to_string_limited(
    path: impl AsRef<Path>,
    max_bytes: usize,
) -> io::Result<LimitedRead> {
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max_bytes;
    if truncated {
        bytes.truncate(max_bytes);
    }
    let content = String::from_utf8_lossy(&bytes).into_owned();
    Ok(LimitedRead { content, truncated })
}

pub(crate) fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    std::fs::write(path, contents)
}

pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(contents)?;
    file.commit()
}

pub(crate) fn exists(path: impl AsRef<Path>) -> bool {
    path.as_ref().exists()
}

pub(crate) fn is_file(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_file()
}

pub(crate) fn is_dir(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_dir()
}

/// Returns paths in OS order - callers sort if they care.
pub(crate) fn read_dir(path: impl AsRef<Path>) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        out.push(entry?.path());
    }
    Ok(out)
}

pub(crate) fn mkdir(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::create_dir(path)
}

pub(crate) fn mkdir_all(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

pub(crate) fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_file(path)
}

pub(crate) fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_dir(path)
}

pub(crate) fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_dir_all(path)
}

pub(crate) fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    std::fs::rename(from, to)
}

pub(crate) fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    std::fs::copy(from, to)
}

pub(crate) fn size(path: impl AsRef<Path>) -> io::Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

pub(crate) struct GlobMatch {
    pub mtime: std::time::SystemTime,
    pub path: String,
}

pub(crate) struct GlobSearch {
    pub matches: Vec<GlobMatch>,
    pub scanned: usize,
    pub match_limit_hit: bool,
    pub scan_limit_hit: bool,
    pub timed_out: bool,
}

/// Walk the narrowest literal subtree implied by `pattern`, honouring
/// `.gitignore`, and match against paths relative to `search_dir`. Stops after
/// `max` matches; returns unsorted.
pub(crate) fn glob(pattern: &str, search_dir: &str, max: usize) -> Result<Vec<GlobMatch>, String> {
    Ok(glob_with_limits(pattern, search_dir, max, None, None)?.matches)
}

pub(crate) fn glob_with_limits(
    pattern: &str,
    search_dir: &str,
    max: usize,
    max_scanned: Option<usize>,
    timeout: Option<Duration>,
) -> Result<GlobSearch, String> {
    let matcher = match globset::Glob::new(pattern) {
        Ok(g) => g.compile_matcher(),
        Err(e) => return Err(format!("invalid glob pattern: {e}")),
    };
    let dir = if search_dir.is_empty() {
        Path::new(".")
    } else {
        Path::new(search_dir)
    };
    let deadline = timeout.map(|d| Instant::now() + d);
    let scan_limit = max_scanned.unwrap_or(usize::MAX);

    if max == 0 {
        return Ok(GlobSearch {
            matches: Vec::new(),
            scanned: 0,
            match_limit_hit: true,
            scan_limit_hit: false,
            timed_out: false,
        });
    }

    if !has_glob_meta(pattern) {
        let path = dir.join(pattern);
        if !path.is_file() || !matcher.is_match(Path::new(pattern)) {
            return Ok(GlobSearch {
                matches: Vec::new(),
                scanned: 1,
                match_limit_hit: false,
                scan_limit_hit: false,
                timed_out: false,
            });
        }
        let meta = path.metadata().map_err(|e| e.to_string())?;
        let mtime = meta.modified().map_err(|e| e.to_string())?;
        return Ok(GlobSearch {
            matches: vec![GlobMatch {
                mtime,
                path: path.display().to_string(),
            }],
            scanned: 1,
            match_limit_hit: false,
            scan_limit_hit: false,
            timed_out: false,
        });
    }

    let walk_root = match literal_prefix_before_meta(pattern) {
        Some(prefix) => dir.join(prefix),
        None => dir.to_path_buf(),
    };
    if !walk_root.exists() {
        return Ok(GlobSearch {
            matches: Vec::new(),
            scanned: 0,
            match_limit_hit: false,
            scan_limit_hit: false,
            timed_out: false,
        });
    }

    let walker = ignore::WalkBuilder::new(&walk_root)
        .hidden(false)
        .git_ignore(true)
        .build();

    let mut out = Vec::new();
    let mut scanned = 0usize;
    let mut match_limit_hit = false;
    let mut scan_limit_hit = false;
    let mut timed_out = false;
    for entry in walker {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            timed_out = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if scanned >= scan_limit {
            scan_limit_hit = true;
            break;
        }
        scanned += 1;
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
            match_limit_hit = true;
            break;
        }
    }
    Ok(GlobSearch {
        matches: out,
        scanned,
        match_limit_hit,
        scan_limit_hit,
        timed_out,
    })
}

fn has_glob_meta(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}'))
}

fn literal_prefix_before_meta(pattern: &str) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for segment in pattern.split('/') {
        if segment.is_empty() {
            continue;
        }
        if has_glob_meta(segment) {
            break;
        }
        prefix.push(segment);
    }
    (!prefix.as_os_str().is_empty()).then_some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn render_text_window_truncates_long_lines_on_char_boundary() {
        let long = format!("{}é", "a".repeat(TEXT_WINDOW_LINE_LIMIT));
        let rendered = render_text_window(&long, 1, 1).unwrap();
        assert_eq!(
            rendered,
            format!("   1\t{}", "a".repeat(TEXT_WINDOW_LINE_LIMIT))
        );
    }

    #[test]
    fn literal_prefix_uses_existing_subtree() {
        assert_eq!(
            literal_prefix_before_meta("crates/smelt_term/**/*.rs"),
            Some(PathBuf::from("crates/smelt_term"))
        );
        assert_eq!(
            literal_prefix_before_meta("crates/term/src/*.rs"),
            Some(PathBuf::from("crates/term/src"))
        );
        assert_eq!(literal_prefix_before_meta("**/*.rs"), None);
        assert_eq!(literal_prefix_before_meta("*.rs"), None);
    }

    #[test]
    fn glob_returns_empty_for_missing_literal_prefix() {
        let tmp = TempDir::new().unwrap();
        mkdir_all(tmp.path().join("present/src")).unwrap();
        write(tmp.path().join("present/src/lib.rs"), "fn main() {}").unwrap();

        let matches = glob("missing/**/*.rs", tmp.path().to_str().unwrap(), 200).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn glob_matches_inside_literal_prefix() {
        let tmp = TempDir::new().unwrap();
        mkdir_all(tmp.path().join("crates/term/src")).unwrap();
        write(tmp.path().join("crates/term/src/lib.rs"), "pub mod x;").unwrap();
        write(tmp.path().join("crates/term/src/lib.txt"), "nope").unwrap();

        let matches = glob("crates/term/**/*.rs", tmp.path().to_str().unwrap(), 200).unwrap();
        assert_eq!(matches.len(), 1);
        assert!(Path::new(&matches[0].path).ends_with(Path::new("crates/term/src/lib.rs")));
    }

    #[test]
    fn glob_with_limits_stops_after_scan_cap() {
        let tmp = TempDir::new().unwrap();
        mkdir_all(tmp.path().join("src")).unwrap();
        write(tmp.path().join("src/a.txt"), "a").unwrap();
        write(tmp.path().join("src/b.txt"), "b").unwrap();

        let result = glob_with_limits(
            "**/*.missing",
            tmp.path().to_str().unwrap(),
            200,
            Some(1),
            None,
        )
        .unwrap();
        assert!(result.matches.is_empty());
        assert!(result.scan_limit_hit);
        assert!(!result.match_limit_hit);
        assert_eq!(result.scanned, 1);
    }

    #[test]
    fn glob_matches_exact_literal_file() {
        let tmp = TempDir::new().unwrap();
        mkdir_all(tmp.path().join("src")).unwrap();
        write(tmp.path().join("src/lib.rs"), "pub mod x;").unwrap();

        let matches = glob("src/lib.rs", tmp.path().to_str().unwrap(), 200).unwrap();
        assert_eq!(matches.len(), 1);
        assert!(Path::new(&matches[0].path).ends_with(Path::new("src/lib.rs")));
    }

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
    fn size_of_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("z.txt");
        write(&p, "hello").unwrap();
        assert_eq!(size(&p).unwrap(), 5);
    }
}

// ── File-state cache ──────────────────────────────────────────────────────

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

/// Collapse `.` and `..` without touching the filesystem. Used as the cache
/// key so `./foo` and `foo/../foo` hit the same entry.
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

pub fn file_mtime_ms(path: &str) -> std::io::Result<u64> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta.modified()?;
    Ok(system_time_ms(mtime))
}

pub fn system_time_ms(time: std::time::SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Shared cache of recent file observations. Arc-backed, cheap to clone.
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

    /// Cache a just-read file (dedup-eligible against a later read).
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

    /// Cache a just-read file with an mtime already observed by the caller.
    pub fn record_read_with_mtime(
        &self,
        path: &str,
        content: String,
        range: (usize, usize),
        mtime_ms: u64,
    ) {
        self.set(
            path,
            FileState {
                content,
                mtime_ms,
                read_range: Some(range),
            },
        );
    }

    /// Cache a just-written file (dedup-eligible only after a later read) with
    /// an mtime already observed by the caller.
    pub fn record_write_with_mtime(&self, path: &str, content: String, mtime_ms: u64) {
        self.set(
            path,
            FileState {
                content,
                mtime_ms,
                read_range: None,
            },
        );
    }

    /// Cache a just-written file. Not dedup-eligible - a follow-up read must re-read.
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

    /// Insert or replace an entry, evicting oldest entries when either cap is exceeded.
    /// Entries larger than `MAX_TOTAL_BYTES` are silently dropped.
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
    pub fn len(&self) -> usize {
        self.0.lock().map(|m| m.entries.len()).unwrap_or(0)
    }
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn cached_state(cache: &FileStateCache, path: &str, noun: &str) -> Result<FileState, String> {
    cache
        .get(path)
        .ok_or_else(|| format!("read the {noun} with read_file before editing"))
}

fn ensure_cached_state_is_current(
    path: &str,
    cached: &FileState,
    noun: &str,
) -> Result<(), String> {
    match file_mtime_ms(path) {
        Ok(current) if current == cached.mtime_ms => Ok(()),
        Err(err) => Err(err.to_string()),
        Ok(_) => Err(format!(
            "{noun} has been modified since last read; use read_file to read the current contents before editing"
        )),
    }
}

fn fresh_cached_state(cache: &FileStateCache, path: &str, noun: &str) -> Result<FileState, String> {
    let cached = cached_state(cache, path, noun)?;
    ensure_cached_state_is_current(path, &cached, noun)?;
    Ok(cached)
}

/// `None` when safe to proceed; error string when the cache has no prior read,
/// the current file state cannot be inspected, or its mtime has changed. `noun`
/// (`"file"` or `"notebook"`) phrases the message for the caller tool.
pub fn staleness_error(cache: &FileStateCache, path: &str, noun: &str) -> Option<String> {
    fresh_cached_state(cache, path, noun).err()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFileOutcome {
    pub old_content: String,
    pub new_content: String,
}

fn validate_edit_file_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("missing required parameter: file_path".into());
    }
    if crate::notebook::is_notebook_path(path) {
        return Err("cannot use edit_file on a Jupyter notebook; use edit_notebook instead".into());
    }
    Ok(())
}

pub fn checked_write_file(
    path: &str,
    content: &str,
    cache: &FileStateCache,
) -> Result<usize, String> {
    if path.is_empty() {
        return Err("missing required parameter: file_path".into());
    }

    let exists = Path::new(path).exists();
    let _lock = if exists {
        if !cache.has(path) {
            return Err("file already exists; use edit_file to modify existing files, or read_file then write_file to replace".into());
        }
        if let Some(err) = staleness_error(cache, path, "file") {
            return Err(err);
        }
        Some(try_flock(path)?)
    } else {
        None
    };

    if let Some(parent) = Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    std::fs::write(path, content.as_bytes()).map_err(|e| e.to_string())?;
    let mtime_ms = file_mtime_ms(path).unwrap_or(0);
    cache.record_write_with_mtime(path, content.to_string(), mtime_ms);
    Ok(content.len())
}

fn plan_edit_file(
    content: String,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<EditFileOutcome, String> {
    if old_string == new_string {
        return Err("old_string and new_string are identical".into());
    }
    if old_string.is_empty() {
        return Err("old_string not found in file".into());
    }

    let count = content.matches(old_string).count();
    if count == 0 {
        return Err("old_string not found in file".into());
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string matched {count} times; make it unique or set replace_all to true"
        ));
    }

    let new_content = if replace_all {
        content.replace(old_string, new_string)
    } else {
        content.replacen(old_string, new_string, 1)
    };

    Ok(EditFileOutcome {
        old_content: content,
        new_content,
    })
}

pub fn checked_plan_edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    cache: &FileStateCache,
) -> Result<EditFileOutcome, String> {
    validate_edit_file_path(path)?;
    let cached = fresh_cached_state(cache, path, "file")?;
    plan_edit_file(cached.content, old_string, new_string, replace_all)
}

pub fn checked_edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    cache: &FileStateCache,
) -> Result<EditFileOutcome, String> {
    validate_edit_file_path(path)?;
    let cached = cached_state(cache, path, "file")?;
    let _lock = try_flock(path)?;
    ensure_cached_state_is_current(path, &cached, "file")?;
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let outcome = plan_edit_file(content, old_string, new_string, replace_all)?;

    std::fs::write(path, outcome.new_content.as_bytes()).map_err(|e| e.to_string())?;
    let mtime_ms = file_mtime_ms(path).unwrap_or(0);
    cache.record_write_with_mtime(path, outcome.new_content.clone(), mtime_ms);

    Ok(outcome)
}

// ── Advisory file locking ─────────────────────────────────────────────────

/// Acquire an exclusive non-blocking advisory lock. Returns `Err` if the file
/// is locked by another process (EWOULDBLOCK) or on I/O error. Released on drop.
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
            return Err("file is currently being edited by another agent, try again later".into());
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
mod edit_file_tests {
    use super::*;

    const CONTENT: &str = "alpha\nbeta\nalpha\n";

    #[test]
    fn planner_replaces_one_unique_match() {
        assert_eq!(
            plan_edit_file(CONTENT.into(), "beta", "gamma", false),
            Ok(EditFileOutcome {
                old_content: CONTENT.into(),
                new_content: "alpha\ngamma\nalpha\n".into(),
            })
        );
    }

    #[test]
    fn planner_replaces_all_matches() {
        assert_eq!(
            plan_edit_file(CONTENT.into(), "alpha", "gamma", true),
            Ok(EditFileOutcome {
                old_content: CONTENT.into(),
                new_content: "gamma\nbeta\ngamma\n".into(),
            })
        );
    }

    #[test]
    fn planner_rejects_invalid_edits() {
        for (old_string, new_string, replace_all, expected) in [
            (
                "alpha",
                "alpha",
                false,
                "old_string and new_string are identical",
            ),
            ("", "gamma", false, "old_string not found in file"),
            ("missing", "gamma", false, "old_string not found in file"),
            (
                "alpha",
                "gamma",
                false,
                "old_string matched 2 times; make it unique or set replace_all to true",
            ),
        ] {
            assert_eq!(
                plan_edit_file(CONTENT.into(), old_string, new_string, replace_all),
                Err(expected.into())
            );
        }
    }
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
        // Half the cap each - third insertion should evict the first.
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
