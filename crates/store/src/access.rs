use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::blob_staging::recover_blob_staging;
#[cfg(test)]
use crate::blob_staging::{stage_session_blobs, SessionBlob, BLOB_STAGING_DIR};
use crate::db::{validate_session_identity_connection, SessionDb};
use crate::{
    FullSession, HistoryIndex, HistoryLen, HistorySuffix, ObjectMeta, RequestAuditPayloadMode,
    RequestAuditPayloads, RequestAuditQuery, RequestAuditStats, RequestAuditSummary, Result,
    SaveReceipt, SessionCommit, SessionCommitFailure, SessionIdentity, SessionMeta,
    SessionMetadata, SideTableSuffixes, StartupRecoveryReceipt, StoreError, StoreHead,
    StoredObject, StoredSession, StoredTranscriptBlock, StoredTurn, SubmitTurn, SubmitTurnReceipt,
    TranscriptBlockMetadataRecord, TranscriptExtentChunk, TranscriptRecordIndex,
    TranscriptRecordOffset, TranscriptRecordRange, TranscriptRecordSlice, TranscriptRecordSuffix,
    TranscriptSearchCandidate, TranscriptSearchDirection, TurnId, TurnTransition,
    TurnTransitionReceipt, WriterOwner,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyAttachmentBlob {
    pub filename: String,
    pub data_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionSchemaStatus {
    Current { version: i32 },
    Upgradeable { found: i32, target: i32 },
    Future { found: i32, supported: i32 },
    Unrecognized { found: i32, supported: i32 },
    Orphaned { found: i32 },
    Corrupt { found: i32, reason: String },
}

impl SessionSchemaStatus {
    fn from_version(version: i32) -> Self {
        match version {
            crate::schema::SCHEMA_VERSION => Self::Current { version },
            1..crate::schema::SCHEMA_VERSION => Self::Upgradeable {
                found: version,
                target: crate::schema::SCHEMA_VERSION,
            },
            version if version > crate::schema::SCHEMA_VERSION => Self::Future {
                found: version,
                supported: crate::schema::SCHEMA_VERSION,
            },
            found => Self::Unrecognized {
                found,
                supported: crate::schema::SCHEMA_VERSION,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct SessionSchemaMigration {
    pub session_id: String,
    pub from_version: i32,
    pub to_version: i32,
    pub migrated: bool,
    pub store_head: StoreHead,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionOrphanQuarantine {
    pub session_id: String,
    pub schema_version: i32,
    pub quarantine_path: PathBuf,
}

#[derive(Debug)]
pub struct SessionReader {
    db: SessionDb,
}

#[derive(Debug)]
pub struct SessionSchemaInspection {
    status: SessionSchemaStatus,
    reader: Option<SessionReader>,
}

impl SessionSchemaInspection {
    pub fn into_parts(self) -> (SessionSchemaStatus, Option<SessionReader>) {
        (self.status, self.reader)
    }
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

    fn from_read_only_connection(
        path: impl AsRef<Path>,
        connection: rusqlite::Connection,
    ) -> Result<Self> {
        Ok(Self {
            db: SessionDb::from_read_only_connection(path, connection)?,
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

    pub fn storage_stats(&self) -> Result<crate::StorageStats> {
        self.db.storage_stats()
    }

    pub fn doctor_report(&self) -> Result<crate::DoctorReport> {
        self.db.doctor_report()
    }

    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<()> {
        self.db.backup_to(destination)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.db.meta(key)
    }

    pub fn writer_owner(&self) -> Result<Option<WriterOwner>> {
        self.db.writer_owner()
    }

    pub fn degraded_warnings(&self) -> Result<Vec<String>> {
        Ok(self
            .db
            .missing_object_references()?
            .into_iter()
            .map(|reference| format!("missing SQLite object {reference}"))
            .collect())
    }

    pub fn stored_session(&self) -> Result<Option<StoredSession>> {
        self.db.stored_session()
    }

    pub fn store_head(&self) -> Result<StoreHead> {
        self.db.store_head()
    }

    pub fn turn(&self, turn_id: TurnId) -> Result<Option<StoredTurn>> {
        self.db.turn(turn_id)
    }

    pub fn turns(&self) -> Result<Vec<StoredTurn>> {
        self.db.turns()
    }

    pub fn latest_terminal_turn_id(&self) -> Result<Option<TurnId>> {
        self.db.latest_terminal_turn_id()
    }

    pub fn load_session_resume_snapshot(
        &self,
        record_width: u16,
        record_target_rows: u16,
    ) -> Result<Option<crate::SessionResumeSnapshot>> {
        self.db
            .load_session_resume_snapshot(record_width, record_target_rows)
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

    pub fn load_full_session(&self) -> Result<Option<FullSession>> {
        let mut session = self.db.load_full_session()?;
        if let Some(session) = &mut session {
            let _ = self.hydrate_legacy_attachments(&mut session.history)?;
        }
        Ok(session)
    }

    pub fn transcript_record_count(&self) -> Result<usize> {
        self.db.transcript_record_count()
    }

    pub fn transcript_record_dense_extent(&self) -> Result<usize> {
        self.db.transcript_record_dense_extent()
    }

    pub fn transcript_record_index_for_block_idx(
        &self,
        block_idx: u64,
    ) -> Result<Option<TranscriptRecordOffset>> {
        self.db.transcript_record_index_for_block_idx(block_idx)
    }

    pub fn transcript_record_estimated_rows(
        &self,
        range: TranscriptRecordRange,
        width: u16,
    ) -> Result<u64> {
        self.db.transcript_record_estimated_rows(range, width)
    }

    pub fn transcript_extent_chunks(&self) -> Result<Vec<TranscriptExtentChunk>> {
        self.db.transcript_extent_chunks()
    }

    pub fn read_all_transcript_records(&self) -> Result<Vec<StoredTranscriptBlock>> {
        self.db.read_all_transcript_records()
    }

    pub fn read_transcript_record_slice(
        &self,
        range: TranscriptRecordRange,
    ) -> Result<TranscriptRecordSlice> {
        self.db.read_transcript_record_slice(range)
    }

    pub fn read_transcript_record_slice_with_total(
        &self,
        range: TranscriptRecordRange,
        total_count: usize,
    ) -> Result<TranscriptRecordSlice> {
        self.db
            .read_transcript_record_slice_with_total(range, total_count)
    }

    pub fn read_transcript_record_tail_slice(&self, count: usize) -> Result<TranscriptRecordSlice> {
        self.db.read_transcript_record_tail_slice(count)
    }

    pub fn read_transcript_record_tail_slice_with_total(
        &self,
        total_count: usize,
        count: usize,
    ) -> Result<TranscriptRecordSlice> {
        self.db
            .read_transcript_record_tail_slice_with_total(total_count, count)
    }

    pub fn read_transcript_record_tail_for_rows(
        &self,
        width: u16,
        target_rows: u16,
    ) -> Result<TranscriptRecordSlice> {
        self.db
            .read_transcript_record_tail_for_rows(width, target_rows)
    }

    pub fn read_transcript_record_centered_slice(
        &self,
        center_record_idx: u64,
        before: usize,
        after: usize,
    ) -> Result<TranscriptRecordSlice> {
        self.db
            .read_transcript_record_centered_slice(center_record_idx, before, after)
    }

    pub fn read_transcript_record_before_kind_at_index(
        &self,
        kind: &str,
        before_or_at_record_index: u64,
    ) -> Result<Option<StoredTranscriptBlock>> {
        self.db
            .read_transcript_record_before_kind_at_index(kind, before_or_at_record_index)
    }

    pub fn read_transcript_record_after_kind_at_index(
        &self,
        kind: &str,
        after_or_at_record_index: u64,
    ) -> Result<Option<StoredTranscriptBlock>> {
        self.db
            .read_transcript_record_after_kind_at_index(kind, after_or_at_record_index)
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
        let mut items = self.db.read_history_items_range(range)?;
        let _ = self.hydrate_legacy_attachments(&mut items)?;
        Ok(items)
    }

    pub fn read_history_items_tail(
        &self,
        end: usize,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<Vec<protocol::HistoryItem>> {
        let mut items = self.db.read_history_items_tail(end, max_items, max_bytes)?;
        let hydrated_legacy = self.hydrate_legacy_attachments(&mut items)?;
        if let (true, Some(max_bytes)) = (hydrated_legacy, max_bytes) {
            trim_history_tail_to_bytes(&mut items, max_bytes)?;
        }
        Ok(items)
    }

    pub fn legacy_attachment_references(&self, history_end: usize) -> Result<Vec<String>> {
        self.db.legacy_attachment_references(history_end)
    }

    pub fn history_any_transcript_visible_before(&self, end: usize) -> Result<bool> {
        self.db.history_any_transcript_visible_before(end)
    }

    pub fn history_note_projection_at(
        &self,
        index: usize,
    ) -> Result<Option<protocol::HistoryNoteProjection>> {
        self.db.history_note_projection_at(index)
    }

    pub fn history_last_context_note_index_before(
        &self,
        end: usize,
        name: &str,
    ) -> Result<Option<usize>> {
        self.db.history_last_context_note_index_before(end, name)
    }

    pub fn history_mode_before(&self, end: usize) -> Result<Option<String>> {
        self.db.history_mode_before(end)
    }

    pub fn history_base_mode_range(&self, range: std::ops::Range<usize>) -> Result<Option<String>> {
        self.db.history_base_mode_range(range)
    }

    pub fn legacy_attachment_blob(&self, reference: &str) -> Result<LegacyAttachmentBlob> {
        let session_dir = self
            .db
            .path()
            .parent()
            .ok_or_else(|| StoreError::Integrity("session database has no parent".into()))?;
        read_legacy_attachment(session_dir, reference)
    }

    pub fn history_item_count(&self) -> Result<usize> {
        self.db.history_item_count()
    }

    pub fn transcript_block_count(&self) -> Result<usize> {
        self.db.transcript_block_count()
    }

    pub fn transcript_missing_block_count(&self) -> Result<usize> {
        self.db.transcript_missing_block_count()
    }

    pub fn transcript_record_max_history_idx(&self) -> Result<Option<usize>> {
        self.db.transcript_record_max_history_idx()
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

    pub fn write_search_blob(&self, writer: &mut impl std::io::Write) -> Result<()> {
        self.db.write_search_blob(writer)
    }

    fn hydrate_legacy_attachments(&self, items: &mut [protocol::HistoryItem]) -> Result<bool> {
        let session_dir = self
            .db
            .path()
            .parent()
            .ok_or_else(|| StoreError::Integrity("session database has no parent".into()))?;
        let mut hydrated = false;
        for item in items {
            if !history_item_has_legacy_attachment(item) {
                continue;
            }
            let mut value = serde_json::to_value(&*item)?;
            hydrate_legacy_attachment_value(session_dir, &mut value)?;
            *item = serde_json::from_value(value)?;
            hydrated = true;
        }
        Ok(hydrated)
    }
}

fn trim_history_tail_to_bytes(
    items: &mut Vec<protocol::HistoryItem>,
    max_bytes: usize,
) -> Result<()> {
    let mut start = items.len();
    let mut budget = protocol::HistoryTailBudget::new(items.len(), Some(max_bytes));
    for (index, item) in items.iter().enumerate().rev() {
        if !budget.try_prepend(item)? {
            break;
        }
        start = index;
    }
    if start > 0 {
        *items = items.split_off(start);
    }
    Ok(())
}

// COMPAT(legacy-attachment-blobs): hydrate pre-object-store image references from
// private external blob files until those sessions have been explicitly migrated.
fn history_item_has_legacy_attachment(item: &protocol::HistoryItem) -> bool {
    fn content_has_legacy_attachment(content: &protocol::Content) -> bool {
        match content {
            protocol::Content::Text(_) => false,
            protocol::Content::Parts(parts) => parts.iter().any(|part| {
                matches!(
                    part,
                    protocol::ContentPart::ImageUrl { url, .. } if url.starts_with("blob:")
                )
            }),
        }
    }

    fn value_has_legacy_attachment(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                let is_legacy_image = map.get("type").and_then(serde_json::Value::as_str)
                    == Some("image_url")
                    && map
                        .get("image_url")
                        .and_then(serde_json::Value::as_object)
                        .and_then(|image| image.get("url"))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|url| url.starts_with("blob:"));
                is_legacy_image || map.values().any(value_has_legacy_attachment)
            }
            serde_json::Value::Array(values) => values.iter().any(value_has_legacy_attachment),
            _ => false,
        }
    }

    match item {
        protocol::HistoryItem::System { content } | protocol::HistoryItem::User { content, .. } => {
            content_has_legacy_attachment(content)
        }
        protocol::HistoryItem::Assistant(step) => {
            step.content
                .as_ref()
                .is_some_and(content_has_legacy_attachment)
                || step
                    .reasoning_blocks
                    .iter()
                    .any(|block| value_has_legacy_attachment(&block.data))
                || step.invocations.iter().any(|invocation| {
                    invocation
                        .result
                        .metadata
                        .as_ref()
                        .is_some_and(value_has_legacy_attachment)
                })
        }
        protocol::HistoryItem::Note(_) => false,
    }
}

fn hydrate_legacy_attachment_value(
    session_dir: &Path,
    value: &mut serde_json::Value,
) -> Result<()> {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("type").and_then(serde_json::Value::as_str) == Some("image_url") {
                let url = map
                    .get_mut("image_url")
                    .and_then(serde_json::Value::as_object_mut)
                    .and_then(|image| image.get_mut("url"));
                if let Some(serde_json::Value::String(reference)) = url {
                    if reference.starts_with("blob:") {
                        *reference = read_legacy_attachment(session_dir, reference)?.data_url;
                    }
                }
            }
            for child in map.values_mut() {
                hydrate_legacy_attachment_value(session_dir, child)?;
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                hydrate_legacy_attachment_value(session_dir, child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn legacy_attachment_path(session_dir: &Path, reference: &str) -> Result<PathBuf> {
    let filename = reference
        .strip_prefix("blob:")
        .filter(|filename| !filename.is_empty())
        .ok_or_else(|| StoreError::Integrity(format!("invalid legacy attachment {reference:?}")))?;
    let path = Path::new(filename);
    if path.components().count() != 1
        || path.file_name().and_then(|name| name.to_str()) != Some(filename)
    {
        return Err(StoreError::Integrity(format!(
            "invalid legacy attachment filename {filename:?}"
        )));
    }
    Ok(session_dir.join("blobs").join(filename))
}

fn read_legacy_attachment(session_dir: &Path, reference: &str) -> Result<LegacyAttachmentBlob> {
    let blob_path = legacy_attachment_path(session_dir, reference)?;
    let filename = blob_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("validated legacy attachment filename");
    let (expected_hash, _) = filename.rsplit_once('.').ok_or_else(|| {
        StoreError::Integrity(format!(
            "legacy attachment filename has no extension: {filename:?}"
        ))
    })?;
    if expected_hash.len() != 64
        || !expected_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(StoreError::Integrity(format!(
            "invalid legacy attachment hash in {filename:?}"
        )));
    }
    let metadata = match fs::symlink_metadata(&blob_path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => {
            return Err(StoreError::Integrity(format!(
                "legacy attachment is not a regular file: {}",
                blob_path.display()
            )));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(StoreError::MissingObject {
                reference: reference.to_string(),
            });
        }
        Err(err) => return Err(err.into()),
    };
    if metadata.len() > crate::object::MAX_OBJECT_RAW_SIZE {
        return Err(StoreError::ObjectTooLarge {
            size: metadata.len(),
            max: crate::object::MAX_OBJECT_RAW_SIZE,
        });
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(&blob_path)?
        .take(crate::object::MAX_OBJECT_RAW_SIZE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > crate::object::MAX_OBJECT_RAW_SIZE {
        return Err(StoreError::ObjectTooLarge {
            size: bytes.len() as u64,
            max: crate::object::MAX_OBJECT_RAW_SIZE,
        });
    }
    let mut hasher = Sha256::new();
    hasher.update(b"image:");
    hasher.update(&bytes);
    let actual_hash = crate::object::hex_lower(&hasher.finalize());
    if actual_hash != expected_hash {
        return Err(StoreError::Integrity(format!(
            "legacy attachment hash mismatch for {filename:?}"
        )));
    }
    let data_url = String::from_utf8(bytes)
        .map_err(|err| StoreError::Integrity(format!("legacy attachment is not UTF-8: {err}")))?;
    if !data_url.starts_with("data:image/") {
        return Err(StoreError::Integrity(format!(
            "legacy attachment is not an image data URL: {filename:?}"
        )));
    }
    Ok(LegacyAttachmentBlob {
        filename: filename.to_string(),
        data_url,
    })
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct SessionCommitOutcome {
    pub receipt: SaveReceipt,
    pub deferred_blob_error: Option<StoreError>,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) enum SessionWriteFailure {
    Stage(StoreError),
    Commit(SessionCommitFailure),
}

const SESSION_ID_LEN: usize = 64;
const LOCKS_DIR: &str = ".locks";
const STAGING_DIR: &str = ".staging";
const TRASH_DIR: &str = ".trash";
const QUARANTINE_DIR: &str = ".quarantine";

#[derive(Clone, Debug)]
struct SessionLayout {
    root: PathBuf,
    session_id: String,
    published: PathBuf,
}

impl SessionLayout {
    fn new(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        let session_id = session_id.into();
        validate_session_id(&session_id)?;
        let root = root.as_ref().to_path_buf();
        let published = root.join(&session_id);
        Ok(Self {
            root,
            session_id,
            published,
        })
    }

    fn published_dir(&self) -> &Path {
        &self.published
    }

    fn lock_dir(&self) -> PathBuf {
        self.root.join(LOCKS_DIR)
    }

    fn lock_path(&self) -> PathBuf {
        self.lock_dir().join(format!("{}.lock", self.session_id))
    }

    fn staging_dir(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }

    fn trash_dir(&self) -> PathBuf {
        self.root.join(TRASH_DIR)
    }
}

#[derive(Debug)]
struct SessionLease {
    layout: SessionLayout,
    token: String,
    owner: WriterOwner,
    _lock: File,
}

impl SessionLease {
    fn acquire(layout: SessionLayout) -> Result<Self> {
        ensure_private_directory_all(&layout.root)?;
        ensure_private_directory(&layout.lock_dir())?;
        let path = layout.lock_path();
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
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                let owner = SessionReader::open_existing(layout.published_dir())
                    .ok()
                    .and_then(|reader| reader.writer_owner().ok().flatten())
                    .map(|owner| owner.summary());
                return Err(StoreError::OwnershipConflict { owner });
            }
            Err(fs::TryLockError::Error(err)) => return Err(StoreError::Io(err)),
        }
        Ok(Self {
            layout,
            token: random_token()?,
            owner: current_writer_owner(),
            _lock: file,
        })
    }
}

pub fn inspect_session_schema(
    root: impl AsRef<Path>,
    session_id: impl Into<String>,
) -> Result<SessionSchemaInspection> {
    let layout = SessionLayout::new(root, session_id)?;
    inspect_session_schema_for_layout(&layout)
}

pub fn session_schema_status(
    root: impl AsRef<Path>,
    session_id: impl Into<String>,
) -> Result<SessionSchemaStatus> {
    Ok(inspect_session_schema(root, session_id)?.status)
}

pub fn migrate_session_schema(
    root: impl AsRef<Path>,
    session_id: impl Into<String>,
) -> Result<SessionSchemaMigration> {
    let lease = SessionLease::acquire(SessionLayout::new(root, session_id)?)?;
    let (status, reader) = inspect_session_schema_for_layout(&lease.layout)?.into_parts();
    match status {
        SessionSchemaStatus::Current { version } => Ok(SessionSchemaMigration {
            session_id: lease.layout.session_id.clone(),
            from_version: version,
            to_version: version,
            migrated: false,
            store_head: reader
                .expect("current schema inspection includes a reader")
                .store_head()?,
        }),
        SessionSchemaStatus::Upgradeable { .. } => {
            // COMPAT(storage-root-lease): older writers may coordinate only through
            // the in-directory lock. Hold it after the root lock through migration.
            let _legacy_lock = LegacySessionLock::acquire(lease.layout.published_dir())?;
            let (status, reader) = inspect_session_schema_for_layout(&lease.layout)?.into_parts();
            let (found, target) = match status {
                SessionSchemaStatus::Current { version } => {
                    return Ok(SessionSchemaMigration {
                        session_id: lease.layout.session_id.clone(),
                        from_version: version,
                        to_version: version,
                        migrated: false,
                        store_head: reader
                            .expect("current schema inspection includes a reader")
                            .store_head()?,
                    });
                }
                SessionSchemaStatus::Upgradeable { found, target } => (found, target),
                status => return Err(schema_status_migration_error(status)),
            };
            let db_path = published_database_path(&lease.layout)?;
            let db = SessionDb::open(&db_path)?;
            validate_stored_identity(&db, &lease.layout.session_id)?;
            let to_version = db.schema_version()?;
            if to_version != target {
                return Err(StoreError::Integrity(format!(
                    "session schema migration stopped at version {to_version}; expected {target}"
                )));
            }
            Ok(SessionSchemaMigration {
                session_id: lease.layout.session_id.clone(),
                from_version: found,
                to_version,
                migrated: true,
                store_head: db.store_head()?,
            })
        }
        status => Err(schema_status_migration_error(status)),
    }
}

fn schema_status_migration_error(status: SessionSchemaStatus) -> StoreError {
    match status {
        SessionSchemaStatus::Future { found, supported } => StoreError::UnsupportedSchema {
            found,
            expected: supported,
        },
        SessionSchemaStatus::Unrecognized { found, supported } => StoreError::Integrity(format!(
            "unrecognized session schema version {found}; supported migrations start at 1 and target {supported}"
        )),
        SessionSchemaStatus::Orphaned { found } => StoreError::OrphanedSession { found },
        SessionSchemaStatus::Corrupt { reason, .. } => StoreError::Integrity(reason),
        SessionSchemaStatus::Current { version } => StoreError::Integrity(format!(
            "session schema changed unexpectedly to current version {version}"
        )),
        SessionSchemaStatus::Upgradeable { found, target } => StoreError::Integrity(format!(
            "session schema remained upgradeable from version {found} to {target}"
        )),
    }
}

pub fn quarantine_orphaned_session(
    root: impl AsRef<Path>,
    session_id: impl Into<String>,
) -> Result<Option<SessionOrphanQuarantine>> {
    let lease = SessionLease::acquire(SessionLayout::new(root, session_id)?)?;
    let Some(observed_version) = orphaned_session_schema_version(&lease.layout)? else {
        return Ok(None);
    };
    // COMPAT(storage-root-lease): an old writer may know only the in-directory
    // lock. Hold it after the root lock while moving an old orphan aside.
    let _legacy_lock = if observed_version < crate::schema::SCHEMA_VERSION {
        Some(LegacySessionLock::acquire(lease.layout.published_dir())?)
    } else {
        None
    };
    let Some(schema_version) = orphaned_session_schema_version(&lease.layout)? else {
        return Ok(None);
    };
    let quarantine_path =
        quarantine_artifact(&lease.layout.root, lease.layout.published_dir(), "orphaned")?;
    Ok(Some(SessionOrphanQuarantine {
        session_id: lease.layout.session_id.clone(),
        schema_version,
        quarantine_path,
    }))
}

#[derive(Debug)]
enum SessionLocation {
    Staged { path: PathBuf },
    Published,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PublicationExpectation {
    identity: SessionIdentity,
    head: StoreHead,
    fingerprint: String,
    receipt: SaveReceipt,
}

#[derive(Debug)]
pub struct OwnedSessionWriter {
    lease: SessionLease,
    location: SessionLocation,
    db: Option<SessionDb>,
    publication: Option<PublicationExpectation>,
    startup_recovery: Option<StartupRecoveryReceipt>,
    owner_active: bool,
}

impl OwnedSessionWriter {
    pub fn open(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Self::open_layout(SessionLayout::new(root, session_id)?, true)
    }

    pub fn open_existing(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Self::open_layout(SessionLayout::new(root, session_id)?, false)
    }

    fn open_layout(layout: SessionLayout, create: bool) -> Result<Self> {
        let lease = SessionLease::acquire(layout)?;
        let published = lease.layout.published_dir();
        match fs::symlink_metadata(published) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                Self::open_published(lease)
            }
            Ok(_) => Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!(
                    "published session path is not a directory: {}",
                    published.display()
                ),
            ))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && create => {
                ensure_destination_available(&lease.layout)?;
                Self::create_staged(lease)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("session does not exist: {}", published.display()),
                )))
            }
            Err(err) => Err(err.into()),
        }
    }

    fn open_published(lease: SessionLease) -> Result<Self> {
        let session_dir = lease.layout.published_dir();
        let db_path = published_database_path(&lease.layout)?;
        let observed_version = database_schema_version(&db_path)?;
        // COMPAT(storage-root-lease): older writers may coordinate only through
        // the in-directory lock. Hold it after the root lock through migration and
        // token claim so an old writer cannot overlap the version boundary.
        let _legacy_lock = if observed_version < crate::schema::SCHEMA_VERSION {
            Some(LegacySessionLock::acquire(session_dir)?)
        } else {
            None
        };
        let version = if _legacy_lock.is_some() {
            database_schema_version(&db_path)?
        } else {
            observed_version
        };
        match SessionSchemaStatus::from_version(version) {
            SessionSchemaStatus::Upgradeable { .. } => {
                validate_database_identity(&db_path, &lease.layout.session_id, version)?;
            }
            SessionSchemaStatus::Unrecognized { found, supported } => {
                return Err(StoreError::Integrity(format!(
                    "unrecognized session schema version {found}; supported migrations start at 1 and target {supported}"
                )));
            }
            SessionSchemaStatus::Current { .. } | SessionSchemaStatus::Future { .. } => {}
            SessionSchemaStatus::Orphaned { .. } | SessionSchemaStatus::Corrupt { .. } => {
                unreachable!("version-only schema classification returned identity status")
            }
        }
        let mut db = SessionDb::open(&db_path)?;
        validate_stored_identity(&db, &lease.layout.session_id)?;
        db.claim_writer_owner(&lease.token, &lease.owner)?;
        if let Err(err) = recover_owned_blob_staging(&db, session_dir) {
            let _ = db.release_writer_owner(&lease.token);
            return Err(err);
        }
        let startup_recovery = match db.interrupt_nonterminal_turns_owned(&lease.token, now_ms()) {
            Ok(receipt) => receipt,
            Err(err) => {
                let _ = db.release_writer_owner(&lease.token);
                return Err(err);
            }
        };
        Ok(Self {
            lease,
            location: SessionLocation::Published,
            db: Some(db),
            publication: None,
            startup_recovery,
            owner_active: true,
        })
    }

    fn create_staged(lease: SessionLease) -> Result<Self> {
        let staging_root = lease.layout.staging_dir();
        ensure_private_directory(&staging_root)?;
        let path = create_staging_directory(&lease.layout, &staging_root)?;
        let open = (|| {
            let mut db = SessionDb::open(path.join("session.db"))?;
            db.claim_writer_owner(&lease.token, &lease.owner)?;
            Ok(db)
        })();
        let db = match open {
            Ok(db) => db,
            Err(err) => {
                let _ = fs::remove_dir_all(&path);
                return Err(err);
            }
        };
        Ok(Self {
            lease,
            location: SessionLocation::Staged { path },
            db: Some(db),
            publication: None,
            startup_recovery: None,
            owner_active: true,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.lease.layout.session_id
    }

    pub fn owner(&self) -> &WriterOwner {
        &self.lease.owner
    }

    pub fn session_dir(&self) -> &Path {
        match &self.location {
            SessionLocation::Staged { path, .. } => path,
            SessionLocation::Published => self.lease.layout.published_dir(),
        }
    }

    pub fn is_staged(&self) -> bool {
        matches!(self.location, SessionLocation::Staged { .. })
    }

    fn db(&self) -> Result<&SessionDb> {
        self.db.as_ref().ok_or_else(|| {
            StoreError::Integrity("session writer SQLite connection is closed".into())
        })
    }

    fn db_mut(&mut self) -> Result<&mut SessionDb> {
        self.db.as_mut().ok_or_else(|| {
            StoreError::Integrity("session writer SQLite connection is closed".into())
        })
    }

    pub fn stored_session(&self) -> Result<Option<StoredSession>> {
        self.db()?.stored_session()
    }

    pub fn store_head(&self) -> Result<StoreHead> {
        self.db()?.store_head()
    }

    pub fn latest_terminal_turn_id(&self) -> Result<Option<TurnId>> {
        self.db()?.latest_terminal_turn_id()
    }

    pub fn startup_recovery(&self) -> Option<&StartupRecoveryReceipt> {
        self.startup_recovery.as_ref()
    }

    pub fn take_startup_recovery(&mut self) -> Option<StartupRecoveryReceipt> {
        self.startup_recovery.take()
    }

    pub fn last_session_commit(&self) -> Result<Option<(String, SaveReceipt)>> {
        self.db()?.last_session_commit()
    }

    pub fn transcript_record_count(&self) -> Result<usize> {
        self.db()?.transcript_record_count()
    }

    pub fn invalidate_connection(&mut self) {
        self.db = None;
    }

    pub fn reopen_connection(&mut self) -> Result<()> {
        if self.db.is_some() {
            smelt_perf::perf::record_value("store:db:cached_read_write", 1);
            return Ok(());
        }
        let db = SessionDb::open_current(self.database_path())?;
        db.verify_writer_owner(&self.lease.token)?;
        if !self.is_staged() || db.stored_session()?.is_some() {
            validate_stored_identity(&db, self.session_id())?;
        }
        if let Some(expected) = &self.publication {
            verify_publication(&db, expected)?;
        }
        recover_owned_blob_staging(&db, self.session_dir())?;
        self.db = Some(db);
        Ok(())
    }

    pub fn publish(&mut self) -> Result<PathBuf> {
        if matches!(self.location, SessionLocation::Published) {
            sync_directory(&self.lease.layout.root)?;
            self.reopen_connection()?;
            self.publication = None;
            return Ok(self.lease.layout.published_dir().to_path_buf());
        }

        self.prepare_publication()?;
        let staged = match &self.location {
            SessionLocation::Staged { path } => path.clone(),
            SessionLocation::Published => unreachable!(),
        };
        let published = self.lease.layout.published_dir().to_path_buf();

        if let Err(rename_err) = rename_without_replacement(&staged, &published) {
            match (path_exists(&staged)?, path_exists(&published)?) {
                (true, false) => return Err(rename_err.into()),
                (false, true) => {}
                (true, true) => {
                    return Err(StoreError::Integrity(format!(
                        "ambiguous session publication left both paths: {} and {}",
                        staged.display(),
                        published.display()
                    )));
                }
                (false, false) => {
                    return Err(StoreError::Integrity(format!(
                        "ambiguous session publication left neither path: {} nor {}",
                        staged.display(),
                        published.display()
                    )));
                }
            }
        }
        self.location = SessionLocation::Published;
        sync_directory(&self.lease.layout.root)?;
        self.reopen_connection()?;
        self.publication = None;
        Ok(published)
    }

    fn prepare_publication(&mut self) -> Result<()> {
        if self.publication.is_none() {
            let db = self.db()?;
            let stored = db.stored_session()?.ok_or_else(|| {
                StoreError::Integrity("cannot publish a session without canonical state".into())
            })?;
            let (fingerprint, receipt) = db.last_session_commit()?.ok_or_else(|| {
                StoreError::Integrity("cannot publish a session without a commit receipt".into())
            })?;
            self.publication = Some(PublicationExpectation {
                identity: stored.identity,
                head: stored.head,
                fingerprint,
                receipt,
            });
        }
        self.close_connection_without_releasing_owner()
    }

    fn close_connection_without_releasing_owner(&mut self) -> Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        record_close_hygiene(db.close_hygiene())?;
        self.db = None;
        Ok(())
    }

    fn database_path(&self) -> PathBuf {
        match &self.location {
            SessionLocation::Staged { path, .. } => path.join("session.db"),
            SessionLocation::Published => self.lease.layout.published_dir().join("session.db"),
        }
    }

    pub fn commit_session(
        &mut self,
        command: &SessionCommit,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        self.recover_staged_blobs()
            .map_err(session_commit_failure_from_blob_error)?;
        self.commit_canonical(command)
    }

    pub fn submit_turn(
        &mut self,
        command: &SubmitTurn,
    ) -> std::result::Result<SubmitTurnReceipt, SessionCommitFailure> {
        self.recover_staged_blobs()
            .map_err(session_commit_failure_from_blob_error)?;
        self.validate_canonical_session(&command.session)?;
        let token = self.lease.token.clone();
        self.db_mut()
            .map_err(session_commit_failure_from_blob_error)?
            .submit_turn_owned(&token, command)
    }

    pub fn recover_submit_turn(
        &mut self,
        command: &SubmitTurn,
    ) -> std::result::Result<Option<SubmitTurnReceipt>, SessionCommitFailure> {
        self.recover_staged_blobs()
            .map_err(session_commit_failure_from_blob_error)?;
        self.validate_canonical_session(&command.session)?;
        let token = self.lease.token.clone();
        self.db()
            .map_err(session_commit_failure_from_blob_error)?
            .recover_submit_turn_owned(&token, command)
    }

    pub fn transition_turn(
        &mut self,
        command: &TurnTransition,
    ) -> std::result::Result<TurnTransitionReceipt, SessionCommitFailure> {
        self.recover_staged_blobs()
            .map_err(session_commit_failure_from_blob_error)?;
        self.validate_canonical_session(&command.session)?;
        let token = self.lease.token.clone();
        self.db_mut()
            .map_err(session_commit_failure_from_blob_error)?
            .transition_turn_owned(&token, command)
    }

    pub fn recover_turn_transition(
        &mut self,
        command: &TurnTransition,
    ) -> std::result::Result<Option<TurnTransitionReceipt>, SessionCommitFailure> {
        self.recover_staged_blobs()
            .map_err(session_commit_failure_from_blob_error)?;
        self.validate_canonical_session(&command.session)?;
        let token = self.lease.token.clone();
        self.db()
            .map_err(session_commit_failure_from_blob_error)?
            .recover_turn_transition_owned(&token, command)
    }

    #[cfg(test)]
    pub(crate) fn commit_session_with_blobs(
        &mut self,
        command: &SessionCommit,
        blobs: &[SessionBlob],
    ) -> std::result::Result<SessionCommitOutcome, SessionWriteFailure> {
        self.recover_staged_blobs()
            .map_err(SessionWriteFailure::Stage)?;
        let fingerprint =
            crate::db::session_commit_fingerprint(command).map_err(SessionWriteFailure::Commit)?;
        let staging_token = random_token().map_err(SessionWriteFailure::Stage)?;
        let staged = stage_session_blobs(self.session_dir(), &fingerprint, &staging_token, blobs)
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

    fn validate_canonical_session(
        &self,
        command: &SessionCommit,
    ) -> std::result::Result<(), SessionCommitFailure> {
        if command.session_id != self.session_id() || command.identity.id != self.session_id() {
            return Err(SessionCommitFailure::SessionMismatch {
                expected: self.session_id().to_string(),
                actual: Some(command.identity.id.clone()),
            });
        }
        Ok(())
    }

    fn commit_canonical(
        &mut self,
        command: &SessionCommit,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        self.validate_canonical_session(command)?;
        let token = self.lease.token.clone();
        self.db_mut()
            .map_err(session_commit_failure_from_blob_error)?
            .apply_session_commit_owned(&token, command)
    }

    // COMPAT(legacy-attachment-blobs): finish or discard external attachment
    // publication staged by pre-object-store writers after a process crash.
    fn recover_staged_blobs(&self) -> Result<()> {
        recover_owned_blob_staging(self.db()?, self.session_dir())
    }

    pub fn append_request_attempt(
        &mut self,
        entry: &protocol::request_log::RequestLogEntry,
        payload_mode: RequestAuditPayloadMode,
    ) -> Result<i64> {
        let token = self.lease.token.clone();
        self.db_mut()?
            .append_request_attempt_owned(&token, entry, payload_mode)
    }

    fn delete(mut self) -> Result<()> {
        if self.is_staged() {
            return Err(StoreError::Integrity(
                "cannot delete an unpublished staged session".into(),
            ));
        }
        self.close_owned_connection()?;
        let source = self.lease.layout.published_dir().to_path_buf();
        let trash = self.lease.layout.trash_dir();
        ensure_private_directory(&trash)?;
        let tombstone = trash.join(format!("{}.{}", self.session_id(), random_token()?));
        if let Err(rename_err) = rename_without_replacement(&source, &tombstone) {
            match (path_exists(&source)?, path_exists(&tombstone)?) {
                (true, false) => return Err(rename_err.into()),
                (false, true) => {}
                (true, true) => {
                    return Err(StoreError::Integrity(format!(
                        "ambiguous session deletion left both paths: {} and {}",
                        source.display(),
                        tombstone.display()
                    )));
                }
                (false, false) => {
                    return Err(StoreError::Integrity(format!(
                        "ambiguous session deletion left neither path: {} nor {}",
                        source.display(),
                        tombstone.display()
                    )));
                }
            }
        }
        sync_directory(&self.lease.layout.root)?;
        fs::remove_dir_all(&tombstone)?;
        sync_directory(&trash)?;
        let _ = fs::remove_dir(&trash);
        sync_directory(&self.lease.layout.root)
    }

    pub fn release(mut self) -> Result<()> {
        let close = self.close_owned_connection();
        let cleanup = self.cleanup_unpublished_stage();
        finish_operation_cleanup("release session writer", close, cleanup)
    }

    fn close_owned_connection(&mut self) -> Result<()> {
        if !self.owner_active {
            return Ok(());
        }
        if self.db.is_none() {
            if let Err(err) = self.reopen_connection() {
                self.owner_active = false;
                return Err(err);
            }
        }
        let mut db = self.db.take().expect("reopened owned database");
        let release = db.release_writer_owner(&self.lease.token);
        self.owner_active = false;
        let hygiene = record_close_hygiene(db.close_hygiene());
        finish_operation_cleanup("close session writer", release, hygiene)
    }

    fn cleanup_unpublished_stage(&mut self) -> Result<()> {
        let SessionLocation::Staged { path } = &self.location else {
            return Ok(());
        };
        if self.publication.is_some() {
            return Ok(());
        }
        match fs::remove_dir_all(path) {
            Ok(()) => sync_directory(&self.lease.layout.staging_dir()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    #[cfg(test)]
    fn token(&self) -> &str {
        &self.lease.token
    }
}

impl Drop for OwnedSessionWriter {
    fn drop(&mut self) {
        let _ = self.close_owned_connection();
        let _ = self.cleanup_unpublished_stage();
    }
}

#[derive(Debug)]
pub struct SessionMaintenance {
    writer: OwnedSessionWriter,
}

impl SessionMaintenance {
    pub fn delete_session(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<()> {
        OwnedSessionWriter::open_existing(root, session_id)?.delete()
    }

    pub fn open(root: impl AsRef<Path>, session_id: impl Into<String>) -> Result<Self> {
        Ok(Self {
            writer: OwnedSessionWriter::open(root, session_id)?,
        })
    }

    pub fn session_dir(&self) -> &Path {
        self.writer.session_dir()
    }

    pub fn publish(&mut self) -> Result<PathBuf> {
        self.writer.publish()
    }

    pub fn session_id(&self) -> &str {
        self.writer.session_id()
    }

    pub fn import_session(&mut self, session: &FullSession) -> Result<SaveReceipt> {
        if session.session.identity.id != self.writer.session_id() {
            return Err(StoreError::Integrity(format!(
                "import session id mismatch: expected {}, got {}",
                self.writer.session_id(),
                session.session.identity.id
            )));
        }
        let expected = self.writer.db()?.store_head()?;
        let command = full_session_commit(
            expected,
            &session.session.identity,
            &session.session.metadata,
            session,
        )?;
        apply_maintenance_commit(&mut self.writer, &command)
    }

    // COMPAT(transcript-record-history-link-mismatch): repair records written by
    // early sparse transcript builds while those sessions remain supported.
    pub fn repair_transcript_history_links(&mut self) -> Result<usize> {
        let mut session = self
            .writer
            .db()?
            .load_full_session()?
            .ok_or_else(|| StoreError::Integrity("session metadata is missing".into()))?;
        let mut repaired = 0;
        for record in &mut session.transcript_records {
            let matches_history = record
                .history_idx
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| session.history.get(index))
                .is_some_and(|item| {
                    protocol::transcript_block_kind_matches_history_item(&record.kind, item)
                });
            if record.history_idx.is_some() && !matches_history {
                record.history_idx = None;
                record.origin_json = None;
                repaired += 1;
            }
        }
        if repaired == 0 {
            return Ok(0);
        }
        let command = metadata_and_record_commit(
            &session,
            session.session.metadata.clone(),
            Some(TranscriptRecordSuffix {
                start: TranscriptRecordIndex::ZERO,
                records: session.transcript_records.clone(),
            }),
        )?;
        apply_maintenance_commit(&mut self.writer, &command)?;
        Ok(repaired)
    }

    pub fn repair_checkpoint(&mut self) -> Result<usize> {
        let Some((stored, metadata)) = self.writer.db()?.repaired_checkpoint_metadata()? else {
            return Ok(0);
        };
        let session = self
            .writer
            .db()?
            .load_full_session()?
            .ok_or_else(|| StoreError::Integrity("session metadata is missing".into()))?;
        let command = full_session_commit(stored.head, &stored.identity, &metadata, &session)?;
        apply_maintenance_commit(&mut self.writer, &command)?;
        Ok(1)
    }

    pub fn replace_transcript_records(&mut self, records: &[StoredTranscriptBlock]) -> Result<()> {
        self.replace_transcript_record_suffix(0, records)
    }

    pub fn replace_transcript_record_suffix(
        &mut self,
        start_record_idx: usize,
        records: &[StoredTranscriptBlock],
    ) -> Result<()> {
        let session = self
            .writer
            .db()?
            .load_full_session()?
            .ok_or_else(|| StoreError::Integrity("session metadata is missing".into()))?;
        let start = TranscriptRecordIndex::try_from(start_record_idx)
            .map_err(|_| StoreError::Integrity("record start exceeds u64".into()))?;
        let command = metadata_and_record_commit(
            &session,
            session.session.metadata.clone(),
            Some(TranscriptRecordSuffix {
                start,
                records: records.to_vec(),
            }),
        )?;
        apply_maintenance_commit(&mut self.writer, &command)?;
        Ok(())
    }

    pub fn commit_session(&mut self, command: &SessionCommit) -> Result<SaveReceipt> {
        apply_maintenance_commit(&mut self.writer, command)
    }

    pub fn import_legacy_attachments(&mut self) -> Result<usize> {
        let mut session = self
            .writer
            .db()?
            .load_full_session()?
            .ok_or_else(|| StoreError::Integrity("session metadata is missing".into()))?;
        let mut changed = 0;
        for item in &mut session.history {
            if !history_item_has_legacy_attachment(item) {
                continue;
            }
            let mut value = serde_json::to_value(&*item)?;
            hydrate_legacy_attachment_value(self.writer.session_dir(), &mut value)?;
            *item = serde_json::from_value(value)?;
            changed += 1;
        }
        if changed == 0 {
            return Ok(0);
        }
        let command = full_session_commit(
            session.session.head,
            &session.session.identity,
            &session.session.metadata,
            &session,
        )?;
        apply_maintenance_commit(&mut self.writer, &command)?;
        Ok(changed)
    }

    pub fn garbage_collect_objects(&mut self) -> Result<usize> {
        let token = self.writer.lease.token.clone();
        self.writer.db_mut()?.garbage_collect_objects_owned(&token)
    }

    pub fn rebuild_search_index(&mut self) -> Result<()> {
        let token = self.writer.lease.token.clone();
        self.writer.db_mut()?.rebuild_search_index_owned(&token)
    }

    pub fn vacuum(&mut self) -> Result<()> {
        let token = self.writer.lease.token.clone();
        self.writer.db()?.vacuum_owned(&token)
    }

    pub fn release(self) -> Result<()> {
        self.writer.release()
    }
}

fn full_session_commit(
    expected: StoreHead,
    identity: &SessionIdentity,
    metadata: &SessionMetadata,
    session: &FullSession,
) -> Result<SessionCommit> {
    let history_len = u64::try_from(session.history.len())
        .map_err(|_| StoreError::Integrity("history length exceeds u64".into()))?;
    Ok(SessionCommit {
        session_id: identity.id.clone(),
        expected,
        identity: identity.clone(),
        metadata: metadata.clone(),
        history: HistorySuffix {
            start: HistoryIndex::ZERO,
            final_len: HistoryLen::new(history_len),
            items: session.history.clone(),
        },
        side_tables: SideTableSuffixes {
            start: HistoryIndex::ZERO,
            turn_metas: side_table_rows(&session.turn_metas),
            metadata_snapshots: side_table_rows(&session.metadata_snapshots),
            context_snapshots: side_table_rows(&session.context_snapshots),
        },
        transcript_records: Some(TranscriptRecordSuffix {
            start: TranscriptRecordIndex::ZERO,
            records: session.transcript_records.clone(),
        }),
    })
}

fn metadata_and_record_commit(
    session: &FullSession,
    metadata: SessionMetadata,
    records: Option<TranscriptRecordSuffix>,
) -> Result<SessionCommit> {
    let history_len = session.session.head.history_len;
    let boundary = history_len.get();
    Ok(SessionCommit {
        session_id: session.session.identity.id.clone(),
        expected: session.session.head,
        identity: session.session.identity.clone(),
        metadata,
        history: HistorySuffix {
            start: HistoryIndex::new(boundary),
            final_len: history_len,
            items: Vec::new(),
        },
        side_tables: SideTableSuffixes {
            start: HistoryIndex::new(boundary),
            turn_metas: side_table_rows_from(&session.turn_metas, boundary),
            metadata_snapshots: side_table_rows_from(&session.metadata_snapshots, boundary),
            context_snapshots: side_table_rows_from(&session.context_snapshots, boundary),
        },
        transcript_records: records,
    })
}

fn side_table_rows(rows: &[(u64, serde_json::Value)]) -> Vec<(HistoryIndex, serde_json::Value)> {
    side_table_rows_from(rows, 0)
}

fn side_table_rows_from(
    rows: &[(u64, serde_json::Value)],
    start: u64,
) -> Vec<(HistoryIndex, serde_json::Value)> {
    rows.iter()
        .filter(|(index, _)| *index >= start)
        .map(|(index, value)| (HistoryIndex::new(*index), value.clone()))
        .collect()
}

fn apply_maintenance_commit(
    writer: &mut OwnedSessionWriter,
    command: &SessionCommit,
) -> Result<SaveReceipt> {
    writer
        .commit_session(command)
        .map_err(|failure| StoreError::Integrity(format!("session commit failed: {failure:?}")))
}

fn session_commit_failure_from_blob_error(err: StoreError) -> SessionCommitFailure {
    crate::db::session_commit_failure_from_store_error(err)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArtifactCleanupReport {
    pub removed: usize,
    pub quarantined: usize,
    pub skipped: usize,
}

pub fn cleanup_abandoned_session_artifacts(
    root: impl AsRef<Path>,
    limit: usize,
) -> Result<ArtifactCleanupReport> {
    let root = root.as_ref();
    ensure_private_directory_all(root)?;
    let mut report = ArtifactCleanupReport::default();
    let mut remaining = limit;
    cleanup_artifact_directory(root, STAGING_DIR, true, &mut remaining, &mut report)?;
    cleanup_artifact_directory(root, TRASH_DIR, false, &mut remaining, &mut report)?;
    Ok(report)
}

fn cleanup_artifact_directory(
    root: &Path,
    directory_name: &str,
    unpublished: bool,
    remaining: &mut usize,
    report: &mut ArtifactCleanupReport,
) -> Result<()> {
    if *remaining == 0 {
        return Ok(());
    }
    let directory = root.join(directory_name);
    match fs::symlink_metadata(&directory) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(StoreError::Integrity(format!(
                "artifact root is not a private directory: {}",
                directory.display()
            )));
        }
        Err(err) => return Err(err.into()),
    }
    let entries = fs::read_dir(&directory)?
        .take(*remaining)
        .collect::<Vec<_>>();
    *remaining = remaining.saturating_sub(entries.len());
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            quarantine_artifact(root, &path, "non-utf8")?;
            report.quarantined += 1;
            continue;
        };
        let Some(session_id) = artifact_session_id(name) else {
            quarantine_artifact(root, &path, "malformed")?;
            report.quarantined += 1;
            continue;
        };
        let layout = SessionLayout::new(root, session_id)?;
        let _lease = match SessionLease::acquire(layout.clone()) {
            Ok(lease) => lease,
            Err(StoreError::OwnershipConflict { .. }) => {
                report.skipped += 1;
                continue;
            }
            Err(err) => return Err(err),
        };
        if unpublished && path_exists(layout.published_dir())? {
            report.skipped += 1;
            continue;
        }
        let valid = fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            && SessionReader::open_existing(&path)
                .and_then(|reader| reader.stored_session())
                .is_ok_and(|session| {
                    session.is_some_and(|session| session.identity.id == layout.session_id)
                });
        if valid {
            fs::remove_dir_all(&path)?;
            sync_directory(&directory)?;
            report.removed += 1;
        } else {
            quarantine_artifact(root, &path, "invalid")?;
            report.quarantined += 1;
        }
    }
    Ok(())
}

fn quarantine_artifact(root: &Path, path: &Path, reason: &str) -> Result<PathBuf> {
    let quarantine = root.join(QUARANTINE_DIR);
    ensure_private_directory(&quarantine)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    let destination = quarantine.join(format!("{name}.{reason}.{}", random_token()?));
    rename_without_replacement(path, &destination)?;
    sync_directory(&quarantine)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    sync_directory(root)?;
    Ok(destination)
}

fn artifact_session_id(name: &str) -> Option<&str> {
    let (session_id, _) = name.split_once('.')?;
    validate_session_id(session_id).ok()?;
    Some(session_id)
}

#[derive(Debug)]
struct LegacySessionLock {
    _file: File,
}

impl LegacySessionLock {
    // COMPAT(storage-root-lease): this is the only in-directory lock creation.
    // Remove it when schema versions older than v6 are no longer supported.
    fn acquire(session_dir: &Path) -> Result<Self> {
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
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => Err(StoreError::OwnershipConflict {
                owner: SessionReader::open_existing(session_dir)
                    .ok()
                    .and_then(|reader| reader.writer_owner().ok().flatten())
                    .map(|owner| owner.summary()),
            }),
            Err(fs::TryLockError::Error(err)) => Err(StoreError::Io(err)),
        }
    }
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.len() != SESSION_ID_LEN
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(StoreError::Integrity(format!(
            "session id must be exactly {SESSION_ID_LEN} lowercase hexadecimal characters: {session_id:?}"
        )));
    }
    Ok(())
}

fn published_database_path(layout: &SessionLayout) -> Result<PathBuf> {
    let session_dir = layout.published_dir();
    match fs::symlink_metadata(session_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!(
                    "published session path is not a directory: {}",
                    session_dir.display()
                ),
            )))
        }
        Err(error) => return Err(error.into()),
    }
    let db_path = session_dir.join("session.db");
    reject_symlink(&db_path)?;
    match fs::metadata(&db_path) {
        Ok(metadata) if metadata.is_file() => Ok(db_path),
        Ok(_) => Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("session database is not a file: {}", db_path.display()),
        ))),
        Err(error) => Err(error.into()),
    }
}

fn inspect_session_schema_for_layout(layout: &SessionLayout) -> Result<SessionSchemaInspection> {
    let db_path = published_database_path(layout)?;
    let connection = database_connection_read_only(&db_path)?;
    let version = crate::schema::user_version(&connection)?;
    let status = SessionSchemaStatus::from_version(version);
    if matches!(&status, SessionSchemaStatus::Current { .. }) {
        let reader = match SessionReader::from_read_only_connection(&db_path, connection) {
            Ok(reader) => reader,
            Err(StoreError::Integrity(reason)) => {
                return Ok(SessionSchemaInspection {
                    status: SessionSchemaStatus::Corrupt {
                        found: version,
                        reason,
                    },
                    reader: None,
                });
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = reader
            .db
            .validate_session_identity(&layout.session_id, version)
        {
            return Ok(SessionSchemaInspection {
                status: schema_identity_error_status(error, version)?,
                reader: None,
            });
        }
        return Ok(SessionSchemaInspection {
            status,
            reader: Some(reader),
        });
    }
    if matches!(&status, SessionSchemaStatus::Upgradeable { .. }) {
        if let Err(error) =
            validate_session_identity_connection(&connection, &layout.session_id, version)
        {
            return Ok(SessionSchemaInspection {
                status: schema_identity_error_status(error, version)?,
                reader: None,
            });
        }
    }
    Ok(SessionSchemaInspection {
        status,
        reader: None,
    })
}

fn schema_identity_error_status(error: StoreError, version: i32) -> Result<SessionSchemaStatus> {
    match error {
        StoreError::OrphanedSession { found } => Ok(SessionSchemaStatus::Orphaned { found }),
        StoreError::Integrity(reason) => Ok(SessionSchemaStatus::Corrupt {
            found: version,
            reason,
        }),
        error => Err(error),
    }
}

fn orphaned_session_schema_version(layout: &SessionLayout) -> Result<Option<i32>> {
    match inspect_session_schema_for_layout(layout)?.status {
        SessionSchemaStatus::Orphaned { found } => Ok(Some(found)),
        SessionSchemaStatus::Corrupt { reason, .. } => Err(StoreError::Integrity(reason)),
        _ => Ok(None),
    }
}

fn database_connection_read_only(path: &Path) -> Result<rusqlite::Connection> {
    reject_symlink(path)?;
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    Ok(rusqlite::Connection::open_with_flags(path, flags)?)
}

fn validate_database_identity(path: &Path, session_id: &str, version: i32) -> Result<()> {
    validate_session_identity_connection(&database_connection_read_only(path)?, session_id, version)
}

fn database_schema_version(path: &Path) -> Result<i32> {
    crate::schema::user_version(&database_connection_read_only(path)?)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn create_staging_directory(layout: &SessionLayout, staging_root: &Path) -> Result<PathBuf> {
    let timestamp = now_ms();
    for _ in 0..16 {
        let path = staging_root.join(format!(
            "{}.{}.{}.{}",
            layout.session_id,
            std::process::id(),
            timestamp,
            random_token()?
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                set_private_permissions(&path)?;
                sync_directory(staging_root)?;
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
    }
    Err(StoreError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique session staging directory",
    )))
}

fn ensure_destination_available(layout: &SessionLayout) -> Result<()> {
    for directory in [layout.staging_dir(), layout.trash_dir()] {
        match fs::symlink_metadata(&directory) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(StoreError::Integrity(format!(
                    "session artifact path is not a directory: {}",
                    directory.display()
                )));
            }
            Err(err) => return Err(err.into()),
        }
        let prefix = format!("{}.", layout.session_id);
        if fs::read_dir(&directory)?.any(|entry| {
            entry.is_ok_and(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        }) {
            return Err(StoreError::Integrity(format!(
                "session destination has an unresolved artifact in {}",
                directory.display()
            )));
        }
    }
    Ok(())
}

fn validate_stored_identity(db: &SessionDb, session_id: &str) -> Result<()> {
    let session = db.stored_session()?.ok_or_else(|| {
        StoreError::Integrity(format!(
            "published session {session_id} is missing canonical identity"
        ))
    })?;
    if session.identity.id != session_id {
        return Err(StoreError::Integrity(format!(
            "session id mismatch: requested {session_id}, stored {}",
            session.identity.id
        )));
    }
    Ok(())
}

fn verify_publication(db: &SessionDb, expected: &PublicationExpectation) -> Result<()> {
    let stored = db.stored_session()?.ok_or_else(|| {
        StoreError::Integrity("published session is missing canonical state".into())
    })?;
    if stored.identity != expected.identity {
        return Err(StoreError::Integrity(
            "published session identity does not match staged identity".into(),
        ));
    }
    if stored.head != expected.head {
        return Err(StoreError::Integrity(format!(
            "published session head mismatch: expected {:?}, got {:?}",
            expected.head, stored.head
        )));
    }
    let actual = db.last_session_commit()?;
    if actual.as_ref().map(|(fingerprint, _)| fingerprint) != Some(&expected.fingerprint) {
        return Err(StoreError::Integrity(
            "published session commit fingerprint does not match staged commit".into(),
        ));
    }
    if actual.as_ref().map(|(_, receipt)| receipt) != Some(&expected.receipt) {
        return Err(StoreError::Integrity(
            "published session receipt does not match staged commit".into(),
        ));
    }
    Ok(())
}

fn recover_owned_blob_staging(db: &SessionDb, session_dir: &Path) -> Result<()> {
    let fingerprint = db.last_session_commit_fingerprint()?;
    recover_blob_staging(session_dir, fingerprint.as_deref())
}

#[cfg(unix)]
fn path_cstring(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path contains a null byte: {}", path.display()),
        )
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn rename_without_replacement(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = path_cstring(source)?;
    let destination = path_cstring(destination)?;
    // SAFETY: both paths are valid, null-terminated byte strings and remain
    // alive for the duration of the syscall.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn rename_without_replacement(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = path_cstring(source)?;
    let destination = path_cstring(destination)?;
    // SAFETY: both paths are valid, null-terminated byte strings and remain
    // alive for the duration of the call.
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn rename_without_replacement(source: &Path, destination: &Path) -> std::io::Result<()> {
    // Windows directory rename fails atomically when the destination exists.
    fs::rename(source, destination)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
)))]
pub(crate) fn rename_without_replacement(
    _source: &Path,
    _destination: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on this platform",
    ))
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn record_close_hygiene(result: Result<bool>) -> Result<()> {
    match result {
        Ok(true) => {
            smelt_perf::perf::record_value("store:close:hygiene_complete", 1);
            Ok(())
        }
        Ok(false) => {
            smelt_perf::perf::record_value("store:close:hygiene_deferred", 1);
            Ok(())
        }
        Err(err) => {
            smelt_perf::perf::record_value("store:close:hygiene_failed", 1);
            Err(err)
        }
    }
}

fn finish_operation_cleanup(
    operation: &'static str,
    primary: Result<()>,
    cleanup: Result<()>,
) -> Result<()> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(primary), Err(cleanup)) => Err(StoreError::OperationCleanup {
            operation,
            primary: Box::new(primary),
            cleanup: vec![cleanup],
        }),
    }
}

pub(crate) fn ensure_private_directory_all(path: &Path) -> Result<()> {
    match fs::create_dir_all(path) {
        Ok(()) => ensure_private_directory(path),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn ensure_private_directory(path: &Path) -> Result<()> {
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
    set_private_permissions(path)
}

fn set_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn reject_symlink(path: &Path) -> Result<()> {
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

    const SESSION_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SECOND_SESSION_ID: &str =
        "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn publish_and_release(mut writer: OwnedSessionWriter) {
        if writer.stored_session().unwrap().is_some() && writer.is_staged() {
            writer.publish().unwrap();
        }
        writer.release().unwrap();
    }

    fn empty_commit(session_id: &str, base_revision: u64) -> SessionCommit {
        SessionCommit {
            session_id: session_id.into(),
            expected: StoreHead {
                revision: crate::Revision::new(base_revision),
                history_len: crate::HistoryLen::ZERO,
                transcript_record_count: crate::TranscriptRecordCount::ZERO,
            },
            identity: SessionIdentity {
                id: session_id.into(),
                created_at: 1,
                parent_id: None,
            },
            metadata: SessionMetadata {
                title: None,
                slug: None,
                first_user_message: None,
                cwd: None,
                mode: None,
                reasoning_effort: None,
                model: None,
                fast_mode: None,
                accounting_json: None,
                checkpoint_json: None,
                checkpoint_events_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: crate::SessionCostUsd::new(0.0).unwrap(),
                updated_at: 1,
            },
            history: crate::HistorySuffix {
                start: crate::HistoryIndex::ZERO,
                final_len: crate::HistoryLen::ZERO,
                items: Vec::new(),
            },
            side_tables: crate::SideTableSuffixes::default(),
            transcript_records: None,
        }
    }

    fn transcript_record(
        block_idx: u64,
        history_idx: u64,
        kind: &str,
        text: &str,
    ) -> StoredTranscriptBlock {
        StoredTranscriptBlock {
            block_idx,
            history_idx: Some(history_idx),
            kind: kind.into(),
            tool_call_id: None,
            tool_name: None,
            content_hash: format!("hash-{block_idx}"),
            estimated_text_bytes: text.len() as u64,
            preview_text: text.into(),
            indexed_text: text.into(),
            block_json: serde_json::json!({ "kind": kind, "text": text }).to_string(),
            origin_json: Some(serde_json::json!({ "History": history_idx }).to_string()),
            tool_state_json: None,
        }
    }

    fn no_op_commit(session_id: &str, expected: StoreHead) -> SessionCommit {
        let mut command = empty_commit(session_id, expected.revision.get());
        command.expected = expected;
        command.history.start = crate::HistoryIndex::new(expected.history_len.get());
        command.history.final_len = expected.history_len;
        command.side_tables.start = crate::HistoryIndex::new(expected.history_len.get());
        command
    }

    fn submit_command(writer: &OwnedSessionWriter, text: &str, created_at_ms: u64) -> SubmitTurn {
        let expected = writer.store_head().unwrap();
        let history_idx = expected.history_len.get();
        let mut session = no_op_commit(writer.session_id(), expected);
        session.history = crate::HistorySuffix {
            start: crate::HistoryIndex::new(history_idx),
            final_len: crate::HistoryLen::new(history_idx + 1),
            items: vec![protocol::HistoryItem::user(protocol::Content::text(text))],
        };
        SubmitTurn {
            session,
            turn: crate::NewTurn {
                kind: crate::TurnKind::User,
                submitted_history_idx: crate::HistoryIndex::new(history_idx),
                continuation_of: None,
                created_at_ms,
            },
        }
    }

    fn transition_command(
        writer: &OwnedSessionWriter,
        turn_id: TurnId,
        state: crate::TurnState,
        at_ms: u64,
    ) -> TurnTransition {
        TurnTransition {
            session: no_op_commit(writer.session_id(), writer.store_head().unwrap()),
            turn_id,
            state,
            at_ms,
            terminal_reason: state.is_terminal().then(|| state.as_str().to_string()),
        }
    }

    #[test]
    fn invalid_session_ids_never_reach_the_lock_namespace() {
        let root = tempfile::tempdir().unwrap();
        for invalid in [
            "",
            "session",
            "../0123",
            "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            assert!(matches!(
                OwnedSessionWriter::open(root.path(), invalid),
                Err(StoreError::Integrity(_))
            ));
        }
        assert!(!root.path().join(LOCKS_DIR).exists());
    }

    #[test]
    fn prepared_publication_is_preserved_if_the_rename_never_starts() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        let staged = writer.session_dir().to_path_buf();
        writer.prepare_publication().unwrap();

        writer.release().unwrap();

        assert!(staged.join("session.db").is_file());
        assert!(!root.path().join(SESSION_ID).exists());
    }

    #[test]
    fn atomic_publication_never_replaces_an_empty_destination() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        let staged = writer.session_dir().to_path_buf();
        let published = root.path().join(SESSION_ID);
        fs::create_dir(&published).unwrap();

        assert!(matches!(writer.publish(), Err(StoreError::Integrity(_))));
        assert!(staged.join("session.db").is_file());
        assert_eq!(fs::read_dir(&published).unwrap().count(), 0);
    }

    #[test]
    fn unexpected_publication_destination_preserves_both_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        let staged = writer.session_dir().to_path_buf();
        let published = root.path().join(SESSION_ID);
        fs::create_dir(&published).unwrap();
        fs::write(published.join("sentinel"), "unexpected").unwrap();

        assert!(matches!(writer.publish(), Err(StoreError::Integrity(_))));
        assert!(staged.join("session.db").is_file());
        assert_eq!(
            fs::read_to_string(published.join("sentinel")).unwrap(),
            "unexpected"
        );
        drop(writer);
        assert!(staged.join("session.db").is_file());
    }

    #[test]
    fn token_mismatch_after_rename_preserves_the_published_database() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        writer
            .db()
            .unwrap()
            .set_meta(
                "writer_owner",
                &serde_json::json!({
                    "token": "replacement-token",
                    "owner": writer.owner(),
                })
                .to_string(),
            )
            .unwrap();

        assert!(matches!(writer.publish(), Err(StoreError::OwnershipLost)));
        let published = root.path().join(SESSION_ID);
        assert!(published.join("session.db").is_file());
        assert_eq!(
            SessionReader::open_existing(&published)
                .unwrap()
                .stored_session()
                .unwrap()
                .unwrap()
                .identity
                .id,
            SESSION_ID
        );
    }

    #[test]
    fn identity_change_after_publication_close_never_overwrites_data() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        writer.prepare_publication().unwrap();
        let staged_db = writer.database_path();
        rusqlite::Connection::open(&staged_db)
            .unwrap()
            .execute(
                "UPDATE session_state SET id = ?1 WHERE singleton = 1",
                [SECOND_SESSION_ID],
            )
            .unwrap();

        assert!(matches!(writer.publish(), Err(StoreError::Integrity(_))));
        let published = root.path().join(SESSION_ID);
        assert!(published.join("session.db").is_file());
        let stored = SessionReader::open_existing(&published)
            .unwrap()
            .stored_session()
            .unwrap()
            .unwrap();
        assert_eq!(stored.identity.id, SECOND_SESSION_ID);
    }

    #[test]
    fn publication_retries_after_rename_and_reopen_failure() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        writer.prepare_publication().unwrap();
        let staged_db = writer.database_path();
        let recoverable_db = staged_db.with_extension("db.recoverable");
        fs::rename(&staged_db, &recoverable_db).unwrap();
        fs::create_dir(&staged_db).unwrap();

        assert!(writer.publish().is_err());
        let published = root.path().join(SESSION_ID);
        assert!(!writer.is_staged());
        assert!(published.join("session.db.recoverable").is_file());

        fs::remove_dir(published.join("session.db")).unwrap();
        fs::rename(
            published.join("session.db.recoverable"),
            published.join("session.db"),
        )
        .unwrap();
        assert_eq!(writer.publish().unwrap(), published);
        assert_eq!(
            writer.stored_session().unwrap().unwrap().head.revision,
            crate::Revision::new(1)
        );
        writer.release().unwrap();
    }

    #[test]
    fn artifact_cleanup_removes_valid_orphans_and_quarantines_unexpected_data() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join(STAGING_DIR);
        fs::create_dir(&staging).unwrap();

        let valid = staging.join(format!("{SESSION_ID}.valid"));
        fs::create_dir(&valid).unwrap();
        let mut valid_db = SessionDb::open(valid.join("session.db")).unwrap();
        valid_db
            .apply_session_commit(&empty_commit(SESSION_ID, 0))
            .unwrap();
        drop(valid_db);

        let mismatched = staging.join(format!("{SECOND_SESSION_ID}.mismatched"));
        fs::create_dir(&mismatched).unwrap();
        let mut mismatched_db = SessionDb::open(mismatched.join("session.db")).unwrap();
        mismatched_db
            .apply_session_commit(&empty_commit(SESSION_ID, 0))
            .unwrap();
        drop(mismatched_db);

        let malformed = staging.join("not-a-session");
        fs::write(&malformed, "unexpected").unwrap();

        let report = cleanup_abandoned_session_artifacts(root.path(), 3).unwrap();

        assert_eq!(report.removed, 1);
        assert_eq!(report.quarantined, 2);
        assert_eq!(report.skipped, 0);
        assert!(!valid.exists());
        assert!(!mismatched.exists());
        assert!(!malformed.exists());
        assert_eq!(
            fs::read_dir(root.path().join(QUARANTINE_DIR))
                .unwrap()
                .count(),
            2
        );
        assert!(root
            .path()
            .join(LOCKS_DIR)
            .join(format!("{SESSION_ID}.lock"))
            .is_file());
        assert!(root
            .path()
            .join(LOCKS_DIR)
            .join(format!("{SECOND_SESSION_ID}.lock"))
            .is_file());
    }

    #[test]
    fn clean_release_clears_owner_and_allows_reacquisition() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join(SESSION_ID);
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        let first_token = writer.token().to_string();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(
                    root.path()
                        .join(LOCKS_DIR)
                        .join(format!("{SESSION_ID}.lock"))
                )
                .unwrap()
                .permissions()
                .mode()
                    & 0o777,
                0o600
            );
        }
        let first_owner = writer
            .db()
            .unwrap()
            .writer_owner()
            .unwrap()
            .expect("owner metadata");

        publish_and_release(writer);
        assert!(SessionReader::open_existing(&session_dir)
            .unwrap()
            .writer_owner()
            .unwrap()
            .is_none());

        let replacement = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        assert_ne!(replacement.token(), first_token);
        assert_ne!(replacement.owner().claimed_at, 0);
        assert_eq!(
            replacement.owner().process_start_id,
            first_owner.process_start_id
        );
    }

    #[test]
    fn writable_reopen_interrupts_nonterminal_turns_once_and_reader_never_mutates() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join(SESSION_ID);
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();

        let first_command = submit_command(&writer, "ready", 100);
        let ready = writer.submit_turn(&first_command).unwrap();
        let second_command = submit_command(&writer, "running", 200);
        let running = writer.submit_turn(&second_command).unwrap();
        let running_command =
            transition_command(&writer, running.turn_id, crate::TurnState::Running, 210);
        writer.transition_turn(&running_command).unwrap();
        let third_command = submit_command(&writer, "terminal", 300);
        let terminal = writer.submit_turn(&third_command).unwrap();
        let terminal_command =
            transition_command(&writer, terminal.turn_id, crate::TurnState::Failed, 310);
        writer.transition_turn(&terminal_command).unwrap();
        let head_before_recovery = writer.store_head().unwrap();
        writer.publish().unwrap();
        writer.release().unwrap();

        let reader = SessionReader::open_existing(&session_dir).unwrap();
        assert_eq!(reader.store_head().unwrap(), head_before_recovery);
        assert_eq!(
            reader
                .turns()
                .unwrap()
                .into_iter()
                .map(|turn| turn.state)
                .collect::<Vec<_>>(),
            vec![
                crate::TurnState::Ready,
                crate::TurnState::Running,
                crate::TurnState::Failed,
            ]
        );
        drop(reader);

        let mut reopened = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        let recovery = reopened
            .startup_recovery()
            .expect("nonterminal turns require recovery");
        assert_eq!(recovery.session.previous, head_before_recovery);
        assert_eq!(
            recovery.session.current.revision,
            crate::Revision::new(head_before_recovery.revision.get() + 1)
        );
        assert_eq!(
            recovery.interrupted_turns,
            vec![ready.turn_id, running.turn_id]
        );
        assert_eq!(
            reopened.latest_terminal_turn_id().unwrap(),
            Some(terminal.turn_id)
        );
        assert_eq!(
            reopened
                .db()
                .unwrap()
                .turns()
                .unwrap()
                .into_iter()
                .map(|turn| (turn.state, turn.terminal_reason))
                .collect::<Vec<_>>(),
            vec![
                (
                    crate::TurnState::Interrupted,
                    Some("process_restart".into())
                ),
                (
                    crate::TurnState::Interrupted,
                    Some("process_restart".into())
                ),
                (crate::TurnState::Failed, Some("failed".into())),
            ]
        );
        let recovered_head = reopened.store_head().unwrap();
        assert_eq!(
            reopened.take_startup_recovery().unwrap().interrupted_turns,
            vec![ready.turn_id, running.turn_id]
        );
        assert!(reopened.startup_recovery().is_none());
        reopened.release().unwrap();

        let reopened_again = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        assert!(reopened_again.startup_recovery().is_none());
        assert_eq!(reopened_again.store_head().unwrap(), recovered_head);
        reopened_again.release().unwrap();
    }

    #[test]
    fn release_reopens_an_invalidated_connection_and_clears_ownership() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.invalidate_connection();

        writer.release().unwrap();

        let replacement = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        replacement.release().unwrap();
    }

    #[test]
    fn release_after_token_mismatch_never_clears_the_replacement_token() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        writer.publish().unwrap();
        writer.invalidate_connection();
        let published = root.path().join(SESSION_ID);
        let db = SessionDb::open(published.join("session.db")).unwrap();
        db.set_meta(
            "writer_owner",
            &serde_json::json!({
                "token": "replacement-token",
                "owner": writer.owner(),
            })
            .to_string(),
        )
        .unwrap();
        drop(db);

        assert!(matches!(writer.release(), Err(StoreError::OwnershipLost)));
        let owner: serde_json::Value = serde_json::from_str(
            &SessionReader::open_existing(&published)
                .unwrap()
                .meta("writer_owner")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(owner["token"], "replacement-token");
    }

    #[test]
    fn writer_rejects_an_id_that_disagrees_with_existing_state() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join(SESSION_ID);
        let mismatched_dir = root.path().join(SECOND_SESSION_ID);
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        publish_and_release(writer);
        fs::rename(&session_dir, &mismatched_dir).unwrap();

        let err = OwnedSessionWriter::open(root.path(), SECOND_SESSION_ID).unwrap_err();

        assert!(matches!(
            err,
            StoreError::Integrity(message)
                if message == format!(
                    "session id mismatch: requested {SECOND_SESSION_ID}, stored {SESSION_ID}"
                )
        ));
        assert!(SessionReader::open_existing(&mismatched_dir)
            .unwrap()
            .writer_owner()
            .unwrap()
            .is_none());
    }

    #[test]
    fn migration_only_ownership_upgrades_without_interrupting_turns() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        let ready = writer
            .submit_turn(&submit_command(&writer, "pending", 10))
            .unwrap();
        writer.publish().unwrap();
        writer.release().unwrap();
        let db_path = root.path().join(SESSION_ID).join("session.db");
        let expected_store_head = SessionReader::open_database(&db_path)
            .unwrap()
            .store_head()
            .unwrap();
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 9")
            .unwrap();

        assert_eq!(
            session_schema_status(root.path(), SESSION_ID).unwrap(),
            SessionSchemaStatus::Upgradeable {
                found: 9,
                target: crate::SCHEMA_VERSION,
            }
        );
        let migration = migrate_session_schema(root.path(), SESSION_ID).unwrap();

        assert_eq!(
            migration,
            SessionSchemaMigration {
                session_id: SESSION_ID.into(),
                from_version: 9,
                to_version: crate::SCHEMA_VERSION,
                migrated: true,
                store_head: expected_store_head,
            }
        );
        let reader = SessionReader::open_database(&db_path).unwrap();
        assert_eq!(
            reader.turn(ready.turn_id).unwrap().unwrap().state,
            crate::TurnState::Ready
        );
        assert!(reader.writer_owner().unwrap().is_none());
    }

    #[test]
    fn migration_probe_rejects_future_and_classifies_missing_identity() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        publish_and_release(writer);
        let db_path = root.path().join(SESSION_ID).join("session.db");
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute_batch(&format!(
                "PRAGMA user_version = {}",
                crate::SCHEMA_VERSION + 1
            ))
            .unwrap();
        drop(connection);

        assert_eq!(
            session_schema_status(root.path(), SESSION_ID).unwrap(),
            SessionSchemaStatus::Future {
                found: crate::SCHEMA_VERSION + 1,
                supported: crate::SCHEMA_VERSION,
            }
        );
        assert!(matches!(
            migrate_session_schema(root.path(), SESSION_ID),
            Err(StoreError::UnsupportedSchema {
                found,
                expected,
            }) if found == crate::SCHEMA_VERSION + 1 && expected == crate::SCHEMA_VERSION
        ));
        assert_eq!(
            database_schema_version(&db_path).unwrap(),
            crate::SCHEMA_VERSION + 1
        );

        let second_root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(second_root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        publish_and_release(writer);
        let db_path = second_root.path().join(SESSION_ID).join("session.db");
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute_batch(
                "DELETE FROM session_state;
                 INSERT INTO request_attempts (started_at) VALUES (1);
                 PRAGMA user_version = 9",
            )
            .unwrap();
        drop(connection);

        assert_eq!(
            session_schema_status(second_root.path(), SESSION_ID).unwrap(),
            SessionSchemaStatus::Orphaned { found: 9 }
        );
        assert!(matches!(
            migrate_session_schema(second_root.path(), SESSION_ID),
            Err(StoreError::OrphanedSession { found: 9 })
        ));
        assert_eq!(database_schema_version(&db_path).unwrap(), 9);

        let quarantine = quarantine_orphaned_session(second_root.path(), SESSION_ID)
            .unwrap()
            .expect("identity-less empty session should be quarantined");
        assert_eq!(quarantine.session_id, SESSION_ID);
        assert_eq!(quarantine.schema_version, 9);
        assert!(!second_root.path().join(SESSION_ID).exists());
        assert!(quarantine.quarantine_path.join("session.db").is_file());

        let third_root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(third_root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        publish_and_release(writer);
        let db_path = third_root.path().join(SESSION_ID).join("session.db");
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute(
                "INSERT INTO history_items (idx, kind, json, hash, created_at)
                 VALUES (0, 'user', '{}', ?1, 1)",
                ["0".repeat(64)],
            )
            .unwrap();
        connection
            .execute_batch("DELETE FROM session_state; PRAGMA user_version = 9")
            .unwrap();
        drop(connection);

        assert!(matches!(
            session_schema_status(third_root.path(), SESSION_ID).unwrap(),
            SessionSchemaStatus::Corrupt { found: 9, reason }
                if reason.contains("missing canonical identity")
        ));
        assert!(matches!(
            quarantine_orphaned_session(third_root.path(), SESSION_ID),
            Err(StoreError::Integrity(message)) if message.contains("missing canonical identity")
        ));
        assert!(third_root.path().join(SESSION_ID).is_dir());
        assert_eq!(database_schema_version(&db_path).unwrap(), 9);
    }

    #[test]
    fn schema_inspection_classifies_current_shape_failure_as_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        publish_and_release(writer);
        let db_path = root.path().join(SESSION_ID).join("session.db");
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute_batch("DROP TABLE session_state")
            .unwrap();

        assert!(matches!(
            session_schema_status(root.path(), SESSION_ID).unwrap(),
            SessionSchemaStatus::Corrupt { found, reason }
                if found == crate::SCHEMA_VERSION
                    && reason.contains("sqlite schema missing table session_state")
        ));
    }

    #[test]
    fn maintenance_delete_requires_ownership_and_removes_tombstone() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join(SESSION_ID);
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        writer.publish().unwrap();

        assert!(matches!(
            SessionMaintenance::delete_session(root.path(), SESSION_ID),
            Err(StoreError::OwnershipConflict { .. })
        ));
        publish_and_release(writer);

        SessionMaintenance::delete_session(root.path(), SESSION_ID).unwrap();
        assert!(!session_dir.exists());
        assert!(root
            .path()
            .join(LOCKS_DIR)
            .join(format!("{SESSION_ID}.lock"))
            .is_file());
        assert!(!root.path().join(TRASH_DIR).exists());
    }

    #[test]
    fn matching_transcript_records_relink_replaced_history() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        let record = transcript_record(0, 0, "user", "user-0");

        let mut initial = empty_commit(SESSION_ID, 0);
        initial.history.final_len = crate::HistoryLen::new(1);
        initial.history.items = vec![protocol::HistoryItem::user(protocol::Content::with_images(
            String::new(),
            vec![(
                "fuzz.png".to_string(),
                "data:image/png;base64,AAAA".to_string(),
            )],
        ))];
        initial.transcript_records = Some(TranscriptRecordSuffix {
            start: TranscriptRecordIndex::ZERO,
            records: vec![record.clone()],
        });
        writer.commit_session(&initial).unwrap();
        if writer.is_staged() {
            writer.publish().unwrap();
        }

        let mut replacement = empty_commit(SESSION_ID, writer.store_head().unwrap().revision.get());
        replacement.expected = writer.store_head().unwrap();
        replacement.history.final_len = crate::HistoryLen::new(1);
        replacement.history.items = vec![protocol::HistoryItem::user(protocol::Content::text(""))];
        replacement.transcript_records = Some(TranscriptRecordSuffix {
            start: TranscriptRecordIndex::ZERO,
            records: vec![record.clone()],
        });
        writer.commit_session(&replacement).unwrap();
        publish_and_release(writer);

        assert_eq!(
            SessionReader::open_existing(root.path().join(SESSION_ID))
                .unwrap()
                .read_all_transcript_records()
                .unwrap(),
            vec![record]
        );
    }

    #[test]
    fn maintenance_repair_preserves_semantic_links_and_detaches_mismatches() {
        let root = tempfile::tempdir().unwrap();
        let summary_history = protocol::compaction_summary_content("retained summary");
        let valid_process = transcript_record(0, 0, "process_status", "process finished");
        let valid_compacted = transcript_record(1, 1, "compacted", "retained summary");
        let mut bad = transcript_record(2, 2, "user", "continue");
        bad.history_idx = None;

        let mut command = empty_commit(SESSION_ID, 0);
        command.history.final_len = crate::HistoryLen::new(3);
        command.history.items = vec![
            protocol::HistoryItem::note(protocol::HistoryNote::process_status("process finished")),
            protocol::HistoryItem::user(summary_history),
            protocol::HistoryItem::note(protocol::HistoryNote::context("cwd changed")),
        ];
        command.transcript_records = Some(TranscriptRecordSuffix {
            start: TranscriptRecordIndex::ZERO,
            records: vec![valid_process.clone(), valid_compacted.clone(), bad],
        });
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&command).unwrap();
        publish_and_release(writer);

        let db_path = root.path().join(SESSION_ID).join("session.db");
        let db = SessionDb::open(&db_path).unwrap();
        db.connection()
            .execute(
                "UPDATE transcript_blocks SET history_idx = 2 WHERE block_idx = 2",
                [],
            )
            .unwrap();
        db.connection()
            .execute(
                "UPDATE transcript_search SET history_idx = 2 WHERE block_idx = 2",
                [],
            )
            .unwrap();
        drop(db);

        let mut maintenance = SessionMaintenance::open(root.path(), SESSION_ID).unwrap();
        assert_eq!(maintenance.repair_transcript_history_links().unwrap(), 1);
        maintenance.release().unwrap();

        let rows = SessionReader::open_existing(root.path().join(SESSION_ID))
            .unwrap()
            .read_all_transcript_records()
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], valid_process);
        assert_eq!(rows[1], valid_compacted);
        assert_eq!(rows[2].history_idx, None);
        assert_eq!(rows[2].origin_json, None);
    }

    #[test]
    fn opening_writer_cleans_abandoned_blob_staging() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join(SESSION_ID);
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&empty_commit(SESSION_ID, 0)).unwrap();
        let blobs = vec![SessionBlob {
            filename: "attachment.png".into(),
            bytes: b"attachment".to_vec(),
        }];
        let staged = stage_session_blobs(
            writer.session_dir(),
            &"a".repeat(64),
            &"b".repeat(64),
            &blobs,
        )
        .unwrap()
        .unwrap();
        assert!(staged.path().join("attachment.png").is_file());
        drop(staged);
        publish_and_release(writer);

        let replacement = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
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
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        let work_dir = writer.session_dir().to_path_buf();
        let command = empty_commit(SESSION_ID, 9);
        let blobs = vec![SessionBlob {
            filename: "attachment.png".into(),
            bytes: b"attachment".to_vec(),
        }];

        assert!(matches!(
            writer.commit_session_with_blobs(&command, &blobs),
            Err(SessionWriteFailure::Commit(
                SessionCommitFailure::StaleBase { .. }
            ))
        ));
        assert!(!work_dir.join("blobs/attachment.png").exists());
        assert_eq!(
            fs::read_dir(work_dir.join(BLOB_STAGING_DIR))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn committed_blob_staging_recovers_after_deferred_publication() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join(SESSION_ID);
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        let work_dir = writer.session_dir().to_path_buf();
        fs::write(work_dir.join("blobs"), b"publication blocker").unwrap();
        let command = empty_commit(SESSION_ID, 0);
        let blobs = vec![SessionBlob {
            filename: "attachment.png".into(),
            bytes: b"attachment".to_vec(),
        }];

        let outcome = writer.commit_session_with_blobs(&command, &blobs).unwrap();
        assert!(outcome.deferred_blob_error.is_some());
        assert_eq!(outcome.receipt.current.revision, crate::Revision::new(1));
        assert_eq!(
            fs::read_dir(work_dir.join(BLOB_STAGING_DIR))
                .unwrap()
                .count(),
            1
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let staging_root = work_dir.join(BLOB_STAGING_DIR);
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
        assert!(writer.publish().is_err());
        fs::remove_file(session_dir.join("blobs")).unwrap();
        writer.reopen_connection().unwrap();
        writer.release().unwrap();
        let replacement = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
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
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        let command = empty_commit(SESSION_ID, 0);
        let blobs = vec![SessionBlob {
            filename: "../escape".into(),
            bytes: b"attachment".to_vec(),
        }];

        assert!(matches!(
            writer.commit_session_with_blobs(&command, &blobs),
            Err(SessionWriteFailure::Stage(StoreError::Integrity(_)))
        ));
        assert!(!root.path().join("escape").exists());
        assert!(writer.stored_session().unwrap().is_none());
    }

    #[test]
    fn legacy_attachment_blobs_remain_readable_and_missing_blobs_are_explicit() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join(SESSION_ID);
        let data_url = "data:image/png;base64,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let mut hasher = Sha256::new();
        hasher.update(b"image:");
        hasher.update(data_url.as_bytes());
        let filename = format!("{}.png", crate::object::hex_lower(&hasher.finalize()));
        let reference = format!("blob:{filename}");
        let mut command = empty_commit(SESSION_ID, 0);
        command.history.final_len = crate::HistoryLen::new(1);
        command.history.items = vec![protocol::HistoryItem::user(protocol::Content::with_images(
            "legacy".into(),
            vec![("attachment.png".into(), reference.clone())],
        ))];
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&command).unwrap();
        publish_and_release(writer);
        fs::create_dir(session_dir.join("blobs")).unwrap();
        fs::write(session_dir.join("blobs").join(&filename), data_url).unwrap();

        let reader = SessionReader::open_existing(&session_dir).unwrap();
        let history = reader.read_history_items_range(0..1).unwrap();
        let value = serde_json::to_value(&history[0]).unwrap();
        assert_eq!(value["content"][1]["image_url"]["url"], data_url);
        let hydrated_bytes = serde_json::to_vec(&history[0]).unwrap().len();
        assert_eq!(
            reader
                .read_history_items_tail(1, 1, Some(hydrated_bytes))
                .unwrap(),
            history
        );
        assert!(reader
            .read_history_items_tail(1, 1, Some(hydrated_bytes - 1))
            .unwrap()
            .is_empty());
        assert_eq!(
            reader.legacy_attachment_references(1).unwrap(),
            vec![reference.clone()]
        );
        let blob_path = session_dir.join("blobs").join(filename);
        fs::remove_file(&blob_path).unwrap();
        assert!(reader.degraded_warnings().unwrap().is_empty());
        assert!(matches!(
            reader.read_history_items_range(0..1),
            Err(StoreError::MissingObject { reference: missing }) if missing == reference
        ));
        fs::write(&blob_path, data_url).unwrap();
        drop(reader);

        let mut maintenance = SessionMaintenance::open(root.path(), SESSION_ID).unwrap();
        assert_eq!(maintenance.import_legacy_attachments().unwrap(), 1);
        assert_eq!(maintenance.import_legacy_attachments().unwrap(), 0);
        maintenance.release().unwrap();
        fs::remove_file(blob_path).unwrap();
        let reader = SessionReader::open_existing(&session_dir).unwrap();
        assert!(reader.legacy_attachment_references(1).unwrap().is_empty());
        assert_eq!(
            serde_json::to_value(&reader.read_history_items_range(0..1).unwrap()[0]).unwrap()
                ["content"][1]["image_url"]["url"],
            data_url
        );
    }

    #[test]
    fn missing_attachment_objects_are_reported_without_hydrating_history() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join(SESSION_ID);
        let data_url = "data:image/png;base64,AAAA";
        let mut command = empty_commit(SESSION_ID, 0);
        command.history.final_len = crate::HistoryLen::new(1);
        command.history.items = vec![protocol::HistoryItem::user(protocol::Content::with_images(
            "attached".into(),
            vec![("attachment.png".into(), data_url.into())],
        ))];
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&command).unwrap();
        let hash = crate::object::sha256_hex(data_url.as_bytes());
        writer
            .db()
            .unwrap()
            .connection()
            .execute_batch("PRAGMA foreign_keys = OFF; DELETE FROM objects;")
            .unwrap();
        publish_and_release(writer);

        let reader = SessionReader::open_existing(&session_dir).unwrap();
        assert_eq!(
            reader.degraded_warnings().unwrap(),
            vec![format!("missing SQLite object {hash}")]
        );
        assert!(matches!(
            reader.read_history_items_range(0..1),
            Err(StoreError::MissingObject { reference }) if reference == hash
        ));
    }

    #[test]
    fn maintenance_gc_removes_only_unreachable_objects_and_vacuums() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join(SESSION_ID);
        let data_url = "data:image/png;base64,AAAA";
        let mut command = empty_commit(SESSION_ID, 0);
        command.history.final_len = crate::HistoryLen::new(1);
        command.history.items = vec![protocol::HistoryItem::user(protocol::Content::with_images(
            "attached".into(),
            vec![("attachment.png".into(), data_url.into())],
        ))];
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&command).unwrap();
        writer.db().unwrap().put_object(b"unreachable").unwrap();
        let mut maintenance = SessionMaintenance { writer };

        assert_eq!(maintenance.garbage_collect_objects().unwrap(), 1);
        assert_eq!(maintenance.garbage_collect_objects().unwrap(), 0);
        maintenance.vacuum().unwrap();
        maintenance.publish().unwrap();
        maintenance.release().unwrap();

        let history = SessionReader::open_existing(&session_dir)
            .unwrap()
            .read_history_items_range(0..1)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&history[0]).unwrap()["content"][1]["image_url"]["url"],
            data_url
        );
    }

    #[test]
    fn large_rewind_reclaims_unreachable_attachment_objects() {
        let root = tempfile::tempdir().unwrap();
        let data_url = "data:image/png;base64,AAAA";
        let item = protocol::HistoryItem::user(protocol::Content::with_images(
            "attached".into(),
            vec![("attachment.png".into(), data_url.into())],
        ));
        let mut initial = empty_commit(SESSION_ID, 0);
        initial.history.final_len = crate::HistoryLen::new(129);
        initial.history.items = vec![item; 129];
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer.commit_session(&initial).unwrap();
        let object_hash = crate::object::sha256_hex(data_url.as_bytes());
        assert!(writer.db().unwrap().object(&object_hash).unwrap().is_some());

        let mut rewind = empty_commit(SESSION_ID, 1);
        rewind.expected.history_len = crate::HistoryLen::new(129);
        writer.commit_session(&rewind).unwrap();

        assert!(writer.db().unwrap().object(&object_hash).unwrap().is_none());
    }

    #[test]
    fn stale_writer_token_cannot_commit_or_append_request_audit() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = OwnedSessionWriter::open(root.path(), SESSION_ID).unwrap();
        writer
            .db()
            .unwrap()
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
            writer.append_request_attempt(&entry, RequestAuditPayloadMode::SUMMARY),
            Err(StoreError::OwnershipLost)
        ));

        let command = empty_commit(SESSION_ID, 0);
        assert_eq!(
            writer.commit_session(&command),
            Err(SessionCommitFailure::OwnershipLost)
        );
    }
}
