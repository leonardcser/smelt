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
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    std::fs::read_to_string(path)
}

/// Read the entire file as raw bytes.
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// Write `contents` to `path`, replacing existing contents.
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    std::fs::write(path, contents)
}

/// `true` if the path exists (file, dir, or otherwise).
pub fn exists(path: impl AsRef<Path>) -> bool {
    path.as_ref().exists()
}

/// `true` if the path resolves to a regular file.
pub fn is_file(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_file()
}

/// `true` if the path resolves to a directory.
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_dir()
}

/// Enumerate a directory's immediate entries. Returns absolute (or
/// input-relative) paths in OS order — callers sort if they care.
pub fn read_dir(path: impl AsRef<Path>) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        out.push(entry?.path());
    }
    Ok(out)
}

/// Create a directory. Errors if the parent does not exist.
pub fn mkdir(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::create_dir(path)
}

/// Create a directory and any missing parents.
pub fn mkdir_all(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Remove a regular file.
pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_file(path)
}

/// Remove an empty directory.
pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_dir(path)
}

/// Remove a directory and all its contents.
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::remove_dir_all(path)
}

/// Rename or move a path.
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// Copy the file `from` to `to`, returning the number of bytes copied.
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    std::fs::copy(from, to)
}

/// Modification time as Unix epoch seconds. `None` if the platform
/// does not expose mtime or the value is before the epoch.
pub fn mtime_secs(path: impl AsRef<Path>) -> io::Result<Option<u64>> {
    let meta = std::fs::metadata(path)?;
    let modified = meta.modified()?;
    Ok(modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs()))
}

/// File size in bytes, or directory link metadata size on platforms
/// that report it.
pub fn size(path: impl AsRef<Path>) -> io::Result<u64> {
    Ok(std::fs::metadata(path)?.len())
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
