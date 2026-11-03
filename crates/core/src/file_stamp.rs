//! Cheap change detection for state derived from files.
//!
//! A [`FileStamps`] records the metadata of every path some derived value was
//! read from. While the stamps still match, the value can be reused without
//! reading the files again. Track paths before reading them, so a write that
//! races the read changes the stamps and forces the next reader to derive the
//! value again.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Paths changed this close to capture may change again without moving their
/// timestamp on filesystems with coarse clocks, so they are never trusted.
const RACY_WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Track {
    /// Any change to the path, including entries added to a directory.
    Content,
    /// Files by content, directories only by identity.
    Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    is_dir: bool,
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(unix)]
    inode: (u64, u64),
}

impl Stamp {
    fn read(path: &Path, track: Track) -> Option<Self> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        let mut stamp = Self {
            is_dir: meta.is_dir(),
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            changed: (meta.ctime(), meta.ctime_nsec()),
            #[cfg(unix)]
            inode: (meta.dev(), meta.ino()),
        };
        if track == Track::Kind && stamp.is_dir {
            stamp.len = 0;
            stamp.modified = None;
            #[cfg(unix)]
            {
                stamp.changed = (0, 0);
            }
        }
        Some(stamp)
    }

    fn is_racy(&self, track: Track, now: SystemTime) -> bool {
        if track == Track::Kind && self.is_dir {
            return false;
        }
        self.modified.is_none_or(|modified| {
            now.duration_since(modified)
                .map_or(true, |age| age < RACY_WINDOW)
        })
    }
}

/// Metadata of a set of paths, including which of them were missing.
#[derive(Clone, Debug)]
pub struct FileStamps {
    entries: Vec<(PathBuf, Track, Option<Stamp>)>,
    trusted: bool,
}

impl Default for FileStamps {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            trusted: true,
        }
    }
}

impl FileStamps {
    /// Track every change to `path`, including entries added to or removed
    /// from a directory.
    pub fn track(&mut self, path: PathBuf) {
        self.push(path, Track::Content);
    }

    /// Track what `path` is: a file by its content, a directory only by its
    /// identity, so writes inside the directory do not count as changes.
    pub fn track_kind(&mut self, path: PathBuf) {
        self.push(path, Track::Kind);
    }

    fn push(&mut self, path: PathBuf, track: Track) {
        let stamp = Stamp::read(&path, track);
        if stamp.is_some_and(|stamp| stamp.is_racy(track, SystemTime::now())) {
            self.trusted = false;
        }
        self.entries.push((path, track, stamp));
    }

    /// Whether every path still has the metadata it had when it was tracked.
    pub fn is_unchanged(&self) -> bool {
        self.entries
            .iter()
            .all(|(path, track, stamp)| Stamp::read(path, *track) == *stamp)
    }

    /// Whether a value derived from the tracked paths can be reused instead of
    /// read again: the paths are unchanged, and none of them had just changed
    /// when tracked, when a change may not move its timestamp.
    pub fn is_reusable(&self) -> bool {
        self.trusted && self.is_unchanged()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn age(path: &Path) {
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    #[test]
    fn content_stamps_track_writes_creation_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        let missing = dir.path().join("missing");
        std::fs::write(&file, "one").unwrap();
        age(&file);

        let mut stamps = FileStamps::default();
        stamps.track(file.clone());
        stamps.track(missing.clone());
        assert!(stamps.is_reusable());

        std::fs::write(&missing, "").unwrap();
        assert!(!stamps.is_unchanged(), "created file must invalidate");
        std::fs::remove_file(&missing).unwrap();
        assert!(stamps.is_reusable());

        std::fs::write(&file, "two").unwrap();
        age(&file);
        assert!(!stamps.is_unchanged(), "rewritten file must invalidate");
    }

    #[test]
    fn kind_stamps_ignore_directory_entries_but_not_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let tracked = dir.path().join("tracked");
        std::fs::create_dir(&tracked).unwrap();

        let mut stamps = FileStamps::default();
        stamps.track_kind(tracked.clone());
        std::fs::write(tracked.join("entry"), "").unwrap();
        assert!(
            stamps.is_reusable(),
            "entries inside a directory are ignored"
        );

        std::fs::remove_dir_all(&tracked).unwrap();
        std::fs::write(&tracked, "now a file").unwrap();
        assert!(
            !stamps.is_unchanged(),
            "a replaced directory must invalidate"
        );
    }

    #[test]
    fn content_stamps_track_directory_entries() {
        let dir = tempfile::tempdir().unwrap();
        let tracked = dir.path().join("tracked");
        std::fs::create_dir(&tracked).unwrap();
        age(&tracked);

        let mut stamps = FileStamps::default();
        stamps.track(tracked.clone());
        assert!(stamps.is_reusable());
        std::fs::create_dir(tracked.join("entry")).unwrap();
        assert!(!stamps.is_unchanged());
    }

    #[test]
    fn recently_modified_files_are_never_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "fresh").unwrap();

        let mut fresh = FileStamps::default();
        fresh.track(file.clone());
        assert!(fresh.is_unchanged());
        assert!(!fresh.is_reusable());

        age(&file);
        let mut aged = FileStamps::default();
        aged.track(file);
        assert!(aged.is_reusable());
    }
}
