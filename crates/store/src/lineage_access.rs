use std::cell::RefCell;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};

use crate::catalog::{Catalog, CatalogAvailability, CatalogSession};
use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::filesystem::{
    ensure_private_directory, ensure_private_directory_all, reject_symlink,
    rename_without_replacement, sync_directory,
};
use crate::history::StoredTranscriptBlock;
use crate::lineage::{self, BranchId, LineageId, LineageSessionSnapshot};
use crate::meta::{SessionIdentity, SessionMetadata};
use crate::session_commit::{SaveReceipt, SessionCommit, SessionCommitFailure, StoreHead};

mod maintenance;
use maintenance::*;
mod storage;
use storage::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineageSessionLocation {
    pub session_id: String,
    pub lineage_id: String,
}

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
    /// Seed rows, frontier nodes and sweep candidates examined in this step.
    pub rows_examined: usize,
    /// An exhausted bounded scan advanced the pass, without requiring a deletion.
    pub phase_advanced: bool,
    pub complete: bool,
}

impl LineageReclamation {
    pub fn work_rows(self) -> usize {
        self.rows_examined.max(
            self.branch_heads_cleared
                .saturating_add(self.canonical_rows_deleted)
                .saturating_add(self.objects_deleted),
        )
    }

    pub fn made_progress(self) -> bool {
        self.work_rows() > 0 || self.phase_advanced
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct LineageVacuum {
    pub free_pages_before: u64,
    pub free_pages_after: u64,
    pub pages_reclaimed: u64,
    /// Active read snapshots can defer truncation without blocking maintenance.
    pub wal_truncated: bool,
}

#[derive(Debug)]
struct LineageLease {
    _file: File,
}

impl LineageLease {
    fn acquire(root: &Path, lineage: &LineageId) -> Result<Self> {
        Self::acquire_named(root, lineage.as_str(), false)
    }

    fn acquire_shared(root: &Path, lineage: &LineageId) -> Result<Self> {
        Self::acquire_named(root, lineage.as_str(), true)
    }

    fn acquire_branch(root: &Path, branch: &BranchId) -> Result<Self> {
        Self::acquire_named(root, branch.as_str(), false)
    }

    fn try_exclusive(&self) -> Result<bool> {
        match fs4::FileExt::try_lock(&self._file) {
            Ok(()) => Ok(true),
            Err(fs4::TryLockError::WouldBlock) => Ok(false),
            Err(fs4::TryLockError::Error(error)) => Err(StoreError::Io(error)),
        }
    }

    fn acquire_named(root: &Path, name: &str, shared: bool) -> Result<Self> {
        let layout = crate::SessionStoreLayout::from_sessions_root(root);
        ensure_private_directory_all(root)?;
        ensure_private_directory_all(&layout.locks_dir())?;
        let path = layout.lineage_lock_path(name);
        reject_symlink(&path)?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        let result = if shared {
            fs4::FileExt::try_lock_shared(&file)
        } else {
            fs4::FileExt::try_lock(&file)
        };
        match result {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs4::TryLockError::WouldBlock) => Err(StoreError::OwnershipConflict {
                owner: Some(name.to_owned()),
            }),
            Err(fs4::TryLockError::Error(error)) => Err(StoreError::Io(error)),
        }
    }
}

pub struct OwnedLineageWriter {
    sessions_root: PathBuf,
    lineage: LineageId,
    branch: BranchId,
    conn: Connection,
    startup_recovery: Option<crate::session_commit::StartupRecoveryResult>,
    connection_invalidated: bool,
    catalog: RefCell<Option<Catalog>>,
    // Writers share the database, but cleanup must wait for every writer to close.
    lineage_lease: LineageLease,
    branch_lease: LineageLease,
}

impl std::fmt::Debug for OwnedLineageWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedLineageWriter")
            .field("sessions_root", &self.sessions_root)
            .field("lineage", &self.lineage)
            .field("branch", &self.branch)
            .field("startup_recovery", &self.startup_recovery)
            .field("connection_invalidated", &self.connection_invalidated)
            .field("branch_lease", &self.branch_lease)
            .finish_non_exhaustive()
    }
}

fn record_submit_turn_commit(history_rows: usize, transcript_record_rows: usize) {
    smelt_perf::perf::record_value(
        "persist:submit_turn:committed_at_us",
        smelt_perf::perf::timestamp_us(),
    );
    smelt_perf::perf::record_value("persist:submit_turn:history_rows", history_rows as u64);
    smelt_perf::perf::record_value(
        "persist:submit_turn:transcript_record_rows",
        transcript_record_rows as u64,
    );
    smelt_perf::perf::record_value(
        "persist:submit_turn:index_rows",
        transcript_record_rows as u64,
    );
}

impl OwnedLineageWriter {
    pub fn open(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Self::acquire(root.as_ref(), session_id.into(), true)?.finish_startup()
    }

    pub fn open_existing(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Self::acquire(root.as_ref(), session_id.into(), false)?.finish_startup()
    }

    pub fn open_existing_in_lineage(
        root: impl AsRef<Path>,
        lineage_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Self> {
        Self::acquire_existing_in_lineage(root.as_ref(), lineage_id.into(), session_id.into())?
            .finish_startup()
    }

    pub(crate) fn acquire_existing_in_lineage(
        root: &Path,
        lineage_id: String,
        session_id: String,
    ) -> Result<Self> {
        let branch = BranchId::new(session_id)?;
        validate_storage_root(root)?;
        let branch_lease = LineageLease::acquire_branch(root, &branch)?;
        let lineage = LineageId::from_hex(lineage_id)?;
        let lease = LineageLease::acquire_shared(root, &lineage)?;
        let path = lineage_database_path(root, &lineage);
        reject_symlink(&path)?;
        if !path.is_file() {
            return Err(StoreError::Integrity(format!(
                "catalog lineage {} for session {} does not exist",
                lineage.as_str(),
                branch.as_str()
            )));
        }
        let mut conn = open_write_connection(&path, &lineage)?;
        crate::schema::initialize_lineage_schema(&mut conn)?;
        if !lineage_exists(&conn, &lineage)? {
            return Err(StoreError::Integrity(format!(
                "catalog lineage {} for session {} has no identity",
                lineage.as_str(),
                branch.as_str()
            )));
        }
        if !branch_exists(&conn, &lineage, &branch)? {
            return Err(StoreError::Integrity(format!(
                "session {} has no branch in catalog lineage {}",
                branch.as_str(),
                lineage.as_str()
            )));
        }
        Ok(Self::from_acquired(
            root,
            lineage,
            branch,
            conn,
            lease,
            branch_lease,
        ))
    }

    /// Acquires storage and leases without exposing a ready public writer.
    pub(crate) fn acquire(root: &Path, session_id: String, create: bool) -> Result<Self> {
        let branch = BranchId::new(session_id)?;
        validate_storage_root(root)?;
        let branch_lease = LineageLease::acquire_branch(root, &branch)?;
        let located = locate_lineage(root, &branch)?;
        let lineage = match (located, create) {
            (Some(lineage), _) => lineage,
            (None, true) => create_lineage_database(root)?,
            (None, false) => {
                return Err(StoreError::Integrity(format!(
                    "session {} has no canonical lineage",
                    branch.as_str()
                )))
            }
        };
        let lease = LineageLease::acquire_shared(root, &lineage)?;
        let path = lineage_database_path(root, &lineage);
        let mut conn = open_write_connection(&path, &lineage)?;
        if !lineage_exists(&conn, &lineage)? {
            lineage::create_lineage(&conn, &lineage, unix_timestamp_seconds()?)?;
        }
        crate::schema::initialize_lineage_schema(&mut conn)?;
        Ok(Self::from_acquired(
            root,
            lineage,
            branch,
            conn,
            lease,
            branch_lease,
        ))
    }

    fn from_acquired(
        root: &Path,
        lineage: LineageId,
        branch: BranchId,
        conn: Connection,
        lease: LineageLease,
        branch_lease: LineageLease,
    ) -> Self {
        Self {
            sessions_root: root.to_path_buf(),
            lineage,
            branch,
            conn,
            startup_recovery: None,
            connection_invalidated: false,
            catalog: RefCell::new(None),
            lineage_lease: lease,
            branch_lease,
        }
    }

    pub(crate) fn finish_startup(mut self) -> Result<Self> {
        if lineage::lineage_has_nonterminal_turns(&self.conn, &self.lineage, &self.branch)? {
            let _catalog_pending = crate::catalog::mark_catalog_session_pending(
                &self.sessions_root,
                self.branch.as_str(),
            )?;
            self.startup_recovery = lineage::recover_lineage_nonterminal_turns(
                &mut self.conn,
                &self.lineage,
                &self.branch,
                unix_timestamp_millis()?,
            )?;
        }
        Ok(self)
    }

    pub fn lineage_id(&self) -> &str {
        self.lineage.as_str()
    }

    pub fn session_id(&self) -> &str {
        self.branch.as_str()
    }

    pub fn sessions_root(&self) -> &Path {
        &self.sessions_root
    }

    pub fn commit_session(
        &mut self,
        command: &SessionCommit,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        let _catalog_pending =
            crate::catalog::mark_catalog_session_pending(&self.sessions_root, self.branch.as_str())
                .map_err(crate::session_command::commit_failure_from_store_error)?;
        let mut transaction =
            crate::write_transaction::begin_write(&mut self.conn, "commit session")
                .map_err(crate::session_command::commit_failure_from_store_error)?;
        let receipt = lineage::apply_lineage_session_commit(
            &mut transaction,
            &self.lineage,
            &self.branch,
            command,
            ObjectCompression::default(),
        )?;
        transaction.commit().map_err(|error| {
            crate::session_command::commit_failure_from_store_error(error.into())
        })?;
        Ok(receipt)
    }

    /// Reads a matching legacy receipt without writing or promising a retained archive result.
    pub fn recover_session_commit(
        &self,
        command: &SessionCommit,
    ) -> std::result::Result<Option<SaveReceipt>, SessionCommitFailure> {
        lineage::recover_lineage_session_commit(&self.conn, &self.lineage, &self.branch, command)
    }

    /// Reads the exact retained native result without publishing or advancing the branch.
    pub fn recover_compact_session(
        &self,
        command: &crate::CompactSessionCommit,
    ) -> std::result::Result<Option<crate::SessionCommitResult>, SessionCommitFailure> {
        lineage::recover_compact_session(&self.conn, &self.lineage, &self.branch, command)
    }

    /// Saves atomically and retains the exact result for replay after rewind and reclamation.
    /// A legacy receipt whose result has already been reclaimed cannot be upgraded.
    pub fn commit_session_with_result(
        &mut self,
        command: &SessionCommit,
    ) -> std::result::Result<crate::SessionCommitResult, SessionCommitFailure> {
        let _catalog_pending =
            crate::catalog::mark_catalog_session_pending(&self.sessions_root, self.branch.as_str())
                .map_err(crate::session_command::commit_failure_from_store_error)?;
        let mut transaction =
            crate::write_transaction::begin_write(&mut self.conn, "commit session with result")
                .map_err(crate::session_command::commit_failure_from_store_error)?;
        let compression = ObjectCompression::default();
        let (fingerprint, receipt) = lineage::apply_lineage_session_commit_with_fingerprint(
            &mut transaction,
            &self.lineage,
            &self.branch,
            command,
            compression,
        )?;
        let result = lineage::retain_session_receipt_result(
            &transaction,
            &self.lineage,
            &self.branch,
            &fingerprint,
            receipt,
            compression,
        )
        .map_err(crate::session_command::commit_failure_from_store_error)?;
        transaction.commit().map_err(|error| {
            crate::session_command::commit_failure_from_store_error(error.into())
        })?;
        Ok(result)
    }

    /// Applies exact-base archive edits and atomically retains the exact result for replay.
    pub fn commit_compact_session(
        &mut self,
        command: &crate::CompactSessionCommit,
    ) -> std::result::Result<crate::SessionCommitResult, SessionCommitFailure> {
        let _catalog_pending =
            crate::catalog::mark_catalog_session_pending(&self.sessions_root, self.branch.as_str())
                .map_err(crate::session_command::commit_failure_from_store_error)?;
        let mut transaction =
            crate::write_transaction::begin_write(&mut self.conn, "commit compact session")
                .map_err(crate::session_command::commit_failure_from_store_error)?;
        let result = lineage::apply_compact_session_commit(
            &mut transaction,
            &self.lineage,
            &self.branch,
            command,
            ObjectCompression::default(),
        )?;
        transaction.commit().map_err(|error| {
            crate::session_command::commit_failure_from_store_error(error.into())
        })?;
        Ok(result)
    }

    pub fn submit_turn(
        &mut self,
        command: &crate::session_commit::SubmitTurn,
    ) -> std::result::Result<crate::session_commit::SubmitTurnReceipt, SessionCommitFailure> {
        let _catalog_pending =
            crate::catalog::mark_catalog_session_pending(&self.sessions_root, self.branch.as_str())
                .map_err(crate::session_command::commit_failure_from_store_error)?;
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
            record_submit_turn_commit(
                command.session.history.items.len(),
                command
                    .session
                    .transcript_records
                    .as_ref()
                    .map_or(0, |suffix| suffix.records.len()),
            );
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
        let _catalog_pending =
            crate::catalog::mark_catalog_session_pending(&self.sessions_root, self.branch.as_str())
                .map_err(crate::session_command::commit_failure_from_store_error)?;
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

    /// Submits a turn and atomically retains its exact compact session result.
    pub fn submit_compact_turn(
        &mut self,
        command: &crate::CompactSubmitTurn,
    ) -> std::result::Result<crate::CompactSubmitTurnResult, SessionCommitFailure> {
        let _catalog_pending =
            crate::catalog::mark_catalog_session_pending(&self.sessions_root, self.branch.as_str())
                .map_err(crate::session_command::commit_failure_from_store_error)?;
        let transaction_duration =
            smelt_perf::perf::begin_value_ms("persist:submit_turn:transaction_ms");
        smelt_perf::perf::record_value("persist:submit_turn:transactions", 1);
        let result = lineage::apply_compact_submit_turn(
            &mut self.conn,
            &self.lineage,
            &self.branch,
            command,
            ObjectCompression::default(),
        );
        drop(transaction_duration);
        if result.is_ok() {
            record_submit_turn_commit(
                command.session.history.items.len(),
                command
                    .session
                    .transcript_records
                    .as_ref()
                    .map_or(0, |suffix| suffix.records.len()),
            );
        }
        result
    }

    pub fn recover_compact_submit_turn(
        &self,
        command: &crate::CompactSubmitTurn,
    ) -> std::result::Result<Option<crate::CompactSubmitTurnResult>, SessionCommitFailure> {
        lineage::recover_compact_submit_turn(&self.conn, &self.lineage, &self.branch, command)
    }

    /// Transitions a turn and atomically retains its exact compact session result.
    pub fn transition_compact_turn(
        &mut self,
        command: &crate::CompactTurnTransition,
    ) -> std::result::Result<crate::CompactTurnTransitionResult, SessionCommitFailure> {
        let _catalog_pending =
            crate::catalog::mark_catalog_session_pending(&self.sessions_root, self.branch.as_str())
                .map_err(crate::session_command::commit_failure_from_store_error)?;
        lineage::apply_compact_turn_transition(
            &mut self.conn,
            &self.lineage,
            &self.branch,
            command,
            ObjectCompression::default(),
        )
    }

    pub fn recover_compact_turn_transition(
        &self,
        command: &crate::CompactTurnTransition,
    ) -> std::result::Result<Option<crate::CompactTurnTransitionResult>, SessionCommitFailure> {
        lineage::recover_compact_turn_transition(&self.conn, &self.lineage, &self.branch, command)
    }

    pub fn store_head(&self) -> Result<StoreHead> {
        if branch_exists(&self.conn, &self.lineage, &self.branch)? {
            Ok(lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?.head)
        } else {
            Ok(StoreHead::default())
        }
    }

    /// Projects current catalog scalars while sharing the message represented by an
    /// unchanged, verified publication. The result must still name the current revision.
    pub fn catalog_session_for_result(
        &self,
        result: &crate::SessionCommitResult,
        first_user_message: Option<Arc<str>>,
    ) -> Result<CatalogSession> {
        load_catalog_session(
            &self.conn,
            &self.lineage,
            &self.branch,
            &mut lineage::OperationStats::default(),
            CatalogMessageSource::Published {
                result,
                first_user_message,
            },
        )
    }

    pub fn last_session_commit(&self) -> Result<Option<(String, SaveReceipt)>> {
        lineage::lineage_last_session_receipt(&self.conn, &self.lineage, &self.branch)
    }

    pub fn take_startup_recovery(
        &mut self,
    ) -> Option<crate::session_commit::StartupRecoveryResult> {
        self.startup_recovery.take()
    }

    pub fn startup_recovery(&self) -> Option<&crate::session_commit::StartupRecoveryResult> {
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

    pub fn refresh_catalog(&self) -> Result<()> {
        self.refresh_catalog_branch(&self.branch)
    }

    fn refresh_catalog_branch(&self, branch: &BranchId) -> Result<()> {
        let session = load_catalog_session(
            &self.conn,
            &self.lineage,
            branch,
            &mut lineage::OperationStats::default(),
            CatalogMessageSource::Stored,
        )?;
        self.upsert_catalog_session(&session)?;
        Ok(())
    }

    fn upsert_catalog_session(&self, session: &CatalogSession) -> Result<bool> {
        self.with_catalog(|catalog| catalog.upsert_available(session))
    }

    fn with_catalog<T>(&self, f: impl FnOnce(&mut Catalog) -> Result<T>) -> Result<T> {
        let mut catalog = self.catalog.borrow_mut();
        if catalog.is_none() {
            *catalog = Some(Catalog::open(
                crate::SessionStoreLayout::from_sessions_root(&self.sessions_root).catalog_path(),
            )?);
        }
        let catalog = catalog.as_mut().expect("catalog initialized");
        f(catalog)
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

    /// Creates and owns a destination without taking ownership of its source.
    /// Captures the exact source head and copied immutable revision in one transaction.
    /// An expected source head fences unsaved suffixes against concurrent edits.
    pub fn fork_from(
        root: impl AsRef<Path>,
        source_session_id: impl Into<String>,
        target_session_id: impl Into<String>,
        created_at: u64,
        expected_source: Option<StoreHead>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(Self, crate::SessionForkResult)> {
        let root = root.as_ref();
        validate_storage_root(root)?;
        let source = BranchId::new(source_session_id.into())?;
        let target = BranchId::new(target_session_id.into())?;
        let target_lease = LineageLease::acquire_branch(root, &target)?;
        if let Some(lineage) = locate_lineage(root, &target)? {
            return Err(StoreError::Integrity(format!(
                "session {} already exists in lineage {}",
                target.as_str(),
                lineage.as_str()
            )));
        }
        let lineage = locate_lineage(root, &source)?.ok_or_else(|| {
            StoreError::Integrity(format!(
                "session {} has no canonical lineage",
                source.as_str()
            ))
        })?;
        let lease = LineageLease::acquire_shared(root, &lineage)?;
        let path = lineage_database_path(root, &lineage);
        reject_symlink(&path)?;
        if !path.is_file() {
            return Err(StoreError::Integrity(
                "fork source database no longer exists".into(),
            ));
        }
        let mut conn = open_write_connection(&path, &lineage)?;
        crate::schema::validate_lineage_schema(&conn)?;
        let _catalog_pending = crate::catalog::mark_catalog_session_pending(root, target.as_str())?;
        let mut tx = crate::write_transaction::begin_write_until(
            &mut conn,
            "fork session",
            std::time::Instant::now() + crate::write_transaction::WRITE_DEADLINE,
            cancelled,
        )?;
        let snapshot = lineage::lineage_session_head(&tx, &lineage, &source)?;
        if let Some(expected) = expected_source {
            if snapshot.head != expected {
                return Err(StoreError::Integrity(format!(
                    "source head changed from {expected:?} to {:?}",
                    snapshot.head
                )));
            }
        }
        lineage::fork_branch(
            &mut tx,
            &lineage,
            &source,
            &target,
            Some(&snapshot.revision_id),
            created_at,
        )?;
        let receipt = SaveReceipt {
            session_id: target.as_str().to_owned(),
            previous: StoreHead::default(),
            current: StoreHead {
                revision: crate::session_commit::Revision::new(1),
                history_len: snapshot.head.history_len,
                transcript_record_count: snapshot.head.transcript_record_count,
            },
            lineage_id: Some(lineage.as_str().to_owned()),
            history_text_bytes: snapshot.history_root.byte_count(),
        };
        let result = crate::SessionForkResult {
            source_session_id: source.as_str().to_owned(),
            source_head: snapshot.head,
            session: crate::SessionCommitResult {
                receipt,
                revision_id: snapshot.revision_id.as_str().to_owned(),
            },
        };
        tx.commit()?;
        let writer = Self::from_acquired(root, lineage, target, conn, lease, target_lease)
            .finish_startup()?;
        Ok((writer, result))
    }

    pub fn fork_current(
        &self,
        target_session_id: impl Into<String>,
        created_at: u64,
    ) -> Result<SaveReceipt> {
        let (destination, result) = Self::fork_from(
            &self.sessions_root,
            self.session_id(),
            target_session_id,
            created_at,
            None,
            &|| false,
        )?;
        destination.release()?;
        Ok(result.session.receipt)
    }

    pub fn rewind_to_sequence(&mut self, sequence: u64, updated_at: u64) -> Result<SaveReceipt> {
        let _catalog_pending = crate::catalog::mark_catalog_session_pending(
            &self.sessions_root,
            self.branch.as_str(),
        )?;
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
            lineage_id: Some(self.lineage.as_str().to_owned()),
            history_text_bytes: current.history_root.byte_count(),
        })
    }

    pub fn delete_branch(self, deleted_at: u64) -> Result<()> {
        let _catalog_pending = crate::catalog::mark_catalog_session_pending(
            &self.sessions_root,
            self.branch.as_str(),
        )?;
        lineage::delete_branch(&self.conn, &self.lineage, &self.branch, deleted_at)?;
        // A failed upgrade may drop the shared lock. Close immediately and leave
        // physical reclamation to cleanup rather than racing another writer.
        if !self.lineage_lease.try_exclusive()? {
            return self.release();
        }
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
        let trash = crate::SessionStoreLayout::from_sessions_root(&self.sessions_root).trash_dir();
        ensure_private_directory(&trash)?;
        let token = LineageId::random()?;
        let tombstone = trash.join(format!("{}.{}", self.lineage.as_str(), token.as_str()));
        self.conn
            .close()
            .map_err(|(_, error)| StoreError::from(error))?;
        sync_directory(&source)?;
        rename_without_replacement(&source, &tombstone)?;
        sync_directory(&trash)?;
        sync_directory(&self.sessions_root)?;

        if fs::remove_dir_all(&tombstone).is_ok() {
            let _ = sync_directory(&trash);
            let _ = fs::remove_dir(&trash);
            let _ = sync_directory(&self.sessions_root);
        }
        Ok(())
    }

    pub fn delete_branch_by_id(
        &mut self,
        session_id: impl Into<String>,
        deleted_at: u64,
    ) -> Result<()> {
        let branch = BranchId::new(session_id)?;
        let _branch_lease = (branch != self.branch)
            .then(|| LineageLease::acquire_branch(&self.sessions_root, &branch))
            .transpose()?;
        let _catalog_pending =
            crate::catalog::mark_catalog_session_pending(&self.sessions_root, branch.as_str())?;
        lineage::delete_branch(&self.conn, &self.lineage, &branch, deleted_at)
    }

    pub fn database_path(&self) -> PathBuf {
        lineage_database_path(&self.sessions_root, &self.lineage)
    }

    pub fn search_database_path(&self) -> PathBuf {
        crate::SessionStoreLayout::from_sessions_root(&self.sessions_root)
            .lineage_search_path(self.lineage.as_str())
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

    /// Advance canonical reclamation within the supplied row budget. Derived
    /// search-cache pruning and physical file compaction are separate cold operations.
    pub fn reclaim_step(&mut self, max_rows: usize) -> Result<LineageReclamation> {
        let step = lineage::reclaim_step(&mut self.conn, &self.lineage, max_rows)?;
        debug_assert!(step.work_rows() <= max_rows);
        Ok(LineageReclamation {
            branch_heads_cleared: step.branch_heads_cleared,
            canonical_rows_deleted: step.canonical_rows_deleted,
            objects_deleted: step.objects_deleted,
            rows_examined: step.rows_examined,
            phase_advanced: step.phase_advanced,
            complete: step.complete,
        })
    }

    /// Cold pruning of obsolete derived search segments. This scans live search
    /// sources once and removes each obsolete segment transactionally. Run before
    /// canonical reclamation while its source text is still available. Never call
    /// it from key handling, rendering or automatic canonical idle maintenance.
    pub fn prune_search_projection(&self) -> Result<usize> {
        crate::lineage_search::prune_search_projection(
            &self.conn,
            &self.search_database_path(),
            &self.lineage,
        )
    }

    /// Losslessly share one aggregate-cost cohort, scanning at most 256 large
    /// object headers and processing at most 128 MiB, enough for two maximum-sized
    /// logical objects. Keep the cursor until the pass reports complete.
    /// A rejected cohort rolls back all allocations and still advances the pass.
    /// This is cold maintenance, never part of key handling or rendering.
    pub fn share_objects(
        &mut self,
        cursor: &mut crate::ObjectSharingCursor,
    ) -> Result<crate::ObjectSharingStep> {
        lineage::share_objects(&mut self.conn, &self.lineage, cursor)
    }

    /// Cold compaction of the pages free at entry. Each incremental statement is
    /// drained and limited to 256 pages. Concurrent readers may defer WAL
    /// truncation; the result reports that explicitly rather than waiting on them.
    pub fn vacuum(&mut self) -> Result<LineageVacuum> {
        let free_pages_before = self
            .conn
            .pragma_query_value(None, "freelist_count", |row| row.get::<_, i64>(0))?;
        let free_pages_before = nonnegative_u64(free_pages_before, "free pages before vacuum")?;
        self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)")?;
        let auto_vacuum: i64 = self
            .conn
            .pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
        if auto_vacuum == 2 {
            // Bound the entire cold pass to its original free pages, even if
            // another branch writer frees additional pages between statements.
            let mut remaining = free_pages_before;
            while remaining > 0 {
                let mut statement = self.conn.prepare(&format!(
                    "PRAGMA incremental_vacuum({})",
                    remaining.min(256),
                ))?;
                let mut rows = statement.query([])?;
                let mut reclaimed = 0u64;
                while rows.next()?.is_some() {
                    reclaimed += 1;
                }
                if reclaimed == 0 {
                    break;
                }
                remaining = remaining.saturating_sub(reclaimed);
            }
        } else if free_pages_before > 0 {
            // Older databases without incremental auto-vacuum still support
            // explicit cold compaction without changing their logical schema.
            self.conn.execute_batch("VACUUM")?;
        }
        self.conn.execute_batch("PRAGMA optimize")?;
        let checkpoint_busy: i64 =
            self.conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        let free_pages_after = self
            .conn
            .pragma_query_value(None, "freelist_count", |row| row.get::<_, i64>(0))?;
        let free_pages_after = nonnegative_u64(free_pages_after, "free pages after vacuum")?;
        Ok(LineageVacuum {
            free_pages_before,
            free_pages_after,
            pages_reclaimed: free_pages_before.saturating_sub(free_pages_after),
            wal_truncated: checkpoint_busy == 0,
        })
    }

    pub fn spawn_search_projector(&self) -> Result<crate::LineageSearchProjector> {
        crate::LineageSearchProjector::spawn(
            self.database_path(),
            self.search_database_path(),
            self.lineage.clone(),
            self.branch.clone(),
        )
    }

    pub fn append_request_attempt(
        &mut self,
        entry: &protocol::request_log::RequestLogEntry,
        payload_mode: crate::request_audit::RequestAuditPayloadMode,
    ) -> Result<i64> {
        let transaction =
            crate::write_transaction::begin_write(&mut self.conn, "append request audit")?;
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

pub fn cleanup_abandoned_lineages(root: impl AsRef<Path>, limit: usize) -> Result<usize> {
    let root = root.as_ref();
    validate_storage_root(root)?;
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(StoreError::Integrity(format!(
                "lineage root is not a private directory: {}",
                root.display()
            )))
        }
        Err(error) => return Err(StoreError::Io(error)),
    }

    let trash = crate::SessionStoreLayout::from_sessions_root(root).trash_dir();
    let mut inspected = 0usize;
    let mut removed = 0usize;
    match fs::symlink_metadata(&trash) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            for entry in fs::read_dir(&trash)? {
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
                if inspected >= limit {
                    break;
                }
                inspected = inspected.saturating_add(1);
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

    for entry in fs::read_dir(root)? {
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
        if inspected >= limit {
            break;
        }
        inspected = inspected.saturating_add(1);
        let _lease = match LineageLease::acquire(root, &lineage) {
            Ok(lease) => lease,
            Err(StoreError::OwnershipConflict { .. }) => continue,
            Err(error) => return Err(error),
        };
        let source = entry.path();
        let path = crate::SessionStoreLayout::from_sessions_root(root)
            .lineage_database_path(lineage.as_str());
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
        sync_directory(root)?;
        fs::remove_dir_all(&tombstone)?;
        sync_directory(&trash)?;
        removed = removed.saturating_add(1);
    }
    let _ = fs::remove_dir(&trash);
    sync_directory(root)?;
    Ok(removed)
}

pub fn lineage_session_locations(root: impl AsRef<Path>) -> Result<Vec<LineageSessionLocation>> {
    let root = root.as_ref();
    validate_storage_root(root)?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    ensure_private_directory(root)?;
    let mut sessions = Vec::new();
    for entry in fs::read_dir(root)? {
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
        let path = crate::SessionStoreLayout::from_sessions_root(root)
            .lineage_database_path(lineage.as_str());
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
        let rows = statement.query_map([lineage.as_str()], |row| {
            Ok(LineageSessionLocation {
                session_id: row.get(0)?,
                lineage_id: lineage.as_str().to_owned(),
            })
        })?;
        sessions.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
    }
    sessions.sort_unstable_by(|left, right| left.session_id.cmp(&right.session_id));
    if sessions
        .windows(2)
        .any(|pair| pair[0].session_id == pair[1].session_id)
    {
        return Err(StoreError::Integrity(
            "a live session belongs to multiple lineages".into(),
        ));
    }
    Ok(sessions)
}

pub fn lineage_session_ids(root: impl AsRef<Path>) -> Result<Vec<String>> {
    lineage_session_locations(root).map(|sessions| {
        sessions
            .into_iter()
            .map(|session| session.session_id)
            .collect()
    })
}

#[derive(Debug)]
pub struct LineageSessionReader {
    sessions_root: PathBuf,
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
        Self::try_open_existing_in_lineage(root, lineage.as_str(), branch.as_str())
    }

    pub fn open_existing_in_lineage(
        root: impl AsRef<Path>,
        lineage_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Self> {
        let session_id = session_id.into();
        Self::try_open_existing_in_lineage(root, lineage_id, session_id.clone())?.ok_or_else(|| {
            StoreError::Integrity(format!(
                "session {session_id} has no branch in catalog lineage"
            ))
        })
    }

    pub fn try_open_existing_in_lineage(
        root: impl AsRef<Path>,
        lineage_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Option<Self>> {
        let _perf = smelt_perf::perf::begin("store:lineage:open_read_only_located");
        let root = root.as_ref();
        validate_storage_root(root)?;
        let lineage = LineageId::from_hex(lineage_id.into())?;
        let branch = BranchId::new(session_id.into())?;
        let path = lineage_database_path(root, &lineage);
        reject_symlink(&path)?;
        if !path.is_file() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        if !branch_exists(&conn, &lineage, &branch)? {
            return Ok(None);
        }
        Ok(Some(Self {
            sessions_root: root.to_path_buf(),
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

    pub fn search_database_path(&self) -> PathBuf {
        crate::SessionStoreLayout::from_sessions_root(&self.sessions_root)
            .lineage_search_path(self.lineage.as_str())
    }

    /// Projects catalog metadata without materializing retained archive values.
    pub fn catalog_session(&self) -> Result<CatalogSession> {
        load_catalog_session(
            &self.conn,
            &self.lineage,
            &self.branch,
            &mut lineage::OperationStats::default(),
            CatalogMessageSource::Stored,
        )
    }

    /// Reuses a prior verified catalog message only when its immutable source matches.
    pub fn catalog_session_with_cache(&self, cached: &CatalogSession) -> Result<CatalogSession> {
        load_catalog_session(
            &self.conn,
            &self.lineage,
            &self.branch,
            &mut lineage::OperationStats::default(),
            CatalogMessageSource::Cached(cached),
        )
    }

    pub fn snapshot(&self) -> Result<LineageSessionState> {
        public_snapshot(
            &self.lineage,
            lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?,
        )
    }

    pub fn store_head(&self) -> Result<StoreHead> {
        Ok(lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?.head)
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

    /// Latest context update or tombstone strictly before `end` on this head.
    pub fn history_last_context_note_index_before(
        &self,
        end: u64,
        name: &str,
    ) -> Result<Option<u64>> {
        Ok(self
            .history_semantic_query(lineage::HistorySemantic::Context(name), 0..end, true)?
            .map(|(index, _)| index))
    }

    pub fn history_mode_before(&self, end: u64) -> Result<Option<String>> {
        Ok(self
            .history_semantic_query(lineage::HistorySemantic::Mode, 0..end, true)?
            .map(|(_, mode)| mode))
    }

    pub fn history_base_mode_range(&self, range: std::ops::Range<u64>) -> Result<Option<String>> {
        Ok(self
            .history_semantic_query(lineage::HistorySemantic::BaseMode, range, false)?
            .map(|(_, mode)| mode))
    }

    pub fn history_any_transcript_visible_before(&self, end: u64) -> Result<bool> {
        Ok(self
            .history_semantic_query(lineage::HistorySemantic::Visible, 0..end, false)?
            .is_some())
    }

    fn history_semantic_query(
        &self,
        semantic: lineage::HistorySemantic<'_>,
        range: std::ops::Range<u64>,
        last: bool,
    ) -> Result<Option<(u64, String)>> {
        let head = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        Ok(lineage::history_semantic_range(
            &self.conn,
            &self.lineage,
            &head.history_root,
            semantic,
            range,
            last,
        )?
        .0)
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

    pub fn transcript_extent_profile(
        &self,
        range: crate::TranscriptRecordRange,
    ) -> Result<crate::TranscriptExtentProfile> {
        let snapshot = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_extent_profile(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            range,
        )
    }

    pub fn transcript_estimated_rows(
        &self,
        range: crate::TranscriptRecordRange,
        width: u16,
    ) -> Result<u64> {
        let _perf = smelt_perf::perf::begin("store:extent:reader_estimated_rows");
        let snapshot = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_estimated_rows(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            range,
            width,
        )
    }

    pub fn transcript_total_estimated_rows(&self, width: u16) -> Result<u64> {
        let snapshot = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_total_estimated_rows(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            width,
        )
    }

    pub fn transcript_record_for_row(
        &self,
        width: u16,
        row: u64,
    ) -> Result<Option<crate::TranscriptRowLocation>> {
        let snapshot = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_row_location(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            width,
            row,
        )
    }

    pub fn transcript_record_before_kind(
        &self,
        kind: &str,
        before_or_at: usize,
    ) -> Result<Option<crate::TranscriptNavigationRecord>> {
        let snapshot = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_record_before_kind(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            kind,
            before_or_at,
        )
    }

    pub fn transcript_record_after_kind(
        &self,
        kind: &str,
        after_or_at: usize,
    ) -> Result<Option<crate::TranscriptNavigationRecord>> {
        let snapshot = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_record_after_kind(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            kind,
            after_or_at,
        )
    }

    pub fn transcript_record_before_role(
        &self,
        role: &str,
        before_or_at: usize,
    ) -> Result<Option<crate::TranscriptNavigationRecord>> {
        let snapshot = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_record_before_role(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            role,
            before_or_at,
        )
    }

    pub fn transcript_record_after_role(
        &self,
        role: &str,
        after_or_at: usize,
    ) -> Result<Option<crate::TranscriptNavigationRecord>> {
        let snapshot = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_record_after_role(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            role,
            after_or_at,
        )
    }

    pub fn transcript_record_index_for_block_idx(&self, block_idx: u64) -> Result<Option<usize>> {
        let snapshot = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_record_index_for_block_idx(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            block_idx,
        )
    }

    pub fn transcript_record_index_for_history_idx(
        &self,
        history_idx: u64,
    ) -> Result<Option<usize>> {
        let snapshot = lineage::lineage_session_head(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_record_index_for_history_idx(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            history_idx,
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
            if crate::history::estimated_transcript_record_rows(&slice.records, width)
                >= target_rows
                || count == total_count
            {
                smelt_perf::perf::record_value("transcript:resume_tail:tail_probes", probes);
                return Ok(slice);
            }
            count = count.saturating_mul(2).min(total_count);
        }
    }

    pub fn spawn_search_projector(&self) -> Result<crate::LineageSearchProjector> {
        crate::LineageSearchProjector::spawn(
            self.path.clone(),
            self.search_database_path(),
            self.lineage.clone(),
            self.branch.clone(),
        )
    }

    pub fn search_transcript_candidate_page(
        &self,
        query: &str,
        origin_block_idx: Option<u64>,
        direction: crate::TranscriptSearchDirection,
        limit: usize,
    ) -> Result<Vec<crate::TranscriptSearchCandidate>> {
        self.search_transcript_candidate_page_with_cancellation(
            query,
            origin_block_idx,
            direction,
            limit,
            || false,
        )
    }

    pub fn search_transcript_candidate_page_with_cancellation(
        &self,
        query: &str,
        origin_block_idx: Option<u64>,
        direction: crate::TranscriptSearchDirection,
        limit: usize,
        cancelled: impl Fn() -> bool,
    ) -> Result<Vec<crate::TranscriptSearchCandidate>> {
        crate::lineage_search::search_transcript_candidate_page(
            &self.conn,
            &self.search_database_path(),
            &self.lineage,
            &self.branch,
            query,
            origin_block_idx,
            direction,
            limit,
            &cancelled,
        )
    }

    pub fn search_projection_status(&self) -> Result<crate::SearchProjectionStatus> {
        crate::lineage_search::search_projection_status(
            &self.conn,
            &self.search_database_path(),
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
        crate::diagnostics::backup_connection_to(&self.conn, destination.as_ref())
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

enum CatalogMessageSource<'a> {
    Stored,
    Cached(&'a CatalogSession),
    Published {
        result: &'a crate::SessionCommitResult,
        first_user_message: Option<Arc<str>>,
    },
}

fn load_catalog_session(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
    stats: &mut lineage::OperationStats,
    message: CatalogMessageSource<'_>,
) -> Result<CatalogSession> {
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()?;
    let record = lineage::load_branch_record(conn, lineage, branch, false)?;
    let state = lineage::load_revision_envelope(conn, lineage, &record.revision, stats)?;
    let first_user_message_id = state.catalog_message_id().map(str::to_owned);
    let first_user_message = match message {
        CatalogMessageSource::Published {
            result,
            first_user_message,
        } => {
            if result.receipt.session_id != branch.as_str()
                || result.receipt.lineage_id.as_deref() != Some(lineage.as_str())
                || result.receipt.current != record.head
                || result.revision_id != record.revision.id().as_str()
                || !matches!(state, lineage::StoredRevisionState::Shared(_))
                || first_user_message.is_some() != first_user_message_id.is_some()
            {
                return Err(StoreError::Integrity(
                    "catalog publication has no matching current revision".into(),
                ));
            }
            first_user_message
        }
        CatalogMessageSource::Cached(cached)
            if cached.id == branch.as_str()
                && cached.lineage_id.as_deref() == Some(lineage.as_str())
                && first_user_message_id.is_some()
                && cached.first_user_message.is_some()
                && cached.first_user_message_id == first_user_message_id =>
        {
            cached.first_user_message.clone()
        }
        _ => state.catalog_message(conn, lineage, stats)?,
    };
    let metadata = state.metadata();
    crate::SessionCostUsd::new(record.metadata.session_cost_usd)?;
    Ok(CatalogSession {
        id: record.identity.id,
        lineage_id: Some(lineage.as_str().to_owned()),
        title: metadata.title.clone(),
        slug: metadata.slug.clone(),
        first_user_message,
        first_user_message_id,
        cwd: record.metadata.cwd,
        mode: record.metadata.mode,
        reasoning_effort: record.metadata.reasoning_effort,
        model: record.metadata.model,
        fast_mode: record.metadata.fast_mode,
        parent_id: record.identity.parent_id,
        context_tokens: metadata.display_context_tokens.or(metadata.context_tokens),
        history_len: Some(record.head.history_len.get()),
        text_bytes: Some(record.revision.history_root().byte_count()),
        created_at: record.identity.created_at,
        updated_at: metadata.updated_at,
        source_revision: record.head.revision.get(),
        availability: CatalogAvailability::Available,
        error_kind: None,
        error_summary: None,
        last_seen_scan: 0,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Catalog, CatalogAvailability, CatalogSession, HistoryIndex, HistoryLen, HistorySuffix,
        NewTurn, RequestAuditPayloadMode, RequestAuditQuery, SessionCostUsd, SideTableSuffixes,
        SubmitTurn, TranscriptRecordCount, TurnKind, TurnState, TurnTransition,
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
            checkpoint_events_json: None,
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

    fn retaining_title_commit(
        initial: &SessionCommit,
        result: &crate::SessionCommitResult,
        title: &str,
    ) -> crate::CompactSessionCommit {
        crate::CompactSessionCommit {
            session_id: initial.session_id.clone(),
            expected: result.receipt.current,
            identity: initial.identity.clone(),
            scalars: crate::SessionScalars {
                title: Some(title.into()),
                slug: None,
                cwd: initial.metadata.cwd.clone(),
                mode: initial.metadata.mode.clone(),
                reasoning_effort: None,
                model: initial.metadata.model.clone(),
                fast_mode: initial.metadata.fast_mode,
                accounting: crate::ValueEdit::Retain,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: initial.metadata.session_cost_usd,
                updated_at: 2,
            },
            archive_base: Some(crate::SessionArchiveBase {
                lineage_id: result.receipt.lineage_id.clone().unwrap(),
                revision_id: result.revision_id.clone(),
                branch_sequence: result.receipt.current.revision,
            }),
            archives: crate::CompactSessionArchives::default(),
            history: HistorySuffix {
                start: HistoryIndex::new(result.receipt.current.history_len.get()),
                final_len: result.receipt.current.history_len,
                items: Vec::new(),
            },
            transcript_records: None,
        }
    }

    #[test]
    fn native_batch_recovery_is_read_only_and_does_not_hydrate_retained_archives() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        use std::sync::atomic::{AtomicU64, Ordering};
        for (events, message_bytes) in [(0, 128), (32, 32_768), (128, 1_048_576)] {
            let root = tempfile::tempdir().unwrap();
            let id = session_id('1');
            let mut writer = crate::SessionWriter::open(root.path(), &id).unwrap();
            let mut initial = initial_commit(&id);
            initial.metadata.first_user_message = Some("m".repeat(message_bytes));
            initial.metadata.checkpoint_events_json = Some(serde_json::Value::Array(
                (0..events)
                    .map(|index| {
                        serde_json::json!({
                            "kind": "auto", "summary": "archive ".repeat(4096),
                            "first_live_index": 0, "completed_at_history_len": 1,
                            "created_at_ms": index,
                        })
                    })
                    .collect(),
            ));
            let first = writer
                .lineage_writer_mut()
                .commit_session_with_result(&initial)
                .unwrap();
            let command = retaining_title_commit(&initial, &first, "recover-title");
            let batch = crate::SessionEventBatch::compact_save(
                2,
                command,
                crate::SessionBatchBarrier::None,
            );
            assert!(writer.recover_batch(&batch).unwrap().is_none());
            let saved = writer.commit_batch(&batch).unwrap();
            let mut later =
                retaining_title_commit(&initial, saved.exact_session().unwrap(), "later-title");
            later.scalars.updated_at = 3;
            writer
                .commit_batch(&crate::SessionEventBatch::compact_save(
                    3,
                    later,
                    crate::SessionBatchBarrier::None,
                ))
                .unwrap();
            let head = writer.store_head().unwrap();
            assert!(head.revision > saved.session().current.revision);
            let marker = crate::catalog_session_pending_token(root.path(), &id).unwrap();
            let changes = writer.lineage_writer_mut().conn.total_changes();
            writer
                .lineage_writer_mut()
                .conn
                .pragma_update(None, "query_only", true)
                .unwrap();
            writer
                .lineage_writer_mut()
                .conn
                .authorizer(Some(|context: AuthContext<'_>| match context.action {
                    AuthAction::Read {
                        table_name: "lineage_sequence_entries",
                        ..
                    } => Authorization::Deny,
                    _ => Authorization::Allow,
                }))
                .unwrap();
            let steps = Arc::new(AtomicU64::new(0));
            let counter = steps.clone();
            writer
                .lineage_writer_mut()
                .conn
                .progress_handler(
                    1,
                    Some(move || {
                        counter.fetch_add(1, Ordering::Relaxed);
                        false
                    }),
                )
                .unwrap();
            assert_eq!(writer.recover_batch(&batch).unwrap(), Some(saved));
            let measured = steps.load(Ordering::Relaxed);
            writer
                .lineage_writer_mut()
                .conn
                .progress_handler(0, None::<fn() -> bool>)
                .unwrap();
            eprintln!("NATIVE_BATCH_RECOVERY events={events} message_bytes={message_bytes} vm_steps={measured}");
            assert!(
                measured < 8192,
                "retained archives inflated recovery to {measured} VM steps"
            );
            assert!(
                writer.lineage_writer_mut().snapshot().is_err(),
                "cold hydration must fail under the same entry-read guard"
            );
            writer
                .lineage_writer_mut()
                .conn
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            assert_eq!(writer.store_head().unwrap(), head);
            assert_eq!(writer.lineage_writer_mut().conn.total_changes(), changes);
            assert_eq!(
                crate::catalog_session_pending_token(root.path(), &id).unwrap(),
                marker
            );
        }
    }

    #[test]
    fn public_reclamation_step_work_is_bounded_on_retained_history() {
        use std::sync::atomic::{AtomicU64, Ordering};
        for (history_len, searchable) in [(16, false), (4096, false), (16, true), (4096, true)] {
            let root = tempfile::tempdir().unwrap();
            let id = session_id('1');
            let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
            let mut command = initial_commit(&id);
            command.history.final_len = HistoryLen::new(history_len);
            command.history.items = (0..history_len)
                .map(|index| {
                    let text = format!("retained history {index}");
                    if searchable {
                        protocol::HistoryItem::user(protocol::Content::text(text))
                    } else {
                        protocol::HistoryItem::system(text)
                    }
                })
                .collect();
            let receipt = writer.commit_session(&command).unwrap();
            let snapshot = writer.snapshot().unwrap();
            let vm_steps = Arc::new(AtomicU64::new(0));
            let counter = vm_steps.clone();
            writer
                .conn
                .progress_handler(
                    1,
                    Some(move || {
                        counter.fetch_add(1, Ordering::Relaxed);
                        false
                    }),
                )
                .unwrap();
            let mut peak = 0;
            let mut calls = 0;
            let (complete, measured) = loop {
                vm_steps.store(0, Ordering::Relaxed);
                let step = writer.reclaim_step(1).unwrap();
                let measured = vm_steps.load(Ordering::Relaxed);
                peak = peak.max(measured);
                calls += 1;
                assert!(step.work_rows() <= 1);
                assert!(step.complete || step.made_progress());
                assert!(calls < history_len * 64 + 1024, "public GC did not settle");
                if step.complete || measured >= 16_384 {
                    break (step.complete, measured);
                }
            };
            writer
                .conn
                .progress_handler(0, None::<fn() -> bool>)
                .unwrap();
            assert_eq!(writer.snapshot().unwrap(), snapshot);
            assert_eq!(writer.commit_session(&command).unwrap(), receipt);
            println!("public GC all-phase one-row steps: history_len={history_len} searchable={searchable} calls={calls} peak_vm_steps={peak} complete={complete}");
            assert!(measured < 16_384,
                "one-row public GC traversed retained history: history_len={history_len} calls={calls} vm_steps={measured}");
            assert!(complete);
        }
    }

    #[test]
    fn public_reclamation_step_work_is_bounded_with_ready_search_projection() {
        use std::sync::atomic::{AtomicU64, Ordering};
        for record_count in [16, 4096] {
            let root = tempfile::tempdir().unwrap();
            let id = session_id('1');
            let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
            let mut command = initial_commit(&id);
            command.transcript_records = Some(crate::TranscriptRecordSuffix {
                start: crate::TranscriptRecordIndex::ZERO,
                records: (0..record_count)
                    .map(|index| {
                        transcript_record(index, format!("retained projected transcript {index}"))
                    })
                    .collect(),
            });
            let receipt = writer.commit_session(&command).unwrap();
            let snapshot = writer.snapshot().unwrap();
            let reader = LineageSessionReader::open_existing(root.path(), &id).unwrap();
            let projector = writer.spawn_search_projector().unwrap();
            projector.request();
            let status = wait_for_search_projection(&reader);
            assert_eq!(status.ready_segments, status.total_segments);
            assert!(status.ready_segments > 0);
            drop(projector);
            let expected_candidates = reader
                .search_transcript_candidate_page(
                    "retained projected transcript",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    3,
                )
                .unwrap();
            assert_eq!(expected_candidates.len(), 3);

            let vm_steps = Arc::new(AtomicU64::new(0));
            let counter = vm_steps.clone();
            writer
                .conn
                .progress_handler(
                    1,
                    Some(move || {
                        counter.fetch_add(1, Ordering::Relaxed);
                        false
                    }),
                )
                .unwrap();
            let mut calls = 0;
            let mut peak = 0;
            let (complete, measured) = loop {
                vm_steps.store(0, Ordering::Relaxed);
                let step = writer.reclaim_step(1).unwrap();
                let measured = vm_steps.load(Ordering::Relaxed);
                peak = peak.max(measured);
                calls += 1;
                assert!(step.work_rows() <= 1);
                assert!(step.complete || step.made_progress());
                assert!(
                    calls < record_count * 64 + 1024,
                    "projected public GC did not settle"
                );
                if step.complete || measured >= 16_384 {
                    break (step.complete, measured);
                }
            };
            writer
                .conn
                .progress_handler(0, None::<fn() -> bool>)
                .unwrap();
            assert_eq!(writer.snapshot().unwrap(), snapshot);
            assert_eq!(writer.commit_session(&command).unwrap(), receipt);
            assert_eq!(
                reader.search_projection_status().unwrap().state,
                crate::SearchProjectionState::Current
            );
            assert_eq!(
                reader
                    .search_transcript_candidate_page(
                        "retained projected transcript",
                        None,
                        crate::TranscriptSearchDirection::Forward,
                        3,
                    )
                    .unwrap(),
                expected_candidates
            );
            println!("projected public GC: records={record_count} calls={calls} peak_vm_steps={peak} complete={complete}");
            assert!(measured < 16_384,
                "one-row public GC traversed ready search sources: records={record_count} calls={calls} vm_steps={measured}");
            assert!(complete);
        }
    }

    #[test]
    fn public_reclamation_marking_matches_retained_graph_with_bounded_steps() {
        use std::collections::BTreeSet;
        use std::sync::atomic::{AtomicU64, Ordering};
        for history_len in [16, 4096] {
            let root = tempfile::tempdir().unwrap();
            let id = session_id('1');
            let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
            let mut command = initial_commit(&id);
            command.history.final_len = HistoryLen::new(history_len);
            command.history.items = (0..history_len)
                .map(|index| protocol::HistoryItem::system(format!("retained history {index}")))
                .collect();
            let receipt = writer.commit_session(&command).unwrap();
            let snapshot = writer.snapshot().unwrap();
            let expected = lineage::inspect_reachability(&writer.conn, &writer.lineage).unwrap();
            let vm_steps = Arc::new(AtomicU64::new(0));
            let mut peak = 0;
            let mut complete = false;
            for _ in 0..history_len * 64 + 1024 {
                let counter = vm_steps.clone();
                vm_steps.store(0, Ordering::Relaxed);
                writer
                    .conn
                    .progress_handler(
                        1,
                        Some(move || {
                            counter.fetch_add(1, Ordering::Relaxed);
                            false
                        }),
                    )
                    .unwrap();
                let step = writer.reclaim_step(1).unwrap();
                writer
                    .conn
                    .progress_handler(0, None::<fn() -> bool>)
                    .unwrap();
                let measured = vm_steps.load(Ordering::Relaxed);
                peak = peak.max(measured);
                assert!(
                    measured < 16_384,
                    "marking traversed retained history: {measured}"
                );
                assert!(step.work_rows() <= 1);
                assert!(step.made_progress());
                assert_eq!(
                    step.canonical_rows_deleted + step.objects_deleted + step.branch_heads_cleared,
                    0
                );
                let phase: i64 = writer
                    .conn
                    .query_row(
                        "SELECT phase FROM smelt_gc_pass WHERE singleton = 1",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                if phase == 9 {
                    complete = true;
                    break;
                }
            }
            assert!(complete, "marking did not settle");
            for (view, column, expected) in [
                (
                    "smelt_reachable_revisions",
                    "revision_id",
                    expected.reachable_revisions,
                ),
                ("smelt_reachable_roots", "root_id", expected.reachable_roots),
                ("smelt_reachable_nodes", "node_id", expected.reachable_nodes),
                (
                    "smelt_reachable_payloads",
                    "payload_id",
                    expected.reachable_payloads,
                ),
            ] {
                let actual = writer
                    .conn
                    .prepare(&format!("SELECT {column} FROM {view}"))
                    .unwrap()
                    .query_map([], |row| row.get::<_, String>(0))
                    .unwrap()
                    .collect::<rusqlite::Result<BTreeSet<_>>>()
                    .unwrap();
                assert_eq!(actual, expected, "marking differs for {view}");
            }
            assert_eq!(writer.snapshot().unwrap(), snapshot);
            assert_eq!(writer.commit_session(&command).unwrap(), receipt);
            println!("public GC marking: history_len={history_len} peak_vm_steps={peak}");
        }
    }

    #[test]
    fn public_reclamation_reopens_and_resumes_with_changing_budgets() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('1');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let mut command = initial_commit(&id);
        command.history.final_len = HistoryLen::new(128);
        command.history.items = (0..128)
            .map(|index| {
                protocol::HistoryItem::user(protocol::Content::text(format!(
                    "retained history {index}"
                )))
            })
            .collect();
        let receipt = writer.commit_session(&command).unwrap();
        let snapshot = writer.snapshot().unwrap();
        for _ in 0..13 {
            let step = writer.reclaim_step(1).unwrap();
            assert!(step.made_progress());
            assert!(step.work_rows() <= 1);
            assert!(!step.complete);
        }
        drop(writer);
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        assert_eq!(
            writer
                .conn
                .query_row(
                    "SELECT count(*) FROM temp.sqlite_schema WHERE name = 'smelt_gc_pass'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        let mut complete = false;
        for (index, budget) in [1, 7, 3, 16].into_iter().cycle().take(128 * 64).enumerate() {
            let step = writer.reclaim_step(budget).unwrap();
            assert!(step.work_rows() <= budget);
            assert!(step.complete || step.made_progress());
            if step.complete {
                complete = true;
                break;
            }
            if index % 97 == 0 {
                assert_eq!(writer.snapshot().unwrap(), snapshot);
            }
        }
        assert!(complete);
        assert_eq!(writer.snapshot().unwrap(), snapshot);
        assert_eq!(writer.commit_session(&command).unwrap(), receipt);
    }

    #[test]
    fn public_reclamation_marking_restarts_after_own_foreign_rolled_back_and_schema_writes() {
        for phase in [2, 8, 9, -1] {
            for mutation in ["own", "foreign", "rollback", "vacuum"] {
                let root = tempfile::tempdir().unwrap();
                let id = session_id('1');
                let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
                let initial = initial_commit(&id);
                let result = writer.commit_session_with_result(&initial).unwrap();
                let mut settled = false;
                for _ in 0..1024 {
                    let step = writer.reclaim_step(1).unwrap();
                    let current: i64 = writer
                        .conn
                        .query_row(
                            "SELECT phase FROM smelt_gc_pass WHERE singleton = 1",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap();
                    if (phase == -1 && step.complete) || current == phase {
                        settled = true;
                        break;
                    }
                }
                assert!(settled, "phase not reached: {phase}");
                let epoch: i64 = writer
                    .conn
                    .query_row(
                        "SELECT epoch FROM smelt_gc_pass WHERE singleton = 1",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                match mutation {
                    "own" => {
                        writer
                            .commit_compact_session(&retaining_title_commit(
                                &initial, &result, "new head",
                            ))
                            .unwrap();
                    }
                    "foreign" => {
                        let other = Connection::open(writer.conn.path().unwrap()).unwrap();
                        other
                            .execute("UPDATE store_meta SET updated_at = updated_at + 1", [])
                            .unwrap();
                    }
                    "rollback" => {
                        let tx = writer.conn.transaction().unwrap();
                        tx.execute("UPDATE store_meta SET updated_at = updated_at + 1", [])
                            .unwrap();
                        tx.rollback().unwrap();
                    }
                    "vacuum" => {
                        writer.conn.execute_batch("VACUUM").unwrap();
                    }
                    _ => unreachable!(),
                }
                let snapshot = writer.snapshot().unwrap();
                let step = writer.reclaim_step(1).unwrap();
                assert!(step.made_progress());
                assert!(!step.complete);
                assert!(step.work_rows() <= 1);
                assert_eq!(
                    step.canonical_rows_deleted + step.objects_deleted + step.branch_heads_cleared,
                    0
                );
                let restarted: i64 = writer
                    .conn
                    .query_row(
                        "SELECT epoch FROM smelt_gc_pass WHERE singleton = 1",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    restarted,
                    epoch + 1,
                    "stale pass survived {mutation} at phase {phase}"
                );
                assert_eq!(writer.snapshot().unwrap(), snapshot);
                let mut complete = false;
                for _ in 0..1024 {
                    if writer.reclaim_step(7).unwrap().complete {
                        complete = true;
                        break;
                    }
                }
                assert!(complete);
                assert_eq!(writer.snapshot().unwrap(), snapshot);
                assert_eq!(writer.commit_session(&initial).unwrap(), result.receipt);
            }
        }
    }

    #[test]
    fn native_batch_recovery_rejects_missing_or_corrupt_exact_result_ownership() {
        for kind in ["save", "submit", "transition"] {
            for missing in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let id = session_id('1');
                let mut writer = crate::SessionWriter::open(root.path(), &id).unwrap();
                let initial = initial_commit(&id);
                let first = writer
                    .lineage_writer_mut()
                    .commit_session_with_result(&initial)
                    .unwrap();
                let mut session = retaining_title_commit(&initial, &first, "native-result");
                let batch = if kind == "save" {
                    crate::SessionEventBatch::compact_save(
                        2,
                        session,
                        crate::SessionBatchBarrier::None,
                    )
                } else {
                    let submitted_batch = crate::SessionEventBatch::compact_submit_turn(
                        2,
                        crate::CompactSubmitTurn {
                            session: session.clone(),
                            turn: crate::NewTurn {
                                kind: crate::TurnKind::Command,
                                submitted_history_idx: HistoryIndex::ZERO,
                                continuation_of: None,
                                created_at_ms: 2,
                            },
                        },
                    );
                    if kind == "submit" {
                        submitted_batch
                    } else {
                        let submitted = writer.commit_batch(&submitted_batch).unwrap();
                        let crate::SessionEventReceipt::CompactSubmitTurn(result) = submitted
                        else {
                            panic!("native submit result")
                        };
                        session =
                            retaining_title_commit(&initial, &result.session, "native-result");
                        crate::SessionEventBatch::compact_turn_transition(
                            3,
                            crate::CompactTurnTransition {
                                session,
                                turn_id: result.turn_id,
                                state: crate::TurnState::Running,
                                at_ms: 3,
                                terminal_reason: None,
                            },
                        )
                    }
                };
                let saved = writer.commit_batch(&batch).unwrap();
                assert_eq!(writer.recover_batch(&batch).unwrap(), Some(saved));
                let fingerprint = match &batch.command {
                    crate::SessionEventCommand::CompactSave { session } => {
                        crate::compact_session_commit_fingerprint(session).unwrap()
                    }
                    crate::SessionEventCommand::CompactSubmitTurn { command } => {
                        crate::compact_submit_turn_fingerprint(command).unwrap()
                    }
                    crate::SessionEventCommand::CompactTurnTransition { command } => {
                        crate::compact_turn_transition_fingerprint(command).unwrap()
                    }
                    _ => unreachable!(),
                };
                let head = writer.store_head().unwrap();
                let conn = &writer.lineage_writer_mut().conn;
                if missing {
                    conn.execute_batch("DROP TRIGGER lineage_session_receipt_result_delete")
                        .unwrap();
                    conn.execute(
                        "DELETE FROM lineage_session_receipt_results WHERE fingerprint = ?1",
                        [&fingerprint],
                    )
                    .unwrap();
                } else {
                    conn.execute_batch("DROP TRIGGER lineage_session_receipt_result_update")
                        .unwrap();
                    conn.execute("UPDATE lineage_session_receipt_results SET result_id = ?1 WHERE fingerprint = ?2", (&"0".repeat(64), &fingerprint)).unwrap();
                }
                conn.pragma_update(None, "query_only", true).unwrap();
                assert!(writer.last_session_commit().unwrap().is_some());
                assert!(matches!(writer.recover_batch(&batch), Err(SessionCommitFailure::Integrity { .. })), "{kind} recovery must not guess an exact result from an ordinary receipt or current head");
                assert_eq!(writer.store_head().unwrap(), head);
            }
        }
    }

    #[test]
    fn native_catalog_projection_reuses_verified_messages_with_bounded_reads() {
        for (checkpoints, message_bytes) in [(0, 128), (32, 32_768), (128, 1_048_576)] {
            let root = tempfile::tempdir().unwrap();
            let id = session_id('1');
            let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
            let mut initial = initial_commit(&id);
            let message: Arc<str> = "m".repeat(message_bytes).into();
            initial.metadata.first_user_message = Some(message.to_string());
            initial.metadata.checkpoint_events_json = Some(serde_json::Value::Array(
                (0..checkpoints)
                    .map(|index| {
                        serde_json::json!({
                            "kind": "auto",
                            "summary": format!("{index} {}", "archive ".repeat(4096)),
                            "first_live_index": 0,
                            "completed_at_history_len": 1,
                            "created_at_ms": index,
                        })
                    })
                    .collect(),
            ));
            let first = writer.commit_session_with_result(&initial).unwrap();
            let projected = writer
                .catalog_session_for_result(&first, Some(message.clone()))
                .unwrap();
            assert!(Arc::ptr_eq(
                projected.first_user_message.as_ref().unwrap(),
                &message
            ));
            let reader = LineageSessionReader::open_existing_in_lineage(
                root.path(),
                writer.lineage_id(),
                &id,
            )
            .unwrap();
            let mut command = retaining_title_commit(&initial, &first, "native-title");
            let title = writer.commit_compact_session(&command).unwrap();
            for source in [
                CatalogMessageSource::Published {
                    result: &title,
                    first_user_message: Some(message.clone()),
                },
                CatalogMessageSource::Cached(&projected),
            ] {
                let mut stats = lineage::OperationStats::default();
                let current = load_catalog_session(
                    &reader.conn,
                    &reader.lineage,
                    &reader.branch,
                    &mut stats,
                    source,
                )
                .unwrap();
                assert!(Arc::ptr_eq(
                    current.first_user_message.as_ref().unwrap(),
                    &message
                ));
                assert_eq!(current.title.as_deref(), Some("native-title"));
                assert_eq!(stats.payloads_read, 1);
                assert_eq!(stats.nodes_read, 0);
                assert_eq!(stats.payloads_written, 0);
                eprintln!("NATIVE_CATALOG_READS checkpoints={checkpoints} message_bytes={message_bytes} payloads={} nodes={}", stats.payloads_read, stats.nodes_read);
            }
            assert!(writer
                .catalog_session_for_result(&first, Some(message.clone()))
                .is_err());
            assert!(writer.catalog_session_for_result(&title, None).is_err());
            let mut foreign = title.clone();
            foreign.receipt.lineage_id = Some("a".repeat(32));
            assert!(writer
                .catalog_session_for_result(&foreign, Some(message.clone()))
                .is_err());

            command.expected = title.receipt.current;
            let runtime = writer.commit_compact_session(&command).unwrap();
            assert_eq!(runtime.receipt.current, title.receipt.current);
            assert_eq!(runtime.revision_id, title.revision_id);
            let mut branch_metadata =
                lineage::load_branch_record(&writer.conn, &writer.lineage, &writer.branch, false)
                    .unwrap()
                    .metadata;
            branch_metadata.mode = Some("plan".into());
            branch_metadata.model = Some("new-model".into());
            lineage::update_branch_metadata(
                &writer.conn,
                &writer.lineage,
                &writer.branch,
                &branch_metadata,
            )
            .unwrap();
            assert_eq!(writer.commit_compact_session(&command).unwrap(), runtime);
            let current = writer
                .catalog_session_for_result(&runtime, Some(message.clone()))
                .unwrap();
            assert_eq!(current.mode.as_deref(), Some("plan"));
            assert_eq!(current.model.as_deref(), Some("new-model"));

            let mut wrong_session = projected.clone();
            wrong_session.id = session_id('2');
            let mut wrong_lineage = projected.clone();
            wrong_lineage.lineage_id = Some("a".repeat(32));
            let mut missing_body = projected.clone();
            missing_body.first_user_message = None;
            for cached in [wrong_session, wrong_lineage, missing_body] {
                let mut stats = lineage::OperationStats::default();
                let current = load_catalog_session(
                    &reader.conn,
                    &reader.lineage,
                    &reader.branch,
                    &mut stats,
                    CatalogMessageSource::Cached(&cached),
                )
                .unwrap();
                assert_eq!(
                    current.first_user_message.as_deref(),
                    Some(message.as_ref())
                );
                assert!(!Arc::ptr_eq(
                    current.first_user_message.as_ref().unwrap(),
                    &message
                ));
                assert_eq!(stats.payloads_read, 2);
                assert_eq!(stats.nodes_read, 1);
            }
            command.archives.first_user_message = crate::ValueEdit::Replace {
                value: Some("replacement α".into()),
            };
            command.scalars.updated_at = 3;
            writer.commit_compact_session(&command).unwrap();
            let changed = reader.catalog_session_with_cache(&projected).unwrap();
            assert_eq!(changed.first_user_message.as_deref(), Some("replacement α"));
            assert_ne!(
                changed.first_user_message_id,
                projected.first_user_message_id
            );
        }
    }

    fn install_legacy_fixture(writer: &mut OwnedLineageWriter) {
        let source = crate::schema::tests::v3_connection();
        lineage::create_lineage(&source, &writer.lineage, 1).unwrap();
        rusqlite::backup::Backup::new(&source, &mut writer.conn)
            .unwrap()
            .run_to_completion(128, std::time::Duration::ZERO, None)
            .unwrap();
        crate::schema::validate_lineage_schema(&writer.conn).unwrap();
    }

    #[test]
    fn catalog_projection_preserves_metadata_without_hydrating_compact_archives() {
        for (version, checkpoints) in [
            (3, 0),
            (3, 32),
            (3, 128),
            (crate::schema::LINEAGE_SCHEMA_VERSION, 0),
            (crate::schema::LINEAGE_SCHEMA_VERSION, 32),
            (crate::schema::LINEAGE_SCHEMA_VERSION, 128),
        ] {
            let root = tempfile::tempdir().unwrap();
            let id = session_id('1');
            let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
            if version == 3 {
                install_legacy_fixture(&mut writer);
            }
            let mut initial = initial_commit(&id);
            initial.metadata.first_user_message = Some("catalog first α".into());
            initial.metadata.slug = Some("catalog-title".into());
            initial.metadata.context_tokens = Some(5);
            initial.metadata.display_context_tokens = Some(7);
            initial.metadata.checkpoint_events_json = Some(serde_json::Value::Array(
                (0..checkpoints).map(|index| serde_json::json!({
                    "kind": "auto", "summary": format!("{index} {}", "archive ".repeat(4096)),
                    "first_live_index": 0, "completed_at_history_len": 1,
                    "created_at_ms": index,
                })).collect(),
            ));
            initial.side_tables.metadata_snapshots.push((
                HistoryIndex::ZERO,
                serde_json::json!({
                    "retained": "metadata α".repeat(4096),
                }),
            ));
            let receipt = writer.commit_session(&initial).unwrap();
            let mut expected =
                CatalogSession::from_commit(&initial, &receipt, receipt.lineage_id.clone());
            let saved =
                lineage::load_branch_record(&writer.conn, &writer.lineage, &writer.branch, false)
                    .unwrap();
            let envelope = lineage::load_revision_envelope(
                &writer.conn,
                &writer.lineage,
                &saved.revision,
                &mut lineage::OperationStats::default(),
            )
            .unwrap();
            expected.first_user_message_id = envelope.catalog_message_id().map(str::to_owned);
            let reader = LineageSessionReader::open_existing_in_lineage(
                root.path(),
                writer.lineage_id(),
                &id,
            )
            .unwrap();
            let mut stats = lineage::OperationStats::default();
            let actual = load_catalog_session(
                &reader.conn,
                &reader.lineage,
                &reader.branch,
                &mut stats,
                CatalogMessageSource::Stored,
            )
            .unwrap();
            assert_eq!(actual, expected);
            assert_eq!(reader.catalog_session().unwrap(), expected);
            assert_eq!(
                stats.payloads_read,
                if version == crate::schema::LINEAGE_SCHEMA_VERSION {
                    2
                } else {
                    1
                }
            );
            assert_eq!(
                stats.nodes_read,
                if version == crate::schema::LINEAGE_SCHEMA_VERSION {
                    1
                } else {
                    0
                }
            );
            assert_eq!(stats.payloads_written, 0);
            let record =
                lineage::load_branch_record(&writer.conn, &writer.lineage, &writer.branch, false)
                    .unwrap();
            let (state_id, state_bytes) = writer.conn.query_row(
                "SELECT revision.state_payload_id, object.raw_size FROM lineage_revisions revision
                 JOIN lineage_payload_object_refs payload ON payload.lineage_id = revision.lineage_id
                   AND payload.payload_id = revision.state_payload_id
                 JOIN objects object ON object.hash = payload.object_hash
                 WHERE revision.lineage_id = ?1 AND revision.revision_id = ?2",
                (writer.lineage_id(), record.revision.id().as_str()),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            ).unwrap();
            if version == crate::schema::LINEAGE_SCHEMA_VERSION {
                let (hash, bytes) = writer.conn.query_row(
                    "SELECT object.hash, object.bytes FROM lineage_revision_state_roots archive
                     JOIN lineage_sequence_roots root ON root.lineage_id = archive.lineage_id
                       AND root.root_id = archive.root_id AND root.item_count = 1 AND root.depth = 1
                     JOIN lineage_sequence_entries entry ON entry.lineage_id = root.lineage_id
                       AND entry.node_id = root.root_node_id AND entry.entry_index = 0
                     JOIN lineage_payload_object_refs payload ON payload.lineage_id = entry.lineage_id
                       AND payload.payload_id = entry.payload_id
                     JOIN objects object ON object.hash = payload.object_hash
                     WHERE archive.lineage_id = ?1 AND archive.state_payload_id = ?2
                       AND archive.role = 'first_user_message'",
                    (writer.lineage_id(), state_id.as_str()),
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
                ).unwrap();
                writer
                    .conn
                    .execute(
                        "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
                        [&hash],
                    )
                    .unwrap();
                assert!(
                    reader.snapshot().is_err(),
                    "active message bytes must be hash-verified"
                );
                assert!(reader.catalog_session().is_err());
                assert!(writer.refresh_catalog().is_err());
                assert_eq!(reader.store_head().unwrap(), receipt.current);
                writer
                    .conn
                    .execute(
                        "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
                        (&bytes, &hash),
                    )
                    .unwrap();
                assert_eq!(
                    reader.snapshot().unwrap().metadata.first_user_message,
                    initial.metadata.first_user_message
                );
                assert_eq!(reader.catalog_session().unwrap(), expected);
            }
            if version == crate::schema::LINEAGE_SCHEMA_VERSION {
                assert!(state_bytes < 2048);
                assert_eq!(writer.conn.execute(
                    "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = (
                         SELECT payload.object_hash FROM lineage_revision_state_roots archive
                         JOIN lineage_sequence_roots root ON root.lineage_id = archive.lineage_id
                           AND root.root_id = archive.root_id AND root.item_count = 2 AND root.depth = 1
                         JOIN lineage_sequence_entries entry ON entry.lineage_id = root.lineage_id
                           AND entry.node_id = root.root_node_id AND entry.entry_index = ?3
                         JOIN lineage_payload_object_refs payload ON payload.lineage_id = entry.lineage_id
                           AND payload.payload_id = entry.payload_id
                         WHERE archive.lineage_id = ?1 AND archive.state_payload_id = ?2
                           AND archive.role = 'metadata_snapshots'
                     )",
                    (writer.lineage_id(), state_id.as_str(), 0),
                ).unwrap(), 1);
                assert!(reader.snapshot().is_err());
                assert_eq!(reader.catalog_session().unwrap(), expected);
                writer.refresh_catalog().unwrap();
                let catalog = crate::CatalogReader::open_existing(
                    crate::SessionStoreLayout::from_sessions_root(root.path()).catalog_path(),
                )
                .unwrap()
                .unwrap();
                assert_eq!(catalog.session(&id).unwrap().unwrap(), expected);
            }
            writer
                .conn
                .execute(
                    "UPDATE objects SET bytes = zeroblob(stored_size)
                 WHERE hash = (SELECT object_hash FROM lineage_payload_object_refs
                               WHERE lineage_id = ?1 AND payload_id = ?2)",
                    (writer.lineage_id(), state_id.as_str()),
                )
                .unwrap();
            assert!(
                reader.catalog_session().is_err(),
                "catalog envelopes must be hash-verified"
            );
            assert!(writer.refresh_catalog().is_err());
        }
    }

    #[test]
    fn vacuum_recovers_free_pages_and_reports_snapshot_blocked_wal_truncation() {
        for incremental in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let id = session_id('1');
            let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
            writer.commit_session(&initial_commit(&id)).unwrap();
            let snapshot = writer.snapshot().unwrap();
            if !incremental {
                writer
                    .conn
                    .execute_batch("PRAGMA auto_vacuum = NONE; VACUUM")
                    .unwrap();
            }
            for index in 0..8 {
                crate::object::put_object(
                    &writer.conn,
                    &vec![index; 512 * 1024],
                    ObjectCompression::none(),
                )
                .unwrap();
            }
            writer
                .conn
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            let before_bytes = fs::metadata(writer.database_path()).unwrap().len();
            let reader = Connection::open(writer.database_path()).unwrap();
            reader.execute_batch("BEGIN").unwrap();
            let pinned_bytes: i64 = reader
                .query_row("SELECT SUM(stored_size) FROM objects", [], |row| row.get(0))
                .unwrap();
            writer
                .conn
                .execute("DELETE FROM objects WHERE raw_size >= 131072", [])
                .unwrap();
            let vacuum = writer.vacuum().unwrap();
            assert!(vacuum.free_pages_before > 256);
            assert_eq!(vacuum.free_pages_after, 0);
            assert_eq!(vacuum.pages_reclaimed, vacuum.free_pages_before);
            assert!(!vacuum.wal_truncated);
            assert_eq!(
                reader
                    .query_row("SELECT SUM(stored_size) FROM objects", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                pinned_bytes
            );
            assert_eq!(writer.snapshot().unwrap(), snapshot);
            reader.execute_batch("COMMIT").unwrap();
            let settled = writer.vacuum().unwrap();
            assert!(settled.wal_truncated);
            assert_eq!(settled.free_pages_after, 0);
            let stats =
                lineage_storage_stats(&writer.conn, &writer.database_path(), Some(&writer.branch))
                    .unwrap();
            assert!(stats.database_bytes < before_bytes / 2);
            assert_eq!(stats.wal_bytes, 0);
            assert_eq!(writer.snapshot().unwrap(), snapshot);
        }
    }

    #[test]
    fn explicit_object_sharing_preserves_session_receipts_forks_and_backups() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('1');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        install_legacy_fixture(&mut writer);
        let mut initial = initial_commit(&id);
        let mut state = 0x123456789abcdef0_u64;
        let summary = (0..512 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                char::from(b'!' + (state % 90) as u8)
            })
            .collect::<String>();
        initial.metadata.checkpoint_events_json = Some(serde_json::json!([{
            "kind": "auto", "summary": summary, "first_live_index": 0,
            "completed_at_history_len": 0, "created_at_ms": 1,
        }]));
        let first = writer.commit_session(&initial).unwrap();
        let mut current = first.current;
        for index in 0..3 {
            let mut command = initial.clone();
            command.expected = current;
            command.metadata.updated_at = index + 2;
            command.metadata.title = Some(format!("title-{index}"));
            command.history.start = HistoryIndex::new(1);
            command.history.items.clear();
            current = writer.commit_session(&command).unwrap().current;
        }
        crate::schema::initialize_lineage_schema(&mut writer.conn).unwrap();
        let before = writer.snapshot().unwrap();
        let before_stats =
            lineage_storage_stats(&writer.conn, &writer.database_path(), Some(&writer.branch))
                .unwrap();
        let hashes = writer
            .conn
            .prepare("SELECT hash FROM objects WHERE raw_size >= 131072 ORDER BY hash")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(hashes.len(), 4);
        let mut cursor = crate::ObjectSharingCursor::default();
        let step = writer.share_objects(&mut cursor).unwrap();
        assert!(step.complete);
        assert_eq!(step.objects_scanned, hashes.len());
        assert_eq!(step.objects_shared, hashes.len());
        assert!(step.pages_saved > 0);
        assert_eq!(writer.store_head().unwrap(), current);
        assert_eq!(
            writer.share_objects(&mut cursor).unwrap().objects_scanned,
            0
        );
        let repeat = writer
            .share_objects(&mut crate::ObjectSharingCursor::default())
            .unwrap();
        assert!(repeat.complete);
        assert_eq!(repeat.objects_scanned, hashes.len());
        assert_eq!(repeat.objects_shared, 0);
        let after_stats =
            lineage_storage_stats(&writer.conn, &writer.database_path(), Some(&writer.branch))
                .unwrap();
        assert!(
            after_stats.object_stored_bytes < before_stats.object_stored_bytes / 2,
            "shared compressed archives must reclaim repeated physical payload bytes"
        );
        let after = writer.snapshot().unwrap();
        assert_eq!(after.metadata, before.metadata);
        assert_eq!(after.history_root_id, before.history_root_id);
        assert_eq!(after.transcript_root_id, before.transcript_root_id);
        assert_eq!(after.revision_id, before.revision_id);
        assert_eq!(writer.commit_session(&initial).unwrap(), first);
        assert_eq!(writer.store_head().unwrap(), current);
        for hash in hashes {
            let object = crate::object::object(&writer.conn, &hash).unwrap().unwrap();
            assert_eq!(crate::object::sha256_hex(&object.bytes), hash);
        }
        let fork_id = session_id('2');
        writer.fork_current(&fork_id, 10).unwrap();
        let reader = LineageSessionReader::open_existing(root.path(), &fork_id).unwrap();
        assert_eq!(
            reader.snapshot().unwrap().metadata.checkpoint_events_json,
            before.metadata.checkpoint_events_json
        );
        assert_eq!(reader.history_range(0, 1).unwrap(), initial.history.items);
        assert!(reader.doctor_report().unwrap().healthy);
        let backup = root.path().join("isolated-sharing-backup.db");
        reader.backup_to(&backup).unwrap();
        assert!(
            verify_lineage_backup(&backup, writer.lineage_id())
                .unwrap()
                .healthy
        );
    }

    #[test]
    fn hot_reads_do_not_hydrate_revision_state() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('1');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let mut command = initial_commit(&id);
        command.metadata.checkpoint_events_json = Some(serde_json::json!([{
            "kind": "auto",
            "summary": "archived summary ".repeat(65_536),
            "first_live_index": 0,
            "completed_at_history_len": 0,
            "created_at_ms": 1,
        }]));
        let record = transcript_record(0, "visible transcript".into());
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: vec![record.clone()],
        });
        let receipt = writer.commit_session(&command).unwrap();
        writer
            .conn
            .execute(
                "UPDATE objects SET codec = 'none', raw_size = 1, stored_size = 1, bytes = x'00'
             WHERE hash = (
                 SELECT payload.object_hash FROM lineage_branches branch
                 JOIN lineage_revisions revision
                   ON revision.lineage_id = branch.lineage_id
                  AND revision.revision_id = branch.head_revision_id
                 JOIN lineage_payload_object_refs payload
                   ON payload.lineage_id = revision.lineage_id
                  AND payload.payload_id = revision.state_payload_id
                 WHERE branch.session_id = ?1
             )",
                [&id],
            )
            .unwrap();
        assert!(
            writer.snapshot().is_err(),
            "cold state read must detect corruption"
        );
        assert_eq!(writer.store_head().unwrap(), receipt.current);
        let reader =
            LineageSessionReader::open_existing_in_lineage(root.path(), writer.lineage_id(), &id)
                .unwrap();
        assert_eq!(reader.store_head().unwrap(), receipt.current);
        assert_eq!(
            reader
                .history_last_context_note_index_before(u64::MAX, "missing")
                .unwrap(),
            None
        );
        assert_eq!(reader.history_mode_before(u64::MAX).unwrap(), None);
        assert_eq!(reader.history_base_mode_range(0..u64::MAX).unwrap(), None);
        assert!(
            !reader
                .history_any_transcript_visible_before(u64::MAX)
                .unwrap(),
            "transcript records must not substitute for history visibility"
        );
        assert_eq!(reader.history_range(0, 1).unwrap(), command.history.items);
        assert_eq!(
            reader.history_tail(1, 1, None).unwrap(),
            command.history.items
        );
        assert_eq!(reader.transcript_range(0, 1).unwrap(), vec![record.clone()]);
        assert_eq!(
            reader.transcript_object_backed_range(0, 1).unwrap(),
            vec![record.clone()]
        );
        assert_eq!(
            reader.transcript_extent_profile((0..1).into()).unwrap(),
            crate::history::transcript_extent_profile(&[record]),
        );
        assert!(reader.transcript_total_estimated_rows(80).unwrap() > 0);
        assert_eq!(
            reader.transcript_record_index_for_block_idx(0).unwrap(),
            Some(0)
        );
        assert_eq!(
            reader.transcript_record_index_for_history_idx(0).unwrap(),
            Some(0)
        );
        assert!(reader
            .transcript_record_before_kind("assistant", 1)
            .unwrap()
            .is_some());
        assert!(reader
            .transcript_record_after_kind("assistant", 0)
            .unwrap()
            .is_some());
    }

    #[test]
    fn indexed_semantics_do_not_hydrate_history_or_revision_objects() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('1');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let mut command = initial_commit(&id);
        command.history.items = vec![
            protocol::HistoryItem::note(protocol::HistoryNote::named_context("shared", "value")),
            protocol::HistoryItem::note(protocol::HistoryNote::named_context("shared", "")),
            protocol::HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                "normal", "plan", "mode",
            )),
            protocol::HistoryItem::user(protocol::Content::text("visible")),
        ];
        command.history.final_len = HistoryLen::new(4);
        writer.commit_session(&command).unwrap();
        writer
            .conn
            .execute(
                "UPDATE objects SET codec = 'none', raw_size = 0, stored_size = 0, bytes = x''",
                [],
            )
            .unwrap();
        let reader =
            LineageSessionReader::open_existing_in_lineage(root.path(), writer.lineage_id(), &id)
                .unwrap();
        assert!(reader.snapshot().is_err());
        assert!(reader.history_range(0, 4).is_err());
        assert_eq!(
            reader
                .history_last_context_note_index_before(0, "shared")
                .unwrap(),
            None
        );
        assert_eq!(
            reader
                .history_last_context_note_index_before(1, "shared")
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            reader
                .history_last_context_note_index_before(u64::MAX, "shared")
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            reader
                .history_last_context_note_index_before(u64::MAX, "missing")
                .unwrap(),
            None
        );
        assert_eq!(reader.history_mode_before(2).unwrap(), None);
        assert_eq!(reader.history_mode_before(3).unwrap(), Some("plan".into()));
        assert_eq!(reader.history_base_mode_range(0..2).unwrap(), None);
        assert_eq!(
            reader.history_base_mode_range(2..3).unwrap(),
            Some("normal".into())
        );
        assert!(!reader.history_any_transcript_visible_before(2).unwrap());
        assert!(reader.history_any_transcript_visible_before(3).unwrap());
        assert_eq!(
            reader.store_head().unwrap().transcript_record_count.get(),
            0
        );
    }

    fn install_reconciled_catalog_hint(sessions_root: &Path, id: &str, lineage_id: &str) {
        let mut catalog = Catalog::open(
            crate::SessionStoreLayout::from_sessions_root(sessions_root).catalog_path(),
        )
        .unwrap();
        let scan_id = catalog.allocate_scan().unwrap();
        catalog
            .upsert_available_for_reconciliation(
                &CatalogSession {
                    id: id.into(),
                    lineage_id: Some(lineage_id.into()),
                    title: Some("catalog hint".into()),
                    slug: None,
                    first_user_message: None,
                    first_user_message_id: None,
                    cwd: Some("/workspace".into()),
                    mode: Some("agent".into()),
                    reasoning_effort: None,
                    model: Some("test-model".into()),
                    fast_mode: Some(false),
                    parent_id: None,
                    context_tokens: None,
                    history_len: Some(1),
                    text_bytes: Some(5),
                    created_at: 1,
                    updated_at: 1,
                    source_revision: 1,
                    availability: CatalogAvailability::Available,
                    error_kind: None,
                    error_summary: None,
                    last_seen_scan: 0,
                },
                scan_id,
            )
            .unwrap();
        catalog.complete_scan(scan_id, 1).unwrap();
        drop(catalog);

        if let Some(token) = crate::catalog_session_pending_token(sessions_root, id).unwrap() {
            assert!(crate::clear_catalog_session_pending(sessions_root, id, &token).unwrap());
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
            history_idx: Some(0),
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
            tool_render_revision: 0,
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
    fn writer_open_waits_for_initialization_contention_without_changing_write_retries() {
        use std::sync::mpsc;
        use std::time::Duration;

        let root = tempfile::tempdir().unwrap();
        let id = session_id('0');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        writer.commit_session(&initial_commit(&id)).unwrap();
        let lineage_id = writer.lineage_id().to_owned();
        let database = writer.database_path();
        writer.release().unwrap();

        // Hold the database read lock needed by the opening connection's pragmas.
        let blocker = Connection::open(database).unwrap();
        blocker
            .execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN IMMEDIATE")
            .unwrap();
        let (opened, result) = mpsc::channel();
        std::thread::scope(|scope| {
            let root = root.path();
            scope.spawn(move || {
                opened
                    .send(OwnedLineageWriter::open_existing_in_lineage(
                        root, lineage_id, id,
                    ))
                    .unwrap();
            });

            let early = result.recv_timeout(Duration::from_millis(100));
            drop(blocker);
            assert!(
                matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
                "writer initialization must wait for the database lock: {early:?}"
            );
            let writer = result
                .recv_timeout(Duration::from_secs(10))
                .expect("writer initialization finishes once the lock is released")
                .expect("open writer after initialization contention");
            let busy_timeout: i64 = writer
                .conn
                .pragma_query_value(None, "busy_timeout", |row| row.get(0))
                .unwrap();
            assert_eq!(
                busy_timeout, 0,
                "write retries must retain their own deadline"
            );
            assert!(writer.conn.is_autocommit());
            assert_eq!(
                writer.store_head().unwrap().revision,
                crate::Revision::new(1)
            );
            writer.release().unwrap();
        });
    }

    #[test]
    fn canonical_lineage_layout_is_flat_and_ignores_the_nested_layout() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('0');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        writer.commit_session(&initial_commit(&id)).unwrap();
        let lineage_id = writer.lineage_id().to_owned();
        let database = writer.database_path();
        let lineage_dir = database.parent().unwrap().to_path_buf();
        writer.release().unwrap();

        let layout = crate::SessionStoreLayout::from_sessions_root(root.path());
        assert_eq!(database, layout.lineage_database_path(&lineage_id));
        assert_eq!(lineage_dir.parent().unwrap(), root.path());
        assert!(!root.path().join("lineages").exists());

        let nested = root.path().join("lineages");
        fs::create_dir(&nested).unwrap();
        fs::rename(&lineage_dir, nested.join(lineage_dir.file_name().unwrap())).unwrap();

        assert!(LineageSessionReader::try_open_existing(root.path(), &id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn lineage_session_locations_retain_direct_addresses() {
        let root = tempfile::tempdir().unwrap();
        let first_id = session_id('1');
        let second_id = session_id('2');
        let mut first = OwnedLineageWriter::open(root.path(), &first_id).unwrap();
        first.commit_session(&initial_commit(&first_id)).unwrap();
        let first_lineage = first.lineage_id().to_owned();
        first.release().unwrap();
        let mut second = OwnedLineageWriter::open(root.path(), &second_id).unwrap();
        second.commit_session(&initial_commit(&second_id)).unwrap();
        let second_lineage = second.lineage_id().to_owned();
        second.release().unwrap();

        assert_eq!(
            lineage_session_locations(root.path()).unwrap(),
            vec![
                LineageSessionLocation {
                    session_id: first_id,
                    lineage_id: first_lineage,
                },
                LineageSessionLocation {
                    session_id: second_id,
                    lineage_id: second_lineage,
                },
            ]
        );
    }

    #[test]
    fn reconciled_catalog_hint_avoids_scanning_unrelated_lineages() {
        let state = tempfile::tempdir().unwrap();
        let sessions_root = state.path().join("sessions");
        let id = session_id('1');
        let mut writer = OwnedLineageWriter::open(&sessions_root, &id).unwrap();
        writer.commit_session(&initial_commit(&id)).unwrap();
        let lineage_id = writer.lineage_id().to_owned();
        writer.release().unwrap();
        install_reconciled_catalog_hint(&sessions_root, &id, &lineage_id);

        let decoy = LineageId::random().unwrap();
        let layout = crate::SessionStoreLayout::from_sessions_root(&sessions_root);
        fs::create_dir_all(layout.lineage_dir(decoy.as_str())).unwrap();
        fs::write(
            layout.lineage_database_path(decoy.as_str()),
            b"not a sqlite database",
        )
        .unwrap();

        let reader = LineageSessionReader::open_existing(&sessions_root, &id).unwrap();
        assert_eq!(reader.lineage_id(), lineage_id);
    }

    #[test]
    fn stale_catalog_hint_falls_back_to_canonical_lineage_scan() {
        let state = tempfile::tempdir().unwrap();
        let sessions_root = state.path().join("sessions");
        let id = session_id('2');
        let other_id = session_id('3');

        let mut target = OwnedLineageWriter::open(&sessions_root, &id).unwrap();
        target.commit_session(&initial_commit(&id)).unwrap();
        let target_lineage = target.lineage_id().to_owned();
        target.release().unwrap();

        let mut other = OwnedLineageWriter::open(&sessions_root, &other_id).unwrap();
        other.commit_session(&initial_commit(&other_id)).unwrap();
        let stale_lineage = other.lineage_id().to_owned();
        other.release().unwrap();

        install_reconciled_catalog_hint(&sessions_root, &id, &stale_lineage);

        let reader = LineageSessionReader::open_existing(&sessions_root, &id).unwrap();
        assert_eq!(reader.lineage_id(), target_lineage);
    }

    #[test]
    fn fork_rejects_session_id_already_owned_by_another_lineage() {
        let state = tempfile::tempdir().unwrap();
        let sessions_root = state.path().join("sessions");
        let id = session_id('4');
        let other_id = session_id('5');

        let mut target = OwnedLineageWriter::open(&sessions_root, &id).unwrap();
        target.commit_session(&initial_commit(&id)).unwrap();
        let target_lineage = target.lineage_id().to_owned();
        target.release().unwrap();

        let mut other = OwnedLineageWriter::open(&sessions_root, &other_id).unwrap();
        other.commit_session(&initial_commit(&other_id)).unwrap();
        install_reconciled_catalog_hint(&sessions_root, &id, &target_lineage);
        let error = other.fork_current(&id, 2).unwrap_err();
        assert!(
            matches!(&error, StoreError::Integrity(message) if message.contains("already exists in lineage")),
            "unexpected duplicate-lineage error: {error}"
        );
    }

    #[test]
    fn branch_writers_are_independent_and_keep_exclusive_session_ownership() {
        let root = tempfile::tempdir().unwrap();
        let source_id = session_id('a');
        let target_id = session_id('b');
        let mut source = OwnedLineageWriter::open(root.path(), &source_id).unwrap();
        source.commit_session(&initial_commit(&source_id)).unwrap();
        source.fork_current(&target_id, 2).unwrap();
        let lineage_id = source.lineage_id().to_owned();
        let target =
            OwnedLineageWriter::open_existing_in_lineage(root.path(), &lineage_id, &target_id)
                .unwrap();
        assert_eq!(source.database_path(), target.database_path());

        for id in [&source_id, &target_id] {
            assert!(matches!(
                OwnedLineageWriter::open(root.path(), id),
                Err(StoreError::OwnershipConflict { .. })
            ));
            assert!(matches!(
                OwnedLineageWriter::open_existing(root.path(), id),
                Err(StoreError::OwnershipConflict { .. })
            ));
            assert!(matches!(
                OwnedLineageWriter::open_existing_in_lineage(root.path(), &lineage_id, id),
                Err(StoreError::OwnershipConflict { .. })
            ));
        }
        assert_eq!(source.session_id(), source_id);
        assert!(matches!(
            source.delete_branch_by_id(&target_id, 3),
            Err(StoreError::OwnershipConflict { .. })
        ));
        assert!(matches!(
            OwnedLineageWriter::open_existing(root.path(), &source_id),
            Err(StoreError::OwnershipConflict { .. })
        ));

        target.delete_branch(4).unwrap();
        assert_eq!(
            source.history_range(0, 1).unwrap(),
            vec![protocol::HistoryItem::system("first")]
        );
        let directory = source.database_path().parent().unwrap().to_owned();
        source.delete_branch(5).unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn fork_cannot_claim_an_unpublished_session_owned_by_another_writer() {
        let root = tempfile::tempdir().unwrap();
        let source_id = session_id('a');
        let target_id = session_id('b');
        let mut source = OwnedLineageWriter::open(root.path(), &source_id).unwrap();
        source.commit_session(&initial_commit(&source_id)).unwrap();
        let target = OwnedLineageWriter::open(root.path(), &target_id).unwrap();
        assert!(matches!(
            source.fork_current(&target_id, 2),
            Err(StoreError::OwnershipConflict { .. })
        ));
        target.release().unwrap();
        source.fork_current(&target_id, 2).unwrap();
    }

    #[test]
    fn public_fork_copies_verified_roots_without_hydrating_retained_bodies() {
        for events in [0, 32, 128] {
            let root = tempfile::tempdir().unwrap();
            let source_id = session_id('a');
            let target_id = session_id('b');
            let mut source = OwnedLineageWriter::open(root.path(), &source_id).unwrap();
            let mut command = initial_commit(&source_id);
            command.metadata.first_user_message = Some("synthetic retained message α".repeat(1024));
            command.metadata.checkpoint_events_json = Some(serde_json::Value::Array(
                (0..events)
                    .map(|index| {
                        serde_json::json!({
                            "kind": "auto", "summary": "synthetic archive ".repeat(2048),
                            "first_live_index": 0, "completed_at_history_len": 1,
                            "created_at_ms": index,
                        })
                    })
                    .collect(),
            ));
            let initial = source.commit_session(&command).unwrap();
            let immutable_revision = source.snapshot().unwrap().revision_id;
            command.expected = initial.current;
            let submitted = source
                .submit_turn(&SubmitTurn {
                    session: command,
                    turn: NewTurn {
                        kind: TurnKind::Command,
                        submitted_history_idx: HistoryIndex::ZERO,
                        continuation_of: None,
                        created_at_ms: 2,
                    },
                })
                .unwrap();
            assert_eq!(submitted.session.current, initial.current);
            source.release().unwrap();
            let mut source = OwnedLineageWriter::open_existing(root.path(), &source_id).unwrap();
            let recovery = source.take_startup_recovery().unwrap();
            assert_eq!(recovery.interrupted_turns, vec![submitted.turn_id]);
            let saved = recovery.session.receipt;
            let original = source.snapshot().unwrap();
            assert!(saved.current.revision > initial.current.revision);
            assert_eq!(original.revision_id, immutable_revision);
            let head = lineage::lineage_session_head(&source.conn, &source.lineage, &source.branch)
                .unwrap();
            assert_eq!(head.head, saved.current);
            assert_eq!(head.revision_id.as_str(), immutable_revision);
            let (hash, bytes) = source
                .conn
                .query_row(
                    "SELECT object.hash, object.bytes FROM lineage_revision_state_roots archive
                 JOIN lineage_revisions revision ON revision.lineage_id = archive.lineage_id
                   AND revision.state_payload_id = archive.state_payload_id
                 JOIN lineage_sequence_roots root ON root.lineage_id = archive.lineage_id
                   AND root.root_id = archive.root_id AND root.item_count = 1 AND root.depth = 1
                 JOIN lineage_sequence_entries entry ON entry.lineage_id = root.lineage_id
                   AND entry.node_id = root.root_node_id AND entry.entry_index = 0
                 JOIN lineage_payload_object_refs payload ON payload.lineage_id = entry.lineage_id
                   AND payload.payload_id = entry.payload_id
                 JOIN objects object ON object.hash = payload.object_hash
                 WHERE revision.lineage_id = ?1 AND revision.revision_id = ?2
                   AND archive.role = 'first_user_message'",
                    (source.lineage_id(), &original.revision_id),
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .unwrap();
            assert_eq!(
                source
                    .conn
                    .execute(
                        "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
                        [&hash],
                    )
                    .unwrap(),
                1
            );
            assert!(
                source.snapshot().is_err(),
                "cold source validation detects body corruption"
            );
            assert_eq!(source.store_head().unwrap(), saved.current);

            let (destination, result) = OwnedLineageWriter::fork_from(
                root.path(),
                &source_id,
                &target_id,
                2,
                Some(saved.current),
                &|| false,
            )
            .expect("root-copy fork must not hydrate unchanged message or checkpoint bodies");
            assert_eq!(result.source_session_id, source_id);
            assert_eq!(result.source_head, saved.current);
            assert_eq!(result.session.revision_id, immutable_revision);
            let receipt = result.session.receipt;
            assert_eq!(receipt.previous, StoreHead::default());
            assert_eq!(receipt.current.revision, crate::Revision::new(1));
            assert_eq!(receipt.current.history_len, saved.current.history_len);
            assert_eq!(
                receipt.current.transcript_record_count,
                saved.current.transcript_record_count
            );
            assert_eq!(receipt.history_text_bytes, saved.history_text_bytes);
            assert_eq!(source.store_head().unwrap(), saved.current);
            assert!(
                destination.snapshot().is_err(),
                "fork does not bypass cold validation"
            );
            source
                .conn
                .execute(
                    "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
                    (&bytes, &hash),
                )
                .unwrap();
            let copied = destination.snapshot().unwrap();
            assert_eq!(copied.revision_id, original.revision_id);
            assert_eq!(copied.history_root_id, original.history_root_id);
            assert_eq!(copied.transcript_root_id, original.transcript_root_id);
            assert_eq!(copied.side_tables, original.side_tables);
            assert_eq!(
                copied.metadata.first_user_message,
                original.metadata.first_user_message
            );
            assert_eq!(
                copied.metadata.checkpoint_events_json,
                original.metadata.checkpoint_events_json
            );
            assert_eq!(
                copied.identity.parent_id.as_deref(),
                Some(source_id.as_str())
            );
            assert_eq!(source.snapshot().unwrap(), original);
            destination.release().unwrap();
            source.release().unwrap();
        }
    }

    #[test]
    fn fork_checks_the_source_head_before_creating_the_destination() {
        let root = tempfile::tempdir().unwrap();
        let source_id = session_id('a');
        let target_id = session_id('b');
        let mut source = OwnedLineageWriter::open(root.path(), &source_id).unwrap();
        let initial = source.commit_session(&initial_commit(&source_id)).unwrap();
        let mut changed = initial_commit(&source_id);
        changed.expected = initial.current;
        changed.metadata.title = Some("parent advanced".into());
        let updated = source.commit_session(&changed).unwrap();
        assert!(matches!(
            OwnedLineageWriter::fork_from(root.path(), &source_id, &target_id, 2, Some(initial.current), &|| false),
            Err(StoreError::Integrity(message)) if message.contains("source head changed")
        ));
        assert!(
            LineageSessionReader::try_open_existing(root.path(), &target_id)
                .unwrap()
                .is_none()
        );
        let (destination, receipt) = OwnedLineageWriter::fork_from(
            root.path(),
            &source_id,
            &target_id,
            2,
            Some(updated.current),
            &|| false,
        )
        .unwrap();
        assert_eq!(destination.session_id(), target_id);
        assert_eq!(receipt.source_session_id, source_id);
        assert_eq!(receipt.source_head, updated.current);
        assert_eq!(
            destination.store_head().unwrap(),
            receipt.session.receipt.current
        );
        assert_eq!(source.store_head().unwrap(), updated.current);
        assert!(matches!(
            OwnedLineageWriter::open_existing(root.path(), &target_id),
            Err(StoreError::OwnershipConflict { .. })
        ));
    }

    #[test]
    fn cancelling_a_contended_fork_releases_destination_ownership() {
        let root = tempfile::tempdir().unwrap();
        let source_id = session_id('a');
        let target_id = session_id('b');
        let mut source = OwnedLineageWriter::open(root.path(), &source_id).unwrap();
        source.commit_session(&initial_commit(&source_id)).unwrap();
        let transaction = source.conn.transaction().unwrap();
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let (waiting, ready) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let task = scope.spawn(|| {
                OwnedLineageWriter::fork_from(root.path(), &source_id, &target_id, 2, None, &|| {
                    let _ = waiting.send(());
                    cancelled.load(std::sync::atomic::Ordering::Acquire)
                })
            });
            ready
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            assert!(matches!(task.join().unwrap(), Err(StoreError::Cancelled)));
        });
        transaction.commit().unwrap();
        assert!(
            LineageSessionReader::try_open_existing(root.path(), &target_id)
                .unwrap()
                .is_none()
        );
        OwnedLineageWriter::open(root.path(), &target_id)
            .unwrap()
            .release()
            .unwrap();
    }

    #[test]
    fn opening_a_clean_fork_does_not_wait_for_its_parents_write_transaction() {
        let root = tempfile::tempdir().unwrap();
        let source_id = session_id('a');
        let target_id = session_id('b');
        let mut source = OwnedLineageWriter::open(root.path(), &source_id).unwrap();
        source.commit_session(&initial_commit(&source_id)).unwrap();
        source.fork_current(&target_id, 2).unwrap();
        let transaction = source.conn.transaction().unwrap();
        let target = OwnedLineageWriter::open_existing(root.path(), &target_id).unwrap();
        assert!(target.startup_recovery().is_none());
        assert_eq!(
            target.history_range(0, 1).unwrap(),
            vec![protocol::HistoryItem::system("first")]
        );
        transaction.commit().unwrap();
    }

    #[test]
    fn concurrent_branch_commits_preserve_independent_histories() {
        let root = tempfile::tempdir().unwrap();
        let source_id = session_id('a');
        let target_id = session_id('b');
        let mut source = OwnedLineageWriter::open(root.path(), &source_id).unwrap();
        source.commit_session(&initial_commit(&source_id)).unwrap();
        source.fork_current(&target_id, 2).unwrap();
        let target = OwnedLineageWriter::open_existing(root.path(), &target_id).unwrap();
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for mut writer in [source, target] {
                let start = &start;
                scope.spawn(move || {
                    let mut snapshot = writer.snapshot().unwrap();
                    start.wait();
                    for index in 1..=20 {
                        let command = SessionCommit {
                            session_id: writer.session_id().to_owned(),
                            expected: snapshot.head,
                            identity: snapshot.identity.clone(),
                            metadata: metadata(index + 2, writer.session_id()),
                            history: HistorySuffix {
                                start: HistoryIndex::new(index as u64),
                                final_len: HistoryLen::new(index as u64 + 1),
                                items: vec![protocol::HistoryItem::system(format!(
                                    "{}:{index}",
                                    writer.session_id()
                                ))],
                            },
                            side_tables: SideTableSuffixes::default(),
                            transcript_records: None,
                        };
                        writer.commit_session(&command).unwrap();
                        snapshot = writer.snapshot().unwrap();
                    }
                    writer.release().unwrap();
                });
            }
        });
        for id in [&source_id, &target_id] {
            let reader = LineageSessionReader::open_existing(root.path(), id).unwrap();
            let expected: Vec<_> = std::iter::once(protocol::HistoryItem::system("first"))
                .chain((1..=20).map(|index| protocol::HistoryItem::system(format!("{id}:{index}"))))
                .collect();
            assert_eq!(reader.history_range(0, 21).unwrap(), expected);
            let report = reader.doctor_report().unwrap();
            assert!(report.healthy, "{:?}", report.issues);
        }
    }

    #[test]
    fn lineage_writer_owns_one_database_and_common_fork_writes_only_metadata() {
        let _ = common_fork_p95();
    }

    #[test]
    #[ignore = "optimized wall-clock benchmark; run in isolation"]
    fn common_fork_latency_benchmark() {
        let p95 = common_fork_p95();
        println!("COMMON_FORK_LATENCY forks=100 p95={p95:?}");
        assert!(
            p95 < std::time::Duration::from_millis(100),
            "100-fork p95 exceeded the interaction ceiling: {p95:?}"
        );
    }

    fn common_fork_p95() -> std::time::Duration {
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
        let p95 = fork_durations[94];
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
        p95
    }

    #[test]
    fn transcript_extent_profiles_follow_suffix_replacement_and_fork_reuse() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('4');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let initial = writer.commit_session(&initial_commit(&id)).unwrap();
        let records = (0..130)
            .map(|index| {
                transcript_record(
                    index,
                    format!(
                        "record {index}\n{}",
                        "wrapped text ".repeat(index as usize % 17)
                    ),
                )
            })
            .collect::<Vec<_>>();
        let mut append = initial_commit(&id);
        append.expected = initial.current;
        append.history = HistorySuffix {
            start: HistoryIndex::new(1),
            final_len: HistoryLen::new(1),
            items: Vec::new(),
        };
        append.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: records.clone(),
        });
        let appended = writer.commit_session(&append).unwrap();

        let replacement = (70..83)
            .map(|index| transcript_record(index, format!("replacement {index}\nline two")))
            .collect::<Vec<_>>();
        let mut split = append;
        split.expected = appended.current;
        split.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::new(70),
            records: replacement.clone(),
        });
        let replaced = writer.commit_session(&split).unwrap();

        let reader = LineageSessionReader::open_existing(root.path(), &id).unwrap();
        let mut expected = records[..70].to_vec();
        expected.extend(replacement);
        let expected_profile = crate::history::transcript_extent_profile(&expected);
        assert_eq!(
            reader
                .transcript_extent_profile(crate::TranscriptRecordRange::from(0..83))
                .unwrap(),
            expected_profile
        );
        assert_eq!(
            reader.transcript_total_estimated_rows(37).unwrap(),
            expected_profile.estimated_rows(37)
        );

        let fork_id = session_id('6');
        writer.fork_current(&fork_id, 20).unwrap();
        let fork = LineageSessionReader::open_existing(root.path(), &fork_id).unwrap();
        assert_eq!(
            fork.transcript_extent_profile(crate::TranscriptRecordRange::from(0..83))
                .unwrap(),
            expected_profile
        );
        drop(fork);
        let retained_node_profiles = row_count(&writer.conn, "lineage_transcript_extent_nodes");
        let retained_record_profiles =
            row_count(&writer.conn, "lineage_transcript_record_profiles");
        let lineage = LineageId::from_hex(writer.lineage_id().to_owned()).unwrap();
        let branch = BranchId::new(id.clone()).unwrap();

        let source_only = transcript_record(83, "source-only suffix".into());
        split.expected = replaced.current;
        split.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::new(83),
            records: vec![source_only],
        });
        writer.commit_session(&split).unwrap();
        assert!(
            row_count(&writer.conn, "lineage_transcript_extent_nodes") > retained_node_profiles
        );
        assert_eq!(
            row_count(&writer.conn, "lineage_transcript_record_profiles"),
            retained_record_profiles + 1
        );

        let current_root = lineage::lineage_session_snapshot(&writer.conn, &lineage, &branch)
            .unwrap()
            .transcript_root;
        let error = writer
            .conn
            .execute(
                "UPDATE lineage_transcript_extent_nodes
                 SET rows_20 = rows_20 + 1
                 WHERE lineage_id = ?1 AND node_id = (
                     SELECT node_id FROM lineage_sequence_roots
                     WHERE lineage_id = ?1 AND root_id = ?2
                 )",
                (lineage.as_str(), current_root.id().as_str()),
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("transcript extent nodes are immutable"));

        writer.delete_branch_by_id(&fork_id, 21).unwrap();
        writer.rewind_to_sequence(3, 22).unwrap();
        let mut node_profiles = row_count(&writer.conn, "lineage_transcript_extent_nodes");
        let mut record_profiles = row_count(&writer.conn, "lineage_transcript_record_profiles");
        let mut reclaimed_profile = false;
        let mut complete = false;
        let snapshot = writer.snapshot().unwrap();
        let vm_steps = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut peak_vm_steps = 0;
        let step_limit = row_count(&writer.conn, "lineage_sequence_entries")
            .saturating_add(row_count(&writer.conn, "lineage_payload_object_refs"))
            .saturating_mul(8)
            .saturating_add(256);
        for _ in 0..step_limit {
            vm_steps.store(0, std::sync::atomic::Ordering::Relaxed);
            let counter = vm_steps.clone();
            writer
                .conn
                .progress_handler(
                    1,
                    Some(move || {
                        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        false
                    }),
                )
                .unwrap();
            let reclamation = writer.reclaim_step(1).unwrap();
            writer
                .conn
                .progress_handler(0, None::<fn() -> bool>)
                .unwrap();
            peak_vm_steps = peak_vm_steps.max(vm_steps.load(std::sync::atomic::Ordering::Relaxed));
            assert!(reclamation.work_rows() <= 1);
            let remaining_nodes = row_count(&writer.conn, "lineage_transcript_extent_nodes");
            let remaining_records = row_count(&writer.conn, "lineage_transcript_record_profiles");
            assert!(node_profiles.saturating_sub(remaining_nodes) <= 1);
            assert!(record_profiles.saturating_sub(remaining_records) <= 1);
            reclaimed_profile |=
                remaining_nodes < node_profiles || remaining_records < record_profiles;
            node_profiles = remaining_nodes;
            record_profiles = remaining_records;
            complete = reclamation.complete;
            if complete {
                break;
            }
        }
        assert!(complete);
        assert_eq!(writer.snapshot().unwrap(), snapshot);
        println!("public GC deleted suffix: peak_vm_steps={peak_vm_steps}");
        assert!(
            peak_vm_steps < 16_384,
            "one-row deleted-suffix GC used {peak_vm_steps} VM steps"
        );
        assert!(reclaimed_profile);
        assert!(node_profiles > 0 && node_profiles <= retained_node_profiles);
        assert!(record_profiles > 0 && record_profiles <= retained_record_profiles);
        assert!(
            LineageSessionReader::open_existing(root.path(), &id)
                .unwrap()
                .transcript_total_estimated_rows(80)
                .unwrap()
                > 0
        );
    }

    #[test]
    fn sparse_extent_navigation_and_block_lookup_do_not_hydrate_payloads() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('7');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let mut records = (0..100)
            .map(|index| transcript_record(index, format!("record {index}")))
            .collect::<Vec<_>>();
        records[0].kind = "user".into();
        records[50].kind = "tool".into();
        let mut command = initial_commit(&id);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records,
        });
        writer.commit_session(&command).unwrap();
        writer
            .conn
            .execute(
                "UPDATE objects SET bytes = zeroblob(stored_size)
                 WHERE hash IN (
                     SELECT object_hash FROM lineage_payload_object_refs
                     WHERE payload_kind = 'transcript'
                 )",
                [],
            )
            .unwrap();

        let reader = LineageSessionReader::open_existing(root.path(), &id).unwrap();
        assert!(
            reader
                .transcript_extent_profile((10..90).into())
                .unwrap()
                .estimated_rows(80)
                >= 80
        );
        assert!(reader.transcript_total_estimated_rows(80).unwrap() >= 100);
        assert!(reader.transcript_record_for_row(80, 75).unwrap().is_some());
        let tool = reader
            .transcript_record_before_kind("tool", 99)
            .unwrap()
            .unwrap();
        assert_eq!(tool.record_index.get(), 50);
        assert_eq!(tool.profile.first_line, "record 50");
        assert_eq!(
            reader
                .transcript_record_after_kind("tool", 1)
                .unwrap()
                .unwrap()
                .record_index
                .get(),
            50
        );
        assert_eq!(
            reader
                .transcript_record_after_role("user", 0)
                .unwrap()
                .unwrap()
                .record_index
                .get(),
            0
        );
        assert_eq!(
            reader.transcript_record_index_for_block_idx(100).unwrap(),
            Some(50)
        );
        assert_eq!(
            reader.transcript_record_index_for_history_idx(0).unwrap(),
            Some(0)
        );
        assert!(reader.transcript_object_backed_range(50, 51).is_err());
    }

    #[test]
    fn transcript_history_lookup_returns_first_record_after_suffix_replacement() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('8');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let records = [Some(0), Some(0), None, Some(4), Some(8)]
            .into_iter()
            .enumerate()
            .map(|(index, history_idx)| {
                let mut record = transcript_record(index as u64, format!("record {index}"));
                record.history_idx = history_idx;
                record
            })
            .collect();
        let mut command = initial_commit(&id);
        command.history = HistorySuffix {
            start: HistoryIndex::ZERO,
            final_len: HistoryLen::new(10),
            items: (0..10)
                .map(|index| protocol::HistoryItem::system(format!("history {index}")))
                .collect(),
        };
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records,
        });
        let initial = writer.commit_session(&command).unwrap();

        let reader = LineageSessionReader::open_existing(root.path(), &id).unwrap();
        assert_eq!(
            reader.transcript_record_index_for_history_idx(0).unwrap(),
            Some(0)
        );
        assert_eq!(
            reader.transcript_record_index_for_history_idx(4).unwrap(),
            Some(3)
        );
        assert_eq!(
            reader.transcript_record_index_for_history_idx(8).unwrap(),
            Some(4)
        );
        assert_eq!(
            reader.transcript_record_index_for_history_idx(7).unwrap(),
            None
        );
        drop(reader);

        let replacement = [Some(2), Some(2), Some(9)]
            .into_iter()
            .enumerate()
            .map(|(offset, history_idx)| {
                let index = offset + 2;
                let mut record = transcript_record(index as u64, format!("replacement {index}"));
                record.history_idx = history_idx;
                record
            })
            .collect();
        command.expected = initial.current;
        command.history = HistorySuffix {
            start: HistoryIndex::new(10),
            final_len: HistoryLen::new(10),
            items: Vec::new(),
        };
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::new(2),
            records: replacement,
        });
        writer.commit_session(&command).unwrap();

        let reader = LineageSessionReader::open_existing(root.path(), &id).unwrap();
        assert_eq!(
            reader.transcript_record_index_for_history_idx(0).unwrap(),
            Some(0)
        );
        assert_eq!(
            reader.transcript_record_index_for_history_idx(2).unwrap(),
            Some(2)
        );
        assert_eq!(
            reader.transcript_record_index_for_history_idx(9).unwrap(),
            Some(4)
        );
        assert_eq!(
            reader.transcript_record_index_for_history_idx(4).unwrap(),
            None
        );
        assert_eq!(
            reader.transcript_record_index_for_history_idx(8).unwrap(),
            None
        );
    }

    #[test]
    fn lineage_sparse_transcript_slices_defer_nested_object_hydration() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('5');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        let mut record = transcript_record(0, "visible text".into());
        record.tool_render_revision = 47;
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
        assert_eq!(sparse.records[0].tool_render_revision, 47);
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
                result: protocol::ToolOutcome::new(
                    "visible text".into(),
                    false,
                    Some(serde_json::json!({"payload": "x".repeat(16 * 1024)})),
                ),
                elapsed_ms: None,
                called_at_ms: None,
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
                    history_idx: Some(0),
                },
                crate::TranscriptSearchCandidate {
                    block_idx: 514,
                    history_idx: Some(0),
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
                    history_idx: Some(0),
                },
                crate::TranscriptSearchCandidate {
                    block_idx: 598,
                    history_idx: Some(0),
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
        records.extend((3..43).map(|index| {
            transcript_record(
                index,
                format!("abc {} false {index} bcd", "x".repeat(33 * 1024)),
            )
        }));
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

        let search_path = reader.search_database_path();
        let search = Connection::open(search_path).unwrap();
        let document_columns = search
            .prepare("PRAGMA table_info(search_docs)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let segment_columns = search
            .prepare("PRAGMA table_info(search_segments)")
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
                "first_record_ordinal",
                "last_record_ordinal",
                "min_block_idx",
                "max_block_idx",
            ],
            "search path={} tables={table_names:?}",
            reader.search_database_path().display(),
        );
        assert_eq!(
            segment_columns,
            [
                "segment_id",
                "source_node_id",
                "source_item_count",
                "source_byte_count",
                "min_block_idx",
                "max_block_idx",
                "logical_text_bytes",
                "doc_count",
                "first_doc_id",
                "last_doc_id",
                "complete",
            ]
        );
        for expected in [
            "search_root_manifests",
            "search_root_sources",
            "search_source_leaves",
        ] {
            assert!(
                table_names.iter().any(|name| name == expected),
                "missing {expected}: {table_names:?}"
            );
        }
        let (manifest_count, manifest_sources, manifest_items): (i64, i64, i64) = search
            .query_row(
                "SELECT COUNT(*),
                        (SELECT COUNT(*) FROM search_root_sources),
                        COALESCE(SUM(item_count), 0)
                 FROM search_root_manifests",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(manifest_count, 1);
        assert_eq!(
            manifest_sources,
            i64::try_from(status.total_segments).unwrap()
        );
        assert_eq!(manifest_items, i64::try_from(records.len()).unwrap());
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
    fn cold_search_pruning_removes_multiple_obsolete_segments_and_is_idempotent() {
        for corrupt_late_segment in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let id = session_id('1');
            let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
            let mut command = initial_commit(&id);
            command.transcript_records = Some(crate::TranscriptRecordSuffix {
                start: crate::TranscriptRecordIndex::ZERO,
                records: (0..1024)
                    .map(|index| transcript_record(index, format!("retained initial {index}")))
                    .collect(),
            });
            let original = command.clone();
            let receipt = writer.commit_session(&command).unwrap();
            let reader = LineageSessionReader::open_existing(root.path(), &id).unwrap();
            let projector = writer.spawn_search_projector().unwrap();
            projector.request();
            assert_eq!(wait_for_search_projection(&reader).ready_segments, 1);
            assert_eq!(writer.prune_search_projection().unwrap(), 0);
            for batch in 1..=2 {
                command.expected = writer.store_head().unwrap();
                command.metadata.updated_at = batch + 1;
                command.history = HistorySuffix {
                    start: HistoryIndex::new(1),
                    final_len: HistoryLen::new(1),
                    items: Vec::new(),
                };
                command.side_tables.start = HistoryIndex::new(1);
                let start = batch as u64 * 1024;
                command.transcript_records = Some(crate::TranscriptRecordSuffix {
                    start: crate::TranscriptRecordIndex::new(start),
                    records: (start..start + 1024)
                        .map(|index| transcript_record(index, format!("obsolete suffix {index}")))
                        .collect(),
                });
                writer.commit_session(&command).unwrap();
                projector.request();
                assert_eq!(
                    wait_for_search_projection(&reader).ready_segments,
                    batch as usize + 1
                );
            }
            drop(projector);
            if corrupt_late_segment {
                let search = Connection::open(reader.search_database_path()).unwrap();
                let damaged = search
                    .execute(
                        "UPDATE search_short_postings SET docs = x'80'
             WHERE segment_id = (SELECT MAX(segment_id) FROM search_segments)",
                        [],
                    )
                    .unwrap();
                assert!(damaged > 0);
                drop(search);
                assert_eq!(
                    reader.search_projection_status().unwrap().state,
                    crate::SearchProjectionState::Corrupt
                );
                let projector = writer.spawn_search_projector().unwrap();
                projector.request();
                assert_eq!(wait_for_search_projection(&reader).ready_segments, 3);
                drop(projector);
            }
            writer.rewind_to_sequence(1, 4).unwrap();
            if corrupt_late_segment {
                // The reset rebuilt the current root, not its historical manifests.
                assert_eq!(
                    reader.search_projection_status().unwrap().state,
                    crate::SearchProjectionState::Partial
                );
                let projector = writer.spawn_search_projector().unwrap();
                projector.request();
                assert_eq!(wait_for_search_projection(&reader).ready_segments, 1);
                drop(projector);
            }
            let snapshot = writer.snapshot().unwrap();
            assert_eq!(writer.prune_search_projection().unwrap(), 2);
            assert_eq!(writer.prune_search_projection().unwrap(), 0);
            assert_eq!(writer.snapshot().unwrap(), snapshot);
            assert_eq!(writer.commit_session(&original).unwrap(), receipt);
            assert_eq!(
                reader.search_projection_status().unwrap().state,
                crate::SearchProjectionState::Current
            );
            assert!(reader
                .search_transcript_candidate_page(
                    "obsolete suffix",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    10,
                )
                .unwrap()
                .is_empty());
            assert_eq!(
                reader
                    .search_transcript_candidate_page(
                        "retained initial",
                        None,
                        crate::TranscriptSearchDirection::Forward,
                        3,
                    )
                    .unwrap()
                    .len(),
                3
            );
            let search = Connection::open_with_flags(
                reader.search_database_path(),
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .unwrap();
            let (segments, obsolete_hits, manifests): (i64, i64, i64) = search
                .query_row(
                    "SELECT (SELECT count(*) FROM search_segments),
                    (SELECT count(*) FROM search_fts WHERE search_fts MATCH 'obs'),
                    (SELECT count(*) FROM search_root_manifests)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!((segments, obsolete_hits, manifests), (1, 0, 1));
        }
    }

    #[test]
    fn cold_search_pruning_preserves_ready_cache_under_writer_contention() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('2');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let mut command = initial_commit(&id);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: (0..1024)
                .map(|index| transcript_record(index, format!("retained initial {index}")))
                .collect(),
        });
        let original = command.clone();
        let receipt = writer.commit_session(&command).unwrap();
        let reader = LineageSessionReader::open_existing(root.path(), &id).unwrap();
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        assert_eq!(wait_for_search_projection(&reader).ready_segments, 1);
        command.expected = receipt.current;
        command.metadata.updated_at = 2;
        command.history = HistorySuffix {
            start: HistoryIndex::new(1),
            final_len: HistoryLen::new(1),
            items: Vec::new(),
        };
        command.side_tables.start = HistoryIndex::new(1);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::new(1024),
            records: (1024..2048)
                .map(|index| transcript_record(index, format!("obsolete suffix {index}")))
                .collect(),
        });
        writer.commit_session(&command).unwrap();
        projector.request();
        assert_eq!(wait_for_search_projection(&reader).ready_segments, 2);
        drop(projector);
        writer.rewind_to_sequence(1, 3).unwrap();
        let snapshot = writer.snapshot().unwrap();
        let candidates = reader
            .search_transcript_candidate_page(
                "retained initial",
                None,
                crate::TranscriptSearchDirection::Forward,
                3,
            )
            .unwrap();
        let path = reader.search_database_path();
        let search = Connection::open(&path).unwrap();
        let segments = || {
            search
                .query_row("SELECT count(*) FROM search_segments", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        assert_eq!(segments(), 2);
        search.pragma_update(None, "foreign_keys", "ON").unwrap();
        for stale_manifests in [true, false] {
            if !stale_manifests {
                assert_eq!(
                    search
                        .execute(
                            "DELETE FROM search_root_manifests WHERE item_count = 2048",
                            [],
                        )
                        .unwrap(),
                    1
                );
            }
            search.execute_batch("BEGIN IMMEDIATE").unwrap();
            let result = writer.prune_search_projection();
            assert_eq!(writer.snapshot().unwrap(), snapshot);
            assert_eq!(writer.commit_session(&original).unwrap(), receipt);
            assert!(
                path.exists(),
                "contended pruning removed a healthy derived search database: {result:?}"
            );
            assert!(
                matches!(result,
                    Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
                        if error.code == rusqlite::ErrorCode::DatabaseBusy
                ),
                "contended pruning must report busy, not repair or succeed"
            );
            assert_eq!(segments(), 2);
            assert_eq!(
                reader.search_projection_status().unwrap().state,
                crate::SearchProjectionState::Current
            );
            assert_eq!(
                reader
                    .search_transcript_candidate_page(
                        "retained initial",
                        None,
                        crate::TranscriptSearchDirection::Forward,
                        3,
                    )
                    .unwrap(),
                candidates
            );
            search.execute_batch("ROLLBACK").unwrap();
        }
        assert_eq!(writer.prune_search_projection().unwrap(), 1);
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
        assert_eq!(segments(), 1);
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Current
        );
        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "retained initial",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    3,
                )
                .unwrap(),
            candidates
        );
        assert_eq!(writer.snapshot().unwrap(), snapshot);
        assert_eq!(writer.commit_session(&original).unwrap(), receipt);
        assert!(reader.doctor_report().unwrap().healthy);
    }

    #[test]
    fn cold_search_pruning_rebuilds_after_canonical_source_reclamation() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('2');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let mut command = initial_commit(&id);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: (0..1024)
                .map(|index| transcript_record(index, format!("retained needle {index}")))
                .collect(),
        });
        let original = command.clone();
        let receipt = writer.commit_session(&command).unwrap();
        let reader = LineageSessionReader::open_existing(root.path(), &id).unwrap();
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        assert_eq!(wait_for_search_projection(&reader).ready_segments, 1);
        command.expected = receipt.current;
        command.metadata.updated_at = 2;
        command.history = HistorySuffix {
            start: HistoryIndex::new(1),
            final_len: HistoryLen::new(1),
            items: Vec::new(),
        };
        command.side_tables.start = HistoryIndex::new(1);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::new(1024),
            records: vec![transcript_record(1024, "obsolete suffix".into())],
        });
        writer.commit_session(&command).unwrap();
        projector.request();
        assert_eq!(wait_for_search_projection(&reader).ready_segments, 2);
        drop(projector);
        writer.rewind_to_sequence(1, 3).unwrap();
        let snapshot = writer.snapshot().unwrap();
        let candidates = reader
            .search_transcript_candidate_page(
                "needle",
                None,
                crate::TranscriptSearchDirection::Forward,
                3,
            )
            .unwrap();
        let search_path = reader.search_database_path();
        let source_nodes = {
            let search = Connection::open(&search_path).unwrap();
            let mut leaves = search
                .prepare("SELECT node_id FROM search_source_leaves")
                .unwrap();
            let nodes = leaves
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            nodes
        };
        let mut complete = false;
        for _ in 0..lineage::reclamation_step_limit(&writer.conn, &writer.lineage) {
            let step = writer.reclaim_step(1).unwrap();
            assert!(step.work_rows() <= 1);
            if step.complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert!(
            source_nodes.iter().any(|node| {
                !writer
                    .conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM lineage_sequence_nodes
                 WHERE lineage_id = ?1 AND node_id = ?2)",
                        (writer.lineage.as_str(), node),
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap()
            }),
            "fixture must actually reclaim an obsolete projected leaf"
        );
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
        assert!(!search_path.exists());
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "needle",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    3,
                )
                .unwrap(),
            candidates
        );
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        assert_eq!(wait_for_search_projection(&reader).ready_segments, 1);
        drop(projector);
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
        assert_eq!(writer.snapshot().unwrap(), snapshot);
        assert_eq!(writer.commit_session(&original).unwrap(), receipt);
        assert!(reader.doctor_report().unwrap().healthy);
    }

    #[test]
    fn cold_search_open_and_projector_preserve_cache_under_exclusive_contention() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('2');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let mut command = initial_commit(&id);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: (0..70)
                .map(|index| transcript_record(index, format!("retained needle {index}")))
                .collect(),
        });
        let receipt = writer.commit_session(&command).unwrap();
        let reader = LineageSessionReader::open_existing(root.path(), &id).unwrap();
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        wait_for_search_projection(&reader);
        drop(projector);
        let snapshot = writer.snapshot().unwrap();
        let candidates = reader
            .search_transcript_candidate_page(
                "needle",
                None,
                crate::TranscriptSearchDirection::Forward,
                10,
            )
            .unwrap();
        let path = reader.search_database_path();
        let search = Connection::open(&path).unwrap();
        search
            .execute_batch("PRAGMA journal_mode = DELETE")
            .unwrap();
        let original_bytes = fs::read(&path).unwrap();
        search.execute_batch("BEGIN EXCLUSIVE").unwrap();
        assert!(matches!(writer.prune_search_projection(),
            Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
                if error.code == rusqlite::ErrorCode::DatabaseBusy
        ));
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !projector.is_idle() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(projector.latest_error().unwrap().contains("locked"));
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        search.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Current
        );
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
        projector.request();
        wait_for_search_projection(&reader);
        while !projector.is_idle() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(projector.latest_error(), None);
        assert_eq!(
            reader
                .search_transcript_candidate_page(
                    "needle",
                    None,
                    crate::TranscriptSearchDirection::Forward,
                    10,
                )
                .unwrap(),
            candidates
        );
        assert_eq!(writer.snapshot().unwrap(), snapshot);
        assert_eq!(writer.commit_session(&command).unwrap(), receipt);
        assert!(reader.doctor_report().unwrap().healthy);
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
        let search_path = reader.search_database_path();

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
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
        loop {
            let reclamation = writer.reclaim_step(1).unwrap();
            assert!(reclamation.work_rows() <= 1);
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
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
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
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
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
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Partial
        );
        let projector = writer.spawn_search_projector().unwrap();
        projector.request();
        let rebuilt = wait_for_search_projection(&reader);
        assert_eq!(rebuilt.ready_segments, rebuilt.total_segments);
        drop(projector);

        for table in [
            "search_meta",
            "search_root_manifests",
            "search_source_leaves",
        ] {
            let search = Connection::open(&search_path).unwrap();
            search
                .execute_batch(&format!("DROP TABLE {table}"))
                .unwrap();
            drop(search);
            assert_eq!(writer.prune_search_projection().unwrap(), 0);
            assert_eq!(
                reader.search_projection_status().unwrap().state,
                crate::SearchProjectionState::Partial
            );
            let projector = writer.spawn_search_projector().unwrap();
            projector.request();
            wait_for_search_projection(&reader);
            drop(projector);
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
        }

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
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
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
        assert!(source.transcript_total_estimated_rows(80).unwrap() >= 1024);
        let source_status = wait_for_search_projection(&source);
        assert_eq!(source_status.total_segments, 1);
        assert_eq!(source_status.ready_segments, 1);
        drop(projector);
        let search_path = source.search_database_path();
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

        writer.release().unwrap();
        let mut writer = OwnedLineageWriter::open_existing(root.path(), &target_id).unwrap();
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
                history_idx: Some(0),
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
                history_idx: Some(0),
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
                history_idx: Some(0),
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

        assert_eq!(writer.prune_search_projection().unwrap(), 1);
        assert_eq!(writer.prune_search_projection().unwrap(), 0);
        let mut complete = false;
        let mut calls = 0;
        let step_limit = lineage::reclamation_step_limit(&writer.conn, &writer.lineage);
        for _ in 0..step_limit {
            let step = writer.reclaim_step(1).unwrap();
            calls += 1;
            assert!(step.work_rows() <= 1);
            assert!(step.complete || step.made_progress());
            if step.complete {
                complete = true;
                break;
            }
        }
        eprintln!(
            "search/fork reclamation: calls={calls} step_limit={step_limit} complete={complete}"
        );
        assert!(complete, "search/fork reclamation did not complete");
        assert_eq!(segment_count(), 1);
        let search = Connection::open_with_flags(
            &search_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let (docs, obsolete_fts_hits, root_manifests): (i64, i64, i64) = search
            .query_row(
                "SELECT (SELECT COUNT(*) FROM search_docs),
                        (SELECT COUNT(*) FROM search_fts WHERE search_fts MATCH 'tar'),
                        (SELECT COUNT(*) FROM search_root_manifests)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(docs > 0);
        assert_eq!(obsolete_fts_hits, 0);
        assert_eq!(root_manifests, 1);
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
                history_idx: Some(0),
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
        assert_eq!(cleanup_abandoned_lineages(root.path(), 1).unwrap(), 1);
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

        assert_eq!(cleanup_abandoned_lineages(root.path(), 1).unwrap(), 0);
        assert!(lineage_directory.is_dir());
        writer.release().unwrap();
        assert_eq!(cleanup_abandoned_lineages(root.path(), 1).unwrap(), 1);
        assert!(!lineage_directory.exists());
    }

    #[test]
    fn lineage_cleanup_bounds_candidates_inspected() {
        let root = tempfile::tempdir().unwrap();
        let active_lineage = LineageId::from_hex("a".repeat(32)).unwrap();
        let _lease = LineageLease::acquire(root.path(), &active_lineage).unwrap();
        let trash = crate::SessionStoreLayout::from_sessions_root(root.path()).trash_dir();
        ensure_private_directory(&trash).unwrap();
        let active_tombstone = trash.join(format!("{}.interrupted", active_lineage.as_str()));
        ensure_private_directory(&active_tombstone).unwrap();

        let abandoned_id = session_id('b');
        let mut writer = OwnedLineageWriter::open(root.path(), &abandoned_id).unwrap();
        writer
            .commit_session(&initial_commit(&abandoned_id))
            .unwrap();
        let abandoned_dir = writer.database_path().parent().unwrap().to_path_buf();
        writer.delete_branch_by_id(&abandoned_id, 2).unwrap();
        writer.release().unwrap();

        assert_eq!(cleanup_abandoned_lineages(root.path(), 1).unwrap(), 0);
        assert!(active_tombstone.exists());
        assert!(abandoned_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn lineage_cleanup_rejects_a_symlinked_trash_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let lineages = root.path();
        let external = tempfile::tempdir().unwrap();
        fs::write(external.path().join("sentinel"), b"keep").unwrap();
        symlink(
            external.path(),
            crate::SessionStoreLayout::from_sessions_root(lineages).trash_dir(),
        )
        .unwrap();

        assert!(matches!(
            cleanup_abandoned_lineages(root.path(), 1),
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

        let trash = crate::SessionStoreLayout::from_sessions_root(root.path()).trash_dir();
        ensure_private_directory(&trash).unwrap();
        let tombstone = trash.join(format!("{}.interrupted", lineage.as_str()));
        fs::rename(&source, &tombstone).unwrap();
        assert_eq!(cleanup_abandoned_lineages(root.path(), 1).unwrap(), 1);
        assert!(!tombstone.exists());
        assert_eq!(cleanup_abandoned_lineages(root.path(), 1).unwrap(), 0);
    }

    #[test]
    fn lineage_doctor_backup_stats_and_vacuum_cover_canonical_database() {
        let root = tempfile::tempdir().unwrap();
        let session_id = session_id('7');
        let mut writer = OwnedLineageWriter::open(root.path(), &session_id).unwrap();
        let mut commit = initial_commit(&session_id);
        commit.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: vec![transcript_record(0, "doctor extent".into())],
        });
        writer.commit_session(&commit).unwrap();
        writer
            .append_request_attempt(&request_entry(1), RequestAuditPayloadMode::Full)
            .unwrap();
        writer.vacuum().unwrap();
        let lineage_id = writer.lineage_id().to_owned();
        let database_path = writer.database_path();
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

        let corrupt = Connection::open(database_path).unwrap();
        corrupt
            .execute("DELETE FROM lineage_transcript_extent_nodes", [])
            .unwrap();
        drop(corrupt);
        let report = reader.doctor_report().unwrap();
        assert!(!report.healthy);
        assert!(
            report.issues.iter().any(|issue| {
                issue.contains("canonical branch") && issue.contains("transcript extent node")
            }),
            "{:?}",
            report.issues
        );
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
        writer.release().unwrap();
        let mut writer = OwnedLineageWriter::open_existing(root.path(), &target_id).unwrap();
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
    fn receipt_result_ownership_survives_rewind_gc_reopen_and_noop_replay() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('c');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let first = writer.commit_session(&initial_commit(&id)).unwrap();
        let mut changed = initial_commit(&id);
        changed.expected = first.current;
        changed.history.start = HistoryIndex::new(1);
        changed.history.items.clear();
        changed.metadata = metadata(2, "retained result");
        changed.metadata.first_user_message = Some("synthetic retained message α\n".repeat(512));
        changed.metadata.checkpoint_json = Some(serde_json::json!({
            "kind": "auto", "summary": "synthetic retained summary 日本語\n".repeat(512),
            "first_live_index": 0, "completed_at_history_len": 1, "created_at_ms": 2,
        }));
        changed.metadata.checkpoint_events_json = Some(serde_json::json!([changed
            .metadata
            .checkpoint_json
            .clone()
            .unwrap()]));
        changed.side_tables.start = HistoryIndex::new(1);
        changed.side_tables.metadata_snapshots.push((
            HistoryIndex::new(1),
            serde_json::json!({"title": changed.metadata.title,
                "first_user_message": changed.metadata.first_user_message}),
        ));
        let second = writer.commit_session(&changed).unwrap();
        let original = writer.snapshot().unwrap();
        let mut noop = changed;
        noop.expected = second.current;
        let owned = writer.commit_session_with_result(&noop).unwrap();
        assert_eq!(owned.revision_id, original.revision_id);
        let receipt = owned.receipt;
        assert_eq!(receipt.current, second.current);
        let exact_receipt = serde_json::to_vec(&receipt).unwrap();
        let rewind = writer.rewind_to_sequence(1, 3).unwrap();
        assert!(rewind.current.revision > receipt.current.revision);
        assert_ne!(writer.snapshot().unwrap().revision_id, original.revision_id);
        let mut complete = false;
        for _ in 0..1000 {
            let step = writer.reclaim_step(1).unwrap();
            assert!(step.work_rows() <= 1);
            if step.complete {
                complete = true;
                break;
            }
        }
        assert!(complete, "bounded fixture reclamation must finish");
        let lineage_id = writer.lineage_id().to_owned();
        writer.release().unwrap();
        let mut writer =
            OwnedLineageWriter::open_existing_in_lineage(root.path(), &lineage_id, &id).unwrap();
        let replay = writer.commit_session_with_result(&noop).unwrap();
        assert_eq!(replay.revision_id, original.revision_id);
        let replay = replay.receipt;
        assert_eq!(serde_json::to_vec(&replay).unwrap(), exact_receipt);
        assert_eq!(writer.store_head().unwrap(), rewind.current);
        let lineage = LineageId::from_hex(lineage_id).unwrap();
        let branch = BranchId::new(id).unwrap();
        let result = lineage::branch_revision_at_sequence(
            &writer.conn,
            &lineage,
            &branch,
            replay.current.revision.get(),
        );
        assert!(result.is_ok(),
            "a result-owning receipt must retain its exact revision after rewind, GC and replay: {result:?}");
        let result = lineage::load_revision(&writer.conn, &lineage, &result.unwrap()).unwrap();
        assert_eq!(result.id().as_str(), original.revision_id);
        let state = serde_json::to_value(
            lineage::load_revision_state(&writer.conn, &lineage, &result).unwrap(),
        )
        .unwrap();
        for (field, expected) in [
            (
                "first_user_message",
                serde_json::to_value(original.metadata.first_user_message).unwrap(),
            ),
            (
                "checkpoint_json",
                serde_json::to_value(original.metadata.checkpoint_json).unwrap(),
            ),
            (
                "checkpoint_events_json",
                serde_json::to_value(original.metadata.checkpoint_events_json).unwrap(),
            ),
        ] {
            assert_eq!(state["metadata"][field], expected);
        }
        assert_eq!(
            state["side_tables"],
            serde_json::to_value(original.side_tables).unwrap()
        );
    }

    #[test]
    fn degraded_direct_search_yields_promptly_when_its_generation_is_cancelled() {
        let root = tempfile::tempdir().unwrap();
        let id = session_id('e');
        let mut writer = OwnedLineageWriter::open(root.path(), &id).unwrap();
        let mut command = initial_commit(&id);
        command.transcript_records = Some(crate::TranscriptRecordSuffix {
            start: crate::TranscriptRecordIndex::ZERO,
            records: (0..512)
                .map(|index| {
                    let mut record =
                        transcript_record(index, format!("ordinary {index} {}", "x".repeat(2048)));
                    record.history_idx = None;
                    record
                })
                .collect(),
        });
        writer.commit_session(&command).unwrap();

        let reader = LineageSessionReader::open_existing(root.path(), &id).unwrap();
        assert_eq!(
            reader.search_projection_status().unwrap().state,
            crate::SearchProjectionState::Missing
        );
        let cancellation_checks = std::cell::Cell::new(0_usize);
        let result = reader.search_transcript_candidate_page_with_cancellation(
            "missing needle",
            None,
            crate::TranscriptSearchDirection::Forward,
            64,
            || {
                let next = cancellation_checks.get() + 1;
                cancellation_checks.set(next);
                next >= 12
            },
        );
        assert!(matches!(result, Err(StoreError::Cancelled)));
        assert!(
            cancellation_checks.get() <= 12,
            "degraded search continued polling after cancellation"
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
        let immutable_revision = writer.snapshot().unwrap().revision_id;
        writer.release().unwrap();

        let mut reopened = OwnedLineageWriter::open_existing(root.path(), &id).unwrap();
        let recovery = reopened
            .take_startup_recovery()
            .expect("running turn is interrupted before the writer becomes available");
        assert_eq!(recovery.interrupted_turns, vec![receipt.turn_id]);
        assert_eq!(recovery.session.revision_id, immutable_revision);
        assert_eq!(reopened.snapshot().unwrap().revision_id, immutable_revision);
        assert_eq!(
            recovery.session.receipt.previous,
            running_receipt.session.current
        );
        assert_eq!(
            recovery.session.receipt.current.revision.get(),
            running_receipt.session.current.revision.get() + 1
        );
        assert_eq!(
            reopened.latest_terminal_turn_id().unwrap(),
            Some(receipt.turn_id)
        );
    }
}
