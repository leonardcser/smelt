use std::fs::{self, File};
use std::io;
use std::path::Path;

use crate::error::{Result, StoreError};
use crate::Store;

/// Exclusive ownership of one session across processes. The OS releases it when
/// the process exits, however it exits.
#[derive(Debug)]
pub(crate) struct SessionLock {
    _file: File,
}

impl SessionLock {
    pub(crate) fn acquire(store: &Store, session_id: &str) -> Result<Self> {
        if session_id.is_empty()
            || !session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(StoreError::Integrity(format!(
                "invalid session id {session_id:?}"
            )));
        }
        let dir = store.locks_dir();
        crate::db::create_private_dir_all(&dir)?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(dir.join(format!("{session_id}.lock")))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => Err(StoreError::Owned {
                session_id: session_id.to_owned(),
            }),
            Err(fs::TryLockError::Error(error)) => Err(StoreError::Io(error)),
        }
    }
}

/// Counts sessions another process owns. A missing lock directory means none.
pub(crate) fn owned_sessions(locks_dir: &Path) -> Result<usize> {
    let entries = match fs::read_dir(locks_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut owned = 0;
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "lock") {
            continue;
        }
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => owned += 1,
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    Ok(owned)
}
