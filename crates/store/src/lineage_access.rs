use std::fs::{self, File};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::access::{
    ensure_private_directory, ensure_private_directory_all, reject_symlink,
    rename_without_replacement, sync_directory, SessionReader,
};
use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::history::StoredTranscriptBlock;
use crate::lineage::{self, BranchId, LineageId, LineageSessionSnapshot};
use crate::meta::{SessionIdentity, SessionMetadata};
use crate::session_commit::{SaveReceipt, SessionCommit, SessionCommitFailure, StoreHead};

const LINEAGE_DB_FILENAME: &str = "lineage.db";
const LINEAGES_DIRECTORY: &str = "lineages";
const LINEAGE_TRASH_DIRECTORY: &str = ".trash";
const LOCKS_DIRECTORY: &str = ".locks";
const LINEAGE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq)]
pub struct LineageSessionState {
    pub lineage_id: String,
    pub identity: SessionIdentity,
    pub metadata: SessionMetadata,
    pub head: StoreHead,
    pub revision_id: String,
    pub history_root_id: String,
    pub transcript_root_id: String,
    pub history_text_bytes: u64,
    pub transcript_len: u64,
    pub side_tables: crate::session_commit::SideTableSuffixes,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct LineageReclamation {
    pub branch_heads_cleared: usize,
    pub canonical_rows_deleted: usize,
    pub objects_deleted: usize,
    pub search_segments_deleted: usize,
    pub complete: bool,
}

impl LineageReclamation {
    pub fn work_rows(self) -> usize {
        self.branch_heads_cleared
            .saturating_add(self.canonical_rows_deleted)
            .saturating_add(self.objects_deleted)
            .saturating_add(self.search_segments_deleted)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct LineageVacuum {
    pub free_pages_before: u64,
    pub free_pages_after: u64,
    pub pages_reclaimed: u64,
}

#[derive(Debug)]
struct LineageLease {
    _file: File,
}

impl LineageLease {
    fn acquire(root: &Path, lineage: &LineageId) -> Result<Self> {
        Self::acquire_named(root, lineage.as_str())
    }

    fn acquire_locator(root: &Path, branch: &BranchId) -> Result<Self> {
        // COMPAT(session-lineage-v1): share the previous per-session root lock so
        // migration and lineage lifecycle operations cannot overlap an old writer.
        Self::acquire_named(root, branch.as_str())
    }

    fn acquire_named(root: &Path, name: &str) -> Result<Self> {
        let locks = root.join(LOCKS_DIRECTORY);
        ensure_private_directory_all(root)?;
        ensure_private_directory_all(&locks)?;
        let path = locks.join(format!("{name}.lock"));
        reject_symlink(&path)?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(StoreError::OwnershipConflict {
                    owner: Some(name.to_owned()),
                });
            }
            return Err(StoreError::Io(error));
        }
        Ok(Self { _file: file })
    }
}

#[derive(Debug)]
pub struct OwnedLineageWriter {
    root: PathBuf,
    lineage: LineageId,
    branch: BranchId,
    conn: Connection,
    startup_recovery: Option<crate::session_commit::StartupRecoveryReceipt>,
    connection_invalidated: bool,
    _lease: LineageLease,
    locator_lease: Option<LineageLease>,
}

impl OwnedLineageWriter {
    pub fn open(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Self::open_inner(root.as_ref(), session_id.into(), true)
    }

    pub fn open_existing(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Self::open_inner(root.as_ref(), session_id.into(), false)
    }

    fn open_inner(root: &Path, session_id: String, create: bool) -> Result<Self> {
        let branch = BranchId::new(session_id)?;
        validate_storage_root(root)?;
        let locator_lease = LineageLease::acquire_locator(root, &branch)?;
        let located = locate_lineage(root, &branch)?;
        let is_new = located.is_none();
        let lineage = match (located, create) {
            (Some(lineage), _) => lineage,
            (None, true) if legacy_database_path(root, &branch).is_file() => {
                return Err(StoreError::LegacyMigrationRequired {
                    session_id: branch.as_str().to_owned(),
                });
            }
            (None, true) => create_lineage_database(root)?,
            (None, false) => {
                return Err(StoreError::Integrity(format!(
                    "session {} has no canonical lineage",
                    branch.as_str()
                )))
            }
        };
        let lease = LineageLease::acquire(root, &lineage)?;
        let path = lineage_database_path(root, &lineage);
        let mut conn = open_write_connection(&path, &lineage)?;
        if !lineage_exists(&conn, &lineage)? {
            lineage::create_lineage(&conn, &lineage, unix_timestamp_seconds()?)?;
        }
        crate::schema::migrate_lineage(&mut conn, lineage.as_str())?;
        let startup_recovery = lineage::recover_lineage_nonterminal_turns(
            &mut conn,
            &lineage,
            &branch,
            unix_timestamp_millis()?,
        )?;
        Ok(Self {
            root: root.to_path_buf(),
            lineage,
            branch,
            conn,
            startup_recovery,
            connection_invalidated: false,
            _lease: lease,
            locator_lease: is_new.then_some(locator_lease),
        })
    }

    pub fn lineage_id(&self) -> &str {
        self.lineage.as_str()
    }

    pub fn session_id(&self) -> &str {
        self.branch.as_str()
    }

    pub fn commit_session(
        &mut self,
        command: &SessionCommit,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        let receipt = lineage::apply_lineage_session_commit(
            &mut self.conn,
            &self.lineage,
            &self.branch,
            command,
            ObjectCompression::default(),
        )?;
        self.locator_lease = None;
        Ok(receipt)
    }

    pub fn submit_turn(
        &mut self,
        command: &crate::session_commit::SubmitTurn,
    ) -> std::result::Result<crate::session_commit::SubmitTurnReceipt, SessionCommitFailure> {
        let transaction_duration =
            smelt_perf::perf::begin_value_ms("persist:submit_turn:transaction_ms");
        smelt_perf::perf::record_value("persist:submit_turn:transactions", 1);
        let result = lineage::apply_lineage_submit_turn(
            &mut self.conn,
            &self.lineage,
            &self.branch,
            command,
            ObjectCompression::default(),
        );
        drop(transaction_duration);
        if result.is_ok() {
            smelt_perf::perf::record_value(
                "persist:submit_turn:committed_at_us",
                smelt_perf::perf::timestamp_us(),
            );
            smelt_perf::perf::record_value(
                "persist:submit_turn:history_rows",
                command.session.history.items.len() as u64,
            );
            let transcript_record_rows = command
                .session
                .transcript_records
                .as_ref()
                .map_or(0, |suffix| suffix.records.len())
                as u64;
            smelt_perf::perf::record_value(
                "persist:submit_turn:transcript_record_rows",
                transcript_record_rows,
            );
            smelt_perf::perf::record_value(
                "persist:submit_turn:index_rows",
                transcript_record_rows,
            );
            self.locator_lease = None;
        }
        result
    }

    pub fn recover_submit_turn(
        &self,
        command: &crate::session_commit::SubmitTurn,
    ) -> std::result::Result<Option<crate::session_commit::SubmitTurnReceipt>, SessionCommitFailure>
    {
        lineage::recover_lineage_submit_turn(&self.conn, &self.lineage, &self.branch, command)
    }

    pub fn transition_turn(
        &mut self,
        command: &crate::session_commit::TurnTransition,
    ) -> std::result::Result<crate::session_commit::TurnTransitionReceipt, SessionCommitFailure>
    {
        lineage::apply_lineage_turn_transition(
            &mut self.conn,
            &self.lineage,
            &self.branch,
            command,
            ObjectCompression::default(),
        )
    }

    pub fn recover_turn_transition(
        &self,
        command: &crate::session_commit::TurnTransition,
    ) -> std::result::Result<
        Option<crate::session_commit::TurnTransitionReceipt>,
        SessionCommitFailure,
    > {
        lineage::recover_lineage_turn_transition(&self.conn, &self.lineage, &self.branch, command)
    }

    pub fn store_head(&self) -> Result<StoreHead> {
        if branch_exists(&self.conn, &self.lineage, &self.branch)? {
            Ok(lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?.head)
        } else {
            Ok(StoreHead::default())
        }
    }

    pub fn last_session_commit(&self) -> Result<Option<(String, SaveReceipt)>> {
        lineage::lineage_last_session_receipt(&self.conn, &self.lineage, &self.branch)
    }

    pub fn take_startup_recovery(
        &mut self,
    ) -> Option<crate::session_commit::StartupRecoveryReceipt> {
        self.startup_recovery.take()
    }

    pub fn startup_recovery(&self) -> Option<&crate::session_commit::StartupRecoveryReceipt> {
        self.startup_recovery.as_ref()
    }

    pub fn latest_terminal_turn_id(&self) -> Result<Option<crate::session_commit::TurnId>> {
        lineage::lineage_latest_terminal_turn_id(&self.conn, &self.lineage, &self.branch)
    }

    pub fn snapshot(&self) -> Result<LineageSessionState> {
        public_snapshot(
            &self.lineage,
            lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?,
        )
    }

    pub fn history_range(&self, start: u64, end: u64) -> Result<Vec<protocol::HistoryItem>> {
        lineage::lineage_history_range(&self.conn, &self.lineage, &self.branch, start, end)
    }

    pub fn history_tail(
        &self,
        end: usize,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<Vec<protocol::HistoryItem>> {
        lineage::lineage_history_tail(
            &self.conn,
            &self.lineage,
            &self.branch,
            end,
            max_items,
            max_bytes,
        )
    }

    pub fn transcript_range(&self, start: u64, end: u64) -> Result<Vec<StoredTranscriptBlock>> {
        lineage::lineage_transcript_range(&self.conn, &self.lineage, &self.branch, start, end)
    }

    pub fn switch_branch(&mut self, session_id: impl Into<String>) -> Result<()> {
        let branch = BranchId::new(session_id.into())?;
        if !branch_exists(&self.conn, &self.lineage, &branch)? {
            return Err(StoreError::Integrity(format!(
                "branch {} is not live in lineage {}",
                branch.as_str(),
                self.lineage.as_str()
            )));
        }
        self.branch = branch;
        Ok(())
    }

    pub fn fork_current(
        &mut self,
        target_session_id: impl Into<String>,
        created_at: u64,
    ) -> Result<SaveReceipt> {
        let target = BranchId::new(target_session_id.into())?;
        let source = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
        lineage::fork_branch(
            &mut self.conn,
            &self.lineage,
            &self.branch,
            &target,
            Some(&source.revision_id),
            created_at,
        )?;
        Ok(SaveReceipt {
            session_id: target.as_str().to_owned(),
            previous: StoreHead::default(),
            current: StoreHead {
                revision: crate::session_commit::Revision::new(1),
                history_len: source.head.history_len,
                transcript_record_count: source.head.transcript_record_count,
            },
        })
    }

    pub fn rewind_to_sequence(&mut self, sequence: u64, updated_at: u64) -> Result<SaveReceipt> {
        let previous = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
        let target = lineage::branch_revision_at_sequence(
            &self.conn,
            &self.lineage,
            &self.branch,
            sequence,
        )?;
        lineage::rewind_branch(
            &mut self.conn,
            &self.lineage,
            &self.branch,
            &previous.revision_id,
            &target,
            updated_at,
        )?;
        let current = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
        Ok(SaveReceipt {
            session_id: self.branch.as_str().to_owned(),
            previous: previous.head,
            current: current.head,
        })
    }

    pub fn delete_branch(self, deleted_at: u64) -> Result<()> {
        lineage::delete_branch(&self.conn, &self.lineage, &self.branch, deleted_at)?;
        let live_branches: bool = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM lineage_branches
                 WHERE lineage_id = ?1 AND deleted_at IS NULL
             )",
            [self.lineage.as_str()],
            |row| row.get(0),
        )?;
        if live_branches {
            return self.release();
        }

        let source = self
            .database_path()
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| StoreError::Integrity("lineage database has no parent".into()))?;
        let lineages = source
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| StoreError::Integrity("lineage directory has no parent".into()))?;
        let trash = lineages.join(LINEAGE_TRASH_DIRECTORY);
        ensure_private_directory(&trash)?;
        let token = LineageId::random()?;
        let tombstone = trash.join(format!("{}.{}", self.lineage.as_str(), token.as_str()));
        self.conn
            .close()
            .map_err(|(_, error)| StoreError::from(error))?;
        sync_directory(&source)?;
        rename_without_replacement(&source, &tombstone)?;
        sync_directory(&trash)?;
        sync_directory(&lineages)?;
        sync_directory(&self.root)?;

        if fs::remove_dir_all(&tombstone).is_ok() {
            let _ = sync_directory(&trash);
            let _ = fs::remove_dir(&trash);
            let _ = sync_directory(&lineages);
            let _ = sync_directory(&self.root);
        }
        Ok(())
    }

    pub fn delete_branch_by_id(
        &mut self,
        session_id: impl Into<String>,
        deleted_at: u64,
    ) -> Result<()> {
        let branch = BranchId::new(session_id)?;
        lineage::delete_branch(&self.conn, &self.lineage, &branch, deleted_at)
    }

    pub fn database_path(&self) -> PathBuf {
        lineage_database_path(&self.root, &self.lineage)
    }

    pub fn is_staged(&self) -> bool {
        false
    }

    pub fn publish(&mut self) -> Result<PathBuf> {
        self.database_path()
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| StoreError::Integrity("lineage database has no parent".into()))
    }

    pub fn invalidate_connection(&mut self) {
        self.connection_invalidated = true;
    }

    pub fn reopen_connection(&mut self) -> Result<()> {
        if !self.connection_invalidated {
            smelt_perf::perf::record_value("store:lineage:cached_read_write", 1);
            return Ok(());
        }
        self.conn = open_write_connection(&self.database_path(), &self.lineage)?;
        self.connection_invalidated = false;
        Ok(())
    }

    pub fn reclaim_step(&mut self, max_rows: usize) -> Result<LineageReclamation> {
        if max_rows == 0 {
            return Err(StoreError::Integrity(
                "lineage reclamation row budget must be positive".into(),
            ));
        }
        let search = crate::lineage_search::reclaim_one_obsolete_search_segment(
            &self.conn,
            &self.database_path(),
            &self.lineage,
        )?;
        debug_assert!(search.segments_deleted <= 1);
        if search.segments_deleted > 0 || !search.complete {
            return Ok(LineageReclamation {
                search_segments_deleted: search.segments_deleted,
                complete: false,
                ..LineageReclamation::default()
            });
        }

        let step = lineage::reclaim_step(&mut self.conn, &self.lineage, max_rows)?;
        debug_assert!(step.work_rows() <= max_rows);
        Ok(LineageReclamation {
            branch_heads_cleared: step.branch_heads_cleared,
            canonical_rows_deleted: step.canonical_rows_deleted,
            objects_deleted: step.objects_deleted,
            search_segments_deleted: search.segments_deleted,
            complete: step.complete,
        })
    }

    pub fn vacuum(&mut self) -> Result<LineageVacuum> {
        let free_pages_before = self
            .conn
            .pragma_query_value(None, "freelist_count", |row| row.get::<_, i64>(0))?;
        let free_pages_before = nonnegative_u64(free_pages_before, "free pages before vacuum")?;
        self.conn.execute_batch(
            "PRAGMA wal_checkpoint(PASSIVE);
             PRAGMA incremental_vacuum(256);
             PRAGMA optimize;",
        )?;
        let free_pages_after = self
            .conn
            .pragma_query_value(None, "freelist_count", |row| row.get::<_, i64>(0))?;
        let free_pages_after = nonnegative_u64(free_pages_after, "free pages after vacuum")?;
        Ok(LineageVacuum {
            free_pages_before,
            free_pages_after,
            pages_reclaimed: free_pages_before.saturating_sub(free_pages_after),
        })
    }

    pub fn spawn_search_projector(&self) -> Result<crate::LineageSearchProjector> {
        crate::LineageSearchProjector::spawn(
            self.database_path(),
            self.lineage.clone(),
            self.branch.clone(),
        )
    }

    pub fn append_request_attempt(
        &mut self,
        entry: &protocol::request_log::RequestLogEntry,
        payload_mode: crate::request_audit::RequestAuditPayloadMode,
    ) -> Result<i64> {
        let transaction = self.conn.transaction()?;
        let attempt_id = crate::request_audit::append_request_attempt(
            &transaction,
            entry,
            ObjectCompression::default(),
            payload_mode,
        )?;
        transaction.execute(
            "INSERT INTO lineage_request_attempts
             (lineage_id, session_id, request_attempt_id)
             VALUES (?1, ?2, ?3)",
            (self.lineage.as_str(), self.branch.as_str(), attempt_id),
        )?;
        transaction.commit()?;
        Ok(attempt_id)
    }

    pub fn release(self) -> Result<()> {
        self.conn
            .close()
            .map_err(|(_, error)| StoreError::from(error))
    }
}

pub fn cleanup_abandoned_lineage_artifacts(root: impl AsRef<Path>, limit: usize) -> Result<usize> {
    let root = root.as_ref();
    validate_storage_root(root)?;
    let lineages = root.join(LINEAGES_DIRECTORY);
    match fs::symlink_metadata(&lineages) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(StoreError::Integrity(format!(
                "lineage root is not a private directory: {}",
                lineages.display()
            )))
        }
        Err(error) => return Err(StoreError::Io(error)),
    }

    let trash = lineages.join(LINEAGE_TRASH_DIRECTORY);
    let mut removed = 0usize;
    let mut remaining = limit;
    match fs::symlink_metadata(&trash) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            let entries = fs::read_dir(&trash)?.take(remaining).collect::<Vec<_>>();
            remaining = remaining.saturating_sub(entries.len());
            for entry in entries {
                let entry = entry?;
                let metadata = entry.file_type()?;
                if metadata.is_symlink() || !metadata.is_dir() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Some((lineage_id, _)) = name.split_once('.') else {
                    continue;
                };
                let Ok(lineage) = LineageId::from_hex(lineage_id.to_owned()) else {
                    continue;
                };
                let _lease = match LineageLease::acquire(root, &lineage) {
                    Ok(lease) => lease,
                    Err(StoreError::OwnershipConflict { .. }) => continue,
                    Err(error) => return Err(error),
                };
                fs::remove_dir_all(entry.path())?;
                sync_directory(&trash)?;
                removed = removed.saturating_add(1);
            }
        }
        Ok(_) => {
            return Err(StoreError::Integrity(format!(
                "lineage trash is not a private directory: {}",
                trash.display()
            )))
        }
        Err(error) => return Err(StoreError::Io(error)),
    }

    let published = fs::read_dir(&lineages)?.take(remaining).collect::<Vec<_>>();
    for entry in published {
        let entry = entry?;
        let metadata = entry.file_type()?;
        if metadata.is_symlink() || !metadata.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(lineage) = LineageId::from_hex(name) else {
            continue;
        };
        let _lease = match LineageLease::acquire(root, &lineage) {
            Ok(lease) => lease,
            Err(StoreError::OwnershipConflict { .. }) => continue,
            Err(error) => return Err(error),
        };
        let source = entry.path();
        let path = source.join(LINEAGE_DB_FILENAME);
        reject_symlink(&path)?;
        if !path.is_file() {
            continue;
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let live_branches: bool = conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM lineage_branches
                 WHERE lineage_id = ?1 AND deleted_at IS NULL
             )",
            [lineage.as_str()],
            |row| row.get(0),
        )?;
        drop(conn);
        if live_branches {
            continue;
        }
        ensure_private_directory(&trash)?;
        let token = LineageId::random()?;
        let tombstone = trash.join(format!("{}.{}", lineage.as_str(), token.as_str()));
        rename_without_replacement(&source, &tombstone)?;
        sync_directory(&trash)?;
        sync_directory(&lineages)?;
        sync_directory(root)?;
        fs::remove_dir_all(&tombstone)?;
        sync_directory(&trash)?;
        removed = removed.saturating_add(1);
    }
    let _ = fs::remove_dir(&trash);
    sync_directory(&lineages)?;
    sync_directory(root)?;
    Ok(removed)
}

pub fn lineage_session_ids(root: impl AsRef<Path>) -> Result<Vec<String>> {
    let root = root.as_ref();
    validate_storage_root(root)?;
    let lineages = root.join(LINEAGES_DIRECTORY);
    if !lineages.exists() {
        return Ok(Vec::new());
    }
    ensure_private_directory(&lineages)?;
    let mut ids = Vec::new();
    for entry in fs::read_dir(lineages)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() || !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(lineage) = LineageId::from_hex(name) else {
            continue;
        };
        let path = entry.path().join(LINEAGE_DB_FILENAME);
        reject_symlink(&path)?;
        if !path.is_file() {
            continue;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut statement = conn.prepare(
            "SELECT session_id FROM lineage_branches
             WHERE lineage_id = ?1 AND deleted_at IS NULL
             ORDER BY session_id",
        )?;
        let rows = statement.query_map([lineage.as_str()], |row| row.get::<_, String>(0))?;
        ids.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
    }
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(StoreError::Integrity(
            "a live session belongs to multiple lineages".into(),
        ));
    }
    Ok(ids)
}

#[derive(Debug)]
pub struct LineageSessionReader {
    lineage: LineageId,
    branch: BranchId,
    path: PathBuf,
    conn: Connection,
}

impl LineageSessionReader {
    pub fn open_existing(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        let session_id = session_id.into();
        Self::try_open_existing(root, session_id.clone())?.ok_or_else(|| {
            StoreError::Integrity(format!("session {session_id} has no canonical lineage"))
        })
    }

    pub fn try_open_existing(
        root: impl AsRef<Path>,
        session_id: impl Into<String>,
    ) -> Result<Option<Self>> {
        let _perf = smelt_perf::perf::begin("store:lineage:open_read_only");
        let root = root.as_ref();
        validate_storage_root(root)?;
        let branch = BranchId::new(session_id.into())?;
        let Some(lineage) = locate_lineage(root, &branch)? else {
            return Ok(None);
        };
        let path = lineage_database_path(root, &lineage);
        reject_symlink(&path)?;
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Some(Self {
            lineage,
            branch,
            path,
            conn,
        }))
    }

    pub fn lineage_id(&self) -> &str {
        self.lineage.as_str()
    }

    pub fn database_path(&self) -> &Path {
        &self.path
    }

    pub fn snapshot(&self) -> Result<LineageSessionState> {
        public_snapshot(
            &self.lineage,
            lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?,
        )
    }

    pub fn history_range(&self, start: u64, end: u64) -> Result<Vec<protocol::HistoryItem>> {
        lineage::lineage_history_range(&self.conn, &self.lineage, &self.branch, start, end)
    }

    pub fn history_tail(
        &self,
        end: usize,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<Vec<protocol::HistoryItem>> {
        lineage::lineage_history_tail(
            &self.conn,
            &self.lineage,
            &self.branch,
            end,
            max_items,
            max_bytes,
        )
    }

    pub fn transcript_range(&self, start: u64, end: u64) -> Result<Vec<StoredTranscriptBlock>> {
        lineage::lineage_transcript_range(&self.conn, &self.lineage, &self.branch, start, end)
    }

    pub fn transcript_object_backed_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<StoredTranscriptBlock>> {
        lineage::lineage_transcript_object_backed_range(
            &self.conn,
            &self.lineage,
            &self.branch,
            start,
            end,
        )
    }

    pub fn transcript_record_slice_with_total(
        &self,
        range: crate::TranscriptRecordRange,
        total_count: usize,
    ) -> Result<crate::TranscriptRecordSlice> {
        let start = range.start().get().min(total_count);
        let end = range.end().get().min(total_count).max(start);
        let records = self.transcript_object_backed_range(start as u64, end as u64)?;
        Ok(crate::TranscriptRecordSlice::new(
            crate::TranscriptRecordOffset::new(start),
            total_count,
            crate::TranscriptRecordHydration::ObjectBacked,
            records,
        ))
    }

    pub fn transcript_tail_for_rows_with_total(
        &self,
        total_count: usize,
        width: u16,
        target_rows: u16,
    ) -> Result<crate::TranscriptRecordSlice> {
        if total_count == 0 {
            return Ok(crate::TranscriptRecordSlice::new(
                crate::TranscriptRecordOffset::new(0),
                0,
                crate::TranscriptRecordHydration::ObjectBacked,
                Vec::new(),
            ));
        }

        let target_rows = u64::from(target_rows.max(1));
        let mut count = target_rows
            .saturating_add(1)
            .saturating_div(2)
            .min(total_count as u64) as usize;
        let mut probes = 0_u64;
        loop {
            probes = probes.saturating_add(1);
            smelt_perf::perf::record_value("transcript:resume_tail:tail_probe_count", count as u64);
            let start = total_count.saturating_sub(count);
            let slice = self.transcript_record_slice_with_total(
                crate::TranscriptRecordRange::from(start..total_count),
                total_count,
            )?;
            if crate::db::estimated_transcript_record_rows(&slice.records, width) >= target_rows
                || count == total_count
            {
                smelt_perf::perf::record_value("transcript:resume_tail:tail_probes", probes);
                return Ok(slice);
            }
            count = count.saturating_mul(2).min(total_count);
        }
    }

    pub fn search_transcript_candidate_page(
        &self,
        query: &str,
        origin_block_idx: Option<u64>,
        direction: crate::TranscriptSearchDirection,
        limit: usize,
    ) -> Result<Vec<crate::TranscriptSearchCandidate>> {
        crate::lineage_search::search_transcript_candidate_page(
            &self.conn,
            &self.path,
            &self.lineage,
            &self.branch,
            query,
            origin_block_idx,
            direction,
            limit,
        )
    }

    pub fn search_projection_status(&self) -> Result<crate::SearchProjectionStatus> {
        crate::lineage_search::search_projection_status(
            &self.conn,
            &self.path,
            &self.lineage,
            &self.branch,
        )
    }

    pub fn turns(&self) -> Result<Vec<crate::StoredTurn>> {
        lineage_turns(&self.conn, &self.lineage, &self.branch)
    }

    pub fn storage_stats(&self) -> Result<crate::StorageStats> {
        lineage_storage_stats(&self.conn, &self.path, Some(&self.branch))
    }

    pub fn doctor_report(&self) -> Result<crate::DoctorReport> {
        lineage_doctor_report(&self.conn, &self.path, &self.lineage, Some(&self.branch))
    }

    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<()> {
        crate::db::backup_connection_to(&self.conn, destination.as_ref())
    }

    pub fn query_request_attempts(
        &self,
        query: &crate::RequestAuditQuery,
    ) -> Result<Vec<crate::RequestAuditSummary>> {
        crate::request_audit::lineage_request_attempts(
            &self.conn,
            self.lineage.as_str(),
            self.branch.as_str(),
            query,
        )
    }

    pub fn request_audit_stats(&self) -> Result<crate::RequestAuditStats> {
        crate::request_audit::lineage_request_stats(
            &self.conn,
            self.lineage.as_str(),
            self.branch.as_str(),
        )
    }

    pub fn request_payloads(
        &self,
        request_attempt_id: i64,
    ) -> Result<Option<crate::RequestAuditPayloads>> {
        let belongs_to_branch = self
            .conn
            .query_row(
                "SELECT 1 FROM lineage_request_attempts
                 WHERE lineage_id = ?1 AND session_id = ?2 AND request_attempt_id = ?3",
                (
                    self.lineage.as_str(),
                    self.branch.as_str(),
                    request_attempt_id,
                ),
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !belongs_to_branch {
            return Ok(None);
        }
        crate::request_audit::request_payloads(&self.conn, request_attempt_id)
    }

    pub fn search_blob(&self) -> Result<String> {
        const CHUNK_RECORDS: u64 = 256;

        let state = self.snapshot()?;
        let transcript_len = state.transcript_len;
        let mut output = String::new();
        if transcript_len == 0 {
            let mut start = 0;
            while start < state.head.history_len.get() {
                let end = start
                    .saturating_add(CHUNK_RECORDS)
                    .min(state.head.history_len.get());
                for item in self.history_range(start, end)? {
                    let text = crate::history::history_search_text(&item)?;
                    if !text.is_empty() {
                        output.push_str(&text);
                        if !text.ends_with('\n') {
                            output.push('\n');
                        }
                    }
                }
                start = end;
            }
            return Ok(output);
        }
        let mut start = 0;
        while start < transcript_len {
            let end = start.saturating_add(CHUNK_RECORDS).min(transcript_len);
            for record in self.transcript_object_backed_range(start, end)? {
                if record.indexed_text.is_empty() {
                    continue;
                }
                output.push_str(&record.indexed_text);
                if !record.indexed_text.ends_with('\n') {
                    output.push('\n');
                }
            }
            start = end;
        }
        Ok(output)
    }

    pub fn export_history_jsonl(&self, mut out: impl Write) -> Result<()> {
        const CHUNK_ITEMS: u64 = 256;

        let history_len = self.snapshot()?.head.history_len.get();
        let mut start = 0;
        while start < history_len {
            let end = start.saturating_add(CHUNK_ITEMS).min(history_len);
            for item in self.history_range(start, end)? {
                serde_json::to_writer(&mut out, &item)?;
                out.write_all(b"\n")?;
            }
            start = end;
        }
        Ok(())
    }

    pub fn export_requests_jsonl(&self, out: impl Write) -> Result<()> {
        crate::jsonl_export::export_lineage_requests_jsonl(
            &self.conn,
            self.lineage.as_str(),
            self.branch.as_str(),
            out,
        )
    }
}

pub fn verify_lineage_backup(
    path: impl AsRef<Path>,
    lineage_id: &str,
) -> Result<crate::DoctorReport> {
    let path = path.as_ref();
    reject_symlink(path)?;
    let lineage = LineageId::from_hex(lineage_id.to_owned())?;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    if !lineage_exists(&conn, &lineage)? {
        return Err(StoreError::Integrity(format!(
            "backup does not contain lineage {}",
            lineage.as_str()
        )));
    }
    lineage_doctor_report(&conn, path, &lineage, None)
}

fn lineage_turns(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<Vec<crate::StoredTurn>> {
    let mut statement = conn.prepare(
        "SELECT turn_id, submitted_history_idx, submitted_history_hash,
                submitted_sequence, turn_kind, turn_state, continuation_of,
                created_at_ms, started_at_ms, finished_at_ms, terminal_reason
         FROM lineage_turns
         WHERE lineage_id = ?1 AND session_id = ?2
         ORDER BY turn_id",
    )?;
    let rows = statement.query_map((lineage.as_str(), branch.as_str()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<String>>(10)?,
        ))
    })?;
    let mut turns = Vec::new();
    for row in rows {
        let (
            turn_id,
            history_idx,
            history_hash,
            revision,
            kind,
            state,
            continuation_of,
            created_at_ms,
            started_at_ms,
            finished_at_ms,
            terminal_reason,
        ) = row?;
        let turn_id = positive_u64(turn_id, "turn ID")?;
        turns.push(crate::StoredTurn {
            turn_id: crate::TurnId::new(turn_id),
            submitted_history_idx: crate::HistoryIndex::new(nonnegative_u64(
                history_idx,
                "submitted history index",
            )?),
            submitted_history_hash: history_hash,
            submitted_revision: crate::Revision::new(positive_u64(
                revision,
                "submitted branch sequence",
            )?),
            kind: crate::TurnKind::from_db(&kind).ok_or_else(|| {
                StoreError::Integrity(format!("invalid lineage turn kind {kind:?}"))
            })?,
            state: crate::TurnState::from_db(&state).ok_or_else(|| {
                StoreError::Integrity(format!("invalid lineage turn state {state:?}"))
            })?,
            continuation_of: continuation_of
                .map(|value| positive_u64(value, "continuation turn ID").map(crate::TurnId::new))
                .transpose()?,
            created_at_ms: nonnegative_u64(created_at_ms, "turn created_at_ms")?,
            started_at_ms: started_at_ms
                .map(|value| nonnegative_u64(value, "turn started_at_ms"))
                .transpose()?,
            finished_at_ms: finished_at_ms
                .map(|value| nonnegative_u64(value, "turn finished_at_ms"))
                .transpose()?,
            terminal_reason,
        });
    }
    Ok(turns)
}

fn lineage_storage_stats(
    conn: &Connection,
    path: &Path,
    branch: Option<&BranchId>,
) -> Result<crate::StorageStats> {
    let history_rows = count_query(
        conn,
        "SELECT COUNT(*) FROM lineage_payload_object_refs WHERE payload_kind = 'history'",
        [],
        "history payload rows",
    )?;
    let transcript_record_rows = count_query(
        conn,
        "SELECT COUNT(*) FROM lineage_payload_object_refs WHERE payload_kind = 'transcript'",
        [],
        "transcript payload rows",
    )?;
    let (object_rows, object_raw_bytes, object_stored_bytes): (i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(raw_size), 0), COALESCE(SUM(stored_size), 0)
             FROM objects",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let request_rows = match branch {
        Some(branch) => count_query(
            conn,
            "SELECT COUNT(*) FROM lineage_request_attempts WHERE session_id = ?1",
            [branch.as_str()],
            "branch request rows",
        )?,
        None => count_query(
            conn,
            "SELECT COUNT(*) FROM lineage_request_attempts",
            [],
            "lineage request rows",
        )?,
    };
    Ok(crate::StorageStats {
        database_bytes: crate::db::file_size(path)?,
        wal_bytes: crate::db::file_size(&crate::db::sqlite_companion_path(path, "-wal"))?,
        shm_bytes: crate::db::file_size(&crate::db::sqlite_companion_path(path, "-shm"))?,
        history_rows,
        transcript_record_rows,
        object_rows: nonnegative_u64(object_rows, "object rows")?,
        object_raw_bytes: nonnegative_u64(object_raw_bytes, "object raw bytes")?,
        object_stored_bytes: nonnegative_u64(object_stored_bytes, "object stored bytes")?,
        request_rows,
    })
}

fn lineage_doctor_report(
    conn: &Connection,
    path: &Path,
    lineage: &LineageId,
    branch: Option<&BranchId>,
) -> Result<crate::DoctorReport> {
    let schema_version = crate::schema::user_version(conn)?;
    let mut issues = Vec::new();
    if let Err(error) = crate::schema::validate_lineage_schema(conn) {
        issues.push(format!("schema: {error}"));
    }
    let mut quick_check = conn.prepare("PRAGMA quick_check")?;
    for result in quick_check
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?
    {
        if result != "ok" {
            issues.push(format!("quick_check: {result}"));
        }
    }
    let mut foreign_key_check = conn.prepare("PRAGMA foreign_key_check")?;
    for (table, rowid, parent, constraint) in foreign_key_check
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    {
        issues.push(format!(
            "foreign_key_check: table={table} rowid={rowid:?} parent={parent} constraint={constraint}"
        ));
    }
    let mut branches = conn.prepare(
        "SELECT session_id FROM lineage_branches
         WHERE lineage_id = ?1 AND deleted_at IS NULL
         ORDER BY session_id",
    )?;
    let branches = branches
        .query_map([lineage.as_str()], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if branches.is_empty() {
        issues.push("lineage has no live branches".into());
    }
    for branch_id in branches {
        let result = (|| {
            let branch_id = BranchId::new(branch_id)?;
            let snapshot = lineage::lineage_session_snapshot(conn, lineage, &branch_id)?;
            lineage::validate_sequence(conn, lineage, &snapshot.history_root)?;
            lineage::validate_sequence(conn, lineage, &snapshot.transcript_root)?;
            Ok::<(), StoreError>(())
        })();
        if let Err(error) = result {
            issues.push(format!("canonical branch: {error}"));
        }
    }
    let stats = lineage_storage_stats(conn, path, branch)?;
    let search = branch
        .map(|branch| crate::lineage_search::search_projection_status(conn, path, lineage, branch))
        .transpose()?;
    Ok(crate::DoctorReport {
        schema_version,
        healthy: issues.is_empty(),
        issues,
        stats,
        search,
    })
}

fn count_query<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    field: &str,
) -> Result<u64> {
    let value = conn.query_row(sql, params, |row| row.get::<_, i64>(0))?;
    nonnegative_u64(value, field)
}

fn nonnegative_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is negative")))
}

fn positive_u64(value: i64, field: &str) -> Result<u64> {
    let value = nonnegative_u64(value, field)?;
    if value == 0 {
        return Err(StoreError::Integrity(format!("{field} must be positive")));
    }
    Ok(value)
}

fn public_snapshot(
    lineage: &LineageId,
    snapshot: LineageSessionSnapshot,
) -> Result<LineageSessionState> {
    Ok(LineageSessionState {
        lineage_id: lineage.as_str().to_owned(),
        identity: snapshot.identity,
        metadata: snapshot.metadata,
        head: snapshot.head,
        revision_id: snapshot.revision_id.as_str().to_owned(),
        history_root_id: snapshot.history_root.id().as_str().to_owned(),
        transcript_root_id: snapshot.transcript_root.id().as_str().to_owned(),
        history_text_bytes: snapshot.history_root.byte_count(),
        transcript_len: snapshot.transcript_root.item_count(),
        side_tables: snapshot.side_tables,
    })
}

fn validate_storage_root(root: &Path) -> Result<()> {
    if root.exists() {
        ensure_private_directory(root)?;
    }
    Ok(())
}

struct StagedLineageDirectory {
    path: PathBuf,
    published: bool,
}

impl StagedLineageDirectory {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            published: false,
        }
    }

    fn mark_published(&mut self) {
        self.published = true;
    }
}

impl Drop for StagedLineageDirectory {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn legacy_database_path(root: &Path, branch: &BranchId) -> PathBuf {
    root.join(branch.as_str()).join("session.db")
}

pub fn migrate_legacy_session(
    root: impl AsRef<Path>,
    session_id: impl Into<String>,
) -> Result<String> {
    let root = root.as_ref();
    validate_storage_root(root)?;
    let branch = BranchId::new(session_id.into())?;
    let _locator_lease = LineageLease::acquire_locator(root, &branch)?;
    if let Some(lineage) = locate_lineage(root, &branch)? {
        return Ok(lineage.as_str().to_owned());
    }
    if !legacy_database_path(root, &branch).is_file() {
        return Err(StoreError::Integrity(format!(
            "session {} has no previous-format database to migrate",
            branch.as_str()
        )));
    }
    migrate_legacy_session_locked(root, &branch).map(|lineage| lineage.as_str().to_owned())
}

// COMPAT(session-lineage-v1): read the immediately preceding per-session format
// only when an explicit migration command requests conversion.
fn migrate_legacy_session_locked(root: &Path, branch: &BranchId) -> Result<LineageId> {
    let legacy_dir = root.join(branch.as_str());
    let reader = SessionReader::open_existing(&legacy_dir)?;
    let full = reader.load_full_session()?.ok_or_else(|| {
        StoreError::Integrity(format!(
            "legacy session {} has no canonical state",
            branch.as_str()
        ))
    })?;
    if full.session.identity.id != branch.as_str() {
        return Err(StoreError::Integrity(format!(
            "legacy session identity {} does not match {}",
            full.session.identity.id,
            branch.as_str()
        )));
    }
    let turns = reader.turns()?;
    let lineage_hash = crate::object::sha256_hex(
        format!("smelt-legacy-lineage-v1\0{}", branch.as_str()).as_bytes(),
    );
    let lineage = LineageId::from_hex(lineage_hash[..32].to_owned())?;
    let lineages = root.join(LINEAGES_DIRECTORY);
    ensure_private_directory_all(&lineages)?;
    let directory = lineages.join(lineage.as_str());
    let staging = lineages.join(format!(".staging-{}", lineage.as_str()));
    let _lease = LineageLease::acquire(root, &lineage)?;
    if directory.exists() {
        return Err(StoreError::Integrity(format!(
            "lineage {} is published without live branch {}; run `smelt session doctor {}` before retrying migration",
            lineage.as_str(),
            branch.as_str(),
            branch.as_str()
        )));
    }
    if staging.exists() {
        ensure_private_directory(&staging)?;
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir(&staging)?;
    ensure_private_directory(&staging)?;
    let mut staged = StagedLineageDirectory::new(staging.clone());
    let path = staging.join(LINEAGE_DB_FILENAME);
    let mut conn = open_write_connection(&path, &lineage)?;
    crate::schema::migrate_lineage(&mut conn, lineage.as_str())?;
    if !lineage_exists(&conn, &lineage)? {
        lineage::create_lineage(&conn, &lineage, unix_timestamp_seconds()?)?;
    }
    if branch_exists(&conn, &lineage, branch)? {
        return Err(StoreError::Integrity(format!(
            "new migration staging database already contains branch {}",
            branch.as_str()
        )));
    }

    conn.execute(
        "ATTACH DATABASE ?1 AS legacy_import",
        [legacy_database_path(root, branch)
            .to_string_lossy()
            .as_ref()],
    )?;
    let mut transaction = conn.transaction()?;
    let history_len = u64::try_from(full.history.len())
        .map_err(|_| StoreError::Integrity("legacy history length exceeds u64".into()))?;
    let transcript_len = u64::try_from(full.transcript_records.len())
        .map_err(|_| StoreError::Integrity("legacy transcript length exceeds u64".into()))?;
    let mut metadata = full.session.metadata.clone();
    if let Some(checkpoint) = metadata.checkpoint_json.as_mut() {
        let is_past_history = checkpoint
            .get("first_live_index")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|first_live_index| first_live_index > history_len);
        if is_past_history {
            if let Some(object) = checkpoint.as_object_mut() {
                object.insert("first_live_index".into(), serde_json::json!(0));
            }
        }
    }
    let side_tables = crate::session_commit::SideTableSuffixes {
        start: crate::session_commit::HistoryIndex::ZERO,
        turn_metas: full
            .turn_metas
            .iter()
            .map(|(index, value)| {
                (
                    crate::session_commit::HistoryIndex::new(*index),
                    value.clone(),
                )
            })
            .collect(),
        metadata_snapshots: full
            .metadata_snapshots
            .iter()
            .map(|(index, value)| {
                (
                    crate::session_commit::HistoryIndex::new(*index),
                    value.clone(),
                )
            })
            .collect(),
        context_snapshots: full
            .context_snapshots
            .iter()
            .map(|(index, value)| {
                (
                    crate::session_commit::HistoryIndex::new(*index),
                    value.clone(),
                )
            })
            .collect(),
    };
    let command = SessionCommit {
        session_id: branch.as_str().to_owned(),
        expected: StoreHead::default(),
        identity: full.session.identity.clone(),
        metadata,
        history: crate::session_commit::HistorySuffix {
            start: crate::session_commit::HistoryIndex::ZERO,
            final_len: crate::session_commit::HistoryLen::new(history_len),
            items: full.history,
        },
        side_tables,
        transcript_records: Some(crate::session_commit::TranscriptRecordSuffix {
            start: crate::session_commit::TranscriptRecordIndex::ZERO,
            records: full.transcript_records,
        }),
    };
    lineage::apply_lineage_session_commit(
        &mut transaction,
        &lineage,
        branch,
        &command,
        ObjectCompression::default(),
    )
    .map_err(|failure| {
        StoreError::Integrity(format!("legacy lineage import failed: {failure:?}"))
    })?;
    transaction.execute_batch(
        "INSERT OR IGNORE INTO objects SELECT * FROM legacy_import.objects;
         INSERT INTO request_attempts SELECT * FROM legacy_import.request_attempts;
         INSERT INTO request_object_refs SELECT * FROM legacy_import.request_object_refs;
         INSERT INTO request_stats SELECT * FROM legacy_import.request_stats;",
    )?;
    transaction.execute(
        "INSERT INTO lineage_request_attempts (lineage_id, session_id, request_attempt_id)
         SELECT ?1, ?2, id FROM legacy_import.request_attempts",
        (lineage.as_str(), branch.as_str()),
    )?;
    let imported = lineage::lineage_session_snapshot(&transaction, &lineage, branch)?;
    let legacy_sequence = full.session.head.revision.get().max(1);
    if legacy_sequence != 1 {
        transaction.execute(
            "INSERT INTO lineage_branch_revisions (
                 lineage_id, session_id, branch_sequence, revision_id
             ) VALUES (?1, ?2, ?3, ?4)",
            (
                lineage.as_str(),
                branch.as_str(),
                i64::try_from(legacy_sequence).map_err(|_| {
                    StoreError::Integrity("legacy revision exceeds SQLite integer range".into())
                })?,
                imported.revision_id.as_str(),
            ),
        )?;
        transaction.execute(
            "UPDATE lineage_branches SET head_sequence = ?1
             WHERE lineage_id = ?2 AND session_id = ?3",
            (
                i64::try_from(legacy_sequence).map_err(|_| {
                    StoreError::Integrity("legacy revision exceeds SQLite integer range".into())
                })?,
                lineage.as_str(),
                branch.as_str(),
            ),
        )?;
    }
    let mut next_turn_id = 1_u64;
    for turn in turns {
        next_turn_id = next_turn_id.max(turn.turn_id.get().saturating_add(1));
        transaction.execute(
            "INSERT INTO lineage_turns (
                 lineage_id, session_id, turn_id, submitted_history_idx,
                 submitted_history_hash, submitted_revision_id, submitted_sequence,
                 turn_kind, turn_state, continuation_of, created_at_ms,
                 started_at_ms, finished_at_ms, terminal_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                i64::try_from(turn.turn_id.get()).map_err(|_| {
                    StoreError::Integrity("legacy turn ID exceeds SQLite integer range".into())
                })?,
                i64::try_from(turn.submitted_history_idx.get()).map_err(|_| {
                    StoreError::Integrity(
                        "legacy submitted history index exceeds SQLite integer range".into(),
                    )
                })?,
                turn.submitted_history_hash,
                imported.revision_id.as_str(),
                i64::try_from(turn.submitted_revision.get().max(1)).map_err(|_| {
                    StoreError::Integrity(
                        "legacy submitted revision exceeds SQLite integer range".into(),
                    )
                })?,
                turn.kind.as_str(),
                turn.state.as_str(),
                turn.continuation_of
                    .map(crate::session_commit::TurnId::get)
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| {
                        StoreError::Integrity(
                            "legacy continuation ID exceeds SQLite integer range".into(),
                        )
                    })?,
                i64::try_from(turn.created_at_ms).map_err(|_| {
                    StoreError::Integrity("legacy turn timestamp exceeds SQLite range".into())
                })?,
                turn.started_at_ms
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| {
                        StoreError::Integrity(
                            "legacy turn start timestamp exceeds SQLite range".into(),
                        )
                    })?,
                turn.finished_at_ms
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| {
                        StoreError::Integrity(
                            "legacy turn finish timestamp exceeds SQLite range".into(),
                        )
                    })?,
                turn.terminal_reason,
            ],
        )?;
    }
    transaction.execute(
        "UPDATE lineage_branches SET next_turn_id = ?1
         WHERE lineage_id = ?2 AND session_id = ?3",
        (
            i64::try_from(next_turn_id).map_err(|_| {
                StoreError::Integrity("legacy next turn ID exceeds SQLite range".into())
            })?,
            lineage.as_str(),
            branch.as_str(),
        ),
    )?;
    let migrated = lineage::lineage_session_snapshot(&transaction, &lineage, branch)?;
    let expected = StoreHead {
        revision: crate::session_commit::Revision::new(legacy_sequence),
        history_len: crate::session_commit::HistoryLen::new(history_len),
        transcript_record_count: crate::session_commit::TranscriptRecordCount::new(transcript_len),
    };
    if migrated.head != expected {
        return Err(StoreError::Integrity(format!(
            "legacy lineage import head mismatch: expected {expected:?}, got {:?}",
            migrated.head
        )));
    }
    transaction.commit()?;
    conn.execute_batch("DETACH DATABASE legacy_import")?;
    conn.close().map_err(|(_, error)| StoreError::from(error))?;
    sync_directory(&staging)?;
    rename_without_replacement(&staging, &directory)?;
    sync_directory(&lineages)?;
    staged.mark_published();
    Ok(lineage)
}

fn create_lineage_database(root: &Path) -> Result<LineageId> {
    ensure_private_directory_all(root)?;
    let lineages = root.join(LINEAGES_DIRECTORY);
    ensure_private_directory_all(&lineages)?;
    loop {
        let lineage = LineageId::random()?;
        let directory = lineages.join(lineage.as_str());
        let staging = lineages.join(format!(".staging-{}", lineage.as_str()));
        match fs::create_dir(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(StoreError::Io(error)),
        }
        let prepare = (|| {
            ensure_private_directory(&staging)?;
            let path = staging.join(LINEAGE_DB_FILENAME);
            let mut conn = open_write_connection(&path, &lineage)?;
            crate::schema::migrate_lineage(&mut conn, lineage.as_str())?;
            lineage::create_lineage(&conn, &lineage, unix_timestamp_seconds()?)?;
            conn.close().map_err(|(_, error)| StoreError::from(error))?;
            sync_directory(&staging)
        })();
        if let Err(error) = prepare {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        match rename_without_replacement(&staging, &directory) {
            Ok(()) => {
                sync_directory(&lineages)?;
                return Ok(lineage);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_dir_all(&staging);
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(StoreError::Io(error));
            }
        }
    }
}

fn locate_lineage(root: &Path, branch: &BranchId) -> Result<Option<LineageId>> {
    let lineages = root.join(LINEAGES_DIRECTORY);
    if lineages.exists() {
        ensure_private_directory(&lineages)?;
    }
    let entries = match fs::read_dir(&lineages) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::Io(error)),
    };
    let mut found = None;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_symlink() || !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(lineage) = LineageId::from_hex(name) else {
            continue;
        };
        let path = entry.path().join(LINEAGE_DB_FILENAME);
        reject_symlink(&path)?;
        if !path.is_file() {
            continue;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let present = conn
            .query_row(
                "SELECT 1 FROM lineage_branches
                 WHERE lineage_id = ?1 AND session_id = ?2 AND deleted_at IS NULL",
                (lineage.as_str(), branch.as_str()),
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if present {
            if found.is_some() {
                return Err(StoreError::Integrity(format!(
                    "session {} belongs to multiple lineages",
                    branch.as_str()
                )));
            }
            found = Some(lineage);
        }
    }
    Ok(found)
}

fn lineage_database_path(root: &Path, lineage: &LineageId) -> PathBuf {
    root.join(LINEAGES_DIRECTORY)
        .join(lineage.as_str())
        .join(LINEAGE_DB_FILENAME)
}

fn open_write_connection(path: &Path, lineage: &LineageId) -> Result<Connection> {
    reject_symlink(path)?;
    let new_database = !path.exists();
    if let Some(parent) = path.parent() {
        ensure_private_directory_all(parent)?;
    }
    let conn = Connection::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    conn.busy_timeout(LINEAGE_BUSY_TIMEOUT)?;
    if new_database {
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    }
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let actual: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !actual.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Integrity(format!(
            "lineage {} did not enter WAL mode",
            lineage.as_str()
        )));
    }
    Ok(conn)
}

fn branch_exists(conn: &Connection, lineage: &LineageId, branch: &BranchId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM lineage_branches
             WHERE lineage_id = ?1 AND session_id = ?2 AND deleted_at IS NULL",
            (lineage.as_str(), branch.as_str()),
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn lineage_exists(conn: &Connection, lineage: &LineageId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM lineage_identity WHERE lineage_id = ?1",
            [lineage.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn unix_timestamp_millis() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| {
            StoreError::Integrity(format!("system clock precedes Unix epoch: {error}"))
        })
}

fn unix_timestamp_seconds() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| {
            StoreError::Integrity(format!("system clock precedes Unix epoch: {error}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        HistoryIndex, HistoryLen, HistorySuffix, NewTurn, RequestAuditPayloadMode,
        RequestAuditQuery, SessionCostUsd, SideTableSuffixes, SubmitTurn, TranscriptRecordCount,
        TurnKind, TurnState, TurnTransition,
    };

    fn session_id(digit: char) -> String {
        digit.to_string().repeat(64)
    }

    fn metadata(updated_at: i64, title: &str) -> SessionMetadata {
        SessionMetadata {
            title: Some(title.into()),
            slug: None,
            first_user_message: None,
            cwd: Some("/workspace".into()),
            mode: Some("agent".into()),
            reasoning_effort: None,
            model: Some("test-model".into()),
            fast_mode: Some(false),
            accounting_json: None,
            checkpoint_json: None,
            context_tokens: None,
            context_tokens_history_len: None,
            display_context_tokens: None,
            session_cost_usd: SessionCostUsd::new(0.0).unwrap(),
            updated_at,
        }
    }

    fn initial_commit(id: &str) -> SessionCommit {
        SessionCommit {
            session_id: id.into(),
            expected: StoreHead::default(),
            identity: SessionIdentity {
                id: id.into(),
                created_at: 1,
                parent_id: None,
            },
            metadata: metadata(1, "initial"),
            history: HistorySuffix {
                start: HistoryIndex::ZERO,
                final_len: HistoryLen::new(1),
                items: vec![protocol::HistoryItem::system("first")],
            },
            side_tables: SideTableSuffixes::default(),
            transcript_records: None,
        }
    }

    fn request_entry(request_id: u64) -> protocol::request_log::RequestLogEntry {
        protocol::request_log::RequestLogEntry {
            request_id,
            kind: "turn".into(),
            turn_id: Some(request_id),
            ask_id: None,
            history_len: Some(1),
            timestamp_ms: request_id,
            provider_kind: "test".into(),
            api_base: "https://api.example.test".into(),
            model: "model".into(),
            url: "https://api.example.test/v1/test".into(),
            http_status: Some(200),
            body: serde_json::json!({"request": request_id}),
            prompt_cache_key: None,
            stream: true,
            system_prompt: None,
            messages: None,
            tools: None,
            response: None,
            usage: None,
            cost_usd: None,
            tokens_per_sec: None,
            elapsed_ms: Some(1),
            attempt: 1,
            error: None,
            background: false,
        }
    }

    fn transcript_record(index: u64, indexed_text: String) -> StoredTranscriptBlock {
        StoredTranscriptBlock {
            block_idx: index.saturating_mul(2),
            history_idx: Some(index),
            kind: "assistant".into(),
            tool_call_id: None,
            tool_name: None,
            content_hash: format!("{index:064x}"),
            estimated_text_bytes: indexed_text.len() as u64,
            preview_text: indexed_text.clone(),
            block_json: serde_json::json!({"Text": {"content": indexed_text.clone()}}).to_string(),
            indexed_text,
            origin_json: None,
            tool_state_json: None,
        }
    }

    fn row_count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn sqlite_storage_bytes(path: &Path) -> u64 {
        [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
        ]
        .into_iter()
        .filter_map(|path| fs::metadata(path).ok())
        .map(|metadata| metadata.len())
        .sum()
    }

    fn wait_for_search_projection(reader: &LineageSessionReader) -> crate::SearchProjectionStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let status = reader.search_projection_status().unwrap();
            if status.state == crate::SearchProjectionState::Current {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "search projection did not become current: {status:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn direct_search_candidates(
        records: &[StoredTranscriptBlock],
        query: &str,
        origin_block_idx: Option<u64>,
        direction: crate::TranscriptSearchDirection,
        limit: usize,
    ) -> Vec<crate::TranscriptSearchCandidate> {
        let mut candidates = records
            .iter()
            .filter(|record| record.indexed_text.contains(query))
            .filter(|record| match direction {
                crate::TranscriptSearchDirection::Forward => {
                    origin_block_idx.is_none_or(|origin| record.block_idx >= origin)
                }
                crate::TranscriptSearchDirection::Backward => {
                    origin_block_idx.is_none_or(|origin| record.block_idx <= origin)
                }
            })
            .map(|record| crate::TranscriptSearchCandidate {
                block_idx: record.block_idx,
                history_idx: record.history_idx,
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by_key(|candidate| candidate.block_idx);
        if direction == crate::TranscriptSearchDirection::Backward {
            candidates.reverse();
        }
        candidates.truncate(limit);
        candidates.sort_unstable_by_key(|candidate| candidate.block_idx);
        candidates
    }

    #[test]
    fn lineage_writer_owns_one_database_and_common_fork_writes_only_metadata() {
        let root = tempfile::tempdir().unwrap();
        let source_id = session_id('a');
        let target_id = session_id('b');
        let mut writer = OwnedLineageWriter::open(root.path(), &source_id).unwrap();

        assert!(matches!(
            OwnedLineageWriter::open(root.path(), &source_id),
            Err(StoreError::OwnershipConflict { .. })
        ));
        let initial = writer.commit_session(&initial_commit(&source_id)).unwrap();
        assert_eq!(initial.current.history_len, HistoryLen::new(1));
        let payloads_before = row_count(&writer.conn, "lineage_payload_object_refs");
        let nodes_before = row_count(&writer.conn, "lineage_sequence_nodes");
        let roots_before = row_count(&writer.conn, "lineage_sequence_roots");
        let storage_before = sqlite_storage_bytes(&writer.database_path());

        let mut fork_durations = Vec::with_capacity(100);
        for index in 0_u64..100 {
            let target = if index == 0 {
                target_id.clone()
            } else {
                format!("{index:064x}")
            };
            let started = std::time::Instant::now();
            let fork = writer.fork_current(&target, index + 2).unwrap();
            fork_durations.push(started.elapsed());
            assert_eq!(fork.current.history_len, HistoryLen::new(1));
        }
        fork_durations.sort_unstable();
        assert!(
            fork_durations[94] < std::time::Duration::from_millis(100),
            "100-fork p95 exceeded the interaction ceiling: {:?}",
            fork_durations[94]
        );
        assert_eq!(row_count(&writer.conn, "lineage_branches"), 101);
        let storage_growth =
            sqlite_storage_bytes(&writer.database_path()).saturating_sub(storage_before);
        assert!(
            storage_growth <= 100 * 64 * 1024,
            "100 common forks used {storage_growth} bytes of physical SQLite storage"
        );
        assert_eq!(
            row_count(&writer.conn, "lineage_payload_object_refs"),
            payloads_before
        );
        assert_eq!(
            row_count(&writer.conn, "lineage_sequence_nodes"),
            nodes_before
        );
        assert_eq!(
            row_count(&writer.conn, "lineage_sequence_roots"),
            roots_before
        );

        let target = LineageSessionReader::open_existing(root.path(), &target_id).unwrap();
        let state = target.snapshot().unwrap();
        assert_eq!(state.lineage_id, writer.lineage_id());
        assert_eq!(
            state.identity.parent_id.as_deref(),
            Some(source_id.as_str())
        );
        assert_eq!(
            target.history_range(0, 1).unwrap(),
            vec![protocol::HistoryItem::system("first")]
        );
    }

    #[test]
    fn lineage_sparse_transcript_slices_defer_nested_object_hydration() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('5');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        let mut record = transcript_record(0, "visible text".into());
        record.tool_state_json = Some(
            serde_json::json!({
                "output": {
                    "content": "visible text",
                    "metadata": {"payload": "x".repeat(16 * 1024)}
                }
            })
            .to_string(),
        );
        let mut command = initial_commit(&session_id);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: vec![record.clone()],
        });
        writer.commit_session(&command).unwrap();

        assert_eq!(
            row_count(&writer.conn, "lineage_payload_nested_object_refs"),
            1
        );
        let reader = LineageSessionReader::open_existing(root.path(), &session_id).unwrap();
        let sparse = reader
            .transcript_record_slice_with_total((0..1).into(), 1)
            .unwrap();
        assert_eq!(
            sparse.hydration,
            crate::TranscriptRecordHydration::ObjectBacked
        );
        let sparse_tool_state: serde_json::Value =
            serde_json::from_str(sparse.records[0].tool_state_json.as_ref().unwrap()).unwrap();
        assert!(sparse_tool_state
            .pointer("/output/metadata/$smelt_object_ref")
            .is_some());
        assert!(
            sparse.records[0].tool_state_json.as_ref().unwrap().len()
                < record.tool_state_json.as_ref().unwrap().len() / 4
        );

        assert_eq!(reader.transcript_range(0, 1).unwrap(), vec![record]);
    }

    #[test]
    fn lineage_history_tail_budgets_nested_objects_before_hydration() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('4');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        let item = protocol::HistoryItem::assistant(protocol::AssistantStep::with_invocations(
            None,
            None,
            Vec::new(),
            vec![protocol::ToolInvocation {
                call_id: "call-1".into(),
                name: "test".into(),
                arguments: "{}".into(),
                result: protocol::ToolOutcome {
                    content: "visible text".into(),
                    is_error: false,
                    metadata: Some(serde_json::json!({"payload": "x".repeat(16 * 1024)})),
                },
                elapsed_ms: None,
            }],
        ));
        let mut command = initial_commit(&session_id);
        command.history.items = vec![item.clone()];
        writer.commit_session(&command).unwrap();

        assert_eq!(
            writer
                .conn
                .query_row(
                    "SELECT count(*) FROM lineage_payload_nested_object_refs
                 WHERE object_role = 'metadata'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let reader = LineageSessionReader::open_existing(root.path(), &session_id).unwrap();
        assert!(reader.history_tail(1, 1, Some(1024)).unwrap().is_empty());
        assert_eq!(
            reader.history_tail(1, 1, Some(32 * 1024)).unwrap(),
            vec![item.clone()]
        );
        assert_eq!(reader.history_range(0, 1).unwrap(), vec![item]);
    }

    #[test]
    fn lineage_transcript_search_pages_exact_canonical_matches_across_chunks() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('6');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        let mut command = initial_commit(&session_id);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: (0..300)
                .map(|index| {
                    let text = if matches!(index, 2 | 257 | 299) {
                        format!("canonical needle {index}")
                    } else {
                        format!("ordinary row {index}")
                    };
                    transcript_record(index, text)
                })
                .collect(),
        });
        writer.commit_session(&command).unwrap();
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();

        let reader = LineageSessionReader::open_existing(root.path(), &session_id).unwrap();
        let search_status = wait_for_search_projection(&reader);
        assert!(search_status.total_segments > 0);
        assert_eq!(search_status.ready_segments, search_status.total_segments);
        let tail = reader
            .transcript_tail_for_rows_with_total(300, 80, 8)
            .unwrap();
        assert_eq!(tail.start, crate::TranscriptRecordOffset::new(296));
        assert_eq!(tail.total_count, 300);
        assert_eq!(
            tail.hydration,
            crate::TranscriptRecordHydration::ObjectBacked
        );
        assert_eq!(tail.records.len(), 4);
        assert_eq!(tail.records[0].block_idx, 592);
        assert_eq!(tail.records[3].block_idx, 598);

        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "needle",
                    Some(1),
                    crate::TranscriptSearchDirection::Forward,
                    2,
                )
                .unwrap(),
            vec![
                crate::TranscriptSearchCandidate {
                    block_idx: 4,
                    history_idx: Some(2),
                },
                crate::TranscriptSearchCandidate {
                    block_idx: 514,
                    history_idx: Some(257),
                },
            ]
        );
        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "needle",
                    Some(598),
                    crate::TranscriptSearchDirection::Backward,
                    2,
                )
                .unwrap(),
            vec![
                crate::TranscriptSearchCandidate {
                    block_idx: 514,
                    history_idx: Some(257),
                },
                crate::TranscriptSearchCandidate {
                    block_idx: 598,
                    history_idx: Some(299),
                },
            ]
        );
        assert!(reader
            .search_transcript_candidate_page(
                "Needle",
                None,
                crate::TranscriptSearchDirection::Forward,
                10,
            )
            .unwrap()
            .is_empty());
    }

    #[test]
    fn derived_search_matches_canonical_literals_and_keeps_text_out_of_storage() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('5');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        let long_query = "界λ".repeat(400);
        let mut records = vec![
            transcript_record(0, format!("{}{}", "p".repeat(32 * 1024 - 256), long_query)),
            transcript_record(1, "record-boundary-left".into()),
            transcript_record(2, "-right x :: café 漢字 ordinary ab".into()),
        ];
        records.extend(
            (3..43).map(|index| transcript_record(index, format!("abc false {index} bcd"))),
        );
        records.push(transcript_record(43, "contains abcd exactly".into()));
        records.push(transcript_record(44, "case-sensitive Needle".into()));

        let mut command = initial_commit(&session_id);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: records.clone(),
        });
        writer.commit_session(&command).unwrap();
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        let reader = LineageSessionReader::open_existing(root.path(), &session_id).unwrap();
        let status = wait_for_search_projection(&reader);
        assert_eq!(status.ready_segments, status.total_segments);
        assert!(status.total_segments > 0);

        let queries = [
            "p",
            "x",
            "é",
            "ab",
            "::",
            "漢字",
            "needle",
            "Needle",
            "abcd",
            "record-boundary-left-right",
            "zz",
            long_query.as_str(),
        ];
        for query in queries {
            for direction in [
                crate::TranscriptSearchDirection::Forward,
                crate::TranscriptSearchDirection::Backward,
            ] {
                for origin in [None, Some(20), Some(88)] {
                    let expected = direct_search_candidates(&records, query, origin, direction, 3);
                    assert_eq!(
                        reader
                            .search_transcript_candidate_page(query, origin, direction, 3)
                            .unwrap(),
                        expected,
                        "query={query:?} origin={origin:?} direction={direction:?}"
                    );
                }
            }
        }
        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "abcd",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    1,
                )
                .unwrap(),
            direct_search_candidates(
                &records,
                "abcd",
                None,
                crate::TranscriptSearchDirection::Forward,
                1,
            )
        );

        let canonical_search_objects: i64 = writer
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE name = 'transcript_search'
                    OR name = 'transcript_search_chars'
                    OR name LIKE 'transcript_search_fts%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(canonical_search_objects, 0);

        let search_path = reader.path.parent().unwrap().join("search.db");
        let search = Connection::open(search_path).unwrap();
        let document_columns = search
            .prepare("PRAGMA table_info(search_docs)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let table_names = search
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            document_columns,
            [
                "doc_id",
                "segment_id",
                "record_ordinal",
                "block_idx",
                "core_start",
                "core_end",
                "record_end",
            ],
            "search path={} tables={table_names:?}",
            reader.path.parent().unwrap().join("search.db").display(),
        );
        assert!(
            table_names
                .iter()
                .any(|name| name == "search_source_leaves"),
            "{table_names:?}"
        );
        let fts_sql: String = search
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name = 'search_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fts_sql.contains("content=''"), "{fts_sql}");
        assert!(fts_sql.contains("detail=none"), "{fts_sql}");
        assert!(fts_sql.contains("columnsize=0"), "{fts_sql}");
    }

    #[test]
    fn missing_corrupt_and_incomplete_search_projection_falls_back_and_rebuilds() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('4');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        let records = (0..70)
            .map(|index| {
                let text = if matches!(index, 2 | 35 | 69) {
                    format!("canonical needle {index}")
                } else {
                    format!("ordinary row {index}")
                };
                transcript_record(index, text)
            })
            .collect::<Vec<_>>();
        let mut command = initial_commit(&session_id);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: records.clone(),
        });
        writer.commit_session(&command).unwrap();

        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        let reader = LineageSessionReader::open_existing(root.path(), &session_id).unwrap();
        wait_for_search_projection(&reader);
        drop(projector);
        let expected = direct_search_candidates(
            &records,
            "needle",
            None,
            crate::TranscriptSearchDirection::Forward,
            10,
        );
        let search_path = reader.path.parent().unwrap().join("search.db");

        for path in [
            search_path.clone(),
            PathBuf::from(format!("{}-wal", search_path.display())),
            PathBuf::from(format!("{}-shm", search_path.display())),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("failed to remove derived search file: {error}"),
            }
        }
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Missing
        );
        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "needle",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    10,
                )
                .unwrap(),
            expected
        );
        loop {
            let reclamation = writer.reclaim_step(1).unwrap();
            assert_eq!(reclamation.search_segments_deleted, 0);
            if reclamation.complete {
                break;
            }
        }
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Missing
        );

        fs::write(&search_path, b"not a sqlite database").unwrap();
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Corrupt
        );
        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "needle",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    10,
                )
                .unwrap(),
            expected
        );
        let reclamation = writer.reclaim_step(1).unwrap();
        assert!(reclamation.complete);
        assert_eq!(reclamation.work_rows(), 0);
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Partial
        );

        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        wait_for_search_projection(&reader);
        drop(projector);
        let search = Connection::open(&search_path).unwrap();
        search
            .pragma_update(None, "user_version", crate::SEARCH_FORMAT_VERSION - 1)
            .unwrap();
        drop(search);
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Incompatible
        );
        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "needle",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    10,
                )
                .unwrap(),
            expected
        );
        let reclamation = writer.reclaim_step(1).unwrap();
        assert!(reclamation.complete);
        assert_eq!(reclamation.work_rows(), 0);
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Partial
        );

        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        wait_for_search_projection(&reader);
        drop(projector);
        let search = Connection::open(&search_path).unwrap();
        let rebuilt_version: i32 = search
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(rebuilt_version, crate::SEARCH_FORMAT_VERSION);
        search
            .execute(
                "UPDATE search_segments SET complete = 0
                 WHERE source_node_id = (SELECT source_node_id FROM search_segments LIMIT 1)",
                [],
            )
            .unwrap();
        drop(search);
        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "needle",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    10,
                )
                .unwrap(),
            expected
        );
        let reclamation = writer.reclaim_step(1).unwrap();
        assert!(reclamation.complete);
        assert_eq!(reclamation.work_rows(), 0);
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Partial
        );
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        let rebuilt = wait_for_search_projection(&reader);
        assert_eq!(rebuilt.ready_segments, rebuilt.total_segments);
        drop(projector);

        let search = Connection::open(&search_path).unwrap();
        search
            .execute("UPDATE search_short_postings SET docs = x'80'", [])
            .unwrap();
        drop(search);
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Corrupt
        );
        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "n",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    10,
                )
                .unwrap(),
            direct_search_candidates(
                &records,
                "n",
                None,
                crate::TranscriptSearchDirection::Forward,
                10,
            )
        );
        let reclamation = writer.reclaim_step(1).unwrap();
        assert!(reclamation.complete);
        assert_eq!(reclamation.work_rows(), 0);
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Missing
        );
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let malformed = Connection::open_with_flags(
                &search_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .and_then(|search| {
                search.query_row(
                    "SELECT EXISTS(SELECT 1 FROM search_short_postings WHERE docs = x'80')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
            })
            .unwrap_or(true);
            if !malformed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "malformed derived search postings were not rebuilt: {:?}",
                projector.latest_error()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let rebuilt = wait_for_search_projection(&reader);
        assert_eq!(rebuilt.ready_segments, rebuilt.total_segments);
    }

    #[test]
    fn fork_append_and_rewind_reuse_and_filter_immutable_search_segments() {
        let root = tempfile::tempdir().unwrap();
        let source_id = session_id('3');
        let target_id = session_id('2');
        let mut writer = OwnedLineageWriter::open(root.path(), &source_id).unwrap();
        let mut records = (0..1024)
            .map(|index| transcript_record(index, format!("shared transcript row {index}")))
            .collect::<Vec<_>>();
        records[1023] = transcript_record(2048, "source-high marker".into());
        let mut command = initial_commit(&source_id);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records,
        });
        writer.commit_session(&command).unwrap();

        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        let source = LineageSessionReader::open_existing(root.path(), &source_id).unwrap();
        let source_status = wait_for_search_projection(&source);
        assert_eq!(source_status.total_segments, 1);
        assert_eq!(source_status.ready_segments, 1);
        drop(projector);
        let search_path = source.path.parent().unwrap().join("search.db");
        let segment_count = || {
            Connection::open_with_flags(
                &search_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM search_segments WHERE complete = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(segment_count(), 1);

        writer.fork_current(&target_id, 2).unwrap();
        let target = LineageSessionReader::open_existing(root.path(), &target_id).unwrap();
        let target_status = target.search_projection_status().unwrap();
        assert_eq!(target_status.state, crate::SearchProjectionState::Current);
        assert_eq!(target_status.ready_segments, 1);
        assert_eq!(segment_count(), 1);

        writer.switch_branch(&target_id).unwrap();
        let state = writer.snapshot().unwrap();
        let mut metadata = state.metadata.clone();
        metadata.updated_at = 3;
        let append = SessionCommit {
            session_id: target_id.clone(),
            expected: state.head,
            identity: state.identity,
            metadata,
            history: HistorySuffix {
                start: HistoryIndex::new(state.head.history_len.get()),
                final_len: state.head.history_len,
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(state.head.history_len.get()),
                ..SideTableSuffixes::default()
            },
            transcript_records: Some(crate::TranscriptRecordSuffix {
                start: crate::TranscriptRecordIndex::new(state.transcript_len),
                records: vec![transcript_record(1024, "target-only suffix marker".into())],
            }),
        };
        writer.commit_session(&append).unwrap();
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        let target_status = wait_for_search_projection(&target);
        assert_eq!(target_status.total_segments, 2);
        assert_eq!(target_status.ready_segments, 2);
        assert_eq!(segment_count(), 2);
        assert_eq!(
            target
                .search_transcript_candidate_page(
                    "target-only",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    10,
                )
                .unwrap(),
            vec![crate::TranscriptSearchCandidate {
                block_idx: 2048,
                history_idx: Some(1024),
            }]
        );
        assert_eq!(
            target
                .search_transcript_candidate_page(
                    "marker",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    1,
                )
                .unwrap(),
            vec![crate::TranscriptSearchCandidate {
                block_idx: 2048,
                history_idx: Some(1024),
            }]
        );
        assert_eq!(
            target
                .search_transcript_candidate_page(
                    "marker",
                    None,
                    crate::TranscriptSearchDirection::Backward,
                    1,
                )
                .unwrap(),
            vec![crate::TranscriptSearchCandidate {
                block_idx: 4096,
                history_idx: Some(2048),
            }]
        );
        assert!(source
            .search_transcript_candidate_page(
                "target-only",
                None,
                crate::TranscriptSearchDirection::Forward,
                10,
            )
            .unwrap()
            .is_empty());
        drop(projector);

        writer.rewind_to_sequence(1, 4).unwrap();
        let rewound_status = target.search_projection_status().unwrap();
        assert_eq!(rewound_status.state, crate::SearchProjectionState::Current);
        assert_eq!(rewound_status.total_segments, 1);
        assert_eq!(rewound_status.ready_segments, 1);
        assert_eq!(segment_count(), 2);
        assert!(target
            .search_transcript_candidate_page(
                "target-only",
                None,
                crate::TranscriptSearchDirection::Forward,
                10,
            )
            .unwrap()
            .is_empty());

        let mut search_segments_deleted = 0usize;
        for _ in 0..10_000 {
            let step = writer.reclaim_step(1).unwrap();
            assert!(step.work_rows() <= 1);
            search_segments_deleted =
                search_segments_deleted.saturating_add(step.search_segments_deleted);
            if step.complete {
                break;
            }
        }
        assert_eq!(search_segments_deleted, 1);
        assert_eq!(segment_count(), 1);
        let search = Connection::open_with_flags(
            &search_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let (docs, obsolete_fts_hits): (i64, i64) = search
            .query_row(
                "SELECT (SELECT COUNT(*) FROM search_docs),
                        (SELECT COUNT(*) FROM search_fts WHERE search_fts MATCH 'tar')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(docs > 0);
        assert_eq!(obsolete_fts_hits, 0);
        assert_eq!(writer.reclaim_step(1).unwrap().work_rows(), 0);
        assert_eq!(
            source
                .search_transcript_candidate_page(
                    "source-high",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    10,
                )
                .unwrap(),
            vec![crate::TranscriptSearchCandidate {
                block_idx: 4096,
                history_idx: Some(2048),
            }]
        );
    }

    #[test]
    fn final_branch_deletion_retires_lineage_without_invalidating_a_fork() {
        let root = tempfile::tempdir().unwrap();
        let source_id = session_id('6');
        let target_id = session_id('7');
        let mut writer = OwnedLineageWriter::open(root.path(), &source_id).unwrap();
        writer.commit_session(&initial_commit(&source_id)).unwrap();
        writer.fork_current(&target_id, 2).unwrap();
        let lineage_directory = writer.database_path().parent().unwrap().to_path_buf();

        writer.delete_branch(3).unwrap();
        assert!(lineage_directory.is_dir());
        let target = LineageSessionReader::open_existing(root.path(), &target_id).unwrap();
        assert_eq!(target.snapshot().unwrap().head.history_len.get(), 1);
        drop(target);

        let writer = OwnedLineageWriter::open_existing(root.path(), &target_id).unwrap();
        writer.delete_branch(4).unwrap();
        assert!(!lineage_directory.exists());
        assert!(lineage_session_ids(root.path()).unwrap().is_empty());
    }

    #[test]
    fn published_lineage_without_a_live_branch_is_retired_after_interruption() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('9');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        writer.commit_session(&initial_commit(&session_id)).unwrap();
        let lineage_directory = writer.database_path().parent().unwrap().to_path_buf();
        writer.delete_branch_by_id(&session_id, 2).unwrap();
        writer.release().unwrap();

        assert!(lineage_directory.is_dir());
        assert_eq!(
            cleanup_abandoned_lineage_artifacts(root.path(), 1).unwrap(),
            1
        );
        assert!(!lineage_directory.exists());
    }

    #[test]
    fn published_lineage_cleanup_skips_an_active_stable_lease() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('5');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        writer.commit_session(&initial_commit(&session_id)).unwrap();
        let lineage_directory = writer.database_path().parent().unwrap().to_path_buf();
        writer.delete_branch_by_id(&session_id, 2).unwrap();

        assert_eq!(
            cleanup_abandoned_lineage_artifacts(root.path(), 1).unwrap(),
            0
        );
        assert!(lineage_directory.is_dir());
        writer.release().unwrap();
        assert_eq!(
            cleanup_abandoned_lineage_artifacts(root.path(), 1).unwrap(),
            1
        );
        assert!(!lineage_directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn lineage_cleanup_rejects_a_symlinked_trash_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let lineages = root.path().join(LINEAGES_DIRECTORY);
        ensure_private_directory_all(&lineages).unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::write(external.path().join("sentinel"), b"keep").unwrap();
        symlink(external.path(), lineages.join(LINEAGE_TRASH_DIRECTORY)).unwrap();

        assert!(matches!(
            cleanup_abandoned_lineage_artifacts(root.path(), 1),
            Err(StoreError::Integrity(_))
        ));
        assert_eq!(fs::read(external.path().join("sentinel")).unwrap(), b"keep");
    }

    #[test]
    fn abandoned_lineage_trash_is_removed_under_its_stable_lease() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('8');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        writer.commit_session(&initial_commit(&session_id)).unwrap();
        let lineage = LineageId::from_hex(writer.lineage_id().to_owned()).unwrap();
        let source = writer.database_path().parent().unwrap().to_path_buf();
        writer.release().unwrap();

        let trash = root
            .path()
            .join(LINEAGES_DIRECTORY)
            .join(LINEAGE_TRASH_DIRECTORY);
        ensure_private_directory(&trash).unwrap();
        let tombstone = trash.join(format!("{}.interrupted", lineage.as_str()));
        fs::rename(&source, &tombstone).unwrap();
        assert_eq!(
            cleanup_abandoned_lineage_artifacts(root.path(), 1).unwrap(),
            1
        );
        assert!(!tombstone.exists());
        assert_eq!(
            cleanup_abandoned_lineage_artifacts(root.path(), 1).unwrap(),
            0
        );
    }

    #[test]
    fn lineage_doctor_backup_stats_and_vacuum_cover_canonical_database() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('7');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        writer.commit_session(&initial_commit(&session_id)).unwrap();
        writer
            .append_request_attempt(&request_entry(1), RequestAuditPayloadMode::Full)
            .unwrap();
        writer.vacuum().unwrap();
        let lineage_id = writer.lineage_id().to_owned();
        writer.release().unwrap();

        let reader = LineageSessionReader::open_existing(root.path(), &session_id).unwrap();
        let report = reader.doctor_report().unwrap();
        assert!(report.healthy, "{:?}", report.issues);
        assert_eq!(report.stats.history_rows, 1);
        assert_eq!(report.stats.request_rows, 1);
        assert!(report.stats.object_rows >= 2);
        assert!(reader.turns().unwrap().is_empty());

        let backup = root.path().join("lineage-backup.db");
        reader.backup_to(&backup).unwrap();
        let backup_report = verify_lineage_backup(&backup, &lineage_id).unwrap();
        assert!(backup_report.healthy, "{:?}", backup_report.issues);
        assert_eq!(backup_report.stats.history_rows, 1);
        assert_eq!(backup_report.stats.request_rows, 1);
    }

    #[test]
    fn request_audit_and_exports_are_branch_local() {
        let root = tempfile::tempdir().unwrap();
        let source_id = session_id('8');
        let target_id = session_id('9');
        let mut writer = OwnedLineageWriter::open(root.path(), &source_id).unwrap();
        writer.commit_session(&initial_commit(&source_id)).unwrap();
        writer.fork_current(&target_id, 2).unwrap();

        let source_attempt = writer
            .append_request_attempt(&request_entry(1), RequestAuditPayloadMode::Full)
            .unwrap();
        writer.switch_branch(&target_id).unwrap();
        let target_attempt = writer
            .append_request_attempt(&request_entry(2), RequestAuditPayloadMode::Full)
            .unwrap();
        writer.release().unwrap();

        let source = LineageSessionReader::open_existing(root.path(), &source_id).unwrap();
        let target = LineageSessionReader::open_existing(root.path(), &target_id).unwrap();
        let source_rows = source
            .query_request_attempts(&RequestAuditQuery::default())
            .unwrap();
        let target_rows = target
            .query_request_attempts(&RequestAuditQuery::default())
            .unwrap();
        assert_eq!(source_rows.len(), 1);
        assert_eq!(source_rows[0].id, source_attempt);
        assert_eq!(target_rows.len(), 1);
        assert_eq!(target_rows[0].id, target_attempt);
        assert_eq!(source.request_audit_stats().unwrap().request_count, 1);
        assert_eq!(target.request_audit_stats().unwrap().request_count, 1);
        assert!(source.request_payloads(target_attempt).unwrap().is_none());
        assert!(target.request_payloads(source_attempt).unwrap().is_none());

        let mut source_export = Vec::new();
        source.export_requests_jsonl(&mut source_export).unwrap();
        let mut target_export = Vec::new();
        target.export_requests_jsonl(&mut target_export).unwrap();
        assert!(String::from_utf8(source_export)
            .unwrap()
            .contains("\"request_id\":\"1\""));
        assert!(String::from_utf8(target_export)
            .unwrap()
            .contains("\"request_id\":\"2\""));

        let mut history_export = Vec::new();
        target.export_history_jsonl(&mut history_export).unwrap();
        let exported: protocol::HistoryItem =
            serde_json::from_slice(history_export.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(exported, protocol::HistoryItem::system("first"));
    }

    #[test]
    fn lineage_writer_reopens_and_rewinds_by_immutable_branch_sequence() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('c');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let first = writer.commit_session(&initial_commit(&id)).unwrap();
        let lineage_id = writer.lineage_id().to_owned();
        writer.release().unwrap();

        let mut writer = OwnedLineageWriter::open_existing(root.path(), &id).unwrap();
        assert_eq!(writer.lineage_id(), lineage_id);
        let mut append = initial_commit(&id);
        append.expected = first.current;
        append.metadata = metadata(2, "append");
        append.history = HistorySuffix {
            start: HistoryIndex::new(1),
            final_len: HistoryLen::new(2),
            items: vec![protocol::HistoryItem::system("second")],
        };
        let second = writer.commit_session(&append).unwrap();
        assert_eq!(second.current.revision.get(), 2);

        let rewind = writer.rewind_to_sequence(1, 3).unwrap();
        assert_eq!(rewind.previous, second.current);
        assert_eq!(rewind.current.revision.get(), 3);
        assert_eq!(rewind.current.history_len, HistoryLen::new(1));
        assert_eq!(
            rewind.current.transcript_record_count,
            TranscriptRecordCount::ZERO
        );
        assert_eq!(
            writer.history_range(0, 1).unwrap(),
            vec![protocol::HistoryItem::system("first")]
        );
    }

    #[test]
    fn legacy_session_migration_preserves_head_and_canonical_content() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('e');
        let mut legacy = crate::OwnedSessionWriter::open(root.path(), &id).unwrap();
        let first = legacy.commit_session(&initial_commit(&id)).unwrap();
        legacy.publish().unwrap();
        let mut append = initial_commit(&id);
        append.expected = first.current;
        append.metadata = metadata(2, "legacy append");
        append.history = HistorySuffix {
            start: HistoryIndex::new(1),
            final_len: HistoryLen::new(2),
            items: vec![protocol::HistoryItem::system("legacy second")],
        };
        let second = legacy.commit_session(&append).unwrap();
        legacy
            .append_request_attempt(&request_entry(7), RequestAuditPayloadMode::Full)
            .unwrap();
        legacy.release().unwrap();

        assert!(matches!(
            OwnedLineageWriter::open(root.path(), &id),
            Err(StoreError::LegacyMigrationRequired { .. })
        ));
        let lineage_id = migrate_legacy_session(root.path(), &id).unwrap();
        let lineage = OwnedLineageWriter::open_existing(root.path(), &id).unwrap();
        assert_eq!(lineage.lineage_id(), lineage_id);
        assert_eq!(lineage.store_head().unwrap(), second.current);
        assert_eq!(lineage.snapshot().unwrap().metadata, append.metadata);
        assert_eq!(
            lineage.history_range(0, 2).unwrap(),
            vec![
                protocol::HistoryItem::system("first"),
                protocol::HistoryItem::system("legacy second")
            ]
        );
        assert!(legacy_database_path(root.path(), &BranchId::new(&id).unwrap()).is_file());
        lineage.release().unwrap();
        let migrated_requests = LineageSessionReader::open_existing(root.path(), &id)
            .unwrap()
            .query_request_attempts(&RequestAuditQuery::default())
            .unwrap();
        assert_eq!(migrated_requests.len(), 1);
        assert_eq!(migrated_requests[0].request_id.as_deref(), Some("7"));

        let reopened = OwnedLineageWriter::open_existing(root.path(), &id).unwrap();
        assert_eq!(reopened.store_head().unwrap(), second.current);
    }

    #[test]
    fn interrupted_legacy_migration_staging_is_discarded_before_retry() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('e');
        let mut legacy = crate::OwnedSessionWriter::open(root.path(), &id).unwrap();
        legacy.commit_session(&initial_commit(&id)).unwrap();
        legacy.publish().unwrap();
        legacy.release().unwrap();

        let lineage_hash =
            crate::object::sha256_hex(format!("smelt-legacy-lineage-v1\0{id}").as_bytes());
        let lineage = LineageId::from_hex(lineage_hash[..32].to_owned()).unwrap();
        let lineages = root.path().join(LINEAGES_DIRECTORY);
        ensure_private_directory_all(&lineages).unwrap();
        let staging = lineages.join(format!(".staging-{}", lineage.as_str()));
        ensure_private_directory(&staging).unwrap();
        fs::write(staging.join(LINEAGE_DB_FILENAME), b"interrupted migration").unwrap();

        assert_eq!(
            migrate_legacy_session(root.path(), &id).unwrap(),
            lineage.as_str()
        );
        assert!(!staging.exists());
        let migrated = LineageSessionReader::open_existing(root.path(), &id).unwrap();
        assert_eq!(migrated.history_range(0, 1).unwrap().len(), 1);
    }

    #[test]
    fn failed_legacy_migration_is_not_published_and_retries_cleanly() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('f');
        let mut legacy = crate::OwnedSessionWriter::open(root.path(), &id).unwrap();
        legacy.commit_session(&initial_commit(&id)).unwrap();
        legacy
            .append_request_attempt(&request_entry(9), RequestAuditPayloadMode::Full)
            .unwrap();
        legacy.publish().unwrap();
        legacy.release().unwrap();

        let legacy_path = legacy_database_path(root.path(), &BranchId::new(&id).unwrap());
        let db = crate::SessionDb::open(&legacy_path).unwrap();
        let original_hash: String = db
            .connection()
            .query_row(
                "SELECT object_hash FROM request_object_refs LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let missing_hash = "0".repeat(64);
        db.connection()
            .execute_batch("PRAGMA foreign_keys = OFF")
            .unwrap();
        db.connection()
            .execute(
                "UPDATE request_object_refs SET object_hash = ?1",
                [&missing_hash],
            )
            .unwrap();
        drop(db);

        assert!(migrate_legacy_session(root.path(), &id).is_err());
        assert!(
            LineageSessionReader::try_open_existing(root.path(), &id)
                .unwrap()
                .is_none(),
            "a failed import must not publish a visible lineage branch"
        );
        assert_eq!(
            fs::read_dir(root.path().join(LINEAGES_DIRECTORY))
                .unwrap()
                .count(),
            0,
            "failed staging data is removed"
        );

        let db = crate::SessionDb::open(&legacy_path).unwrap();
        db.connection()
            .execute_batch("PRAGMA foreign_keys = OFF")
            .unwrap();
        db.connection()
            .execute(
                "UPDATE request_object_refs SET object_hash = ?1",
                [&original_hash],
            )
            .unwrap();
        drop(db);

        migrate_legacy_session(root.path(), &id).unwrap();
        let migrated = LineageSessionReader::open_existing(root.path(), &id).unwrap();
        assert_eq!(migrated.history_range(0, 1).unwrap().len(), 1);
        assert_eq!(
            migrated
                .query_request_attempts(&RequestAuditQuery::default())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn turn_submission_is_atomic_idempotent_and_recovered_on_reopen() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('d');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let submit = SubmitTurn {
            session: initial_commit(&id),
            turn: NewTurn {
                kind: TurnKind::User,
                submitted_history_idx: HistoryIndex::ZERO,
                continuation_of: None,
                created_at_ms: 10,
            },
        };
        let receipt = writer.submit_turn(&submit).unwrap();
        assert_eq!(receipt.turn_id.get(), 1);
        assert_eq!(
            writer.recover_submit_turn(&submit).unwrap(),
            Some(receipt.clone())
        );
        assert_eq!(writer.submit_turn(&submit).unwrap(), receipt);

        let mut running_session = initial_commit(&id);
        running_session.expected = receipt.session.current;
        running_session.metadata = metadata(2, "running");
        running_session.history = HistorySuffix {
            start: HistoryIndex::new(1),
            final_len: HistoryLen::new(1),
            items: Vec::new(),
        };
        let running = TurnTransition {
            session: running_session,
            turn_id: receipt.turn_id,
            state: TurnState::Running,
            at_ms: 11,
            terminal_reason: None,
        };
        let running_receipt = writer.transition_turn(&running).unwrap();
        assert_eq!(running_receipt.state, TurnState::Running);
        assert_eq!(
            writer.recover_turn_transition(&running).unwrap(),
            Some(running_receipt.clone())
        );
        assert_eq!(writer.transition_turn(&running).unwrap(), running_receipt);
        writer.release().unwrap();

        let mut reopened = OwnedLineageWriter::open_existing(root.path(), &id).unwrap();
        let recovery = reopened
            .take_startup_recovery()
            .expect("running turn is interrupted before the writer becomes available");
        assert_eq!(recovery.interrupted_turns, vec![receipt.turn_id]);
        assert_eq!(recovery.session.previous, running_receipt.session.current);
        assert_eq!(
            recovery.session.current.revision.get(),
            running_receipt.session.current.revision.get() + 1
        );
        assert_eq!(
            reopened.latest_terminal_turn_id().unwrap(),
            Some(receipt.turn_id)
        );
    }
}
