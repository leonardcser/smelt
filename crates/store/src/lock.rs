use std::fs::{self, File};

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
