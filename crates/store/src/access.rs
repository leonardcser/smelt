use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;

#[cfg(test)]
use crate::blob_staging::BLOB_STAGING_DIR;
use crate::blob_staging::{recover_blob_staging, stage_session_blobs, SessionBlob};
use crate::db::SessionDb;
use crate::{
    ObjectMeta, RequestAuditPayloadMode, RequestAuditPayloads, RequestAuditQuery,
    RequestAuditStats, RequestAuditSummary, Result, SaveReceipt, SessionCommit,
    SessionCommitFailure, SessionMeta, SessionSaveReport, SessionSnapshot, SessionState,
    StoreError, StoredObject, TranscriptBlockMetadataRecord, TranscriptDescriptorIndex,
    TranscriptDescriptorRange, TranscriptDescriptorRecord, TranscriptDescriptorSlice,
    TranscriptSearchCandidate, TranscriptSearchDirection, WriterOwner,
};

#[derive(Debug)]
pub struct SessionReader {
    db: SessionDb,
}

impl SessionReader {
    pub fn open_existing(session_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_database(session_dir.as_ref().join("session.db"))
    }

    pub fn open_database(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            db: SessionDb::open_read_only(path)?,
        })
    }

    pub fn path(&self) -> &Path {
        self.db.path()
    }

    pub fn quick_check(&self) -> Result<()> {
        self.db.quick_check()
    }

    pub fn schema_version(&self) -> Result<i32> {
        self.db.schema_version()
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.db.meta(key)
    }

    pub fn writer_owner(&self) -> Result<Option<WriterOwner>> {
        self.db.writer_owner()
    }

    pub fn session_state(&self) -> Result<Option<SessionState>> {
        self.db.session_state()
    }

    pub fn session_meta(&self) -> Result<Option<SessionMeta>> {
        self.db.session_meta()
    }

    pub fn object(&self, hash: &str) -> Result<Option<StoredObject>> {
        self.db.object(hash)
    }

    pub fn object_meta(&self, hash: &str) -> Result<Option<ObjectMeta>> {
        self.db.object_meta(hash)
    }

    pub fn object_bytes(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        self.db.object_bytes(hash)
    }

    pub fn export_history_jsonl(&self, out: impl Write) -> Result<()> {
        self.db.export_history_jsonl(out)
    }

    pub fn export_requests_jsonl(&self, out: impl Write) -> Result<()> {
        self.db.export_requests_jsonl(out)
    }

    pub fn query_request_attempts(
        &self,
        query: &RequestAuditQuery,
    ) -> Result<Vec<RequestAuditSummary>> {
        self.db.query_request_attempts(query)
    }

    pub fn request_audit_stats(&self) -> Result<RequestAuditStats> {
        self.db.request_audit_stats()
    }

    pub fn request_payloads(
        &self,
        request_attempt_id: i64,
    ) -> Result<Option<RequestAuditPayloads>> {
        self.db.request_payloads(request_attempt_id)
    }

    pub fn load_full_session_snapshot(&self) -> Result<Option<SessionSnapshot>> {
        self.db.load_full_session_snapshot()
    }

    pub fn transcript_descriptor_count(&self) -> Result<usize> {
        self.db.transcript_descriptor_count()
    }

    pub fn transcript_descriptor_dense_extent(&self) -> Result<usize> {
        self.db.transcript_descriptor_dense_extent()
    }

    pub fn transcript_descriptor_index_for_block_idx(
        &self,
        block_idx: u64,
    ) -> Result<Option<TranscriptDescriptorIndex>> {
        self.db.transcript_descriptor_index_for_block_idx(block_idx)
    }

    pub fn transcript_descriptor_estimated_rows(
        &self,
        range: TranscriptDescriptorRange,
        width: u16,
    ) -> Result<u64> {
        self.db.transcript_descriptor_estimated_rows(range, width)
    }

    pub fn read_all_transcript_descriptor_records(
        &self,
    ) -> Result<Vec<TranscriptDescriptorRecord>> {
        self.db.read_all_transcript_descriptor_records()
    }

    pub fn read_transcript_descriptor_slice(
        &self,
        range: TranscriptDescriptorRange,
    ) -> Result<TranscriptDescriptorSlice> {
        self.db.read_transcript_descriptor_slice(range)
    }

    pub fn read_transcript_descriptor_slice_with_total(
        &self,
        range: TranscriptDescriptorRange,
        total_count: usize,
    ) -> Result<TranscriptDescriptorSlice> {
        self.db
            .read_transcript_descriptor_slice_with_total(range, total_count)
    }

    pub fn read_transcript_descriptor_tail_slice(
        &self,
        count: usize,
    ) -> Result<TranscriptDescriptorSlice> {
        self.db.read_transcript_descriptor_tail_slice(count)
    }

    pub fn read_transcript_descriptor_tail_slice_with_total(
        &self,
        total_count: usize,
        count: usize,
    ) -> Result<TranscriptDescriptorSlice> {
        self.db
            .read_transcript_descriptor_tail_slice_with_total(total_count, count)
    }

    pub fn read_transcript_descriptor_centered_slice(
        &self,
        center_descriptor_idx: u64,
        before: usize,
        after: usize,
    ) -> Result<TranscriptDescriptorSlice> {
        self.db
            .read_transcript_descriptor_centered_slice(center_descriptor_idx, before, after)
    }

    pub fn read_transcript_descriptor_before_kind_at_index(
        &self,
        kind: &str,
        before_or_at_descriptor_index: u64,
    ) -> Result<Option<TranscriptDescriptorRecord>> {
        self.db
            .read_transcript_descriptor_before_kind_at_index(kind, before_or_at_descriptor_index)
    }

    pub fn read_transcript_descriptor_after_kind_at_index(
        &self,
        kind: &str,
        after_or_at_descriptor_index: u64,
    ) -> Result<Option<TranscriptDescriptorRecord>> {
        self.db
            .read_transcript_descriptor_after_kind_at_index(kind, after_or_at_descriptor_index)
    }

    pub fn search_transcript_candidates(
        &self,
        query: &str,
    ) -> Result<Vec<TranscriptSearchCandidate>> {
        self.db.search_transcript_candidates(query)
    }

    pub fn search_transcript_candidate_page(
        &self,
        query: &str,
        origin_block_idx: Option<u64>,
        direction: TranscriptSearchDirection,
        limit: usize,
    ) -> Result<Vec<TranscriptSearchCandidate>> {
        self.db
            .search_transcript_candidate_page(query, origin_block_idx, direction, limit)
    }

    pub fn read_history_items_range(
        &self,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<protocol::HistoryItem>> {
        self.db.read_history_items_range(range)
    }

    pub fn history_item_count(&self) -> Result<usize> {
        self.db.history_item_count()
    }

    pub fn transcript_block_count(&self) -> Result<usize> {
        self.db.transcript_block_count()
    }

    pub fn transcript_missing_descriptor_count(&self) -> Result<usize> {
        self.db.transcript_missing_descriptor_count()
    }

    pub fn transcript_descriptor_max_history_idx(&self) -> Result<Option<usize>> {
        self.db.transcript_descriptor_max_history_idx()
    }

    pub fn transcript_max_block_idx(&self) -> Result<Option<u64>> {
        self.db.transcript_max_block_idx()
    }

    pub fn read_transcript_block_metadata_range(
        &self,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<TranscriptBlockMetadataRecord>> {
        self.db.read_transcript_block_metadata_range(range)
    }

    pub fn read_transcript_block_metadata_tail(
        &self,
        count: usize,
    ) -> Result<Vec<TranscriptBlockMetadataRecord>> {
        self.db.read_transcript_block_metadata_tail(count)
    }

    pub fn history_text_bytes(&self) -> Result<u64> {
        self.db.history_text_bytes()
    }

    pub fn search_blob(&self) -> Result<String> {
        self.db.search_blob()
    }
}

#[derive(Debug)]
pub struct SessionCommitOutcome {
    pub receipt: SaveReceipt,
    pub deferred_blob_error: Option<StoreError>,
}

#[derive(Debug)]
pub enum SessionWriteFailure {
    Stage(StoreError),
    Commit(SessionCommitFailure),
}

#[derive(Debug)]
pub struct OwnedSessionWriter {
    session_id: String,
    session_dir: PathBuf,
    db: SessionDb,
    owner: WriterOwner,
    token: Option<String>,
    _lock: SessionLock,
}

impl OwnedSessionWriter {
    pub fn open(session_dir: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        let session_dir = session_dir.as_ref().to_path_buf();
        let session_id = session_id.into();
        let lock = SessionLock::acquire(&session_dir)?;
        let db = SessionDb::open(session_dir.join("session.db"))?;
        let token = random_token()?;
        let owner = current_writer_owner();
        db.claim_writer_owner(&token, &owner)?;
        let recovery = db
            .last_session_commit_fingerprint()
            .and_then(|fingerprint| recover_blob_staging(&session_dir, fingerprint.as_deref()));
        if let Err(err) = recovery {
            let _ = db.release_writer_owner(&token);
            return Err(err);
        }
        Ok(Self {
            session_id,
            session_dir,
            db,
            owner,
            token: Some(token),
            _lock: lock,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn owner(&self) -> &WriterOwner {
        &self.owner
    }

    pub fn path(&self) -> &Path {
        self.db.path()
    }

    pub fn session_state(&self) -> Result<Option<SessionState>> {
        self.db.session_state()
    }

    pub fn transcript_descriptor_count(&self) -> Result<usize> {
        self.db.transcript_descriptor_count()
    }

    pub fn commit_session(
        &self,
        command: &SessionCommit,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        self.recover_staged_blobs()
            .map_err(session_commit_failure_from_blob_error)?;
        self.commit_canonical(command)
    }

    pub fn commit_session_with_blobs(
        &self,
        command: &SessionCommit,
        blobs: &[SessionBlob],
    ) -> std::result::Result<SessionCommitOutcome, SessionWriteFailure> {
        self.recover_staged_blobs()
            .map_err(SessionWriteFailure::Stage)?;
        let fingerprint =
            crate::db::session_commit_fingerprint(command).map_err(SessionWriteFailure::Stage)?;
        let staging_token = random_token().map_err(SessionWriteFailure::Stage)?;
        let staged = stage_session_blobs(&self.session_dir, &fingerprint, &staging_token, blobs)
            .map_err(SessionWriteFailure::Stage)?;
        let receipt = match self.commit_canonical(command) {
            Ok(receipt) => receipt,
            Err(err) => {
                if let Some(staged) = staged {
                    staged.abandon();
                }
                return Err(SessionWriteFailure::Commit(err));
            }
        };
        let deferred_blob_error = staged.and_then(|staged| staged.publish().err());
        Ok(SessionCommitOutcome {
            receipt,
            deferred_blob_error,
        })
    }

    fn commit_canonical(
        &self,
        command: &SessionCommit,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        if command.session_id != self.session_id || command.state.id != self.session_id {
            return Err(SessionCommitFailure::SessionMismatch {
                expected: self.session_id.clone(),
                actual: Some(command.state.id.clone()),
            });
        }
        self.db.commit_session_owned(self.token(), command)
    }

    fn recover_staged_blobs(&self) -> Result<()> {
        let fingerprint = self.db.last_session_commit_fingerprint()?;
        recover_blob_staging(&self.session_dir, fingerprint.as_deref())
    }

    pub fn append_request_attempt(
        &self,
        entry: &protocol::request_log::RequestLogEntry,
        payload_mode: RequestAuditPayloadMode,
    ) -> Result<i64> {
        self.db
            .append_request_attempt_owned(self.token(), entry, payload_mode)
    }

    pub fn release(mut self) -> Result<()> {
        let token = self.token.as_deref().expect("owned writer token");
        self.db.release_writer_owner(token)?;
        self.token = None;
        Ok(())
    }

    fn token(&self) -> &str {
        self.token.as_deref().expect("owned writer token")
    }
}

impl Drop for OwnedSessionWriter {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            let _ = self.db.release_writer_owner(&token);
        }
    }
}

#[derive(Debug)]
pub struct SessionMaintenance {
    writer: OwnedSessionWriter,
}

impl SessionMaintenance {
    pub fn delete_session(session_dir: impl AsRef<Path>) -> Result<()> {
        let session_dir = session_dir.as_ref();
        let session_name = session_dir
            .file_name()
            .ok_or_else(|| StoreError::Integrity("session directory has no name".into()))?;
        let lock = SessionLock::acquire(session_dir)?;
        let root = session_dir
            .parent()
            .ok_or_else(|| StoreError::Integrity("session directory has no parent".into()))?;
        let trash = root.join(".trash");
        ensure_private_directory(&trash)?;
        let tombstone = trash.join(format!(
            "{}-{}",
            session_name.to_string_lossy(),
            random_token()?
        ));
        fs::rename(session_dir, &tombstone)?;
        sync_directory(root)?;
        drop(lock);
        fs::remove_dir_all(&tombstone)?;
        sync_directory(&trash)?;
        let _ = fs::remove_dir(&trash);
        sync_directory(root)?;
        Ok(())
    }

    pub fn open(session_dir: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Ok(Self {
            writer: OwnedSessionWriter::open(session_dir, session_id)?,
        })
    }

    pub fn session_id(&self) -> &str {
        self.writer.session_id()
    }

    pub fn import_snapshot(&self, snapshot: &SessionSnapshot) -> Result<SessionSaveReport> {
        if snapshot.state.id != self.writer.session_id {
            return Err(StoreError::Integrity(format!(
                "import session id mismatch: expected {}, got {}",
                self.writer.session_id, snapshot.state.id
            )));
        }
        self.writer
            .db
            .save_session_snapshot_for_import_owned(self.writer.token(), snapshot)
    }

    pub fn repair_transcript_history_links(&self) -> Result<usize> {
        self.writer
            .db
            .repair_mismatched_transcript_descriptor_history_links_owned(self.writer.token())
    }

    pub fn repair_checkpoint(&self) -> Result<usize> {
        self.writer
            .db
            .repair_checkpoint_first_live_index_past_history_owned(self.writer.token())
    }

    pub fn replace_transcript_descriptors(
        &self,
        records: &[TranscriptDescriptorRecord],
    ) -> Result<()> {
        self.writer
            .db
            .replace_transcript_descriptor_records_owned(self.writer.token(), records)
    }

    pub fn replace_transcript_descriptor_suffix(
        &self,
        start_descriptor_idx: usize,
        records: &[TranscriptDescriptorRecord],
    ) -> Result<()> {
        self.writer.db.replace_transcript_descriptor_suffix_owned(
            self.writer.token(),
            start_descriptor_idx,
            records,
        )
    }

    pub fn copy_prefix_from(
        &self,
        source: &SessionReader,
        state: &SessionState,
        history_len: usize,
    ) -> Result<()> {
        if state.id != self.writer.session_id {
            return Err(StoreError::Integrity(format!(
                "fork session id mismatch: expected {}, got {}",
                self.writer.session_id, state.id
            )));
        }
        self.writer
            .db
            .copy_prefix_from(&source.db, state, history_len, Some(self.writer.token()))
    }

    pub fn release(self) -> Result<()> {
        self.writer.release()
    }
}

fn session_commit_failure_from_blob_error(err: StoreError) -> SessionCommitFailure {
    SessionCommitFailure::Integrity {
        message: format!("prepare staged session blobs: {err}"),
    }
}

#[derive(Debug)]
struct SessionLock {
    _file: File,
}

impl SessionLock {
    fn acquire(session_dir: &Path) -> Result<Self> {
        match fs::symlink_metadata(session_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing non-directory session path {}",
                        session_dir.display()
                    ),
                )));
            }
            Ok(_) => {}
            Err(err) => return Err(err.into()),
        }
        let path = session_dir.join("session.lock");
        reject_symlink(&path)?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        if let Err(err) = file.try_lock_exclusive() {
            if err.kind() == std::io::ErrorKind::WouldBlock {
                let owner = SessionReader::open_existing(session_dir)
                    .ok()
                    .and_then(|reader| reader.writer_owner().ok().flatten())
                    .map(|owner| owner.summary());
                return Err(StoreError::OwnershipConflict { owner });
            }
            return Err(StoreError::Io(err));
        }
        Ok(Self { _file: file })
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing non-directory storage path {}", path.display()),
            )));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)?,
        Err(err) => return Err(err.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing symlinked storage path {}", path.display()),
            )))
        }
        Ok(metadata) if !metadata.is_file() => Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing non-file storage path {}", path.display()),
        ))),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|err| StoreError::Io(std::io::Error::other(err.to_string())))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn current_writer_owner() -> WriterOwner {
    WriterOwner {
        hostname: local_hostname(),
        pid: std::process::id(),
        process_start_id: process_start_id(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        claimed_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
    }
}

fn local_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn process_start_id() -> String {
    static PROCESS_START_ID: OnceLock<String> = OnceLock::new();
    PROCESS_START_ID
        .get_or_init(|| {
            #[cfg(target_os = "linux")]
            if let Ok(stat) = fs::read_to_string("/proc/self/stat") {
                if let Some(fields) = stat.rsplit_once(')').map(|(_, fields)| fields) {
                    if let Some(start_ticks) = fields.split_whitespace().nth(19) {
                        return start_ticks.to_string();
                    }
                }
            }
            let started_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            format!("{}:{started_at}", std::process::id())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_commit(session_id: &str, save_id: u64, base_revision: u64) -> SessionCommit {
        SessionCommit {
            session_id: session_id.into(),
            save_id: crate::SaveId::new(save_id),
            base_revision: crate::Revision::new(base_revision),
            base_history_len: crate::HistoryLen::ZERO,
            base_descriptor_len: crate::DescriptorLen::ZERO,
            state: SessionState {
                id: session_id.into(),
                title: None,
                slug: None,
                first_user_message: None,
                cwd: None,
                mode: None,
                reasoning_effort: None,
                model: None,
                parent_id: None,
                accounting_json: None,
                checkpoint_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: 0.0,
                revision: base_revision,
                history_len: 0,
                created_at: 1,
                updated_at: 1,
            },
            history: crate::HistorySuffix {
                start: crate::HistoryIndex::ZERO,
                final_len: crate::HistoryLen::ZERO,
                items: Vec::new(),
            },
            side_tables: crate::SideTableSuffixes::default(),
            descriptors: None,
        }
    }

    #[test]
    fn clean_release_clears_owner_and_allows_reacquisition() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("session");
        fs::create_dir(&session_dir).unwrap();
        let writer = OwnedSessionWriter::open(&session_dir, "session").unwrap();
        let first_token = writer.token().to_string();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(session_dir.join("session.lock"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let first_owner = SessionReader::open_existing(&session_dir)
            .unwrap()
            .writer_owner()
            .unwrap()
            .expect("owner metadata");

        writer.release().unwrap();
        assert!(SessionReader::open_existing(&session_dir)
            .unwrap()
            .writer_owner()
            .unwrap()
            .is_none());

        let replacement = OwnedSessionWriter::open(&session_dir, "session").unwrap();
        assert_ne!(replacement.token(), first_token);
        assert_ne!(replacement.owner().claimed_at, 0);
        assert_eq!(
            replacement.owner().process_start_id,
            first_owner.process_start_id
        );
    }

    #[test]
    fn maintenance_delete_requires_ownership_and_removes_tombstone() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("session");
        fs::create_dir(&session_dir).unwrap();
        let writer = OwnedSessionWriter::open(&session_dir, "session").unwrap();

        assert!(matches!(
            SessionMaintenance::delete_session(&session_dir),
            Err(StoreError::OwnershipConflict { .. })
        ));
        writer.release().unwrap();

        SessionMaintenance::delete_session(&session_dir).unwrap();
        assert!(!session_dir.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn opening_writer_cleans_abandoned_blob_staging() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("session");
        fs::create_dir(&session_dir).unwrap();
        let writer = OwnedSessionWriter::open(&session_dir, "session").unwrap();
        let blobs = vec![SessionBlob {
            filename: "attachment.png".into(),
            bytes: b"attachment".to_vec(),
        }];
        let staged = stage_session_blobs(&session_dir, &"a".repeat(64), &"b".repeat(64), &blobs)
            .unwrap()
            .unwrap();
        assert!(staged.path().join("attachment.png").is_file());
        drop(staged);
        writer.release().unwrap();

        let replacement = OwnedSessionWriter::open(&session_dir, "session").unwrap();
        assert_eq!(
            fs::read_dir(session_dir.join(BLOB_STAGING_DIR))
                .unwrap()
                .count(),
            0
        );
        drop(replacement);
    }

    #[test]
    fn failed_commit_discards_staged_blobs() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("session");
        fs::create_dir(&session_dir).unwrap();
        let writer = OwnedSessionWriter::open(&session_dir, "session").unwrap();
        let command = empty_commit("session", 1, 9);
        let blobs = vec![SessionBlob {
            filename: "attachment.png".into(),
            bytes: b"attachment".to_vec(),
        }];

        assert!(matches!(
            writer.commit_session_with_blobs(&command, &blobs),
            Err(SessionWriteFailure::Commit(
                SessionCommitFailure::StaleRevision { .. }
            ))
        ));
        assert!(!session_dir.join("blobs/attachment.png").exists());
        assert_eq!(
            fs::read_dir(session_dir.join(BLOB_STAGING_DIR))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn committed_blob_staging_recovers_after_deferred_publication() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("session");
        fs::create_dir(&session_dir).unwrap();
        fs::write(session_dir.join("blobs"), b"publication blocker").unwrap();
        let writer = OwnedSessionWriter::open(&session_dir, "session").unwrap();
        let command = empty_commit("session", 1, 0);
        let blobs = vec![SessionBlob {
            filename: "attachment.png".into(),
            bytes: b"attachment".to_vec(),
        }];

        let outcome = writer.commit_session_with_blobs(&command, &blobs).unwrap();
        assert!(outcome.deferred_blob_error.is_some());
        assert_eq!(outcome.receipt.revision, crate::Revision::new(1));
        assert_eq!(
            fs::read_dir(session_dir.join(BLOB_STAGING_DIR))
                .unwrap()
                .count(),
            1
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let staging_root = session_dir.join(BLOB_STAGING_DIR);
            let staging_dir = fs::read_dir(&staging_root)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            assert_eq!(
                fs::metadata(staging_root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&staging_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(staging_dir.join("attachment.png"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        writer.release().unwrap();

        fs::remove_file(session_dir.join("blobs")).unwrap();
        let replacement = OwnedSessionWriter::open(&session_dir, "session").unwrap();
        assert_eq!(
            fs::read(session_dir.join("blobs/attachment.png")).unwrap(),
            b"attachment"
        );
        assert_eq!(
            fs::read_dir(session_dir.join(BLOB_STAGING_DIR))
                .unwrap()
                .count(),
            0
        );
        drop(replacement);
    }

    #[test]
    fn staged_blobs_reject_paths_outside_the_blob_directory() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("session");
        fs::create_dir(&session_dir).unwrap();
        let writer = OwnedSessionWriter::open(&session_dir, "session").unwrap();
        let command = empty_commit("session", 1, 0);
        let blobs = vec![SessionBlob {
            filename: "../escape".into(),
            bytes: b"attachment".to_vec(),
        }];

        assert!(matches!(
            writer.commit_session_with_blobs(&command, &blobs),
            Err(SessionWriteFailure::Stage(StoreError::Integrity(_)))
        ));
        assert!(!root.path().join("escape").exists());
        assert!(writer.session_state().unwrap().is_none());
    }

    #[test]
    fn stale_writer_token_cannot_commit_or_append_request_audit() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("session");
        fs::create_dir(&session_dir).unwrap();
        let writer = OwnedSessionWriter::open(&session_dir, "session").unwrap();
        writer
            .db
            .set_meta(
                "writer_owner",
                &serde_json::json!({
                    "token": "replacement-token",
                    "owner": writer.owner(),
                })
                .to_string(),
            )
            .unwrap();
        let entry = protocol::request_log::RequestLogEntry {
            request_id: 1,
            kind: "test".into(),
            turn_id: None,
            ask_id: None,
            history_len: None,
            timestamp_ms: 1,
            provider_kind: "test".into(),
            api_base: "https://example.test".into(),
            model: "test".into(),
            url: "https://example.test".into(),
            http_status: None,
            body: serde_json::Value::Null,
            prompt_cache_key: None,
            stream: false,
            system_prompt: None,
            messages: None,
            tools: None,
            response: None,
            usage: None,
            cost_usd: None,
            tokens_per_sec: None,
            elapsed_ms: None,
            attempt: 1,
            error: None,
            background: false,
        };

        assert!(matches!(
            writer.append_request_attempt(&entry, RequestAuditPayloadMode::Summary),
            Err(StoreError::OwnershipLost)
        ));

        let command = empty_commit("session", 1, 0);
        assert_eq!(
            writer.commit_session(&command),
            Err(SessionCommitFailure::OwnershipLost)
        );
    }
}
