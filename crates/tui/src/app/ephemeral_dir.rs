const EPHEMERAL_DIR_PREFIX: &str = "smelt-session-";
const EPHEMERAL_LOCK_FILE: &str = ".lock";
/// Directories without a lock file younger than this may still be starting up.
const EPHEMERAL_STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Scratch directory of an ephemeral session. The held lock lets later runs
/// tell a live directory from one orphaned by a killed process.
#[derive(Debug)]
pub struct EphemeralDir {
    dir: tempfile::TempDir,
    _lock: std::fs::File,
}

impl EphemeralDir {
    pub(super) fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    pub(super) fn create() -> std::io::Result<Self> {
        static SWEPT: std::sync::Once = std::sync::Once::new();
        SWEPT.call_once(|| sweep_orphaned(&std::env::temp_dir()));
        let dir = tempfile::Builder::new()
            .prefix(EPHEMERAL_DIR_PREFIX)
            .tempdir()?;
        let lock = std::fs::File::create(dir.path().join(EPHEMERAL_LOCK_FILE))?;
        lock.lock()?;
        Ok(Self { dir, _lock: lock })
    }
}

/// Removes `smelt-session-*` directories under `root` whose owner no longer
/// holds the lock, as left behind by SIGKILL or a crash. Best effort.
fn sweep_orphaned(root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_session_dir = entry
            .file_name()
            .to_string_lossy()
            .starts_with(EPHEMERAL_DIR_PREFIX)
            && entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !is_session_dir {
            continue;
        }
        let orphaned = match std::fs::File::open(path.join(EPHEMERAL_LOCK_FILE)) {
            Ok(lock) => lock.try_lock().is_ok(),
            Err(_) => entry
                .metadata()
                .and_then(|meta| meta.modified())
                .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
                .is_ok_and(|age| age > EPHEMERAL_STARTUP_GRACE),
        };
        if orphaned {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_removes_only_unlocked_ephemeral_dirs() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("smelt-session-live");
        let orphan = root.path().join("smelt-session-orphan");
        let unrelated = root.path().join("other");
        for dir in [&live, &orphan, &unrelated] {
            std::fs::create_dir(dir).unwrap();
        }
        let held = std::fs::File::create(live.join(EPHEMERAL_LOCK_FILE)).unwrap();
        held.lock().unwrap();
        std::fs::File::create(orphan.join(EPHEMERAL_LOCK_FILE)).unwrap();

        sweep_orphaned(root.path());

        assert!(live.exists());
        assert!(unrelated.exists());
        assert!(!orphan.exists());
    }
}
