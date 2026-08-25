use std::cell::RefCell;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

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

    fn acquire_branch(root: &Path, branch: &BranchId) -> Result<Self> {
        Self::acquire_named(root, branch.as_str())
    }

    fn acquire_named(root: &Path, name: &str) -> Result<Self> {
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
        match fs4::FileExt::try_lock(&file) {
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
    startup_recovery: Option<crate::session_commit::StartupRecoveryReceipt>,
    connection_invalidated: bool,
    catalog: RefCell<Option<Catalog>>,
    _lease: LineageLease,
    branch_lease: Option<LineageLease>,
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

impl OwnedLineageWriter {
    pub fn open(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Self::open_inner(root.as_ref(), session_id.into(), true)
    }

    pub fn open_existing(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Self::open_inner(root.as_ref(), session_id.into(), false)
    }

    pub fn open_existing_in_lineage(
        root: impl AsRef<Path>,
        lineage_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Self> {
        let root = root.as_ref();
        let branch = BranchId::new(session_id.into())?;
        validate_storage_root(root)?;
        let _branch_lease = LineageLease::acquire_branch(root, &branch)?;
        let lineage = LineageId::from_hex(lineage_id.into())?;
        let lease = LineageLease::acquire(root, &lineage)?;
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
        Self::finish_open(root, lineage, branch, conn, lease, None)
    }

    fn open_inner(root: &Path, session_id: String, create: bool) -> Result<Self> {
        let branch = BranchId::new(session_id)?;
        validate_storage_root(root)?;
        let branch_lease = LineageLease::acquire_branch(root, &branch)?;
        let located = locate_lineage(root, &branch)?;
        let is_new = located.is_none();
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
        let lease = LineageLease::acquire(root, &lineage)?;
        let path = lineage_database_path(root, &lineage);
        let mut conn = open_write_connection(&path, &lineage)?;
        if !lineage_exists(&conn, &lineage)? {
            lineage::create_lineage(&conn, &lineage, unix_timestamp_seconds()?)?;
        }
        crate::schema::initialize_lineage_schema(&mut conn)?;
        Self::finish_open(
            root,
            lineage,
            branch,
            conn,
            lease,
            is_new.then_some(branch_lease),
        )
    }

    fn finish_open(
        root: &Path,
        lineage: LineageId,
        branch: BranchId,
        mut conn: Connection,
        lease: LineageLease,
        branch_lease: Option<LineageLease>,
    ) -> Result<Self> {
        let _catalog_pending = lineage::lineage_has_nonterminal_turns(&conn, &lineage, &branch)?
            .then(|| crate::catalog::mark_catalog_session_pending(root, branch.as_str()))
            .transpose()?;
        let startup_recovery = lineage::recover_lineage_nonterminal_turns(
            &mut conn,
            &lineage,
            &branch,
            unix_timestamp_millis()?,
        )?;
        Ok(Self {
            sessions_root: root.to_path_buf(),
            lineage,
            branch,
            conn,
            startup_recovery,
            connection_invalidated: false,
            catalog: RefCell::new(None),
            _lease: lease,
            branch_lease,
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
        let _catalog_pending =
            crate::catalog::mark_catalog_session_pending(&self.sessions_root, self.branch.as_str())
                .map_err(crate::session_command::commit_failure_from_store_error)?;
        let receipt = lineage::apply_lineage_session_commit(
            &mut self.conn,
            &self.lineage,
            &self.branch,
            command,
            ObjectCompression::default(),
        )?;
        self.branch_lease = None;
        self.publish_catalog_for_commit(command, &receipt)
            .map_err(crate::session_command::commit_failure_from_store_error)?;
        Ok(receipt)
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
            self.branch_lease = None;
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

    pub fn refresh_catalog(&self) -> Result<()> {
        self.refresh_catalog_branch(&self.branch)
    }

    pub fn publish_catalog_for_commit(
        &self,
        command: &SessionCommit,
        receipt: &SaveReceipt,
    ) -> Result<()> {
        let session =
            CatalogSession::from_commit(command, receipt, Some(self.lineage.as_str().to_string()));
        self.upsert_catalog_session(&session)?;
        Ok(())
    }

    fn refresh_catalog_branch(&self, branch: &BranchId) -> Result<()> {
        let snapshot = public_snapshot(
            &self.lineage,
            lineage::lineage_session_snapshot(&self.conn, &self.lineage, branch)?,
        )?;
        let metadata = &snapshot.metadata;
        let session = CatalogSession {
            id: branch.as_str().to_string(),
            lineage_id: Some(self.lineage.as_str().to_string()),
            title: metadata.title.clone(),
            slug: metadata.slug.clone(),
            first_user_message: metadata.first_user_message.clone(),
            cwd: metadata.cwd.clone(),
            mode: metadata.mode.clone(),
            reasoning_effort: metadata.reasoning_effort.clone(),
            model: metadata.model.clone(),
            fast_mode: metadata.fast_mode,
            parent_id: snapshot.identity.parent_id.clone(),
            context_tokens: metadata.display_context_tokens.or(metadata.context_tokens),
            history_len: Some(snapshot.head.history_len.get()),
            text_bytes: Some(snapshot.history_text_bytes),
            created_at: snapshot.identity.created_at,
            updated_at: metadata.updated_at,
            source_revision: snapshot.head.revision.get(),
            availability: CatalogAvailability::Available,
            error_kind: None,
            error_summary: None,
            last_seen_scan: 0,
        };
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
        if let Some(lineage) = locate_lineage(&self.sessions_root, &target)? {
            return Err(StoreError::Integrity(format!(
                "session {} already exists in lineage {}",
                target.as_str(),
                lineage.as_str()
            )));
        }
        let _catalog_pending =
            crate::catalog::mark_catalog_session_pending(&self.sessions_root, target.as_str())?;
        let source = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
        lineage::fork_branch(
            &mut self.conn,
            &self.lineage,
            &self.branch,
            &target,
            Some(&source.revision_id),
            created_at,
        )?;
        self.refresh_catalog_branch(&target)?;
        Ok(SaveReceipt {
            session_id: target.as_str().to_owned(),
            previous: StoreHead::default(),
            current: StoreHead {
                revision: crate::session_commit::Revision::new(1),
                history_len: source.head.history_len,
                transcript_record_count: source.head.transcript_record_count,
            },
            lineage_id: Some(self.lineage.as_str().to_owned()),
            history_text_bytes: source.history_root.byte_count(),
        })
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

    pub fn reclaim_step(&mut self, max_rows: usize) -> Result<LineageReclamation> {
        if max_rows == 0 {
            return Err(StoreError::Integrity(
                "lineage reclamation row budget must be positive".into(),
            ));
        }
        let search = crate::lineage_search::reclaim_one_obsolete_search_segment(
            &self.conn,
            &self.search_database_path(),
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

pub fn lineage_session_ids(root: impl AsRef<Path>) -> Result<Vec<String>> {
    let root = root.as_ref();
    validate_storage_root(root)?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    ensure_private_directory(root)?;
    let mut ids = Vec::new();
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

    pub fn transcript_extent_profile(
        &self,
        range: crate::TranscriptRecordRange,
    ) -> Result<crate::TranscriptExtentProfile> {
        let snapshot = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
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
        let snapshot = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_estimated_rows(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            range,
            width,
        )
    }

    pub fn transcript_total_estimated_rows(&self, width: u16) -> Result<u64> {
        let snapshot = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
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
        let snapshot = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
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
        let snapshot = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
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
        let snapshot = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
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
        let snapshot = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
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
        let snapshot = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
        lineage::lineage_transcript_record_after_role(
            &self.conn,
            &self.lineage,
            &snapshot.transcript_root,
            role,
            after_or_at,
        )
    }

    pub fn transcript_record_index_for_block_idx(&self, block_idx: u64) -> Result<Option<usize>> {
        let snapshot = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
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
        let snapshot = lineage::lineage_session_snapshot(&self.conn, &self.lineage, &self.branch)?;
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
        for _ in 0..512 {
            let reclamation = writer.reclaim_step(1).unwrap();
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
