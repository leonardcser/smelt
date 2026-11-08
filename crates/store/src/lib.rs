//! Session storage: one SQLite database holding every session as flat rows.
//!
//! History, transcript records and side tables are sequences keyed by session and
//! position. A save replaces the changed suffix of each sequence in one transaction,
//! so its cost follows the new content, not the session size. Readers query rows
//! by position or by indexed columns and never decode more than they return.
//! Large JSON strings are stored once however many rows hold them, and a full-text
//! index over conversation text finds sessions without reading them.

mod catalog;
mod db;
mod document;
mod error;
mod export;
mod history;
mod lock;
mod prompts;
mod reader;
mod requests;
mod transcript;
mod types;
mod usage;
mod writer;

use std::path::{Path, PathBuf};

pub use catalog::{CatalogCursor, CatalogPage, CatalogQuery, CatalogSession, SEARCH_MIN_CHARS};
pub use db::{create_private_dir_all, SCHEMA_VERSION};
pub use error::{Result, StoreError};
pub use history::is_user_turn;
pub use reader::{SessionReader, StorageStats};
pub use requests::{
    RequestAuditOrder, RequestAuditPayloadMode, RequestAuditPayloads, RequestAuditQuery,
    RequestAuditStats, RequestAuditSummary, ResponsePayloadKind,
};
pub use transcript::{
    transcript_role, transcript_text_extent_profile, StoredTranscriptBlock, TranscriptBlockLookup,
    TranscriptExtentProfile, TranscriptNavigationRecord, TranscriptRecordOffset,
    TranscriptRecordProfile, TranscriptRecordRange, TranscriptRecordSlice, TranscriptRecordSummary,
    TranscriptSearchCandidate, TranscriptSearchDirection, TRANSCRIPT_EXTENT_PROFILE_WIDTHS,
};
pub use types::{
    HistoryLen, HistorySuffix, SessionCommit, SessionIdentity, SessionMetadata, SessionState,
    SideRows, StoreHead, TranscriptRecordCount, TranscriptRecordSuffix,
};
pub use usage::UsageEntry;
pub use writer::{SessionBatch, SessionWriter};

/// The database and session files under the state directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            root: state_root.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn database_path(&self) -> PathBuf {
        self.root.join("smelt.db")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }

    pub fn artifacts_dir(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    pub fn session_artifact_dir(&self, session_id: &str) -> PathBuf {
        self.artifacts_dir().join(session_id)
    }

    pub(crate) fn connect(&self) -> Result<rusqlite::Connection> {
        db::open(self)
    }

    /// A connection for ledgers such as prompts and usage, which run on the UI
    /// thread. In WAL mode their commits skip the fsync; a crash keeps them,
    /// only a power loss can drop the latest rows.
    pub(crate) fn connect_ledger(&self) -> Result<rusqlite::Connection> {
        let conn = self.connect()?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    /// Opens a session for reading. Fails with [`StoreError::NotFound`] if it was never saved.
    pub fn reader(&self, session_id: &str) -> Result<SessionReader> {
        SessionReader::open(self, session_id)
    }

    /// Takes ownership of a session for writing. Its first commit creates it.
    pub fn writer(&self, session_id: &str) -> Result<SessionWriter> {
        SessionWriter::open(self, session_id)
    }

    pub fn exists(&self, session_id: &str) -> Result<bool> {
        Ok(db::session_key(&self.connect()?, session_id)?.is_some())
    }

    pub fn list(&self, query: &CatalogQuery) -> Result<CatalogPage> {
        catalog::list(&self.connect()?, query)
    }

    pub fn session(&self, session_id: &str) -> Result<Option<CatalogSession>> {
        catalog::session(&self.connect()?, session_id)
    }

    pub fn ids_with_prefix(&self, prefix: &str, limit: u32) -> Result<Vec<String>> {
        catalog::ids_with_prefix(&self.connect()?, prefix, limit)
    }

    /// Ids of the sessions whose conversation text contains `query`, ignoring
    /// case. Queries shorter than [`SEARCH_MIN_CHARS`] match nothing.
    pub fn search_sessions(&self, query: &str) -> Result<Vec<String>> {
        catalog::search(&self.connect()?, query)
    }

    /// Copies a saved session into a new one that names it as parent.
    pub fn fork(&self, source_id: &str, target_id: &str, created_at: i64) -> Result<StoreHead> {
        let _target = lock::SessionLock::acquire(self, target_id)?;
        writer::fork(&mut self.connect()?, source_id, target_id, created_at)
    }

    /// SQLite's own consistency check of the whole database.
    pub fn integrity_issues(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare("PRAGMA quick_check")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let issues = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(issues.into_iter().filter(|issue| issue != "ok").collect())
    }

    /// Writes a consistent, compacted copy of the whole store to a new private file.
    pub fn backup_to(&self, output: &Path) -> Result<()> {
        let target = output.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("backup path is not UTF-8: {}", output.display()),
            )
        })?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        drop(options.open(output)?);
        let result = self
            .connect()
            .and_then(|conn| Ok(conn.execute("VACUUM INTO ?1", [target])?));
        if result.is_err() {
            let _ = std::fs::remove_file(output);
        }
        result.map(drop)
    }

    /// Deletes a session nobody has open.
    pub fn delete(&self, session_id: &str) -> Result<()> {
        let _lock = lock::SessionLock::acquire(self, session_id)?;
        writer::delete(&mut self.connect()?, session_id)?;
        let _ = std::fs::remove_dir_all(self.session_artifact_dir(session_id));
        Ok(())
    }
}
